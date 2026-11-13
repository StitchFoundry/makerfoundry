//! The apps' API (MF-7): StitchFoundry and LightFoundry send a design to MakerFoundry with
//! **Publish to MakerFoundry**. The app is already signed in to the person's Foundry Account, so it
//! sends that sign-in (the account service's session token) as a bearer token; MakerFoundry checks
//! it with the account service, uses it for this one request, and never keeps it.
//!
//! The design arrives as a draft, read and checked like an upload from the site; the app then
//! opens its page (`url`) in the browser, where the maker adds the details and publishes it. The
//! contract the apps follow is `docs/api.md`.
//!
//! Browsers may call it from the web apps' own sites (`MF_API_ORIGINS`), without cookies.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::designs::{self, MAX_PHOTOS, Refused};
use crate::{db, limits};

/// The most a request to the API may be: an upload's limit, as base64, and room for the rest.
pub const API_LIMIT_BYTES: usize = designs::MAX_UPLOAD_BYTES / 3 * 4 + 64 * 1024;

/// The web apps that may call the API from a browser, unless `MF_API_ORIGINS` says otherwise:
/// StitchFoundry's site, and its web app's (GitHub Pages).
pub const DEFAULT_ORIGINS: &str = "https://stitchfoundry.app,https://stitchfoundry.github.io";

/// StitchFoundry's web app, which opens a design from `?open=<address>` (Open in StitchFoundry).
pub const STITCHFOUNDRY_WEB: &str = "https://stitchfoundry.github.io/StitchFoundry/";
/// LaserFoundry's web app, which opens SVG and DXF laser designs from `?open=` (since its v0.2.0).
pub const LASERFOUNDRY_WEB: &str = "https://stitchfoundry.github.io/laserfoundry/";
/// Whether LaserFoundry's web app opens a design from an address: on since 2026-10-31 (#46), when
/// LaserFoundry v0.2.0 was live and opened a MakerFoundry address in a real browser.
pub const LASERFOUNDRY_OPENS_LINKS: bool = true;

/// ScreenFoundry's web app (screen-printing prepress, MF-87), which opens SVG designs from
/// `?open=` the same way.
pub const SCREENFOUNDRY_WEB: &str = "https://app.screenfoundry.app/";
/// Whether ScreenFoundry's web app opens a design from an address: on since 2026-11-01, when its
/// web app at app.screenfoundry.app (screenfoundry#28) read `?open=`, fetched makerfoundry.app (its
/// site allowed in `MF_API_ORIGINS`) and showed MakerFoundry's own sentence for one that isn't there.
pub const SCREENFOUNDRY_OPENS_LINKS: bool = true;

/// PatternFoundry's web app (sewing patterns, StitchFoundry family; MF-89, issue #47), which opens SVG
/// designs from `?open=` the same way. Until MakerFoundry has a sewing-pattern kind, patterns come
/// as SVG vector designs.
pub const PATTERNFOUNDRY_WEB: &str = "https://stitchfoundry.github.io/patternfoundry/";
/// Whether PatternFoundry's web app opens a design from an address: on since 2026-11-01, when its
/// live web app read `?open=`, fetched makerfoundry.app (its site allowed in `MF_API_ORIGINS`),
/// opened a served SVG and showed MakerFoundry's own sentence for one that isn't there.
pub const PATTERNFOUNDRY_OPENS_LINKS: bool = true;

/// Which web apps open links yet (the switches above).
#[derive(Clone, Copy, Debug)]
pub struct Switches {
    pub laser: bool,
    pub screen: bool,
    pub pattern: bool,
}

/// The switches as they are.
pub const SWITCHES: Switches = Switches {
    laser: LASERFOUNDRY_OPENS_LINKS,
    screen: SCREENFOUNDRY_OPENS_LINKS,
    pattern: PATTERNFOUNDRY_OPENS_LINKS,
};

/// The web apps that open design `d`, by its kind and format: (address, name), in the order their
/// buttons show. Embroidery opens in StitchFoundry; an SVG or DXF laser design in LaserFoundry, and
/// an SVG one in ScreenFoundry and PatternFoundry, each once its web app opens links.
pub fn web_apps(d: &designs::DesignRow) -> Vec<(&'static str, &'static str)> {
    let format = d.facts.get("format").and_then(serde_json::Value::as_str);
    apps_for(&d.kind, format, SWITCHES)
}

