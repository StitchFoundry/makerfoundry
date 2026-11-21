//! A design's other files (MF-8): the same design in more formats (a PES, a DST and a JEF; an SVG
//! and a DXF), added by its maker after the first. Each is read and checked as the first was, and
//! must be the same kind (embroidery or laser). Whoever may download the design may download each.

use axum::extract::{Multipart, Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{User, current_user, forbidden, same_origin};
use crate::designs::{self, read_design};
use crate::pages::escape;
use crate::storage::new_key;

/// Files a design may have besides its first.
pub const MAX_FILES: i64 = 10;

/// One of a design's other files.
pub struct OtherFile {
    pub id: i64,
    pub file_name: String,
    pub format: String,
    pub file_key: String,
}

pub async fn others(pool: &PgPool, design: i64) -> Vec<OtherFile> {
    sqlx::query_as::<_, (i64, String, String, String)>(
        "SELECT id, file_name, format, file_key FROM mf_design_files WHERE design_id = $1 ORDER BY id",
    )
    .bind(design)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, file_name, format, file_key)| OtherFile {
        id,
        file_name,
        format,
        file_key,
    })
    .collect()
}

/// The keys of every other file of `design`, for deleting it.
pub async fn keys(pool: &PgPool, design: i64) -> Vec<String> {
    others(pool, design).await.into_iter().map(|f| f.file_key).collect()
}

/// The design's files as its maker manages them: each with a download link, the others with
/// a Remove button (unless people have bought it), and a form to add another.
pub fn manage_html(id: i64, first_format: &str, first_name: &str, others: &[OtherFile], sold: bool) -> String {
    let rows: String = others
        .iter()
        .map(|f| {
            let remove = if sold {
                String::new()
            } else {
                format!(
                    r#"<form method="post" action="/designs/{id}/files/{fid}/delete"><button type="submit" class="secondary">Remove</button></form>"#,
                    fid = f.id
                )
            };
            format!(
                r#"<li><a href="/d/{id}/download?file={fid}">{format}</a> <span class="muted">{name}</span>{remove}</li>"#,
                fid = f.id,
                format = escape(&f.format),
                name = escape(&f.file_name),
            )
        })
        .collect();
    let add = if (others.len() as i64) < MAX_FILES {
        format!(
            r#"<form method="post" action="/designs/{id}/files" enctype="multipart/form-data" class="add-file">
<label>Add the same design in another format <small>(it's read and checked like the first)</small><input type="file" name="file" required></label>
<button type="submit" class="secondary">Add the file</button>
</form>"#
        )
    } else {
        String::new()
    };
    format!(
        r#"<h2>Files</h2><ul class="files"><li><a href="/designs/{id}/download">{first}</a> <span class="muted">{first_name}</span></li>{rows}</ul>{add}"#,
        first = escape(first_format),
        first_name = escape(first_name),
    )
}

/// The formats on a design's public page, its maker's and those MakerFoundry converts it to
/// (`converted`, lower-case extensions): download links for those who may download it, the format
/// names for everyone else.
pub fn public_html(
    id: i64,
    first_format: &str,
    others: &[OtherFile],
    converted: &[&str],
    may_download: bool,
) -> String {
    if others.is_empty() && converted.is_empty() {
        return String::new();
    }
    let item = |format: &str, link: String| {
        if may_download {
            format!(r#"<li><a href="{link}">{}</a></li>"#, escape(format))
        } else {
            format!("<li>{}</li>", escape(format))
        }
    };
    let list: String = std::iter::once(item(first_format, format!("/d/{id}/download")))
        .chain(
            others
                .iter()
                .map(|f| item(&f.format, format!("/d/{id}/download?file={}", f.id))),
        )
        .collect();
    let made = if converted.is_empty() {
        String::new()
    } else {
        let list: String = converted
            .iter()
            .map(|ext| item(&ext.to_uppercase(), format!("/d/{id}/download?as={ext}")))
            .collect();
        format!(
            r#"<p class="muted">Also in these formats, converted by MakerFoundry from the maker's file (formats without thread colours keep only the colour changes):</p><ul class="tags files-public">{list}</ul>"#
        )
    };
    format!(r#"<h2>Formats</h2><ul class="tags files-public">{list}</ul>{made}"#)
}

/// The maker's design `id` → the maker, the pool and the design, or the response to give instead.
async fn own(state: &AppState, headers: &HeaderMap, id: i64) -> Result<(User, PgPool, designs::DesignRow), Response> {
    let (Some(user), Some(pool)) = (current_user(state, headers).await, state.db.clone()) else {
        return Err(Redirect::to("/signin").into_response());
    };
    match designs::design(&pool, id).await {
        Ok(Some(d)) if d.account_id == user.account_id => Ok((user, pool, d)),
        _ => Err(Redirect::to("/designs").into_response()),
    }
}

/// Another file to keep for a design (already in storage under `key`).
struct NewFile<'a> {
    design: i64,
    name: String,
    key: &'a str,
    bytes: i64,
    format: &'a str,
}

/// What became of a new file.
#[derive(Debug, PartialEq)]
enum Saved {
    Kept,
    /// The design has `MAX_FILES` other files already.
    Full,
    /// The design has another file of this format already.
    HasFormat,
}

/// Keeps a file's row, holding a lock on the design so uploads sent at the same moment can't
/// pass the cap or keep a format twice (issue #163).
async fn save(pool: &PgPool, f: &NewFile<'_>) -> Result<Saved, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("mf-design-files-{}", f.design))
        .execute(&mut *tx)
        .await?;
    let (count, same): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE format = $2) FROM mf_design_files WHERE design_id = $1",
    )
    .bind(f.design)
    .bind(f.format)
    .fetch_one(&mut *tx)
    .await?;
    let refused = if same > 0 {
        Some(Saved::HasFormat)
    } else if count >= MAX_FILES {
        Some(Saved::Full)
    } else {
        None
    };
    if let Some(r) = refused {
        tx.rollback().await?;
        return Ok(r);
    }
    sqlx::query(
        "INSERT INTO mf_design_files (design_id, file_name, file_key, file_bytes, format) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(f.design)
    .bind(&f.name)
    .bind(f.key)
    .bind(f.bytes)
    .bind(f.format)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Saved::Kept)
}

