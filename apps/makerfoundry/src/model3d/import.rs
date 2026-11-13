//! Importing a creator's own models in bulk (MF-52): many files at once, or ZIPs of them, each
//! kept as a pending import file as it arrives (so memory holds one file at a time), then grouped
//! into models, by folder inside a ZIP, else by name: files whose names share a stem once endings
//! like `_plate2`, `-v2`, `_part3` or ` (1)` are taken off are one model, and images join the
//! model whose stem they share. The creator sees the groups and makes them into drafts; a file
//! that fails its checks is reported and left out, the rest kept (docs/3d-hub-plan.md § 8).

/// One model found among the files: its name and its members (indexes into the names given).
#[derive(Debug, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub members: Vec<usize>,
}

/// Whether a name is an image (by its extension).
pub fn is_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp"].iter().any(|e| lower.ends_with(e))
}

/// The folder part of a path inside a ZIP (`""` for none) and the file's own name.
fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((folder, name)) => (folder, name),
        None => ("", path),
    }
}

/// A file's stem with the endings that mark parts, plates, versions and copies taken off:
/// `Bracket_plate2.3mf` → `Bracket`, `gear-v2 (1).stl` → `gear`.
pub fn stem(name: &str) -> String {
    let mut s = name.rsplit_once('.').map_or(name, |(s, _)| s).trim().to_string();
    loop {
        let before = s.len();
        // " (1)"
        if s.ends_with(')')
            && let Some(open) = s.rfind('(')
            && s.get(open + 1..s.len() - 1)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            s.truncate(open);
            s = s.trim_end().to_string();
        }
        // "_plate2", "-v2", " part 3", "_version10"
        let lower = s.to_ascii_lowercase();
        let digits = lower.len() - lower.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            let head = lower[..lower.len() - digits].trim_end_matches([' ', '_', '-']);
            for word in ["plate", "part", "version", "v"] {
                if let Some(rest) = head.strip_suffix(word)
                    && (rest.is_empty() || rest.ends_with([' ', '_', '-']))
                    && !rest.trim_end_matches([' ', '_', '-']).is_empty()
                {
                    s.truncate(rest.trim_end_matches([' ', '_', '-']).len());
                    break;
                }
            }
        }
        if s.len() == before {
            return s;
        }
    }
}

/// Groups files (paths as given, `folder/name` inside a ZIP) into models.
pub fn group(paths: &[String]) -> Vec<Group> {
    let mut groups: Vec<(String, String, Vec<usize>)> = Vec::new(); // (key, name, members)
    let key_of = |path: &str| -> (String, String) {
        let (folder, name) = split_path(path);
        if folder.is_empty() {
            let s = stem(name);
            (format!("stem:{}", s.to_lowercase()), s)
        } else {
            let shown = folder.rsplit('/').next().unwrap_or(folder).to_string();
            (format!("folder:{}", folder.to_lowercase()), shown)
        }
    };
    // models first, so images have somewhere to go
    for (i, path) in paths.iter().enumerate().filter(|(_, p)| !is_image(p)) {
        let (key, name) = key_of(path);
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.2.push(i),
            None => groups.push((key, name, vec![i])),
        }
    }
    for (i, path) in paths.iter().enumerate().filter(|(_, p)| is_image(p)) {
        let (key, _) = key_of(path);
        let stem_key = key.strip_prefix("stem:").unwrap_or("");
        let home = groups
            .iter()
            .position(|g| g.0 == key)
            // "bracket photo 1.jpg" beside "bracket.stl"
            .or_else(|| {
                groups.iter().position(|g| {
                    g.0.strip_prefix("stem:")
                        .is_some_and(|k| !k.is_empty() && stem_key.starts_with(k))
                })
            })
            .or_else(|| (groups.len() == 1).then_some(0));
        if let Some(g) = home.and_then(|h| groups.get_mut(h)) {
            g.2.push(i);
        }
    }
    groups
        .into_iter()
        .map(|(_, name, members)| Group { name, members })
        .collect()
}

// ---------------------------------------------------------------- the pages

