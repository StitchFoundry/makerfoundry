//! Chargebacks (issue #90). A sale is a destination charge with no `on_behalf_of`
//! (https://docs.stripe.com/connect/destination-charges), so MakerFoundry is the merchant of record
//! and Stripe takes a disputed payment, and its dispute fee, from MakerFoundry's balance
//! (https://docs.stripe.com/disputes/how-disputes-work). The seller's transfer isn't touched by
//! Stripe, so MakerFoundry takes it back itself, as on a refund (Seller Terms § 7):
//!
//! - `charge.dispute.created`: the order is `disputed`, so the buyer can't download it; the
//!   disputed price is taken back from the seller's transfer (a transfer reversal), then
//!   MakerFoundry's 10% of it goes back to the seller (a partial refund of the application fee), so
//!   the seller is out the card fee only; the seller is emailed.
//! - `charge.dispute.closed`, `won` (or an inquiry closed, `warning_closed`): the order is paid
//!   again and what was taken from the seller is transferred back (less the 10% they had back);
//!   `lost`: the order is a `chargeback`, still blocked. The seller is emailed either way.
//!
//! The event is recorded in the webhook's transaction (once, by its id); the Stripe calls come
//! after it (`settle`), and again from the sweep until each is done. Every call is checked with
//! Stripe first (the reversals on the transfer, the refunds of the application fee, the transfers
//! in the order's transfer group) and sent with an idempotency key, so none is ever made twice,
//! and runs under a per-order lock, so the webhook and the sweep never race. A step Stripe refuses
//! (the seller's balance too low, say) is logged and the admins are alerted; the webhook still
//! answers Stripe, and `/admin/orders` shows the dispute until it's settled.
//!
//! Orders from shops (MF-83) are disputed the same way at Stripe, but their money moves differently
//! (`shop_orders::after`); `shop_orders::disputes` settles those (issue #99).

use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};

use crate::AppState;
use crate::money::dollars;
use crate::selling::{self, MAKERFOUNDRY};
use crate::stripe::Stripe;

/// The reasons the dispute's Stripe calls carry (`metadata[reason]`), as the ledger names them.
const REVERSAL: &str = "dispute";
const FEE_BACK: &str = "dispute_fee";
const RESTORED: &str = "dispute_won";

/// What a dispute on an order disputes (`cents` of the price and tax the buyer paid) → the price
/// part (taken from the seller), the tax part (MakerFoundry's) and MakerFoundry's 10% of the price
/// part (given back to the seller). A dispute of everything is the whole price, tax and fee; part
/// of it is shared in proportion, rounded down.
pub fn parts(cents: i64, price: i64, tax: i64, fee: i64) -> (i64, i64, i64) {
    let total = price.saturating_add(tax);
    if cents <= 0 || price <= 0 || total <= 0 {
        return (0, 0, 0);
    }
    let cents = cents.min(total);
    let share = |of: i64, part: i64, whole: i64| -> i64 {
        if whole <= 0 {
            return 0;
        }
        i64::try_from(i128::from(of) * i128::from(part) / i128::from(whole))
            .unwrap_or(of)
            .clamp(0, of)
    };
    let p = if cents == total {
        price
    } else {
        share(price, cents, total)
    };
    let t = cents.saturating_sub(p).clamp(0, tax.max(0));
    let f = if p == price {
        fee.max(0)
    } else {
        share(fee.max(0), p, price)
    };
    (p, t, f)
}

fn id_of<'a>(dispute: &'a Value, key: &str, prefix: &str) -> Option<&'a str> {
    dispute
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| selling::is_stripe_id(s, prefix))
}

