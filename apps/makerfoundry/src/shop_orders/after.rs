//! After an order is paid (MF-84): what the shop reports (`order.status`, `proof.sent`,
//! `order.cancel`) and what the hub gives up on (`message.expired`) applied to the order; the
//! customer's answers to proofs and cancelling; and refunds.
//!
//! A message that can never apply (an order that isn't this shop's, a status from the past) is
//! logged by its id and left, so it doesn't block the rest; only a failure that may pass (the
//! database, the hub, storage) is an `Err`, and the message comes again.
//!
//! Refunds are as for designs (Seller Terms § 7): the whole payment back to the customer with the
//! transfer to the shop reversed, then MakerFoundry's 10% given back to the shop, so the shop is
//! out only the card fee. A refund is due when the order is cancelled before production (by
//! either side) or the shop never took it (`message.expired` for `order.placed`); it's tried at
//! once and then by the sweep until Stripe has done it, each step recorded once.

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde_json::Value;

use sqlx::{PgConnection, PgPool, Row};

use super::contract::{self, Envelope, Message, MessageExpired, OrderCancel, OrderStatus, ProofAnswer, ProofSent};
use super::orders::{Order, order};
use crate::AppState;
use crate::accounts::{current_user, forbidden, same_origin};
use crate::pages::escape;

/// Where an order is while a shop still has it.
const OPEN: &[&str] = &[
    "sent",
    "received",
    "proof_sent",
    "approved",
    "in_production",
    "ready",
    "shipped",
];
/// Before production: either side may cancel, for a full refund.
const BEFORE_PRODUCTION: &[&str] = &["sent", "received", "proof_sent", "approved"];
/// Done with: the art is deleted 30 days after.
const CLOSED: &[&str] = &["collected", "shipped", "cancelled", "refunded", "expired"];
/// The largest mock-up taken (as the contract's).
pub const MAX_MOCKUP_BYTES: usize = 2 * 1024 * 1024;
/// How long a message to a shop waits at the hub.
const MESSAGE_DAYS: i64 = 7;

fn ignored(env: &Envelope, why: &str) -> Result<(), String> {
    // (by id only: never the message's text)
    eprintln!("stitchhub: {} {} left: {why}", env.kind, env.id);
    Ok(())
}

/// The order a shop's message is about, when it's that shop's: its id and status.
async fn shops_order(pool: &PgPool, env: &Envelope) -> Result<Option<(i64, String)>, String> {
    sqlx::query_as("SELECT id, status FROM mf_shop_orders WHERE ref = $1 AND shop_party = $2")
        .bind(&env.conversation)
        .bind(env.sender())
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())
}

/// Where a status comes in an order's life: a shop's status never takes an order back past where
/// it is, except between its proofs (a new proof after an approval, or the other way round).
fn stage(status: &str) -> usize {
    match status {
        "sent" => 0,
        "received" | "proof_sent" | "approved" => 1,
        "in_production" => 2,
        "ready" => 3,
        "shipped" => 4,
        _ => 5,
    }
}

/// Whether a shop may move an order from `now` to `next`.
pub fn moves_on(now: &str, next: &str) -> bool {
    stage(next) >= stage(now)
}

/// `order.status` from the shop.
pub async fn status(pool: &PgPool, env: &Envelope, s: &OrderStatus) -> Result<(), String> {
    let Some((id, now)) = shops_order(pool, env).await? else {
        return ignored(env, "not this shop's order");
    };
    if !OPEN.contains(&now.as_str()) {
        return ignored(env, "the order is closed");
    }
    if !moves_on(&now, &s.status) {
        return ignored(env, "a status from the past");
    }
    // (only from the status just read: a cancel that came in between stands)
    let done = sqlx::query(
        "UPDATE mf_shop_orders SET status = $2, tracking = $3, shop_note = $4, updated_at = now()
         WHERE id = $1 AND status = $5",
    )
    .bind(id)
    .bind(&s.status)
    .bind(&s.tracking)
    .bind(&s.note)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    if done.rows_affected() == 0 {
        // (it changed meanwhile: the message comes again and is read against the new status)
        return Err("the order changed meanwhile".into());
    }
    // the customer is emailed (issue #82), once for this message
    super::mail::note(pool, id, &env.id, &s.status).await
}

