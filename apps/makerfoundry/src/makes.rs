//! Makes (MF-11): photos people post of what they stitched or cut from someone's design, with a
//! short note, shown on the design's page. Posting needs a maker profile (the name shown with the
//! photo) and, until the launch, an account that may publish. Photos are cleaned like a design's
//! (`photos`): saved again without their hidden details. The person who posted a make, the
//! design's maker and the admins can take it down.

use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{User, current_user, forbidden, private, same_origin};
use crate::listings::slug;
use crate::pages::{escape, page};
use crate::storage::new_key;
use crate::{limits, makers, photos};

/// The longest note with a make.
pub const MAX_NOTE: usize = 500;
/// Makes one account may post on one design.
pub const MAX_PER_DESIGN: i64 = 10;
/// Makes shown on a design's page (the newest).
const SHOWN: i64 = 60;

/// A make as shown.
pub struct Make {
    pub id: i64,
    pub account_id: i64,
    pub note: String,
    pub width: i32,
    pub height: i32,
    pub by: Option<(String, String)>,
}

pub async fn on(pool: &PgPool, design: i64) -> Vec<Make> {
    sqlx::query_as::<_, (i64, i64, String, i32, i32, Option<String>, Option<String>)>(
        "SELECT k.id, k.account_id, k.note, k.width, k.height, m.handle, m.name
         FROM mf_makes k LEFT JOIN mf_makers m ON m.account_id = k.account_id
         WHERE k.design_id = $1 ORDER BY k.created_at DESC LIMIT $2",
    )
    .bind(design)
    .bind(SHOWN)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, account_id, note, width, height, handle, name)| Make {
        id,
        account_id,
        note,
        width,
        height,
        by: handle.zip(name),
    })
    .collect()
}