/// `charge.dispute.created` (or a `closed` that came first), in the webhook's transaction: the
/// order it disputes is `disputed`, once.
pub async fn record_created(conn: &mut PgConnection, dispute: &Value) -> Result<(), sqlx::Error> {
    let (Some(id), Some(intent)) = (id_of(dispute, "id", "dp_"), id_of(dispute, "payment_intent", "pi_")) else {
        return Ok(());
    };
    let amount = dispute.get("amount").and_then(Value::as_i64).unwrap_or(0).max(0);
    let order: Option<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, status, dispute_id FROM mf_orders WHERE stripe_payment_intent = $1")
            .bind(intent)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((order, status, known)) = order else {
        // (or an order from a shop: #99)
        crate::shop_orders::disputes::record_created(conn, dispute).await?;
        return Ok(());
    };
    match known {
        Some(k) if k == id => return Ok(()),
        Some(k) => {
            eprintln!("ALERT selling: order {order} has a second dispute {id} (after {k}): settle it by hand");
            return Ok(());
        }
        None => {}
    }
    if status != "paid" {
        // (refunded already, or never confirmed here: the admins look at it)
        eprintln!("ALERT selling: dispute {id} on order {order}, which is {status}: settle it by hand");
    }
    sqlx::query(
        "UPDATE mf_orders SET status = CASE WHEN status = 'paid' THEN 'disputed' ELSE status END,
                dispute_id = $2, dispute_cents = $3, disputed_at = now()
         WHERE id = $1 AND dispute_id IS NULL",
    )
    .bind(order)
    .bind(id)
    .bind(amount)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `charge.dispute.closed`, in the webhook's transaction: won (or an inquiry closed), the order is
/// paid again; lost, it's a chargeback. A `closed` that came before its `created` records both.
pub async fn record_closed(conn: &mut PgConnection, dispute: &Value) -> Result<(), sqlx::Error> {
    let Some(id) = id_of(dispute, "id", "dp_") else {
        return Ok(());
    };
    record_created(conn, dispute).await?;
    // (an order from a shop is closed in its own table: #99)
    if crate::shop_orders::disputes::record_closed(conn, dispute).await? {
        return Ok(());
    }
    let outcome: String = dispute
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(40)
        .collect();
    let closed: Option<(i64, String)> = sqlx::query_as(
        "UPDATE mf_orders SET dispute_outcome = $2, dispute_closed_at = now(),
                status = CASE WHEN status = 'disputed' AND $2 IN ('won', 'warning_closed') THEN 'paid'
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
        eprintln!("ALERT selling: dispute {id} on order {order} closed as '{outcome}': settle it by hand");
    }
    Ok(())
}

/// The PaymentIntent a dispute event names, for `settle` once the event is recorded.
pub fn disputed_intent(event: &Value) -> Option<String> {
    let kind = event.get("type").and_then(Value::as_str)?;
    if kind != "charge.dispute.created" && kind != "charge.dispute.closed" {
        return None;
    }
    id_of(event.pointer("/data/object")?, "payment_intent", "pi_").map(str::to_string)
}

/// After a dispute event is recorded: the order's Stripe steps, or (an order from a shop) the
/// admins alerted.
pub async fn after_event(state: &AppState, pool: &PgPool, intent: &str, _new: bool) {
    let order: Option<i64> =
        sqlx::query_scalar("SELECT id FROM mf_orders WHERE stripe_payment_intent = $1 AND dispute_id IS NOT NULL")
            .bind(intent)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    if let Some(order) = order {
        if let Err(e) = settle(state, pool, order).await {
            // (`settle` alerted the admins; the sweep tries again)
            eprintln!("selling: dispute of order {order}: {e}");
        }
        return;
    }
    // (an order from a shop: #99)
    crate::shop_orders::disputes::after_event(state, pool, intent).await;
}

/// The sweep: disputes whose Stripe steps or email aren't done yet, up to 50 at a time.
pub async fn settle_all(state: &AppState, pool: &PgPool) -> Result<(), sqlx::Error> {
    let orders: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM mf_orders WHERE dispute_id IS NOT NULL AND (
             (status IN ('disputed', 'chargeback') AND (dispute_reversed_at IS NULL OR dispute_fee_returned_at IS NULL))
          OR (status = 'paid' AND dispute_restored_at IS NULL)
          OR ($1 AND dispute_mailed <> CASE status WHEN 'disputed' THEN 'disputed' WHEN 'chargeback' THEN 'lost'
                                                   WHEN 'paid' THEN 'won' ELSE dispute_mailed END))
         ORDER BY id LIMIT 50",
    )
    // (emails only wait while there's a sender to send them)
    .bind(state.notices.is_some())
    .fetch_all(pool)
    .await?;
    for order in orders {
        if let Err(e) = settle(state, pool, order).await {
            eprintln!("selling: dispute of order {order}: {e}");
        }
    }
    crate::shop_orders::disputes::settle_all(state, pool).await
}