/// Another file for design `id`.
pub async fn add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool, d) = match own(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let account = user.account_id;
    let Some(storage) = state.storage.clone() else {
        return designs::manage_page(&state, &pool, &user, id, "Keeping files isn't switched on yet.").await;
    };
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_design_files WHERE design_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap_or(MAX_FILES);
    if count >= MAX_FILES {
        return designs::manage_page(
            &state,
            &pool,
            &user,
            id,
            &format!("A design can have up to {} files.", MAX_FILES + 1),
        )
        .await;
    }
    if !state.limits.allow(crate::limits::UPLOAD, &account.to_string()) {
        return designs::manage_page(
            &state,
            &pool,
            &user,
            id,
            "You've added a lot of files today. Please try again tomorrow.",
        )
        .await;
    }
    let mut file: Option<(String, Vec<u8>)> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() != Some("file") {
            continue;
        }
        let name: String = field.file_name().unwrap_or("design").chars().take(200).collect();
        match field.bytes().await {
            Ok(bytes) if !bytes.is_empty() => file = Some((name, bytes.to_vec())),
            _ => {
                return designs::manage_page(
                    &state,
                    &pool,
                    &user,
                    id,
                    "The file didn't come through whole. Please try again.",
                )
                .await;
            }
        }
    }
    let Some((file_name, bytes)) = file else {
        return designs::manage_page(&state, &pool, &user, id, "Choose a file to add.").await;
    };
    let read_name = file_name.clone();
    let read_bytes = bytes.clone();
    let read = match tokio::task::spawn_blocking(move || read_design(&read_bytes, &read_name)).await {
        Ok(Ok(r)) => r,
        Ok(Err(why)) => {
            return designs::manage_page(
                &state,
                &pool,
                &user,
                id,
                &format!("{}: {why}.", file_name.chars().take(80).collect::<String>()),
            )
            .await;
        }
        Err(_) => {
            return designs::manage_page(&state, &pool, &user, id, "The file couldn't be read. Please try again.")
                .await;
        }
    };
    if read.kind() != d.kind {
        return designs::manage_page(
            &state,
            &pool,
            &user,
            id,
            if d.kind == "laser" {
                "This is a laser design: add a laser file (SVG, DXF or PDF)."
            } else {
                "This is an embroidery design: add an embroidery file."
            },
        )
        .await;
    }
    // one file a format
    let first_format = d.facts.get("format").and_then(serde_json::Value::as_str).unwrap_or("");
    let has = first_format == read.format() || others(&pool, id).await.iter().any(|f| f.format == read.format());
    if has {
        let why = format!("This design already has a {} file.", read.format());
        return designs::manage_page(&state, &pool, &user, id, &why).await;
    }
    let kept = async {
        let key = new_key("design")?;
        let size = bytes.len() as i64;
        storage.put(&key, bytes, "application/octet-stream").await?;
        let new = NewFile {
            design: id,
            name: designs::safe_file_name(&file_name, Some(read.format())),
            key: &key,
            bytes: size,
            format: read.format(),
        };
        let saved = save(&pool, &new).await;
        if !matches!(saved, Ok(Saved::Kept)) {
            // (best effort: an orphaned file costs a little storage)
            let _ = storage.delete(&key).await;
        }
        saved.map_err(|e| e.to_string())
    };
    match kept.await {
        Ok(Saved::Kept) => Redirect::to(&format!("/designs/{id}")).into_response(),
        Ok(Saved::Full) => {
            let why = format!("A design can have up to {} files.", MAX_FILES + 1);
            designs::manage_page(&state, &pool, &user, id, &why).await
        }
        Ok(Saved::HasFormat) => {
            let why = format!("This design already has a {} file.", read.format());
            designs::manage_page(&state, &pool, &user, id, &why).await
        }
        Err(e) => {
            eprintln!("files: {id}: {e}");
            designs::manage_page(
                &state,
                &pool,
                &user,
                id,
                "The file couldn't be kept just now. Please try again in a few minutes.",
            )
            .await
        }
    }
}

/// Removes another file from design `id` (not once people have bought it: they keep every file).
pub async fn remove(State(state): State<AppState>, headers: HeaderMap, Path((id, file)): Path<(i64, i64)>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (_, pool, _) = match own(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    if crate::selling::has_sales(&pool, id).await {
        return Redirect::to(&format!("/designs/{id}")).into_response();
    }
    let key: Option<String> =
        sqlx::query_scalar("DELETE FROM mf_design_files WHERE id = $1 AND design_id = $2 RETURNING file_key")
            .bind(file)
            .bind(id)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
    if let (Some(key), Some(storage)) = (key, state.storage.as_ref())
        && let Err(e) = storage.delete(&key).await
    {
        eprintln!("files: remove {file}: {e}");
    }
    Redirect::to(&format!("/designs/{id}")).into_response()
}