/// `proof.sent` from the shop: the mock-up fetched from the hub's files, checked by size and
/// SHA-256, re-drawn and kept.
pub async fn proof(state: &AppState, pool: &PgPool, env: &Envelope, p: &ProofSent) -> Result<(), String> {
    let Some((id, now)) = shops_order(pool, env).await? else {
        return ignored(env, "not this shop's order");
    };
    if !["sent", "received", "proof_sent", "approved"].contains(&now.as_str()) {
        return ignored(env, "the order is past its proofs");
    }
    let known: Option<i32> = sqlx::query_scalar("SELECT proof FROM mf_shop_proofs WHERE order_id = $1 AND proof = $2")
        .bind(id)
        .bind(i32::try_from(p.proof).unwrap_or(i32::MAX))
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
    if known.is_some() {
        return Ok(());
    }
    if p.mockup.size > MAX_MOCKUP_BYTES as u64 {
        return ignored(env, "a mock-up too large");
    }
    let (Some(hub), Some(storage)) = (state.stitchhub.as_ref(), state.storage.as_ref()) else {
        return Err("proofs need storage".into());
    };
    let bytes = match hub
        .download(&env.conversation, &p.mockup.sha256, p.mockup.size, MAX_MOCKUP_BYTES)
        .await
    {
        Ok(b) => b,
        // (a file that will never be right is left; the hub being away is tried again)
        Err(why)
            if why.contains("isn't the one described") || why.contains("too large") || why.contains("no such file") =>
        {
            return ignored(env, "the mock-up isn't the one described");
        }
        Err(why) => return Err(why),
    };
    let drawn = tokio::task::spawn_blocking(move || {
        let out = crate::photos::reencode(&bytes)?;
        let kind = crate::photos::check(&out)?;
        Ok::<_, String>((out, kind))
    })
    .await
    .map_err(|e| e.to_string())?;
    let Ok((image, kind)) = drawn else {
        return ignored(env, "the mock-up isn't a PNG or JPEG");
    };
    let key = crate::storage::new_key("shop-proof")?;
    storage.put(&key, image, kind).await?;
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO mf_shop_proofs (order_id, proof, image_key, image_type, note) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(i32::try_from(p.proof).unwrap_or(i32::MAX))
    .bind(&key)
    .bind(kind)
    .bind(&p.note)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query(
        "UPDATE mf_shop_orders SET status = 'proof_sent', updated_at = now() WHERE id = $1 AND status = ANY($2)",
    )
    .bind(id)
    .bind(BEFORE_PRODUCTION)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// `order.cancel` from the shop: before production a full refund; after, the refund is the shop's
/// to decide (Seller Terms), so none is made here.
pub async fn shop_cancelled(state: &AppState, pool: &PgPool, env: &Envelope, c: &OrderCancel) -> Result<(), String> {
    let Some((id, now)) = shops_order(pool, env).await? else {
        return ignored(env, "not this shop's order");
    };
    if c.by != "shop" || !OPEN.contains(&now.as_str()) {
        return ignored(env, "not the shop's to cancel now");
    }
    let refund = BEFORE_PRODUCTION.contains(&now.as_str());
    sqlx::query(
        "UPDATE mf_shop_orders SET status = 'cancelled', cancelled_by = 'shop', cancel_why = $2, refund_due = $3,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(&c.why)
    .bind(refund)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    if refund {
        settle_now(state, pool, id).await;
    }
    Ok(())
}

/// `message.expired` from the hub: an `order.placed` no shop took is refunded in full.
pub async fn expired(state: &AppState, pool: &PgPool, env: &Envelope, e: &MessageExpired) -> Result<(), String> {
    if env.from.as_deref() != Some("hub") {
        return ignored(env, "not from the hub");
    }
    let (true, reference) = (e.kind == "order.placed", e.conversation.as_str()) else {
        // (an answer or a cancel the shop never took: the order shows what it is)
        return ignored(env, "nothing to undo");
    };
    let id: Option<i64> = sqlx::query_scalar(
        "UPDATE mf_shop_orders SET status = 'expired', refund_due = true, updated_at = now()
         WHERE ref = $1 AND status = 'sent' RETURNING id",
    )
    .bind(reference)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    if let Some(id) = id {
        settle_now(state, pool, id).await;
    }
    Ok(())
}

// ---------------------------------------------------------------- refunds

/// A refund Stripe has made, recorded once: the order's payment back to the customer, the price
/// taken from the shop and the sales tax from MakerFoundry. `false` when it was recorded before.
pub async fn record_refund(conn: &mut PgConnection, id: i64) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "UPDATE mf_shop_orders SET refunded_at = now(), updated_at = now(),
             status = CASE WHEN status = ANY($2) THEN 'refunded' ELSE status END
         WHERE id = $1 AND refunded_at IS NULL AND paid_at IS NOT NULL
         RETURNING shop_account, paid_cents, tax_cents",
    )
    .bind(id)
    .bind(OPEN)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(false) };
    let tax: i64 = row.try_get("tax_cents")?;
    let price = row.try_get::<i64, _>("paid_cents")? - tax;
    ledger(conn, id, row.try_get("shop_account")?, "shop", -price, "refund").await?;
    if tax > 0 {
        ledger(conn, id, crate::selling::MAKERFOUNDRY, "tax", -tax, "refund").await?;
    }
    Ok(true)
}

