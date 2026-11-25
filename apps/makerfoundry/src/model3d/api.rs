//! The public API for 3D models (issue #36): read-only JSON for slicers and apps, extending the
//! apps' API (MF-7, `docs/api.md`). Searching published models (the same search and filters as
//! `/3d`, paged by key), and one model's details, files and print profiles. Each request carries a
//! key a member makes at Account › API keys (`Authorization: Bearer mfk_…`); only its SHA-256 is
//! kept, and each key may make `limits::API_READ` requests an hour. Nothing private is ever
//! answered: only published models, their makers' public names, and no files' contents (a free
//! model's download stays on the site, where its licence and terms are shown).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::search;
use crate::AppState;
use crate::accounts::{current_user, forbidden, private, same_origin};
use crate::listings::slug;
use crate::pages::escape;

/// The keys one account may have at once.
pub const MAX_KEYS: i64 = 5;

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": { "message": message } }))).into_response()
}

fn hash(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

/// The account whose key the request carries, once the key is checked and within its limit.
async fn keyed(state: &AppState, headers: &HeaderMap) -> Result<(PgPool, i64), Response> {
    let Some(pool) = state.db.clone() else {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MakerFoundry can't answer just now.",
        ));
    };
    let key = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| k.starts_with("mfk_") && k.len() <= 100);
    let Some(key) = key else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "Send an API key (Account › API keys) as Authorization: Bearer mfk_…",
        ));
    };
    let found: Option<(i64, i64)> = sqlx::query_as(
        "UPDATE mf_api_keys SET last_used_at = now() WHERE key_hash = $1 AND revoked_at IS NULL RETURNING id, account_id",
    )
    .bind(hash(key))
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten();
    let Some((id, account)) = found else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "That API key isn't known, or it was revoked.",
        ));
    };
    // until MakerFoundry 3D opens, only admins' keys answer
    if !state.three_d && !state.admins.contains(&account) {
        return Err(error(StatusCode::NOT_FOUND, "MakerFoundry 3D isn't open yet."));
    }
    if !state.limits.allow(crate::limits::API_READ, &format!("key:{id}")) {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "This key has made too many requests this hour. Please slow down.",
        ));
    }
    Ok((pool, account))
}

/// `GET /api/v1/models?q=…&category=…&licence=…&format=…&printer=…&filament=…&makes=yes&profile=yes&multicolour=yes&sort=popular&after=…`
pub async fn search(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<search::Query>) -> Response {
    let (pool, _) = match keyed(&state, &headers).await {
        Ok(k) => k,
        Err(r) => return r,
    };
    let (cards, next) = search::find(&pool, &q).await;
    let base = &state.public_url;
    let models: Vec<serde_json::Value> = cards
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "title": c.title,
                "url": format!("{base}/d/{}/{}", c.id, slug(&c.title)),
                "maker": c.maker,
                "price_cents": c.price,
                "downloads": c.downloads,
                "picture": c.model_file.map(|f| format!("{base}/3d/{}/files/{f}/hero", c.id)),
                "api": format!("{base}/api/v1/models/{}", c.id),
            })
        })
        .collect();
    let next = next.map(|after| {
        let href = search::next_href(&q, after).replace("&amp;", "&");
        format!("{base}/api/v1/models{}", href.trim_start_matches("/3d"))
    });
    Json(json!({ "models": models, "next": next })).into_response()
}

type ModelRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<String>,
    String,
    String,
    Vec<String>,
);

/// `GET /api/v1/models/<id>`: a published model's details, files and print profiles.
pub async fn model(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let (pool, _) = match keyed(&state, &headers).await {
        Ok(k) => k,
        Err(r) => return r,
    };
    let row: Option<ModelRow> = sqlx::query_as(
        "SELECT d.title, d.description, d.licence, d.category, d.price_cents, d.downloads,
                to_char(d.published_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                m.name, m.handle, d.tags
         FROM mf_designs d JOIN mf_makers m ON m.account_id = d.account_id
         WHERE d.id = $1 AND d.kind = 'model3d' AND d.status = 'published'",
    )
    .bind(id)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten();
    let Some((title, description, licence, category, price, downloads, published, maker, handle, tags)) = row else {
        return error(StatusCode::NOT_FOUND, "No published 3D model has that number.");
    };
    let base = &state.public_url;
    let licence = licence
        .as_deref()
        .and_then(|l| crate::licences::find_for("model3d", l))
        .map(|l| json!({ "id": l.id, "name": l.name, "url": l.url }));
    let files: Vec<serde_json::Value> = super::upload::model_files(&pool, id)
        .await
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "name": f.file_name,
                "format": f.format,
                "role": f.role,
                "bytes": f.file_bytes,
                "triangles": f.triangles,
                "size_mm": f.size_mm,
                "picture": f.has_thumb.then(|| format!("{base}/3d/{id}/files/{}/hero", f.id)),
            })
        })
        .collect();
    let profiles: Vec<serde_json::Value> = super::upload::profiles(&pool, id)
        .await
        .iter()
        .map(|p| {
            json!({
                "file_id": p.file_id,
                "file_name": p.file_name,
                "name": p.name,
                "slicer": p.slicer,
                "printers": p.printers,
                "nozzle_mm": p.nozzle_mm,
                "layer_mm": p.layer_mm,
                "infill_percent": p.infill_percent,
                "walls": p.walls,
                "supports": p.supports,
                "plates": p.plates,
                "filaments": p.filaments,
            })
        })
        .collect();
    Json(json!({
        "id": id,
        "title": title,
        "url": format!("{base}/d/{id}/{}", slug(&title)),
        "description": description,
        "tags": tags,
        "category": category,
        "licence": licence,
        "price_cents": price,
        "downloads": downloads,
        "likes": super::likes::count(&pool, id).await,
        "published_at": published,
        "maker": { "name": maker, "url": format!("{base}/makers/{handle}") },
        "files": files,
        "profiles": profiles,
        "download": (price == 0).then(|| format!("{base}/d/{id}/download")),
    }))
    .into_response()
}

