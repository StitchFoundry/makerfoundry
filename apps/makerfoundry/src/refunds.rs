//! Refunds made outside MakerFoundry's admin, and the sellers' payout delay (issue #91, the Stripe
//! Connect audit's fixes 4 and 5).
//!
//! A refund made in the Stripe Dashboard reaches MakerFoundry only as `charge.refunded`. Stripe
//! reverses the seller's transfer only if "Reverse transfer" was ticked
//! (https://docs.stripe.com/connect/destination-charges#issue-refunds); otherwise MakerFoundry
//! would pay the buyer back while the seller kept the money. So, for every refund on a sale that
//! MakerFoundry didn't make itself:
//! - a partial one is recorded (`record_partial`: the ledger takes the refunded price from the
//!   seller, the sale goes on) and MakerFoundry's 10% of it goes back to the seller, as on a full
//!   refund (Seller Terms § 7);
//! - if Stripe made no transfer reversal for it, MakerFoundry reverses the refunded price itself;
//! - a full one gives the rest of the 10% back, as the admin's refund does.
//!
//! The checks run after the event is recorded (`after_event`) and again from the sweep until done
//! (`refund_check_due`); each Stripe call is checked with Stripe first and idempotent. A step
//! Stripe refuses is logged and the admins alerted once; the webhook still answers.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};

use crate::AppState;
use crate::accounts::{forbidden, same_origin};
use crate::disputes::parts;
use crate::selling::{self, MAKERFOUNDRY};

/// `charge.refunded` for part of a sale, `so_far` cents refunded in all (price and tax), in the
/// webhook's transaction: the part not yet recorded is taken from the seller in the ledger, once
/// (events come out of order and twice: only a larger total counts).
pub async fn record_partial(conn: &mut PgConnection, order: i64, so_far: i64) -> Result<(), sqlx::Error> {
    let row = sqlx::query(
        "SELECT seller_account, price_cents, tax_cents, fee_cents, refunded_cents FROM mf_orders
         WHERE id = $1 AND status = 'paid' FOR UPDATE",
    )
    .bind(order)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(()) };
    let seller: i64 = row.try_get("seller_account")?;
    let price: i64 = row.try_get("price_cents")?;
    let tax: i64 = row.try_get("tax_cents")?;
    let fee: i64 = row.try_get("fee_cents")?;
    let before: i64 = row.try_get("refunded_cents")?;
    let so_far = so_far.min(price.saturating_add(tax));
    if so_far <= before {
        return Ok(());
    }
    let (p0, t0, _) = parts(before, price, tax, fee);
    let (p1, t1, _) = parts(so_far, price, tax, fee);
    sqlx::query("UPDATE mf_orders SET refunded_cents = $2 WHERE id = $1")
        .bind(order)
        .bind(so_far)
        .execute(&mut *conn)
        .await?;
    let key = so_far.to_string();
    if p1 > p0 {
        selling::ledger_ref(conn, order, seller, "seller", -(p1 - p0), "partial_refund", &key).await?;
    }
    if t1 > t0 {
        selling::ledger_ref(conn, order, MAKERFOUNDRY, "tax", -(t1 - t0), "partial_refund", &key).await?;
    }
    Ok(())
}

/// After `charge.refunded` is recorded: the order's refunds checked with Stripe.
pub async fn after_event(state: &AppState, pool: &PgPool, intent: &str) {
    let order: Option<i64> =
        sqlx::query_scalar("SELECT id FROM mf_orders WHERE stripe_payment_intent = $1 AND refund_check_due")
            .bind(intent)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    if let Some(order) = order {
        check(state, pool, order).await;
    }
}

/// The sweep: orders whose refunds aren't checked yet, up to 50 at a time.
pub async fn check_all(state: &AppState, pool: &PgPool) -> Result<(), sqlx::Error> {
    let orders: Vec<i64> = sqlx::query_scalar("SELECT id FROM mf_orders WHERE refund_check_due ORDER BY id LIMIT 50")
        .fetch_all(pool)
        .await?;
    for order in orders {
        check(state, pool, order).await;
    }
    Ok(())
}

async fn check(state: &AppState, pool: &PgPool, order: i64) {
    match settle(state, pool, order).await {
        // a refund still pending: checked again by the sweep, nothing to alert
        Ok(false) => {}
        Ok(true) => {
            let _ = sqlx::query("UPDATE mf_orders SET refund_check_due = false WHERE id = $1")
                .bind(order)
                .execute(pool)
                .await
                .map_err(|e| eprintln!("selling: refunds of order {order}: {e}"));
        }
        Err(e) => {
            eprintln!("ALERT selling: refunds of order {order}: {e}");
            let first = sqlx::query(
                "UPDATE mf_orders SET refund_alerted_at = now() WHERE id = $1 AND refund_alerted_at IS NULL",
            )
            .bind(order)
            .execute(pool)
            .await
            .is_ok_and(|r| r.rows_affected() > 0);
            if first {
                let text = format!(
                    "Order {order} was refunded outside MakerFoundry's admin (in the Stripe Dashboard) and Stripe refused one of MakerFoundry's steps: {e}\n\nMakerFoundry tries again every 10 minutes. Dashboard refunds should tick \"Reverse transfer\". The order is on {}/admin/orders.",
                    state.public_url.trim_end_matches('/')
                );
                crate::disputes::alert_admins(state, pool, &format!("Refund of order {order} needs a look"), &text)
                    .await;
            }
        }
    }
}

/// An order's status, payment, seller, price, tax, 10% and whether its 10% is still to go back.
type Sale = (String, Option<String>, i64, i64, i64, i64, bool);

