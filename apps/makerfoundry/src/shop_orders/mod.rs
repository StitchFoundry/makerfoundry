//! Ordering physical goods from shops through StitchHub (issues #1 and #2): the owner's decisions
//! of 2026-10-31 are in `docs/stitchshop-ordering-audit.md`, the messages in
//! `docs/shop-orders-contract.md`, in the form StitchHub implements (#53). Switched off until
//! `MF_STITCHHUB_URL`, `MF_STITCHHUB_KEY` and `MF_STITCHHUB_SIGNING_KEY` are set.

pub mod after;
pub mod brand;
pub mod contract;
pub mod disputes;
pub mod hub;
pub mod mail;
pub mod orders;
pub mod shops;
pub mod sign;
pub mod tax;

use std::future::Future;
use std::pin::Pin;

use sqlx::PgPool;

use crate::AppState;
use contract::{Envelope, Message};

/// Applies a checked message from a shop (or the hub), once and in order (`hub::sync`).
pub async fn apply(state: AppState, pool: PgPool, env: Envelope, msg: Message) -> Result<(), String> {
    match &msg {
        Message::Catalog(c) => shops::keep(&pool, state.storage.as_ref(), env.sender(), c).await,
        Message::OrderStatus(s) => after::status(&pool, &env, s).await,
        Message::ProofSent(p) => after::proof(&state, &pool, &env, p).await,
        Message::OrderCancel(c) => after::shop_cancelled(&state, &pool, &env, c).await,
        Message::MessageExpired(e) => after::expired(&state, &pool, &env, e).await,
        // (MakerFoundry's own messages never come back to it)
        Message::OrderPlaced(_) | Message::ProofAnswer(_) => {
            eprintln!("stitchhub: {} {} left: not for MakerFoundry", env.kind, env.id);
            Ok(())
        }
    }
}

/// One round with StitchHub when it's switched on (the sweep, every minute).
pub async fn sync(state: &AppState) -> Result<(), String> {
    let (Some(hub), Some(pool)) = (state.stitchhub.as_ref(), state.db.as_ref()) else {
        return Ok(());
    };
    let (p, s) = (pool.clone(), state.clone());
    let run = move |env: Envelope, msg: Message| {
        let (p, s) = (p.clone(), s.clone());
        Box::pin(apply(s, p, env, msg)) as Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
    };
    let result = hub::sync(pool, hub, state.storage.as_ref(), &run).await;
    // which shops take orders now (whatever happened above)
    match hub.shops().await {
        Ok(listed) => {
            let ready: Vec<String> = listed.into_iter().filter(|s| s.ready).map(|s| s.id).collect();
            shops::refresh(pool, &ready).await?;
        }
        Err(e) => eprintln!("stitchhub: shops: {e}"),
    }
    // the customers' emails about what their shops reported (issue #82)
    if let Err(e) = mail::send_due(state, pool).await {
        eprintln!("shop orders: emails: {e}");
    }
    // shops' logos not fetched yet
    if let Some(storage) = state.storage.as_ref() {
        shops::fetch_logos(pool, hub, storage).await;
    }
    result
}
