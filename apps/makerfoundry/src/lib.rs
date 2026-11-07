//! MakerFoundry's web service: server-rendered pages for the marketplace.
//!
//! MF-0: a home page, `/health` for the host's health check, a stylesheet and a 404 page, all
//! behind the security headers every page gets. MF-1: accounts, which are Foundry Accounts kept by StitchFoundry's account service
//! (`cloud`), with MakerFoundry's own sessions in Postgres (`db`) and the pages in `accounts`.

pub mod accounts;
pub mod activity;
pub mod api;
pub mod cloud;
pub mod coarse;
pub mod collections;
pub mod comments;
pub mod convert;
pub mod data;
pub mod db;
pub mod designs;
pub mod disputes;
pub mod embroidery;
pub mod files;
pub mod follows;
pub mod founding;
pub mod laser;
pub mod legal;
pub mod licences;
pub mod limits;
pub mod listings;
pub mod makers;
pub mod makes;
pub mod model3d;
pub mod moderation;
pub mod money;
pub mod notices;
pub mod overview;
pub mod pages;
pub mod photos;
pub mod picks;
pub mod preview;
pub mod proofs;
pub mod refunds;
pub mod scan;
pub mod selling;
pub mod shop_orders;
pub mod sizes;
pub mod storage;
pub mod stripe;

use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use sqlx::PgPool;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;

use crate::pages::{MARK_SVG, page};