/// `web_apps`, with the switches given (for tests).
pub fn apps_for(kind: &str, format: Option<&str>, on: Switches) -> Vec<(&'static str, &'static str)> {
    let mut apps = Vec::new();
    match kind {
        "embroidery" => apps.push((STITCHFOUNDRY_WEB, "StitchFoundry")),
        "laser" => {
            if on.laser && matches!(format, Some("SVG" | "DXF")) {
                apps.push((LASERFOUNDRY_WEB, "LaserFoundry"));
            }
            if on.screen && format == Some("SVG") {
                apps.push((SCREENFOUNDRY_WEB, "ScreenFoundry"));
            }
            if on.pattern && format == Some("SVG") {
                apps.push((PATTERNFOUNDRY_WEB, "PatternFoundry"));
            }
        }
        _ => {}
    }
    apps
}

/// The name design `id`'s file goes by in the links and the download: its title as a slug
/// (lowercase ASCII letters, digits and dashes), or `design-<id>` when that leaves nothing, and its
/// file's extension (letters and digits only). Every web app takes it as one plain path part.
pub fn file_name(d: &designs::DesignRow, id: i64) -> String {
    let format = d.facts.get("format").and_then(serde_json::Value::as_str);
    name_of(&d.title, &d.file_name, format, id)
}

/// `file_name`, from the design's title, its file's name and format.
pub fn name_of(title: &str, file: &str, format: Option<&str>, id: i64) -> String {
    let stem = crate::listings::slug(title);
    let stem = if stem.is_empty() { format!("design-{id}") } else { stem };
    let ext: String = file.rsplit_once('.').map(|(_, e)| e).unwrap_or("").to_ascii_lowercase();
    // a laser file's ending is its format's, whatever its file was called: an SVG read from
    // `logo.xml` still goes out as `.svg`, which is all ScreenFoundry and LaserFoundry take
    // ...and an embroidery file's is its format's too: never an ending it was uploaded with that
    // isn't a design format's, such as `.exe` (#86)
    let ext = format
        .and_then(crate::designs::format_extension)
        .or_else(|| crate::designs::design_extension(&format!(".{ext}")))
        .unwrap_or_default();
    if ext.is_empty() { stem } else { format!("{stem}.{ext}") }
}

/// Whether design `d`'s file is one a web app can open: any embroidery file; an SVG or DXF laser file.
pub fn openable(d: &designs::DesignRow) -> bool {
    d.kind == "embroidery" || (d.kind == "laser" && opens_in_laserfoundry(d))
}

fn opens_in_laserfoundry(d: &designs::DesignRow) -> bool {
    matches!(
        d.facts.get("format").and_then(serde_json::Value::as_str),
        Some("SVG" | "DXF")
    )
}

/// The link that opens design `id` in StitchFoundry's web app: its file's address on this site
/// (`public_url`), ending in `name` so the app knows how to read it.
pub fn open_in_stitchfoundry(public_url: &str, id: i64, name: &str) -> String {
    open_link(STITCHFOUNDRY_WEB, public_url, id, name, None)
}

/// The link that opens design `id`'s file (named `name`) in web app `app`.
pub fn open_in(app: &str, public_url: &str, id: i64, name: &str) -> String {
    open_link(app, public_url, id, name, None)
}

/// The same, for a paid design: the file address carries a buyer's link (`t`, see `open_paid`).
fn open_link(app: &str, public_url: &str, id: i64, name: &str, token: Option<&str>) -> String {
    let file = format!(
        "{}/api/v1/designs/{id}/file/{}{}",
        public_url.trim_end_matches('/'),
        crate::listings::url_word(name),
        token.map(|t| format!("?t={t}")).unwrap_or_default()
    );
    format!("{app}?open={}", crate::listings::url_word(&file))
}

/// How long a buyer's link to a paid design's file lasts.
const OPEN_LINK_MINUTES: i32 = 10;