// ---------------------------------------------------------------- keys

type KeyRow = (i64, String, String, Option<String>);

async fn keys_page(state: &AppState, pool: &PgPool, account: i64, shown: Option<&str>, problem: &str) -> Response {
    let keys: Vec<KeyRow> = sqlx::query_as(
        "SELECT id, name, to_char(created_at, 'YYYY-MM-DD'), to_char(last_used_at, 'YYYY-MM-DD') FROM mf_api_keys WHERE account_id = $1 AND revoked_at IS NULL ORDER BY created_at",
    )
    .bind(account)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let list: String = keys
        .iter()
        .map(|(id, name, made, used)| {
            format!(
                r#"<li><strong>{name}</strong> <span class="muted">made {made}, {used}</span><form method="post" action="/account/api-keys/{id}/revoke"><button type="submit" class="secondary small">Revoke</button></form></li>"#,
                name = escape(name),
                made = escape(made),
                used = used.as_deref().map_or("never used".to_string(), |u| format!("last used {}", escape(u))),
            )
        })
        .collect();
    let list = if list.is_empty() {
        "<p>You have no API keys.</p>".to_string()
    } else {
        format!(r#"<ul class="api-keys">{list}</ul>"#)
    };
    let shown = shown.map_or(String::new(), |k| {
        format!(
            r#"<p class="notice">Your new key, shown only this once: <code>{}</code></p>"#,
            escape(k)
        )
    });
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem">{}</p>"#, escape(problem))
    };
    let base = escape(&state.public_url);
    private(
        "API keys · MakerFoundry",
        &format!(
            r#"<section class="form"><h1>API keys</h1>
<p class="lead">For apps and slicers that read published 3D models from MakerFoundry: <code>{base}/api/v1/models</code>. Keep a key secret, like a password; anyone with it can read the API as you. Up to {MAX_KEYS} keys.</p>
{shown}{problem}{list}
<form method="post" action="/account/api-keys">
<label>What it's for <input name="name" maxlength="60" required></label>
<button type="submit">Make a key</button>
</form>
<p><a href="/account">Your account</a></p></section>"#
        ),
        true,
    )
}

/// `GET /account/api-keys`.
pub async fn page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = current_user(&state, &headers).await;
    let (Some(user), Some(pool)) = (user, state.db.as_ref()) else {
        return Redirect::to("/signin?next=/account/api-keys").into_response();
    };
    if !super::open_to(&state, Some(&user)).await {
        return crate::designs::not_found();
    }
    keys_page(&state, pool, user.account_id, None, "").await
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct KeyForm {
    name: String,
}

/// Keeps a new key for `account`, holding a lock on the account so keys made at the same moment
/// can't take it past `MAX_KEYS` (issue #187) → whether it was kept (false: at the cap).
async fn add_key(pool: &sqlx::PgPool, account: i64, name: &str, key_hash: &[u8]) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("mf-api-keys-{account}"))
        .execute(&mut *tx)
        .await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mf_api_keys WHERE account_id = $1 AND revoked_at IS NULL")
            .bind(account)
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_KEYS {
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query("INSERT INTO mf_api_keys (account_id, name, key_hash) VALUES ($1, $2, $3)")
        .bind(account)
        .bind(name)
        .bind(key_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// `POST /account/api-keys`: a new key, shown once.
pub async fn make(State(state): State<AppState>, headers: HeaderMap, axum::Form(f): axum::Form<KeyForm>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let user = current_user(&state, &headers).await;
    let (Some(user), Some(pool)) = (user, state.db.as_ref()) else {
        return Redirect::to("/signin?next=/account/api-keys").into_response();
    };
    if !super::open_to(&state, Some(&user)).await {
        return crate::designs::not_found();
    }
    let name: String = f.name.trim().chars().filter(|c| !c.is_control()).take(60).collect();
    if name.is_empty() {
        return keys_page(&state, pool, user.account_id, None, "Say what the key is for.").await;
    }
    let mut bytes = [0u8; 24];
    if getrandom::fill(&mut bytes).is_err() {
        return keys_page(&state, pool, user.account_id, None, "A key couldn't be made just now.").await;
    }
    let key = format!("mfk_{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let saved = add_key(pool, user.account_id, &name, &hash(&key)).await;
    match saved {
        Ok(false) => {
            keys_page(
                &state,
                pool,
                user.account_id,
                None,
                &format!("You can have up to {MAX_KEYS} keys. Revoke one to make another."),
            )
            .await
        }
        Ok(true) => keys_page(&state, pool, user.account_id, Some(&key), "").await,
        Err(e) => {
            eprintln!("api keys: {e}");
            keys_page(&state, pool, user.account_id, None, "A key couldn't be made just now.").await
        }
    }
}

/// `POST /account/api-keys/<id>/revoke`.
pub async fn revoke(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let user = current_user(&state, &headers).await;
    let (Some(user), Some(pool)) = (user, state.db.as_ref()) else {
        return Redirect::to("/signin?next=/account/api-keys").into_response();
    };
    let _ = sqlx::query(
        "UPDATE mf_api_keys SET revoked_at = now() WHERE id = $1 AND account_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user.account_id)
    .execute(pool)
    .await;
    Redirect::to("/account/api-keys").into_response()
}