/// The version shown on `/health`, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit running, also shown on `/health` so a deploy can be checked from outside: set once
/// at start-up from Railway's `RAILWAY_GIT_COMMIT_SHA` (its first 7 characters).
pub static COMMIT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// A commit id as shown: its first 7 hex digits, or nothing if it isn't one.
pub fn short_commit(sha: &str) -> Option<String> {
    let sha = sha.trim();
    sha.get(..7)
        .filter(|_| sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

/// Bodies bigger than this are refused before a handler sees them. Uploads (MF-2) get their own,
/// larger cap on their own route.
const BODY_LIMIT_BYTES: usize = 64 * 1024;

/// The upload routes' cap: the largest file accepted, and room for the form around it.
const UPLOAD_LIMIT_BYTES: usize = laser::MAX_FILE_BYTES + 64 * 1024;

/// A request that takes longer than this is answered with 408.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The same for a 3D model's upload: up to 300 MB to receive, read, draw and store.
const MODEL_UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// The same for a bulk import (up to 2 GB) and making its drafts.
const IMPORT_TIMEOUT: Duration = Duration::from_secs(900);

/// Scripts, frames, plug-ins and other origins are off; pages use only our own stylesheet. Forms
/// post only here, and may be sent on to Stripe's checkout and set-up pages (browsers check
/// `form-action` against a form's redirects too).
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'self'; img-src 'self' data:; \
     base-uri 'none'; form-action 'self' https://checkout.stripe.com https://connect.stripe.com; \
     frame-ancestors 'none'";

/// Stripe's events can be larger than a form.
const WEBHOOK_LIMIT_BYTES: usize = 1024 * 1024;

const STYLESHEET: &str = include_str!("../static/site.css");
const VIEWER_SCRIPT: &str = include_str!("../static/viewer.js");
const NOTE_COUNTER_SCRIPT: &str = include_str!("../static/note-counter.js");

/// The order form and the customer's order page also run our note counter (issue #80; the owner
/// agreed to its JavaScript, 2026-11-03): `CONTENT_SECURITY_POLICY` with only `script-src 'self'`
/// added, so our own script files run and nothing else does (no inline script, no other origin,
/// no `connect-src`: the counter fetches nothing).
pub const NOTE_COUNTER_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; base-uri 'none'; form-action 'self' https://checkout.stripe.com https://connect.stripe.com; \
     frame-ancestors 'none'";

/// A page that loads `/static/note-counter.js`: its policy lets our own scripts run.
pub fn with_note_counter(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(NOTE_COUNTER_SECURITY_POLICY),
    );
    response
}

/// A 3D model's page also runs our viewer script and lets it fetch the model's mesh (MF-46; the
/// owner agreed to its JavaScript, 2026-10-30). Every other page keeps `CONTENT_SECURITY_POLICY`.
pub const MODEL_PAGE_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; connect-src 'self'; style-src 'self'; \
     img-src 'self' data:; base-uri 'none'; form-action 'self' https://checkout.stripe.com https://connect.stripe.com; \
     frame-ancestors 'none'";

/// What every request may use: the account service, and the database when there is one (without
/// `DATABASE_URL` the site runs, but nobody can sign in).
#[derive(Clone)]
pub struct AppState {
    pub cloud: cloud::Cloud,
    pub db: Option<PgPool>,
    /// Where files are kept (R2); without it designs can be previewed but not kept.
    pub storage: Option<storage::Storage>,
    /// Who may publish designs (`MF_PUBLISHERS`; nobody until the launch).
    pub publishers: listings::Publishers,
    /// The accounts that see reports and take designs down (`MF_ADMINS`).
    pub admins: Vec<i64>,
    /// How often an address or an account may do things.
    pub limits: std::sync::Arc<limits::Limits>,
    /// Stripe, for selling (MF-4); without it every design is free.
    pub stripe: Option<stripe::Stripe>,
    /// Whether MakerFoundry has opened to everyone (MF-6: `MF_LAUNCHED`, once the legal pages are
    /// approved): the legal pages are public, and signing up, publishing and selling ask people to
    /// agree to them.
    pub launched: bool,
    /// The site's address (`MF_PUBLIC_URL`), for the sitemap and the links the API answers.
    pub public_url: String,
    /// The web apps' sites that may call the API from a browser (`MF_API_ORIGINS`).
    pub api_origins: Vec<String>,
    /// Approval links (MF-25: `MF_PROOF_KEY`, and `MF_PROOFS=yes` for everyone, admins before);
    /// without the key there are none.
    pub proofs: Option<proofs::Proofs>,
    /// MakerFoundry 3D (MF-35: `MF_3D=yes` for everyone, admins before).
    pub three_d: bool,
    /// Virus scanning of 3D uploads (issue #29: `MF_CLAMAV`, `host:port` of a ClamAV daemon);
    /// without it uploads aren't scanned.
    pub scanner: Option<scan::Scanner>,
    /// Importing from a linked Thingiverse account (issue #28: `MF_THINGIVERSE_CLIENT_ID` and
    /// `MF_THINGIVERSE_CLIENT_SECRET`); without them it's off.
    pub thingiverse: Option<model3d::thingiverse::Thingiverse>,
    /// Posting showcase items to MakerFoundry's Facebook Page and Instagram (issue #30:
    /// `MF_FACEBOOK_PAGE_ID`, `MF_FACEBOOK_PAGE_TOKEN`, `MF_INSTAGRAM_USER_ID`); without them
    /// admins post by hand.
    pub social: Option<model3d::social::Social>,
    /// Email notices, a daily digest each member chooses (issue #31: `MF_RESEND_KEY`); without it
    /// there are none.
    pub notices: Option<notices::Notices>,
    /// STEP previews through a converter service (issue #32: `MF_STEP_CONVERTER`, and
    /// `MF_STEP_CONVERTER_KEY`); without it STEP files have no preview.
    pub step: Option<model3d::step::Converter>,
    /// Parametric models through a sandboxed OpenSCAD service (issue #35: `MF_SCAD_SERVICE`, and
    /// `MF_SCAD_SERVICE_KEY`); without it there's no customizing.
    pub scad: Option<model3d::scad::Renderer>,
    /// Points (issue #34: `MF_POINTS=yes`), with no rewards until the owner decides on them.
    pub points: bool,
    /// Ordering from shops through StitchHub (MF-80 to MF-84: `MF_STITCHHUB_URL` and
    /// `MF_STITCHHUB_KEY`); without them there's none.
    pub stitchhub: Option<shop_orders::hub::Hub>,
}

/// The whole site: every route, with the security headers and limits applied to all of them.
pub fn app(state: AppState) -> Router {
    let shop_addresses = state.clone();
    let pages = Router::new()
        .route("/", get(home))
        .route("/health", get(health))
        .route("/static/site.css", get(stylesheet))
        .route("/static/viewer.js", get(viewer_script))
        .route("/static/note-counter.js", get(note_counter_script))
        .route("/favicon.svg", get(favicon))
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/terms", get(legal::terms))
        .route("/privacy", get(legal::privacy))
        .route("/seller-terms", get(legal::seller_terms))
        .route("/signin", get(accounts::signin_page).post(accounts::signin))
        .route("/signup", get(accounts::signup_page).post(accounts::signup))
        .route("/signout", post(accounts::signout))
        .route("/forgot", get(accounts::forgot_page).post(accounts::forgot))
        .route("/account", get(accounts::account_page))
        .route("/designs", get(designs::list))
        .route("/designs/{id}", get(designs::show))
        .route("/designs/{id}/download", get(designs::download))
        .route("/designs/{id}/photos/{photo}/{size}", get(designs::photo))
        .route("/designs/{id}/delete", post(designs::delete))
        .route("/designs/{id}/files/{file}/delete", post(files::remove))
        .route("/designs/{id}/edit", get(listings::edit_page).post(listings::save))
        .route("/designs/{id}/publish", post(listings::publish))
        .route("/designs/{id}/unpublish", post(listings::unpublish))
        .route("/d/{id}", get(listings::public_page))
        .route("/d/{id}/download", get(listings::download))
        .route("/d/{id}/open", get(api::open_paid))
        .route("/d/{id}/preview.png", get(listings::preview_png))
        .route("/d/{id}/{slug}", get(listings::public_page_slug))
        .route("/browse", get(listings::browse))
        .route("/picks", get(picks::past))
        .route("/d/{id}/pick", post(picks::pick))
        .route("/d/{id}/save", post(collections::save))
        .route("/d/{id}/like", post(model3d::likes::like))
        .route("/shops", get(shop_orders::shops::list))
        .route("/shops/{handle}", get(shop_orders::shops::show))
        .route("/shops/{handle}/logo", get(shop_orders::shops::logo))
        .route("/shops/{handle}/brand.css", get(shop_orders::shops::brand_css))
        .route("/shop-orders/{reference}", get(shop_orders::orders::page))
        .route("/shop-orders/{reference}/pay", post(shop_orders::orders::pay))
        .route("/shop-orders/{reference}/cancel", post(shop_orders::after::cancel))
        .route(
            "/shop-orders/{reference}/proofs/{proof}",
            post(shop_orders::after::answer),
        )
        .route(
            "/shop-orders/{reference}/proofs/{proof}/image",
            get(shop_orders::after::image),
        )
        .route("/contests", get(model3d::contests::list))
        .route("/contests/{slug}", get(model3d::contests::page))
        .route("/contests/{slug}/enter", post(model3d::contests::enter))
        .route("/contests/{slug}/withdraw/{design}", post(model3d::contests::withdraw))
        .route("/contests/{slug}/vote/{design}", post(model3d::contests::vote))
        .route(
            "/admin/contests",
            get(model3d::contests::admin_page).post(model3d::contests::create),
        )
        .route("/admin/contests/{id}/open", post(model3d::contests::open))
        .route("/admin/contests/{id}/announce", post(model3d::contests::announce))
        .route(
            "/d/{id}/customize/{file}",
            get(model3d::scad::page).post(model3d::scad::make),
        )
        .route("/makes/{mid}/delete", post(makes::delete))
        .route(
            "/makes/{mid}/report",
            get(moderation::make_report_page).post(moderation::make_report),
        )
        .route("/admin/reports/{rid}/remove-make", post(moderation::remove_make))
        .route("/d/{id}/comments", post(comments::post))
        .route("/comments/{cid}/delete", post(comments::delete))
        .route(
            "/comments/{cid}/report",
            get(moderation::comment_report_page).post(moderation::comment_report),
        )
        .route("/admin/reports/{rid}/remove-comment", post(moderation::remove_comment))
        .route("/makes/{mid}/{size}", get(makes::photo))
        .route("/collections", get(collections::list).post(collections::create))
        .route("/collections/{cid}", get(collections::show))
        .route("/collections/{cid}/edit", post(collections::edit))
        .route("/collections/{cid}/delete", post(collections::delete))
        .route("/collections/{cid}/remove/{id}", post(collections::remove))
        .route("/makers/{handle}", get(makers::show))
        .route("/makers/{handle}/follow", post(follows::follow))
        .route("/makers/{handle}/unfollow", post(follows::unfollow))
        .route("/following", get(follows::following))
        .route("/activity", get(activity::page))
        .route("/account/data", get(data::page))
        .route("/account/showcase", get(model3d::showcase::page))
        .route("/account/showcase/{id}/withdraw", post(model3d::showcase::withdraw))
        .route("/account/notices", get(notices::page).post(notices::save))
        .route("/account/points", get(model3d::points::page))
        .route("/account/api-keys", get(model3d::api::page).post(model3d::api::make))
        .route("/account/api-keys/{id}/revoke", post(model3d::api::revoke))
        .route(
            "/notices/unsubscribe/{account}/{token}",
            get(notices::unsubscribe_page).post(notices::unsubscribe),
        )
        .route("/account/data.json", get(data::download))
        .route("/account/data/delete", post(data::delete))
        .route("/account/shops/{handle}/{action}", post(shop_orders::shops::confirm))
        .route("/account/maker", get(makers::edit_page).post(makers::save))
        .route("/d/{id}/report", get(moderation::report_page).post(moderation::report))
        .route(
            "/designs/{id}/counter",
            get(moderation::counter_page).post(moderation::counter),
        )
        .route("/dmca", get(moderation::policy))
        .route("/dmca/notice", get(moderation::notice_page).post(moderation::notice))
        .route("/admin", get(moderation::queue))
        .route("/admin/showcase", get(model3d::showcase::queue))
        .route("/admin/showcase/{id}/posted", post(model3d::showcase::posted))
        .route(
            "/admin/showcase/posts/{id}/taken-down",
            post(model3d::showcase::taken_down),
        )
        .route(
            "/admin/showcase/{id}/post/{platform}",
            post(model3d::showcase::post_now),
        )
        .route("/admin/overview", get(overview::overview))
        .route("/admin/accounts", get(data::admin_accounts))
        .route("/admin/accounts/{account}/delete", post(data::admin_delete))
        .route("/admin/apps", get(overview::apps))
        .route("/admin/reports/{rid}/takedown", post(moderation::takedown))
        .route("/admin/reports/{rid}/dismiss", post(moderation::dismiss))
        .route("/admin/reports/{rid}/restore", post(moderation::restore))
        .route("/account/selling", get(selling::selling_page))
        .route("/account/selling/start", post(selling::start))
        .route("/account/selling/return", get(selling::returned))
        .route("/account/selling/dashboard", post(selling::dashboard))
        .route("/account/sales", get(selling::sales))
        .route("/library", get(selling::library))
        .route("/d/{id}/buy", post(selling::buy))
        .route("/admin/orders", get(selling::orders))
        .route("/admin/orders/{order}/refund", post(selling::refund))
        .route("/admin/orders/{order}/fee-back", post(selling::fee_back))
        .route("/admin/sellers/payout-delay", post(refunds::payout_delay))
        .route("/3d", get(model3d::landing))
        .route("/3d/{id}/files/{file}/{view}", get(model3d::upload::picture))
        .route("/3d/import/{id}", get(model3d::import::review))
        .route("/3d/{id}/files/{file}/delete", post(model3d::upload::remove_file))
        .route("/3d/{id}/parents", post(model3d::remix::add_parent))
        .route("/3d/{id}/parents/{parent}/delete", post(model3d::remix::remove_parent))
        .route("/3d/{id}/tree", get(model3d::remix::tree))
        .route("/3d/import/{id}/arrange", post(model3d::import::arrange))
        .route("/3d/import/{id}/details", post(model3d::import::details))
        .route("/3d/import/{id}/publish", post(model3d::import::publish))
        .route("/founding", get(founding::page).post(founding::apply))
        .route("/admin/founding", get(founding::admin))
        .route("/admin/founding/{account}/{action}", post(founding::decide))
        .route("/proofs", get(proofs::list))
        .route("/proofs/{id}", get(proofs::show))
        .route("/proofs/{id}/link", post(proofs::link))
        .route("/proofs/{id}/reply", post(proofs::shop_reply))
        .route("/proofs/{id}/delete", post(proofs::delete))
        .route("/proofs/{id}/photos/{photo}/{size}", get(proofs::shop_photo))
        .route(
            "/p/{id}/{token}",
            get(proofs::customer_page).post(proofs::customer_reply),
        )
        .route("/p/{id}/{token}/photos/{photo}/{size}", get(proofs::customer_photo))
        .fallback(not_found)
        .layer(RequestBodyLimitLayer::new(BODY_LIMIT_BYTES));
    let uploads = Router::new()
        .route("/preview", get(preview::page).post(preview::upload))
        .layer(DefaultBodyLimit::max(UPLOAD_LIMIT_BYTES))
        .layer(RequestBodyLimitLayer::new(UPLOAD_LIMIT_BYTES));
    let new_designs = Router::new()
        .route("/designs/new", get(designs::new_page).post(designs::upload))
        .route("/designs/{id}/files", post(files::add))
        .route("/d/{id}/makes", post(makes::post))
        .route("/proofs/new", get(proofs::new_page).post(proofs::create))
        .route("/proofs/{id}/versions", post(proofs::add_version))
        .layer(DefaultBodyLimit::max(designs::MAX_UPLOAD_BYTES))
        .layer(RequestBodyLimitLayer::new(designs::MAX_UPLOAD_BYTES));
    // ordering from a shop (MF-83): the form carries the customer's art
    let shop_orders = Router::new()
        .route(
            "/shops/{handle}/order/{sku}",
            get(shop_orders::orders::form).post(shop_orders::orders::create),
        )
        .layer(DefaultBodyLimit::max(shop_orders::orders::BODY_LIMIT_BYTES))
        .layer(RequestBodyLimitLayer::new(shop_orders::orders::BODY_LIMIT_BYTES));
    let webhooks = Router::new()
        .route("/stripe/webhook", post(selling::webhook))
        .layer(DefaultBodyLimit::max(WEBHOOK_LIMIT_BYTES))
        .layer(RequestBodyLimitLayer::new(WEBHOOK_LIMIT_BYTES));
    let api = Router::new()
        .route("/api/v1", get(api::about))
        .route("/api/v1/designs", post(api::new_design).options(api::preflight))
        .route("/api/v1/designs/{id}/file/{name}", get(api::design_file))
        .route("/api/v1/models", get(model3d::api::search))
        .route("/api/v1/models/{id}", get(model3d::api::model))
        .layer(DefaultBodyLimit::max(api::API_LIMIT_BYTES))
        .layer(RequestBodyLimitLayer::new(api::API_LIMIT_BYTES));
    // 3D models (MF-41) are larger and take longer to read and draw: their own cap and time
    let models = Router::new()
        .route("/3d/new", get(model3d::upload::new_page).post(model3d::upload::create))
        .route("/3d/{id}/files", post(model3d::upload::add_files))
        .layer(DefaultBodyLimit::max(model3d::upload::BODY_LIMIT_BYTES))
        .layer(RequestBodyLimitLayer::new(model3d::upload::BODY_LIMIT_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            MODEL_UPLOAD_TIMEOUT,
        ));
    // bulk imports (MF-52): up to 2 GB, kept file by file as it arrives
    let imports = Router::new()
        .route(
            "/3d/import",
            get(model3d::import::new_page).post(model3d::import::create),
        )
        .route("/3d/import/{id}/drafts", post(model3d::import::drafts))
        .route("/3d/import/thingiverse", get(model3d::thingiverse::start))
        .route("/3d/import/thingiverse/back", get(model3d::thingiverse::back))
        .layer(DefaultBodyLimit::max(model3d::import::BODY_LIMIT_BYTES))
        .layer(RequestBodyLimitLayer::new(model3d::import::BODY_LIMIT_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            IMPORT_TIMEOUT,
        ));
    pages
        .merge(uploads)
        .merge(shop_orders)
        .merge(api)
        .merge(new_designs)
        .merge(webhooks)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .merge(models)
        .merge(imports)
        .with_state(state)
        // shops' short addresses (<shop>.makerfoundry.app), before anything else answers
        .layer(axum::middleware::from_fn_with_state(
            shop_addresses,
            shop_orders::shops::subdomains,
        ))
        .layer(header_layer(header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY))
        .layer(header_layer(header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
        .layer(header_layer(header::REFERRER_POLICY, "strict-origin-when-cross-origin"))
        .layer(header_layer(header::X_FRAME_OPTIONS, "DENY"))
        // HTTPS only, remembered for a year; not includeSubDomains, since other services may share
        // the domain (release audit, issue #40)
        .layer(header_layer(header::STRICT_TRANSPORT_SECURITY, "max-age=31536000"))
}

fn header_layer(name: HeaderName, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
}

/// The host's health check: answers as soon as the server is up. Never touches the database, so
/// a database outage shows as errors on pages rather than a restart loop.
async fn health() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        match COMMIT.get() {
            Some(c) => format!("ok {VERSION} {c}\n"),
            None => format!("ok {VERSION}\n"),
        },
    )
        .into_response()
}

/// The 3D viewer's script (MF-46); only model pages load it.
async fn viewer_script() -> Response {
    script(VIEWER_SCRIPT)
}

/// The note counter (issue #80); only the order form and the customer's order page load it.
async fn note_counter_script() -> Response {
    script(NOTE_COUNTER_SCRIPT)
}

fn script(body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
}

async fn stylesheet() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        STYLESHEET,
    )
        .into_response()
}