fn token_hash(token: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct OpenQuery {
    /// Which web app (its name in lowercase); the first that opens the design if not given.
    app: String,
}

/// `GET /d/{id}/open?app=<name>`: Open in StitchFoundry (or another web app) for a paid design. The web app fetches without
/// anyone's sign-in, so this checks here that the person may download it (a buyer, its maker or an
/// admin), makes a link good for this design for ten minutes (kept only as its hash), and sends
/// them to the web app with it.
pub async fn open_paid(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    axum::extract::Query(q): axum::extract::Query<OpenQuery>,
) -> Response {
    let back = Redirect::to(&format!("/d/{id}")).into_response();
    let Some(viewer) = crate::accounts::current_user(&state, &headers).await else {
        return Redirect::to(&format!("/signin?next=/d/{id}")).into_response();
    };
    let Some(pool) = state.db.as_ref() else { return back };
    let Ok(Some(d)) = designs::design(pool, id).await else {
        return back;
    };
    let (_, may) = crate::listings::access(&state, pool, Some(&viewer), &d, id).await;
    let apps = web_apps(&d);
    let chosen = apps
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(&q.app))
        .or_else(|| apps.first());
    let Some(&(app, _)) = chosen.filter(|_| may) else {
        return back;
    };
    let mut bytes = [0u8; 24];
    if getrandom::fill(&mut bytes).is_err() {
        return back;
    }
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let made = sqlx::query(
        "INSERT INTO mf_open_links (token_hash, design_id, expires_at) VALUES ($1, $2, now() + make_interval(mins => $3))",
    )
    .bind(token_hash(&token))
    .bind(id)
    .bind(OPEN_LINK_MINUTES)
    .execute(pool)
    .await;
    if let Err(e) = made {
        eprintln!("api: open link {id}: {e}");
        return back;
    }
    Redirect::to(&open_link(app, &state.public_url, id, &file_name(&d, id), Some(&token))).into_response()
}

/// Whether `token` is a buyer's link to design `id` that hasn't run out.
async fn link_good(pool: &sqlx::PgPool, token: &str, id: i64) -> bool {
    if token.len() != 48 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM mf_open_links WHERE token_hash = $1 AND design_id = $2 AND expires_at > now()",
    )
    .bind(token_hash(token))
    .bind(id)
    .fetch_one(pool)
    .await
    .is_ok_and(|n| n > 0)
}

/// Forgets buyers' links that have run out. Run every few minutes.
pub async fn forget_old_links(pool: &sqlx::PgPool) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM mf_open_links WHERE expires_at < now()")
        .execute(pool)
        .await
        .map(|r| r.rows_affected())
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct FileQuery {
    /// A buyer's link to a paid design's file (`open_paid`).
    t: String,
}

/// An error as the apps get it: `{"error": {"message"}}` with the status.
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": { "message": message } }))).into_response()
}

/// The CORS headers for a request from `origin`, if it's one of the web apps.
fn cors(state: &AppState, headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) else {
        return Vec::new();
    };
    if !state.api_origins.iter().any(|o| o == origin) {
        return Vec::new();
    }
    let Ok(origin) = HeaderValue::from_str(origin) else {
        return Vec::new();
    };
    vec![
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin),
        (header::VARY, HeaderValue::from_static("Origin")),
    ]
}

fn with_cors(state: &AppState, headers: &HeaderMap, mut resp: Response) -> Response {
    for (k, v) in cors(state, headers) {
        resp.headers_mut().insert(k, v);
    }
    resp
}

/// `GET /api/v1/designs/{id}/file/{name}`: a free, published embroidery design's file, for
/// StitchFoundry's web app (Open in StitchFoundry). The file itself is sent (not a link to the
/// storage, which another site's page couldn't follow), with CORS for the web apps' sites; cookies
/// aren't read, so it is only what anyone may download. `name` only ends the address, for the app.
pub async fn design_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, _name)): Path<(i64, String)>,
    axum::extract::Query(q): axum::extract::Query<FileQuery>,
) -> Response {
    let mut resp = design_file_answer(&state, &headers, id, &q.t).await;
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("content-disposition"),
    );
    with_cors(&state, &headers, resp)
}

