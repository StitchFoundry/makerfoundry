//! Chargebacks on orders from shops (issue #99), as `crate::disputes` does for designs. A shop
//! order is a destination charge too: the shop's share is the transfer, MakerFoundry's 10%, the
//! card fee and the sales tax are the application fee. Stripe takes a disputed payment from
//! MakerFoundry's balance (https://docs.stripe.com/disputes/how-disputes-work), so MakerFoundry
//! takes the shop's share back itself:
//!
//! - `charge.dispute.created`: the order is `disputed` (paused; the shop's statuses are ignored
//!   meanwhile), the shop's share of the disputed amount is taken back from its transfer, then
//!   MakerFoundry's 10% of it goes back to the shop. The sales tax stays MakerFoundry's. The shop
//!   is emailed.
//! - `charge.dispute.closed`, `won` (or an inquiry closed, `warning_closed`): the order goes back
//!   to the status it had, and what was taken is transferred back (less the 10% the shop had
//!   back); `lost`: the order is a `chargeback` and stays reversed. The dispute fee stays
//!   MakerFoundry's (Seller Terms § 8). The shop is emailed either way.
//!
//! The event is recorded in the webhook's transaction (once, by its id); the Stripe calls come
//! after it (`settle`) and again from the sweep until each is done, each checked with Stripe first
//! and sent with an idempotency key, under a per-order lock. A step Stripe refuses alerts the
//! admins once. Not done here: telling the shop's program through StitchHub (the contract has no
//! message for it) and the owed-balance ledger (Seller Terms § 9) that doesn't exist yet.

use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};

use super::after::{OPEN, stripe_id};
use crate::AppState;
use crate::disputes::{alert_admins, give_fee_back, parts};
use crate::money::dollars;
use crate::selling::{self, MAKERFOUNDRY};
use crate::stripe::Stripe;

const REVERSAL: &str = "shop_dispute";
const FEE_BACK: &str = "shop_dispute_fee";
const RESTORED: &str = "shop_dispute_won";

/// Statuses a dispute pauses: the order is paid for and the money not already given back.
fn pausable(status: &str) -> bool {
    OPEN.contains(&status) || matches!(status, "paid" | "collected")
}

/// What a dispute of `cents` comes to: the disputed part of the price (what the ledger gives
/// back, as a refund's does), the shop's share of it (what is taken from the shop's transfer, at
/// most the transfer) and MakerFoundry's 10% of it (given back to the shop). The sales tax is
/// MakerFoundry's.
pub fn shop_parts(cents: i64, paid: i64, tax: i64, shop: i64, fee: i64) -> (i64, i64, i64) {
    let goods = paid.saturating_sub(tax);
    let (part, _, fee_part) = parts(cents, goods, tax, fee);
    let shop_part = if goods <= 0 || shop <= 0 {
        0
    } else if part >= goods {
        shop
    } else {
        i64::try_from(i128::from(shop) * i128::from(part) / i128::from(goods))
            .unwrap_or(shop)
            .clamp(0, shop)
    };
    (part, shop_part, fee_part)
}

fn dispute_ids(dispute: &Value) -> Option<(&str, &str)> {
    let get = |key: &str, prefix: &str| {
        dispute
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| stripe_id(s, prefix))
    };
    Some((get("id", "dp_")?, get("payment_intent", "pi_")?))
}