/// How many makes design `id` has.
pub async fn count(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mf_makes WHERE design_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// The Makes part of a published design's page: the photos, and the form to add one.
/// `owner` is the design's maker.
pub async fn section(state: &AppState, pool: &PgPool, viewer: Option<&User>, id: i64, owner: i64) -> String {
    let makes = on(pool, id).await;
    // featuring a make on our social pages is offered for 3D models (MF-55)
    let is_model: bool = sqlx::query_scalar("SELECT kind = 'model3d' FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
    let showcase = if is_model {
        crate::model3d::showcase::CHECKBOX
    } else {
        ""
    };
    let staff = viewer.is_some_and(|u| u.account_id == owner || state.admins.contains(&u.account_id));
    let items: String = makes
        .iter()
        .map(|k| {
            let by = match &k.by {
                Some((h, n)) => format!(r#"<a href="/makers/{}">{}</a>"#, escape(h), escape(n)),
                None => "a maker".to_string(),
            };
            let remove = if staff || viewer.is_some_and(|u| u.account_id == k.account_id) {
                format!(
                    r#"<form method="post" action="/makes/{}/delete"><button type="submit" class="secondary">Take down</button></form>"#,
                    k.id
                )
            } else {
                format!(r#"<a href="/makes/{}/report" class="muted report-make">Report</a>"#, k.id)
            };
            let tw = photos::THUMB_SIDE.min(u32::try_from(k.width).unwrap_or(1)).max(1);
            let th = (f64::from(k.height) * f64::from(tw) / f64::from(k.width.max(1))).round();
            format!(
                r#"<figure class="make"><a href="/makes/{mid}/large"><img src="/makes/{mid}/thumb" alt="Made by {alt}" width="{tw}" height="{th}" loading="lazy"></a><figcaption>{note}<span class="muted">by {by}</span>{remove}</figcaption></figure>"#,
                mid = k.id,
                alt = escape(&k.by.as_ref().map_or("a maker".into(), |(_, n)| n.clone())),
                note = if k.note.is_empty() {
                    String::new()
                } else {
                    format!("{}<br>", escape(&k.note))
                },
            )
        })
        .collect();
    let gallery = if makes.is_empty() {
        r#"<p class="muted">Nobody has posted a make yet.</p>"#.to_string()
    } else {
        format!(r#"<div class="makes">{items}</div>"#)
    };
    let add = match viewer {
        None => format!(r#"<p class="muted"><a href="/signin?next=/d/{id}">Sign in to post your make</a></p>"#),
        Some(u) if !state.publishers.allows(u.account_id) => String::new(),
        Some(_) => format!(
            r#"<details class="add-make"><summary>Post a photo of what you made</summary>
<form method="post" action="/d/{id}/makes" enctype="multipart/form-data" class="add-file">
<label>Photo <small>(JPEG, PNG or WebP; saved again without its hidden details, such as where it was taken)</small><input type="file" name="photo" accept="image/jpeg,image/png,image/webp" required></label>
<label>A note <small>(optional: fabric, thread, material, tips)</small><textarea name="note" rows="3" maxlength="{MAX_NOTE}"></textarea></label>
<label class="choice"><input type="checkbox" name="confirm" value="yes" required> <span>It's my own photo of something I made from this design.</span></label>
{showcase}<button type="submit">Post it</button>
</form></details>"#
        ),
    };
    format!(r#"<h2>Makes</h2>{gallery}{add}"#)
}

fn problem(id: i64, why: &str) -> Response {
    private(
        "Post a make · MakerFoundry",
        &format!(
            r#"<section class="form"><h1>Post a make</h1><p class="problem" role="alert">{}</p><p><a href="/d/{id}">Back to the design</a></p></section>"#,
            escape(why)
        ),
        true,
    )
}

/// A make posted on design `id`.
pub async fn post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.as_ref()) else {
        return Redirect::to(&format!("/signin?next=/d/{id}")).into_response();
    };
    let Some(storage) = state.storage.as_ref() else {
        return problem(id, "Posting photos isn't switched on yet.");
    };
    let title: Option<String> =
        sqlx::query_scalar("SELECT title FROM mf_designs WHERE id = $1 AND status = 'published'")
            .bind(id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let Some(title) = title else {
        return Redirect::to("/browse").into_response();
    };
    if !state.publishers.allows(user.account_id) {
        return problem(id, "Posting makes opens when MakerFoundry launches.");
    }
    if makers::profile(pool, user.account_id).await.is_none() {
        return problem(
            id,
            "Set up your maker profile first (Account › Your maker profile): it's the name shown with your photo.",
        );
    }
    let posted: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_makes WHERE design_id = $1 AND account_id = $2")
        .bind(id)
        .bind(user.account_id)
        .fetch_one(pool)
        .await
        .unwrap_or(MAX_PER_DESIGN);
    if posted >= MAX_PER_DESIGN {
        return problem(id, &format!("You can post up to {MAX_PER_DESIGN} makes on a design."));
    }
    if !state.limits.allow(limits::UPLOAD, &user.account_id.to_string()) {
        return problem(id, "You've posted a lot today. Please try again tomorrow.");
    }
    let (mut photo, mut note, mut confirm, mut showcase) = (None, String::new(), false, false);
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                let Ok(bytes) = field.bytes().await else {
                    return problem(id, "The photo didn't come through whole. Please try again.");
                };
                match name.as_str() {
                    "photo" if !bytes.is_empty() => photo = Some(bytes.to_vec()),
                    "note" => note = String::from_utf8_lossy(&bytes).trim().replace("\r\n", "\n"),
                    "confirm" => confirm = bytes.as_ref() == b"yes",
                    "showcase" => showcase = bytes.as_ref() == b"yes",
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(_) => return problem(id, "The photo didn't come through whole. Please try again."),
        }
    }
    if !confirm {
        return problem(id, "Confirm that it's your own photo of something you made.");
    }
    let Some(photo) = photo else {
        return problem(id, "Choose a photo.");
    };
    if note.chars().count() > MAX_NOTE || note.chars().any(|c| c.is_control() && c != '\n') {
        return problem(id, &format!("The note can be up to {MAX_NOTE} characters."));
    }
    let clean = match tokio::task::spawn_blocking(move || photos::read(&photo)).await {
        Ok(Ok(p)) => p,
        Ok(Err(why)) => return problem(id, &format!("The photo: {why}.")),
        Err(_) => return problem(id, "The photo couldn't be read."),
    };
    let kept = async {
        let (photo_key, thumb_key) = (new_key("make")?, new_key("thumb")?);
        storage.put(&photo_key, clean.jpeg, "image/jpeg").await?;
        if let Err(e) = storage.put(&thumb_key, clean.thumb, "image/jpeg").await {
            let _ = storage.delete(&photo_key).await;
            return Err(e);
        }
        let row = NewMake {
            design: id,
            account: user.account_id,
            photo_key: &photo_key,
            thumb_key: &thumb_key,
            width: i32::try_from(clean.width).unwrap_or(i32::MAX),
            height: i32::try_from(clean.height).unwrap_or(i32::MAX),
            note: &note,
        };
        let make = match save(pool, &row).await {
            Ok(Some(m)) => m,
            other => {
                // (best effort: an orphaned photo costs a little storage)
                let _ = storage.delete(&photo_key).await;
                let _ = storage.delete(&thumb_key).await;
                return Err(match other {
                    Err(e) => e.to_string(),
                    _ => AT_CAP.to_string(),
                });
            }
        };
        let is_model: bool = sqlx::query_scalar("SELECT kind = 'model3d' FROM mf_designs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap_or(false);
        if showcase && is_model {
            crate::model3d::showcase::give(pool, user.account_id, None, Some(make)).await;
        }
        Ok::<(), String>(())
    };
    match kept.await {
        Ok(()) => {
            if let Some(owner) = crate::notices::owner(pool, id).await {
                crate::notices::record(&state, pool, owner, user.account_id, "make", Some(id)).await;
            }
            Redirect::to(&format!("/d/{id}/{}", slug(&title))).into_response()
        }
        Err(e) if e == AT_CAP => problem(id, &format!("You can post up to {MAX_PER_DESIGN} makes on a design.")),
        Err(e) => {
            eprintln!("makes: {id}: {e}");
            problem(
                id,
                "The photo couldn't be kept just now. Please try again in a few minutes.",
            )
        }
    }
}

/// `save`'s answer (inside `post`) when the member already has `MAX_PER_DESIGN` makes on it.
const AT_CAP: &str = "at the cap";

/// A make about to be kept (`save`).
struct NewMake<'a> {
    design: i64,
    account: i64,
    photo_key: &'a str,
    thumb_key: &'a str,
    width: i32,
    height: i32,
    note: &'a str,
}

/// Keeps a make, holding a lock on (design, account) so posts sent at the same moment can't take
/// the member past `MAX_PER_DESIGN` (issue #137) → its id, or `None` when at the cap.
async fn save(pool: &PgPool, m: &NewMake<'_>) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("mf-make-{}-{}", m.design, m.account))
        .execute(&mut *tx)
        .await?;
    let posted: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_makes WHERE design_id = $1 AND account_id = $2")
        .bind(m.design)
        .bind(m.account)
        .fetch_one(&mut *tx)
        .await?;
    if posted >= MAX_PER_DESIGN {
        tx.rollback().await?;
        return Ok(None);
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO mf_makes (design_id, account_id, photo_key, thumb_key, width, height, note) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(m.design)
    .bind(m.account)
    .bind(m.photo_key)
    .bind(m.thumb_key)
    .bind(m.width)
    .bind(m.height)
    .bind(m.note)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// A make's photo or thumbnail, while its design is published.
pub async fn photo(State(state): State<AppState>, Path((mid, size)): Path<(i64, String)>) -> Response {
    let not_found = || StatusCode::NOT_FOUND.into_response();
    let (Some(pool), Some(storage)) = (state.db.as_ref(), state.storage.as_ref()) else {
        return not_found();
    };
    let column = match size.as_str() {
        "thumb" => "k.thumb_key",
        "large" => "k.photo_key",
        _ => return not_found(),
    };
    let key: Option<String> = sqlx::query_scalar(&format!(
        "SELECT {column} FROM mf_makes k JOIN mf_designs d ON d.id = k.design_id WHERE k.id = $1 AND d.status = 'published'"
    ))
    .bind(mid)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some(key) = key else { return not_found() };
    match storage.get(&key).await {
        Ok(Some(bytes)) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => not_found(),
        Err(e) => {
            eprintln!("makes: photo {mid}: {e}");
            (StatusCode::BAD_GATEWAY, "The photo can't be fetched just now.").into_response()
        }
    }
}

/// Takes a make down: its poster, the design's maker or an admin.
pub async fn delete(State(state): State<AppState>, headers: HeaderMap, Path(mid): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.as_ref()) else {
        return Redirect::to("/signin").into_response();
    };
    let row: Option<(i64, i64, i64, String, String)> = sqlx::query_as(
        "SELECT k.design_id, k.account_id, d.account_id, k.photo_key, k.thumb_key
         FROM mf_makes k JOIN mf_designs d ON d.id = k.design_id WHERE k.id = $1",
    )
    .bind(mid)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((design, poster, owner, photo_key, thumb_key)) = row else {
        return (
            StatusCode::NOT_FOUND,
            Html(page(
                "Not found · MakerFoundry",
                r#"<section class="hero"><h1>Not found</h1></section>"#,
                true,
            )),
        )
            .into_response();
    };
    let allowed = user.account_id == poster || user.account_id == owner || state.admins.contains(&user.account_id);
    if allowed && let Err(e) = remove_keys(&state, pool, mid, [photo_key, thumb_key]).await {
        eprintln!("makes: delete {mid}: {e}");
    }
    Redirect::to(&format!("/d/{design}")).into_response()
}

/// Takes make `mid` down: gone from the page and from storage (an admin acting on a report).
pub async fn remove(state: &AppState, pool: &PgPool, mid: i64) -> Result<(), String> {
    let keys: Option<(String, String)> = sqlx::query_as("SELECT photo_key, thumb_key FROM mf_makes WHERE id = $1")
        .bind(mid)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
    match keys {
        Some((photo, thumb)) => remove_keys(state, pool, mid, [photo, thumb]).await,
        None => Ok(()),
    }
}

async fn remove_keys(state: &AppState, pool: &PgPool, mid: i64, keys: [String; 2]) -> Result<(), String> {
    sqlx::query("DELETE FROM mf_makes WHERE id = $1")
        .bind(mid)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(storage) = &state.storage {
        for key in keys {
            // (the make is gone either way; a photo left behind costs a little storage)
            if let Err(e) = storage.delete(&key).await {
                eprintln!("makes: delete {mid}: {e}");
            }
        }
    }
    Ok(())
}

/// The keys of every make's photos on design `id`, for deleting the design.
pub async fn keys(pool: &PgPool, id: i64) -> Vec<String> {
    sqlx::query_as::<_, (String, String)>("SELECT photo_key, thumb_key FROM mf_makes WHERE design_id = $1")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .flat_map(|(a, b)| [a, b])
        .collect()
}