async fn design_file_answer(state: &AppState, headers: &HeaderMap, id: i64, token: &str) -> Response {
    let (Some(pool), Some(storage)) = (state.db.as_ref(), state.storage.as_ref()) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MakerFoundry can't send designs just now.",
        );
    };
    let missing = || error(StatusCode::NOT_FOUND, "That design isn't on MakerFoundry.");
    let Ok(Some(d)) = designs::design(pool, id).await else {
        return missing();
    };
    // what anyone may see and download: published, not taken down, and free; or a paid design
    // with a buyer's link (made once their right to it was checked), unless it was taken down
    let (seen, may) = crate::listings::access(state, pool, None, &d, id).await;
    let bought = !may && d.status != "removed" && !token.is_empty() && link_good(pool, token, id).await;
    if (!seen && !bought) || !openable(&d) {
        return missing();
    }
    if !may && !bought {
        return error(
            StatusCode::FORBIDDEN,
            "This design is for sale: buy it on MakerFoundry, then download it there.",
        );
    }
    if !state.limits.allow(limits::OPEN, &limits::client(headers)) {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many designs opened lately. Please wait a while.",
        );
    }
    let bytes = match storage.get(&d.file_key).await {
        Ok(Some(b)) => b,
        Ok(None) => return missing(),
        Err(e) => {
            eprintln!("api: file {id}: {e}");
            return error(
                StatusCode::BAD_GATEWAY,
                "The design can't be fetched just now. Please try again.",
            );
        }
    };
    let _ = sqlx::query("UPDATE mf_designs SET downloads = downloads + 1 WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await; // (a count; the file goes out anyway)
    let name = file_name(&d, id);
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300")),
        ],
        bytes,
    )
        .into_response()
}

/// A browser asking first whether a web app may send to the API.
pub async fn preflight(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let allowed = cors(&state, &headers);
    if allowed.is_empty() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    let h = resp.headers_mut();
    for (k, v) in allowed {
        h.insert(k, v);
    }
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("POST"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    resp
}

/// What an app sends: the design file and, if it has them, photos of the result, each base64.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NewDesign {
    file_name: String,
    file: String,
    photos: Vec<String>,
    /// The app and its version, for the log (`StitchFoundry 0.9.1`).
    app: String,
}

/// `POST /api/v1/designs`: a new draft → `201 {id, url, kind, format}`.
pub async fn new_design(State(state): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let resp = answer(&state, &headers, &body).await;
    with_cors(&state, &headers, resp)
}

async fn answer(state: &AppState, headers: &HeaderMap, body: &[u8]) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MakerFoundry can't take designs just now.",
        );
    };
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if token.is_empty() || token.len() > 200 {
        return error(StatusCode::UNAUTHORIZED, "Sign in to your Foundry Account first.");
    }
    // a wrong token costs a try, as a wrong password does
    let client = limits::client(headers);
    let account = match state.cloud.account(token).await {
        Ok(a) => a,
        Err(e) if e.is_signed_out() => {
            if !state.limits.allow(limits::SIGNIN, &client) {
                return error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many tries lately. Please wait a while.",
                );
            }
            return error(
                StatusCode::UNAUTHORIZED,
                "Your Foundry Account sign-in has ended: sign in again.",
            );
        }
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Foundry Accounts can't be reached right now. Please try again in a few minutes.",
            );
        }
    };
    if let Err(e) = db::remember_account(pool, &account).await {
        eprintln!("api: {e}");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MakerFoundry can't take designs just now.",
        );
    }
    let Ok(d) = serde_json::from_slice::<NewDesign>(body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "The request isn't a design MakerFoundry can read.",
        );
    };
    let file_name = clean_name(&d.file_name);
    if file_name.is_empty() {
        return error(StatusCode::BAD_REQUEST, "Give the design file's name.");
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let Ok(bytes) = b64.decode(d.file.as_bytes()) else {
        return error(StatusCode::BAD_REQUEST, "The design file isn't base64.");
    };
    if bytes.is_empty() {
        return error(StatusCode::BAD_REQUEST, "The design file is empty.");
    }
    if d.photos.len() > MAX_PHOTOS {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("A design can have up to {MAX_PHOTOS} photos."),
        );
    }
    let mut photos = Vec::new();
    for (i, p) in d.photos.iter().enumerate() {
        match b64.decode(p.as_bytes()) {
            Ok(bytes) => photos.push(bytes),
            Err(_) => return error(StatusCode::BAD_REQUEST, &format!("Photo {} isn't base64.", i + 1)),
        }
    }
    match designs::add(state, pool, account.id, &file_name, bytes, photos).await {
        Ok(id) => {
            // a draft from an app is named after its file, so it's never left without a title, and
            // keeps the app's name (MF-88), for finding designs by the app that made them
            let _ = sqlx::query(
                "UPDATE mf_designs SET title = CASE WHEN title = '' THEN $2 ELSE title END, from_app = $3 WHERE id = $1",
            )
            .bind(id)
            .bind(title_from(&file_name))
            .bind(app_name(&d.app))
            .execute(pool)
                .await
                .map_err(|e| eprintln!("api: title of {id}: {e}"));
            let app: String = d.app.chars().filter(|c| !c.is_control()).take(60).collect();
            eprintln!("api: design {id} from {}", if app.is_empty() { "an app" } else { &app });
            let read: Option<(String, String)> = sqlx::query_as("SELECT kind, format FROM mf_designs WHERE id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
            let (kind, format) = read.unwrap_or_default();
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": id,
                    "url": format!("{}/designs/{id}", state.public_url),
                    "kind": kind,
                    "format": format,
                })),
            )
                .into_response()
        }
        Err(Refused::Unavailable) => error(StatusCode::SERVICE_UNAVAILABLE, &Refused::Unavailable.message()),
        Err(r @ Refused::TooMany(_)) => error(StatusCode::TOO_MANY_REQUESTS, &r.message()),
        Err(r @ Refused::Bad(_)) => error(StatusCode::UNPROCESSABLE_ENTITY, &r.message()),
        Err(r @ Refused::Failed) => error(StatusCode::SERVICE_UNAVAILABLE, &r.message()),
    }
}