/// `charge.dispute.created` in the webhook's transaction, for a shop order's payment: the order
/// is `disputed`, once. `false` when the payment isn't a shop order's.
pub async fn record_created(conn: &mut PgConnection, dispute: &Value) -> Result<bool, sqlx::Error> {
    let Some((id, intent)) = dispute_ids(dispute) else {
        return Ok(false);
    };
    let amount = dispute.get("amount").and_then(Value::as_i64).unwrap_or(0).max(0);
    let order: Option<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, status, dispute_id FROM mf_shop_orders WHERE stripe_payment_intent = $1")
            .bind(intent)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((order, status, known)) = order else {
        return Ok(false);
    };
    match known {
        Some(k) if k == id => return Ok(true),
        Some(k) => {
            eprintln!("ALERT shop orders: order {order} has a second dispute {id} (after {k}): settle it by hand");
            return Ok(true);
        }
        None => {}
    }
    let pause = pausable(&status);
    if !pause {
        eprintln!("ALERT shop orders: dispute {id} on order {order}, which is {status}: settle it by hand");
    }
    sqlx::query(
        "UPDATE mf_shop_orders SET status = CASE WHEN $4 THEN 'disputed' ELSE status END,
                dispute_prev_status = CASE WHEN $4 THEN status ELSE '' END,
                dispute_id = $2, dispute_cents = $3, disputed_at = now(), updated_at = now()
         WHERE id = $1 AND dispute_id IS NULL",
    )
    .bind(order)
    .bind(id)
    .bind(amount)
    .bind(pause)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// `charge.dispute.closed` in the webhook's transaction, for a shop order's payment: won (or an
/// inquiry closed), the order goes back to its status; lost, it's a `chargeback`. A `closed` that
/// came before its `created` records both. `false` when the payment isn't a shop order's.
pub async fn record_closed(conn: &mut PgConnection, dispute: &Value) -> Result<bool, sqlx::Error> {
    if !record_created(conn, dispute).await? {
        return Ok(false);
    }
    let Some((id, _)) = dispute_ids(dispute) else {
        return Ok(false);
    };
    let outcome: String = dispute
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(40)
        .collect();
    let closed: Option<(i64, String)> = sqlx::query_as(
        "UPDATE mf_shop_orders SET dispute_outcome = $2, dispute_closed_at = now(), updated_at = now(),
                status = CASE WHEN status = 'disputed' AND $2 IN ('won', 'warning_closed')
                                   THEN coalesce(nullif(dispute_prev_status, ''), 'paid')
                              WHEN status = 'disputed' AND $2 = 'lost' THEN 'chargeback'
                              ELSE status END
         WHERE dispute_id = $1 AND dispute_closed_at IS NULL RETURNING id, status",
    )
    .bind(id)
    .bind(&outcome)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((order, status)) = closed
        && status == "disputed"
    {
        eprintln!("ALERT shop orders: dispute {id} on order {order} closed as '{outcome}': settle it by hand");
    }
    Ok(true)
}

/// After a dispute event is recorded: the shop order's Stripe steps. `false` when `intent` isn't
/// a shop order's.
pub async fn after_event(state: &AppState, pool: &PgPool, intent: &str) -> bool {
    let order: Option<i64> =
        sqlx::query_scalar("SELECT id FROM mf_shop_orders WHERE stripe_payment_intent = $1 AND dispute_id IS NOT NULL")
            .bind(intent)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let Some(order) = order else { return false };
    if let Err(e) = settle(state, pool, order).await {
        // (`settle` alerted the admins; the sweep tries again)
        eprintln!("shop orders: dispute of order {order}: {e}");
    }
    true
}

/// The sweep: disputes whose Stripe steps or email aren't done yet, up to 50 at a time.
pub async fn settle_all(state: &AppState, pool: &PgPool) -> Result<(), sqlx::Error> {
    let orders: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM mf_shop_orders WHERE dispute_id IS NOT NULL AND dispute_prev_status <> '' AND (
             (status IN ('disputed', 'chargeback') AND (dispute_reversed_at IS NULL OR dispute_fee_returned_at IS NULL))
          OR (status NOT IN ('disputed', 'chargeback') AND dispute_closed_at IS NOT NULL AND dispute_restored_at IS NULL)
          OR ($1 AND dispute_mailed <> CASE WHEN status = 'disputed' THEN 'disputed' WHEN status = 'chargeback' THEN 'lost'
                                            WHEN dispute_closed_at IS NOT NULL THEN 'won' ELSE dispute_mailed END))
         ORDER BY id LIMIT 50",
    )
    .bind(state.notices.is_some())
    .fetch_all(pool)
    .await?;
    for order in orders {
        if let Err(e) = settle(state, pool, order).await {
            eprintln!("shop orders: dispute of order {order}: {e}");
        }
    }
    Ok(())
}

/// A disputed order taken as far as it goes now: its Stripe steps (each once), then the shop's
/// email. `Err` says which step Stripe refused; the admins are alerted.
pub async fn settle(state: &AppState, pool: &PgPool, order: i64) -> Result<(), String> {
    let Some(stripe) = state.stripe.as_ref() else {
        return Ok(());
    };
    // one at a time per order (the webhook and the sweep): held until `lock` is dropped
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    let mine: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(99, ($1 % 2147483647)::int)")
        .bind(order)
        .fetch_one(&mut *lock)
        .await
        .map_err(|e| e.to_string())?;
    if !mine {
        return Ok(());
    }
    let steps = money_steps(stripe, pool, order).await;
    if let Err(e) = &steps {
        eprintln!("ALERT shop orders: dispute of order {order}: {e}");
        alert_once(state, pool, order, e).await;
    }
    mail_shop(state, pool, order).await;
    let _ = lock.rollback().await;
    steps
}

