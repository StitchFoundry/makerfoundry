//! Adding a 3D model (MF-41): its files (STL, 3MF, OBJ, STEP and text companions) and images in
//! one upload, each file read and checked (`files`), pictures drawn for its meshes (`render`), the
//! images cleaned (`photos`), all of it stored and noted as one draft, seen only by its maker.
//! Limits (the owner's decisions, 2026-10-30): 100 MB a file, 300 MB in all, 30 files and 12
//! images a model, 2,000 models an account, 100 new models a day.

use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use sqlx::PgPool;

use super::files::{self, Role};
use super::mesh::MAX_FILE_BYTES;
use super::render::{self, View};
use crate::AppState;
use crate::accounts::{User, current_user, forbidden, private, same_origin};
use crate::designs::{DesignRow, Refused, photos_html};
use crate::pages::escape;
use crate::photos;
use crate::storage::{Storage, new_key};

/// Files a model may have.
pub const MAX_FILES: usize = 30;
/// Images (photos of prints, renders) a model may have.
pub const MAX_IMAGES: usize = 12;
/// All of a model's files and images together.
pub const MAX_MODEL_BYTES: usize = 300 * 1024 * 1024;
/// The upload route's cap: everything, and room for the form around it.
pub const BODY_LIMIT_BYTES: usize = MAX_MODEL_BYTES + 1024 * 1024;
/// The most 3D models an account may keep.
pub const MAX_MODELS: i64 = 2000;
/// The main picture's side, and the views', in pixels.
const HERO_SIDE: u32 = 800;
const VIEW_SIDE: u32 = 400;

fn not_found() -> Response {
    crate::designs::not_found()
}

fn upload_form(problem: &str) -> String {
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    };
    format!(
        r#"<section class="form">
<h1>Add a 3D model</h1>
<p class="lead">Upload the model's files and images of the print. It stays a draft, seen only by you, until you publish it.</p>
{problem}
<form method="post" action="/3d/new" enctype="multipart/form-data">
<label>Model files <small>(STL, 3MF, OBJ or STEP; also .mtl, .scad, .txt or .md beside them; up to {MAX_FILES} files, {file_mb} MB each)</small><input type="file" name="files" multiple required></label>
<label>Images of the print <small>(JPEG, PNG or WebP, up to {MAX_IMAGES})</small><input type="file" name="images" accept="image/jpeg,image/png,image/webp" multiple></label>
{showcase}
<button type="submit">Add the model</button>
</form>
<p class="muted">A 3MF is saved again with only what's needed to print it: its model, pictures and slicer settings. Sliced G-code and anything else in it is left out. Images are saved again without their hidden details (such as where they were taken).</p>
</section>"#,
        file_mb = MAX_FILE_BYTES / (1024 * 1024),
        showcase = super::showcase::CHECKBOX,
    )
}

/// The signed-in maker who may add models now (everyone once MakerFoundry 3D is on, admins
/// before), and the database; or the answer to give instead.
async fn uploader(state: &AppState, headers: &HeaderMap) -> Result<(User, PgPool), Response> {
    let viewer = current_user(state, headers).await;
    match (viewer, state.db.clone()) {
        (Some(u), Some(pool)) if super::open_to(state, Some(&u)).await => Ok((u, pool)),
        (None, _) if state.three_d => Err(Redirect::to("/signin?next=/3d/new").into_response()),
        _ => Err(not_found()),
    }
}

pub async fn new_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = uploader(&state, &headers).await {
        return r;
    }
    if state.storage.is_none() {
        return private(
            "Add a 3D model · MakerFoundry",
            r#"<section class="form"><h1>Add a 3D model</h1><p class="lead">Keeping models isn't switched on yet.</p></section>"#,
            true,
        );
    }
    private("Add a 3D model · MakerFoundry", &upload_form(""), true)
}

pub async fn create(State(state): State<AppState>, headers: HeaderMap, mut multipart: Multipart) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool) = match uploader(&state, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let again = |why: &str| private("Add a 3D model · MakerFoundry", &upload_form(why), true);
    let whole = || {
        format!(
            "The upload didn't come through whole. A model can be up to {} MB in all.",
            MAX_MODEL_BYTES / (1024 * 1024)
        )
    };
    let mut model_files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut images: Vec<Vec<u8>> = Vec::new();
    let mut showcase = false;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let part = field.name().unwrap_or_default().to_string();
                let name = clean_name(field.file_name().unwrap_or("model"));
                let Ok(bytes) = field.bytes().await else {
                    return again(&whole());
                };
                if part == "showcase" {
                    showcase = bytes.as_ref() == b"yes";
                    continue;
                }
                // (a form sent with nothing chosen has one empty part)
                if bytes.is_empty() {
                    continue;
                }
                match part.as_str() {
                    "files" => {
                        if model_files.len() >= MAX_FILES {
                            return again(&format!("A model can have up to {MAX_FILES} files."));
                        }
                        if bytes.len() > MAX_FILE_BYTES {
                            return again(&format!(
                                "{} is larger than {} MB.",
                                name,
                                MAX_FILE_BYTES / (1024 * 1024)
                            ));
                        }
                        model_files.push((name, bytes.to_vec()));
                    }
                    "images" => {
                        if images.len() >= MAX_IMAGES {
                            return again(&format!("A model can have up to {MAX_IMAGES} images."));
                        }
                        images.push(bytes.to_vec());
                    }
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(_) => return again(&whole()),
        }
    }
    match add(&state, &pool, user.account_id, model_files, images).await {
        Ok(id) => {
            if showcase {
                super::showcase::give(&pool, user.account_id, Some(id), None).await;
            }
            Redirect::to(&format!("/designs/{id}")).into_response()
        }
        Err(Refused::Unavailable) => Redirect::to("/3d/new").into_response(),
        Err(r) => again(&r.message()),
    }
}

/// A file's name as kept and offered again: without any folders (a browser sends none, but a
/// request can), control characters or surrounding spaces and dots, at most 200 characters.
pub fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = base.chars().filter(|c| !c.is_control()).take(200).collect();
    let clean = clean.trim().trim_start_matches('.').trim();
    if clean.is_empty() {
        "model".to_string()
    } else {
        clean.to_string()
    }
}

