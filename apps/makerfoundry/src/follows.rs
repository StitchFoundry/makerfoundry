//! Following makers (MF-13): anyone signed in follows a maker from the maker's page, and
//! `/following` shows the newest designs of the makers they follow. A maker's page says how many
//! follow them; who follows whom isn't shown to anyone.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{User, current_user, forbidden, private, same_origin};
use crate::listings::{Filter, cards_html, find};
use crate::pages::escape;

/// Makers one account may follow.
pub const MAX_FOLLOWS: i64 = 1000;
/// Designs shown on `/following`.
const SHOWN: i64 = 60;

pub async fn followers(pool: &PgPool, maker: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mf_follows WHERE maker_account = $1")
        .bind(maker)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

async fn follows(pool: &PgPool, follower: i64, maker: i64) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mf_follows WHERE follower_account = $1 AND maker_account = $2")
        .bind(follower)
        .bind(maker)
        .fetch_one(pool)
        .await
        .is_ok_and(|n| n > 0)
}

/// The follow part of a maker's page (`handle`, account `maker`): the count and the button.
pub async fn button_html(pool: &PgPool, viewer: Option<&User>, maker: i64, handle: &str) -> String {
    let n = followers(pool, maker).await;
    let count = match n {
        0 => String::new(),
        1 => r#"<span class="muted">1 follower</span>"#.to_string(),
        n => format!(r#"<span class="muted">{n} followers</span>"#),
    };
    let h = escape(handle);
    let button = match viewer {
        None => format!(r#"<a href="/signin?next=/makers/{h}">Sign in to follow</a>"#),
        Some(u) if u.account_id == maker => String::new(),
        Some(u) if follows(pool, u.account_id, maker).await => format!(
            r#"<form method="post" action="/makers/{h}/unfollow"><button type="submit" class="secondary">Following ✓ (unfollow)</button></form>"#
        ),
        Some(_) => {
            format!(r#"<form method="post" action="/makers/{h}/follow"><button type="submit">Follow</button></form>"#)
        }
    };
    format!(r#"<div class="follow">{button}{count}</div>"#)
}

/// The maker with address `handle` → their account.
async fn maker(pool: &PgPool, handle: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT account_id FROM mf_makers WHERE handle = $1")
        .bind(handle)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

pub async fn follow(State(state): State<AppState>, headers: HeaderMap, Path(handle): Path<String>) -> Response {
    change(&state, &headers, &handle, true).await
}

pub async fn unfollow(State(state): State<AppState>, headers: HeaderMap, Path(handle): Path<String>) -> Response {
    change(&state, &headers, &handle, false).await
}

/// Notes that `follower` follows `maker`, holding a lock on the follower so follows sent at the
/// same moment can't take them past `MAX_FOLLOWS` (issue #187) → false at the cap.
async fn add(pool: &PgPool, follower: i64, maker: i64) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("mf-follows-{follower}"))
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_follows WHERE follower_account = $1")
        .bind(follower)
        .fetch_one(&mut *tx)
        .await?;
    if count >= MAX_FOLLOWS {
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query("INSERT INTO mf_follows (follower_account, maker_account) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(follower)
        .bind(maker)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

async fn change(state: &AppState, headers: &HeaderMap, handle: &str, on: bool) -> Response {
    if !same_origin(headers) {
        return forbidden();
    }
    let back = format!("/makers/{}", crate::listings::url_word(handle));
    let (Some(user), Some(pool)) = (current_user(state, headers).await, state.db.as_ref()) else {
        return Redirect::to(&format!("/signin?next={back}")).into_response();
    };
    let Some(maker) = maker(pool, handle).await.filter(|m| *m != user.account_id) else {
        return Redirect::to(&back).into_response();
    };
    let done = if on {
        let added = add(pool, user.account_id, maker).await;
        if matches!(added, Ok(true)) {
            crate::notices::record(state, pool, maker, user.account_id, "follow", None).await;
        }
        added.map(|_| ())
    } else {
        sqlx::query("DELETE FROM mf_follows WHERE follower_account = $1 AND maker_account = $2")
            .bind(user.account_id)
            .bind(maker)
            .execute(pool)
            .await
            .map(|_| ())
    };
    if let Err(e) = done {
        eprintln!("follows: {e}");
    }
    Redirect::to(&back).into_response()
}

/// The newest designs of the makers the viewer follows, and who they are.
pub async fn following(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.as_ref()) else {
        return Redirect::to("/signin?next=/following").into_response();
    };
    let makers: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.handle, m.name FROM mf_follows f JOIN mf_makers m ON m.account_id = f.maker_account
         WHERE f.follower_account = $1 ORDER BY m.name",
    )
    .bind(user.account_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let body = if makers.is_empty() {
        r#"<p class="lead">You don't follow any makers yet. Follow one from their page, and their new designs show here.</p><p><a href="/browse">Browse designs</a></p>"#.to_string()
    } else {
        let found = find(
            pool,
            &Filter {
                followed_by: Some(user.account_id),
                three_d: state.three_d,
                ..Filter::default()
            },
            SHOWN,
        )
        .await;
        let list: Vec<String> = makers
            .iter()
            .map(|(h, n)| format!(r#"<a href="/makers/{}">{}</a>"#, escape(h), escape(n)))
            .collect();
        let cards = if found.is_empty() {
            r#"<p class="muted">No designs from them yet.</p>"#.to_string()
        } else {
            cards_html(&found)
        };
        format!(
            r#"<p class="muted">The newest designs from {}.</p>{cards}"#,
            list.join(", ")
        )
    };
    private(
        "Following · MakerFoundry",
        &format!(r#"<section class="design"><h1>Following</h1>{body}</section>"#),
        true,
    )
}