/// A disputed order taken as far as it goes now: its Stripe steps (each once), then the seller's
/// email. `Err` says which step Stripe refused; the admins are alerted.
pub async fn settle(state: &AppState, pool: &PgPool, order: i64) -> Result<(), String> {
    let Some(stripe) = state.stripe.as_ref() else {
        return Ok(());
    };
    // one at a time per order (the webhook and the sweep): held until `lock` is dropped
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    let mine: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(90, ($1 % 2147483647)::int)")
        .bind(order)
        .fetch_one(&mut *lock)
        .await
        .map_err(|e| e.to_string())?;
    if !mine {
        return Ok(());
    }
    let steps = money_steps(stripe, pool, order).await;
    if let Err(e) = &steps {
        eprintln!("ALERT selling: dispute of order {order}: {e}");
        alert_once(state, pool, order, e).await;
    }
    mail_seller(state, pool, order).await;
    let _ = lock.rollback().await;
    steps
}

struct Disputed {
    status: String,
    intent: Option<String>,
    seller: i64,
    currency: String,
    price: i64,
    tax: i64,
    fee: i64,
    cents: i64,
    reversed: bool,
    fee_returned: bool,
    restored: bool,
}

async fn disputed(pool: &PgPool, order: i64) -> Result<Option<Disputed>, String> {
    let row = sqlx::query(
        "SELECT status, stripe_payment_intent, seller_account, currency, price_cents, tax_cents, fee_cents,
                dispute_cents, dispute_reversed_at IS NOT NULL AS reversed,
                dispute_fee_returned_at IS NOT NULL AS fee_returned, dispute_restored_at IS NOT NULL AS restored
         FROM mf_orders WHERE id = $1 AND dispute_id IS NOT NULL",
    )
    .bind(order)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    let Some(r) = row else { return Ok(None) };
    let get = |r: &sqlx::postgres::PgRow| -> Result<Disputed, sqlx::Error> {
        Ok(Disputed {
            status: r.try_get("status")?,
            intent: r.try_get("stripe_payment_intent")?,
            seller: r.try_get("seller_account")?,
            currency: r.try_get("currency")?,
            price: r.try_get("price_cents")?,
            tax: r.try_get("tax_cents")?,
            fee: r.try_get("fee_cents")?,
            cents: r.try_get("dispute_cents")?,
            reversed: r.try_get("reversed")?,
            fee_returned: r.try_get("fee_returned")?,
            restored: r.try_get("restored")?,
        })
    };
    get(&r).map(Some).map_err(|e| e.to_string())
}