/// One file, read and checked, with its pictures drawn (the mesh itself isn't kept).
struct Prepared {
    name: String,
    format: &'static str,
    role: Role,
    bytes: Vec<u8>,
    triangles: Option<u64>,
    size_mm: Option<[f64; 3]>,
    hero: Option<(Vec<u8>, &'static str)>,
    views: Vec<Vec<u8>>,
    /// What the viewer draws (`viewer`), for meshes and projects.
    viewer: Option<Vec<u8>>,
    dropped: Vec<String>,
    profile: Option<super::profile::Profile>,
}

fn prepare(name: String, bytes: &[u8], preview: Option<Vec<u8>>) -> Result<Prepared, String> {
    let mut checked = files::check(&name, bytes)?;
    // a STEP file's preview (issue #32): the converter's STL, read like any upload; one that
    // can't be read leaves the file without a preview
    if checked.format == "step"
        && let Some(stl) = preview
    {
        checked.mesh = super::stl::read(&stl).ok();
    }
    let drawn = matches!(checked.role, Role::Mesh | Role::Project) || checked.format == "step";
    let viewer = match (&checked.mesh, drawn) {
        (Some(mesh), true) => Some(super::viewer::encode(mesh)),
        _ => None,
    };
    let (hero, views) = match (&checked.mesh, drawn) {
        (Some(mesh), true) => {
            let hero = render::hero(Some(mesh), &checked.pictures, HERO_SIDE);
            let views = [View::Front, View::Side, View::Top]
                .into_iter()
                .map(|v| render::render(mesh, v, VIEW_SIDE))
                .collect::<Result<Vec<_>, _>>();
            match views {
                Ok(views) => (hero, views),
                // (a preview that can't be drawn is no reason to refuse a STEP file)
                Err(_) if checked.format == "step" => (None, Vec::new()),
                Err(why) => return Err(why),
            }
        }
        _ => (None, Vec::new()),
    };
    Ok(Prepared {
        name,
        format: checked.format,
        role: checked.role,
        triangles: checked.mesh.as_ref().map(|m| m.total),
        size_mm: checked.mesh.as_ref().map(super::mesh::Mesh::size_mm),
        bytes: checked.bytes,
        hero,
        views,
        viewer,
        dropped: checked.dropped,
        profile: checked.profile,
    })
}

/// A new 3D draft for `account`: the limits checked, every file read and checked and its pictures
/// drawn, the images cleaned, all of it stored → the design's id. The upload form and the bulk
/// import (MF-52) add models this way.
pub async fn add(
    state: &AppState,
    pool: &PgPool,
    account: i64,
    model_files: Vec<(String, Vec<u8>)>,
    images: Vec<Vec<u8>>,
) -> Result<i64, Refused> {
    let Some(storage) = state.storage.as_ref() else {
        return Err(Refused::Unavailable);
    };
    if model_files.is_empty() {
        return Err(Refused::Bad("Choose the model's files to upload.".into()));
    }
    if model_files.len() > MAX_FILES {
        return Err(Refused::Bad(format!("A model can have up to {MAX_FILES} files.")));
    }
    if images.len() > MAX_IMAGES {
        return Err(Refused::Bad(format!("A model can have up to {MAX_IMAGES} images.")));
    }
    let total: usize = model_files
        .iter()
        .map(|(_, b)| b.len())
        .chain(images.iter().map(Vec::len))
        .sum();
    if total > MAX_MODEL_BYTES {
        return Err(Refused::Bad(format!(
            "A model can be up to {} MB in all.",
            MAX_MODEL_BYTES / (1024 * 1024)
        )));
    }
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_designs WHERE account_id = $1 AND kind = 'model3d'")
        .bind(account)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    if kept >= MAX_MODELS {
        return Err(Refused::TooMany(format!(
            "An account can keep up to {MAX_MODELS} models. Delete some drafts to add more."
        )));
    }
    if !state.limits.allow(crate::limits::MODEL_UPLOAD, &account.to_string()) {
        return Err(Refused::TooMany(
            "You've added a lot of models today. Please try again tomorrow.".into(),
        ));
    }
    scan_all(state, &model_files, &images).await?;
    // reading and drawing are CPU-heavy: off the threads that answer requests, one file at a time
    let mut prepared = Vec::with_capacity(model_files.len());
    for (name, bytes) in model_files {
        let name = clean_name(&name);
        let shown: String = name.chars().take(80).collect();
        let preview = step_preview(state, &name, &bytes).await;
        match tokio::task::spawn_blocking(move || prepare(name, &bytes, preview)).await {
            Ok(Ok(p)) => prepared.push(p),
            Ok(Err(why)) => return Err(Refused::Bad(format!("{shown}: {why}."))),
            Err(_) => return Err(Refused::Bad(format!("{shown} couldn't be read. Please try again."))),
        }
    }
    if !prepared.iter().any(|p| matches!(p.role, Role::Mesh | Role::Project)) {
        return Err(Refused::Bad(
            "A model needs at least one STL, 3MF or OBJ file to print from.".into(),
        ));
    }
    let mut clean = Vec::new();
    for (i, image) in images.into_iter().enumerate() {
        match tokio::task::spawn_blocking(move || photos::read(&image)).await {
            Ok(Ok(p)) => clean.push(p),
            Ok(Err(why)) => return Err(Refused::Bad(format!("Image {}: {why}.", i + 1))),
            Err(_) => return Err(Refused::Bad(format!("Image {} couldn't be read.", i + 1))),
        }
    }
    keep(storage, pool, account, prepared, clean).await.map_err(|why| {
        eprintln!("model3d: {why}");
        Refused::Failed
    })
}

/// A STEP file's preview as an STL from the converter service, when there is one (issue #32).
async fn step_preview(state: &AppState, name: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    let converter = state.step.as_ref()?;
    if files::kind_of(name).map(|(f, _)| f) != Some("step") {
        return None;
    }
    match converter.to_stl(bytes).await {
        Ok(stl) => Some(stl),
        Err(e) => {
            eprintln!("model3d: STEP preview: {e}");
            None
        }
    }
}

/// Scans every file and image with the virus scanner, when there is one (issue #29): a flagged
/// one is refused by name; if the scanner can't answer, the upload is held.
async fn scan_all(state: &AppState, files: &[(String, Vec<u8>)], images: &[Vec<u8>]) -> Result<(), Refused> {
    let Some(scanner) = &state.scanner else {
        return Ok(());
    };
    let named = files
        .iter()
        .map(|(n, b)| (n.clone(), b))
        .chain(images.iter().enumerate().map(|(i, b)| (format!("Image {}", i + 1), b)));
    for (name, bytes) in named {
        match scanner.scan(bytes).await {
            Ok(crate::scan::Verdict::Clean) => {}
            Ok(crate::scan::Verdict::Found(_)) => {
                return Err(Refused::Bad(format!(
                    "{}: our virus scanner flagged it, so it isn't kept.",
                    name.chars().take(80).collect::<String>()
                )));
            }
            Err(e) => {
                eprintln!("scan: {e}");
                return Err(Refused::Bad(
                    "Uploads can't be checked for viruses right now, so they're paused. Please try again in a few minutes.".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Stores the files, then notes the model; if noting fails, the stored files go again.
/// One file stored, ready to be noted.
struct StoredFile {
    name: String,
    format: &'static str,
    role: Role,
    key: String,
    bytes: i64,
    triangles: Option<i64>,
    size_mm: Option<Vec<f64>>,
    thumb: Option<(String, &'static str)>,
    views: Vec<String>,
    mesh: Option<String>,
    dropped: Vec<String>,
    profile: Option<super::profile::Profile>,
}

/// Stores prepared files and their pictures, noting every key stored in `stored`.
async fn store_files(
    storage: &Storage,
    prepared: Vec<Prepared>,
    stored: &mut Vec<String>,
) -> Result<Vec<StoredFile>, String> {
    let mut rows = Vec::new();
    for p in prepared {
        let key = new_key("model")?;
        let bytes = p.bytes.len() as i64;
        storage.put(&key, p.bytes, "application/octet-stream").await?;
        stored.push(key.clone());
        let thumb = match p.hero {
            Some((picture, kind)) => {
                let k = new_key("thumb")?;
                storage.put(&k, picture, kind).await?;
                stored.push(k.clone());
                Some((k, kind))
            }
            None => None,
        };
        let mut views = Vec::new();
        for v in p.views {
            let k = new_key("view")?;
            storage.put(&k, v, "image/png").await?;
            stored.push(k.clone());
            views.push(k);
        }
        let mesh = match p.viewer {
            Some(bytes) => {
                let k = new_key("mesh")?;
                storage.put(&k, bytes, "application/octet-stream").await?;
                stored.push(k.clone());
                Some(k)
            }
            None => None,
        };
        rows.push(StoredFile {
            mesh,
            name: p.name,
            format: p.format,
            role: p.role,
            key,
            bytes,
            triangles: p.triangles.map(|t| i64::try_from(t).unwrap_or(i64::MAX)),
            size_mm: p.size_mm.map(|s| s.to_vec()),
            thumb,
            views,
            dropped: p.dropped,
            profile: p.profile,
        });
    }
    Ok(rows)
}

/// Notes stored files as model `id`'s, from `start` on in its order, with their profiles.
async fn note_files(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
    rows: &[StoredFile],
    start: i32,
) -> Result<(), String> {
    for (n, r) in rows.iter().enumerate() {
        let file_id: i64 = sqlx::query_scalar(
            "INSERT INTO mf_model_files (design_id, role, format, file_name, file_key, file_bytes, triangles, size_mm, thumb_key, thumb_type, view_keys, dropped, position, mesh_key)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id",
        )
        .bind(id)
        .bind(r.role.as_str())
        .bind(r.format)
        .bind(&r.name)
        .bind(&r.key)
        .bind(r.bytes)
        .bind(r.triangles)
        .bind(&r.size_mm)
        .bind(r.thumb.as_ref().map(|t| t.0.clone()))
        .bind(r.thumb.as_ref().map(|t| t.1))
        .bind(&r.views)
        .bind(&r.dropped)
        .bind(start.saturating_add(i32::try_from(n).unwrap_or(i32::MAX)))
        .bind(&r.mesh)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
        if let Some(p) = &r.profile {
            save_profile(tx, id, file_id, p).await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Deletes stored keys, best effort (an orphaned file costs a little storage, nothing else).
async fn forget(storage: &Storage, keys: &[String]) {
    for key in keys {
        if let Err(e) = storage.delete(key).await {
            eprintln!("model3d: delete {key}: {e}");
        }
    }
}

/// Stores the files, then notes the model; if noting fails, the stored files go again.
async fn keep(
    storage: &Storage,
    pool: &PgPool,
    account: i64,
    prepared: Vec<Prepared>,
    clean: Vec<photos::Photo>,
) -> Result<i64, String> {
    let mut stored: Vec<String> = Vec::new();
    let result = async {
        let rows = store_files(storage, prepared, &mut stored).await?;
        let mut photo_keys = Vec::new();
        for p in clean {
            let (photo_key, thumb_key) = (new_key("photo")?, new_key("thumb")?);
            storage.put(&photo_key, p.jpeg, "image/jpeg").await?;
            stored.push(photo_key.clone());
            storage.put(&thumb_key, p.thumb, "image/jpeg").await?;
            stored.push(thumb_key.clone());
            photo_keys.push((photo_key, thumb_key, p.width, p.height));
        }
        // the design's own file is its first mesh or project: what its listing and lists name
        let main = rows
            .iter()
            .find(|r| matches!(r.role, Role::Mesh | Role::Project))
            .ok_or("no model file")?;
        let size = main.size_mm.clone().unwrap_or_default();
        let facts = serde_json::json!({
            "format": main.format,
            "files": rows.len(),
            "triangles": main.triangles,
            "size_mm": size,
        });
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO mf_designs (account_id, kind, file_name, file_key, file_bytes, format, facts, preview_svg, width_mm, height_mm)
             VALUES ($1, 'model3d', $2, $3, $4, $5, $6, '', $7, $8) RETURNING id",
        )
        .bind(account)
        .bind(&main.name)
        .bind(&main.key)
        .bind(main.bytes)
        .bind(main.format)
        .bind(facts)
        .bind(size.first().copied())
        .bind(size.get(1).copied())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        note_files(&mut tx, id, &rows, 0).await?;
        super::search::refresh_facets(&mut *tx, id).await.map_err(|e| e.to_string())?;
        for (position, (photo_key, thumb_key, w, h)) in photo_keys.iter().enumerate() {
            sqlx::query("INSERT INTO mf_design_photos (design_id, photo_key, thumb_key, width, height, position) VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(id)
                .bind(photo_key)
                .bind(thumb_key)
                .bind(i32::try_from(*w).unwrap_or(i32::MAX))
                .bind(i32::try_from(*h).unwrap_or(i32::MAX))
                .bind(i32::try_from(position).unwrap_or(i32::MAX))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(id)
    }
    .await;
    if result.is_err() {
        forget(storage, &stored).await;
    }
    result
}

/// Notes a project file's print profile.
async fn save_profile(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    design: i64,
    file: i64,
    p: &super::profile::Profile,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO mf_print_profiles (design_id, file_id, name, slicer, printer, printers, nozzle_mm, layer_mm, infill_percent, walls, supports, plates, filaments, readable)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(design)
    .bind(file)
    .bind(p.name())
    .bind(p.slicer)
    .bind(&p.printer)
    .bind(p.printers.iter().map(|b| b.to_string()).collect::<Vec<_>>())
    .bind(p.nozzle_mm)
    .bind(p.layer_mm)
    .bind(p.infill_percent)
    .bind(p.walls)
    .bind(p.supports)
    .bind(p.plates)
    .bind(serde_json::to_value(&p.filaments).unwrap_or_default())
    .bind(p.readable)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

/// Every stored key of these designs' model files and their pictures (for deleting them).
pub async fn keys(pool: &PgPool, designs: &[i64]) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT zip_key FROM mf_designs WHERE id = ANY($1) AND zip_key IS NOT NULL
         UNION ALL SELECT file_key FROM mf_model_files WHERE design_id = ANY($1)
         UNION ALL SELECT thumb_key FROM mf_model_files WHERE design_id = ANY($1) AND thumb_key IS NOT NULL
         UNION ALL SELECT unnest(view_keys) FROM mf_model_files WHERE design_id = ANY($1)
         UNION ALL SELECT mesh_key FROM mf_model_files WHERE design_id = ANY($1) AND mesh_key IS NOT NULL",
    )
    .bind(designs)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// One of a model's files, as kept.
pub struct ModelFile {
    pub id: i64,
    pub role: String,
    pub format: String,
    pub file_name: String,
    pub file_bytes: i64,
    pub triangles: Option<i64>,
    pub size_mm: Option<Vec<f64>>,
    pub has_thumb: bool,
    pub views: usize,
    pub dropped: Vec<String>,
}

type FileRow = (
    i64,
    String,
    String,
    String,
    i64,
    Option<i64>,
    Option<Vec<f64>>,
    bool,
    i32,
    Vec<String>,
);

pub async fn model_files(pool: &PgPool, design: i64) -> Vec<ModelFile> {
    let rows: Vec<FileRow> = sqlx::query_as(
        "SELECT id, role, format, file_name, file_bytes, triangles, size_mm, thumb_key IS NOT NULL,
                coalesce(cardinality(view_keys), 0), dropped
         FROM mf_model_files WHERE design_id = $1 ORDER BY position",
    )
    .bind(design)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(
            |(id, role, format, file_name, file_bytes, triangles, size_mm, has_thumb, views, dropped)| ModelFile {
                id,
                role,
                format,
                file_name,
                file_bytes,
                triangles,
                size_mm,
                has_thumb,
                views: usize::try_from(views).unwrap_or(0),
                dropped,
            },
        )
        .collect()
}

/// A length in millimetres as shown: whole when it is, else to a tenth.
fn mm(v: f64) -> String {
    if (v - v.round()).abs() < 0.05 {
        format!("{:.0}", v)
    } else {
        format!("{v:.1}")
    }
}

/// A size in bytes as shown.
fn bytes(n: i64) -> String {
    match n {
        n if n >= 1024 * 1024 => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
        n if n >= 1024 => format!("{} KB", n / 1024),
        n => format!("{n} bytes"),
    }
}

/// A model's files, each with its picture and what reading found.
pub fn files_html(id: i64, list: &[ModelFile], owner: bool) -> String {
    let items: String = list
        .iter()
        .map(|f| {
            let picture = if f.has_thumb {
                format!(
                    r#"<img src="/3d/{id}/files/{fid}/hero" alt="" width="160" height="160" loading="lazy">"#,
                    fid = f.id
                )
            } else {
                format!(r#"<span class="file-kind">{}</span>"#, escape(&f.format.to_uppercase()))
            };
            let what = match f.role.as_str() {
                "mesh" => "mesh",
                "project" => "slicer project",
                "source" => "CAD source",
                _ => "notes",
            };
            let mut facts = vec![format!("{} {what}", escape(&f.format.to_uppercase()))];
            if let Some([x, y, z]) = f.size_mm.as_deref().and_then(|s| <[f64; 3]>::try_from(s).ok()) {
                facts.push(format!("{} × {} × {} mm", mm(x), mm(y), mm(z)));
            }
            if let Some(t) = f.triangles {
                facts.push(format!("{t} triangles"));
            }
            if f.role == "source" && f.has_thumb {
                facts.push("preview drawn from the STEP file".into());
            }
            facts.push(bytes(f.file_bytes));
            let views: String = ["front", "side", "top"]
                .iter()
                .take(f.views)
                .map(|v| {
                    format!(
                        r#"<img src="/3d/{id}/files/{fid}/{v}" alt="{v} view" width="80" height="80" loading="lazy">"#,
                        fid = f.id
                    )
                })
                .collect();
            let views = if views.is_empty() { String::new() } else { format!(r#"<span class="views">{views}</span>"#) };
            let remove = if owner {
                format!(
                    r#"<form method="post" action="/3d/{id}/files/{fid}/delete"><button type="submit" class="secondary small">Remove</button></form>"#,
                    fid = f.id
                )
            } else {
                String::new()
            };
            let dropped = if owner && !f.dropped.is_empty() {
                format!(
                    r#"<span class="muted">Left out of the download (not needed to print, or not safe to pass on): {}</span>"#,
                    escape(&f.dropped.join(", "))
                )
            } else {
                String::new()
            };
            format!(
                r#"<li>{picture}<span class="about"><strong>{name}</strong><span class="muted">{facts}</span>{views}{dropped}{remove}</span></li>"#,
                name = escape(&f.file_name),
                facts = facts.join(" · "),
            )
        })
        .collect();
    format!(r#"<h2>Files</h2><ul class="model-files">{items}</ul>"#)
}

/// A maker's own page for a 3D model: its listing and publishing, its files and images, and
/// deleting a draft.
pub async fn manage_page(
    state: &AppState,
    pool: &PgPool,
    user: &User,
    id: i64,
    d: &DesignRow,
    problem: &str,
) -> Response {
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    };
    let title = if d.title.is_empty() { &d.file_name } else { &d.title };
    let files = files_html(id, &model_files(pool, id).await, true);
    let photos = photos_html(pool, id).await;
    let delete = if d.status == "published" {
        String::new()
    } else {
        format!(
            r#"<form method="post" action="/designs/{id}/delete"><button type="submit" class="secondary">Delete this draft</button></form>"#
        )
    };
    let has_profile = crate::makers::profile(pool, user.account_id).await.is_some();
    let listing = crate::listings::manage_html(d, id, has_profile, state.launched);
    let profiles = profiles_html(id, &profiles(pool, id).await, true);
    let remix = super::remix::manage_section(pool, id).await;
    let add = format!(
        r#"<form method="post" action="/3d/{id}/files" enctype="multipart/form-data" class="form">
<label>Add files <small>(another print profile as a 3MF, a STEP source, more parts…)</small><input type="file" name="files" multiple required></label>
<button type="submit" class="secondary">Add them</button>
</form>"#
    );
    let note = if d.status == "draft" {
        r#"<p class="lead">A draft, seen only by you.</p>"#
    } else {
        ""
    };
    private(
        &format!("{} · MakerFoundry", escape(title)),
        &format!(
            r#"<section class="design"><h1>{title}</h1>{problem}{note}{listing}{remix}{profiles}{files}{add}{photos}{delete}<p><a href="/designs">Your designs</a></p></section>"#,
            title = escape(title),
        ),
        true,
    )
}

/// A model file's role, stored key, picture, views and viewer mesh.
type FileKeys = (String, String, Option<String>, Vec<String>, Option<String>);

/// A model file's stored pictures: main picture and its type, views, and the viewer's mesh.
type Pictures = (Option<String>, Option<String>, Vec<String>, Option<String>);

/// A model file's picture (`hero`) or view (`front`, `side`, `top`): anyone's once the model is
/// published, only its maker's (and admins') before.
pub async fn picture(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, file, view)): Path<(i64, i64, String)>,
) -> Response {
    let viewer = current_user(&state, &headers).await;
    let Some(pool) = state.db.as_ref() else {
        return not_found();
    };
    if !super::open_to(&state, viewer.as_ref()).await {
        return not_found();
    }
    let d = match crate::designs::design(pool, id).await {
        Ok(Some(d)) if d.kind == "model3d" && crate::listings::visible(&d, viewer.as_ref(), &state.admins) => d,
        _ => return not_found(),
    };
    let row: Option<Pictures> = sqlx::query_as(
        "SELECT thumb_key, thumb_type, view_keys, mesh_key FROM mf_model_files WHERE id = $1 AND design_id = $2",
    )
    .bind(file)
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((thumb, kind, views, mesh)) = row else {
        return not_found();
    };
    let found = match view.as_str() {
        "hero" => thumb.zip(kind),
        "mesh" => mesh.map(|k| (k, "application/octet-stream".to_string())),
        v => View::ALL
            .iter()
            .skip(1)
            .position(|w| w.name() == v)
            .and_then(|i| views.get(i).cloned())
            .map(|k| (k, "image/png".to_string())),
    };
    let (Some((key, kind)), Some(storage)) = (found, state.storage.as_ref()) else {
        return not_found();
    };
    // a paid model's mesh: the whole of it only for the people who may download the model, a
    // rough one for everyone else (`viewer::coarse`; the owner's decision, 2026-11-01)
    let rough = view == "mesh" && !crate::listings::access(&state, pool, viewer.as_ref(), &d, id).await.1;
    let cache = if d.status == "published" && (view != "mesh" || d.price_cents == 0) {
        "public, max-age=3600"
    } else {
        // (what a paid model's mesh is depends on who asks: never kept for others)
        "private, max-age=3600"
    };
    // (only the two picture types are ever stored)
    let kind = match kind.as_str() {
        "image/jpeg" => "image/jpeg",
        "application/octet-stream" => "application/octet-stream",
        _ => "image/png",
    };
    match storage.get(&key).await {
        Ok(Some(bytes)) if rough => match super::viewer::coarse(&bytes) {
            Some(bytes) => ([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)], bytes).into_response(),
            None => not_found(),
        },
        Ok(Some(bytes)) => ([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)], bytes).into_response(),
        Ok(None) => not_found(),
        Err(e) => {
            eprintln!("model3d: picture {file}: {e}");
            (StatusCode::BAD_GATEWAY, "The picture can't be fetched just now.").into_response()
        }
    }
}

/// A model's files on its public page, each with a download link when `may_download`, and the
/// whole model as a ZIP.
pub fn public_files_html(id: i64, list: &[ModelFile], may_download: bool) -> String {
    let mut html = files_html(id, list, false);
    if may_download {
        let links: String = list
            .iter()
            .map(|f| {
                format!(
                    r#"<li><a href="/d/{id}/download?model_file={fid}" rel="nofollow">{name}</a></li>"#,
                    fid = f.id,
                    name = escape(&f.file_name)
                )
            })
            .collect();
        html.push_str(&format!(
            r#"<p class="actions"><a class="button" href="/d/{id}/download" rel="nofollow">Download all (ZIP)</a></p><ul class="downloads">{links}</ul>"#
        ));
    }
    html
}

/// The first of a model's files with a picture: the model's main picture.
pub async fn first_picture(pool: &PgPool, id: i64) -> Option<i64> {
    sqlx::query_scalar(
        "SELECT id FROM mf_model_files WHERE design_id = $1 AND thumb_key IS NOT NULL ORDER BY position LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// A model's public page: its title, main picture, `intro` and `footer` (HTML).
pub async fn public_section(pool: &PgPool, id: i64, title: &str, intro: &str, footer: &str) -> Option<String> {
    let picture = match first_picture(pool, id).await {
        Some(f) => format!(
            r#"<p class="model-picture"><img src="/3d/{id}/files/{f}/hero" alt="{alt}" width="800" height="800"></p>"#,
            alt = escape(title)
        ),
        None => String::new(),
    };
    // the viewer (MF-46): hidden until its script has the mesh drawn; the picture stays without it
    let meshed: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM mf_model_files WHERE design_id = $1 AND mesh_key IS NOT NULL ORDER BY position LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let viewer = match meshed {
        Some(f) => format!(
            r#"<div class="viewer" hidden data-mesh="/3d/{id}/files/{f}/mesh"><canvas width="800" height="800" aria-label="A 3D view of {alt}: drag to turn it, scroll or pinch to zoom"></canvas></div><script src="/static/viewer.js" defer></script>"#,
            alt = escape(title)
        ),
        None => String::new(),
    };
    Some(format!(
        r#"<section class="design"><h1>{title}</h1>{intro}{viewer}{picture}{footer}</section>"#,
        title = escape(title)
    ))
}

/// One of model `id`'s files → its key and the name to save it as.
pub async fn model_file_key(pool: &PgPool, id: i64, file: i64) -> Option<(String, String)> {
    sqlx::query_as("SELECT file_key, file_name FROM mf_model_files WHERE id = $1 AND design_id = $2")
        .bind(file)
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// The key of model `id`'s files as one ZIP: made from its stored files the first time it's
/// asked for, then kept.
pub async fn zip_for(storage: &Storage, pool: &PgPool, id: i64, d: &DesignRow) -> Result<String, String> {
    if let Some(key) = &d.zip_key {
        return Ok(key.clone());
    }
    let list: Vec<(String, String)> =
        sqlx::query_as("SELECT file_name, file_key FROM mf_model_files WHERE design_id = $1 ORDER BY position")
            .bind(id)
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(list.len());
    for (name, key) in list {
        let bytes = storage.get(&key).await?.ok_or_else(|| format!("{key} is missing"))?;
        // two files of the same name: the later ones are numbered
        let name = clean_name(&name);
        let mut unique = name.clone();
        let mut n = 2;
        while entries.iter().any(|(e, _)| e.eq_ignore_ascii_case(&unique)) {
            unique = match name.rsplit_once('.') {
                Some((stem, ext)) => format!("{stem} ({n}).{ext}"),
                None => format!("{name} ({n})"),
            };
            n += 1;
        }
        entries.push((unique, bytes));
    }
    let zip = tokio::task::spawn_blocking(move || super::zip::write(&entries))
        .await
        .map_err(|e| e.to_string())??;
    let key = new_key("zip")?;
    storage.put(&key, zip, "application/zip").await?;
    // two first downloads at once: the first one noted wins, the other's ZIP goes again
    let noted: Option<String> =
        sqlx::query_scalar("UPDATE mf_designs SET zip_key = $2 WHERE id = $1 AND zip_key IS NULL RETURNING zip_key")
            .bind(id)
            .bind(&key)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
    if noted.is_some() {
        return Ok(key);
    }
    let _ = storage.delete(&key).await;
    let kept: Option<String> = sqlx::query_scalar("SELECT zip_key FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    kept.ok_or_else(|| "no ZIP".to_string())
}

// ---------------------------------------------------------------- print profiles (MF-45)

/// A print profile as a model's page shows it.
pub struct ProfileView {
    pub file_id: i64,
    pub file_name: String,
    pub name: String,
    pub slicer: String,
    pub printers: Vec<String>,
    pub nozzle_mm: Option<f64>,
    pub layer_mm: Option<f64>,
    pub infill_percent: Option<f64>,
    pub walls: Option<i32>,
    pub supports: Option<bool>,
    pub plates: Option<i32>,
    pub filaments: serde_json::Value,
    pub readable: bool,
}

pub async fn profiles(pool: &PgPool, design: i64) -> Vec<ProfileView> {
    let rows = sqlx::query(
        "SELECT p.file_id, f.file_name, p.name, p.slicer, p.printers, p.nozzle_mm, p.layer_mm, p.infill_percent, p.walls,
                p.supports, p.plates, p.filaments, p.readable
         FROM mf_print_profiles p JOIN mf_model_files f ON f.id = p.file_id
         WHERE p.design_id = $1 ORDER BY f.position",
    )
    .bind(design)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    use sqlx::Row;
    rows.iter()
        .filter_map(|r| {
            Some(ProfileView {
                file_id: r.try_get("file_id").ok()?,
                file_name: r.try_get("file_name").ok()?,
                name: r.try_get("name").ok()?,
                slicer: r.try_get("slicer").ok()?,
                printers: r.try_get("printers").ok()?,
                nozzle_mm: r.try_get("nozzle_mm").ok()?,
                layer_mm: r.try_get("layer_mm").ok()?,
                infill_percent: r.try_get("infill_percent").ok()?,
                walls: r.try_get("walls").ok()?,
                supports: r.try_get("supports").ok()?,
                plates: r.try_get("plates").ok()?,
                filaments: r.try_get("filaments").ok()?,
                readable: r.try_get("readable").ok()?,
            })
        })
        .collect()
}

/// A slicer's name as shown.
fn slicer_name(id: &str) -> &'static str {
    match id {
        "bambu" => "Bambu Studio",
        "orca" => "OrcaSlicer",
        "prusa" => "PrusaSlicer",
        "cura" => "Cura",
        _ => "a slicer",
    }
}

/// A filament's colour as a small round swatch: an SVG in a `data:` address (pages allow no
/// inline styles), only for a checked `#RRGGBB`.
fn swatch(kind: &str, colour: &str) -> String {
    let hex = colour
        .strip_prefix('#')
        .filter(|h| h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()));
    let Some(hex) = hex else {
        return format!(r#"<span class="filament">{}</span>"#, escape(kind));
    };
    format!(
        r#"<span class="filament"><img src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 10 10'%3E%3Ccircle cx='5' cy='5' r='4.5' fill='%23{hex}' stroke='%23888888'/%3E%3C/svg%3E" alt="" width="14" height="14"> {kind} <span class="muted">#{hex}</span></span>"#,
        kind = escape(kind),
    )
}

/// A model's print profiles, each with its printer badges, settings, filaments and a download of
/// its own 3MF when `may_download`.
pub fn profiles_html(id: i64, list: &[ProfileView], may_download: bool) -> String {
    if list.is_empty() {
        return String::new();
    }
    let items: String = list
        .iter()
        .map(|p| {
            let badges: String = p
                .printers
                .iter()
                .map(|b| super::profile::badge_name(b))
                .filter(|n| !n.is_empty())
                .map(|n| format!(r#"<span class="badge printer">{}</span> "#, escape(n)))
                .collect();
            let mut facts = Vec::new();
            if let Some(n) = p.nozzle_mm {
                facts.push(format!("{} mm nozzle", super::profile::trim_number(n)));
            }
            if let Some(l) = p.layer_mm {
                facts.push(format!("{} mm layers", super::profile::trim_number(l)));
            }
            if let Some(i) = p.infill_percent {
                facts.push(format!("{}% infill", super::profile::trim_number(i)));
            }
            if let Some(w) = p.walls {
                facts.push(format!("{w} wall{}", if w == 1 { "" } else { "s" }));
            }
            if let Some(s) = p.supports {
                facts.push(if s { "supports" } else { "no supports" }.to_string());
            }
            if let Some(n) = p.plates {
                facts.push(format!("{n} plate{}", if n == 1 { "" } else { "s" }));
            }
            let filaments: String = p
                .filaments
                .as_array()
                .map(|list| {
                    list.iter()
                        .take(32)
                        .map(|f| {
                            swatch(
                                f.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                                f.get("colour").and_then(|v| v.as_str()).unwrap_or(""),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let settings = if p.readable {
                format!(r#"<span class="muted">{}</span>"#, facts.join(" · "))
            } else {
                r#"<span class="muted">This project's settings couldn't be read; the file still has them.</span>"#.to_string()
            };
            let download = if may_download {
                format!(
                    r#" <a href="/d/{id}/download?model_file={fid}" rel="nofollow">Download this 3MF</a>"#,
                    fid = p.file_id
                )
            } else {
                String::new()
            };
            format!(
                r#"<li><strong>{name}</strong> {badges}<span class="muted">from {slicer}, in {file}</span>{settings}<span class="filaments">{filaments}</span>{download}</li>"#,
                name = escape(&p.name),
                slicer = slicer_name(&p.slicer),
                file = escape(&p.file_name),
            )
        })
        .collect();
    format!(r#"<h2>Print profiles</h2><ul class="profiles">{items}</ul>"#)
}

// ---------------------------------------------------------------- adding and removing files

/// The maker's own model `id`, its database and the signed-in maker; or the answer instead.
pub(crate) async fn own_model(
    state: &AppState,
    headers: &HeaderMap,
    id: i64,
) -> Result<(User, PgPool, DesignRow), Response> {
    let (user, pool) = uploader(state, headers).await?;
    match crate::designs::design(&pool, id).await {
        Ok(Some(d)) if d.account_id == user.account_id && d.kind == "model3d" => Ok((user, pool, d)),
        _ => Err(not_found()),
    }
}

/// Forgets a model's ZIP after its files change (the next download makes it again).
async fn drop_zip(storage: &Storage, pool: &PgPool, id: i64) {
    let old: Option<Option<String>> = sqlx::query_scalar(
        "UPDATE mf_designs d SET zip_key = NULL FROM (SELECT zip_key FROM mf_designs WHERE id = $1 FOR UPDATE) old
         WHERE d.id = $1 RETURNING old.zip_key",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some(Some(key)) = old {
        forget(storage, &[key]).await;
    }
}

/// `POST /3d/<id>/files`: more files for a model (another print profile, a STEP source…), read
/// and checked as an upload's are.
pub async fn add_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool, _) = match own_model(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let Some(storage) = state.storage.as_ref() else {
        return not_found();
    };
    let again = |why: String| {
        let (state, pool, user) = (&state, &pool, &user);
        async move { crate::designs::manage_page(state, pool, user, id, &why).await }
    };
    let (count, kept_bytes, _) = match files_kept(&pool, id).await {
        Ok(k) => k,
        Err(e) => {
            eprintln!("model3d: files of {id}: {e}");
            return again("The files couldn't be kept just now. Please try again in a few minutes.".into()).await;
        }
    };
    let room = MAX_FILES.saturating_sub(usize::try_from(count).unwrap_or(MAX_FILES));
    let mut new_files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut new_bytes = 0usize;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = clean_name(field.file_name().unwrap_or("model"));
                let part = field.name().unwrap_or_default().to_string();
                let Ok(bytes) = field.bytes().await else {
                    return again("The files didn't come through whole. Please try again.".into()).await;
                };
                if part != "files" || bytes.is_empty() {
                    continue;
                }
                if new_files.len() >= room {
                    return again(format!("A model can have up to {MAX_FILES} files.")).await;
                }
                if bytes.len() > MAX_FILE_BYTES {
                    return again(format!("{name} is larger than {} MB.", MAX_FILE_BYTES / (1024 * 1024))).await;
                }
                new_bytes = new_bytes.saturating_add(bytes.len());
                if !fits(kept_bytes, new_bytes) {
                    return again(too_big()).await;
                }
                new_files.push((name, bytes.to_vec()));
            }
            Ok(None) => break,
            Err(_) => return again("The files didn't come through whole. Please try again.".into()).await,
        }
    }
    if new_files.is_empty() {
        return again("Choose the files to add.".into()).await;
    }
    if !state
        .limits
        .allow(crate::limits::MODEL_UPLOAD, &user.account_id.to_string())
    {
        return again("You've added a lot of models today. Please try again tomorrow.".into()).await;
    }
    if let Err(r) = scan_all(&state, &new_files, &[]).await {
        return again(r.message()).await;
    }
    let mut prepared = Vec::new();
    for (name, bytes) in new_files {
        let shown: String = name.chars().take(80).collect();
        let preview = step_preview(&state, &name, &bytes).await;
        match tokio::task::spawn_blocking(move || prepare(name, &bytes, preview)).await {
            Ok(Ok(p)) => prepared.push(p),
            Ok(Err(why)) => return again(format!("{shown}: {why}.")).await,
            Err(_) => return again(format!("{shown} couldn't be read. Please try again.")).await,
        }
    }
    let mut stored = Vec::new();
    let result = async {
        let rows = store_files(storage, prepared, &mut stored).await?;
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        // counted again under a per-model lock, so adds sent at once can't pass the caps (#170)
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("mf-model-files-{id}"))
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        let (count, kept_bytes, next) = files_kept(&mut *tx, id).await.map_err(|e| e.to_string())?;
        if usize::try_from(count).unwrap_or(MAX_FILES).saturating_add(rows.len()) > MAX_FILES {
            return Ok(Some(format!("A model can have up to {MAX_FILES} files.")));
        }
        if !fits(kept_bytes, new_bytes) {
            return Ok(Some(too_big()));
        }
        note_files(&mut tx, id, &rows, next).await?;
        super::search::refresh_facets(&mut *tx, id)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok::<_, String>(None)
    }
    .await;
    match result {
        Ok(None) => {}
        Ok(Some(why)) => {
            forget(storage, &stored).await;
            return again(why).await;
        }
        Err(e) => {
            eprintln!("model3d: add files to {id}: {e}");
            forget(storage, &stored).await;
            return again("The files couldn't be kept just now. Please try again in a few minutes.".into()).await;
        }
    }
    drop_zip(storage, &pool, id).await;
    Redirect::to(&format!("/designs/{id}")).into_response()
}

/// A model's files: how many, their bytes in all, and the next position.
async fn files_kept<'e>(db: impl sqlx::PgExecutor<'e>, id: i64) -> Result<(i64, i64, i32), sqlx::Error> {
    sqlx::query_as(
        "SELECT count(*), coalesce(sum(file_bytes), 0)::bigint, coalesce(max(position) + 1, 0) FROM mf_model_files WHERE design_id = $1",
    )
    .bind(id)
    .fetch_one(db)
    .await
}

/// Whether `new` more bytes fit beside a model's `kept` ones within `MAX_MODEL_BYTES` (#170).
fn fits(kept: i64, new: usize) -> bool {
    usize::try_from(kept.max(0)).is_ok_and(|kept| kept.saturating_add(new) <= MAX_MODEL_BYTES)
}

fn too_big() -> String {
    format!("A model can be up to {} MB in all.", MAX_MODEL_BYTES / (1024 * 1024))
}

/// `POST /3d/<id>/files/<file>/delete`: removes one of a model's files, with its pictures and
/// profile; never its last STL, 3MF or OBJ, nor any file of a model people have bought.
pub async fn remove_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, file)): Path<(i64, i64)>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool, d) = match own_model(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let Some(storage) = state.storage.as_ref() else {
        return not_found();
    };
    let row: Option<FileKeys> = sqlx::query_as(
        "SELECT role, file_key, thumb_key, view_keys, mesh_key FROM mf_model_files WHERE id = $1 AND design_id = $2",
    )
    .bind(file)
    .bind(id)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten();
    let Some((role, key, thumb, views, mesh)) = row else {
        return not_found();
    };
    let printable: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mf_model_files WHERE design_id = $1 AND role IN ('mesh', 'project')")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
    let problem = if crate::selling::has_sales(&pool, id).await {
        Some("People have bought this model, so its files are kept for their downloads.")
    } else if matches!(role.as_str(), "mesh" | "project") && printable <= 1 {
        Some("A model needs at least one STL, 3MF or OBJ file to print from.")
    } else {
        None
    };
    if let Some(p) = problem {
        return crate::designs::manage_page(&state, &pool, &user, id, p).await;
    }
    if let Err(e) = sqlx::query("DELETE FROM mf_model_files WHERE id = $1 AND design_id = $2")
        .bind(file)
        .bind(id)
        .execute(&pool)
        .await
    {
        eprintln!("model3d: remove {file}: {e}");
        return crate::designs::manage_page(&state, &pool, &user, id, "The file couldn't be removed just now.").await;
    }
    // the model's own file moves to the next printable one
    if d.file_key == key {
        let _ = sqlx::query(
            "UPDATE mf_designs d SET file_name = f.file_name, file_key = f.file_key, file_bytes = f.file_bytes, format = f.format,
                    width_mm = f.size_mm[1], height_mm = f.size_mm[2]
             FROM (SELECT * FROM mf_model_files WHERE design_id = $1 AND role IN ('mesh', 'project') ORDER BY position LIMIT 1) f
             WHERE d.id = $1",
        )
        .bind(id)
        .execute(&pool)
        .await;
    }
    if let Err(e) = super::search::refresh_facets(&pool, id).await {
        eprintln!("model3d: facets {id}: {e}");
    }
    let mut keys = vec![key];
    keys.extend(thumb);
    keys.extend(views);
    keys.extend(mesh);
    forget(storage, &keys).await;
    drop_zip(storage, &pool, id).await;
    Redirect::to(&format!("/designs/{id}")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_lose_their_folders() {
        assert_eq!(clean_name("../../etc/evil.stl"), "evil.stl");
        assert_eq!(clean_name("C:\\Users\\me\\part.stl"), "part.stl");
        assert_eq!(clean_name("  cube project.3mf "), "cube project.3mf");
        assert_eq!(clean_name("a\u{0}b.stl"), "ab.stl");
        assert_eq!(clean_name("..."), "model");
        assert_eq!(clean_name("/"), "model");
        assert_eq!(clean_name(".hidden.stl"), "hidden.stl");
        assert_eq!(clean_name(&"x".repeat(300)).len(), 200);
    }
}