use axum::extract::{Multipart, Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use sqlx::PgPool;

use super::files;
use super::upload::{self, clean_name};
use crate::AppState;
use crate::accounts::{User, current_user, forbidden, private, same_origin};
use crate::designs::Refused;
use crate::pages::escape;
use crate::storage::{Storage, new_key};

/// The most files one import may bring, counting what's inside ZIPs (the owner's decision,
/// 2026-10-30).
pub const MAX_FILES: usize = 200;
/// The most one import may bring, all told (files, and what ZIPs unpack to).
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The largest ZIP sent (it's read whole, in memory).
pub const MAX_ZIP_BYTES: usize = upload::MAX_MODEL_BYTES;
/// The import route's cap.
pub const BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024 * 1024 + 1024 * 1024;
/// How long making drafts may go on in one request before the rest wait for the next press.
const DRAFTS_TIME: std::time::Duration = std::time::Duration::from_secs(200);

fn not_found() -> Response {
    crate::designs::not_found()
}

/// The signed-in maker who may import (as for uploads), and the database; or the answer instead.
pub(crate) async fn importer(state: &AppState, headers: &HeaderMap) -> Result<(User, PgPool), Response> {
    let viewer = current_user(state, headers).await;
    match (viewer, state.db.clone()) {
        (Some(u), Some(pool)) if super::open_to(state, Some(&u)).await => Ok((u, pool)),
        (None, _) if state.three_d => Err(Redirect::to("/signin?next=/3d/import").into_response()),
        _ => Err(not_found()),
    }
}

fn import_form(problem: &str, linked: bool) -> String {
    let linked = if linked {
        r#"<p>Or <a class=\"button secondary\" href=\"/3d/import/thingiverse\">bring in your Thingiverse things</a>: sign in there and we bring only the things you made, with their licence and date.</p>"#
    } else {
        ""
    };
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    };
    format!(
        r#"<section class="form">
<h1>Import your models</h1>
<p class="lead">Bring the models you've made and shared elsewhere: choose many files at once, or ZIPs of them (an export from your account on another site, or a folder of your own). Each folder in a ZIP becomes one model; loose files are grouped by name, so <code>bracket_plate1.3mf</code> and <code>bracket_plate2.3mf</code> make one model, and <code>bracket.jpg</code> joins it.</p>
{problem}
<form method="post" action="/3d/import" enctype="multipart/form-data">
<label>Files or ZIPs <small>(STL, 3MF, OBJ, STEP, images; up to {MAX_FILES} files or {gb} GB at once)</small><input type="file" name="files" multiple required></label>
<button type="submit">Import</button>
</form>
{linked}
<p class="muted">Only import models you made or have the right to share. You'll see how they were grouped, and each becomes a draft for you to check before publishing.</p>
</section>"#,
        gb = MAX_BYTES / (1024 * 1024 * 1024),
    )
}

pub async fn new_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = importer(&state, &headers).await {
        return r;
    }
    private(
        "Import your models · MakerFoundry",
        &import_form("", state.thingiverse.is_some()),
        true,
    )
}

/// What one import has taken so far.
pub(crate) struct Taking<'a> {
    storage: &'a Storage,
    pool: &'a PgPool,
    import: i64,
    count: usize,
    bytes: u64,
    stored: Vec<String>,
    /// Where the next files were first published (address, date, licence), for a linked
    /// account's import (`thingiverse`).
    pub(crate) origin: Option<(Option<String>, Option<String>, Option<&'static str>)>,
}

impl<'a> Taking<'a> {
    pub(crate) fn new(storage: &'a Storage, pool: &'a PgPool, import: i64) -> Self {
        Taking {
            storage,
            pool,
            import,
            count: 0,
            bytes: 0,
            stored: Vec::new(),
            origin: None,
        }
    }

    /// Deletes everything this import stored (a refused import keeps nothing).
    pub(crate) async fn forget(&self) {
        for key in &self.stored {
            let _ = self.storage.delete(key).await;
        }
    }