async fn money_steps(stripe: &Stripe, pool: &PgPool, order: i64) -> Result<(), String> {
    let Some(d) = disputed(pool, order).await? else {
        return Ok(());
    };
    let (price, tax, fee) = parts(d.cents, d.price, d.tax, d.fee);
    match d.status.as_str() {
        "disputed" | "chargeback" => {
            if !d.reversed {
                // the tax is MakerFoundry's whatever happens: taken first if it's still there
                selling::withhold_tax(stripe, pool, order).await?;
                if price > 0 {
                    selling::reverse(stripe, d.intent.as_deref(), order, price, REVERSAL).await?;
                }
                record_reversal(pool, order, d.seller, price, tax).await?;
            }
            if !d.fee_returned {
                if fee > 0 {
                    give_fee_back(stripe, d.intent.as_deref(), order, fee, FEE_BACK).await?;
                }
                record_fee_back(pool, order, d.seller, fee).await?;
            }
            Ok(())
        }
        // won, or an inquiry closed: what was taken from the seller goes back
        "paid" if !d.restored => {
            let mut reversed = d.reversed;
            if !reversed && price > 0 {
                // (a reversal Stripe made that wasn't recorded here is recorded first)
                let transfer = selling::transfer_of(stripe, d.intent.as_deref()).await?;
                if selling::reversed(stripe, &transfer, order, REVERSAL).await? {
                    record_reversal(pool, order, d.seller, price, tax).await?;
                    reversed = true;
                }
            }
            if !reversed {
                // nothing was taken: nothing to give back
                return mark(pool, order, "dispute_restored_at").await;
            }
            // the 10% the seller had back is kept by MakerFoundry again
            let fee_back = if fee_returned(pool, order).await? { fee } else { 0 };
            let back = price.saturating_sub(fee_back);
            if back > 0 {
                transfer_back(stripe, pool, order, d.seller, &d.currency, back).await?;
            }
            record_restored(pool, order, d.seller, back, fee_back, tax).await
        }
        _ => Ok(()),
    }
}

async fn fee_returned(pool: &PgPool, order: i64) -> Result<bool, String> {
    sqlx::query_scalar("SELECT dispute_fee_returned_at IS NOT NULL FROM mf_orders WHERE id = $1")
        .bind(order)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())
}