struct Disputed {
    status: String,
    reference: String,
    intent: Option<String>,
    shop: i64,
    cents: i64,
    paid: i64,
    tax: i64,
    shop_cents: i64,
    fee: i64,
    closed: bool,
    reversed: bool,
    fee_returned: bool,
    restored: bool,
    paused: bool,
}

async fn disputed(pool: &PgPool, order: i64) -> Result<Option<Disputed>, String> {
    let row = sqlx::query(
        "SELECT status, ref, stripe_payment_intent, shop_account, dispute_cents, paid_cents, tax_cents, shop_cents,
                fee_cents, dispute_closed_at IS NOT NULL AS closed, dispute_reversed_at IS NOT NULL AS reversed,
                dispute_fee_returned_at IS NOT NULL AS fee_returned, dispute_restored_at IS NOT NULL AS restored,
                dispute_prev_status <> '' AS paused
         FROM mf_shop_orders WHERE id = $1 AND dispute_id IS NOT NULL",
    )
    .bind(order)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    let Some(r) = row else { return Ok(None) };
    let get = |r: &sqlx::postgres::PgRow| -> Result<Disputed, sqlx::Error> {
        Ok(Disputed {
            status: r.try_get("status")?,
            reference: r.try_get("ref")?,
            intent: r.try_get("stripe_payment_intent")?,
            shop: r.try_get("shop_account")?,
            cents: r.try_get("dispute_cents")?,
            paid: r.try_get("paid_cents")?,
            tax: r.try_get("tax_cents")?,
            shop_cents: r.try_get("shop_cents")?,
            fee: r.try_get("fee_cents")?,
            closed: r.try_get("closed")?,
            reversed: r.try_get("reversed")?,
            fee_returned: r.try_get("fee_returned")?,
            restored: r.try_get("restored")?,
            paused: r.try_get("paused")?,
        })
    };
    get(&r).map(Some).map_err(|e| e.to_string())
}

async fn money_steps(stripe: &Stripe, pool: &PgPool, order: i64) -> Result<(), String> {
    let Some(d) = disputed(pool, order).await? else {
        return Ok(());
    };
    if !d.paused {
        // (a dispute on an order that was already given back: the admins settle it)
        return Ok(());
    }
    let (price, shop, fee) = shop_parts(d.cents, d.paid, d.tax, d.shop_cents, d.fee);
    match d.status.as_str() {
        "disputed" | "chargeback" => {
            if !d.reversed {
                if shop > 0 {
                    selling::reverse(stripe, d.intent.as_deref(), order, shop, REVERSAL).await?;
                }
                record(
                    pool,
                    order,
                    "dispute_reversed_at",
                    &[(d.shop, "shop", -price, "dispute")],
                )
                .await?;
            }
            if !d.fee_returned {
                if fee > 0 {
                    give_fee_back(stripe, d.intent.as_deref(), order, fee, FEE_BACK).await?;
                }
                record(
                    pool,
                    order,
                    "dispute_fee_returned_at",
                    &[
                        (d.shop, "shop", fee, "dispute_fee"),
                        (MAKERFOUNDRY, "fee", -fee, "dispute_fee"),
                    ],
                )
                .await?;
            }
            Ok(())
        }
        // won, or an inquiry closed: what was taken from the shop goes back
        _ if d.closed && !d.restored => {
            let mut reversed = d.reversed;
            if !reversed && shop > 0 {
                // (a reversal Stripe made that wasn't recorded here is recorded first)
                let transfer = selling::transfer_of(stripe, d.intent.as_deref()).await?;
                if selling::reversed(stripe, &transfer, order, REVERSAL).await? {
                    record(
                        pool,
                        order,
                        "dispute_reversed_at",
                        &[(d.shop, "shop", -price, "dispute")],
                    )
                    .await?;
                    reversed = true;
                }
            }
            if !reversed {
                // nothing was taken: nothing to give back
                return record(pool, order, "dispute_restored_at", &[]).await;
            }
            // the 10% the shop had back is MakerFoundry's again
            let fee_back = if d.fee_returned { fee } else { 0 };
            let back = shop.saturating_sub(fee_back);
            if back > 0 {
                transfer_back(stripe, pool, order, &d.reference, d.shop, back).await?;
            }
            let back = price.saturating_sub(fee_back);
            record(
                pool,
                order,
                "dispute_restored_at",
                &[
                    (d.shop, "shop", back, "dispute_won"),
                    (MAKERFOUNDRY, "fee", fee_back, "dispute_won"),
                ],
            )
            .await
        }
        _ => Ok(()),
    }
}

