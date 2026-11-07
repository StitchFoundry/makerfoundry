//! The pages answer, with the headers every page must carry.

// The helpers below are test code too, but outside `#[test]` functions, where clippy.toml's
// allow-unwrap-in-tests does not reach.
#![allow(clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// The site without a database or a reachable account service: the pages that need neither.
fn app() -> axum::Router {
    let cloud = makerfoundry::cloud::Cloud::new("http://127.0.0.1:9").unwrap();
    makerfoundry::app(makerfoundry::AppState {
        cloud,
        db: None,
        storage: None,
        publishers: makerfoundry::listings::Publishers::Only(Vec::new()),
        admins: Vec::new(),
        limits: std::sync::Arc::default(),
        stripe: None,
        launched: false,
        public_url: "https://makerfoundry.test".into(),
        api_origins: vec!["https://stitchfoundry.test".into()],
        proofs: None,
        three_d: false,
        scanner: None,
        thingiverse: None,
        social: None,
        notices: None,
        step: None,
        scad: None,
        points: false,
        stitchhub: None,
    })
}

async fn get(path: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let request = Request::builder().uri(path).body(Body::empty()).unwrap();
    let response = app().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn health_answers_ok_with_the_version() {
    let (status, headers, body) = get("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, format!("ok {}\n", makerfoundry::VERSION));
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn health_answers_head() {
    let request = Request::builder()
        .method(Method::HEAD)
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn home_page_is_html_and_links_the_stylesheet() {
    let (status, headers, body) = get("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
    assert!(body.starts_with("<!doctype html>"));
    assert!(body.contains("<title>MakerFoundry</title>"));
    assert!(body.contains(r#"href="/static/site.css""#));
}

#[tokio::test]
async fn home_page_has_a_share_card() {
    let (_, _, body) = get("/").await;
    for tag in ["og:title", "og:description", "og:url", "twitter:card"] {
        assert!(body.contains(tag), "missing {tag}");
    }
}

#[tokio::test]
async fn home_page_names_all_three_crafts() {
    let (_, _, body) = get("/").await;
    assert!(body.contains("<h2>Embroidery</h2>") && body.contains("<h2>Laser cutting</h2>"));
    assert!(body.contains("<h2>3D printing</h2>"), "{body}");
    assert!(body.contains("STL, 3MF, OBJ and STEP"));
    // Without MakerFoundry 3D switched on, the card says the models are still coming.
    assert!(body.contains("download. 3D models open soon.</p>"), "{body}");
    assert!(
        body.contains("3D-print models"),
        "the lead and the description name them too"
    );
}

#[tokio::test]
async fn before_the_launch_the_home_page_leads_with_founding_creators() {
    let (_, _, body) = get("/").await;
    assert!(body.contains("<h2>Become a founding creator</h2>"), "{body}");
    assert!(body.contains(r#"<a class="button" href="/founding">Apply to be a founding creator</a>"#));
    assert!(
        !body.contains("Coming soon. MakerFoundry is being built now."),
        "the old notice is gone"
    );
}

#[tokio::test]
async fn stylesheet_is_css() {
    let (status, headers, body) = get("/static/site.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/css"));
    assert!(body.contains(":root"));
}

#[tokio::test]
async fn favicon_is_a_standalone_svg() {
    let (status, headers, body) = get("/favicon.svg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/svg+xml");
    assert!(body.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
    assert!(!body.contains("currentColor"));
    assert!(
        !body.contains("class=\"mark\""),
        "the page's class doesn't belong in a stand-alone icon"
    );
    assert!(
        body.contains("<rect width=\"64\" height=\"64\""),
        "the mark's own tile, so it shows on any tab colour"
    );
}

#[tokio::test]
async fn unknown_paths_get_the_404_page() {
    let (status, _, body) = get("/no/such/page").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Page not found"));
}

#[tokio::test]
async fn every_response_carries_the_security_headers() {
    for path in ["/", "/health", "/static/site.css", "/favicon.svg", "/missing"] {
        let (_, headers, _) = get(path).await;
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("default-src 'none'"), "{path}: {csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY", "{path}");
        assert!(headers.contains_key(header::REFERRER_POLICY), "{path}");
        // HTTPS only, remembered for a year (no includeSubDomains: other services may share the domain)
        assert_eq!(headers[header::STRICT_TRANSPORT_SECURITY], "max-age=31536000", "{path}");
    }
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let big = vec![b'x'; 1024 * 1024];
    let request = Request::builder()
        .method(Method::POST)
        .uri("/")
        .header(header::CONTENT_LENGTH, big.len())
        .body(Body::from(big))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn every_page_links_the_legal_pages_which_wait_for_the_launch() {
    let (_, _, home) = get("/").await;
    for link in ["/terms", "/privacy", "/seller-terms", "/dmca"] {
        assert!(home.contains(&format!(r#"href="{link}""#)), "{link}");
    }
    let (status, _, terms) = get("/terms").await;
    assert_eq!(status, StatusCode::OK);
    assert!(terms.contains("hasn't opened yet"));
}

#[test]
fn commit_ids_are_shortened_and_checked() {
    assert_eq!(
        makerfoundry::short_commit("E82F8691dc91fbdfd74427f84f8d7b259801b99d").as_deref(),
        Some("e82f869")
    );
    for bad in ["", "abc", "e82f869x", "../../etc", "ééééééé"] {
        assert_eq!(makerfoundry::short_commit(bad), None, "{bad}");
    }
}

/// Issue #80: the note counter is our own script file, and the policy that lets it run adds only
/// `script-src 'self'` to the site's: no inline script, no other origin, nothing fetched.
#[tokio::test]
async fn the_note_counter_is_a_same_origin_script_and_its_policy_adds_only_that() {
    let (status, headers, body) = get("/static/note-counter.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert!(
        body.contains("textarea[data-count]") && body.contains(".trim().length"),
        "{body}"
    );
    let csp = makerfoundry::NOTE_COUNTER_SECURITY_POLICY;
    assert!(csp.contains("script-src 'self';"), "{csp}");
    for never in ["unsafe-inline", "unsafe-eval", "connect-src", "http:", "*"] {
        assert!(!csp.contains(never), "{never}: {csp}");
    }
    // everything else is the site's own policy, as every other page gets it
    let (_, home, _) = get("/").await;
    let site = home[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(!site.contains("script-src"), "{site}");
    assert_eq!(csp.replace(" script-src 'self';", ""), site);
}