async fn mark(pool: &PgPool, order: i64, column: &str) -> Result<(), String> {
    // (`column` is one of this file's own names, never input)
    sqlx::query(&format!(
        "UPDATE mf_orders SET {column} = now() WHERE id = $1 AND {column} IS NULL"
    ))
    .bind(order)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Steps recorded with their ledger rows, each once (the `*_at` column, and the ledger's own key).
async fn record(pool: &PgPool, order: i64, column: &str, rows: &[(i64, &str, i64, &str)]) -> Result<(), String> {
    let done = async {
        let mut tx = pool.begin().await?;
        let first = sqlx::query(&format!(
            "UPDATE mf_orders SET {column} = now() WHERE id = $1 AND {column} IS NULL"
        ))
        .bind(order)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if first > 0 {
            for &(account, kind, cents, reason) in rows {
                if cents != 0 {
                    selling::ledger(&mut tx, order, account, kind, cents, reason).await?;
                }
            }
        }
        tx.commit().await
    };
    done.await.map_err(|e: sqlx::Error| e.to_string())
}

async fn record_reversal(pool: &PgPool, order: i64, seller: i64, price: i64, tax: i64) -> Result<(), String> {
    // the seller gives the price back; MakerFoundry has lost the tax with the buyer's payment
    record(
        pool,
        order,
        "dispute_reversed_at",
        &[
            (seller, "seller", -price, REVERSAL),
            (MAKERFOUNDRY, "tax", -tax, REVERSAL),
        ],
    )
    .await
}

async fn record_fee_back(pool: &PgPool, order: i64, seller: i64, fee: i64) -> Result<(), String> {
    record(
        pool,
        order,
        "dispute_fee_returned_at",
        &[(seller, "seller", fee, FEE_BACK), (MAKERFOUNDRY, "fee", -fee, FEE_BACK)],
    )
    .await
}

async fn record_restored(pool: &PgPool, order: i64, seller: i64, back: i64, fee: i64, tax: i64) -> Result<(), String> {
    record(
        pool,
        order,
        "dispute_restored_at",
        &[
            (seller, "seller", back, RESTORED),
            (MAKERFOUNDRY, "fee", fee, RESTORED),
            (MAKERFOUNDRY, "tax", tax, RESTORED),
        ],
    )
    .await
}

/// MakerFoundry's 10% of the disputed price back to the seller, as a partial refund of the sale's
/// application fee (https://docs.stripe.com/api/fee_refunds/create), unless Stripe has it already.
pub(crate) async fn give_fee_back(
    stripe: &Stripe,
    intent: Option<&str>,
    order: i64,
    fee: i64,
    reason: &str,
) -> Result<(), String> {
    let intent = intent
        .filter(|i| selling::is_stripe_id(i, "pi_"))
        .ok_or_else(|| "the order has no Stripe payment.".to_string())?;
    let payment = stripe
        .get(&format!("/v1/payment_intents/{intent}?expand[]=latest_charge"))
        .await
        .map_err(|e| e.to_string())?;
    let application_fee = payment
        .pointer("/latest_charge/application_fee")
        .and_then(Value::as_str)
        .filter(|f| selling::is_stripe_id(f, "fee_"))
        .ok_or_else(|| "Stripe shows no application fee for this sale.".to_string())?;
    let known = stripe
        .get(&format!("/v1/application_fees/{application_fee}"))
        .await
        .map_err(|e| e.to_string())?;
    let order_text = order.to_string();
    let done = known
        .pointer("/refunds/data")
        .and_then(Value::as_array)
        .is_some_and(|rs| {
            rs.iter().any(|r| {
                r.pointer("/metadata/order").and_then(Value::as_str) == Some(order_text.as_str())
                    && r.pointer("/metadata/reason").and_then(Value::as_str) == Some(reason)
            })
        });
    if done {
        return Ok(());
    }
    stripe
        .post(
            &format!("/v1/application_fees/{application_fee}/refunds"),
            &[
                ("amount", fee.to_string()),
                ("metadata[order]", order_text.clone()),
                ("metadata[reason]", reason.into()),
            ],
            Some(&format!("mf-{reason}-{order}")),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A won dispute's money back to the seller's Stripe account, as a transfer in the order's own
/// transfer group (https://docs.stripe.com/api/transfers/create), unless Stripe has it already.
async fn transfer_back(
    stripe: &Stripe,
    pool: &PgPool,
    order: i64,
    seller: i64,
    currency: &str,
    cents: i64,
) -> Result<(), String> {
    let account = selling::seller(pool, seller)
        .await
        .map(|s| s.stripe_account)
        .filter(|a| selling::is_stripe_id(a, "acct_"))
        .ok_or_else(|| "the seller has no Stripe account.".to_string())?;
    let group = format!("mf-dispute-{order}");
    let known = stripe
        .get(&format!("/v1/transfers?transfer_group={group}&limit=100"))
        .await
        .map_err(|e| e.to_string())?;
    let order_text = order.to_string();
    let done = known.get("data").and_then(Value::as_array).is_some_and(|ts| {
        ts.iter().any(|t| {
            t.pointer("/metadata/order").and_then(Value::as_str) == Some(order_text.as_str())
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
                ("currency", currency.to_string()),
                ("destination", account),
                ("transfer_group", group),
                ("metadata[order]", order_text),
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
    let first =
        sqlx::query("UPDATE mf_orders SET dispute_alerted_at = now() WHERE id = $1 AND dispute_alerted_at IS NULL")
            .bind(order)
            .execute(pool)
            .await
            .is_ok_and(|r| r.rows_affected() > 0);
    if first {
        let text = format!(
            "A buyer disputed order {order} and Stripe refused one of MakerFoundry's steps: {why}\n\nMakerFoundry tries again every 10 minutes. If the seller's Stripe balance is too low, the money may have to be recovered from them another way. The order is on {}/admin/orders.",
            state.public_url.trim_end_matches('/')
        );
        alert_admins(state, pool, &format!("Dispute on order {order} needs a look"), &text).await;
    }
}

pub(crate) async fn alert_admins(state: &AppState, pool: &PgPool, subject: &str, text: &str) {
    let Some(notices) = state.notices.as_ref() else {
        return;
    };
    let emails: Vec<String> = sqlx::query_scalar("SELECT email FROM mf_accounts WHERE account_id = ANY($1)")
        .bind(&state.admins)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    for email in emails.iter().filter(|e| !e.trim().is_empty()) {
        if let Err(e) = notices
            .send_about_order(email, &format!("MakerFoundry: {subject}"), text)
            .await
        {
            eprintln!("selling: alert to an admin: {e}");
        }
    }
}

/// The seller told what happened to their sale, once for each state (disputed, won, lost).
async fn mail_seller(state: &AppState, pool: &PgPool, order: i64) {
    let Some(notices) = state.notices.as_ref() else {
        return;
    };
    let row: Option<(String, String, String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT o.status, o.dispute_mailed, o.title, a.email, o.dispute_cents, o.price_cents, o.tax_cents
         FROM mf_orders o JOIN mf_accounts a ON a.account_id = o.seller_account
         WHERE o.id = $1 AND o.dispute_id IS NOT NULL",
    )
    .bind(order)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((status, mailed, title, email, cents, price, tax)) = row else {
        return;
    };
    let now = match status.as_str() {
        "disputed" => "disputed",
        "chargeback" => "lost",
        "paid" => "won",
        _ => return,
    };
    if mailed == now {
        return;
    }
    let (taken, _, _) = parts(cents, price, tax, 0);
    let link = format!("{}/account/sales", state.public_url.trim_end_matches('/'));
    let (subject, words) = match now {
        "disputed" => (
            format!("A buyer disputed their payment for {title}"),
            format!(
                "A buyer has disputed their payment for \"{title}\" (order {order}) with their bank. While the dispute is open they can't download the design.\n\nThe disputed price, {}, is taken back from your Stripe balance, and MakerFoundry's 10% of it goes back to you, as with a refund. If the dispute is decided in your favour, the money comes back to you.\n\nYou don't need to do anything now; MakerFoundry may ask you for something that shows the sale was real.",
                dollars(taken)
            ),
        ),
        "won" => (
            format!("The dispute about {title} is over"),
            format!(
                "The dispute about \"{title}\" (order {order}) has been closed in your favour. The money taken back from you is given back, and the buyer can download the design again."
            ),
        ),
        _ => (
            format!("The dispute about {title} went to the buyer"),
            format!(
                "The buyer's bank decided the dispute about \"{title}\" (order {order}) for the buyer. The money stays with the buyer and the design can't be downloaded on that order."
            ),
        ),
    };
    let text = format!(
        "Hello,\n\n{words}\n\nYour sales: {link}\n\nYou get this email because you sell designs on MakerFoundry.\n"
    );
    if email.trim().is_empty() {
        return;
    }
    match notices.send_about_order(&email, &subject, &text).await {
        Ok(()) => {
            let _ = sqlx::query("UPDATE mf_orders SET dispute_mailed = $2 WHERE id = $1")
                .bind(order)
                .bind(now)
                .execute(pool)
                .await
                .map_err(|e| eprintln!("selling: dispute email of order {order}: {e}"));
        }
        // (by the order only: never the address)
        Err(e) => eprintln!("selling: dispute email of order {order}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dispute_is_shared_between_the_price_the_tax_and_the_fee() {
        // all of it: the whole price, tax and 10%
        assert_eq!(parts(535, 500, 35, 50), (500, 35, 50));
        assert_eq!(parts(500, 500, 0, 50), (500, 0, 50));
        // more than was paid is all of it
        assert_eq!(parts(9_999, 500, 35, 50), (500, 35, 50));
        // part of it, in proportion, rounded down
        assert_eq!(parts(250, 500, 0, 50), (250, 0, 25));
        assert_eq!(parts(107, 500, 35, 50), (100, 7, 10));
        // nothing, or nonsense, is nothing
        for (c, p, t, f) in [
            (0, 500, 0, 50),
            (-5, 500, 0, 50),
            (100, 0, 0, 0),
            (i64::MAX, i64::MAX, i64::MAX, 50),
        ] {
            let (a, b, x) = parts(c, p, t, f);
            assert!(a >= 0 && b >= 0 && x >= 0, "{c} {p} {t} {f}");
            assert!(a <= p.max(0) && x <= f.max(0));
        }
        assert_eq!(parts(1, 1, 0, 0), (1, 0, 0));
    }
}