/// Steps recorded with their ledger rows, each once (the `*_at` column, and the ledger's own key).
async fn record(pool: &PgPool, order: i64, column: &str, rows: &[(i64, &str, i64, &str)]) -> Result<(), String> {
    let done = async {
        let mut tx = pool.begin().await?;
        // (`column` is one of this file's own names, never input)
        let first = sqlx::query(&format!(
            "UPDATE mf_shop_orders SET {column} = now() WHERE id = $1 AND {column} IS NULL"
        ))
        .bind(order)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if first > 0 {
            for &(account, kind, cents, reason) in rows {
                if cents != 0 {
                    sqlx::query(
                        "INSERT INTO mf_shop_ledger (order_id, account_id, kind, cents, reason)
                         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (order_id, kind, reason) DO NOTHING",
                    )
                    .bind(order)
                    .bind(account)
                    .bind(kind)
                    .bind(cents)
                    .bind(reason)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        tx.commit().await
    };
    done.await.map_err(|e: sqlx::Error| e.to_string())
}

/// A won dispute's money back to the shop's Stripe account, as a transfer in the order's own
/// transfer group (https://docs.stripe.com/api/transfers/create), unless Stripe has it already.
async fn transfer_back(
    stripe: &Stripe,
    pool: &PgPool,
    order: i64,
    reference: &str,
    shop: i64,
    cents: i64,
) -> Result<(), String> {
    let account = selling::seller(pool, shop)
        .await
        .map(|s| s.stripe_account)
        .filter(|a| selling::is_stripe_id(a, "acct_"))
        .ok_or_else(|| "the shop has no Stripe account.".to_string())?;
    let group = format!("mf-shop-dispute-{order}");
    let known = stripe
        .get(&format!("/v1/transfers?transfer_group={group}&limit=100"))
        .await
        .map_err(|e| e.to_string())?;
    let done = known.get("data").and_then(Value::as_array).is_some_and(|ts| {
        ts.iter().any(|t| {
            t.pointer("/metadata/shop_order").and_then(Value::as_str) == Some(reference)
                && t.pointer("/metadata/reason").and_then(Value::as_str) == Some(RESTORED)
        })
    });
    if done {
        return Ok(());
    }
    stripe
        .post(
            "/v1/transfers",
            &[
                ("amount", cents.to_string()),
                ("currency", "usd".to_string()),
                ("destination", account),
                ("transfer_group", group),
                ("metadata[shop_order]", reference.to_string()),
                ("metadata[reason]", RESTORED.into()),
                ("metadata[app]", selling::APP.into()),
            ],
            Some(&format!("mf-{RESTORED}-{order}")),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The admins emailed (once per order) that a dispute's Stripe step couldn't be done.
async fn alert_once(state: &AppState, pool: &PgPool, order: i64, why: &str) {
    let reference: Option<String> = sqlx::query_scalar(
        "UPDATE mf_shop_orders SET dispute_alerted_at = now() WHERE id = $1 AND dispute_alerted_at IS NULL RETURNING ref",
    )
    .bind(order)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some(reference) = reference {
        let text = format!(
            "A buyer disputed shop order {reference} and Stripe refused one of MakerFoundry's steps: {why}\n\nMakerFoundry tries again every minute or so. If the shop's Stripe balance is too low, the money may have to be recovered from the shop another way. The order is on {}/admin/orders.",
            state.public_url.trim_end_matches('/')
        );
        alert_admins(
            state,
            pool,
            &format!("Dispute on shop order {reference} needs a look"),
            &text,
        )
        .await;
    }
}

/// status, mailed, ref, email, closed, dispute cents, paid, tax, shop cents.
type MailRow = (String, String, String, String, bool, i64, i64, i64, i64);

/// The shop told what happened to its order, once for each state (disputed, won, lost).
async fn mail_shop(state: &AppState, pool: &PgPool, order: i64) {
    let Some(notices) = state.notices.as_ref() else {
        return;
    };
    let row: Option<MailRow> = sqlx::query_as(
        "SELECT o.status, o.dispute_mailed, o.ref, a.email, o.dispute_closed_at IS NOT NULL, o.dispute_cents,
                o.paid_cents, o.tax_cents, o.shop_cents
         FROM mf_shop_orders o JOIN mf_accounts a ON a.account_id = o.shop_account
         WHERE o.id = $1 AND o.dispute_id IS NOT NULL AND o.dispute_prev_status <> ''",
    )
    .bind(order)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((status, mailed, reference, email, closed, cents, paid, tax, shop_cents)) = row else {
        return;
    };
    let now = match status.as_str() {
        "disputed" => "disputed",
        "chargeback" => "lost",
        _ if closed => "won",
        _ => return,
    };
    if mailed == now || email.trim().is_empty() {
        return;
    }
    let (_, taken, _) = shop_parts(cents, paid, tax, shop_cents, 0);
    let (subject, words) = match now {
        "disputed" => (
            format!("A customer disputed the payment for order {reference}"),
            format!(
                "A customer has disputed their payment for order {reference} with their bank. The order is paused while the dispute is open.\n\nYour share of the disputed amount, {}, is taken back from your Stripe balance, and MakerFoundry's 10% of it goes back to you, as with a refund. If the dispute is decided in your favour, the money comes back to you.\n\nYou don't need to do anything now.",
                dollars(taken)
            ),
        ),
        "won" => (
            format!("The dispute about order {reference} is over"),
            format!(
                "The dispute about order {reference} has been closed in your favour. The money taken back from you is given back, and the order carries on."
            ),
        ),
        _ => (
            format!("The dispute about order {reference} went to the customer"),
            format!(
                "The customer's bank decided the dispute about order {reference} for the customer. The money stays with the customer; the order is closed."
            ),
        ),
    };
    let text = format!("Hello,\n\n{words}\n\nYou get this email because you take orders from MakerFoundry.\n");
    match notices.send_about_order(&email, &subject, &text).await {
        Ok(()) => {
            let _ = sqlx::query("UPDATE mf_shop_orders SET dispute_mailed = $2 WHERE id = $1")
                .bind(order)
                .bind(now)
                .execute(pool)
                .await
                .map_err(|e| eprintln!("shop orders: dispute email of order {order}: {e}"));
        }
        // (by the order only: never the address)
        Err(e) => eprintln!("shop orders: dispute email of order {order}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dispute_takes_the_shops_share_and_returns_the_ten_percent() {
        // the whole payment of the test order: shop 21_178, 10% 2_435, card fee 736, no tax
        assert_eq!(shop_parts(24_349, 24_349, 0, 21_178, 2_435), (24_349, 21_178, 2_435));
        // more than was paid is all of it
        assert_eq!(shop_parts(999_999, 24_349, 0, 21_178, 2_435), (24_349, 21_178, 2_435));
        // with tax, the tax stays out of both
        assert_eq!(shop_parts(1_100, 1_100, 100, 800, 100), (1_000, 800, 100));
        // half of it, in proportion, rounded down
        assert_eq!(shop_parts(500, 1_000, 0, 800, 100), (500, 400, 50));
        // nothing, or nonsense, is nothing
        for (c, p, t, s, f) in [
            (0, 1_000, 0, 800, 100),
            (-1, 1_000, 0, 800, 100),
            (5, 0, 0, 0, 0),
            (5, 1, 9, 1, 0),
        ] {
            let (_, a, b) = shop_parts(c, p, t, s, f);
            assert!(
                a >= 0 && a <= s.max(0) && b >= 0 && b <= f.max(0),
                "{c} {p} {t} {s} {f}"
            );
        }
        assert_eq!(shop_parts(i64::MAX, i64::MAX, i64::MAX, 5, 5), (0, 0, 0));
    }

    #[test]
    fn only_paid_orders_the_shop_still_holds_are_paused() {
        for s in ["sent", "received", "in_production", "shipped", "paid", "collected"] {
            assert!(pausable(s), "{s}");
        }
        for s in [
            "pending",
            "refunded",
            "cancelled",
            "expired",
            "abandoned",
            "disputed",
            "chargeback",
        ] {
            assert!(!pausable(s), "{s}");
        }
    }
}