/// The mark as a stand-alone icon (its colours are its own, so it needs only the SVG namespace).
async fn favicon() -> Response {
    let icon = MARK_SVG.replace(r#"class="mark" "#, r#"xmlns="http://www.w3.org/2000/svg" "#);
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        icon,
    )
        .into_response()
}

async fn home(State(state): State<AppState>, headers: HeaderMap) -> Html<String> {
    let signed_in = accounts::current_user(&state, &headers).await.is_some();
    let newest = listings::newest(&state).await;
    // picks of the week, and the galleries: what people made, and the most downloaded
    let picks = picks::home_html(&state).await;
    let made = picks::makes_html(&state).await;
    let popular = listings::popular(&state).await;
    // Founding creators (MF-54): before the launch the home page leads with the invitation; after
    // it, a third button while spots are left.
    let left = founding::spots_left(&state).await;
    let call = if state.launched {
        let share = if signed_in { "/designs/new" } else { "/signup" };
        let founding = if left.is_some_and(|n| n > 0) {
            r#" <a class="button secondary" href="/founding">Become a founding creator</a>"#
        } else {
            ""
        };
        format!(
            r#"<p class="actions"><a class="button" href="/browse">Browse designs</a> <a class="button secondary" href="{share}">Share your designs</a>{founding}</p>"#
        )
    } else {
        let spots = match left {
            Some(0) => format!(
                "All {} founding spots are taken; MakerFoundry opens to everyone soon.",
                founding::SPOTS
            ),
            Some(1) => format!("1 of {} spots left.", founding::SPOTS),
            Some(n) => format!("{n} of {} spots left.", founding::SPOTS),
            None => String::new(),
        };
        format!(
            r#"<div class="invite">
  <h2>Become a founding creator</h2>
  <p>MakerFoundry opens soon. We're looking for our first {} creators: makers of embroidery designs, laser-cut files and 3D-print models who'll share their work here first. Founding creators get a numbered badge on their page and designs, for good, and early access to MakerFoundry 3D.</p>
  <p class="actions"><a class="button" href="/founding">Apply to be a founding creator</a> <span class="muted">{spots}</span></p>
</div>"#,
            founding::SPOTS
        )
    };
    // 3D models open with MakerFoundry 3D (MF-35, `MF_3D`); until then the card says they're coming.
    let three_d_soon = if state.three_d { "" } else { " 3D models open soon." };
    let home_url = format!("{}/", state.public_url);
    let preview = pages::Preview {
        title: "MakerFoundry: designs for your machines",
        description: "Machine-embroidery designs, laser-cut files and 3D-print models, shared free or sold by the makers who made them.",
        url: &home_url,
        image: None,
    };
    Html(pages::page_with(
        "MakerFoundry",
        &format!(
            r#"<section class="hero">
  <h1>Designs for your machines</h1>
  <p class="lead">Machine-embroidery designs, laser-cut files and 3D-print models, shared free or
  sold by the makers who made them.</p>
  {call}
</section>
<section class="kinds">
  <article>
    <h2>Embroidery</h2>
    <p>DST, PES, JEF, EXP, VP3 and more, with a preview of every stitch, the stitch count,
    the size and the colours.</p>
  </article>
  <article>
    <h2>Laser cutting</h2>
    <p>SVG, DXF and PDF files for cutting and engraving, drawn from their paths with their
    size.</p>
  </article>
  <article>
    <h2>3D printing</h2>
    <p>STL, 3MF, OBJ and STEP models, shown in a 3D viewer you can turn before you
    download.{three_d_soon}</p>
  </article>
  <article>
    <h2>For makers</h2>
    <p>Share a design for free, or sell it: you keep 90% of the price less the card fee, paid
    monthly, and your designs stay yours.</p>
  </article>
</section>
{picks}
{newest}
{made}
{popular}"#
        ),
        signed_in,
        Some(&preview),
    ))
}