/// `charge.refunded` for a shop order's payment (a full refund): recorded once, and the refund
/// made due, so `settle` (from the sweep) checks the transfer to the shop was reversed and gives
/// the 10% back, as for a refund MakerFoundry makes (issue #106: one made in the Stripe Dashboard
/// otherwise left the shop with the transfer, or without its 10%).
pub async fn refunded_by_stripe(conn: &mut PgConnection, intent: &str) -> Result<(), sqlx::Error> {
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM mf_shop_orders WHERE stripe_payment_intent = $1")
        .bind(intent)
        .fetch_optional(&mut *conn)
        .await?;
    if let Some(id) = id {
        record_refund(conn, id).await?;
        sqlx::query("UPDATE mf_shop_orders SET refund_due = true WHERE id = $1 AND paid_at IS NOT NULL")
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// After a new `charge.refunded` for part of a shop order's payment (made in the Stripe Dashboard;
/// MakerFoundry only refunds shop orders in full): the admins are told, since the shop's share
/// isn't taken back for it (issue #106).
pub async fn partly_refunded(state: &AppState, pool: &PgPool, charge: &Value) {
    if charge.get("refunded").and_then(Value::as_bool) == Some(true)
        || charge.get("amount_refunded").and_then(Value::as_i64).unwrap_or(0) <= 0
    {
        return;
    }
    let Some(intent) = charge.get("payment_intent").and_then(Value::as_str) else {
        return;
    };
    let reference: Option<String> =
        sqlx::query_scalar("SELECT ref FROM mf_shop_orders WHERE stripe_payment_intent = $1 AND paid_at IS NOT NULL")
            .bind(intent)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let Some(reference) = reference else { return };
    eprintln!("ALERT shop orders: {reference} was partly refunded outside MakerFoundry");
    let text = format!(
        "Shop order {reference} was partly refunded outside MakerFoundry (in the Stripe Dashboard). MakerFoundry refunds shop orders only in full, so the shop's share and the ledger weren't changed for it; if the rest is refunded later, MakerFoundry takes each refund back from the shop's transfer itself (unless Stripe did). The order is on {}/admin/orders.",
        state.public_url.trim_end_matches('/')
    );
    crate::disputes::alert_admins(
        state,
        pool,
        &format!("Partial refund of shop order {reference} needs a look"),
        &text,
    )
    .await;
}

/// Refunds of `intent`'s charge made outside MakerFoundry (the Stripe Dashboard) without "Reverse
/// transfer": the refunded amount taken back from the shop's transfer, once for each
/// (https://docs.stripe.com/connect/destination-charges#issue-refunds).
async fn reverse_outside_refunds(stripe: &crate::stripe::Stripe, intent: &str, id: i64) -> Result<(), String> {
    let payment = stripe
        .get(&format!("/v1/payment_intents/{intent}?expand[]=latest_charge"))
        .await
        .map_err(|e| e.to_string())?;
    let charge = payment
        .pointer("/latest_charge/id")
        .and_then(Value::as_str)
        .filter(|c| stripe_id(c, "ch_"))
        .ok_or("Stripe shows no charge for this order")?;
    let list = stripe
        .get(&format!("/v1/refunds?charge={charge}&limit=100"))
        .await
        .map_err(|e| e.to_string())?;
    for r in list.get("data").and_then(Value::as_array).into_iter().flatten() {
        let ours = r.pointer("/metadata/shop_order").and_then(Value::as_str).is_some();
        let refund = r.get("id").and_then(Value::as_str).unwrap_or("");
        if ours || r.get("status").and_then(Value::as_str) != Some("succeeded") || !stripe_id(refund, "re_") {
            continue;
        }
        let amount = r.get("amount").and_then(Value::as_i64).unwrap_or(0);
        if amount > 0 && r.get("transfer_reversal").and_then(Value::as_str).is_none() {
            crate::selling::reverse(stripe, Some(intent), id, amount, &format!("shop_refund_{refund}")).await?;
        }
    }
    Ok(())
}

async fn ledger(
    conn: &mut PgConnection,
    id: i64,
    account: i64,
    kind: &str,
    cents: i64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO mf_shop_ledger (order_id, account_id, kind, cents, reason) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (order_id, kind, reason) DO NOTHING",
    )
    .bind(id)
    .bind(account)
    .bind(kind)
    .bind(cents)
    .bind(reason)
    .execute(conn)
    .await
    .map(|_| ())
}

fn stripe_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix) && id.len() <= 255 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A due refund taken as far as it goes now: Stripe's refund, then the 10% back to the shop.
pub async fn settle(state: &AppState, pool: &PgPool, id: i64) -> Result<(), String> {
    let stripe = state.stripe.as_ref().ok_or("Stripe isn't switched on")?;
    let row = sqlx::query(
        "SELECT ref, stripe_payment_intent, fee_cents, tax_cents, refunded_at IS NOT NULL AS refunded
         FROM mf_shop_orders WHERE id = $1 AND refund_due AND paid_at IS NOT NULL AND fee_returned_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    let Some(row) = row else { return Ok(()) };
    let reference: String = row.try_get("ref").map_err(|e| e.to_string())?;
    let intent: String = row
        .try_get::<Option<String>, _>("stripe_payment_intent")
        .ok()
        .flatten()
        .filter(|i| stripe_id(i, "pi_"))
        .ok_or("the order has no Stripe payment")?;
    let fee: i64 = row.try_get("fee_cents").map_err(|e| e.to_string())?;
    // the whole transfer comes back from the shop, so the tax MakerFoundry took as part of its
    // application fee goes back to the shop with the 10%
    let tax: i64 = row.try_get("tax_cents").map_err(|e| e.to_string())?;
    if !row.try_get::<bool, _>("refunded").map_err(|e| e.to_string())? {
        let r = stripe
            .post(
                "/v1/refunds",
                &[
                    ("payment_intent", intent.clone()),
                    ("reverse_transfer", "true".into()),
                    // (the 10% goes back below, without the card fee)
                    ("refund_application_fee", "false".into()),
                    ("metadata[shop_order]", reference.clone()),
                    ("metadata[app]", crate::selling::APP.into()),
                ],
                Some(&format!("mf-shop-refund-{reference}")),
            )
            .await
            .map_err(|e| e.to_string())?;
        if r.get("status").and_then(Value::as_str) == Some("failed") {
            return Err("Stripe couldn't make the refund".into());
        }
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        record_refund(&mut tx, id).await.map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
    } else {
        // refunded by Stripe before MakerFoundry did: perhaps in the Dashboard (issue #106)
        reverse_outside_refunds(stripe, &intent, id).await?;
    }
    if tax > 0 {
        super::tax::reverse(stripe, pool, id).await?;
    }
    if fee + tax > 0 {
        let payment = stripe
            .get(&format!("/v1/payment_intents/{intent}?expand[]=latest_charge"))
            .await
            .map_err(|e| e.to_string())?;
        let application_fee = payment
            .pointer("/latest_charge/application_fee")
            .and_then(Value::as_str)
            .filter(|f| stripe_id(f, "fee_"))
            .ok_or("Stripe shows no application fee")?;
        stripe
            .post(
                &format!("/v1/application_fees/{application_fee}/refunds"),
                &[
                    ("amount", (fee + tax).to_string()),
                    ("metadata[shop_order]", reference.clone()),
                ],
                Some(&format!("mf-shop-fee-back-{reference}")),
            )
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let shop: Option<i64> = sqlx::query_scalar(
        "UPDATE mf_shop_orders SET fee_returned_at = now() WHERE id = $1 AND fee_returned_at IS NULL RETURNING shop_account",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    if let Some(shop) = shop {
        ledger(&mut tx, id, shop, "shop", fee, "fee_return")
            .await
            .map_err(|e| e.to_string())?;
        ledger(&mut tx, id, crate::selling::MAKERFOUNDRY, "fee", -fee, "fee_return")
            .await
            .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())
}

/// Payments Stripe took for an order that couldn't take them (`orders::confirm_paid` records
/// them): refunded in full, the transfer to the shop and MakerFoundry's fee with them. One that
/// turns out to be an order's own payment after all is left alone.
async fn refund_strays(stripe: &crate::stripe::Stripe, pool: &PgPool) -> Result<(), String> {
    let strays: Vec<(String, String)> = sqlx::query_as(
        "UPDATE mf_shop_stray_payments SET tried_at = now()
         WHERE payment_intent IN (
             SELECT s.payment_intent FROM mf_shop_stray_payments s
             WHERE s.refunded_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM mf_shop_orders o WHERE o.stripe_payment_intent = s.payment_intent)
             ORDER BY s.tried_at NULLS FIRST LIMIT 20)
         RETURNING payment_intent, ref",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    for (intent, reference) in strays {
        if !intent.starts_with("pi_") {
            continue;
        }
        let form = [
            ("payment_intent", intent.clone()),
            ("reverse_transfer", "true".to_string()),
            ("refund_application_fee", "true".to_string()),
            ("metadata[shop_order]", reference.clone()),
            ("metadata[app]", crate::selling::APP.to_string()),
            ("metadata[why]", "stray".to_string()),
        ];
        match stripe
            .post("/v1/refunds", &form, Some(&format!("mf-shop-stray-{intent}")))
            .await
        {
            Ok(_) => {
                sqlx::query("UPDATE mf_shop_stray_payments SET refunded_at = now() WHERE payment_intent = $1")
                    .bind(&intent)
                    .execute(pool)
                    .await
                    .map_err(|e| e.to_string())?;
                eprintln!("shop orders: refunded a payment for {reference} that its order couldn't take");
            }
            Err(e) => eprintln!("shop orders: refunding a stray payment for {reference}: {e} (tried again)"),
        }
    }
    Ok(())
}

/// `settle`, now, with any failure left for the sweep.
async fn settle_now(state: &AppState, pool: &PgPool, id: i64) {
    if let Err(e) = settle(state, pool, id).await {
        eprintln!("shop orders: refund of order {id}: {e} (the sweep tries again)");
    }
}

/// Every minute: due refunds tried again; unpaid orders given up after 2 days and closed ones'
/// files deleted after 30, with their art and mock-ups.
pub async fn sweep(state: &AppState) -> Result<(), String> {
    let Some(pool) = state.db.as_ref() else { return Ok(()) };
    // (the least recently tried first, so refunds that keep failing don't hold up the rest)
    let due: Vec<i64> = sqlx::query_scalar(
        "UPDATE mf_shop_orders SET refund_tried_at = now()
         WHERE id IN (SELECT id FROM mf_shop_orders WHERE refund_due AND fee_returned_at IS NULL
                      ORDER BY refund_tried_at NULLS FIRST, id LIMIT 20)
         RETURNING id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    for id in due {
        settle_now(state, pool, id).await;
    }
    // a paid order the hub refused for good (not one that only expired: the hub says so itself)
    // can never reach its shop: refunded in full
    let refused: Vec<i64> = sqlx::query_scalar(
        "UPDATE mf_shop_orders o SET status = 'expired', refund_due = true, updated_at = now()
         FROM mf_hub_outbox m
         WHERE m.kind = 'order.placed' AND m.order_ref = o.ref AND m.failed IS NOT NULL AND o.status = 'sent'
         RETURNING o.id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    for id in refused {
        eprintln!("shop orders: order {id} couldn't be sent to its shop; refunding it");
        settle_now(state, pool, id).await;
    }
    if let Some(stripe) = state.stripe.as_ref() {
        super::tax::settle_all(stripe, pool).await?;
        refund_strays(stripe, pool).await?;
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let gone: Vec<(i64, Value)> = sqlx::query_as(
        "SELECT id, art FROM mf_shop_orders
         WHERE (status = 'pending' AND created_at < now() - interval '2 days')
            OR (status = ANY($1) AND updated_at < now() - interval '30 days' AND art <> '[]'::jsonb)
         ORDER BY id LIMIT 100 FOR UPDATE SKIP LOCKED",
    )
    .bind(CLOSED)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let ids: Vec<i64> = gone.iter().map(|(id, _)| *id).collect();
    let mut keys = art_keys(gone.iter().map(|(_, art)| art));
    keys.extend(
        sqlx::query_scalar::<_, String>("DELETE FROM mf_shop_proofs WHERE order_id = ANY($1) RETURNING image_key")
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| e.to_string())?,
    );
    sqlx::query(
        "UPDATE mf_shop_orders SET art = '[]'::jsonb,
             status = CASE WHEN status = 'pending' THEN 'abandoned' ELSE status END
         WHERE id = ANY($1)",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    if let Some(storage) = state.storage.as_ref() {
        for key in keys {
            if let Err(e) = storage.delete(&key).await {
                eprintln!("shop orders: deleting a file: {e}");
            }
        }
    }
    forget_messages(pool).await
}

/// Copies of the messages sent to shops, kept in the outbox (each carries the customer's name,
/// address and note), go 30 days after their order is done with, as its art does: those the
/// hub took or refused for good. The last one of each conversation stays, its contents blanked,
/// because the next message's number counts on from it (`hub::enqueue`).
pub async fn forget_messages(pool: &PgPool) -> Result<(), String> {
    let mut done = CLOSED.to_vec();
    done.push("abandoned");
    let old = "SELECT m.id FROM mf_hub_outbox m LEFT JOIN mf_shop_orders o ON o.ref = m.order_ref
         WHERE (m.sent_at IS NOT NULL OR m.failed IS NOT NULL)
           AND coalesce(m.sent_at, m.last_try_at, m.created_at) < now() - interval '30 days'
           AND (m.order_ref IS NULL OR o.id IS NULL
                OR (o.status = ANY($1) AND o.updated_at < now() - interval '30 days'))";
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(&format!(
        "DELETE FROM mf_hub_outbox m WHERE m.id IN ({old})
           AND (m.order_ref IS NULL OR m.seq < (SELECT max(x.seq) FROM mf_hub_outbox x
                                                WHERE x.order_ref = m.order_ref AND x.to_party = m.to_party))"
    ))
    .bind(&done)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query(&format!(
        "UPDATE mf_hub_outbox SET envelope = jsonb_set(envelope, '{{body}}', '{{}}'::jsonb)
         WHERE id IN ({old}) AND envelope->'body' <> '{{}}'::jsonb"
    ))
    .bind(&done)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// The storage keys in orders' `art`.
pub fn art_keys<'a>(arts: impl Iterator<Item = &'a Value>) -> Vec<String> {
    arts.filter_map(Value::as_array)
        .flatten()
        .filter_map(|a| a.get("key").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------- the customer

async fn buyers_order(state: &AppState, headers: &HeaderMap, reference: &str) -> Result<(PgPool, Order), Response> {
    let (Some(user), Some(pool)) = (current_user(state, headers).await, state.db.clone()) else {
        return Err(crate::designs::not_found());
    };
    let Ok(mut conn) = pool.acquire().await else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "Try again in a minute.").into_response());
    };
    match order(&mut conn, reference).await.filter(|o| o.buyer == user.account_id) {
        Some(o) => Ok((pool, o)),
        None => Err(crate::designs::not_found()),
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct Said {
    answer: String,
    note: String,
    /// Why the customer cancels: one of `CANCEL_REASONS`.
    reason: String,
    /// A few words more (needed for `other`).
    detail: String,
}

/// Why a customer may cancel (issue #80: the hub needs a reason on every `order.cancel`). `other`
/// needs a few words of the customer's own.
pub const CANCEL_REASONS: &[(&str, &str)] = &[
    ("mistake", "I ordered by mistake"),
    ("change", "I need to change the order"),
    ("late", "It won't arrive in time"),
    ("other", "Another reason"),
];

/// The most a customer may add to their reason for cancelling: with the longest reason before it,
/// well inside `contract::MAX_NOTE`.
pub const CANCEL_DETAIL_CHARS: usize = 400;

/// What the customer's order page says went wrong, for the `?problem=` their cancel or answer was
/// sent back with. Unknown codes say nothing.
pub fn problem_words(code: &str) -> Option<String> {
    Some(match code {
        "cancel-reason" => {
            "Choose why you're cancelling: the shop needs a reason. \"Another reason\" needs a few words.".into()
        }
        "too-long" => format!(
            "That's longer than the shop can take: {} characters at most. Please shorten it.",
            contract::MAX_NOTE
        ),
        "contact" => "Leave email addresses and phone numbers out: the shop answers you on your order's page.".into(),
        _ => return None,
    })
}

/// The customer's reason for cancelling, as `order.cancel`'s `why`, or the `?problem=` code that
/// says what's missing. Never empty, never cut short.
pub fn cancel_why(reason: &str, detail: &str) -> Result<String, &'static str> {
    let detail = contract::tidy(detail);
    let label = CANCEL_REASONS
        .iter()
        .find(|(k, _)| *k == reason)
        .map(|(_, label)| *label)
        .ok_or("cancel-reason")?;
    let why = match (reason, detail.is_empty()) {
        ("other", true) => return Err("cancel-reason"),
        ("other", false) => detail,
        (_, true) => label.to_string(),
        (_, false) => format!("{label}: {detail}"),
    };
    if contract::hub_len(&why) > contract::MAX_NOTE {
        return Err("too-long");
    }
    if contract::has_contact(&why) {
        return Err("contact");
    }
    Ok(why)
}

/// `POST /shop-orders/<ref>/cancel`: before production, the customer cancels for a full refund.
pub async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(reference): Path<String>,
    Form(said): Form<Said>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (pool, o) = match buyers_order(&state, &headers, &reference).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let back = Redirect::to(&format!("/shop-orders/{reference}")).into_response();
    let why = match cancel_why(&said.reason, &said.detail) {
        Ok(why) => why,
        Err(problem) => return Redirect::to(&format!("/shop-orders/{reference}?problem={problem}")).into_response(),
    };
    let done = async {
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        let cancelled = sqlx::query(
            "UPDATE mf_shop_orders SET status = 'cancelled', cancelled_by = 'customer', cancel_why = $2,
                 refund_due = true, updated_at = now()
             WHERE id = $1 AND status = ANY($3)",
        )
        .bind(o.id)
        .bind(&why)
        .bind(BEFORE_PRODUCTION)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
        if cancelled == 0 {
            return Ok(false);
        }
        let message = Message::OrderCancel(OrderCancel {
            by: "customer".into(),
            why,
        });
        super::hub::enqueue(&mut tx, &message, &o.reference, &o.shop_party, MESSAGE_DAYS, &[]).await?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok::<bool, String>(true)
    };
    match done.await {
        Ok(true) => settle_now(&state, &pool, o.id).await,
        Ok(false) => {}
        Err(e) => eprintln!("shop orders: cancelling {reference}: {e}"),
    }
    back
}

/// `POST /shop-orders/<ref>/proofs/<n>`: the customer approves the latest proof or asks for changes.
pub async fn answer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((reference, proof)): Path<(String, u32)>,
    Form(said): Form<Said>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (pool, o) = match buyers_order(&state, &headers, &reference).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let back = Redirect::to(&format!("/shop-orders/{reference}")).into_response();
    let note = contract::tidy(&said.note);
    if contract::hub_len(&note) > contract::MAX_NOTE {
        return Redirect::to(&format!("/shop-orders/{reference}?problem=too-long")).into_response();
    }
    if contract::has_contact(&note) {
        return Redirect::to(&format!("/shop-orders/{reference}?problem=contact")).into_response();
    }
    let approved = said.answer == "approved";
    if !approved && (said.answer != "changes" || note.is_empty()) {
        // (asking for changes needs a note: the page's form requires one)
        return back;
    }
    let number = i32::try_from(proof).unwrap_or(i32::MAX);
    let done = async {
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        let answered = sqlx::query(
            "UPDATE mf_shop_proofs SET answer = $3, answer_note = $4, answered_at = now()
             WHERE order_id = $1 AND proof = $2 AND answer IS NULL
               AND proof = (SELECT max(proof) FROM mf_shop_proofs WHERE order_id = $1)
               AND EXISTS (SELECT 1 FROM mf_shop_orders WHERE id = $1 AND status = 'proof_sent')",
        )
        .bind(o.id)
        .bind(number)
        .bind(if approved { "approved" } else { "changes" })
        .bind(&note)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
        if answered == 0 {
            return Ok(());
        }
        if approved {
            sqlx::query("UPDATE mf_shop_orders SET status = 'approved', updated_at = now() WHERE id = $1")
                .bind(o.id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        let message = Message::ProofAnswer(ProofAnswer {
            proof,
            answer: if approved { "approved" } else { "changes" }.into(),
            note,
        });
        super::hub::enqueue(&mut tx, &message, &o.reference, &o.shop_party, MESSAGE_DAYS, &[]).await?;
        tx.commit().await.map_err(|e| e.to_string())
    };
    if let Err(e) = done.await {
        eprintln!("shop orders: answering a proof of {reference}: {e}");
    }
    back
}

/// `GET /shop-orders/<ref>/proofs/<n>/image`: a proof's mock-up (as re-drawn), for the customer.
pub async fn image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((reference, proof)): Path<(String, u32)>,
) -> Response {
    let (pool, o) = match buyers_order(&state, &headers, &reference).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT image_key, image_type FROM mf_shop_proofs WHERE order_id = $1 AND proof = $2")
            .bind(o.id)
            .bind(i32::try_from(proof).unwrap_or(i32::MAX))
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
    let (Some((key, kind)), Some(storage)) = (row, state.storage.as_ref()) else {
        return crate::designs::not_found();
    };
    match storage.get(&key).await {
        Ok(Some(bytes)) => (
            [
                (header::CONTENT_TYPE, kind),
                (header::CACHE_CONTROL, "private, no-store".to_string()),
            ],
            bytes,
        )
            .into_response(),
        _ => crate::designs::not_found(),
    }
}

/// The parts of the customer's order page that come after payment: the shop's word, the proofs,
/// cancelling and refunds.
pub async fn page_html(pool: &PgPool, o: &Order) -> String {
    let row = sqlx::query(
        "SELECT tracking, shop_note, cancelled_by, cancel_why, refund_due, refunded_at IS NOT NULL AS refunded
         FROM mf_shop_orders WHERE id = $1",
    )
    .bind(o.id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some(row) = row else { return String::new() };
    let text = |k: &str| row.try_get::<String, _>(k).unwrap_or_default();
    let mut html = String::new();
    if !text("shop_note").is_empty() {
        html.push_str(&format!(
            r#"<p class="shop-note">The shop says: {}</p>"#,
            escape(&text("shop_note"))
        ));
    }
    if !text("tracking").is_empty() {
        html.push_str(&format!("<p>Tracking: {}</p>", escape(&text("tracking"))));
    }
    let proofs: Vec<(i32, String, Option<String>, String)> = sqlx::query_as(
        "SELECT proof, note, answer, answer_note FROM mf_shop_proofs WHERE order_id = $1 ORDER BY proof DESC LIMIT 20",
    )
    .bind(o.id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for (i, (n, note, answer, answer_note)) in proofs.iter().enumerate() {
        let r = escape(&o.reference);
        let max = contract::MAX_NOTE;
        let said = match answer.as_deref() {
            Some("approved") => "<p><strong>You approved this proof.</strong></p>".to_string(),
            Some(_) => format!("<p><strong>You asked for changes:</strong> {}</p>", escape(answer_note)),
            None if i == 0 && o.status == "proof_sent" => format!(
                r#"<form method="post" action="/shop-orders/{r}/proofs/{n}"><input type="hidden" name="answer" value="approved"><button type="submit">Approve this proof</button></form>
<form method="post" action="/shop-orders/{r}/proofs/{n}"><input type="hidden" name="answer" value="changes"><label>Or ask for changes <small>({max} characters at most)</small> <textarea name="note" maxlength="{max}" rows="2" required data-count="{max}"></textarea></label><button type="submit" class="secondary">Ask for changes</button></form>"#
            ),
            None => String::new(),
        };
        html.push_str(&format!(
            r#"<figure class="proof"><img src="/shop-orders/{r}/proofs/{n}/image" alt="Proof {n}: the shop's mock-up of your order"><figcaption>Proof {n}{note}</figcaption></figure>{said}"#,
            note = if note.is_empty() { String::new() } else { format!(": {}", escape(note)) },
        ));
    }
    let refunded = row.try_get::<bool, _>("refunded").unwrap_or(false);
    let due = row.try_get::<bool, _>("refund_due").unwrap_or(false);
    let by = row.try_get::<Option<String>, _>("cancelled_by").ok().flatten();
    if let Some(by) = by {
        let who = if by == "shop" {
            "The shop cancelled this order"
        } else {
            "You cancelled this order"
        };
        let why = text("cancel_why");
        html.push_str(&format!(
            "<p>{who}{}.</p>",
            if why.is_empty() {
                String::new()
            } else {
                format!(": {}", escape(&why))
            }
        ));
    }
    if refunded {
        html.push_str(&format!(
            r#"<p class="lead">Refunded: {} back to your card.</p>"#,
            super::shops::dollars(o.totals.paid_cents)
        ));
    } else if due {
        html.push_str("<p>Your refund is on its way back to your card.</p>");
    } else if o.status == "cancelled" {
        html.push_str("<p>Production had started, so a refund is the shop's to decide. MakerFoundry will help if you need it.</p>");
    }
    if BEFORE_PRODUCTION.contains(&o.status.as_str()) {
        html.push_str(&format!(
            r#"<details class="cancel"><summary>Cancel the order</summary><form method="post" action="/shop-orders/{r}/cancel"><p>Until production starts you can cancel for a full refund.</p><fieldset><legend>Why are you cancelling? <small>(the shop is told)</small></legend>{reasons}</fieldset><label>A few words more <small>(needed for "Another reason"; {detail} characters at most)</small> <textarea name="detail" maxlength="{detail}" rows="2" data-count="{detail}"></textarea></label><button type="submit" class="secondary">Cancel the order</button></form></details>"#,
            r = escape(&o.reference),
            reasons = CANCEL_REASONS
                .iter()
                .map(|(k, label)| format!(
                    r#"<label class="choice"><input type="radio" name="reason" value="{k}" required> <span>{label}</span></label>"#
                ))
                .collect::<String>(),
            detail = CANCEL_DETAIL_CHARS,
        ));
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #80: a customer's cancel always says why, within what the hub takes.
    #[test]
    fn a_cancel_says_why_and_is_never_cut() {
        assert_eq!(cancel_why("mistake", "").unwrap(), "I ordered by mistake");
        assert_eq!(
            cancel_why("change", "  bigger sizes ").unwrap(),
            "I need to change the order: bigger sizes"
        );
        assert_eq!(cancel_why("other", "Wrong colour").unwrap(), "Wrong colour");
        for (reason, detail) in [("", ""), ("other", ""), ("other", " \r\n "), ("bored", "x")] {
            assert_eq!(
                cancel_why(reason, detail),
                Err("cancel-reason"),
                "{reason:?} {detail:?}"
            );
        }
        assert_eq!(cancel_why("other", &"w".repeat(501)), Err("too-long"));
        assert_eq!(cancel_why("other", &"w".repeat(500)).unwrap().len(), 500);
        assert_eq!(cancel_why("late", "call 401 555 0100"), Err("contact"));
        // the longest reason with the most the form lets through still fits
        let longest = CANCEL_REASONS.iter().map(|(_, l)| l.len()).max().unwrap();
        assert!(longest + 2 + CANCEL_DETAIL_CHARS <= contract::MAX_NOTE);
        for (k, _) in CANCEL_REASONS {
            if *k != "other" {
                assert!(cancel_why(k, &"d".repeat(CANCEL_DETAIL_CHARS)).is_ok(), "{k}");
            }
        }
        for code in ["cancel-reason", "too-long", "contact"] {
            assert!(problem_words(code).is_some(), "{code}");
        }
        assert!(problem_words("<script>").is_none());
    }
}