/// The order's refunds as Stripe lists them, each one MakerFoundry didn't make taken care of once
/// it has succeeded; → false while one is still pending or waiting on the buyer (Stripe holds a
/// card refund as pending while the balance is low, https://docs.stripe.com/refunds), so the
/// sweep checks the order again.
async fn settle(state: &AppState, pool: &PgPool, order: i64) -> Result<bool, String> {
    let Some(stripe) = state.stripe.as_ref() else {
        return Ok(true);
    };
    let row = sqlx::query(
        "SELECT status, stripe_payment_intent, seller_account, price_cents, tax_cents, fee_cents,
                fee_returned_at IS NULL AS fee_waiting
         FROM mf_orders WHERE id = $1",
    )
    .bind(order)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    let Some(row) = row else { return Ok(true) };
    let get = |r: &sqlx::postgres::PgRow| -> Result<Sale, sqlx::Error> {
        Ok((
            r.try_get("status")?,
            r.try_get("stripe_payment_intent")?,
            r.try_get("seller_account")?,
            r.try_get("price_cents")?,
            r.try_get("tax_cents")?,
            r.try_get("fee_cents")?,
            r.try_get("fee_waiting")?,
        ))
    };
    let (status, intent, seller, price, tax, fee, fee_waiting) = get(&row).map_err(|e| e.to_string())?;
    let intent = intent
        .filter(|i| selling::is_stripe_id(i, "pi_"))
        .ok_or_else(|| "the order has no Stripe payment.".to_string())?;
    let payment = stripe
        .get(&format!("/v1/payment_intents/{intent}?expand[]=latest_charge"))
        .await
        .map_err(|e| e.to_string())?;
    let charge = payment
        .pointer("/latest_charge/id")
        .and_then(Value::as_str)
        .filter(|c| selling::is_stripe_id(c, "ch_"))
        .ok_or_else(|| "Stripe shows no charge for this sale.".to_string())?;
    let list = stripe
        .get(&format!("/v1/refunds?charge={charge}&limit=100"))
        .await
        .map_err(|e| e.to_string())?;
    let refunds = list.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut waiting = false;
    for r in &refunds {
        let ours = r.pointer("/metadata/order").is_some() || r.pointer("/metadata/shop_order").is_some();
        let id = r.get("id").and_then(Value::as_str).unwrap_or("");
        let refund_status = r.get("status").and_then(Value::as_str);
        if !ours && matches!(refund_status, Some("pending" | "requires_action")) {
            waiting = true;
        }
        if ours || refund_status != Some("succeeded") || !selling::is_stripe_id(id, "re_") {
            continue;
        }
        let amount = r.get("amount").and_then(Value::as_i64).unwrap_or(0);
        let (p, _, f) = parts(amount, price, tax, fee);
        let reversed_by_stripe = r.get("transfer_reversal").and_then(Value::as_str).is_some();
        if !reversed_by_stripe && p > 0 {
            selling::reverse(stripe, Some(&intent), order, p, &format!("refund_{id}")).await?;
        }
        // a partial refund: its share of the 10% back to the seller (a full one: below)
        if status == "paid" && f > 0 {
            crate::disputes::give_fee_back(stripe, Some(&intent), order, f, &format!("refund_fee_{id}")).await?;
            let done = async {
                let mut tx = pool.begin().await?;
                selling::ledger_ref(&mut tx, order, seller, "seller", f, "fee_return", id).await?;
                selling::ledger_ref(&mut tx, order, MAKERFOUNDRY, "fee", -f, "fee_return", id).await?;
                tx.commit().await
            };
            done.await.map_err(|e: sqlx::Error| e.to_string())?;
        }
    }
    if waiting {
        return Ok(false);
    }
    if status == "refunded" && fee_waiting {
        selling::return_fee(stripe, pool, order).await?;
    }
    Ok(true)
}

/// An admin's one-off task (issue #91): every seller's Stripe account given the payout delay
/// (`MF_PAYOUT_DELAY_DAYS`, monthly on the 1st as before), for accounts made before it existed.
pub async fn payout_delay(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (_, pool) = match crate::moderation::admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let Some(stripe) = state.stripe.as_ref() else {
        return selling::order_list(&pool, "Selling isn't switched on.").await;
    };
    let days = i32::try_from(stripe.payout_delay_days).unwrap_or(i32::MAX);
    let sellers: Vec<(i64, String)> = sqlx::query_as(
        "SELECT account_id, stripe_account FROM mf_sellers WHERE payout_delay_days IS DISTINCT FROM $1 ORDER BY account_id LIMIT 200",
    )
    .bind(days)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    let (mut done, mut failed) = (0, 0);
    for (account, acct) in sellers {
        if !selling::is_stripe_id(&acct, "acct_") {
            failed += 1;
            continue;
        }
        let set = stripe
            .post(
                &format!("/v1/accounts/{acct}"),
                &[
                    ("settings[payouts][schedule][interval]", "monthly".into()),
                    ("settings[payouts][schedule][monthly_anchor]", "1".into()),
                    ("settings[payouts][schedule][delay_days]", days.to_string()),
                ],
                None,
            )
            .await;
        match set {
            Ok(_) => {
                done += 1;
                let _ = sqlx::query("UPDATE mf_sellers SET payout_delay_days = $2 WHERE account_id = $1")
                    .bind(account)
                    .bind(days)
                    .execute(&pool)
                    .await
                    .map_err(|e| eprintln!("selling: payout delay of {account}: {e}"));
            }
            Err(e) => {
                failed += 1;
                eprintln!("selling: payout delay of {account}: {e}");
            }
        }
    }
    let note = if failed == 0 {
        format!("The {days}-day payout delay is set on {done} more seller accounts.")
    } else {
        format!(
            "The {days}-day payout delay is set on {done} more seller accounts; {failed} couldn't be changed (see the log): try again."
        )
    };
    selling::order_list(&pool, &note).await
}