/// Search engines: everything public once launched, nothing before.
async fn robots(State(state): State<AppState>) -> Response {
    let text = if state.launched {
        format!(
            "User-agent: *\nDisallow: /account\nDisallow: /admin\nDisallow: /designs\nDisallow: /library\nDisallow: /preview\nDisallow: /signin\nDisallow: /signup\nDisallow: /forgot\nDisallow: /d/*/download\nDisallow: /d/*/report\nDisallow: /api/\nDisallow: /proofs\nDisallow: /p/\n{}Sitemap: {}/sitemap.xml\n",
            if state.three_d { "" } else { "Disallow: /3d\n" },
            state.public_url
        )
    } else {
        "User-agent: *\nDisallow: /\n".to_string()
    };
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        text,
    )
        .into_response()
}

/// The public pages for search engines: the fixed pages, published designs and makers.
async fn sitemap(State(state): State<AppState>) -> Response {
    if !state.launched {
        return not_found().await.into_response();
    }
    let base = &state.public_url;
    let mut urls: Vec<String> = ["/", "/browse", "/picks", "/terms", "/privacy", "/seller-terms", "/dmca"]
        .iter()
        .map(|p| format!("{base}{p}"))
        .collect();
    if state.three_d {
        urls.push(format!("{base}/3d"));
    }
    if let Some(pool) = &state.db {
        let designs: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, title FROM mf_designs WHERE status = 'published' AND (kind <> 'model3d' OR $1)
             ORDER BY published_at DESC LIMIT 40000",
        )
        .bind(state.three_d)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        urls.extend(
            designs
                .iter()
                .map(|(id, t)| format!("{base}/d/{id}/{}", listings::slug(t))),
        );
        let makers: Vec<String> = sqlx::query_scalar(
            "SELECT handle FROM mf_makers m WHERE EXISTS (SELECT 1 FROM mf_designs d WHERE d.account_id = m.account_id AND d.status = 'published') LIMIT 9000",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        urls.extend(makers.iter().map(|h| format!("{base}/makers/{h}")));
    }
    let body: String = urls
        .iter()
        .map(|u| format!("<url><loc>{}</loc></url>", pages::escape(u)))
        .collect();
    (
        [
            (header::CONTENT_TYPE, "application/xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{body}</urlset>"#),
    )
        .into_response()
}

pub(crate) async fn not_found() -> (StatusCode, Html<String>) {
    (
        StatusCode::NOT_FOUND,
        Html(page(
            "Not found · MakerFoundry",
            r#"<section class="hero">
  <h1>Page not found</h1>
  <p class="lead">There's nothing at this address. <a href="/">Go to the home page</a>.</p>
</section>"#,
            false,
        )),
    )
}