    /// Notes one file of the import: stored as pending if it's a model file or an image, else
    /// only noted with why it's left out.
    pub(crate) async fn take(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), String> {
        self.count += 1;
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if self.count > MAX_FILES {
            return Err(format!(
                "An import can bring up to {MAX_FILES} files at once (counting those in ZIPs)."
            ));
        }
        if self.bytes > MAX_BYTES {
            return Err(format!(
                "An import can bring up to {} GB at once.",
                MAX_BYTES / (1024 * 1024 * 1024)
            ));
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let image = is_image(name);
        let problem = if image || files::kind_of(name).is_some() {
            None
        } else {
            Some("isn't a model file or an image MakerFoundry takes")
        };
        let key = match problem {
            None => {
                let key = new_key("import")?;
                self.storage
                    .put(&key, bytes.clone(), "application/octet-stream")
                    .await?;
                self.stored.push(key.clone());
                Some(key)
            }
            Some(_) => None,
        };
        sqlx::query(
            "INSERT INTO mf_import_files (import_id, path, file_key, file_bytes, image, problem, position, origin_url, origin_date, origin_licence)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::date, $10)",
        )
        .bind(self.import)
        .bind(path)
        .bind(key)
        .bind(bytes.len() as i64)
        .bind(image)
        .bind(problem)
        .bind(i32::try_from(self.count).unwrap_or(i32::MAX))
        .bind(self.origin.as_ref().and_then(|o| o.0.clone()))
        .bind(self.origin.as_ref().and_then(|o| o.1.clone()))
        .bind(self.origin.as_ref().and_then(|o| o.2))
        .execute(self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Takes every file in a ZIP sent: folders kept in their paths, system files skipped.
    async fn take_zip(&mut self, zip_name: &str, bytes: &[u8]) -> Result<(), String> {
        let archive = super::zip::Archive::open(bytes).map_err(|why| format!("{zip_name}: {why}."))?;
        for entry in archive.entries() {
            let path = entry.name.as_str();
            let base = path.rsplit('/').next().unwrap_or(path);
            if path.ends_with('/') || path.starts_with("__MACOSX/") || base.starts_with('.') || base.is_empty() {
                continue;
            }
            if entry.size > super::mesh::MAX_FILE_BYTES as u64 {
                return Err(format!("{zip_name}: {path} is larger than 100 MB."));
            }
            let data = archive.read(entry).map_err(|why| format!("{zip_name}: {why}."))?;
            self.take(path, data).await?;
        }
        Ok(())
    }
}

/// Reads one part of the form, refusing it once it passes `max` bytes.
async fn read_part(field: &mut axum::extract::multipart::Field<'_>, max: usize) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|_| ())? {
        if out.len() + chunk.len() > max {
            return Err(());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

pub async fn create(State(state): State<AppState>, headers: HeaderMap, mut multipart: Multipart) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool) = match importer(&state, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some(storage) = state.storage.as_ref() else {
        return Redirect::to("/3d/import").into_response();
    };
    let again = |why: &str| {
        private(
            "Import your models · MakerFoundry",
            &import_form(why, state.thingiverse.is_some()),
            true,
        )
    };
    if !state.limits.allow(crate::limits::IMPORT, &user.account_id.to_string()) {
        return again("You've imported a lot today. Please try again tomorrow.");
    }
    let import: i64 = match sqlx::query_scalar("INSERT INTO mf_imports (account_id) VALUES ($1) RETURNING id")
        .bind(user.account_id)
        .fetch_one(&pool)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            eprintln!("import: {e}");
            return again("The import couldn't be started just now. Please try again.");
        }
    };
    let mut taking = Taking::new(storage, &pool, import);
    let outcome: Result<(), String> = async {
        loop {
            let mut field = match multipart.next_field().await {
                Ok(Some(f)) => f,
                Ok(None) => return Ok(()),
                Err(_) => return Err("The import didn't come through whole. Please try again.".into()),
            };
            if field.name() != Some("files") {
                continue;
            }
            let name = clean_name(field.file_name().unwrap_or("file"));
            let zip = name.to_ascii_lowercase().ends_with(".zip");
            let max = if zip {
                MAX_ZIP_BYTES
            } else {
                super::mesh::MAX_FILE_BYTES
            };
            let bytes = read_part(&mut field, max).await.map_err(|()| {
                format!(
                    "{name} is larger than {} MB, or didn't come through whole.",
                    max / (1024 * 1024)
                )
            })?;
            if bytes.is_empty() {
                continue;
            }
            if zip {
                taking.take_zip(&name, &bytes).await?;
            } else {
                taking.take(&name, bytes).await?;
            }
        }
    }
    .await;
    let outcome = outcome.and_then(|()| {
        if taking.count == 0 {
            Err("Choose the files or ZIPs to import.".to_string())
        } else {
            Ok(())
        }
    });
    if let Err(why) = outcome {
        // nothing of a refused import is kept
        for key in &taking.stored {
            let _ = storage.delete(key).await;
        }
        let _ = sqlx::query("DELETE FROM mf_imports WHERE id = $1")
            .bind(import)
            .execute(&pool)
            .await;
        return again(&why);
    }
    if let Err(e) = regroup(&pool, import).await {
        eprintln!("import {import}: {e}");
    }
    Redirect::to(&format!("/3d/import/{import}")).into_response()
}

/// Groups an import's waiting files (`group`), noting each one's group; an image with no model to
/// join is noted as left out.
pub async fn regroup(pool: &PgPool, import: i64) -> Result<(), sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, path FROM mf_import_files WHERE import_id = $1 AND problem IS NULL AND design_id IS NULL ORDER BY position",
    )
    .bind(import)
    .fetch_all(pool)
    .await?;
    let paths: Vec<String> = rows.iter().map(|(_, p)| p.clone()).collect();
    let mut placed = vec![false; rows.len()];
    let mut tx = pool.begin().await?;
    for g in group(&paths) {
        for &m in &g.members {
            if let (Some((id, _)), Some(p)) = (rows.get(m), placed.get_mut(m)) {
                *p = true;
                sqlx::query("UPDATE mf_import_files SET group_name = $2 WHERE id = $1")
                    .bind(id)
                    .bind(&g.name)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    for ((id, _), p) in rows.iter().zip(placed) {
        if !p {
            sqlx::query(
                "UPDATE mf_import_files SET group_name = '', problem = 'has no model of the same name to join' WHERE id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await
}

/// One file of an import as its review shows it: path, group, draft made, why it's left out.
type FileRow = (i64, String, String, Option<i64>, Option<String>);

/// The import `id` if `user` made it.
async fn own_import(pool: &PgPool, user: &User, id: i64) -> Option<()> {
    let owner: Option<i64> = sqlx::query_scalar("SELECT account_id FROM mf_imports WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    (owner == Some(user.account_id)).then_some(())
}

pub async fn review(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let (user, pool) = match importer(&state, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    if own_import(&pool, &user, id).await.is_none() {
        return not_found();
    }
    private(
        "Your import · MakerFoundry",
        &review_html(&state, &pool, id, "").await,
        true,
    )
}

/// One of an import's drafts as its review shows it: id, title, status, category, licence.
type DraftRow = (i64, String, String, Option<String>, Option<String>);

async fn review_html(state: &AppState, pool: &PgPool, id: i64, note: &str) -> String {
    let rows: Vec<FileRow> = sqlx::query_as(
        "SELECT id, path, group_name, design_id, problem FROM mf_import_files WHERE import_id = $1 ORDER BY group_name, position",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    // the groups still waiting to be drafts, which can be arranged
    let mut waiting: Vec<(&str, Vec<&FileRow>)> = Vec::new();
    for r in rows.iter().filter(|r| r.4.is_none() && r.3.is_none()) {
        match waiting.iter_mut().find(|g| g.0 == r.2) {
            Some(g) => g.1.push(r),
            None => waiting.push((&r.2, vec![r])),
        }
    }
    let arrange = if waiting.is_empty() {
        String::new()
    } else {
        let groups: String = waiting
            .iter()
            .enumerate()
            .map(|(n, (name, files))| {
                let items: String = files
                    .iter()
                    .map(|r| {
                        format!(
                            r#"<li><label>{path} <small>in</small> <input name="f{fid}" value="{name}" maxlength="100" aria-label="Model for {path}"></label></li>"#,
                            fid = r.0,
                            path = escape(&r.1),
                            name = escape(name),
                        )
                    })
                    .collect();
                format!(
                    r#"<li><input type="hidden" name="old{n}" value="{name}"><label>Model name <input name="new{n}" value="{name}" maxlength="100"></label><ul>{items}</ul></li>"#,
                    name = escape(name),
                )
            })
            .collect();
        let count = waiting.len();
        format!(
            r#"<h2>Waiting to be drafts</h2>
<p class="muted">Rename a model, or move a file by typing another model's name beside it (a new name makes it a model of its own).</p>
<form method="post" action="/3d/import/{id}/arrange"><ol class="import-groups">{groups}</ol><button type="submit" class="secondary">Save the changes</button></form>
<form method="post" action="/3d/import/{id}/drafts"><button type="submit">Make {count} draft{s}</button></form>"#,
            s = if count == 1 { "" } else { "s" },
        )
    };
    // the drafts already made
    let drafts: Vec<DraftRow> = sqlx::query_as(
        "SELECT id, title, status, category, licence FROM mf_designs WHERE import_id = $1 ORDER BY title, id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let made = if drafts.is_empty() {
        String::new()
    } else {
        let list: String = drafts
            .iter()
            .map(|(did, title, status, category, licence)| {
                let state = match (status.as_str(), category, licence) {
                    ("published", _, _) => "published".to_string(),
                    ("draft", Some(_), Some(_)) => "ready to publish".to_string(),
                    ("draft", _, _) => "needs a category and a licence".to_string(),
                    (other, _, _) => other.to_string(),
                };
                format!(
                    r#"<li><a href="/designs/{did}">{title}</a> <span class="muted">· {state}</span></li>"#,
                    title = escape(title)
                )
            })
            .collect();
        let waiting_drafts = drafts.iter().filter(|d| d.2 == "draft").count();
        let forms = if waiting_drafts == 0 {
            String::new()
        } else {
            let categories: String = super::CATEGORIES
                .iter()
                .map(|c| format!(r#"<option value="{}">{}</option>"#, c.id, escape(c.name)))
                .collect();
            let licences: String = crate::licences::for_kind("model3d")
                .iter()
                .map(|l| format!(r#"<option value="{}">{}</option>"#, l.id, escape(l.name)))
                .collect();
            let seller_terms = if state.launched {
                r#" I agree to the <a href="/seller-terms">Seller Terms</a>."#
            } else {
                ""
            };
            format!(
                r#"<form method="post" action="/3d/import/{id}/details" class="form">
<p class="muted">Give every draft that has none yet a category and a licence (each draft's own Edit details can change them, and add a description and tags).</p>
<label>Category<select name="category" required><option value="">Choose one</option>{categories}</select></label>
<label>Licence<select name="licence" required><option value="">Choose one</option>{licences}</select></label>
<button type="submit" class="secondary">Give them to every draft</button>
</form>
<form method="post" action="/3d/import/{id}/publish" class="publish">
<label class="choice"><input type="checkbox" name="confirm" value="yes"> <span>I confirm I created these models or have the right to share them, and they don't copy anyone else's logos, characters or designs.{seller_terms}</span></label>
{showcase}
<button type="submit">Publish every ready draft</button>
</form>"#,
                showcase = super::showcase::CHECKBOX,
            )
        };
        format!(r#"<h2>Drafts</h2><ul class="import-drafts">{list}</ul>{forms}"#)
    };
    let left_out: String = rows
        .iter()
        .filter_map(|r| {
            r.4.as_ref()
                .map(|why| format!("<li>{}: {}</li>", escape(&r.1), escape(why)))
        })
        .collect();
    let left_out = if left_out.is_empty() {
        String::new()
    } else {
        format!(r#"<h2>Left out</h2><ul class="problems">{left_out}</ul>"#)
    };
    let note = if note.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(note))
    };
    let count = waiting.len() + drafts.len();
    format!(
        r#"<section class="design"><h1>Your import</h1>{note}<p class="lead">{count} model{s} found.</p>{arrange}{made}{left_out}<p><a href="/3d/import">Import more</a></p></section>"#,
        s = if count == 1 { "" } else { "s" },
    )
}

/// A model's name as typed: spaces tidied, at most 100 characters.
fn tidy_name(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(100)
        .collect()
}

/// Moves files between an import's waiting groups and renames groups: `f<file>` gives a file's
/// model (another's name merges it in, a new name splits it out), and `old<n>`/`new<n>` rename a
/// model, for every file that ends up in it.
pub async fn arrange(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    axum::Form(form): axum::Form<Vec<(String, String)>>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool) = match importer(&state, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    if own_import(&pool, &user, id).await.is_none() {
        return not_found();
    }
    let value = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| tidy_name(v));
    let renames: Vec<(String, String)> = (0..form.len())
        .map_while(|n| Some((value(&format!("old{n}"))?, value(&format!("new{n}"))?)))
        .filter(|(old, new)| !new.is_empty() && old != new)
        .collect();
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, group_name FROM mf_import_files WHERE import_id = $1 AND design_id IS NULL AND problem IS NULL",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    for (fid, current) in rows {
        let target = value(&format!("f{fid}"))
            .filter(|t| !t.is_empty())
            .unwrap_or(current.clone());
        let target = renames
            .iter()
            .find(|(old, _)| *old == target)
            .map_or(target.clone(), |(_, new)| new.clone());
        if target != current {
            let _ = sqlx::query("UPDATE mf_import_files SET group_name = $2 WHERE id = $1")
                .bind(fid)
                .bind(&target)
                .execute(&pool)
                .await;
        }
    }
    Redirect::to(&format!("/3d/import/{id}")).into_response()
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct DetailsForm {
    category: String,
    licence: String,
}

/// Gives every draft of an import that has none yet this category and licence.
pub async fn details(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    axum::Form(f): axum::Form<DetailsForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool) = match importer(&state, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    if own_import(&pool, &user, id).await.is_none() {
        return not_found();
    }
    let problem = if super::category(&f.category).is_none() {
        Some("Choose a category.")
    } else if crate::licences::find_for("model3d", &f.licence).is_none() {
        Some("Choose a licence.")
    } else {
        None
    };
    if let Some(p) = problem {
        return private(
            "Your import · MakerFoundry",
            &review_html(&state, &pool, id, p).await,
            true,
        );
    }
    if let Err(e) = sqlx::query(
        "UPDATE mf_designs SET category = coalesce(category, $3), licence = coalesce(licence, $4)
         WHERE import_id = $1 AND account_id = $2 AND status = 'draft'",
    )
    .bind(id)
    .bind(user.account_id)
    .bind(&f.category)
    .bind(&f.licence)
    .execute(&pool)
    .await
    {
        eprintln!("import {id}: details: {e}");
    }
    Redirect::to(&format!("/3d/import/{id}")).into_response()
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct PublishForm {
    confirm: String,
    showcase: String,
}

/// Publishes every draft of an import that has its title, category and licence, once the creator
/// confirms they made them or may share them; the others are named with what they still need.
pub async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    axum::Form(f): axum::Form<PublishForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool) = match importer(&state, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    if own_import(&pool, &user, id).await.is_none() {
        return not_found();
    }
    let again = |note: String| {
        let (state, pool) = (state.clone(), pool.clone());
        async move {
            private(
                "Your import · MakerFoundry",
                &review_html(&state, &pool, id, &note).await,
                true,
            )
        }
    };
    if f.confirm != "yes" {
        return again("Confirm that you created these models or have the right to share them.".into()).await;
    }
    if !state.publishers.allows(user.account_id) {
        return again("Publishing opens when MakerFoundry launches. Your drafts stay saved.".into()).await;
    }
    if crate::makers::profile(&pool, user.account_id).await.is_none() {
        return again("Set up your maker profile first: it's the name people see on your models.".into()).await;
    }
    let drafts: Vec<(i64, String, bool, i64, Option<String>)> = sqlx::query_as(
        "SELECT id, title, (title <> '' AND category IS NOT NULL AND licence IS NOT NULL), price_cents, licence
         FROM mf_designs WHERE import_id = $1 AND account_id = $2 AND status = 'draft' ORDER BY title",
    )
    .bind(id)
    .bind(user.account_id)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    let can_sell = crate::selling::can_sell(&state, &pool, user.account_id).await;
    let mut waiting = Vec::new();
    let mut published = 0;
    for (did, title, ready, price, licence) in drafts {
        // a remix's licence has to be one its parents allow, as on a model's own publish (#139)
        let refused = match licence.as_deref() {
            Some(l) if ready => super::remix::check_licence(&pool, did, user.account_id, l).await.err(),
            _ => None,
        };
        if !ready {
            waiting.push(format!("{title} needs a category and a licence"));
        } else if let Some(why) = refused {
            waiting.push(format!("{title}: {}", why.trim_end_matches('.')));
        } else if price > 0 && !can_sell {
            waiting.push(format!("{title} has a price: finish setting up selling first"));
        } else if sqlx::query(
            "UPDATE mf_designs SET status = 'published', published_at = coalesce(published_at, now()) WHERE id = $1 AND status = 'draft'",
        )
        .bind(did)
        .execute(&pool)
        .await
        .is_ok()
        {
            published += 1;
            crate::notices::remix(&state, &pool, did, user.account_id).await;
            if f.showcase == "yes" {
                super::showcase::give(&pool, user.account_id, Some(did), None).await;
            }
        }
    }
    if published > 0 && state.launched {
        crate::legal::agree(&pool, user.account_id, &[&crate::legal::SELLER_TERMS]).await;
    }
    if waiting.is_empty() {
        Redirect::to(&format!("/3d/import/{id}")).into_response()
    } else {
        again(format!("Not published yet: {}.", waiting.join("; "))).await
    }
}

/// Makes each waiting group of an import into a draft: its files fetched and checked one by one
/// (a bad one is noted and left out, the rest kept), then added as an upload is.
pub async fn drafts(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool) = match importer(&state, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    if own_import(&pool, &user, id).await.is_none() {
        return not_found();
    }
    let Some(storage) = state.storage.as_ref() else {
        return not_found();
    };
    let started = std::time::Instant::now();
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT group_name FROM mf_import_files
         WHERE import_id = $1 AND design_id IS NULL AND problem IS NULL AND group_name <> '' ORDER BY group_name",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    let mut note = String::new();
    for name in names {
        if started.elapsed() > DRAFTS_TIME {
            note = "Some models are still waiting: press the button again to carry on.".into();
            break;
        }
        match make_draft(&state, storage, &pool, user.account_id, id, &name).await {
            Ok(()) => {}
            Err(Refused::TooMany(why)) => {
                note = why;
                break;
            }
            Err(r) => {
                let why = r.message();
                let _ = sqlx::query(
                    "UPDATE mf_import_files SET problem = $3 WHERE import_id = $1 AND group_name = $2 AND design_id IS NULL AND problem IS NULL",
                )
                .bind(id)
                .bind(&name)
                .bind(&why)
                .execute(&pool)
                .await;
            }
        }
    }
    if note.is_empty() {
        Redirect::to(&format!("/3d/import/{id}")).into_response()
    } else {
        private(
            "Your import · MakerFoundry",
            &review_html(&state, &pool, id, &note).await,
            true,
        )
    }
}

async fn make_draft(
    state: &AppState,
    storage: &Storage,
    pool: &PgPool,
    account: i64,
    import: i64,
    name: &str,
) -> Result<(), Refused> {
    let rows: Vec<(i64, String, String, bool)> = sqlx::query_as(
        "SELECT id, path, file_key, image FROM mf_import_files
         WHERE import_id = $1 AND group_name = $2 AND design_id IS NULL AND problem IS NULL AND file_key IS NOT NULL
         ORDER BY position",
    )
    .bind(import)
    .bind(name)
    .fetch_all(pool)
    .await
    .map_err(|_| Refused::Failed)?;
    let mut model_files = Vec::new();
    let mut images = Vec::new();
    let mut used = Vec::new();
    let leave_out = |fid: i64, why: String| async move {
        let _ = sqlx::query("UPDATE mf_import_files SET problem = $2 WHERE id = $1")
            .bind(fid)
            .bind(why)
            .execute(pool)
            .await;
    };
    for (fid, path, key, image) in rows {
        let Ok(Some(bytes)) = storage.get(&key).await else {
            return Err(Refused::Failed);
        };
        let file_name = clean_name(&path);
        if image {
            if images.len() >= upload::MAX_IMAGES {
                leave_out(
                    fid,
                    format!("left out: a model can have up to {} images", upload::MAX_IMAGES),
                )
                .await;
                used.push((fid, key));
                continue;
            }
            let check = bytes.clone();
            match tokio::task::spawn_blocking(move || crate::photos::read(&check)).await {
                Ok(Ok(_)) => images.push(bytes),
                Ok(Err(why)) => leave_out(fid, format!("left out: {why}")).await,
                Err(_) => leave_out(fid, "left out: it couldn't be read".into()).await,
            }
        } else {
            if model_files.len() >= upload::MAX_FILES {
                leave_out(
                    fid,
                    format!("left out: a model can have up to {} files", upload::MAX_FILES),
                )
                .await;
                used.push((fid, key));
                continue;
            }
            let (check, check_name) = (bytes.clone(), file_name.clone());
            match tokio::task::spawn_blocking(move || files::check(&check_name, &check).map(|_| ())).await {
                Ok(Ok(())) => model_files.push((file_name, bytes)),
                Ok(Err(why)) => leave_out(fid, format!("left out: {why}")).await,
                Err(_) => leave_out(fid, "left out: it couldn't be read".into()).await,
            }
        }
        used.push((fid, key));
    }
    if model_files.is_empty() {
        return Err(Refused::Bad(
            "left out: none of this model's files could be read".into(),
        ));
    }
    let design = upload::add(state, pool, account, model_files, images).await?;
    let _ = sqlx::query(
        "UPDATE mf_designs SET import_id = $2, title = $3, search = setweight(to_tsvector('english', $3), 'A') WHERE id = $1",
    )
        .bind(design)
        .bind(import)
        .bind(name.chars().take(crate::listings::MAX_TITLE).collect::<String>())
        .execute(pool)
        .await;
    // a linked account's thing: where it was first published, and its licence when it maps
    let _ = sqlx::query(
        "UPDATE mf_designs d SET original_site = 'thingiverse', original_url = f.origin_url, original_published_at = f.origin_date,
                licence = coalesce(d.licence, f.origin_licence)
         FROM (SELECT origin_url, origin_date, origin_licence FROM mf_import_files
               WHERE import_id = $2 AND group_name = $3 AND origin_url IS NOT NULL LIMIT 1) f
         WHERE d.id = $1",
    )
    .bind(design)
    .bind(import)
    .bind(name)
    .execute(pool)
    .await;
    for (fid, key) in used {
        let _ = sqlx::query("UPDATE mf_import_files SET design_id = $2, file_key = NULL WHERE id = $1")
            .bind(fid)
            .bind(design)
            .execute(pool)
            .await;
        let _ = storage.delete(&key).await;
    }
    Ok(())
}

/// Pending import files and unpublished drafts from imports, 30 days on (the 10-minute sweep).
pub async fn forget_old(pool: &PgPool, storage: Option<&Storage>) -> Result<(), sqlx::Error> {
    let keys: Vec<String> = sqlx::query_scalar(
        "WITH old AS (
             SELECT f.id, f.file_key FROM mf_import_files f JOIN mf_imports i ON i.id = f.import_id
             WHERE i.created_at < now() - interval '30 days' AND f.file_key IS NOT NULL FOR UPDATE OF f)
         UPDATE mf_import_files f SET file_key = NULL, problem = coalesce(f.problem, 'not made into a draft within 30 days')
         FROM old WHERE f.id = old.id RETURNING old.file_key",
    )
    .fetch_all(pool)
    .await?;
    let old: Vec<i64> = sqlx::query_scalar(
        "SELECT d.id FROM mf_designs d JOIN mf_imports i ON i.id = d.import_id
         WHERE d.status = 'draft' AND d.published_at IS NULL AND i.created_at < now() - interval '30 days'
           AND NOT EXISTS (SELECT 1 FROM mf_orders o WHERE o.design_id = d.id)",
    )
    .fetch_all(pool)
    .await?;
    let mut keys = keys;
    if !old.is_empty() {
        keys.extend(upload::keys(pool, &old).await);
        let photos: Vec<(String, String)> =
            sqlx::query_as("SELECT photo_key, thumb_key FROM mf_design_photos WHERE design_id = ANY($1)")
                .bind(&old)
                .fetch_all(pool)
                .await?;
        keys.extend(photos.into_iter().flat_map(|(a, b)| [a, b]));
        sqlx::query("DELETE FROM mf_designs WHERE id = ANY($1)")
            .bind(&old)
            .execute(pool)
            .await?;
    }
    if let Some(storage) = storage {
        for key in keys {
            if let Err(e) = storage.delete(&key).await {
                eprintln!("imports: {e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn grouped(list: &[&str]) -> Vec<(String, Vec<String>)> {
        let paths = names(list);
        group(&paths)
            .into_iter()
            .map(|g| (g.name, g.members.iter().map(|&i| paths[i].clone()).collect()))
            .collect()
    }

    #[test]
    fn endings_come_off_stems() {
        for (name, want) in [
            ("Bracket_plate2.3mf", "Bracket"),
            ("gear-v2.stl", "gear"),
            ("gear-v2 (1).stl", "gear"),
            ("Robot arm_part3.stl", "Robot arm"),
            ("Robot arm part 12.stl", "Robot arm"),
            ("vase_version10.obj", "vase"),
            ("cube (1).stl", "cube"),
            ("cube.stl", "cube"),
            ("v2.stl", "v2"),
            ("plate2.stl", "plate2"),
            ("level2.stl", "level2"),
            ("Benchy.STL", "Benchy"),
        ] {
            assert_eq!(stem(name), want, "{name}");
        }
    }

    #[test]
    fn files_group_by_name_and_images_join_their_stem() {
        let g = grouped(&[
            "Bracket_plate1.3mf",
            "Bracket_plate2.3mf",
            "Bracket.stl",
            "gear-v2.stl",
            "gear (1).stl",
            "Bracket.jpg",
            "gear photo.png",
            "stray.jpg",
        ]);
        assert_eq!(
            g,
            [
                (
                    "Bracket".to_string(),
                    names(&["Bracket_plate1.3mf", "Bracket_plate2.3mf", "Bracket.stl", "Bracket.jpg"])
                ),
                (
                    "gear".to_string(),
                    names(&["gear-v2.stl", "gear (1).stl", "gear photo.png"])
                ),
            ],
            "a stray image with no model of its name is left out"
        );
    }

    #[test]
    fn files_in_folders_group_by_folder() {
        let g = grouped(&[
            "my models/Planter/planter_base.stl",
            "my models/Planter/planter_tray.stl",
            "my models/Planter/photo.jpg",
            "my models/Hook/hook.3mf",
            "my models/Hook/README.txt",
            "loose.stl",
        ]);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].0, "Planter");
        assert_eq!(g[0].1.len(), 3);
        assert_eq!(
            g[1],
            (
                "Hook".to_string(),
                names(&["my models/Hook/hook.3mf", "my models/Hook/README.txt"])
            )
        );
        assert_eq!(g[2].0, "loose");
    }

    #[test]
    fn one_model_takes_every_image() {
        let g = grouped(&["dragon.stl", "IMG_0001.jpg", "IMG_0002.jpg"]);
        assert_eq!(
            g,
            [(
                "dragon".to_string(),
                names(&["dragon.stl", "IMG_0001.jpg", "IMG_0002.jpg"])
            )]
        );
        assert!(group(&[]).is_empty());
        assert!(grouped(&["only.jpg"]).is_empty());
    }
}