/// An app's name from what it sends as `app` ("ScreenFoundry 0.1.0"): its first word, letters and
/// digits only, at most 40; `None` when that leaves nothing.
pub fn app_name(app: &str) -> Option<String> {
    let first = app.split_whitespace().next().unwrap_or("");
    let name: String = first.chars().filter(char::is_ascii_alphanumeric).take(40).collect();
    (!name.is_empty()).then_some(name)
}

/// A title made from a file's name: without its extension, `_` and `-` as spaces, at most 100
/// characters; "Untitled design" when that leaves nothing.
pub fn title_from(file_name: &str) -> String {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(s, _)| s);
    let words: Vec<&str> = stem
        .split(|c: char| c == '_' || c == '-' || c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let title: String = words.join(" ").chars().take(100).collect();
    let title = title.trim().to_string();
    if title.is_empty() {
        "Untitled design".into()
    } else {
        title
    }
}

/// A file name as sent, reduced to its last part, without control characters, at most 200
/// characters.
fn clean_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    last.chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect::<String>()
        .trim()
        .to_string()
}

/// `GET /api/v1`: what the API is, for an app checking it's talking to MakerFoundry.
pub async fn about(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let resp = Json(json!({
        "name": "MakerFoundry",
        "version": crate::VERSION,
        "designs": "/api/v1/designs",
        "max_upload_bytes": designs::MAX_UPLOAD_BYTES,
        "max_photos": MAX_PHOTOS,
    }))
    .into_response();
    with_cors(&state, &headers, resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_keep_only_their_last_part() {
        assert_eq!(clean_name("rose.pes"), "rose.pes");
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name(r"C:\Users\ann\rose.dst"), "rose.dst");
        assert_eq!(clean_name("a\u{0}b\nc.svg"), "abc.svg");
        assert_eq!(clean_name("dir/"), "");
        assert_eq!(clean_name(&"x".repeat(300)).len(), 200);
    }

    #[test]
    fn each_web_app_opens_only_what_it_reads() {
        let names = |kind, format, laser, screen| -> Vec<&str> {
            let on = Switches {
                laser,
                screen,
                pattern: false,
            };
            apps_for(kind, format, on).iter().map(|(_, n)| *n).collect()
        };
        // PatternFoundry: SVG only, after the others
        let all = Switches {
            laser: true,
            screen: true,
            pattern: true,
        };
        let with_pattern: Vec<&str> = apps_for("laser", Some("SVG"), all).iter().map(|(_, n)| *n).collect();
        assert_eq!(with_pattern, ["LaserFoundry", "ScreenFoundry", "PatternFoundry"]);
        for (kind, format) in [
            ("laser", Some("DXF")),
            ("laser", Some("PDF")),
            ("embroidery", Some("PES")),
            ("model3d", Some("STL")),
        ] {
            assert!(
                !apps_for(kind, format, all).iter().any(|(_, n)| *n == "PatternFoundry"),
                "{kind} {format:?}"
            );
        }
        assert_eq!(names("embroidery", Some("PES"), true, true), ["StitchFoundry"]);
        assert_eq!(
            names("laser", Some("SVG"), true, true),
            ["LaserFoundry", "ScreenFoundry"]
        );
        assert_eq!(names("laser", Some("SVG"), false, true), ["ScreenFoundry"]);
        assert_eq!(names("laser", Some("DXF"), true, true), ["LaserFoundry"]);
        // ScreenFoundry opens SVG only: no DXF, PDF, embroidery or 3D
        for (kind, format) in [
            ("laser", Some("DXF")),
            ("laser", Some("PDF")),
            ("embroidery", Some("DST")),
            ("model3d", Some("STL")),
            ("laser", None),
        ] {
            assert!(
                !names(kind, format, false, true).contains(&"ScreenFoundry"),
                "{kind} {format:?}"
            );
        }
        // off until the owner says its web app opens links
        assert!(names("laser", Some("SVG"), false, false).is_empty());
    }

    /// ScreenFoundry's rules for an address it will fetch (SC-11, as its owner gave them):
    /// `Some(reason)` when it would refuse `url`.
    fn screenfoundry_refuses(url: &str) -> Option<&'static str> {
        if url.len() > 2048 || !url.is_ascii() {
            return Some("too long or not ASCII");
        }
        if url
            .bytes()
            .any(|b| b.is_ascii_control() || b == b' ' || b == b'\\' || b == b'#')
        {
            return Some("space, control, backslash or fragment");
        }
        let Some(rest) = url.strip_prefix("https://") else {
            return Some("not https");
        };
        let (host, rest) = rest.split_once('/').unwrap_or((rest, ""));
        if host != "makerfoundry.app" && host != "www.makerfoundry.app" {
            return Some("host");
        }
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        if !path.starts_with("api/v1/") {
            return Some("not under /api/v1/");
        }
        for part in path.split('/') {
            if !part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~%+()!',;=".contains(&b))
            {
                return Some("a character a path part may not have");
            }
            let Some(decoded) = percent_decode(part) else {
                return Some("bad %XX");
            };
            if decoded.is_empty() || decoded == "." || decoded == ".." {
                return Some("an empty, . or .. path part");
            }
            if decoded.chars().any(|c| c == '/' || c == '\\' || c.is_control())
                || percent_decode(&decoded) != Some(decoded.clone())
            {
                return Some("a path part that isn't a plain name");
            }
        }
        if !query
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~%+=&".contains(&b))
        {
            return Some("a character the query may not have");
        }
        None
    }

    fn percent_decode(s: &str) -> Option<String> {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).ok()
    }

    /// The file address inside an `?open=` link.
    fn opened(link: &str) -> String {
        percent_decode(link.split_once("?open=").unwrap().1).unwrap()
    }

    #[test]
    fn the_checker_refuses_what_screenfoundry_refuses() {
        // (so that passing it means something)
        for bad in [
            "http://makerfoundry.app/api/v1/designs/1/file/a.svg",
            "https://makerfoundry.app.evil.com/api/v1/designs/1/file/a.svg",
            "https://makerfoundry.app@evil.com/api/v1/designs/1/file/a.svg",
            "https://makerfoundry.app:443/api/v1/designs/1/file/a.svg",
            "https://makerfoundry.app./api/v1/designs/1/file/a.svg",
            "https://makerfoundry.app/designs/1/file/a.svg",
            "https://makerfoundry.app/api/v1/designs/1/file/..",
            "https://makerfoundry.app/api/v1/designs/1/file/%2e%2e",
            "https://makerfoundry.app/api/v1/designs/1/file/a%2Fb.svg",
            "https://makerfoundry.app/api/v1/designs/1/file/a%252Fb.svg",
            "https://makerfoundry.app/api/v1/designs/1/file/a b.svg",
            "https://makerfoundry.app/api/v1/designs/1/file/a:b.svg",
            "https://makerfoundry.app/api/v1/designs/1/file/a.svg#x",
            "https://makerfoundry.app/api/v1/designs/1/file/a.svg?t=<x>",
            "https://makerfoundry.app/api/v1/designs/1/file/ä.svg",
        ] {
            assert!(screenfoundry_refuses(bad).is_some(), "{bad}");
        }
        assert_eq!(
            screenfoundry_refuses("https://makerfoundry.app/api/v1/designs/1/file/a.svg?t=ab12"),
            None
        );
    }

    #[test]
    fn every_title_makes_an_address_screenfoundry_takes() {
        for title in [
            "Box",
            "",
            "   ",
            "..",
            ".",
            "%2e%2e",
            "a/b\\c",
            "日本の箱",
            "☃",
            "Rose & \"Fern\"",
            "<script>",
            "a:b@c#d?e",
            "x\u{0}y",
            "café crème",
            "C:\\Users\\x",
            &"long ".repeat(100),
        ] {
            for (file, format) in [
                ("box.svg", Some("SVG")),
                ("weird.s v/g", Some("SVG")),
                ("noext", Some("SVG")),
                ("x.SVG", None),
            ] {
                let name = name_of(title, file, format, 42);
                for token in [None, Some("0123456789abcdef0123456789abcdef0123456789abcdef")] {
                    let link = open_link(SCREENFOUNDRY_WEB, "https://makerfoundry.app", 42, &name, token);
                    assert!(link.starts_with("https://app.screenfoundry.app/?open=https%3A%2F%2Fmakerfoundry.app%2Fapi%2Fv1%2Fdesigns%2F42%2Ffile%2F"));
                    let address = opened(&link);
                    assert_eq!(screenfoundry_refuses(&address), None, "{title:?} {file} → {address}");
                }
                assert!(name.ends_with(".svg"), "{title:?} {file} → {name}");
                // ScreenFoundry's own rule (2026-11-01): only a-z, 0-9 and "-", then ".svg"
                let stem = name.strip_suffix(".svg").unwrap_or("");
                assert!(
                    !stem.is_empty()
                        && stem
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                    "{title:?} {file} → {name}"
                );
            }
        }
        assert_eq!(name_of("", "box.svg", Some("SVG"), 42), "design-42.svg");
        assert_eq!(name_of("日本", "box.svg", Some("SVG"), 7), "design-7.svg");
        assert_eq!(name_of("Rose & Fern", "x.svg", Some("SVG"), 1), "rose-fern.svg");
        // an SVG uploaded under another ending still ends in .svg (ScreenFoundry takes only that)
        assert_eq!(name_of("Logo", "logo.xml", Some("SVG"), 3), "logo.svg");
        assert_eq!(name_of("Logo", "LOGO.SVG", Some("SVG"), 3), "logo.svg");
        assert_eq!(name_of("Box", "box.txt", Some("DXF"), 3), "box.dxf");
    }

    proptest::proptest! {
        #[test]
        fn any_title_and_file_name_make_a_plain_name(title in "\\PC{0,120}", file in "\\PC{0,40}") {
            let name = name_of(&title, &file, Some("SVG"), 9);
            let link = open_link(SCREENFOUNDRY_WEB, "https://makerfoundry.app", 9, &name, None);
            proptest::prop_assert_eq!(screenfoundry_refuses(&opened(&link)), None);
            proptest::prop_assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'));
            let stem = name.strip_suffix(".svg").unwrap_or("");
            proptest::prop_assert!(!stem.is_empty() && !stem.contains('.'), "{}", name);
        }
    }

    #[test]
    fn an_apps_name_is_kept_without_its_version() {
        assert_eq!(app_name("ScreenFoundry 0.1.0").as_deref(), Some("ScreenFoundry"));
        assert_eq!(app_name("  StitchFoundry\t0.9.1").as_deref(), Some("StitchFoundry"));
        assert_eq!(app_name("LaserFoundry").as_deref(), Some("LaserFoundry"));
        assert_eq!(
            app_name("<script>alert(1)</script>").as_deref(),
            Some("scriptalert1script")
        );
        assert_eq!(app_name("").as_deref(), None);
        assert_eq!(app_name("  1.0").as_deref(), Some("10"));
        assert_eq!(app_name("日本 1").as_deref(), None);
        assert_eq!(app_name(&"x".repeat(100)).map(|n| n.len()), Some(40));
    }

    #[test]
    fn a_draft_from_an_app_is_named_after_its_file() {
        assert_eq!(title_from("Tiger_shirt-front.svg"), "Tiger shirt front");
        assert_eq!(title_from("tiger.svg"), "tiger");
        assert_eq!(title_from(".svg"), "Untitled design");
        assert_eq!(title_from("___.svg"), "Untitled design");
        assert_eq!(title_from("a\u{7}b.svg"), "a b");
        assert_eq!(title_from(&format!("{}.svg", "x".repeat(300))).chars().count(), 100);
    }

    #[test]
    fn the_limit_takes_a_whole_upload_in_base64() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let encoded = b64.encode(vec![0u8; designs::MAX_UPLOAD_BYTES - 3]);
        assert!(encoded.len() < API_LIMIT_BYTES);
    }
}
