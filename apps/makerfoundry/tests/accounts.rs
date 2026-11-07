//! Accounts end to end: MakerFoundry's pages, its sessions in a real Postgres, and a stand-in for
//! StitchFoundry's account service on a local port (answering as `docs/cloud.md` there says).
//!
//! The database tests need `MF_TEST_DATABASE_URL` (a Postgres they may make schemas in); without
//! it they are skipped. CI sets it.

// The helpers below are test code too, but outside `#[test]` functions, where clippy.toml's
// allow-unwrap-in-tests does not reach.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use http_body_util::BodyExt;
use makerfoundry::AppState;
use makerfoundry::accounts::COOKIE;
use makerfoundry::cloud::Cloud;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;

const HOST: &str = "makerfoundry.test";

/// The stand-in account service's state.
#[derive(Default)]
struct Fake {
    /// email → (id, password)
    accounts: HashMap<String, (i64, String)>,
    /// token → account id
    sessions: HashMap<String, i64>,
    next: i64,
    /// Reset emails asked for.
    forgot: Vec<String>,
}

type Shared = Arc<Mutex<Fake>>;

fn error(status: StatusCode, message: &str) -> axum::response::Response {
    (
        status,
        Json(json!({"type": "error", "error": {"type": "x", "message": message}})),
    )
        .into_response()
}

fn token_of(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string()
}

fn signed_in(f: &mut Fake, id: i64, email: &str) -> Value {
    f.next += 1;
    let token = format!("sfs_test_{}", f.next);
    f.sessions.insert(token.clone(), id);
    json!({"token": token, "account": {"id": id, "email": email, "credits": 100}})
}

/// Starts the stand-in service → its address and state.
async fn fake_cloud() -> (String, Shared) {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route(
            "/v1/signup",
            post(|State(s): State<Shared>, Json(b): Json<Value>| async move {
                let email = b["email"].as_str().unwrap_or("").to_lowercase();
                let password = b["password"].as_str().unwrap_or("").to_string();
                if !email.contains('@') {
                    return error(StatusCode::BAD_REQUEST, "give a real email address");
                }
                if password.len() < 8 {
                    return error(StatusCode::BAD_REQUEST, "a password needs at least 8 characters");
                }
                let mut f = s.lock().unwrap();
                if f.accounts.contains_key(&email) {
                    return error(StatusCode::CONFLICT, "an account with that email already exists: sign in instead");
                }
                let id = 1000 + f.accounts.len() as i64;
                f.accounts.insert(email.clone(), (id, password));
                Json(signed_in(&mut f, id, &email)).into_response()
            }),
        )
        .route(
            "/v1/signin",
            post(|State(s): State<Shared>, Json(b): Json<Value>| async move {
                let email = b["email"].as_str().unwrap_or("").to_lowercase();
                let mut f = s.lock().unwrap();
                match f.accounts.get(&email).cloned() {
                    Some((id, p)) if b["password"] == p.as_str() => Json(signed_in(&mut f, id, &email)).into_response(),
                    _ => error(StatusCode::UNAUTHORIZED, "the email or the password is wrong"),
                }
            }),
        )
        .route(
            "/v1/signout",
            post(|State(s): State<Shared>, headers: HeaderMap| async move {
                s.lock().unwrap().sessions.remove(&token_of(&headers));
                Json(json!({}))
            }),
        )
        .route(
            "/v1/account",
            post(|State(s): State<Shared>, headers: HeaderMap| async move {
                let f = s.lock().unwrap();
                let Some(id) = f.sessions.get(&token_of(&headers)).copied() else {
                    return error(StatusCode::UNAUTHORIZED, "your sign-in has ended: sign in again");
                };
                let email = f.accounts.iter().find(|(_, (i, _))| *i == id).map(|(e, _)| e.clone()).unwrap();
                Json(json!({"id": id, "email": email, "credits": 100})).into_response()
            }),
        )
        .route(
            "/v1/password/forgot",
            post(|State(s): State<Shared>, Json(b): Json<Value>| async move {
                s.lock().unwrap().forgot.push(b["email"].as_str().unwrap_or("").to_string());
                Json(json!({"sent": true, "message": "If there's a Foundry Account with that email, a link to reset its password is on its way. It works for an hour."}))
            }),
        )
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), shared)
}

/// What the stand-in storage holds: key → (content type, bytes).
type Stored = Arc<Mutex<HashMap<String, (String, Vec<u8>)>>>;

/// A stand-in for R2's S3 API on a local port (path style: `/bucket/key`), checking each request
/// is signed → the storage MakerFoundry uses with it, and what it holds.
async fn fake_storage() -> (makerfoundry::storage::Storage, Stored) {
    use axum::extract::{Path, RawQuery};
    let stored: Stored = Arc::default();
    let signed = |q: &Option<String>| q.as_deref().unwrap_or("").contains("X-Amz-Signature=");
    let app = Router::new()
        .route(
            "/files/{*key}",
            axum::routing::put(
                move |State(s): State<Stored>,
                      Path(key): Path<String>,
                      RawQuery(q): RawQuery,
                      headers: HeaderMap,
                      body: axum::body::Bytes| async move {
                    if !signed(&q) {
                        return StatusCode::FORBIDDEN;
                    }
                    let ct = headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    s.lock().unwrap().insert(key, (ct, body.to_vec()));
                    StatusCode::OK
                },
            )
            .get(
                move |State(s): State<Stored>, Path(key): Path<String>, RawQuery(q): RawQuery| async move {
                    if !signed(&q) {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    match s.lock().unwrap().get(&key) {
                        Some((_, b)) => b.clone().into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            )
            .delete(
                move |State(s): State<Stored>, Path(key): Path<String>, RawQuery(q): RawQuery| async move {
                    if !signed(&q) {
                        return StatusCode::FORBIDDEN;
                    }
                    s.lock().unwrap().remove(&key);
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .with_state(stored.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let storage = makerfoundry::storage::Storage::new(&format!("http://{addr}"), "files", "key", "secret").unwrap();
    (storage, stored)
}

/// A database of this test's own (a fresh schema), its tables made; `None` without
/// `MF_TEST_DATABASE_URL`.
async fn test_db() -> Option<PgPool> {
    let url = std::env::var("MF_TEST_DATABASE_URL").ok()?;
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let schema = format!("t{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let admin = PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let opts: PgConnectOptions = url
        .parse::<PgConnectOptions>()
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .unwrap();
    makerfoundry::db::MIGRATOR.run(&pool).await.unwrap();
    Some(pool)
}

struct Site {
    app: Router,
    pool: PgPool,
    fake: Shared,
    stored: Stored,
    storage: Option<makerfoundry::storage::Storage>,
}

async fn site() -> Option<Site> {
    site_with(Vec::new()).await
}

/// The test site, with `admins` (account ids: the stand-in service numbers accounts from 1000).
async fn site_with(admins: Vec<i64>) -> Option<Site> {
    site_full(admins, None).await
}

/// The test site, with `admins` and Stripe.
async fn site_full(admins: Vec<i64>, stripe: Option<makerfoundry::stripe::Stripe>) -> Option<Site> {
    site_opts(admins, stripe, false).await
}

/// The test site, open to everyone (`launched`) or not.
async fn site_opts(admins: Vec<i64>, stripe: Option<makerfoundry::stripe::Stripe>, launched: bool) -> Option<Site> {
    site_all(admins, stripe, launched, None, false).await
}

/// The key the test sites make proof links with.
const PROOF_KEY: [u8; 32] = [9; 32];

/// The test site with approval links (MF-25), for everyone (`open`) or for `admins` only.
async fn site_with_proofs(admins: Vec<i64>, open: bool) -> Option<Site> {
    let proofs = makerfoundry::proofs::Proofs::new(&PROOF_KEY, open).unwrap();
    site_all(admins, None, false, Some(proofs), false).await
}

/// The test site, open to everyone, with MakerFoundry 3D (MF-35) switched on or not.
async fn site_3d(admins: Vec<i64>, three_d: bool) -> Option<Site> {
    site_all(admins, None, true, None, three_d).await
}

async fn site_all(
    admins: Vec<i64>,
    stripe: Option<makerfoundry::stripe::Stripe>,
    launched: bool,
    proofs: Option<makerfoundry::proofs::Proofs>,
    three_d: bool,
) -> Option<Site> {
    site_scanned(admins, stripe, launched, proofs, three_d, None, None).await
}

/// The same, with a virus scanner (issue #29) at `scanner`.
async fn site_scanned(
    admins: Vec<i64>,
    stripe: Option<makerfoundry::stripe::Stripe>,
    launched: bool,
    proofs: Option<makerfoundry::proofs::Proofs>,
    three_d: bool,
    scanner: Option<makerfoundry::scan::Scanner>,
    thingiverse: Option<makerfoundry::model3d::thingiverse::Thingiverse>,
) -> Option<Site> {
    let pool = test_db().await?;
    let (storage, stored) = fake_storage().await;
    let storage_kept = storage.clone();
    let (url, fake) = fake_cloud().await;
    let app = makerfoundry::app(AppState {
        cloud: Cloud::new(&url).unwrap(),
        db: Some(pool.clone()),
        storage: Some(storage),
        publishers: makerfoundry::listings::Publishers::Everyone,
        admins,
        limits: std::sync::Arc::default(),
        stripe,
        launched,
        public_url: "https://makerfoundry.test".into(),
        api_origins: vec!["https://stitchfoundry.test".into()],
        proofs,
        three_d,
        scanner,
        thingiverse,
        social: None,
        notices: None,
        step: None,
        scad: None,
        points: false,
        stitchhub: None,
    });
    Some(Site {
        app,
        pool,
        fake,
        stored,
        storage: Some(storage_kept),
    })
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("")
    }

    /// The session cookie this answer sets, as `name=value`.
    fn cookie(&self) -> String {
        let set = self
            .headers
            .get(header::SET_COOKIE)
            .expect("a cookie")
            .to_str()
            .unwrap();
        set.split(';').next().unwrap().to_string()
    }
}

impl Site {
    async fn send(&self, req: Request<Body>) -> Answer {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned();
        Answer { status, headers, body }
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Answer {
        let mut req = Request::get(path).header(header::HOST, HOST);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    /// A form posted from our own page (its `Origin`), with `cookie` if given.
    async fn post(&self, path: &str, form: &str, cookie: Option<&str>) -> Answer {
        let mut req = Request::post(path)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, format!("https://{HOST}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        self.send(req.body(Body::from(form.to_string())).unwrap()).await
    }

    async fn sessions(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM mf_sessions")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_person_makes_an_account_signs_out_and_back_in() {
    let Some(s) = site().await else { return };
    // the pages are there, signed out
    assert!(s.get("/signup", None).await.body.contains("Foundry Account"));
    assert_eq!(s.get("/account", None).await.location(), "/signin");
    // sign up → signed in, with a cookie only this site gets, over HTTPS, out of scripts' reach
    let r = s
        .post(
            "/signup",
            "email=Maker%40Example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await;
    assert_eq!(
        (r.status, r.location()),
        (StatusCode::SEE_OTHER, "/account"),
        "{}",
        r.body
    );
    let set = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    for part in [COOKIE, "Path=/", "HttpOnly", "Secure", "SameSite=Lax"] {
        assert!(set.contains(part), "{set}");
    }
    let cookie = r.cookie();
    let page = s.get("/account", Some(&cookie)).await;
    assert!(
        page.body.contains("maker@example.com") && page.body.contains("Sign out"),
        "{}",
        page.body
    );
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
    assert!(
        s.get("/", Some(&cookie))
            .await
            .body
            .contains(r#"href="/account">Account</a>"#)
    );
    // the account is kept under the service's id; the cookie's id is kept only as its hash
    let (id, email): (i64, String) = sqlx::query_as("SELECT account_id, email FROM mf_accounts")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!((id, email.as_str()), (1000, "maker@example.com"));
    let id_in_cookie = cookie.split_once('=').unwrap().1;
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_sessions WHERE id_hash = $1")
        .bind(id_in_cookie)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(kept, 0, "never the id itself");
    // sign out → the session ends here and at the service, and the cookie goes
    let r = s.post("/signout", "", Some(&cookie)).await;
    assert_eq!(r.location(), "/");
    assert!(r.headers[header::SET_COOKIE].to_str().unwrap().contains("Max-Age=0"));
    assert_eq!(s.sessions().await, 0);
    assert!(s.fake.lock().unwrap().sessions.is_empty());
    assert_eq!(s.get("/account", Some(&cookie)).await.location(), "/signin");
    // a wrong password says so and signs nobody in; the right one does
    let r = s
        .post("/signin", "email=maker%40example.com&password=wrong+one", None)
        .await;
    assert!(r.body.contains("The email or the password is wrong.") && !r.headers.contains_key(header::SET_COOKIE));
    assert!(
        r.body.contains(r#"value="maker@example.com""#),
        "the email stays filled in"
    );
    let r = s
        .post("/signin", "email=maker%40example.com&password=fire+truck+1", None)
        .await;
    assert_eq!(r.location(), "/account");
    // signed in already: the sign-in page goes to the account
    assert_eq!(s.get("/signin", Some(&r.cookie())).await.location(), "/account");
}

#[tokio::test]
async fn sign_up_problems_are_explained() {
    let Some(s) = site().await else { return };
    let r = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+2",
            None,
        )
        .await;
    assert!(r.body.contains("The two passwords aren&#39;t the same."));
    let r = s
        .post("/signup", "email=a%40b.c&password=short&again=short", None)
        .await;
    assert!(r.body.contains("A password needs at least 8 characters."), "{}", r.body);
    s.post(
        "/signup",
        "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
        None,
    )
    .await;
    let r = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await;
    assert!(r.body.contains("already a Foundry Account"));
    // what was typed comes back escaped
    let r = s
        .post("/signup", "email=%22%3E%3Cscript%3E&password=x&again=y", None)
        .await;
    assert!(r.body.contains("&quot;&gt;&lt;script&gt;") && !r.body.contains("\"><script>"));
}

#[tokio::test]
async fn a_session_ended_at_the_account_service_ends_here() {
    let Some(s) = site().await else { return };
    let r = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await;
    let cookie = r.cookie();
    // a password reset there ends every session of the account
    s.fake.lock().unwrap().sessions.clear();
    assert!(
        s.get("/account", Some(&cookie)).await.body.contains("a@b.c"),
        "until the next check"
    );
    sqlx::query("UPDATE mf_sessions SET checked_at = now() - interval '1 hour'")
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(s.get("/account", Some(&cookie)).await.location(), "/signin");
    assert_eq!(s.sessions().await, 0);
}

#[tokio::test]
async fn sessions_end_when_unused_and_carry_on_while_checks_pass() {
    let Some(s) = site().await else { return };
    let cookie = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    sqlx::query("UPDATE mf_sessions SET checked_at = now() - interval '1 hour'")
        .execute(&s.pool)
        .await
        .unwrap();
    assert!(
        s.get("/account", Some(&cookie)).await.body.contains("a@b.c"),
        "checked and still good"
    );
    let fresh: bool = sqlx::query_scalar("SELECT checked_at > now() - interval '1 minute' FROM mf_sessions")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert!(fresh);
    sqlx::query("UPDATE mf_sessions SET last_seen_at = now() - interval '31 days'")
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        s.get("/account", Some(&cookie)).await.location(),
        "/signin",
        "30 days unused"
    );
}

#[tokio::test]
async fn a_forgotten_password_goes_to_the_account_service() {
    let Some(s) = site().await else { return };
    let r = s.post("/forgot", "email=+maker%40example.com+", None).await;
    assert!(
        r.body.contains("Check your email") && r.body.contains("works for an hour"),
        "{}",
        r.body
    );
    assert_eq!(s.fake.lock().unwrap().forgot, ["maker@example.com"]);
}

#[tokio::test]
async fn forms_from_other_sites_are_refused() {
    let Some(s) = site().await else { return };
    let form = "email=a%40b.c&password=fire+truck+1&again=fire+truck+1";
    let req = |origin: Option<&str>| {
        let mut r = Request::post("/signup")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(o) = origin {
            r = r.header(header::ORIGIN, o);
        }
        r.body(Body::from(form)).unwrap()
    };
    assert_eq!(
        s.send(req(Some("https://evil.example"))).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(s.send(req(None)).await.status, StatusCode::FORBIDDEN);
    assert!(
        s.fake.lock().unwrap().accounts.is_empty(),
        "nothing reached the service"
    );
}

#[tokio::test]
async fn without_the_account_service_people_are_told_to_try_later() {
    let Some(pool) = test_db().await else { return };
    let app = makerfoundry::app(AppState {
        cloud: Cloud::new("http://127.0.0.1:9").unwrap(),
        db: Some(pool),
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
    });
    let s = Site {
        stored: Arc::default(),
        app,
        pool: test_db().await.unwrap(),
        fake: Arc::default(),
        storage: None,
    };
    let r = s.post("/signin", "email=a%40b.c&password=fire+truck+1", None).await;
    assert!(r.body.contains("can&#39;t be reached right now"), "{}", r.body);
}

#[tokio::test]
async fn without_a_database_nobody_can_sign_in() {
    let app = makerfoundry::app(AppState {
        cloud: Cloud::new("http://127.0.0.1:9").unwrap(),
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
    });
    let req = Request::get("/signin")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(body.contains("isn&#39;t available yet"));
}

/// A small two-colour design as `format`, written by StitchFoundry's own writer.
fn sample(format: sf_formats::Format) -> Vec<u8> {
    use sf_core::{Cmd, Pattern, Point, Rgb, Thread};
    let mut p = Pattern::new();
    p.threads = vec![
        Thread {
            color: Rgb { r: 200, g: 30, b: 40 },
            name: "Rose <red>".into(),
            code: String::new(),
            chart: String::new(),
        },
        Thread {
            color: Rgb { r: 20, g: 60, b: 200 },
            name: String::new(),
            code: String::new(),
            chart: String::new(),
        },
    ];
    for i in 0..=30 {
        p.add(Point::new(f64::from(i), f64::from(i % 2) * 4.0), Cmd::Stitch);
    }
    p.add(Point::new(30.0, 0.0), Cmd::ColorChange);
    for i in 0..=30 {
        p.add(Point::new(f64::from(i), 10.0 + f64::from(i % 2) * 4.0), Cmd::Stitch);
    }
    p.add(Point::new(30.0, 14.0), Cmd::End);
    sf_formats::write(format, &p, &sf_formats::WriteOptions::default()).unwrap()
}

/// A multipart upload of `bytes` as the form's `file`, named `name`.
fn upload(name: &str, bytes: &[u8], cookie: Option<&str>, origin: &str) -> Request<Body> {
    let boundary = "----mf-test-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let mut req = Request::post("/preview")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, origin)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        );
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    req.body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn an_uploaded_pes_shows_its_stitches() {
    use sf_formats::Format::{Pes, Vp3};
    let Some(s) = site().await else { return };
    let ours = format!("https://{HOST}");
    // signed out: asked to sign in, and nothing is read
    assert!(s.get("/preview", None).await.body.contains("Sign in to upload"));
    assert!(
        s.send(upload("rose.pes", &sample(Pes), None, &ours))
            .await
            .body
            .contains("Sign in to upload")
    );
    let cookie = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    assert!(
        s.get("/preview", Some(&cookie))
            .await
            .body
            .contains(r#"enctype="multipart/form-data""#)
    );
    // the done-when of MF-2: an uploaded PES shows its stitches
    let r = s.send(upload("rose.pes", &sample(Pes), Some(&cookie), &ours)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body.contains(r#"<svg class="stitches""#) && r.body.matches("<path stroke=").count() == 2,
        "{}",
        r.body
    );
    assert!(
        r.body.contains("<dd>PES</dd>") && r.body.contains("30.0 × 14.0 mm"),
        "{}",
        r.body
    );
    assert!(
        r.body.contains("Red 5 (#ed171f)"),
        "PES colours are Brother's thread chart: {}",
        r.body
    );
    // VP3 keeps the file's own thread names, which come back escaped
    let r = s.send(upload("rose.vp3", &sample(Vp3), Some(&cookie), &ours)).await;
    assert!(
        r.body.contains("Rose &lt;red&gt; (#c81e28)") && !r.body.contains("<red>"),
        "{}",
        r.body
    );
    // not a design, or too big: a reason, no preview
    let r = s
        .send(upload(
            "evil.pes",
            b"MZ\x90\x00 this is not a design <b>",
            Some(&cookie),
            &ours,
        ))
        .await;
    assert!(
        r.body.contains("isn&#39;t an embroidery file") && !r.body.contains("<b>"),
        "{}",
        r.body
    );
    let big = vec![0u8; makerfoundry::laser::MAX_FILE_BYTES + 200 * 1024];
    let r = s.send(upload("big.dst", &big, Some(&cookie), &ours)).await;
    assert!(
        r.status == StatusCode::PAYLOAD_TOO_LARGE || r.body.contains("up to 8 MB"),
        "{}: {}",
        r.status,
        r.body
    );
    // from another site: refused
    assert_eq!(
        s.send(upload("rose.pes", &sample(Pes), Some(&cookie), "https://evil.example"))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn uploaded_laser_files_show_their_lines() {
    let Some(s) = site().await else { return };
    let ours = format!("https://{HOST}");
    let cookie = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    // an SVG in mm with a script in it: drawn from its shapes, the script nowhere
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="60mm" height="40mm" viewBox="0 0 60 40" onload="alert(1)"><script>alert(2)</script><rect x="5" y="5" width="50" height="30" fill="none" stroke="#ff0000"/><text x="10" y="20">Name</text></svg>"##;
    let r = s.send(upload("coaster.svg", svg, Some(&cookie), &ours)).await;
    assert!(
        r.body.contains("<dd>SVG</dd>") && r.body.contains("50.0 × 30.0 mm") && r.body.contains("160 mm"),
        "{}",
        r.body
    );
    assert!(r.body.contains("Not drawn: text"), "{}", r.body);
    assert!(!r.body.contains("alert"), "{}", r.body);
    // a DXF in inches
    let dxf = "0\nSECTION\n2\nHEADER\n9\n$INSUNITS\n70\n1\n0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nCIRCLE\n62\n1\n10\n0\n20\n0\n40\n1\n0\nENDSEC\n0\nEOF\n";
    let r = s.send(upload("disc.dxf", dxf.as_bytes(), Some(&cookie), &ours)).await;
    assert!(
        r.body.contains("<dd>DXF</dd>") && r.body.contains("50.8 × 50.8 mm"),
        "{}",
        r.body
    );
}

#[tokio::test]
async fn an_uploaded_pdf_is_read_as_a_laser_file() {
    let Some(s) = site().await else { return };
    let ours = format!("https://{HOST}");
    let cookie = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let r = s
        .send(upload("broken.pdf", b"%PDF-1.7\nnot a real pdf", Some(&cookie), &ours))
        .await;
    assert!(r.body.contains("PDF couldn&#39;t be read"), "{}", r.body);
}

/// A multipart form of `(field, file name, bytes)` parts, posted to `path`.
fn multipart(path: &str, parts: &[(&str, &str, Vec<u8>)], cookie: &str) -> Request<Body> {
    let boundary = "----mf-test-parts";
    let mut body = Vec::new();
    for (field, name, bytes) in parts {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::post(path)
        .header(header::HOST, HOST)
        .header(header::ORIGIN, format!("https://{HOST}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::COOKIE, cookie)
        .body(Body::from(body))
        .unwrap()
}

/// A small JPEG with an EXIF block naming a place, as phones write them.
fn photo_with_location() -> Vec<u8> {
    let picture = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(800, 600, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    }));
    let mut out = std::io::Cursor::new(Vec::new());
    picture.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    let mut bytes = out.into_inner();
    bytes.splice(2..2, b"\xff\xe1\x00\x18Exif\x00\x00GPSLatitude=41.8".iter().copied());
    bytes
}

#[tokio::test]
async fn a_maker_keeps_a_design_with_photos() {
    let Some(s) = site().await else { return };
    let cookie = s
        .post(
            "/signup",
            "email=maker%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    assert!(
        s.get("/designs", Some(&cookie))
            .await
            .body
            .contains("haven't added a design")
    );
    assert!(
        s.get("/designs/new", Some(&cookie))
            .await
            .body
            .contains(r#"name="photos""#)
    );
    // a design and two photos
    let parts = [
        ("file", "rose.pes", sample(sf_formats::Format::Pes)),
        ("photos", "sewn.jpg", photo_with_location()),
        ("photos", "close-up.jpg", photo_with_location()),
    ];
    let r = s.send(multipart("/designs/new", &parts, &cookie)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let page_path = r.location().to_string();
    assert!(page_path.starts_with("/designs/"), "{page_path}");
    // the design file, two photos and two thumbnails are stored under keys of our own
    {
        let stored = s.stored.lock().unwrap();
        assert_eq!(stored.len(), 5, "{:?}", stored.keys().collect::<Vec<_>>());
        assert!(
            stored.keys().all(|k| !k.contains("rose") && !k.contains("sewn")),
            "no uploader names in keys"
        );
        for (key, (ct, bytes)) in stored.iter() {
            if key.starts_with("photo/") || key.starts_with("thumb/") {
                assert_eq!(ct, "image/jpeg");
                assert!(
                    !bytes.windows(11).any(|w| w == b"GPSLatitude"),
                    "where it was taken is gone"
                );
            }
        }
    }
    // its page: the stitches, the facts, the photos; and it's in the list
    let page = s.get(&page_path, Some(&cookie)).await;
    assert!(
        page.body.contains(r#"<svg class="stitches""#) && page.body.contains("<dd>PES</dd>"),
        "{}",
        page.body
    );
    assert_eq!(page.body.matches("/thumb\"").count(), 2, "{}", page.body);
    assert!(s.get("/designs", Some(&cookie)).await.body.contains("rose.pes"));
    // a photo comes back as a JPEG; the download goes to a signed link that names the file
    let thumb_path = page
        .body
        .split("src=\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap()
        .to_string();
    let thumb = s.get(&thumb_path, Some(&cookie)).await;
    assert_eq!(thumb.headers[header::CONTENT_TYPE], "image/jpeg");
    let dl = s.get(&format!("{page_path}/download"), Some(&cookie)).await;
    assert_eq!(dl.status, StatusCode::SEE_OTHER);
    assert!(
        dl.location().contains("X-Amz-Signature=") && dl.location().contains("rose.pes"),
        "{}",
        dl.location()
    );
    // another maker sees none of it
    let other = s
        .post(
            "/signup",
            "email=other%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    assert_eq!(s.get(&page_path, Some(&other)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get(&thumb_path, Some(&other)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        s.get(&format!("{page_path}/download"), Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.post(&format!("{page_path}/delete"), "", Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.get(&page_path, None).await.location(),
        format!("/signin?next={page_path}")
    );
    // deleting it takes its files too
    let r = s.post(&format!("{page_path}/delete"), "", Some(&cookie)).await;
    assert_eq!(r.location(), "/designs");
    assert!(s.stored.lock().unwrap().is_empty());
    assert_eq!(s.get(&page_path, Some(&cookie)).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_photos_and_too_many_are_refused_and_nothing_is_kept() {
    let Some(s) = site().await else { return };
    let cookie = s
        .post(
            "/signup",
            "email=maker%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let design = ("file", "rose.pes", sample(sf_formats::Format::Pes));
    let r = s
        .send(multipart(
            "/designs/new",
            &[design.clone(), ("photos", "x.jpg", b"<svg onload=alert(1)>".to_vec())],
            &cookie,
        ))
        .await;
    assert!(
        r.body.contains("Photo 1: this isn&#39;t a JPEG, PNG or WebP photo."),
        "{}",
        r.body
    );
    let mut parts = vec![design.clone()];
    for i in 0..=makerfoundry::designs::MAX_PHOTOS {
        parts.push(("photos", if i == 0 { "a.jpg" } else { "b.jpg" }, photo_with_location()));
    }
    let r = s.send(multipart("/designs/new", &parts, &cookie)).await;
    assert!(r.body.contains("up to 6 photos"), "{}", r.body);
    let r = s
        .send(multipart(
            "/designs/new",
            &[("file", "notes.txt", b"hello".to_vec())],
            &cookie,
        ))
        .await;
    assert!(
        r.body.contains("notes.txt: this isn&#39;t an embroidery file"),
        "{}",
        r.body
    );
    assert!(s.stored.lock().unwrap().is_empty());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_designs")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn without_storage_designs_can_only_be_previewed() {
    let Some(pool) = test_db().await else { return };
    let (url, _fake) = fake_cloud().await;
    let app = makerfoundry::app(AppState {
        cloud: Cloud::new(&url).unwrap(),
        db: Some(pool.clone()),
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
    });
    let s = Site {
        app,
        pool,
        fake: Arc::default(),
        stored: Arc::default(),
        storage: None,
    };
    let cookie = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let r = s.get("/designs/new", Some(&cookie)).await;
    assert!(
        r.body.contains("isn't switched on yet") && r.body.contains("/preview"),
        "{}",
        r.body
    );
}

/// The id in a `/designs/<id>` address.
fn design_id(path: &str) -> i64 {
    path.trim_start_matches("/designs/").parse().unwrap()
}

/// Adds a design as the maker signed in with `cookie` → its id.
async fn add_design(s: &Site, cookie: &str, format: sf_formats::Format, name: &str) -> i64 {
    let parts = [
        ("file", name, sample(format)),
        ("photos", "sewn.jpg", photo_with_location()),
    ];
    let r = s.send(multipart("/designs/new", &parts, cookie)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    design_id(r.location())
}

/// #86: a Windows program is never taken as a design, whatever it's named, and a design's file is
/// only ever kept and downloaded under its format's own ending, never the one it was uploaded with.
#[tokio::test]
async fn a_program_is_refused_and_files_download_under_their_formats_ending() {
    let Some(s) = site().await else { return };
    let cookie = s.sign_up("ann%40example.com").await;
    let mut program = b"MZ".to_vec();
    program.extend(std::iter::repeat_n(0x90u8, 4094));
    for (name, bytes) in [
        ("setup.exe", program.clone()),
        ("setup.exp", program.clone()),
        ("setup.EXP", program.clone()),
        ("rose.exe", sample(sf_formats::Format::Exp)),
        ("rose.dll", sample(sf_formats::Format::Dst)),
        ("page.html", sample(sf_formats::Format::Pes)),
    ] {
        let r = s
            .send(multipart("/designs/new", &[("file", name, bytes)], &cookie))
            .await;
        assert_ne!(r.status, StatusCode::SEE_OTHER, "{name}");
        assert!(
            r.body.contains("this isn&#39;t an embroidery file"),
            "{name}: {}",
            r.body
        );
    }
    assert!(s.stored.lock().unwrap().is_empty());
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 0);
    // a real EXP is taken, kept as .exp
    let exp = add_design(&s, &cookie, sf_formats::Format::Exp, "tulip.EXP").await;
    // a DST named .pes is kept, and downloaded, as the DST it is
    let dst = add_design(&s, &cookie, sf_formats::Format::Dst, "rose.pes").await;
    for (id, kept) in [(exp, "tulip.exp"), (dst, "rose.dst")] {
        let name: String = sqlx::query_scalar("SELECT file_name FROM mf_designs WHERE id = $1")
            .bind(id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(name, kept);
        for path in [format!("/designs/{id}/download"), format!("/d/{id}/download")] {
            let dl = s.get(&path, Some(&cookie)).await;
            assert_eq!(dl.status, StatusCode::SEE_OTHER, "{path}");
            assert!(
                dl.location().contains(kept) && !dl.location().contains("rose.pes"),
                "{path}: {}",
                dl.location()
            );
        }
    }
    // a file kept before #86 under a program's ending still downloads under its format's
    sqlx::query("UPDATE mf_designs SET file_name = 'setup.exe' WHERE id = $1")
        .bind(exp)
        .execute(&s.pool)
        .await
        .unwrap();
    for path in [format!("/designs/{exp}/download"), format!("/d/{exp}/download")] {
        let dl = s.get(&path, Some(&cookie)).await;
        assert!(
            dl.location().contains("setup.exp") && !dl.location().contains(".exe"),
            "{path}: {}",
            dl.location()
        );
    }
}

#[tokio::test]
async fn a_free_design_is_published_found_and_downloaded() {
    let Some(s) = site().await else { return };
    let cookie = s
        .post(
            "/signup",
            "email=ann%40example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let id = add_design(&s, &cookie, sf_formats::Format::Pes, "rose.pes").await;
    let manage = format!("/designs/{id}");
    // publishing needs a profile, details, and the maker's word that it's theirs to share
    let r = s.post(&format!("{manage}/publish"), "confirm=yes", Some(&cookie)).await;
    assert!(r.body.contains("maker profile first"), "{}", r.body);
    let r = s
        .post(
            "/account/maker",
            "name=Ann%27s+Stitches&handle=Ann+S&bio=",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("lower-case"), "{}", r.body);
    let r = s
        .post(
            "/account/maker",
            "name=Ann%27s+Stitches&handle=ann-s&bio=Hoops+%26+more",
            Some(&cookie),
        )
        .await;
    assert_eq!(r.location(), "/account");
    let r = s.post(&format!("{manage}/publish"), "confirm=yes", Some(&cookie)).await;
    assert!(r.body.contains("title and a licence first"), "{}", r.body);
    let r = s.post(&format!("{manage}/edit"), "title=Rose+Bouquet&description=Roses+for+a+4x4+hoop.%0A%0AStitch+on+cotton.&tags=Flowers%2C+Roses%2C+flowers&licence=nope", Some(&cookie)).await;
    assert!(r.body.contains("Choose a licence."), "{}", r.body);
    let r = s.post(&format!("{manage}/edit"), "title=Rose+Bouquet&description=Roses+for+a+4x4+hoop.%0A%0AStitch+on+cotton.&tags=Flowers%2C+Roses%2C+flowers&licence=personal", Some(&cookie)).await;
    assert_eq!(r.location(), manage);
    // before publishing, nobody else can see it
    assert_eq!(s.get(&format!("/d/{id}"), None).await.status, StatusCode::NOT_FOUND);
    let r = s.post(&format!("{manage}/publish"), "", Some(&cookie)).await;
    assert!(r.body.contains("yours to share"), "{}", r.body);
    let r = s.post(&format!("{manage}/publish"), "confirm=yes", Some(&cookie)).await;
    assert_eq!(r.location(), format!("/d/{id}/rose-bouquet"));
    // its public page, for anyone: title, maker, description, tags, licence, photo; never an email
    let page = s.get(&format!("/d/{id}/rose-bouquet"), None).await;
    assert_eq!(page.status, StatusCode::OK);
    for want in [
        "<h1>Rose Bouquet</h1>",
        r#"href="/makers/ann-s">Ann&#39;s Stitches</a>"#,
        "<p>Roses for a 4x4 hoop.</p>",
        "/browse?tag=flowers",
        "Personal use",
        "Don&#39;t share",
        "/thumb\"",
        "Download free",
    ] {
        assert!(page.body.contains(want), "{want}: {}", page.body);
    }
    assert!(!page.body.contains("ann@example.com"));
    // found by words (stemmed: "rose" finds "Roses"), by tag, by kind; not by the wrong kind
    for query in [
        "/browse",
        "/browse?q=rose",
        "/browse?q=bouquet+cotton",
        "/browse?tag=roses",
        "/browse?kind=embroidery",
        "/browse?sort=popular",
    ] {
        assert!(s.get(query, None).await.body.contains("Rose Bouquet"), "{query}");
    }
    for query in ["/browse?q=tulip", "/browse?kind=laser", "/browse?tag=tulips"] {
        assert!(!s.get(query, None).await.body.contains("Rose Bouquet"), "{query}");
    }
    // on its maker's page and the home page
    let maker = s.get("/makers/ann-s", None).await;
    assert!(
        maker.body.contains("Ann&#39;s Stitches")
            && maker.body.contains("Hoops &amp; more")
            && maker.body.contains("Rose Bouquet")
    );
    assert!(s.get("/", None).await.body.contains("Newest designs"));
    // downloaded by anyone, counted, saved under its title
    let dl = s.get(&format!("/d/{id}/download"), None).await;
    assert_eq!(dl.status, StatusCode::SEE_OTHER);
    assert!(
        dl.location().contains("rose-bouquet.pes") && dl.location().contains("X-Amz-Signature="),
        "{}",
        dl.location()
    );
    assert!(s.get(&format!("/d/{id}"), None).await.body.contains("1 download"));
    // a published design's photos are public; the maker's own download isn't counted
    let thumb = page
        .body
        .split("src=\"")
        .find(|p| p.starts_with("/designs/"))
        .and_then(|p| p.split('"').next())
        .unwrap()
        .to_string();
    assert_eq!(
        s.get(&thumb, None).await.headers[header::CACHE_CONTROL],
        "public, max-age=3600"
    );
    s.get(&format!("{manage}/download"), Some(&cookie)).await;
    let n: i64 = sqlx::query_scalar("SELECT downloads FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // a published design isn't deleted by mistake; unpublished, it's hidden again
    assert!(
        s.post(&format!("{manage}/delete"), "", Some(&cookie))
            .await
            .body
            .contains("Unpublish the design before deleting it.")
    );
    s.post(&format!("{manage}/unpublish"), "", Some(&cookie)).await;
    assert_eq!(s.get(&format!("/d/{id}"), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get(&thumb, None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        s.get(&format!("/d/{id}/download"), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!s.get("/browse", None).await.body.contains("Rose Bouquet"));
    // the maker still sees it, marked a draft
    assert!(
        s.get(&format!("/d/{id}"), Some(&cookie))
            .await
            .body
            .contains("only you can see this page")
    );
}

#[tokio::test]
async fn others_cant_change_a_design_or_take_a_maker_address() {
    let Some(s) = site().await else { return };
    let ann = s
        .post(
            "/signup",
            "email=ann%40example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let bob = s
        .post(
            "/signup",
            "email=bob%40example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let id = add_design(&s, &ann, sf_formats::Format::Dst, "star.dst").await;
    s.post("/account/maker", "name=Ann&handle=ann-s", Some(&ann)).await;
    let r = s.post("/account/maker", "name=Bob&handle=ann-s", Some(&bob)).await;
    assert!(r.body.contains("That address is taken"), "{}", r.body);
    for (path, form) in [
        ("edit", "title=Mine&licence=cc0"),
        ("publish", "confirm=yes"),
        ("unpublish", ""),
    ] {
        let r = s.post(&format!("/designs/{id}/{path}"), form, Some(&bob)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path}");
    }
    let title: String = sqlx::query_scalar("SELECT title FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(title, "");
}

#[tokio::test]
async fn publishing_waits_for_the_launch_unless_allowed() {
    let Some(pool) = test_db().await else { return };
    let (url, fake) = fake_cloud().await;
    let (storage, stored) = fake_storage().await;
    let app = makerfoundry::app(AppState {
        cloud: Cloud::new(&url).unwrap(),
        db: Some(pool.clone()),
        storage: Some(storage),
        publishers: makerfoundry::listings::Publishers::Only(vec![]),
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
    });
    let s = Site {
        app,
        pool,
        fake,
        stored,
        storage: None,
    };
    let cookie = s
        .post(
            "/signup",
            "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let id = add_design(&s, &cookie, sf_formats::Format::Pes, "rose.pes").await;
    s.post("/account/maker", "name=Ann&handle=ann-s", Some(&cookie)).await;
    s.post(&format!("/designs/{id}/edit"), "title=Rose&licence=cc0", Some(&cookie))
        .await;
    let r = s
        .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert!(
        r.body.contains("Publishing opens when MakerFoundry launches"),
        "{}",
        r.body
    );
    assert_eq!(s.get(&format!("/d/{id}"), None).await.status, StatusCode::NOT_FOUND);
}

/// A published design by a maker signed in with `cookie` → its id.
async fn published(s: &Site, cookie: &str, handle: &str, title: &str) -> i64 {
    let id = add_design(s, cookie, sf_formats::Format::Pes, "logo.pes").await;
    s.post("/account/maker", &format!("name=Maker&handle={handle}"), Some(cookie))
        .await;
    s.post(
        &format!("/designs/{id}/edit"),
        &format!("title={title}&licence=commercial"),
        Some(cookie),
    )
    .await;
    let r = s
        .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(cookie))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    id
}

#[tokio::test]
async fn a_takedown_is_done_in_minutes() {
    // the first account made is the admin
    let Some(s) = site_with(vec![1000]).await else { return };
    let admin = s
        .post(
            "/signup",
            "email=admin%40example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let maker = s
        .post(
            "/signup",
            "email=maker%40example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let id = published(&s, &maker, "copycat", "Famous+Mouse").await;
    // anyone can report it, from its page
    let page = s.get(&format!("/d/{id}"), None).await;
    assert!(page.body.contains(&format!("/d/{id}/report")), "{}", page.body);
    let r = s
        .post(
            &format!("/d/{id}/report"),
            "reason=trademark&details=That+is+a+famous+cartoon+mouse.",
            None,
        )
        .await;
    assert!(r.body.contains("Your report is in"), "{}", r.body);
    assert!(
        s.post(&format!("/d/{id}/report"), "reason=nope", None)
            .await
            .body
            .contains("Choose what the problem is")
    );
    // and its copyright owner sends a notice, every element the law asks for
    let notice = format!(
        "name=Rights+Holder&email=legal%40example.com&address=1+Main+St%0ASpringfield&phone=555-0100&original=Our+mouse+character&designs=https%3A%2F%2Fmakerfoundry.app%2Fd%2F{id}%2Ffamous-mouse&good_faith=yes&accurate=yes&signature=Rights+Holder"
    );
    assert!(
        s.post("/dmca/notice", &notice.replace("&accurate=yes", ""), None)
            .await
            .body
            .contains("Both statements are needed")
    );
    assert!(
        s.post("/dmca/notice", &notice.replace(&format!("%2Fd%2F{id}"), "%2Fx"), None)
            .await
            .body
            .contains("at least one design")
    );
    assert!(
        s.post("/dmca/notice", &notice, None)
            .await
            .body
            .contains("Notice received")
    );
    // the queue is the admins' alone
    assert_eq!(s.get("/admin", Some(&maker)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get("/admin", None).await.status, StatusCode::NOT_FOUND);
    let queue = s.get("/admin", Some(&admin)).await;
    assert!(
        queue.body.contains("A copyright notice (DMCA)")
            && queue.body.contains("Rights Holder")
            && queue.body.contains("famous cartoon mouse"),
        "{}",
        queue.body
    );
    // one click: down
    let rid = queue
        .body
        .split("/admin/reports/")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap()
        .to_string();
    assert_eq!(
        s.post(&format!("/admin/reports/{rid}/takedown"), "", Some(&maker))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "not by the maker"
    );
    let r = s
        .post(&format!("/admin/reports/{rid}/takedown"), "", Some(&admin))
        .await;
    assert_eq!(r.location(), "/admin");
    // gone from every public place, both reports answered, the action logged
    assert_eq!(s.get(&format!("/d/{id}"), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        s.get(&format!("/d/{id}/download"), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!s.get("/browse", None).await.body.contains("Famous Mouse"));
    assert!(!s.get("/makers/copycat", None).await.body.contains("Famous Mouse"));
    assert!(s.get("/admin", Some(&admin)).await.body.contains("Nothing waiting"));
    let logged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mf_moderation_log WHERE action = 'takedown' AND design_id = $1")
            .bind(id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(logged, 1);
    // the maker sees why, can't publish it again, and may send a counter-notice
    let manage = s.get(&format!("/designs/{id}"), Some(&maker)).await;
    assert!(
        manage.body.contains("Taken down after a report") && manage.body.contains("counter-notice"),
        "{}",
        manage.body
    );
    let r = s
        .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&maker))
        .await;
    assert!(
        r.body.contains("can&#39;t be published again") || r.body.contains("can't be published again"),
        "{}",
        r.body
    );
    s.post(&format!("/designs/{id}/unpublish"), "", Some(&maker)).await;
    let status: String = sqlx::query_scalar("SELECT status FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(status, "removed", "unpublishing doesn't undo a takedown");
    let counter = "name=Maker+Person&address=2+Elm+St&phone=555-0101&email=maker%40example.com&mistake=yes&consent=yes&signature=Maker+Person";
    assert!(
        s.post(
            &format!("/designs/{id}/counter"),
            &counter.replace("&consent=yes", ""),
            Some(&maker)
        )
        .await
        .body
        .contains("Both statements")
    );
    assert!(
        s.post(&format!("/designs/{id}/counter"), counter, Some(&maker))
            .await
            .body
            .contains("Counter-notice received")
    );
    // after the waiting time, the admin restores it
    let queue = s.get("/admin", Some(&admin)).await;
    assert!(
        queue.body.contains("A counter-notice from the maker") && queue.body.contains("Restore the design"),
        "{}",
        queue.body
    );
    let rid = queue
        .body
        .split("/admin/reports/")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap()
        .to_string();
    s.post(&format!("/admin/reports/{rid}/restore"), "", Some(&admin)).await;
    assert_eq!(s.get(&format!("/d/{id}"), None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn reports_and_sign_ups_are_rate_limited() {
    let Some(s) = site().await else { return };
    let maker = s
        .post(
            "/signup",
            "email=maker%40example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await
        .cookie();
    let id = published(&s, &maker, "maker-one", "Rose").await;
    for _ in 0..makerfoundry::limits::REPORT.max {
        assert!(
            s.post(&format!("/d/{id}/report"), "reason=spam", None)
                .await
                .body
                .contains("Your report is in")
        );
    }
    let r = s.post(&format!("/d/{id}/report"), "reason=spam", None).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    // one sign-up made above; the rest of the hour's allowance, then no more
    for i in 1..makerfoundry::limits::SIGNUP.max {
        s.post(
            "/signup",
            &format!("email=p{i}%40example.com&password=fire+truck+1&again=fire+truck+1"),
            None,
        )
        .await;
    }
    let r = s
        .post(
            "/signup",
            "email=late%40example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await;
    assert!(r.body.contains("Too many tries"), "{}", r.body);
    assert!(!s.fake.lock().unwrap().accounts.contains_key("late@example.com"));
}

#[tokio::test]
async fn the_copyright_page_says_how_to_send_a_notice() {
    let Some(s) = site().await else { return };
    let page = s.get("/dmca", None).await;
    assert!(
        page.body.contains("512(c)(3)")
            && page.body.contains("/dmca/notice")
            && page.body.contains("repeatedly infringe")
    );
    assert!(
        s.get("/", None).await.body.contains(r#"href="/dmca""#),
        "linked from every page"
    );
}

#[tokio::test]
async fn sessions_past_their_days_are_forgotten() {
    let Some(s) = site().await else { return };
    s.post(
        "/signup",
        "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
        None,
    )
    .await;
    s.post(
        "/signup",
        "email=d%40e.f&password=fire+truck+1&again=fire+truck+1",
        None,
    )
    .await;
    sqlx::query("UPDATE mf_sessions SET last_seen_at = now() - interval '31 days' WHERE account_id = 1000")
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(makerfoundry::db::forget_old_sessions(&s.pool).await.unwrap(), 1);
    assert_eq!(s.sessions().await, 1, "the recent one stays");
}

/// After a breach: sessions made before the time end, later ones go on.
#[tokio::test]
async fn sessions_made_before_a_time_end() {
    assert_eq!(
        makerfoundry::db::parse_time("2026-10-22T09:00:00Z"),
        Some(1_792_659_600)
    );
    assert_eq!(makerfoundry::db::parse_time("2026-10-22"), Some(1_792_627_200));
    assert_eq!(makerfoundry::db::parse_time("soon"), None);
    let Some(s) = site().await else { return };
    s.post(
        "/signup",
        "email=a%40b.c&password=fire+truck+1&again=fire+truck+1",
        None,
    )
    .await;
    s.post(
        "/signup",
        "email=d%40e.f&password=fire+truck+1&again=fire+truck+1",
        None,
    )
    .await;
    sqlx::query("UPDATE mf_sessions SET created_at = now() - interval '2 days' WHERE account_id = 1000")
        .execute(&s.pool)
        .await
        .unwrap();
    let yesterday = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 86_400;
    assert_eq!(
        makerfoundry::db::end_sessions_before(&s.pool, yesterday).await.unwrap(),
        1
    );
    assert_eq!(s.sessions().await, 1, "the newer one stays");
}

// ---------------------------------------------------------------- MF-4: selling

const STRIPE_KEY: &str = "sk_test_fake";
const WEBHOOK_SECRET: &str = "whsec_fake";

/// The stand-in Stripe's state.
#[derive(Default)]
struct StripeFake {
    /// Express account → whether Stripe lets it take payments.
    accounts: HashMap<String, bool>,
    /// Idempotency key → the account made with it.
    made: HashMap<String, String>,
    /// Checkout Session → (its form, paid).
    sessions: HashMap<String, (HashMap<String, String>, bool)>,
    /// Every form posted, by path.
    posts: Vec<(String, HashMap<String, String>)>,
    refunds: Vec<HashMap<String, String>>,
    /// Application fee refunds: (the fee, the form).
    fee_refunds: Vec<(String, HashMap<String, String>)>,
    /// Whether Stripe answers application fee refunds with an error.
    fee_refunds_fail: bool,
    /// Transfer reversals: (the transfer, the form).
    reversals: Vec<(String, HashMap<String, String>)>,
    /// Whether Stripe answers transfer reversals with an error.
    reversals_fail: bool,
    /// Whether reading a Checkout Session fails (Stripe having a bad moment).
    sessions_unreadable: bool,
    /// Checkout Sessions expired (`/v1/checkout/sessions/{id}/expire`, or by time).
    expired: Vec<String>,
    /// Transfers made (`/v1/transfers`): their forms.
    transfers: Vec<HashMap<String, String>>,
    /// Refunds made outside MakerFoundry (the Stripe Dashboard), as Stripe lists them.
    listed_refunds: Vec<Value>,
}

type StripeShared = Arc<Mutex<StripeFake>>;

/// A stand-in for Stripe's API (the few calls MakerFoundry makes, answered as
/// https://docs.stripe.com/api says) → MakerFoundry's client for it, and its state.
async fn fake_stripe() -> (makerfoundry::stripe::Stripe, StripeShared) {
    use axum::Form;
    use axum::extract::Path;
    use axum::extract::Query;
    use axum::routing::get;
    let shared: StripeShared = Arc::default();
    let authorised = |h: &HeaderMap| token_of(h) == STRIPE_KEY;
    let app = Router::new()
        .route(
            "/v1/accounts",
            post(move |State(s): State<StripeShared>, h: HeaderMap, Form(f): Form<HashMap<String, String>>| async move {
                assert!(authorised(&h));
                let key = h.get("idempotency-key").unwrap().to_str().unwrap().to_string();
                let mut st = s.lock().unwrap();
                st.posts.push(("/v1/accounts".into(), f));
                let n = st.made.len();
                let id = st.made.entry(key).or_insert_with(|| format!("acct_test{n}")).clone();
                st.accounts.entry(id.clone()).or_insert(false);
                Json(json!({"id": id, "object": "account"}))
            }),
        )
        .route(
            "/v1/account_links",
            post(|State(s): State<StripeShared>, Form(f): Form<HashMap<String, String>>| async move {
                let url = format!("https://connect.stripe.com/setup/e/{}/x", f["account"]);
                s.lock().unwrap().posts.push(("/v1/account_links".into(), f));
                Json(json!({"object": "account_link", "url": url}))
            }),
        )
        .route(
            "/v1/accounts/{id}",
            get(|State(s): State<StripeShared>, Path(id): Path<String>| async move {
                let on = s.lock().unwrap().accounts.get(&id).copied().unwrap_or(false);
                Json(json!({"id": id, "details_submitted": true, "charges_enabled": on, "payouts_enabled": on}))
            })
            .post(|State(s): State<StripeShared>, Path(id): Path<String>, Form(f): Form<HashMap<String, String>>| async move {
                s.lock().unwrap().posts.push((format!("/v1/accounts/{id}"), f));
                Json(json!({"id": id, "object": "account"}))
            }),
        )
        .route(
            "/v1/accounts/{id}/login_links",
            post(|Path(id): Path<String>| async move {
                Json(json!({"object": "login_link", "url": format!("https://connect.stripe.com/express/{id}")}))
            }),
        )
        .route(
            "/v1/checkout/sessions",
            post(move |State(s): State<StripeShared>, h: HeaderMap, Form(f): Form<HashMap<String, String>>| async move {
                assert!(authorised(&h));
                let mut st = s.lock().unwrap();
                let id = format!("cs_test_{}", st.sessions.len() + 1);
                st.sessions.insert(id.clone(), (f.clone(), false));
                st.posts.push(("/v1/checkout/sessions".into(), f));
                Json(json!({"id": id, "url": format!("https://checkout.stripe.com/c/pay/{id}"), "payment_status": "unpaid"}))
            }),
        )
        .route(
            "/v1/checkout/sessions/{id}",
            get(|State(s): State<StripeShared>, Path(id): Path<String>| async move {
                let st = s.lock().unwrap();
                if st.sessions_unreadable {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "api_error"}})))
                        .into_response();
                }
                // (an unknown session: Stripe's 404, resource_missing)
                match st.sessions.get(&id).cloned() {
                    Some((f, paid)) => {
                        let mut session = session_json(&id, &f, paid);
                        if st.expired.contains(&id) {
                            session["status"] = json!("expired");
                        }
                        Json(session).into_response()
                    }
                    None => (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": {"type": "invalid_request_error", "code": "resource_missing"}})),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/v1/checkout/sessions/{id}/expire",
            post(|State(s): State<StripeShared>, Path(id): Path<String>| async move {
                let mut st = s.lock().unwrap();
                // (only an open session can be expired)
                let open = st.sessions.get(&id).is_some_and(|(_, paid)| !paid) && !st.expired.contains(&id);
                if !open {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "not open"}})));
                }
                st.expired.push(id.clone());
                st.posts.push((format!("/v1/checkout/sessions/{id}/expire"), HashMap::new()));
                (StatusCode::OK, Json(json!({"id": id, "object": "checkout.session", "status": "expired"})))
            }),
        )
        // Stripe Tax (MF-86): 7% of the items, shipping untaxed
        .route(
            "/v1/tax/calculations",
            post(|State(s): State<StripeShared>, Form(f): Form<HashMap<String, String>>| async move {
                let items = f["line_items[0][amount]"].parse::<i64>().unwrap();
                let shipping = f.get("shipping_cost[amount]").map_or(0, |a| a.parse::<i64>().unwrap());
                let mut st = s.lock().unwrap();
                st.posts.push(("/v1/tax/calculations".into(), f));
                let n = st.posts.len();
                Json(json!({"id": format!("taxcalc_test_{n}"), "object": "tax.calculation",
                    "amount_total": items + shipping + items * 7 / 100, "tax_amount_exclusive": items * 7 / 100}))
            }),
        )
        .route(
            "/v1/tax/transactions/create_from_calculation",
            post(|State(s): State<StripeShared>, Form(f): Form<HashMap<String, String>>| async move {
                let id = format!("tax_test_{}", f["reference"].replace('-', "_"));
                s.lock().unwrap().posts.push(("/v1/tax/transactions/create_from_calculation".into(), f));
                Json(json!({"id": id, "object": "tax.transaction", "type": "transaction"}))
            }),
        )
        .route(
            "/v1/tax/transactions/create_reversal",
            post(|State(s): State<StripeShared>, Form(f): Form<HashMap<String, String>>| async move {
                s.lock().unwrap().posts.push(("/v1/tax/transactions/create_reversal".into(), f));
                Json(json!({"id": "tax_test_reversal", "object": "tax.transaction", "type": "reversal"}))
            }),
        )
        .route(
            "/v1/refunds",
            post(|State(s): State<StripeShared>, Form(f): Form<HashMap<String, String>>| async move {
                s.lock().unwrap().refunds.push(f);
                Json(json!({"id": "re_test_1", "object": "refund", "status": "succeeded"}))
            })
            .get(|State(s): State<StripeShared>, Query(q): Query<HashMap<String, String>>| async move {
                let st = s.lock().unwrap();
                // MakerFoundry's own (labelled), then the Dashboard's on this charge
                let mut data: Vec<Value> = st
                    .refunds
                    .iter()
                    .filter(|f| f.get("payment_intent").map(|p| format!("ch_{p}")) == q.get("charge").cloned())
                    .map(|f| json!({"id": "re_test_1", "object": "refund", "status": "succeeded", "metadata": {"order": f.get("metadata[order]")}}))
                    .collect();
                data.extend(st.listed_refunds.iter().filter(|r| r["charge"].as_str() == q.get("charge").map(String::as_str)).cloned());
                Json(json!({"object": "list", "data": data}))
            }),
        )
        .route(
            "/v1/payment_intents/{id}",
            get(|Path(id): Path<String>, Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!(q.get("expand[]").map(String::as_str), Some("latest_charge"));
                let charge = json!({"id": format!("ch_{id}"), "object": "charge", "application_fee": format!("fee_{id}"), "transfer": format!("tr_{id}")});
                Json(json!({"id": id, "object": "payment_intent", "latest_charge": charge}))
            }),
        )
        .route(
            "/v1/application_fees/{id}/refunds",
            post(|State(s): State<StripeShared>, Path(id): Path<String>, Form(f): Form<HashMap<String, String>>| async move {
                let mut st = s.lock().unwrap();
                if st.fee_refunds_fail {
                    return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": {"message": "try again"}})));
                }
                st.fee_refunds.push((id.clone(), f));
                (StatusCode::OK, Json(json!({"id": "fr_test_1", "object": "fee_refund", "fee": id})))
            }),
        )
        .route(
            "/v1/application_fees/{id}",
            get(|State(s): State<StripeShared>, Path(id): Path<String>| async move {
                let st = s.lock().unwrap();
                let data: Vec<Value> = st
                    .fee_refunds
                    .iter()
                    .filter(|(f, _)| *f == id)
                    .map(|(_, f)| json!({"object": "fee_refund", "amount": f["amount"].parse::<i64>().unwrap(), "metadata": {"order": f.get("metadata[order]"), "reason": f.get("metadata[reason]")}}))
                    .collect();
                Json(json!({"id": id, "object": "application_fee", "refunds": {"object": "list", "data": data}}))
            }),
        )
        .route(
            "/v1/transfers",
            get(|State(s): State<StripeShared>, Query(q): Query<HashMap<String, String>>| async move {
                let st = s.lock().unwrap();
                let data: Vec<Value> = st
                    .transfers
                    .iter()
                    .filter(|f| f.get("transfer_group") == q.get("transfer_group"))
                    .map(|f| json!({"object": "transfer", "amount": f["amount"].parse::<i64>().unwrap(), "metadata": {"order": f.get("metadata[order]"), "reason": f.get("metadata[reason]")}}))
                    .collect();
                Json(json!({"object": "list", "data": data}))
            })
            .post(|State(s): State<StripeShared>, Form(f): Form<HashMap<String, String>>| async move {
                let mut st = s.lock().unwrap();
                st.transfers.push(f);
                let n = st.transfers.len();
                Json(json!({"id": format!("tr_back_{n}"), "object": "transfer"}))
            }),
        )
        .route(
            "/v1/transfers/{id}",
            get(|State(s): State<StripeShared>, Path(id): Path<String>| async move {
                let st = s.lock().unwrap();
                let data: Vec<Value> = st
                    .reversals
                    .iter()
                    .filter(|(t, _)| *t == id)
                    .map(|(_, f)| json!({"object": "transfer_reversal", "amount": f["amount"].parse::<i64>().unwrap(), "metadata": {"order": f["metadata[order]"], "reason": f["metadata[reason]"]}}))
                    .collect();
                Json(json!({"id": id, "object": "transfer", "reversals": {"object": "list", "data": data}}))
            }),
        )
        .route(
            "/v1/transfers/{id}/reversals",
            post(|State(s): State<StripeShared>, Path(id): Path<String>, Form(f): Form<HashMap<String, String>>| async move {
                let mut st = s.lock().unwrap();
                if st.reversals_fail {
                    return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": {"message": "try again"}})));
                }
                st.reversals.push((id.clone(), f));
                (StatusCode::OK, Json(json!({"id": "trr_test_1", "object": "transfer_reversal", "transfer": id})))
            }),
        )
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let stripe = makerfoundry::stripe::Stripe::new(
        &format!("http://{addr}"),
        STRIPE_KEY,
        vec![WEBHOOK_SECRET.into()],
        "https://makerfoundry.test",
    )
    .unwrap();
    (stripe, shared)
}

/// A Checkout Session as Stripe describes it, made from the form that made it: with automatic tax,
/// Stripe Tax adds 7% (rounded down).
fn session_json(id: &str, f: &HashMap<String, String>, paid: bool) -> Value {
    let price = f["line_items[0][price_data][unit_amount]"].parse::<i64>().unwrap();
    let tax = if f.contains_key("automatic_tax[enabled]") {
        price * 7 / 100
    } else {
        0
    };
    let mut metadata = json!({"app": f["metadata[app]"]});
    for k in ["order", "shop_order"] {
        if let Some(v) = f.get(&format!("metadata[{k}]")) {
            metadata[k] = json!(v);
        }
    }
    json!({
        "id": id,
        "object": "checkout.session",
        "payment_status": if paid { "paid" } else { "unpaid" },
        "amount_subtotal": price,
        "amount_total": price + tax,
        "total_details": {"amount_tax": tax, "amount_discount": 0, "amount_shipping": 0},
        "currency": "usd",
        "payment_intent": format!("pi_{id}"),
        "metadata": metadata,
        "status": if paid { "complete" } else { "open" },
        "url": format!("https://checkout.stripe.com/c/pay/{id}"),
    })
}

#[tokio::test]
async fn a_shared_stripe_accounts_other_payments_are_left_alone() {
    let Some((s, st, _sam, fox)) = selling_site(Vec::new()).await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    s.post(&format!("/d/{fox}/buy"), "", Some(&bea)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    assert_eq!(form["metadata[app]"], "makerfoundry");
    assert_eq!(form["payment_intent_data[metadata][app]"], "makerfoundry");
    // StitchFoundry's credit pack, paid in the same Stripe account: its event comes here too
    let credits = json!({"id": "cs_sf_1", "object": "checkout.session", "payment_status": "paid", "amount_total": 1000,
        "currency": "usd", "payment_intent": "pi_sf_1", "metadata": {"account": "1000", "credits": "1000", "pack": "small"}});
    assert_eq!(
        s.stripe_event(WEBHOOK_SECRET, "evt_sf_1", "checkout.session.completed", credits)
            .await
            .status,
        StatusCode::OK
    );
    let refund = json!({"id": "ch_sf_1", "object": "charge", "payment_intent": "pi_sf_1", "refunded": true});
    assert_eq!(
        s.stripe_event(WEBHOOK_SECRET, "evt_sf_2", "charge.refunded", refund)
            .await
            .status,
        StatusCode::OK
    );
    // a session that names one of our orders but isn't labelled ours changes nothing either
    let mut unlabelled = session_json("cs_test_1", &form, true);
    unlabelled["metadata"]["app"] = json!("stitchfoundry");
    s.stripe_event(WEBHOOK_SECRET, "evt_sf_3", "checkout.session.completed", unlabelled)
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 0);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE status = 'pending'")
            .await,
        1
    );
    // ours, labelled, is paid
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_mf_1",
        "checkout.session.completed",
        session_json("cs_test_1", &form, true),
    )
    .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 3);
}

impl Site {
    /// A Stripe event sent to the webhook, signed with `secret`.
    async fn stripe_event(&self, secret: &str, id: &str, kind: &str, object: Value) -> Answer {
        let body = json!({"id": id, "object": "event", "type": kind, "data": {"object": object}}).to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let sig = makerfoundry::stripe::signature(secret, now, body.as_bytes());
        self.send(
            Request::post("/stripe/webhook")
                .header(header::HOST, HOST)
                .header(header::CONTENT_TYPE, "application/json")
                .header("stripe-signature", format!("t={now},v1={sig}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    async fn scalar(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }

    async fn sign_up(&self, email: &str) -> String {
        self.post(
            "/signup",
            &format!("email={email}&password=fire+truck+1&again=fire+truck+1"),
            None,
        )
        .await
        .cookie()
    }
}

/// A seller (account 1000) whose Stripe account can take payments, with a published design
/// priced $5 → the site, the stand-in Stripe, the seller's cookie and the design.
async fn selling_site(admins: Vec<i64>) -> Option<(Site, StripeShared, String, i64)> {
    let (stripe, st) = fake_stripe().await;
    selling_site_with(admins, stripe, st).await
}

/// The same, charging sales tax with Stripe Tax (MF-24) → also the Stripe client, for the sweep.
async fn taxed_selling_site(
    admins: Vec<i64>,
) -> Option<(Site, StripeShared, String, i64, makerfoundry::stripe::Stripe)> {
    let (stripe, st) = fake_stripe().await;
    let stripe = stripe.with_tax(makerfoundry::stripe::DEFAULT_TAX_CODE).unwrap();
    let (s, st, seller, id) = selling_site_with(admins, stripe.clone(), st).await?;
    Some((s, st, seller, id, stripe))
}

async fn selling_site_with(
    admins: Vec<i64>,
    stripe: makerfoundry::stripe::Stripe,
    st: StripeShared,
) -> Option<(Site, StripeShared, String, i64)> {
    let s = site_full(admins, Some(stripe)).await?;
    selling_setup(s, st).await
}

/// `selling_site`'s seller, set up on the test site `s`.
async fn selling_setup(s: Site, st: StripeShared) -> Option<(Site, StripeShared, String, i64)> {
    let seller = s.sign_up("sam%40example.com").await;
    let id = published(&s, &seller, "sams-shop", "Fox+Patch").await;
    // a price needs selling set up first
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Fox+Patch&licence=personal&price=5",
            Some(&seller),
        )
        .await;
    assert!(r.body.contains("first set up selling"), "{}", r.body);
    // setting up: a Stripe Express account, paid monthly, then Stripe's own pages
    let r = s.post("/account/selling/start", "", Some(&seller)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(
        r.location()
            .starts_with("https://connect.stripe.com/setup/e/acct_test0"),
        "{}",
        r.location()
    );
    {
        let f = st.lock().unwrap();
        let made = &f.posts.iter().find(|(p, _)| p == "/v1/accounts").unwrap().1;
        assert_eq!(made["type"], "express");
        assert_eq!(made["settings[payouts][schedule][interval]"], "monthly");
        // held 14 days before payout, for refunds and disputes (issue #91)
        assert_eq!(made["settings[payouts][schedule][delay_days]"], "14");
    }
    // pressed again: the same account
    s.post("/account/selling/start", "", Some(&seller)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_sellers").await, 1);
    // back from Stripe, not finished yet
    assert_eq!(
        s.get("/account/selling/return", Some(&seller)).await.location(),
        "/account/selling"
    );
    assert!(
        s.get("/account/selling", Some(&seller))
            .await
            .body
            .contains("Carry on with Stripe")
    );
    // Stripe says it's done, by a webhook from the connected accounts' endpoint
    st.lock().unwrap().accounts.insert("acct_test0".into(), true);
    let account = json!({"id": "acct_test0", "object": "account", "details_submitted": true, "charges_enabled": true, "payouts_enabled": true});
    let r = s
        .stripe_event("whsec_wrong", "evt_acct_0", "account.updated", account.clone())
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        s.get("/account/selling", Some(&seller))
            .await
            .body
            .contains("Carry on with Stripe")
    );
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_acct_1", "account.updated", account)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        s.get("/account/selling", Some(&seller))
            .await
            .body
            .contains("You can sell")
    );
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Fox+Patch&licence=personal&price=%245.00",
            Some(&seller),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    Some((s, st, seller, id))
}

#[tokio::test]
async fn a_design_is_sold_paid_for_delivered_and_refunded() {
    let Some((s, st, seller, id)) = selling_site(vec![1002]).await else {
        return;
    };
    let page = s.get(&format!("/d/{id}"), None).await.body;
    assert!(page.contains("Sign in to buy · $5"), "{page}");
    let buyer = s.sign_up("bea%40example.com").await;
    let admin = s.sign_up("ada%40example.com").await;
    // not bought: no file, just the page with the button
    let r = s.get(&format!("/d/{id}/download"), Some(&buyer)).await;
    assert_eq!(r.location(), format!("/d/{id}/fox-patch"));
    assert!(s.get(&format!("/d/{id}"), Some(&buyer)).await.body.contains("Buy · $5"));
    // buying: an order with its amounts fixed, then Stripe's checkout page
    let r = s.post(&format!("/d/{id}/buy"), "", Some(&buyer)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "https://checkout.stripe.com/c/pay/cs_test_1");
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    assert_eq!(form["line_items[0][price_data][unit_amount]"], "500");
    // 10% ($0.50) and the card fee ($0.45) to MakerFoundry, $4.05 to the seller's Stripe account
    assert_eq!(form["payment_intent_data[application_fee_amount]"], "95");
    assert_eq!(form["payment_intent_data[transfer_data][destination]"], "acct_test0");
    // the app's name on the card statement (issue #91)
    assert_eq!(form["payment_intent_data[statement_descriptor_suffix]"], "MAKERFOUNDRY");
    let order = form["metadata[order]"].clone();
    // back from Stripe before the payment is confirmed: no download yet
    let lib = s.get(&format!("/library?order={order}"), Some(&buyer)).await.body;
    assert!(lib.contains("Waiting for the payment"), "{lib}");
    assert!(
        !s.get(&format!("/d/{id}/download"), Some(&buyer))
            .await
            .location()
            .contains("/files/")
    );
    // a forged event changes nothing
    let paid = session_json("cs_test_1", &form, true);
    let r = s
        .stripe_event("whsec_wrong", "evt_1", "checkout.session.completed", paid.clone())
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 0);
    // a session paid for less than the price changes nothing
    let mut short = paid.clone();
    short["amount_total"] = json!(100);
    assert_eq!(
        s.stripe_event(WEBHOOK_SECRET, "evt_short", "checkout.session.completed", short)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 0);
    // Stripe's signed event: paid, and the sale in the ledger, to the cent
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_1", "checkout.session.completed", paid.clone())
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let ledger: Vec<(i64, String, i64)> = sqlx::query_as("SELECT account_id, kind, cents FROM mf_ledger ORDER BY kind")
        .fetch_all(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        ledger,
        [
            (0, "fee".to_string(), 50),
            (0, "processing".to_string(), 45),
            (1000, "seller".to_string(), 405)
        ]
    );
    // sent again, or as another event for the same payment: handled once
    s.stripe_event(WEBHOOK_SECRET, "evt_1", "checkout.session.completed", paid.clone())
        .await;
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_2",
        "checkout.session.async_payment_succeeded",
        paid.clone(),
    )
    .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 3);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_stripe_events").await, 4);
    // delivered: the buyer downloads it, from their library too
    let r = s.get(&format!("/d/{id}/download"), Some(&buyer)).await;
    assert!(r.location().contains("/files/"), "{}", r.location());
    assert!(
        s.get(&format!("/d/{id}"), Some(&buyer))
            .await
            .body
            .contains("You bought this")
    );
    assert!(
        s.get("/library", Some(&buyer))
            .await
            .body
            .contains(&format!(r#"href="/d/{id}/download""#))
    );
    // buying again goes to the library
    assert_eq!(
        s.post(&format!("/d/{id}/buy"), "", Some(&buyer)).await.location(),
        "/library"
    );
    // the seller's sales
    let sales = s.get("/account/sales", Some(&seller)).await.body;
    assert!(
        sales.contains("$5 − fee $0.50 − card fee $0.45 = <strong>$4.05</strong>"),
        "{sales}"
    );
    // unpublished, it's still the buyer's; and it can't be deleted
    s.post(&format!("/designs/{id}/unpublish"), "", Some(&seller)).await;
    assert!(
        s.get(&format!("/d/{id}/download"), Some(&buyer))
            .await
            .location()
            .contains("/files/")
    );
    let r = s.post(&format!("/designs/{id}/delete"), "", Some(&seller)).await;
    assert!(r.body.contains("People have bought this design"), "{}", r.body);
    // an admin refunds it in full: the sale reversed in the ledger, and no more download
    assert_eq!(s.get("/admin/orders", Some(&buyer)).await.status, StatusCode::NOT_FOUND);
    let r = s.post(&format!("/admin/orders/{order}/refund"), "", Some(&admin)).await;
    assert!(r.body.contains("refunded"), "{}", r.body);
    {
        let f = st.lock().unwrap();
        assert_eq!(f.refunds[0]["payment_intent"], "pi_cs_test_1");
        assert_eq!(f.refunds[0]["reverse_transfer"], "true");
        // MakerFoundry keeps the application fee at first, then gives the 10% back, not the card fee
        assert_eq!(f.refunds[0]["refund_application_fee"], "false");
        assert_eq!(f.fee_refunds.len(), 1);
        assert_eq!(f.fee_refunds[0].0, "fee_pi_cs_test_1");
        assert_eq!(f.fee_refunds[0].1["amount"], "50");
    }
    let ledger: Vec<(i64, String, i64, String)> =
        sqlx::query_as("SELECT account_id, kind, cents, reason FROM mf_ledger WHERE reason <> 'sale' ORDER BY id")
            .fetch_all(&s.pool)
            .await
            .unwrap();
    assert_eq!(
        ledger,
        [
            (1000, "seller".to_string(), -500, "refund".to_string()),
            (1000, "seller".to_string(), 50, "fee_return".to_string()),
            (0, "fee".to_string(), -50, "fee_return".to_string()),
        ]
    );
    // the buyer has everything back; the seller is out the card fee, which went to Stripe
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_ledger").await, 0);
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'processing'")
            .await,
        45
    );
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        -45
    );
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'fee'")
            .await,
        0
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 6);
    // Stripe's own word of the refund afterwards changes nothing more
    let charge = json!({"id": "ch_1", "object": "charge", "payment_intent": "pi_cs_test_1", "refunded": true});
    assert_eq!(
        s.stripe_event(WEBHOOK_SECRET, "evt_3", "charge.refunded", charge)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 6);
    assert!(
        !s.get(&format!("/d/{id}/download"), Some(&buyer))
            .await
            .location()
            .contains("/files/")
    );
    assert!(s.get("/library", Some(&buyer)).await.body.contains("Refunded"));
    assert!(
        s.get("/account/sales", Some(&seller))
            .await
            .body
            .contains("<dd>-$0.45</dd>")
    );
}

#[tokio::test]
async fn sales_tax_is_charged_kept_by_makerfoundry_and_refunded() {
    let Some((s, st, seller, id, stripe)) = taxed_selling_site(vec![1002]).await else {
        return;
    };
    let buyer = s.sign_up("bea%40example.com").await;
    let admin = s.sign_up("ada%40example.com").await;
    s.post(&format!("/d/{id}/buy"), "", Some(&buyer)).await;
    // the checkout asks Stripe Tax for the tax, on top of the price, MakerFoundry liable
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    assert_eq!(form["automatic_tax[enabled]"], "true");
    assert_eq!(form["automatic_tax[liability][type]"], "self");
    assert_eq!(form["billing_address_collection"], "required");
    assert_eq!(form["line_items[0][price_data][tax_behavior]"], "exclusive");
    assert_eq!(
        form["line_items[0][price_data][product_data][tax_code]"],
        "txcd_10505001"
    );
    // the seller's share is worked out from the price alone
    assert_eq!(form["line_items[0][price_data][unit_amount]"], "500");
    assert_eq!(form["payment_intent_data[application_fee_amount]"], "95");
    let order = form["metadata[order]"].clone();
    // a total that isn't the price and the tax changes nothing
    let paid = session_json("cs_test_1", &form, true);
    assert_eq!(paid["amount_total"], 535);
    let mut wrong = paid.clone();
    wrong["amount_total"] = json!(500);
    s.stripe_event(WEBHOOK_SECRET, "evt_wrong", "checkout.session.completed", wrong)
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 0);
    // paid: the tax is MakerFoundry's, and taken back from the seller's transfer at once
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_1", "checkout.session.completed", paid.clone())
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let ledger: Vec<(i64, String, i64)> = sqlx::query_as("SELECT account_id, kind, cents FROM mf_ledger ORDER BY kind")
        .fetch_all(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        ledger,
        [
            (0, "fee".to_string(), 50),
            (0, "processing".to_string(), 45),
            (1000, "seller".to_string(), 405),
            (0, "tax".to_string(), 35)
        ]
    );
    {
        let f = st.lock().unwrap();
        assert_eq!(f.reversals.len(), 1);
        assert_eq!(f.reversals[0].0, "tr_pi_cs_test_1");
        assert_eq!(f.reversals[0].1["amount"], "35");
        assert_eq!(f.reversals[0].1["metadata[reason]"], "sales_tax");
    }
    // told again, or swept: the tax isn't taken twice
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_2",
        "checkout.session.async_payment_succeeded",
        paid.clone(),
    )
    .await;
    makerfoundry::selling::withhold_taxes(&stripe, &s.pool).await.unwrap();
    assert_eq!(st.lock().unwrap().reversals.len(), 1);
    // the buyer sees what they paid; the seller's sales don't show the tax
    assert!(s.get("/library", Some(&buyer)).await.body.contains("$5 + $0.35 tax"));
    assert!(
        s.get("/account/sales", Some(&seller))
            .await
            .body
            .contains("$5 − fee $0.50 − card fee $0.45 = <strong>$4.05</strong>")
    );
    assert!(s.get("/admin/overview", Some(&admin)).await.body.contains("$0.35"));
    // refunded: the price taken back from the seller by MakerFoundry, the buyer refunded all of it
    let r = s.post(&format!("/admin/orders/{order}/refund"), "", Some(&admin)).await;
    assert!(r.body.contains("refunded"), "{}", r.body);
    {
        let f = st.lock().unwrap();
        assert_eq!(f.reversals.len(), 2);
        assert_eq!(f.reversals[1].1["amount"], "500");
        assert_eq!(f.reversals[1].1["metadata[reason]"], "refund");
        assert_eq!(f.refunds[0]["reverse_transfer"], "false");
        assert_eq!(f.refunds[0]["refund_application_fee"], "false");
        assert_eq!(f.fee_refunds[0].1["amount"], "50");
    }
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_ledger").await, 0);
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'tax'")
            .await,
        0
    );
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        -45
    );
}

#[tokio::test]
async fn tax_stripe_couldnt_take_back_is_taken_by_the_sweep_or_the_refund() {
    let Some((s, st, _seller, id, stripe)) = taxed_selling_site(vec![1002]).await else {
        return;
    };
    let buyer = s.sign_up("bea%40example.com").await;
    let admin = s.sign_up("ada%40example.com").await;
    // two sales while Stripe can't reverse transfers
    st.lock().unwrap().reversals_fail = true;
    s.post(&format!("/d/{id}/buy"), "", Some(&buyer)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    let paid = session_json("cs_test_1", &form, true);
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_1", "checkout.session.completed", paid)
        .await;
    // the sale itself is recorded (the payment happened)
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE status = 'paid' AND tax_withheld_at IS NULL")
            .await,
        1
    );
    let other = s.sign_up("cy%40example.com").await;
    s.post(&format!("/d/{id}/buy"), "", Some(&other)).await;
    let form2 = st.lock().unwrap().sessions["cs_test_2"].0.clone();
    let order2 = form2["metadata[order]"].clone();
    st.lock().unwrap().sessions.get_mut("cs_test_2").unwrap().1 = true;
    s.get(&format!("/library?order={order2}"), Some(&other)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE status = 'paid' AND tax_withheld_at IS NULL")
            .await,
        2
    );
    // a refund can't go ahead while the money can't be taken back from the seller
    let r = s
        .post(&format!("/admin/orders/{order2}/refund"), "", Some(&admin))
        .await;
    assert!(r.body.contains("Nothing was refunded"), "{}", r.body);
    assert!(st.lock().unwrap().refunds.is_empty());
    // Stripe answers again: the sweep takes the first sale's tax; the refund takes the second's
    // tax, then its price
    st.lock().unwrap().reversals_fail = false;
    let order1: i64 = form["metadata[order]"].parse().unwrap();
    sqlx::query("UPDATE mf_orders SET tax_withheld_at = NULL WHERE id = $1")
        .bind(order1)
        .execute(&s.pool)
        .await
        .unwrap();
    let r = s
        .post(&format!("/admin/orders/{order2}/refund"), "", Some(&admin))
        .await;
    assert!(r.body.contains("refunded"), "{}", r.body);
    makerfoundry::selling::withhold_taxes(&stripe, &s.pool).await.unwrap();
    let mut got: Vec<(String, String, String)> = st
        .lock()
        .unwrap()
        .reversals
        .iter()
        .map(|(t, r)| (t.clone(), r["amount"].clone(), r["metadata[reason]"].clone()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("tr_pi_cs_test_1".to_string(), "35".to_string(), "sales_tax".to_string()),
            ("tr_pi_cs_test_2".to_string(), "35".to_string(), "sales_tax".to_string()),
            ("tr_pi_cs_test_2".to_string(), "500".to_string(), "refund".to_string()),
        ]
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE tax_withheld_at IS NULL AND tax_cents > 0")
            .await,
        0
    );
}

#[tokio::test]
async fn a_fee_stripe_couldnt_give_back_is_given_back_later() {
    let Some((s, st, seller, id)) = selling_site(vec![1002]).await else {
        return;
    };
    let buyer = s.sign_up("bea%40example.com").await;
    let admin = s.sign_up("ada%40example.com").await;
    s.post(&format!("/d/{id}/buy"), "", Some(&buyer)).await;
    let order = st.lock().unwrap().sessions["cs_test_1"].0["metadata[order]"].clone();
    st.lock().unwrap().sessions.get_mut("cs_test_1").unwrap().1 = true;
    s.get(&format!("/library?order={order}"), Some(&buyer)).await;
    // the buyer is refunded, but Stripe can't give the fee back just then
    st.lock().unwrap().fee_refunds_fail = true;
    let r = s.post(&format!("/admin/orders/{order}/refund"), "", Some(&admin)).await;
    assert!(r.body.contains("refunded, but Stripe couldn"), "{}", r.body);
    assert!(
        r.body.contains(&format!("/admin/orders/{order}/fee-back")),
        "{}",
        r.body
    );
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        -95
    );
    // nobody else can press it; an admin tries again once Stripe answers
    assert_eq!(
        s.post(&format!("/admin/orders/{order}/fee-back"), "", Some(&seller))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    st.lock().unwrap().fee_refunds_fail = false;
    let r = s
        .post(&format!("/admin/orders/{order}/fee-back"), "", Some(&admin))
        .await;
    assert!(r.body.contains("given back to the seller"), "{}", r.body);
    assert!(!r.body.contains("/fee-back"), "{}", r.body);
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        -45
    );
    // pressed again: nothing more
    let r = s
        .post(&format!("/admin/orders/{order}/fee-back"), "", Some(&admin))
        .await;
    assert!(r.body.contains("no fee waiting"), "{}", r.body);
    assert_eq!(st.lock().unwrap().fee_refunds.len(), 1);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 6);
}

#[tokio::test]
async fn coming_back_from_stripe_delivers_without_waiting_for_the_webhook() {
    let Some((s, st, seller, id)) = selling_site(Vec::new()).await else {
        return;
    };
    let buyer = s.sign_up("bea%40example.com").await;
    s.post(&format!("/d/{id}/buy"), "", Some(&buyer)).await;
    // the seller can't buy their own; someone else can't see the buyer's order
    assert_eq!(
        s.post(&format!("/d/{id}/buy"), "", Some(&seller)).await.location(),
        format!("/d/{id}")
    );
    let order = st.lock().unwrap().sessions["cs_test_1"].0["metadata[order]"].clone();
    st.lock().unwrap().sessions.get_mut("cs_test_1").unwrap().1 = true;
    s.get(&format!("/library?order={order}"), Some(&seller)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE status = 'paid'").await,
        0
    );
    let lib = s.get(&format!("/library?order={order}"), Some(&buyer)).await.body;
    assert!(lib.contains(&format!(r#"href="/d/{id}/download""#)), "{lib}");
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE reason = 'sale'")
            .await,
        500
    );
    // and the webhook arriving later adds nothing
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_9",
        "checkout.session.completed",
        session_json("cs_test_1", &form, true),
    )
    .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 3);
}

// ---------------------------------------------------------------- issue #90: chargebacks

/// A dispute as Stripe describes it (https://docs.stripe.com/api/disputes/object), of the sale
/// paid by `intent`.
fn dispute_json(id: &str, intent: &str, amount: i64, status: &str) -> Value {
    json!({"id": id, "object": "dispute", "amount": amount, "currency": "usd",
        "charge": format!("ch_{intent}"), "payment_intent": intent, "reason": "fraudulent", "status": status})
}

/// The selling site with email notices (a test mailbox) and the admin 1002 → also the site's state
/// (for the sweep) and the mailbox.
async fn disputes_site() -> Option<(
    Site,
    StripeShared,
    String,
    i64,
    AppState,
    makerfoundry::notices::stand_in::Sent,
)> {
    let (stripe, st) = fake_stripe().await;
    let pool = test_db().await?;
    let (storage, stored) = fake_storage().await;
    let storage_kept = storage.clone();
    let (url, fake) = fake_cloud().await;
    let (base, sent) = makerfoundry::notices::stand_in::start().await.unwrap();
    let state = AppState {
        cloud: Cloud::new(&url).unwrap(),
        db: Some(pool.clone()),
        storage: Some(storage),
        publishers: makerfoundry::listings::Publishers::Everyone,
        admins: vec![1002],
        limits: std::sync::Arc::default(),
        stripe: Some(stripe),
        launched: false,
        public_url: "https://makerfoundry.test".into(),
        api_origins: vec!["https://stitchfoundry.test".into()],
        proofs: None,
        three_d: false,
        scanner: None,
        thingiverse: None,
        social: None,
        notices: Some(makerfoundry::notices::Notices::to_catcher("", None, &base).unwrap()),
        step: None,
        scad: None,
        points: false,
        stitchhub: None,
    };
    let s = Site {
        app: makerfoundry::app(state.clone()),
        pool,
        fake,
        stored,
        storage: Some(storage_kept),
    };
    let (s, st, seller, id) = selling_setup(s, st).await?;
    Some((s, st, seller, id, state, sent))
}

/// A buyer (bea, 1001) has paid for design `id` (confirmed by Stripe's webhook) → their cookie
/// and the order.
async fn paid_sale(s: &Site, st: &StripeShared, id: i64) -> (String, String) {
    let buyer = s.sign_up("bea%40example.com").await;
    s.post(&format!("/d/{id}/buy"), "", Some(&buyer)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    let r = s
        .stripe_event(
            WEBHOOK_SECRET,
            "evt_paid",
            "checkout.session.completed",
            session_json("cs_test_1", &form, true),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    (buyer, form["metadata[order]"].clone())
}

impl Site {
    async fn downloads(&self, id: i64, cookie: &str) -> bool {
        self.get(&format!("/d/{id}/download"), Some(cookie))
            .await
            .location()
            .contains("/files/")
    }

    async fn order_status(&self, order: &str) -> String {
        sqlx::query_scalar("SELECT status FROM mf_orders WHERE id = $1")
            .bind(order.parse::<i64>().unwrap())
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_disputed_sale_is_blocked_taken_back_once_and_given_back_when_won() {
    let Some((s, st, seller, id)) = selling_site(vec![1002]).await else {
        return;
    };
    let (buyer, order) = paid_sale(&s, &st, id).await;
    let admin = s.sign_up("ada%40example.com").await;
    assert!(s.downloads(id, &buyer).await);
    // the buyer disputes it with their bank
    let created = dispute_json("dp_test_1", "pi_cs_test_1", 500, "needs_response");
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_dp_1", "charge.dispute.created", created.clone())
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(s.order_status(&order).await, "disputed");
    assert!(!s.downloads(id, &buyer).await);
    assert!(
        s.get("/library", Some(&buyer))
            .await
            .body
            .contains("disputed with your bank")
    );
    // the price taken back from the seller's transfer, then MakerFoundry's 10% back to them
    {
        let f = st.lock().unwrap();
        assert_eq!(f.reversals.len(), 1);
        assert_eq!(f.reversals[0].0, "tr_pi_cs_test_1");
        assert_eq!(f.reversals[0].1["amount"], "500");
        assert_eq!(f.reversals[0].1["metadata[reason]"], "dispute");
        assert_eq!(f.reversals[0].1["metadata[order]"], order);
        assert_eq!(f.fee_refunds.len(), 1);
        assert_eq!(f.fee_refunds[0].0, "fee_pi_cs_test_1");
        assert_eq!(f.fee_refunds[0].1["amount"], "50");
        assert_eq!(f.fee_refunds[0].1["metadata[reason]"], "dispute_fee");
        assert!(f.refunds.is_empty());
    }
    // the seller is out the card fee, as on a refund; the buyer's money is back with them
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        -45
    );
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_ledger").await, 0);
    // delivered again (Stripe retries), as another event, or swept: nothing more happens
    for evt in ["evt_dp_1", "evt_dp_1b"] {
        let r = s
            .stripe_event(WEBHOOK_SECRET, evt, "charge.dispute.created", created.clone())
            .await;
        assert_eq!(r.status, StatusCode::OK);
    }
    {
        let f = st.lock().unwrap();
        assert_eq!(f.reversals.len(), 1);
        assert_eq!(f.fee_refunds.len(), 1);
    }
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 6);
    // the seller and the admins see it
    let sales = s.get("/account/sales", Some(&seller)).await.body;
    assert!(sales.contains("disputed by the buyer"), "{sales}");
    let list = s.get("/admin/orders", Some(&admin)).await.body;
    assert!(list.contains("disputed"), "{list}");
    assert!(!list.contains("Stripe's steps aren't all done"), "{list}");
    // won: the order is paid again and the seller gets back what they lost
    let won = dispute_json("dp_test_1", "pi_cs_test_1", 500, "won");
    for evt in ["evt_dp_2", "evt_dp_2"] {
        let r = s
            .stripe_event(WEBHOOK_SECRET, evt, "charge.dispute.closed", won.clone())
            .await;
        assert_eq!(r.status, StatusCode::OK);
    }
    assert_eq!(s.order_status(&order).await, "paid");
    assert!(s.downloads(id, &buyer).await);
    {
        let f = st.lock().unwrap();
        assert_eq!(f.transfers.len(), 1, "transferred back once");
        let t = &f.transfers[0];
        assert_eq!(t["amount"], "450");
        assert_eq!(t["destination"], "acct_test0");
        assert_eq!(t["currency"], "usd");
        assert_eq!(t["transfer_group"], format!("mf-dispute-{order}"));
        assert_eq!(t["metadata[reason]"], "dispute_won");
        assert_eq!(f.reversals.len(), 1);
    }
    // back where the sale was: the seller's $4.05, MakerFoundry's 95¢
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        405
    );
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_ledger").await, 500);
    let list = s.get("/admin/orders", Some(&admin)).await.body;
    assert!(list.contains("dispute closed: won"), "{list}");
}

#[tokio::test]
async fn a_lost_dispute_is_a_chargeback_and_a_refused_reversal_alerts_the_admins() {
    let Some((s, st, _seller, id, state, sent)) = disputes_site().await else {
        return;
    };
    let (buyer, order) = paid_sale(&s, &st, id).await;
    let admin = s.sign_up("ada%40example.com").await;
    // the seller's Stripe balance can't be reversed just now
    st.lock().unwrap().reversals_fail = true;
    let created = dispute_json("dp_test_9", "pi_cs_test_1", 500, "needs_response");
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_dp_9", "charge.dispute.created", created.clone())
        .await;
    // Stripe is answered all the same, and the buyer is blocked
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(s.order_status(&order).await, "disputed");
    assert!(!s.downloads(id, &buyer).await);
    assert!(st.lock().unwrap().reversals.is_empty());
    assert!(
        st.lock().unwrap().fee_refunds.is_empty(),
        "no fee back before the price is taken"
    );
    // the seller is told, and the admins alerted, once each
    let mails = || -> Vec<(String, String)> {
        sent.lock()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["mailpit"]["To"][0]["Email"].as_str().unwrap().to_string(),
                    v["mailpit"]["Subject"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let got = mails();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got.iter()
            .any(|(to, subject)| to == "sam@example.com" && subject.contains("disputed")),
        "{got:?}"
    );
    assert!(
        got.iter()
            .any(|(to, subject)| to == "ada@example.com" && subject.contains("needs a look")),
        "{got:?}"
    );
    s.stripe_event(WEBHOOK_SECRET, "evt_dp_9", "charge.dispute.created", created)
        .await;
    makerfoundry::disputes::settle_all(&state, &s.pool).await.unwrap();
    assert_eq!(mails().len(), 2);
    assert!(
        s.get("/admin/orders", Some(&admin))
            .await
            .body
            .contains("Stripe's steps aren't all done")
    );
    // Stripe answers again: the sweep takes the price back and gives the 10%, once
    st.lock().unwrap().reversals_fail = false;
    makerfoundry::disputes::settle_all(&state, &s.pool).await.unwrap();
    makerfoundry::disputes::settle_all(&state, &s.pool).await.unwrap();
    {
        let f = st.lock().unwrap();
        assert_eq!(f.reversals.len(), 1);
        assert_eq!(f.reversals[0].1["amount"], "500");
        assert_eq!(f.fee_refunds.len(), 1);
    }
    // lost: a chargeback, still blocked, nothing given back
    let lost = dispute_json("dp_test_9", "pi_cs_test_1", 500, "lost");
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_dp_10", "charge.dispute.closed", lost)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(s.order_status(&order).await, "chargeback");
    assert!(!s.downloads(id, &buyer).await);
    assert!(s.get("/library", Some(&buyer)).await.body.contains("Charged back"));
    assert!(st.lock().unwrap().transfers.is_empty());
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        -45
    );
    let got = mails();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(
        got[2].0 == "sam@example.com" && got[2].1.contains("went to the buyer"),
        "{got:?}"
    );
    makerfoundry::disputes::settle_all(&state, &s.pool).await.unwrap();
    assert_eq!(mails().len(), 3);
}

#[tokio::test]
async fn a_dispute_closed_before_it_was_seen_takes_and_gives_nothing() {
    let Some((s, st, _seller, id)) = selling_site(Vec::new()).await else {
        return;
    };
    let (buyer, order) = paid_sale(&s, &st, id).await;
    // an inquiry closed without a chargeback, its events delivered out of order
    let closed = dispute_json("dp_test_5", "pi_cs_test_1", 500, "warning_closed");
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_dp_5b", "charge.dispute.closed", closed)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let created = dispute_json("dp_test_5", "pi_cs_test_1", 500, "warning_needs_response");
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_dp_5a", "charge.dispute.created", created)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(s.order_status(&order).await, "paid");
    assert!(s.downloads(id, &buyer).await);
    {
        let f = st.lock().unwrap();
        assert!(f.reversals.is_empty() && f.fee_refunds.is_empty() && f.transfers.is_empty());
    }
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 3);
    // a dispute on a payment that isn't MakerFoundry's (StitchFoundry's, in the shared account)
    // changes nothing
    let other = dispute_json("dp_sf_1", "pi_sf_1", 1000, "needs_response");
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_dp_sf", "charge.dispute.created", other)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE dispute_id = 'dp_sf_1'")
            .await,
        0
    );
}

// ---------------------------------------------------------------- issue #91: Stripe hardening

impl Site {
    /// A Stripe event made at `created` (Unix seconds), signed.
    async fn stripe_event_at(&self, id: &str, kind: &str, object: Value, created: i64) -> Answer {
        let body = json!({"id": id, "object": "event", "type": kind, "created": created, "data": {"object": object}})
            .to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let sig = makerfoundry::stripe::signature(WEBHOOK_SECRET, now, body.as_bytes());
        self.send(
            Request::post("/stripe/webhook")
                .header(header::HOST, HOST)
                .header(header::CONTENT_TYPE, "application/json")
                .header("stripe-signature", format!("t={now},v1={sig}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }
}

/// A charge as `charge.refunded` describes it: `so_far` of 500 refunded.
fn refunded_charge(so_far: i64) -> Value {
    json!({"id": "ch_pi_cs_test_1", "object": "charge", "payment_intent": "pi_cs_test_1", "amount": 500,
        "amount_refunded": so_far, "refunded": so_far >= 500})
}

#[tokio::test]
async fn refunds_made_in_the_stripe_dashboard_take_back_from_the_seller_once() {
    let Some((s, st, _seller, id, state, _sent)) = disputes_site().await else {
        return;
    };
    let (buyer, order) = paid_sale(&s, &st, id).await;
    // $2 refunded in the Dashboard without "Reverse transfer", while Stripe can't reverse
    st.lock().unwrap().reversals_fail = true;
    st.lock()
        .unwrap()
        .listed_refunds
        .push(json!({"id": "re_dash_1", "object": "refund", "amount": 200,
        "charge": "ch_pi_cs_test_1", "status": "succeeded", "transfer_reversal": null, "metadata": {}}));
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_rf_1", "charge.refunded", refunded_charge(200))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // a partial refund: the sale goes on, the refunded part is the seller's loss in the ledger
    assert_eq!(s.order_status(&order).await, "paid");
    assert!(s.downloads(id, &buyer).await);
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE reason = 'partial_refund'")
            .await,
        -200
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE refund_check_due").await,
        1
    );
    // Stripe answers again: the sweep reverses the $2 and gives 20¢ of the 10% back, once
    st.lock().unwrap().reversals_fail = false;
    makerfoundry::refunds::check_all(&state, &s.pool).await.unwrap();
    makerfoundry::refunds::check_all(&state, &s.pool).await.unwrap();
    s.stripe_event(WEBHOOK_SECRET, "evt_rf_1", "charge.refunded", refunded_charge(200))
        .await;
    s.stripe_event(WEBHOOK_SECRET, "evt_rf_1b", "charge.refunded", refunded_charge(200))
        .await;
    {
        let f = st.lock().unwrap();
        assert_eq!(f.reversals.len(), 1);
        assert_eq!(f.reversals[0].1["amount"], "200");
        assert_eq!(f.reversals[0].1["metadata[reason]"], "refund_re_dash_1");
        assert_eq!(f.fee_refunds.len(), 1);
        assert_eq!(f.fee_refunds[0].1["amount"], "20");
    }
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE refund_check_due").await,
        0
    );
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        405 - 200 + 20
    );
    // the rest refunded in the Dashboard, this time with "Reverse transfer" ticked: Stripe reversed
    // it, so MakerFoundry doesn't; the sale is refunded and the rest of the 10% goes back
    st.lock()
        .unwrap()
        .listed_refunds
        .push(json!({"id": "re_dash_2", "object": "refund", "amount": 300,
        "charge": "ch_pi_cs_test_1", "status": "succeeded", "transfer_reversal": "trr_dash_2", "metadata": {}}));
    let r = s
        .stripe_event(WEBHOOK_SECRET, "evt_rf_2", "charge.refunded", refunded_charge(500))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(s.order_status(&order).await, "refunded");
    assert!(!s.downloads(id, &buyer).await);
    {
        let f = st.lock().unwrap();
        assert_eq!(f.reversals.len(), 1);
        let back: Vec<&str> = f.fee_refunds.iter().map(|(_, r)| r["amount"].as_str()).collect();
        assert_eq!(back, ["20", "30"]);
    }
    // as an admin's full refund leaves it: the seller out the card fee, the buyer paid back
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_ledger WHERE kind = 'seller'")
            .await,
        -45
    );
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_ledger").await, 0);
    // an older, smaller total arriving late changes nothing
    s.stripe_event(WEBHOOK_SECRET, "evt_rf_0", "charge.refunded", refunded_charge(100))
        .await;
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_ledger").await, 0);
}

#[tokio::test]
async fn an_older_account_update_never_overwrites_a_newer_one() {
    let Some((s, _st, seller, _id)) = selling_site(vec![1001]).await else {
        return;
    };
    let admin = s.sign_up("ada%40example.com").await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // Stripe pauses the account (newer), then an older "all fine" arrives late
    let paused = json!({"id": "acct_test0", "object": "account", "details_submitted": true, "charges_enabled": false,
        "payouts_enabled": false, "requirements": {"disabled_reason": "requirements.past_due"}});
    let fine = json!({"id": "acct_test0", "object": "account", "details_submitted": true, "charges_enabled": true,
        "payouts_enabled": true, "requirements": {"disabled_reason": null}});
    assert_eq!(
        s.stripe_event_at("evt_acct_new", "account.updated", paused, now + 100)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        s.stripe_event_at("evt_acct_old", "account.updated", fine.clone(), now + 50)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        s.scalar(
            "SELECT count(*) FROM mf_sellers WHERE NOT charges_enabled AND disabled_reason = 'requirements.past_due'"
        )
        .await,
        1
    );
    // the seller and the admins are told why
    let page = s.get("/account/selling", Some(&seller)).await.body;
    assert!(page.contains("Stripe needs more information from you"), "{page}");
    assert!(page.contains("requirements.past_due"), "{page}");
    let list = s.get("/admin/orders", Some(&admin)).await.body;
    assert!(list.contains("Sellers Stripe has paused"), "{list}");
    // a newer one applies
    s.stripe_event_at("evt_acct_newer", "account.updated", fine, now + 200)
        .await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_sellers WHERE charges_enabled AND disabled_reason = ''")
            .await,
        1
    );
    assert!(
        !s.get("/admin/orders", Some(&admin))
            .await
            .body
            .contains("Sellers Stripe has paused")
    );
}

#[tokio::test]
async fn an_admin_sets_the_payout_delay_on_sellers_made_before_it() {
    let Some((s, st, seller, _id)) = selling_site(vec![1001]).await else {
        return;
    };
    let admin = s.sign_up("ada%40example.com").await;
    // a seller set up before the delay existed
    sqlx::query("UPDATE mf_sellers SET payout_delay_days = NULL")
        .execute(&s.pool)
        .await
        .unwrap();
    let set = || {
        st.lock()
            .unwrap()
            .posts
            .iter()
            .filter(|(p, _)| p == "/v1/accounts/acct_test0")
            .map(|(_, f)| f.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        s.post("/admin/sellers/payout-delay", "", Some(&seller)).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(set().is_empty());
    let r = s.post("/admin/sellers/payout-delay", "", Some(&admin)).await;
    assert!(r.body.contains("payout delay is set on 1 more seller"), "{}", r.body);
    let f = set();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0]["settings[payouts][schedule][delay_days]"], "14");
    assert_eq!(f[0]["settings[payouts][schedule][interval]"], "monthly");
    assert_eq!(f[0]["settings[payouts][schedule][monthly_anchor]"], "1");
    // pressed again: nothing left to change
    let r = s.post("/admin/sellers/payout-delay", "", Some(&admin)).await;
    assert!(r.body.contains("set on 0 more"), "{}", r.body);
    assert_eq!(set().len(), 1);
}

#[tokio::test]
async fn without_stripe_every_design_is_free() {
    let Some(s) = site().await else { return };
    let cookie = s.sign_up("ann%40example.com").await;
    let id = published(&s, &cookie, "anns", "Rose").await;
    assert!(
        s.get("/account/selling", Some(&cookie))
            .await
            .body
            .contains("isn't switched on")
    );
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Rose&licence=personal&price=3",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("first set up selling"), "{}", r.body);
    assert_eq!(
        s.post("/stripe/webhook", "{}", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(s.get(&format!("/d/{id}"), None).await.body.contains("Download free"));
}

// ---------------------------------------------------------------- MF-6: the launch

#[tokio::test]
async fn before_the_launch_the_legal_pages_are_for_admins_only() {
    let Some(s) = site_with(vec![1000]).await else { return };
    let admin = s.sign_up("ada%40example.com").await;
    let other = s.sign_up("bea%40example.com").await;
    for path in ["/terms", "/privacy", "/seller-terms"] {
        let public = s.get(path, None).await;
        assert_eq!(public.status, StatusCode::OK);
        assert!(public.body.contains("hasn't opened yet"), "{}", public.body);
        assert!(!public.body.contains("OWNER"));
        assert!(!s.get(path, Some(&other)).await.body.contains("OWNER"));
        // admins see the document itself, marked as a draft while the owner has decisions left
        let draft = s.get(path, Some(&admin)).await.body;
        assert!(!draft.contains("hasn't opened yet"), "{draft}");
        let source = std::fs::read_to_string(format!(
            "{}/../../docs/legal/{}.md",
            env!("CARGO_MANIFEST_DIR"),
            path.trim_start_matches('/')
        ))
        .unwrap();
        let unfinished = ["[OWNER", "[When", "DRAFT for the owner"]
            .iter()
            .any(|m| source.contains(m));
        assert_eq!(draft.contains("A draft for the owner"), unfinished, "{draft}");
        assert_eq!(draft.contains("<mark>[OWNER"), source.contains("[OWNER"), "{draft}");
    }
    // no agreement asked for, search engines kept out, and still "coming soon"
    assert!(!s.get("/signup", None).await.body.contains("name=\"agree\""));
    assert_eq!(s.scalar("SELECT count(*) FROM mf_agreements").await, 0);
    assert_eq!(s.get("/robots.txt", None).await.body, "User-agent: *\nDisallow: /\n");
    assert_eq!(s.get("/sitemap.xml", None).await.status, StatusCode::NOT_FOUND);
    let home = s.get("/", None).await.body;
    assert!(
        home.contains("Become a founding creator") && home.contains(r#"href="/founding""#),
        "{home}"
    );
    assert!(home.contains("100 of 100 spots left."), "{home}");
}

#[tokio::test]
async fn once_launched_people_agree_to_the_terms_and_the_site_is_found() {
    let Some(s) = site_opts(Vec::new(), None, true).await else {
        return;
    };
    let terms = s.get("/terms", None).await.body;
    assert!(terms.contains("<h1>Terms of Service: MakerFoundry</h1>"), "{terms}");
    // signing up asks for agreement to the Terms and the Privacy Policy, and keeps it
    let form = s.get("/signup", None).await.body;
    assert!(form.contains(r#"name="agree""#) && form.contains(r#"href="/terms""#));
    let r = s
        .post(
            "/signup",
            "email=ann%40example.com&password=fire+truck+1&again=fire+truck+1",
            None,
        )
        .await;
    assert!(r.body.contains("agree to the Terms"), "{}", r.body);
    let cookie = s
        .post(
            "/signup",
            "email=ann%40example.com&password=fire+truck+1&again=fire+truck+1&agree=yes",
            None,
        )
        .await
        .cookie();
    let agreed: Vec<(String, String)> =
        sqlx::query_as("SELECT document, version FROM mf_agreements WHERE account_id = 1000 ORDER BY document")
            .fetch_all(&s.pool)
            .await
            .unwrap();
    assert_eq!(
        agreed,
        [
            (
                "privacy".to_string(),
                makerfoundry::legal::PRIVACY.version().to_string()
            ),
            ("terms".to_string(), makerfoundry::legal::TERMS.version().to_string()),
        ]
    );
    // publishing is agreeing to the Seller Terms
    let id = published(&s, &cookie, "anns", "Rose+Spray").await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_agreements WHERE document = 'seller-terms'")
            .await,
        1
    );
    // the home page invites people in; search engines are welcome, with a map
    let home = s.get("/", None).await.body;
    assert!(
        home.contains("Browse designs")
            && !home.contains("opens soon")
            && home.contains(r#"href="/founding">Become a founding creator"#),
        "{home}"
    );
    let robots = s.get("/robots.txt", None).await.body;
    assert!(robots.contains("Disallow: /account") && robots.contains("Sitemap: https://makerfoundry.test/sitemap.xml"));
    let map = s.get("/sitemap.xml", None).await.body;
    assert!(
        map.contains(&format!("<loc>https://makerfoundry.test/d/{id}/rose-spray</loc>")),
        "{map}"
    );
    assert!(map.contains("<loc>https://makerfoundry.test/makers/anns</loc>"));
}

// ---------------------------------------------------------------- MF-7: publishing from the apps

impl Site {
    /// A request to the apps' API: `body` as JSON, with the app's Foundry Account `token`.
    async fn api(&self, method: &str, path: &str, token: Option<&str>, origin: Option<&str>, body: &Value) -> Answer {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(o) = origin {
            req = req.header(header::ORIGIN, o);
        }
        self.send(req.body(Body::from(body.to_string())).unwrap()).await
    }
}

#[tokio::test]
async fn an_app_sends_a_design_which_opens_as_a_draft_to_finish_on_the_site() {
    use base64::Engine;
    let Some(s) = site().await else { return };
    let b64 = base64::engine::general_purpose::STANDARD;
    let cookie = s.sign_up("ann%40example.com").await;
    // the app's own sign-in to the same Foundry Account
    s.fake.lock().unwrap().sessions.insert("sfs_app".into(), 1000);
    let design = json!({
        "file_name": "C:\\Users\\ann\\rose.pes",
        "file": b64.encode(sample(sf_formats::Format::Pes)),
        "photos": [b64.encode(photo_with_location())],
        "app": "StitchFoundry 0.9.0",
    });
    // without a sign-in, or with one that has ended (or only the site's cookie): refused
    for token in [None, Some("sfs_nope")] {
        let r = s.api("POST", "/api/v1/designs", token, None, &design).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{}", r.body);
        assert!(r.body.contains("Foundry Account"), "{}", r.body);
    }
    let mut req = Request::post("/api/v1/designs").header(header::HOST, HOST);
    req = req.header(header::COOKIE, &cookie);
    assert_eq!(
        s.send(req.body(Body::from(design.to_string())).unwrap()).await.status,
        StatusCode::UNAUTHORIZED
    );
    // a file that isn't a design, or isn't base64
    let mut junk = design.clone();
    junk["file"] = json!(b64.encode(b"not a design at all"));
    let r = s.api("POST", "/api/v1/designs", Some("sfs_app"), None, &junk).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    junk["file"] = json!("%%%");
    assert_eq!(
        s.api("POST", "/api/v1/designs", Some("sfs_app"), None, &junk)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 0);
    // sent: a draft of Ann's, read, with its photo
    let r = s.api("POST", "/api/v1/designs", Some("sfs_app"), None, &design).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    let id = v["id"].as_i64().unwrap();
    assert_eq!(v["url"], format!("https://makerfoundry.test/designs/{id}"));
    assert_eq!(
        (v["kind"].as_str(), v["format"].as_str()),
        (Some("embroidery"), Some("PES"))
    );
    let (owner, name, status): (i64, String, String) =
        sqlx::query_as("SELECT account_id, file_name, status FROM mf_designs WHERE id = $1")
            .bind(id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((owner, name.as_str(), status.as_str()), (1000, "rose.pes", "draft"));
    assert_eq!(s.scalar("SELECT count(*) FROM mf_design_photos").await, 1);
    assert_eq!(s.stored.lock().unwrap().len(), 3);
    // the app opens its page: signed in, the draft to finish; signed out, sign in and come back
    let page = s.get(&format!("/designs/{id}"), Some(&cookie)).await;
    assert!(page.body.contains("Edit details"), "{}", page.body);
    let r = s.get(&format!("/designs/{id}"), None).await;
    assert_eq!(r.location(), format!("/signin?next=/designs/{id}"));
    let form = s.get(&format!("/signin?next=/designs/{id}"), None).await.body;
    assert!(
        form.contains(&format!(r#"name="next" value="/designs/{id}""#)),
        "{form}"
    );
    let r = s
        .post(
            "/signin",
            &format!("email=ann%40example.com&password=fire+truck+1&next=%2Fdesigns%2F{id}"),
            None,
        )
        .await;
    assert_eq!(r.location(), format!("/designs/{id}"));
    // only to pages of this site
    for bad in ["%2F%2Fevil.example", "https%3A%2F%2Fevil.example", "%2F%5Cevil.example"] {
        let r = s
            .post(
                "/signin",
                &format!("email=ann%40example.com&password=fire+truck+1&next={bad}"),
                None,
            )
            .await;
        assert_eq!(r.location(), "/account", "{bad}");
    }
}

#[tokio::test]
async fn the_web_apps_may_call_the_api_from_their_own_sites_only() {
    let Some(s) = site().await else { return };
    let ok = s
        .api(
            "OPTIONS",
            "/api/v1/designs",
            None,
            Some("https://stitchfoundry.test"),
            &json!({}),
        )
        .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT);
    assert_eq!(
        ok.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://stitchfoundry.test"
    );
    assert!(
        ok.headers[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .unwrap()
            .contains("authorization")
    );
    assert!(ok.headers.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());
    let evil = s
        .api(
            "OPTIONS",
            "/api/v1/designs",
            None,
            Some("https://evil.example"),
            &json!({}),
        )
        .await;
    assert_eq!(evil.status, StatusCode::FORBIDDEN);
    assert!(evil.headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    // answers carry it too, errors included, so the web app can show the message
    let r = s
        .api(
            "POST",
            "/api/v1/designs",
            None,
            Some("https://stitchfoundry.test"),
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        r.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://stitchfoundry.test"
    );
    let about = s.api("GET", "/api/v1", None, None, &json!({})).await;
    assert!(about.body.contains(r#""designs":"/api/v1/designs""#), "{}", about.body);
}

// ---------------------------------------------------------------- MF-8: finishing the first version

#[tokio::test]
async fn browsing_finds_free_or_paid_designs_and_unpaid_checkouts_expire() {
    let Some((s, st, seller, paid)) = selling_site(Vec::new()).await else {
        return;
    };
    let free = published(&s, &seller, "sams-shop", "Owl+Patch").await;
    let free_list = s.get("/browse?price=free", None).await.body;
    assert!(free_list.contains(&format!("/d/{free}/")) && !free_list.contains(&format!("/d/{paid}/")));
    let paid_list = s.get("/browse?price=paid", None).await.body;
    assert!(paid_list.contains(&format!("/d/{paid}/")) && !paid_list.contains(&format!("/d/{free}/")));
    assert!(paid_list.contains(r#"<option value="paid" selected>"#), "{paid_list}");
    let both = s.get("/browse", None).await.body;
    assert!(both.contains(&format!("/d/{free}/")) && both.contains(&format!("/d/{paid}/")));
    // two purchases started and never paid for: Stripe's page lasts 2 hours
    let buyer = s.sign_up("bea%40example.com").await;
    s.post(&format!("/d/{paid}/buy"), "", Some(&buyer)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    let expires: i64 = form["expires_at"].parse().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!((expires - now - 7200).abs() < 60, "{expires}");
    s.post(&format!("/d/{paid}/buy"), "", Some(&buyer)).await;
    // Stripe says the first page expired
    let r = s
        .stripe_event(
            WEBHOOK_SECRET,
            "evt_x",
            "checkout.session.expired",
            json!({"id": "cs_test_1", "object": "checkout.session"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE status = 'expired'")
            .await,
        1
    );
    // the second, a day on, expires anyway
    assert_eq!(makerfoundry::selling::expire_orders(&s.pool).await.unwrap(), 0);
    sqlx::query("UPDATE mf_orders SET created_at = now() - interval '25 hours' WHERE status = 'pending'")
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(makerfoundry::selling::expire_orders(&s.pool).await.unwrap(), 1);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE status = 'pending'")
            .await,
        0
    );
    // neither counts as a sale, and the design can still be deleted once unpublished
    assert!(
        s.get("/account/sales", Some(&seller))
            .await
            .body
            .contains("No sales yet")
    );
    s.post(&format!("/designs/{paid}/unpublish"), "", Some(&seller)).await;
    let r = s.post(&format!("/designs/{paid}/delete"), "", Some(&seller)).await;
    assert_eq!(r.location(), "/designs", "{}", r.body);
}

#[tokio::test]
async fn a_delayed_payment_confirmed_after_the_order_expired_is_still_delivered() {
    let Some((s, st, _seller, paid)) = selling_site(Vec::new()).await else {
        return;
    };
    let buyer = s.sign_up("bea%40example.com").await;
    s.post(&format!("/d/{paid}/buy"), "", Some(&buyer)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    // a bank debit: the checkout completes unpaid, and Stripe confirms the money days later
    // (https://docs.stripe.com/payments/checkout/fulfill-orders)
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_done",
        "checkout.session.completed",
        session_json("cs_test_1", &form, false),
    )
    .await;
    sqlx::query("UPDATE mf_orders SET created_at = now() - interval '3 days'")
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(makerfoundry::selling::expire_orders(&s.pool).await.unwrap(), 1);
    let r = s
        .stripe_event(
            WEBHOOK_SECRET,
            "evt_paid",
            "checkout.session.async_payment_succeeded",
            session_json("cs_test_1", &form, true),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // the buyer paid: the sale is recorded and the design is theirs
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE status = 'paid'").await,
        1
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_ledger").await, 3);
    let r = s.get(&format!("/d/{paid}/download"), Some(&buyer)).await;
    assert!(r.location().contains("/files/"), "{}", r.location());
}

#[tokio::test]
async fn designs_are_found_by_the_hoop_they_fit_and_the_material_they_cut() {
    let Some(s) = site().await else { return };
    let cookie = s.sign_up("ann%40example.com").await;
    // the sample PES is 60 × 60 mm
    let rose = published(&s, &cookie, "anns", "Rose").await;
    assert!(
        s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains("fits a 4×4 in hoop")
    );
    // a laser design, 300 × 200 mm, made for 3 mm plywood
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="300mm" height="200mm" viewBox="0 0 300 200"><rect x="1" y="1" width="298" height="198" fill="none" stroke="red"/></svg>"#;
    let r = s
        .send(multipart("/designs/new", &[("file", "box.svg", svg.to_vec())], &cookie))
        .await;
    let boxed = design_id(r.location());
    let edit = format!("/designs/{boxed}/edit");
    assert!(
        s.get(&edit, Some(&cookie))
            .await
            .body
            .contains(r#"<select name="material">"#)
    );
    let r = s
        .post(
            &edit,
            "title=Box&licence=personal&price=0&material=plywood&thickness=sixty",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("thickness in mm"), "{}", r.body);
    let r = s
        .post(
            &edit,
            "title=Box&licence=personal&price=0&material=gold&thickness=3",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("Choose a material"), "{}", r.body);
    let r = s
        .post(
            &edit,
            "title=Box&licence=personal&price=0&material=plywood&thickness=3",
            Some(&cookie),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    s.post(&format!("/designs/{boxed}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert!(
        s.get(&format!("/d/{boxed}"), None)
            .await
            .body
            .contains("for 3 mm plywood")
    );
    // an embroidery design has no material, whatever is sent
    s.post(
        &format!("/designs/{rose}/edit"),
        "title=Rose&licence=personal&price=0&material=acrylic",
        Some(&cookie),
    )
    .await;
    assert!(
        !s.get(&format!("/designs/{rose}/edit"), Some(&cookie))
            .await
            .body
            .contains("Material")
    );
    // browsing
    let has = |body: &str, id: i64| body.contains(&format!("/d/{id}/"));
    let fits = s.get("/browse?hoop=4x4", None).await.body;
    assert!(has(&fits, rose) && !has(&fits, boxed));
    assert!(fits.contains(r#"<option value="4x4" selected>"#));
    let ply = s.get("/browse?material=plywood", None).await.body;
    assert!(has(&ply, boxed) && !has(&ply, rose));
    let acrylic = s.get("/browse?material=acrylic", None).await.body;
    assert!(!has(&acrylic, boxed) && !has(&acrylic, rose));
    // nonsense is ignored
    let all = s.get("/browse?hoop=99x99&material=gold", None).await.body;
    assert!(has(&all, boxed) && has(&all, rose));
    // only the chosen kind's filters show (#110)
    let laser = s.get("/browse?kind=laser", None).await.body;
    assert!(laser.contains(r#"name="material""#) && !laser.contains(r#"name="hoop""#));
    assert!(has(&laser, boxed) && !has(&laser, rose));
    let emb = s.get("/browse?kind=embroidery", None).await.body;
    assert!(emb.contains(r#"name="hoop""#) && !emb.contains(r#"name="material""#));
    let both = s.get("/browse", None).await.body;
    assert!(!both.contains(r#"name="hoop""#) && !both.contains(r#"name="material""#));
    assert!(both.contains(r#"<a href="/browse?q=&amp;kind=laser"#));
    // a filter from another kind is ignored, and the tab for the kind is marked
    let ignored = s.get("/browse?kind=embroidery&material=acrylic", None).await.body;
    assert!(has(&ignored, rose) && !has(&ignored, boxed));
    let ignored = s.get("/browse?kind=laser&hoop=4x4", None).await.body;
    assert!(has(&ignored, boxed) && !has(&ignored, rose));
    assert!(ignored.contains(r#"kind=laser&amp;price=&amp;sort=newest" aria-current="page""#));
    // 3D print is not a tab until models are open, and kind=3d is ignored until then
    assert!(!both.contains("3D print"));
    let three = s.get("/browse?kind=3d", None).await.body;
    assert!(has(&three, boxed) && has(&three, rose));
}

#[tokio::test]
async fn a_design_has_more_formats_each_checked_and_downloadable() {
    let Some(s) = site().await else { return };
    let cookie = s.sign_up("ann%40example.com").await;
    let rose = published(&s, &cookie, "anns", "Rose").await;
    let other = add_design(&s, &cookie, sf_formats::Format::Pes, "other.pes").await;
    let add = |name: &'static str, bytes: Vec<u8>| {
        multipart(&format!("/designs/{rose}/files"), &[("file", name, bytes)], &cookie)
    };
    // the same design as a DST: read, checked and kept
    let r = s.send(add("rose.dst", sample(sf_formats::Format::Dst))).await;
    assert_eq!(r.location(), format!("/designs/{rose}"), "{}", r.body);
    // a laser file isn't another format of an embroidery design, and junk isn't a file at all
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10mm" height="10mm"><rect width="5" height="5" stroke="red" fill="none"/></svg>"#;
    assert!(
        s.send(add("rose.svg", svg.to_vec()))
            .await
            .body
            .contains("This is an embroidery design")
    );
    let junk = s.send(add("rose.jef", b"not a design at all".to_vec())).await;
    assert!(
        junk.body.contains("rose.jef"),
        "{} {} {}",
        junk.status,
        junk.location(),
        junk.body
    );
    // one file a format
    assert!(
        s.send(add("again.pes", sample(sf_formats::Format::Pes)))
            .await
            .body
            .contains("already has a PES file")
    );
    assert!(
        s.send(add("again.dst", sample(sf_formats::Format::Dst)))
            .await
            .body
            .contains("already has a DST file")
    );
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_design_files WHERE design_id = $1")
        .bind(rose)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(s.scalar("SELECT count(*) FROM mf_design_files").await, 1);
    let manage = s.get(&format!("/designs/{rose}"), Some(&cookie)).await.body;
    assert!(
        manage.contains(&format!(r#"href="/d/{rose}/download?file={fid}">DST</a>"#)),
        "{manage}"
    );
    // on the public page, both formats, each downloadable
    let page = s.get(&format!("/d/{rose}"), None).await.body;
    assert!(
        page.contains(&format!(r#"<a href="/d/{rose}/download">PES</a>"#)),
        "{page}"
    );
    assert!(page.contains(&format!(r#"<a href="/d/{rose}/download?file={fid}">DST</a>"#)));
    let r = s.get(&format!("/d/{rose}/download?file={fid}"), None).await;
    assert!(
        r.location().contains("/files/") && r.location().contains("rose.dst"),
        "{}",
        r.location()
    );
    // a file of another design is not this design's
    assert_eq!(
        s.get(&format!("/d/{other}/download?file={fid}"), Some(&cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // others can't add or remove files
    let bea = s.sign_up("bea%40example.com").await;
    let r = s
        .send(multipart(
            &format!("/designs/{rose}/files"),
            &[("file", "x.dst", sample(sf_formats::Format::Dst))],
            &bea,
        ))
        .await;
    assert_eq!(r.location(), "/designs");
    s.post(&format!("/designs/{rose}/files/{fid}/delete"), "", Some(&bea))
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_design_files").await, 1);
    // removed by its maker: gone from storage too
    let stored = s.stored.lock().unwrap().len();
    s.post(&format!("/designs/{rose}/files/{fid}/delete"), "", Some(&cookie))
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_design_files").await, 0);
    assert_eq!(s.stored.lock().unwrap().len(), stored - 1);
    assert!(!s.get(&format!("/d/{rose}"), None).await.body.contains("download?file="));
    // a deleted design takes its other files with it
    s.send(add("rose.dst", sample(sf_formats::Format::Dst))).await;
    s.post(&format!("/designs/{rose}/unpublish"), "", Some(&cookie)).await;
    let before = s.stored.lock().unwrap().len();
    s.post(&format!("/designs/{rose}/delete"), "", Some(&cookie)).await;
    // the first file, its photo and thumbnail, and the DST
    assert_eq!(s.stored.lock().unwrap().len(), before - 4);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_design_files").await, 0);
}

#[tokio::test]
async fn a_paid_designs_other_formats_are_for_its_buyers() {
    let Some((s, st, seller, id)) = selling_site(Vec::new()).await else {
        return;
    };
    let r = s
        .send(multipart(
            &format!("/designs/{id}/files"),
            &[("file", "fox.dst", sample(sf_formats::Format::Dst))],
            &seller,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_design_files")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let buyer = s.sign_up("bea%40example.com").await;
    // before buying: the formats named, not linked, and no file
    let page = s.get(&format!("/d/{id}"), Some(&buyer)).await.body;
    assert!(
        page.contains("<li>DST</li>") && !page.contains(&format!("download?file={fid}")),
        "{page}"
    );
    assert!(
        !s.get(&format!("/d/{id}/download?file={fid}"), Some(&buyer))
            .await
            .location()
            .contains("/files/")
    );
    // bought: every format
    s.post(&format!("/d/{id}/buy"), "", Some(&buyer)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_p",
        "checkout.session.completed",
        session_json("cs_test_1", &form, true),
    )
    .await;
    assert!(
        s.get(&format!("/d/{id}"), Some(&buyer))
            .await
            .body
            .contains(&format!("download?file={fid}"))
    );
    assert!(
        s.get(&format!("/d/{id}/download?file={fid}"), Some(&buyer))
            .await
            .location()
            .contains("/files/")
    );
    // and once bought, the seller can't take a format away
    s.post(&format!("/designs/{id}/files/{fid}/delete"), "", Some(&seller))
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_design_files").await, 1);
    assert!(
        !s.get(&format!("/designs/{id}"), Some(&seller))
            .await
            .body
            .contains("/files/{fid}/delete")
    );
}

// ---------------------------------------------------------------- MF-9: converted downloads

#[tokio::test]
async fn an_embroidery_design_downloads_converted_to_the_other_formats() {
    let Some(s) = site().await else { return };
    let cookie = s.sign_up("ann%40example.com").await;
    let rose = published(&s, &cookie, "anns", "Rose").await;
    let page = s.get(&format!("/d/{rose}"), None).await.body;
    assert!(
        page.contains(&format!(r#"<a href="/d/{rose}/download?as=dst">DST</a>"#)),
        "{page}"
    );
    assert!(!page.contains("download?as=pes") && !page.contains("download?as=dsz"));
    // made on the spot, as a real DST
    let r = s.get(&format!("/d/{rose}/download?as=dst"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.headers[header::CONTENT_DISPOSITION],
        r#"attachment; filename="rose.dst""#
    );
    let resp = s
        .app
        .clone()
        .oneshot(
            Request::get(format!("/d/{rose}/download?as=jef"))
                .header(header::HOST, HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let back = sf_formats::read(sf_formats::Format::Jef, &bytes).unwrap();
    let original = sf_formats::read(sf_formats::Format::Pes, &sample(sf_formats::Format::Pes)).unwrap();
    // the same design in the other format: its stitches (long ones split where the format's
    // stitches are shorter) over the same size, to a tenth of a millimetre
    let sewn = |p: &sf_core::Pattern| p.stitches.iter().filter(|s| s.cmd == sf_core::Cmd::Stitch).count();
    assert!(sewn(&back) >= sewn(&original));
    let size = |p: &sf_core::Pattern| {
        let xs = p.stitches.iter().map(|s| s.x);
        let ys = p.stitches.iter().map(|s| s.y);
        (
            xs.clone().fold(f64::MIN, f64::max) - xs.fold(f64::MAX, f64::min),
            ys.clone().fold(f64::MIN, f64::max) - ys.fold(f64::MAX, f64::min),
        )
    };
    let ((w1, h1), (w2, h2)) = (size(&original), size(&back));
    assert!(
        (w1 - w2).abs() < 0.11 && (h1 - h2).abs() < 0.11,
        "{w1}x{h1} became {w2}x{h2}"
    );
    assert_eq!(
        s.scalar(&format!("SELECT downloads FROM mf_designs WHERE id = {rose}"))
            .await,
        2
    );
    // only formats that can be written
    for bad in ["dsz", "svg", "x.dst", ""] {
        assert_eq!(
            s.get(&format!("/d/{rose}/download?as={bad}"), None).await.status,
            StatusCode::NOT_FOUND,
            "{bad}"
        );
    }
    // a DST of its own: no longer converted to DST
    s.send(multipart(
        &format!("/designs/{rose}/files"),
        &[("file", "rose.dst", sample(sf_formats::Format::Dst))],
        &cookie,
    ))
    .await;
    assert!(
        !s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains("download?as=dst")
    );
    // saving the details without the box (another form) leaves it on; unticking it turns it off
    s.post(
        &format!("/designs/{rose}/edit"),
        "title=Rose&licence=personal&price=0",
        Some(&cookie),
    )
    .await;
    assert!(
        s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains("download?as=jef")
    );
    let form = s.get(&format!("/designs/{rose}/edit"), Some(&cookie)).await.body;
    assert!(form.contains(r#"name="convert" value="yes" checked"#), "{form}");
    s.post(
        &format!("/designs/{rose}/edit"),
        "title=Rose&licence=personal&price=0&convert_shown=yes",
        Some(&cookie),
    )
    .await;
    assert!(!s.get(&format!("/d/{rose}"), None).await.body.contains("download?as="));
    assert_eq!(
        s.get(&format!("/d/{rose}/download?as=jef"), None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_paid_designs_conversions_are_for_its_buyers() {
    let Some((s, _st, _seller, id)) = selling_site(Vec::new()).await else {
        return;
    };
    let buyer = s.sign_up("bea%40example.com").await;
    let page = s.get(&format!("/d/{id}"), Some(&buyer)).await.body;
    assert!(
        page.contains("<li>DST</li>") && !page.contains("download?as=dst"),
        "{page}"
    );
    let r = s.get(&format!("/d/{id}/download?as=dst"), Some(&buyer)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), format!("/d/{id}/fox-patch"));
}

// ---------------------------------------------------------------- MF-10: saved designs and collections

#[tokio::test]
async fn people_save_designs_and_keep_collections_public_or_private() {
    let Some(s) = site().await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let rose = published(&s, &ann, "anns", "Rose").await;
    let owl = published(&s, &ann, "anns", "Owl").await;
    let bea = s.sign_up("bea%40example.com").await;
    let cal = s.sign_up("cal%40example.com").await;
    // signed out: sign in first
    assert!(
        s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains("Sign in to save")
    );
    assert_eq!(
        s.post(&format!("/d/{rose}/save"), "", None).await.location(),
        format!("/signin?next=/d/{rose}")
    );
    // Bea saves the rose to her Saved list (twice is once)
    let r = s.post(&format!("/d/{rose}/save"), "collection=", Some(&bea)).await;
    assert_eq!(r.location(), format!("/d/{rose}/rose"));
    s.post(&format!("/d/{rose}/save"), "", Some(&bea)).await;
    let page = s.get(&format!("/d/{rose}"), Some(&bea)).await.body;
    assert!(
        page.contains("saved by 1 person") && page.contains("In your <a href="),
        "{page}"
    );
    // a public collection of her own, with the owl in it
    let r = s
        .post("/collections", "name=Christmas++ideas&public=yes", Some(&bea))
        .await;
    let christmas: i64 = r.location().trim_start_matches("/collections/").parse().unwrap();
    s.post(
        &format!("/d/{owl}/save"),
        &format!("collection={christmas}"),
        Some(&bea),
    )
    .await;
    let public = s.get(&format!("/collections/{christmas}"), None).await;
    assert_eq!(public.status, StatusCode::OK);
    assert!(public.body.contains("<h1>Christmas ideas</h1>") && public.body.contains(&format!("/d/{owl}/")));
    // her Saved list is hers alone
    let saved: i64 = sqlx::query_scalar("SELECT id FROM mf_collections WHERE is_saved")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert!(
        s.get(&format!("/collections/{saved}"), Some(&bea))
            .await
            .body
            .contains(&format!("/d/{rose}/"))
    );
    assert_eq!(
        s.get(&format!("/collections/{saved}"), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.get(&format!("/collections/{saved}"), Some(&ann)).await.status,
        StatusCode::NOT_FOUND
    );
    // nobody else adds to, takes from, renames or deletes it
    s.post(
        &format!("/d/{rose}/save"),
        &format!("collection={christmas}"),
        Some(&cal),
    )
    .await;
    s.post(&format!("/collections/{christmas}/remove/{owl}"), "", Some(&cal))
        .await;
    s.post(&format!("/collections/{christmas}/edit"), "name=Mine", Some(&cal))
        .await;
    s.post(&format!("/collections/{christmas}/delete"), "", Some(&cal))
        .await;
    let items: Vec<i64> = sqlx::query_scalar("SELECT design_id FROM mf_collection_items WHERE collection_id = $1")
        .bind(christmas)
        .fetch_all(&s.pool)
        .await
        .unwrap();
    assert_eq!(items, [owl]);
    assert!(
        s.get(&format!("/collections/{christmas}"), None)
            .await
            .body
            .contains("Christmas ideas")
    );
    // Cal saves the rose too: the most saved comes first
    s.post(&format!("/d/{rose}/save"), "", Some(&cal)).await;
    assert!(
        s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains("saved by 2 people")
    );
    let browse = s.get("/browse?sort=saved", None).await.body;
    let (r_at, o_at) = (
        browse.find(&format!("/d/{rose}/")).unwrap(),
        browse.find(&format!("/d/{owl}/")).unwrap(),
    );
    assert!(r_at < o_at);
    let browse = s.get("/browse", None).await.body;
    assert!(browse.find(&format!("/d/{owl}/")).unwrap() < browse.find(&format!("/d/{rose}/")).unwrap());
    // an unpublished design drops out of collections, and can't be saved
    s.post(&format!("/designs/{owl}/unpublish"), "", Some(&ann)).await;
    assert!(
        !s.get(&format!("/collections/{christmas}"), None)
            .await
            .body
            .contains(&format!("/d/{owl}/"))
    );
    assert_eq!(
        s.post(&format!("/d/{owl}/save"), "", Some(&cal)).await.location(),
        "/browse"
    );
    // a maker's public collections show on their page; private ones don't
    s.post("/collections", "name=Favourites&public=yes", Some(&ann)).await;
    s.post("/collections", "name=Secret", Some(&ann)).await;
    let maker = s.get("/makers/anns", None).await.body;
    assert!(maker.contains("Favourites") && !maker.contains("Secret"), "{maker}");
    // Bea makes hers private, then deletes it; her Saved list can't be deleted
    s.post(&format!("/collections/{christmas}/edit"), "name=Christmas", Some(&bea))
        .await;
    assert_eq!(
        s.get(&format!("/collections/{christmas}"), None).await.status,
        StatusCode::NOT_FOUND
    );
    s.post(&format!("/collections/{christmas}/delete"), "", Some(&bea))
        .await;
    s.post(&format!("/collections/{saved}/delete"), "", Some(&bea)).await;
    let left: Vec<String> = sqlx::query_scalar("SELECT name FROM mf_collections WHERE account_id = 1001")
        .fetch_all(&s.pool)
        .await
        .unwrap();
    assert_eq!(left, ["Saved"]);
    let mine = s.get("/collections", Some(&bea)).await.body;
    assert!(mine.contains("Saved") && mine.contains("1 design"), "{mine}");
}

// ---------------------------------------------------------------- MF-11: makes

#[tokio::test]
async fn people_post_photos_of_what_they_made_and_can_take_them_down() {
    let Some(s) = site().await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let rose = published(&s, &ann, "anns", "Rose").await;
    let bea = s.sign_up("bea%40example.com").await;
    let cal = s.sign_up("cal%40example.com").await;
    assert!(
        s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains("Sign in to post your make")
    );
    let make = |cookie: &str, confirm: &str, photo: Vec<u8>| {
        multipart(
            &format!("/d/{rose}/makes"),
            &[
                ("photo", "sewn.jpg", photo),
                ("note", "", "On linen, with <b>rayon</b> thread".as_bytes().to_vec()),
                ("confirm", "", confirm.as_bytes().to_vec()),
            ],
            cookie,
        )
    };
    // a maker profile first: it's the name shown
    assert!(
        s.send(make(&bea, "yes", photo_with_location()))
            .await
            .body
            .contains("maker profile first")
    );
    s.post("/account/maker", "name=Bea+Bakes&handle=bea", Some(&bea)).await;
    // her word that it's her own photo, and a real photo
    assert!(
        s.send(make(&bea, "", photo_with_location()))
            .await
            .body
            .contains("Confirm")
    );
    assert!(
        s.send(make(&bea, "yes", b"not a photo".to_vec()))
            .await
            .body
            .contains("The photo:")
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_makes").await, 0);
    let stored = s.stored.lock().unwrap().len();
    let r = s.send(make(&bea, "yes", photo_with_location())).await;
    assert_eq!(r.location(), format!("/d/{rose}/rose"), "{}", r.body);
    assert_eq!(s.stored.lock().unwrap().len(), stored + 2);
    let mid: i64 = sqlx::query_scalar("SELECT id FROM mf_makes")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    // on the design's page, for everyone, the note escaped
    let page = s.get(&format!("/d/{rose}"), None).await.body;
    assert!(
        page.contains("1 make") && page.contains(&format!(r#"src="/makes/{mid}/thumb""#)),
        "{page}"
    );
    assert!(
        page.contains("On linen, with &lt;b&gt;rayon&lt;/b&gt; thread")
            && page.contains(r#"<a href="/makers/bea">Bea Bakes</a>"#)
    );
    let photo = s.get(&format!("/makes/{mid}/large"), None).await;
    assert_eq!(
        (photo.status, photo.headers[header::CONTENT_TYPE].to_str().unwrap()),
        (StatusCode::OK, "image/jpeg")
    );
    assert_eq!(
        s.get(&format!("/makes/{mid}/huge"), None).await.status,
        StatusCode::NOT_FOUND
    );
    // Cal can't take it down; Ann (the design's maker) can
    s.post(&format!("/makes/{mid}/delete"), "", Some(&cal)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_makes").await, 1);
    assert!(
        !s.get(&format!("/d/{rose}"), Some(&cal))
            .await
            .body
            .contains("/delete\"")
    );
    assert!(
        s.get(&format!("/d/{rose}"), Some(&ann))
            .await
            .body
            .contains(&format!("/makes/{mid}/delete"))
    );
    s.post(&format!("/makes/{mid}/delete"), "", Some(&ann)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_makes").await, 0);
    assert_eq!(s.stored.lock().unwrap().len(), stored);
    // Bea's own, taken down by her; an unpublished design's makes aren't shown
    s.send(make(&bea, "yes", photo_with_location())).await;
    let mid: i64 = sqlx::query_scalar("SELECT id FROM mf_makes")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/designs/{rose}/unpublish"), "", Some(&ann)).await;
    assert_eq!(
        s.get(&format!("/makes/{mid}/thumb"), None).await.status,
        StatusCode::NOT_FOUND
    );
    s.post(&format!("/designs/{rose}/publish"), "confirm=yes", Some(&ann))
        .await;
    s.post(&format!("/makes/{mid}/delete"), "", Some(&bea)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_makes").await, 0);
    // a deleted design takes its makes' photos with it
    s.send(make(&bea, "yes", photo_with_location())).await;
    s.post(&format!("/designs/{rose}/unpublish"), "", Some(&ann)).await;
    let before = s.stored.lock().unwrap().len();
    s.post(&format!("/designs/{rose}/delete"), "", Some(&ann)).await;
    // the file, its photo and thumbnail, and the make's photo and thumbnail
    assert_eq!(s.stored.lock().unwrap().len(), before - 5);
}

#[tokio::test]
async fn a_reported_make_is_taken_down_from_the_admin_queue() {
    let Some(s) = site_with(vec![1003]).await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let rose = published(&s, &ann, "anns", "Rose").await;
    let bea = s.sign_up("bea%40example.com").await;
    let cal = s.sign_up("cal%40example.com").await;
    let ada = s.sign_up("ada%40example.com").await;
    s.post("/account/maker", "name=Bea&handle=bea", Some(&bea)).await;
    let post = || {
        multipart(
            &format!("/d/{rose}/makes"),
            &[
                ("photo", "a.jpg", photo_with_location()),
                ("confirm", "", b"yes".to_vec()),
            ],
            &bea,
        )
    };
    s.send(post()).await;
    let mid: i64 = sqlx::query_scalar("SELECT id FROM mf_makes")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    // others see a Report link; its poster sees Take down instead
    assert!(
        s.get(&format!("/d/{rose}"), Some(&cal))
            .await
            .body
            .contains(&format!("/makes/{mid}/report"))
    );
    assert!(
        !s.get(&format!("/d/{rose}"), Some(&bea))
            .await
            .body
            .contains(&format!("/makes/{mid}/report"))
    );
    assert!(
        s.get(&format!("/makes/{mid}/report"), None)
            .await
            .body
            .contains("Report a make")
    );
    let r = s.post(&format!("/makes/{mid}/report"), "details=x", None).await;
    assert!(r.body.contains("Choose what the problem is"), "{}", r.body);
    let r = s
        .post(
            &format!("/makes/{mid}/report"),
            "reason=not-theirs&details=My+photo+from+my+blog",
            None,
        )
        .await;
    assert!(r.body.contains("Your report is in"), "{}", r.body);
    // in the queue, with the photo and its own action
    let queue = s.get("/admin", Some(&ada)).await.body;
    assert!(
        queue.contains("It isn&#39;t their photo") && queue.contains(&format!(r#"src="/makes/{mid}/thumb""#)),
        "{queue}"
    );
    let rid: i64 = sqlx::query_scalar("SELECT id FROM mf_reports")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert!(queue.contains(&format!("/admin/reports/{rid}/remove-make")));
    // only admins act on it
    assert_eq!(
        s.post(&format!("/admin/reports/{rid}/remove-make"), "", Some(&cal))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_makes").await, 1);
    let stored = s.stored.lock().unwrap().len();
    s.post(&format!("/admin/reports/{rid}/remove-make"), "", Some(&ada))
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_makes").await, 0);
    assert_eq!(s.stored.lock().unwrap().len(), stored - 2);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_reports WHERE status = 'actioned'")
            .await,
        1
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_moderation_log WHERE action = 'remove-make'")
            .await,
        1
    );
    // a report of a make its poster has since taken down: just closed
    s.send(post()).await;
    let mid: i64 = sqlx::query_scalar("SELECT id FROM mf_makes")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/makes/{mid}/report"), "reason=spam", Some(&cal)).await;
    s.post(&format!("/makes/{mid}/delete"), "", Some(&bea)).await;
    let queue = s.get("/admin", Some(&ada)).await.body;
    assert!(
        queue.contains("taken down already") && !queue.contains("remove-make"),
        "{queue}"
    );
    assert_eq!(
        s.get(&format!("/makes/{mid}/report"), None).await.status,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------- MF-13: following makers

#[tokio::test]
async fn following_a_maker_shows_their_new_designs() {
    let Some(s) = site().await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let rose = published(&s, &ann, "anns", "Rose").await;
    let cal = s.sign_up("cal%40example.com").await;
    let owl = published(&s, &cal, "cals", "Owl").await;
    let bea = s.sign_up("bea%40example.com").await;
    assert!(s.get("/makers/anns", None).await.body.contains("Sign in to follow"));
    assert!(
        s.get("/following", Some(&bea))
            .await
            .body
            .contains("don't follow any makers")
    );
    // Bea follows Ann (twice is once); Ann can't follow herself
    assert_eq!(
        s.post("/makers/anns/follow", "", Some(&bea)).await.location(),
        "/makers/anns"
    );
    s.post("/makers/anns/follow", "", Some(&bea)).await;
    s.post("/makers/anns/follow", "", Some(&ann)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_follows").await, 1);
    let page = s.get("/makers/anns", Some(&bea)).await.body;
    assert!(
        page.contains("1 follower") && page.contains("/makers/anns/unfollow"),
        "{page}"
    );
    assert!(!s.get("/makers/anns", Some(&ann)).await.body.contains("/follow\""));
    // her Following page: Ann's designs, not Cal's
    let feed = s.get("/following", Some(&bea)).await.body;
    assert!(
        feed.contains(&format!("/d/{rose}/")) && !feed.contains(&format!("/d/{owl}/")),
        "{feed}"
    );
    assert!(feed.contains(r#"<a href="/makers/anns">Maker</a>"#));
    assert_eq!(s.get("/following", None).await.location(), "/signin?next=/following");
    // a form from elsewhere, or a maker who isn't there, changes nothing
    let mut req = Request::post("/makers/cals/follow").header(header::HOST, HOST);
    req = req
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, &bea);
    assert_eq!(
        s.send(req.body(Body::empty()).unwrap()).await.status,
        StatusCode::FORBIDDEN
    );
    s.post("/makers/nobody-here/follow", "", Some(&bea)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_follows").await, 1);
    // unfollowed
    s.post("/makers/anns/unfollow", "", Some(&bea)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_follows").await, 0);
    assert!(
        s.get("/following", Some(&bea))
            .await
            .body
            .contains("don't follow any makers")
    );
}

// ---------------------------------------------------------------- MF-14: comments

/// A comment `text` (form-encoded) posted on design `id` by the person signed in with `cookie`.
async fn say(s: &Site, id: i64, cookie: &str, text: &str) -> Answer {
    s.post(&format!("/d/{id}/comments"), &format!("body={text}"), Some(cookie))
        .await
}

#[tokio::test]
async fn people_comment_on_designs_and_bad_comments_come_down() {
    let Some(s) = site_with(vec![1003]).await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let rose = published(&s, &ann, "anns", "Rose").await;
    let bea = s.sign_up("bea%40example.com").await;
    let cal = s.sign_up("cal%40example.com").await;
    let ada = s.sign_up("ada%40example.com").await;
    assert!(
        s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains("Sign in to comment")
    );
    // a maker profile first; then plain text, links and tags included
    assert!(say(&s, rose, &bea, "Hello").await.body.contains("maker profile first"));
    s.post("/account/maker", "name=Bea&handle=bea", Some(&bea)).await;
    assert!(say(&s, rose, &bea, "+++").await.body.contains("Write something first"));
    let r = say(
        &s,
        rose,
        &bea,
        "Which+stabilizer%3F+See+%3Ca+href%3D%22https%3A%2F%2Fspam.example%22%3Ehere%3C%2Fa%3E",
    )
    .await;
    assert!(r.location().starts_with(&format!("/d/{rose}/rose#c")), "{}", r.body);
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_comments")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let page = s.get(&format!("/d/{rose}"), None).await.body;
    assert!(
        page.contains("1 comment")
            && page.contains("Which stabilizer? See &lt;a href=&quot;https://spam.example&quot;&gt;here&lt;/a&gt;"),
        "{page}"
    );
    assert!(!page.contains(r#"<a href="https://spam.example""#));
    // the maker answers, marked as the maker
    s.post("/account/maker", "name=Ann&handle=anns", Some(&ann)).await;
    say(&s, rose, &ann, "Tear-away+works+best").await;
    assert!(
        s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains(r#"<span class="badge">the maker</span>"#)
    );
    // Cal can't take Bea's down, but can report it
    s.post(&format!("/comments/{cid}/delete"), "", Some(&cal)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_comments").await, 2);
    assert!(
        s.get(&format!("/d/{rose}"), Some(&cal))
            .await
            .body
            .contains(&format!("/comments/{cid}/report"))
    );
    assert!(
        s.post(&format!("/comments/{cid}/report"), "", Some(&cal))
            .await
            .body
            .contains("Choose what the problem is")
    );
    assert!(
        s.post(&format!("/comments/{cid}/report"), "reason=spam", Some(&cal))
            .await
            .body
            .contains("Your report is in")
    );
    let queue = s.get("/admin", Some(&ada)).await.body;
    let rid: i64 = sqlx::query_scalar("SELECT id FROM mf_reports")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert!(
        queue.contains("Which stabilizer?") && queue.contains(&format!("/admin/reports/{rid}/remove-comment")),
        "{queue}"
    );
    assert_eq!(
        s.post(&format!("/admin/reports/{rid}/remove-comment"), "", Some(&cal))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    s.post(&format!("/admin/reports/{rid}/remove-comment"), "", Some(&ada))
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_comments").await, 1);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_moderation_log WHERE action = 'remove-comment'")
            .await,
        1
    );
    // the design's maker takes down any comment on it; its author their own
    s.post("/account/maker", "name=Cal&handle=cal", Some(&cal)).await;
    say(&s, rose, &cal, "Nice").await;
    let cals: i64 = sqlx::query_scalar("SELECT id FROM mf_comments WHERE account_id = 1002")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/comments/{cals}/delete"), "", Some(&ann)).await;
    let anns: i64 = sqlx::query_scalar("SELECT id FROM mf_comments WHERE account_id = 1000")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/comments/{anns}/delete"), "", Some(&ann)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_comments").await, 0);
    // twenty an hour
    for _ in 0..makerfoundry::limits::COMMENT.max {
        say(&s, rose, &bea, "again").await;
    }
    assert!(say(&s, rose, &bea, "again").await.body.contains("commented a lot"));
}

// ---------------------------------------------------------------- MF-15: your data

#[tokio::test]
async fn a_maker_downloads_their_data_and_deletes_it_and_buyers_keep_their_designs() {
    let Some((s, st, sam, fox)) = selling_site(Vec::new()).await else {
        return;
    };
    // Bea buys the fox, and has a design of her own
    let bea = s.sign_up("bea%40example.com").await;
    s.post(&format!("/d/{fox}/buy"), "", Some(&bea)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_d",
        "checkout.session.completed",
        session_json("cs_test_1", &form, true),
    )
    .await;
    let cat = published(&s, &bea, "bea", "Cat").await;
    // Sam: a free design too, a collection, a follow, a comment and a make; Bea follows Sam
    let owl = published(&s, &sam, "sams-shop", "Owl").await;
    s.post("/collections", "name=Ideas&public=yes", Some(&sam)).await;
    s.post(&format!("/d/{cat}/save"), "", Some(&sam)).await;
    s.post("/makers/bea/follow", "", Some(&sam)).await;
    s.post("/makers/sams-shop/follow", "", Some(&bea)).await;
    say(&s, cat, &sam, "Lovely").await;
    s.send(multipart(
        &format!("/d/{cat}/makes"),
        &[
            ("photo", "a.jpg", photo_with_location()),
            ("confirm", "", b"yes".to_vec()),
        ],
        &sam,
    ))
    .await;
    // the copy
    assert!(
        s.get("/account/data", Some(&sam))
            .await
            .body
            .contains("Download a copy")
    );
    let r = s.get("/account/data.json", Some(&sam)).await;
    assert_eq!(
        r.headers[header::CONTENT_DISPOSITION],
        r#"attachment; filename="makerfoundry-data.json""#
    );
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["account"]["email"], "sam@example.com");
    assert_eq!(v["maker_profile"][0]["handle"], "sams-shop");
    assert_eq!(v["designs"].as_array().unwrap().len(), 2);
    assert_eq!(v["sales"][0]["seller_cents"], 405);
    assert_eq!(v["comments"][0]["body"], "Lovely");
    assert_eq!(v["makes"].as_array().unwrap().len(), 1);
    assert_eq!(v["following"][0]["handle"], "bea");
    assert_eq!(v["collections"].as_array().unwrap().len(), 2);
    assert_eq!(
        s.get("/account/data.json", None).await.location(),
        "/signin?next=/account/data"
    );
    // deleting needs the word
    assert!(
        s.post("/account/data/delete", "confirm=yes", Some(&sam))
            .await
            .body
            .contains("Type delete")
    );
    let stored = s.stored.lock().unwrap().len();
    let r = s.post("/account/data/delete", "confirm=DELETE", Some(&sam)).await;
    assert!(r.body.contains("Your MakerFoundry data is deleted"), "{}", r.body);
    assert!(r.headers[header::SET_COOKIE].to_str().unwrap().contains("Max-Age=0"));
    assert_eq!(s.get("/account", Some(&sam)).await.location(), "/signin");
    // gone: the owl and its file and photos, the make's photos, the rest of Sam's records
    assert_eq!(s.stored.lock().unwrap().len(), stored - 5);
    for (sql, n) in [
        (format!("SELECT count(*) FROM mf_designs WHERE id = {owl}"), 0),
        ("SELECT count(*) FROM mf_makes".to_string(), 0),
        ("SELECT count(*) FROM mf_comments".to_string(), 0),
        (
            "SELECT count(*) FROM mf_collections WHERE account_id = 1000".to_string(),
            0,
        ),
        ("SELECT count(*) FROM mf_follows".to_string(), 0),
        ("SELECT count(*) FROM mf_makers WHERE account_id = 1000".to_string(), 0),
        (
            "SELECT count(*) FROM mf_sessions WHERE account_id = 1000".to_string(),
            0,
        ),
        (
            "SELECT count(*) FROM mf_accounts WHERE account_id = 1000 AND email = ''".to_string(),
            1,
        ),
        // kept: the sale, its ledger, and the fox, unpublished, for its buyer
        ("SELECT count(*) FROM mf_orders WHERE status = 'paid'".to_string(), 1),
        ("SELECT count(*) FROM mf_ledger".to_string(), 3),
        (
            format!("SELECT count(*) FROM mf_designs WHERE id = {fox} AND status = 'draft'"),
            1,
        ),
    ] {
        assert_eq!(s.scalar(&sql).await, n, "{sql}");
    }
    assert!(
        s.get(&format!("/d/{fox}/download"), Some(&bea))
            .await
            .location()
            .contains("/files/")
    );
    assert!(
        s.get("/library", Some(&bea))
            .await
            .body
            .contains(&format!("/d/{fox}/download"))
    );
    // Bea's own things are untouched
    assert!(s.get(&format!("/d/{cat}"), None).await.status == StatusCode::OK);
}

// ---------------------------------------------------------------- everything people type is escaped

#[tokio::test]
async fn hostile_text_in_every_field_never_reaches_a_page_as_html() {
    let Some(s) = site_with(vec![1002]).await else { return };
    const X: &str = "%3Cscript%3Ex()%3C%2Fscript%3E%22%27";
    let eve = s.sign_up("eve%40example.com").await;
    let bob = s.sign_up("bob%40example.com").await;
    let ada = s.sign_up("ada%40example.com").await;
    // a design whose file name, title, description, tags, maker name and bio are all script
    let r = s
        .send(multipart(
            "/designs/new",
            &[
                ("file", "<script>x()</script>.pes", sample(sf_formats::Format::Pes)),
                ("photos", "p.jpg", photo_with_location()),
            ],
            &eve,
        ))
        .await;
    let id = design_id(r.location());
    s.post("/account/maker", &format!("name={X}&handle=eve&bio={X}"), Some(&eve))
        .await;
    s.post(
        &format!("/designs/{id}/edit"),
        &format!("title={X}&description={X}&tags={X}&licence=personal&price=0"),
        Some(&eve),
    )
    .await;
    s.post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&eve))
        .await;
    let r = s
        .send(multipart(
            &format!("/designs/{id}/files"),
            &[("file", "<script>.dst", sample(sf_formats::Format::Dst))],
            &eve,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // a collection, a comment, a make and reports, all script
    s.post("/collections", &format!("name={X}&public=yes"), Some(&eve))
        .await;
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_collections")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/d/{id}/save"), &format!("collection={cid}"), Some(&eve))
        .await;
    s.post("/account/maker", &format!("name={X}&handle=bob"), Some(&bob))
        .await;
    say(&s, id, &bob, X).await;
    s.send(multipart(
        &format!("/d/{id}/makes"),
        &[
            ("photo", "a.jpg", photo_with_location()),
            ("note", "", b"<script>x()</script>".to_vec()),
            ("confirm", "", b"yes".to_vec()),
        ],
        &bob,
    ))
    .await;
    let mid: i64 = sqlx::query_scalar("SELECT id FROM mf_makes")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let comment: i64 = sqlx::query_scalar("SELECT id FROM mf_comments")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(
        &format!("/d/{id}/report"),
        &format!("reason=other&details={X}&email={X}"),
        None,
    )
    .await;
    s.post(
        &format!("/makes/{mid}/report"),
        &format!("reason=other&details={X}"),
        None,
    )
    .await;
    s.post(
        &format!("/comments/{comment}/report"),
        &format!("reason=other&details={X}"),
        None,
    )
    .await;
    s.post(
        "/dmca/notice",
        &format!("name={X}&email=a%40b.c&address={X}&phone={X}&original={X}&designs=%2Fd%2F{id}&good_faith=yes&accurate=yes&signature={X}"),
        None,
    )
    .await;
    // a report of the design, the make and the comment, and a copyright notice
    assert_eq!(s.scalar("SELECT count(*) FROM mf_reports").await, 4);
    // forms sent back with a problem show what was typed, escaped
    let again = s
        .post(
            &format!("/designs/{id}/edit"),
            &format!("title={X}&licence=nope"),
            Some(&eve),
        )
        .await;
    let signup = s.post("/signup", &format!("email={X}&password=a&again=b"), None).await;
    let mut pages = vec![again.body, signup.body];
    for path in [
        "/".to_string(),
        "/browse".into(),
        format!("/browse?q={X}&tag={X}"),
        format!("/d/{id}"),
        format!("/designs/{id}"),
        format!("/designs/{id}/edit"),
        "/designs".into(),
        "/makers/eve".into(),
        format!("/collections/{cid}"),
        "/collections".into(),
        format!("/makes/{mid}/report"),
        format!("/comments/{comment}/report"),
        "/admin".into(),
        "/account".into(),
        "/following".into(),
        "/library".into(),
        "/account/sales".into(),
    ] {
        for who in [None, Some(&eve), Some(&bob), Some(&ada)] {
            pages.push(s.get(&path, who.map(String::as_str)).await.body);
        }
    }
    let raw = pages.iter().filter(|p| p.contains("<script")).count();
    assert_eq!(raw, 0, "{}", pages.iter().find(|p| p.contains("<script")).unwrap());
    // and it was all there, escaped
    assert!(
        pages
            .iter()
            .any(|p| p.contains("&lt;script&gt;x()&lt;/script&gt;&quot;&#39;"))
    );
}

// ---------------------------------------------------------------- MF-16: the admins' overview

#[tokio::test]
async fn admins_see_the_sites_numbers_and_nobody_else_does() {
    let Some((s, st, _sam, fox)) = selling_site(vec![1002]).await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    let ada = s.sign_up("ada%40example.com").await;
    s.post(&format!("/d/{fox}/buy"), "", Some(&bea)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_o",
        "checkout.session.completed",
        session_json("cs_test_1", &form, true),
    )
    .await;
    s.post(&format!("/d/{fox}/report"), "reason=spam", None).await;
    assert_eq!(s.get("/admin/overview", Some(&bea)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get("/admin/overview", None).await.status, StatusCode::NOT_FOUND);
    let page = s.get("/admin/overview", Some(&ada)).await.body;
    for fact in [
        "<dt>Accounts</dt><dd>3</dd>",
        "<dt>Published</dt><dd>1</dd>",
        "<dt>Paid</dt><dd>1</dd>",
        "<dt>Sales</dt><dd>1</dd>",
        "<dt>Paid by buyers</dt><dd>$5</dd>",
        "<dt>To sellers</dt><dd>$4.05</dd>",
        "<dt>MakerFoundry&#39;s fees</dt><dd>$0.50</dd>",
        "<dt>Card fees</dt><dd>$0.45</dd>",
        "<dt>Fees this month</dt><dd>$0.50</dd>",
        "<dt>Reports</dt><dd>1</dd>",
    ] {
        assert!(page.contains(fact), "{fact} in {page}");
    }
}

/// Open in StitchFoundry: a free, published embroidery design's page links to StitchFoundry's web
/// app with its file's address, and the web app gets the file itself, with CORS; nothing that isn't
/// free and public is sent, whoever asks.
#[tokio::test]
async fn a_free_design_opens_in_stitchfoundrys_web_app() {
    let Some(s) = site().await else { return };
    let cookie = s.sign_up("ann%40example.com").await;
    let id = published(&s, &cookie, "anns-shop", "Rose").await;
    let page = s.get(&format!("/d/{id}/rose"), None).await;
    let file = format!("https%3A%2F%2Fmakerfoundry.test%2Fapi%2Fv1%2Fdesigns%2F{id}%2Ffile%2Frose.pes");
    assert!(
        page.body.contains(&format!(
            r#"href="https://stitchfoundry.github.io/StitchFoundry/?open={file}""#
        )),
        "{}",
        page.body
    );
    assert!(page.body.contains("Open in StitchFoundry"));
    // the web app's fetch: the file, from the web app's site
    let path = format!("/api/v1/designs/{id}/file/rose.pes");
    let r = s
        .api("GET", &path, None, Some("https://stitchfoundry.test"), &json!({}))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.starts_with("#PES"), "a PES file");
    assert_eq!(
        r.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://stitchfoundry.test"
    );
    assert_eq!(
        r.headers[header::CONTENT_DISPOSITION],
        r#"attachment; filename="rose.pes""#
    );
    assert_eq!(r.headers[header::ACCESS_CONTROL_EXPOSE_HEADERS], "content-disposition");
    assert!(r.headers.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());
    assert_eq!(
        s.scalar(&format!("SELECT downloads::bigint FROM mf_designs WHERE id = {id}"))
            .await,
        1
    );
    // another site's page gets the file but may not read it
    let evil = s
        .api("GET", &path, None, Some("https://evil.example"), &json!({}))
        .await;
    assert!(evil.headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    // a draft isn't sent, even to its maker (cookies aren't read), nor a design that isn't there
    let draft = add_design(&s, &cookie, sf_formats::Format::Pes, "tulip.pes").await;
    let mut req = Request::get(format!("/api/v1/designs/{draft}/file/tulip.pes")).header(header::HOST, HOST);
    req = req.header(header::COOKIE, &cookie);
    let r = s.send(req.body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
    assert!(r.body.contains("isn't on MakerFoundry"), "{}", r.body);
    assert_eq!(
        s.api("GET", "/api/v1/designs/999999/file/x.pes", None, None, &json!({}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // a draft's page has no button
    assert!(
        !s.get(&format!("/designs/{draft}"), Some(&cookie))
            .await
            .body
            .contains("Open in StitchFoundry")
    );
}

/// A design for sale isn't opened in StitchFoundry: it's bought on MakerFoundry first.
#[tokio::test]
async fn a_design_for_sale_isnt_sent_to_stitchfoundry() {
    let Some((s, _st, seller, id)) = selling_site(vec![]).await else {
        return;
    };
    let r = s
        .api(
            "GET",
            &format!("/api/v1/designs/{id}/file/fox-patch.pes"),
            None,
            Some("https://stitchfoundry.test"),
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(r.body.contains("for sale"), "{}", r.body);
    assert_eq!(
        r.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://stitchfoundry.test"
    );
    // its maker opens it through a link made for them (see the buyers' test)
    let page = s.get(&format!("/d/{id}/fox-patch"), Some(&seller)).await;
    assert!(
        page.body.contains(&format!(r#"href="/d/{id}/open?app=stitchfoundry""#)),
        "{}",
        page.body
    );
    assert!(
        !s.get(&format!("/d/{id}/fox-patch"), None)
            .await
            .body
            .contains("Open in StitchFoundry")
    );
}

/// Open in StitchFoundry for a paid design: only its buyers (and maker) get the button, which
/// makes a ten-minute link to that design's file; the web app's fetch with it gets the file.
#[tokio::test]
async fn a_paid_design_opens_in_stitchfoundry_for_its_buyers() {
    let Some((s, st, _seller, id)) = selling_site(Vec::new()).await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    let cal = s.sign_up("cal%40example.com").await;
    // not bought: no button, and the open route sends them back to the page
    assert!(
        !s.get(&format!("/d/{id}"), Some(&bea))
            .await
            .body
            .contains("Open in StitchFoundry")
    );
    assert_eq!(
        s.get(&format!("/d/{id}/open"), Some(&bea)).await.location(),
        format!("/d/{id}")
    );
    assert_eq!(
        s.get(&format!("/d/{id}/open"), None).await.location(),
        format!("/signin?next=/d/{id}")
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_open_links").await, 0);
    // bought
    s.post(&format!("/d/{id}/buy"), "", Some(&bea)).await;
    let form = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_op",
        "checkout.session.completed",
        session_json("cs_test_1", &form, true),
    )
    .await;
    assert!(
        s.get(&format!("/d/{id}"), Some(&bea))
            .await
            .body
            .contains(&format!(r#"href="/d/{id}/open?app=stitchfoundry""#))
    );
    let r = s.get(&format!("/d/{id}/open"), Some(&bea)).await;
    let prefix = format!(
        "https://stitchfoundry.github.io/StitchFoundry/?open=https%3A%2F%2Fmakerfoundry.test%2Fapi%2Fv1%2Fdesigns%2F{id}%2Ffile%2Ffox-patch.pes%3Ft%3D"
    );
    assert!(r.location().starts_with(&prefix), "{}", r.location());
    let token = r.location().trim_start_matches(&prefix).to_string();
    assert_eq!(token.len(), 48);
    // the web app's fetch with the link: the file
    let path = format!("/api/v1/designs/{id}/file/fox-patch.pes");
    let ok = s
        .api(
            "GET",
            &format!("{path}?t={token}"),
            None,
            Some("https://stitchfoundry.test"),
            &json!({}),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert!(ok.body.starts_with("#PES"));
    // without it, with a wrong one, or for another design: refused
    for bad in [String::new(), "0".repeat(48), "nothex".into()] {
        let r = s.api("GET", &format!("{path}?t={bad}"), None, None, &json!({})).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{bad}: {}", r.body);
    }
    let other = published(&s, &cal, "cals", "Owl").await;
    let r = s
        .api(
            "GET",
            &format!("/api/v1/designs/{other}/file/owl.pes?t={token}"),
            None,
            None,
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "a free design is anyone's anyway");
    sqlx::query("UPDATE mf_designs SET price_cents = 300 WHERE id = $1")
        .bind(other)
        .execute(&s.pool)
        .await
        .unwrap();
    let r = s
        .api(
            "GET",
            &format!("/api/v1/designs/{other}/file/owl.pes?t={token}"),
            None,
            None,
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "the link is only for its design");
    // ten minutes on, it's gone
    sqlx::query("UPDATE mf_open_links SET expires_at = now() - interval '1 second'")
        .execute(&s.pool)
        .await
        .unwrap();
    let r = s.api("GET", &format!("{path}?t={token}"), None, None, &json!({})).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(makerfoundry::api::forget_old_links(&s.pool).await.unwrap(), 1);
}

/// A one-page PDF with a red square to cut.
fn tiny_pdf() -> Vec<u8> {
    use lopdf::{Document, Object, Stream, dictionary};
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"1 0 0 RG 72 72 72 72 re S".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1 }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

/// MakerFoundry's side of Open in LaserFoundry: a laser design's SVG or DXF file is served to the
/// web apps like an embroidery file; no button shows until LaserFoundry's web app opens links.
#[tokio::test]
async fn laser_files_are_ready_for_laserfoundrys_web_app() {
    let Some(s) = site().await else { return };
    let cookie = s.sign_up("lou%40example.com").await;
    s.post("/account/maker", "name=Lou&handle=lou", Some(&cookie)).await;
    let mut ids = Vec::new();
    for (name, bytes) in [
        ("box.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" width="50mm" height="40mm" viewBox="0 0 50 40"><rect x="1" y="1" width="48" height="38" fill="none" stroke="red"/></svg>"#.to_vec()),
        ("lid.pdf", tiny_pdf()),
    ] {
        let r = s.send(multipart("/designs/new", &[("file", name, bytes)], &cookie)).await;
        let id = design_id(r.location());
        s.post(&format!("/designs/{id}/edit"), "title=Box&licence=personal&price=0", Some(&cookie)).await;
        s.post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie)).await;
        ids.push(id);
    }
    let svg = ids[0];
    let r = s
        .api(
            "GET",
            &format!("/api/v1/designs/{svg}/file/box.svg"),
            None,
            Some("https://stitchfoundry.test"),
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("<svg"));
    assert_eq!(
        r.headers[header::CONTENT_DISPOSITION],
        r#"attachment; filename="box.svg""#
    );
    // the button follows the switch: off until LaserFoundry's web app opens links
    let page = s.get(&format!("/d/{svg}"), None).await.body;
    if makerfoundry::api::LASERFOUNDRY_OPENS_LINKS {
        assert!(
            page.contains("https://stitchfoundry.github.io/laserfoundry/?open="),
            "{page}"
        );
    } else {
        assert!(!page.contains("Open in"), "{page}");
    }
    let pdf = ids[1];
    let r = s
        .api(
            "GET",
            &format!("/api/v1/designs/{pdf}/file/box.pdf"),
            None,
            None,
            &json!({}),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "LaserFoundry doesn't import PDF: {}",
        r.body
    );
}

/// MakerFoundry's side of ScreenFoundry (MF-87): an SVG it publishes lands as a laser draft with a
/// title, and a published SVG's file is served for its web app under a plain name, whatever the
/// title; no button shows until ScreenFoundry's web app opens links.
#[tokio::test]
async fn svg_designs_are_ready_for_screenfoundrys_web_app() {
    use base64::Engine;
    // (Sol, the first account, 1000, is an admin here: for the list of designs from apps)
    let Some(s) = site_with(vec![1000]).await else { return };
    let b64 = base64::engine::general_purpose::STANDARD;
    let cookie = s.sign_up("sol%40example.com").await;
    s.post("/account/maker", "name=Sol&handle=sol", Some(&cookie)).await;
    s.fake.lock().unwrap().sessions.insert("sfs_screen".into(), 1000);
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="50mm" height="40mm" viewBox="0 0 50 40"><circle cx="25" cy="20" r="15" fill="black"/></svg>"#;
    let r = s
        .api(
            "POST",
            "/api/v1/designs",
            Some("sfs_screen"),
            None,
            &json!({"file_name": "Tiger_shirt-front.svg", "file": b64.encode(svg), "app": "ScreenFoundry 0.1.0"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    let id = v["id"].as_i64().unwrap();
    assert_eq!((v["kind"].as_str(), v["format"].as_str()), (Some("laser"), Some("SVG")));
    let (title, status): (String, String) = sqlx::query_as("SELECT title, status FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!((title.as_str(), status.as_str()), ("Tiger shirt front", "draft"));
    // it keeps the app's name, without the version (MF-88), for the admins' list
    let app: Option<String> = sqlx::query_scalar("SELECT from_app FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(app.as_deref(), Some("ScreenFoundry"));
    let list = s.get("/admin/apps", Some(&cookie)).await.body;
    assert!(
        list.contains(r#"<a href="/admin/apps?app=ScreenFoundry">ScreenFoundry</a>"#)
            && list.contains("1 sent, 0 published"),
        "{list}"
    );
    let one = s.get("/admin/apps?app=ScreenFoundry", Some(&cookie)).await.body;
    assert!(
        one.contains("Latest from ScreenFoundry") && one.contains("Tiger shirt front"),
        "{one}"
    );
    assert!(
        !s.get("/admin/apps?app=StitchFoundry", Some(&cookie))
            .await
            .body
            .contains("Tiger shirt front")
    );
    s.dump_page("admin-apps", &one, Some(&cookie)).await;
    // nobody else sees the list
    let other = s.sign_up("oz%40example.com").await;
    assert_eq!(s.get("/admin/apps", Some(&other)).await.status, StatusCode::NOT_FOUND);
    assert!(
        s.get("/account/data.json", Some(&cookie))
            .await
            .body
            .contains(r#""from_app": "ScreenFoundry""#)
    );

    // published with a title that leaves no slug: the file goes by design-<id>.svg
    s.post(
        &format!("/designs/{id}/edit"),
        "title=%E6%97%A5%E6%9C%AC%E3%81%AE%E7%AE%B1&licence=personal&price=0",
        Some(&cookie),
    )
    .await;
    s.post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie))
        .await;
    let r = s
        .api(
            "GET",
            &format!("/api/v1/designs/{id}/file/design-{id}.svg"),
            None,
            Some("https://stitchfoundry.test"),
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("<circle"));
    assert_eq!(
        r.headers[header::CONTENT_DISPOSITION],
        format!(r#"attachment; filename="design-{id}.svg""#).as_str()
    );
    assert_eq!(r.headers[header::ACCESS_CONTROL_EXPOSE_HEADERS], "content-disposition");
    assert_eq!(
        r.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://stitchfoundry.test"
    );
    // never shown as a page from our site: a download, not an image
    assert_eq!(r.headers[header::CONTENT_TYPE], "application/octet-stream");
    assert!(r.headers.get(header::LOCATION).is_none());
    // a missing design: MakerFoundry's own sentence, as JSON
    let r = s
        .api(
            "GET",
            "/api/v1/designs/999999/file/x.svg",
            None,
            Some("https://stitchfoundry.test"),
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["error"]["message"], "That design isn't on MakerFoundry.");
    // the button follows the switch
    let page = s.get(&format!("/d/{id}"), None).await.body;
    // Open in PatternFoundry (MF-89): the same file address, PatternFoundry's web app, SVG only
    let pattern = format!(
        "https://stitchfoundry.github.io/patternfoundry/?open=https%3A%2F%2Fmakerfoundry.test%2Fapi%2Fv1%2Fdesigns%2F{id}%2Ffile%2Fdesign-{id}.svg"
    );
    if makerfoundry::api::PATTERNFOUNDRY_OPENS_LINKS {
        assert!(
            page.contains(&pattern) && page.contains("Open in PatternFoundry"),
            "{page}"
        );
        s.dump_page("svg-open-buttons", &page, None).await;
    } else {
        assert!(!page.contains("Open in PatternFoundry"), "{page}");
    }
    if makerfoundry::api::SCREENFOUNDRY_OPENS_LINKS {
        let want = format!(
            "https://app.screenfoundry.app/?open=https%3A%2F%2Fmakerfoundry.test%2Fapi%2Fv1%2Fdesigns%2F{id}%2Ffile%2Fdesign-{id}.svg"
        );
        assert!(page.contains(&want), "{page}");
        s.dump_page("svg-open-buttons", &page, None).await;
    } else {
        assert!(!page.contains("Open in ScreenFoundry"), "{page}");
    }
}

// ---------------------------------------------------------------- link previews

#[tokio::test]
async fn a_shared_link_to_a_design_shows_its_title_words_and_photo() {
    let Some(s) = site().await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let id = published(&s, &ann, "anns", "Rose+%26+%22Fern%22").await;
    s.post(
        &format!("/designs/{id}/edit"),
        "title=Rose+%26+%22Fern%22&licence=personal&price=0&description=A+bright+rose+for+a+4x4+hoop.%0A%0AStitch+on+cotton.",
        Some(&ann),
    )
    .await;
    let photo: i64 = sqlx::query_scalar("SELECT id FROM mf_design_photos")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let page = s.get(&format!("/d/{id}"), None).await.body;
    for tag in [
        r#"<meta property="og:title" content="Rose &amp; &quot;Fern&quot;">"#.to_string(),
        r#"<meta property="og:description" content="A bright rose for a 4x4 hoop. Stitch on cotton.">"#.to_string(),
        format!(r#"<meta property="og:url" content="https://makerfoundry.test/d/{id}/rose-fern">"#),
        format!(r#"<link rel="canonical" href="https://makerfoundry.test/d/{id}/rose-fern">"#),
        format!(r#"<meta property="og:image" content="https://makerfoundry.test/designs/{id}/photos/{photo}/large">"#),
        r#"<meta name="twitter:card" content="summary_large_image">"#.to_string(),
    ] {
        assert!(page.contains(&tag), "{tag} in {page}");
    }
    // and the picture it names is there for anyone
    let img = s.get(&format!("/designs/{id}/photos/{photo}/large"), None).await;
    assert_eq!(img.status, StatusCode::OK);
    // a maker's page too; a draft shows its maker no preview
    let maker = s.get("/makers/anns", None).await.body;
    assert!(
        maker.contains(r#"<meta property="og:url" content="https://makerfoundry.test/makers/anns">"#),
        "{maker}"
    );
    s.post(&format!("/designs/{id}/unpublish"), "", Some(&ann)).await;
    assert!(!s.get(&format!("/d/{id}"), Some(&ann)).await.body.contains("og:title"));
    // other pages keep the site's own description
    assert!(s.get("/browse", None).await.body.contains("shared and sold by makers"));
}

// ---------------------------------------------------------------- MF-22: activity

#[tokio::test]
async fn a_maker_sees_what_people_did_with_their_designs() {
    let Some(s) = site().await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let rose = published(&s, &ann, "anns", "Rose").await;
    let bea = s.sign_up("bea%40example.com").await;
    s.post("/account/maker", "name=Bea+%3Cb%3EB%3C%2Fb%3E&handle=bea", Some(&bea))
        .await;
    assert!(s.get("/activity", None).await.location().starts_with("/signin"));
    let empty = s.get("/activity", Some(&ann)).await.body;
    assert!(empty.contains("Nothing yet"), "{empty}");
    assert!(!s.get("/account", Some(&ann)).await.body.contains(" new</strong>"));

    // a comment, a make, a follow and a sale, and ann's own comment, which isn't news to her
    say(&s, rose, &bea, "Lovely+%3Cscript%3Ex%3C%2Fscript%3E").await;
    say(&s, rose, &ann, "Thanks").await;
    sqlx::query(
        "INSERT INTO mf_makes (design_id, account_id, photo_key, thumb_key, width, height, note)
         VALUES ($1, 1001, 'k', 't', 10, 10, 'On a tote bag')",
    )
    .bind(rose)
    .execute(&s.pool)
    .await
    .unwrap();
    s.post("/makers/anns/follow", "", Some(&bea)).await;
    sqlx::query(
        "INSERT INTO mf_orders (buyer_account, design_id, seller_account, price_cents, fee_cents,
             processing_cents, seller_cents, title, status, paid_at)
         VALUES (1001, $1, 1000, 500, 50, 45, 405, 'Rose', 'paid', now())",
    )
    .bind(rose)
    .execute(&s.pool)
    .await
    .unwrap();

    let account = s.get("/account", Some(&ann)).await.body;
    assert!(account.contains("4 new"), "{account}");
    let page = s.get("/activity", Some(&ann)).await.body;
    for want in [
        "commented on",
        "Lovely &lt;script&gt;",
        "posted a make of",
        "On a tote bag",
        "Someone started following you",
        "sold: $4.05 to you",
        r#"<a href="/makers/bea">Bea &lt;b&gt;B&lt;/b&gt;</a>"#,
        &format!(r#"<a href="/d/{rose}">Rose</a>"#),
    ] {
        assert!(page.contains(want), "{want} missing: {page}");
    }
    assert!(!page.contains("Thanks") && !page.contains("<script>x"), "{page}");
    assert_eq!(page.matches(r#"<strong class="new">new</strong>"#).count(), 4);
    // looked at: no longer new, still listed
    assert!(!s.get("/account", Some(&ann)).await.body.contains(" new</strong>"));
    let again = s.get("/activity", Some(&ann)).await.body;
    assert!(again.contains("commented on") && !again.contains(r#"class="new">new"#));
    // a refund is news too; bea's activity is her own (nobody did anything with her designs)
    sqlx::query("UPDATE mf_orders SET status = 'refunded', refunded_at = now() + interval '1 second'")
        .execute(&s.pool)
        .await
        .unwrap();
    // as a refund leaves the ledger: the price back from the seller, then the 10% given back
    sqlx::query(
        "INSERT INTO mf_ledger (order_id, account_id, kind, cents, reason)
         SELECT id, 1000, 'seller', -500, 'refund' FROM mf_orders
         UNION ALL SELECT id, 1000, 'seller', 50, 'fee_return' FROM mf_orders",
    )
    .execute(&s.pool)
    .await
    .unwrap();
    assert!(s.get("/account", Some(&ann)).await.body.contains("1 new"));
    // what they got ($4.05) and the card fee Stripe keeps ($0.45)
    assert!(
        s.get("/activity", Some(&ann))
            .await
            .body
            .contains("was refunded: $4.50 less for you")
    );
    assert!(s.get("/activity", Some(&bea)).await.body.contains("Nothing yet"));
    // only when someone looked is kept, and only once there was something to see
    assert_eq!(s.scalar("SELECT count(*) FROM mf_activity_seen").await, 1);
}

// ---------------------------------------------------------------- MF-25: approval links

/// A multipart form with text fields and files (`(field, file name, bytes)`; a text field has no
/// file name), posted from our own page with `cookie`.
fn form_parts(path: &str, text: &[(&str, &str)], files: &[(&str, &str, Vec<u8>)], cookie: &str) -> Request<Body> {
    let boundary = "----mf-test-proof";
    let mut body = Vec::new();
    for (field, value) in text {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    for (field, name, bytes) in files {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::post(path)
        .header(header::HOST, HOST)
        .header(header::ORIGIN, format!("https://{HOST}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::COOKIE, cookie)
        .body(Body::from(body))
        .unwrap()
}

/// Sends a proof of a PES with a mock-up photo as the shop signed in with `cookie` → its id.
async fn send_proof(s: &Site, cookie: &str, title: &str) -> i64 {
    let r = s
        .send(form_parts(
            "/proofs/new",
            &[
                ("title", title),
                ("shop_name", "Rosa's Stitch Co."),
                ("customer", "Café Luna"),
                ("message", "Here's your logo for the aprons."),
            ],
            &[
                ("file", "logo.pes", sample(sf_formats::Format::Pes)),
                ("photos", "apron.jpg", photo_with_location()),
            ],
            cookie,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    r.location().trim_start_matches("/proofs/").parse().unwrap()
}

/// The customer's link as the shop's page shows it, as a path (`/p/<id>/<token>`).
async fn proof_link(s: &Site, cookie: &str, id: i64) -> String {
    let page = s.get(&format!("/proofs/{id}"), Some(cookie)).await.body;
    let start = page
        .find("value=\"https://makerfoundry.test/p/")
        .unwrap_or_else(|| panic!("no link on {page}"));
    let rest = &page[start + "value=\"https://makerfoundry.test".len()..];
    rest[..rest.find('"').unwrap()].to_string()
}

#[tokio::test]
async fn a_customer_asks_for_changes_then_approves_version_2() {
    let Some(s) = site_with_proofs(Vec::new(), true).await else {
        return;
    };
    let shop = s.sign_up("rosa%40example.com").await;
    assert!(s.get("/account", Some(&shop)).await.body.contains(r#"href="/proofs""#));
    // a proof: the design read and drawn, the photo cleaned, a private link for the customer
    let id = send_proof(&s, &shop, "Café Luna logo").await;
    let mine = s.get(&format!("/proofs/{id}"), Some(&shop)).await;
    assert!(
        mine.body.contains("Café Luna logo") && mine.body.contains("Sent"),
        "{}",
        mine.body
    );
    assert!(mine.body.contains("<svg"), "the stitches are drawn");
    let link = proof_link(&s, &shop, id).await;
    assert!(link.starts_with(&format!("/p/{id}/")) && link.len() == format!("/p/{id}/").len() + 64);
    // the customer, with no account
    let seen = s.get(&link, None).await;
    assert_eq!(seen.status, StatusCode::OK);
    for want in [
        "Rosa&#39;s Stitch Co.",
        "Café Luna logo",
        "Here&#39;s your logo",
        "<svg",
        "Approve version 1",
        "Mock-ups",
    ] {
        assert!(seen.body.contains(want), "{want} missing: {}", seen.body);
    }
    assert!(
        !seen.body.contains("Café Luna</"),
        "who it's for is the shop's own note"
    );
    assert_eq!(seen.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(seen.headers[header::REFERRER_POLICY], "no-referrer");
    assert_eq!(seen.headers["x-robots-tag"], "noindex, nofollow");
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_proofs WHERE id = {id} AND status = 'viewed'"
        ))
        .await,
        1
    );
    // its photo, through the link only
    let photo: i64 = s.scalar("SELECT id FROM mf_proof_photos").await;
    let thumb = s.get(&format!("{link}/photos/{photo}/thumb"), None).await;
    assert_eq!(thumb.status, StatusCode::OK);
    assert_eq!(thumb.headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(
        s.get(&format!("/p/{id}/{}/photos/{photo}/thumb", "0".repeat(64)), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // asking for changes needs words, and a name
    let r = s.post(&link, "version=1&decision=changes&name=Ana&body=", None).await;
    assert!(r.body.contains("Write what you&#39;d like changed"), "{}", r.body);
    let r = s
        .post(&link, "version=1&decision=changes&name=+&body=Bigger", None)
        .await;
    assert!(r.body.contains("Give your name"), "{}", r.body);
    let r = s
        .post(
            &link,
            "version=1&decision=changes&name=Ana+Ruiz&body=Make+the+moon+%3Cbigger%3E",
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let mine = s.get(&format!("/proofs/{id}"), Some(&shop)).await.body;
    assert!(
        mine.contains("Changes asked for") && mine.contains("Ana Ruiz"),
        "{mine}"
    );
    assert!(mine.contains("Make the moon &lt;bigger&gt;"), "plain text: {mine}");
    // the shop answers, and sends version 2
    let r = s.post(&format!("/proofs/{id}/reply"), "body=On+it", Some(&shop)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let r = s
        .send(form_parts(
            &format!("/proofs/{id}/versions"),
            &[("note", "The moon is 20% bigger.")],
            &[("file", "logo2.pes", sample(sf_formats::Format::Pes))],
            &shop,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_proofs WHERE id = {id} AND status = 'sent'"
        ))
        .await,
        1
    );
    // an answer to the old version is refused; the link shows the new one
    let r = s.post(&link, "version=1&decision=approved&name=Ana+Ruiz", None).await;
    assert!(r.body.contains("newer version"), "{}", r.body);
    let seen = s.get(&link, None).await.body;
    assert!(
        seen.contains("Approve version 2") && seen.contains("The moon is 20% bigger.") && seen.contains("On it"),
        "{seen}"
    );
    let old = s.get(&format!("{link}?v=1"), None).await.body;
    assert!(
        old.contains("This is version 1") && !old.contains("Approve version"),
        "{old}"
    );
    // approved: who, which version and when, on the shop's page
    let r = s
        .post(&link, "version=2&decision=approved&name=Ana+Ruiz&body=", None)
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let seen = s.get(&link, None).await.body;
    assert!(
        seen.contains("The shop has your approval") && !seen.contains("Approve version"),
        "{seen}"
    );
    assert!(seen.contains("Ask for changes"), "they may still change their mind");
    let mine = s.get(&format!("/proofs/{id}"), Some(&shop)).await.body;
    assert!(mine.contains("approved version 2") && mine.contains("UTC"), "{mine}");
    assert_eq!(
        s.scalar(
            "SELECT count(*) FROM mf_proof_replies WHERE decision = 'approved' AND version = 2 AND name = 'Ana Ruiz'"
        )
        .await,
        1
    );
    let list = s.get("/proofs", Some(&shop)).await.body;
    assert!(
        list.contains("Approved") && list.contains("version 2") && list.contains("for Café Luna"),
        "{list}"
    );
    // the shop opening its own link doesn't count as the customer viewing
    let id2 = send_proof(&s, &shop, "Napkins").await;
    let link2 = proof_link(&s, &shop, id2).await;
    s.get(&link2, Some(&shop)).await;
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_proofs WHERE id = {id2} AND status = 'sent'"
        ))
        .await,
        1
    );
    // in the shop's data download, and gone with it
    let data = s.get("/account/data.json", Some(&shop)).await.body;
    assert!(data.contains("\"proofs\"") && data.contains("Ana Ruiz"), "{data}");
    let files_before = s.stored.lock().unwrap().keys().filter(|k| k.contains("proof")).count();
    assert!(
        files_before >= 4,
        "two files and a photo with its thumbnail: {files_before}"
    );
    s.post("/account/data/delete", "confirm=delete", Some(&shop)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_proofs").await, 0);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_proof_replies").await, 0);
    assert_eq!(
        s.stored.lock().unwrap().keys().filter(|k| k.contains("proof")).count(),
        0
    );
    assert_eq!(s.get(&link, None).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_proof_link_can_be_changed_and_turned_off_and_is_the_shops_alone() {
    let Some(s) = site_with_proofs(Vec::new(), true).await else {
        return;
    };
    let shop = s.sign_up("rosa%40example.com").await;
    let other = s.sign_up("bea%40example.com").await;
    let id = send_proof(&s, &shop, "Hats").await;
    let link = proof_link(&s, &shop, id).await;
    let token = link.rsplit('/').next().unwrap().to_string();
    // a token doesn't open another proof, a made-up one opens nothing
    let id2 = send_proof(&s, &shop, "Bags").await;
    assert_eq!(
        s.get(&format!("/p/{id2}/{token}"), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.get(&format!("/p/{id}/{}", "ab".repeat(32)), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.get(&format!("/p/{id}/xyz"), None).await.status, StatusCode::NOT_FOUND);
    // another account sees nothing of it, and can't change it
    assert_eq!(
        s.get(&format!("/proofs/{id}"), Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.post(&format!("/proofs/{id}/link"), "action=off", Some(&other))
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(s.get(&link, None).await.status, StatusCode::OK);
    assert_eq!(
        s.post(&format!("/proofs/{id}/delete"), "", Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    // a new link ends the old one
    s.post(&format!("/proofs/{id}/link"), "action=new", Some(&shop)).await;
    let new_link = proof_link(&s, &shop, id).await;
    assert_ne!(new_link, link);
    assert_eq!(s.get(&link, None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get(&new_link, None).await.status, StatusCode::OK);
    // off, then on again
    s.post(&format!("/proofs/{id}/link"), "action=off", Some(&shop)).await;
    assert_eq!(s.get(&new_link, None).await.status, StatusCode::NOT_FOUND);
    assert!(
        s.get(&format!("/proofs/{id}"), Some(&shop))
            .await
            .body
            .contains("link is off")
    );
    s.post(&format!("/proofs/{id}/link"), "action=on", Some(&shop)).await;
    assert_eq!(s.get(&new_link, None).await.status, StatusCode::OK);
    // a form from another site is refused
    let r = s
        .send(
            Request::post(&new_link)
                .header(header::HOST, HOST)
                .header(header::ORIGIN, "https://evil.example")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("version=1&decision=approved&name=Eve"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // deleted: link, rows and files gone
    s.post(&format!("/proofs/{id}/delete"), "", Some(&shop)).await;
    assert_eq!(s.get(&new_link, None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_proof_versions WHERE proof_id = {id}"))
            .await,
        0
    );
    // a bad file is refused and nothing is kept
    let before = s.stored.lock().unwrap().len();
    let r = s
        .send(form_parts(
            "/proofs/new",
            &[("title", "Junk"), ("shop_name", "Rosa")],
            &[("file", "junk.pes", b"not a design".to_vec())],
            &shop,
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body.contains("junk.pes") && r.body.contains("value=\"Junk\""),
        "{}",
        r.body
    );
    assert_eq!(s.stored.lock().unwrap().len(), before);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_proofs").await, 1);
}

#[tokio::test]
async fn proofs_are_for_admins_until_switched_on() {
    let Some(s) = site_with_proofs(vec![1000], false).await else {
        return;
    };
    let admin = s.sign_up("ada%40example.com").await;
    let shop = s.sign_up("rosa%40example.com").await;
    assert_eq!(s.get("/proofs", Some(&shop)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get("/proofs/new", Some(&shop)).await.status, StatusCode::NOT_FOUND);
    assert!(!s.get("/account", Some(&shop)).await.body.contains("/proofs"));
    assert_eq!(s.get("/proofs", None).await.location(), "/signin?next=/proofs");
    // an admin tries it out; the link opens only for admins until then
    let id = send_proof(&s, &admin, "Trial").await;
    let link = proof_link(&s, &admin, id).await;
    assert_eq!(s.get(&link, None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get(&link, Some(&shop)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get(&link, Some(&admin)).await.status, StatusCode::OK);
    // without the key, no proofs for anyone
    let Some(plain) = site_with(vec![1000]).await else {
        return;
    };
    let admin = plain.sign_up("ada%40example.com").await;
    assert_eq!(plain.get("/proofs", Some(&admin)).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_proof_is_deleted_a_year_after_its_last_activity() {
    let Some(s) = site_with_proofs(Vec::new(), true).await else {
        return;
    };
    let shop = s.sign_up("rosa%40example.com").await;
    let old = send_proof(&s, &shop, "Old").await;
    let recent = send_proof(&s, &shop, "Recent").await;
    let old_link = proof_link(&s, &shop, old).await;
    sqlx::query("UPDATE mf_proofs SET active_at = now() - interval '366 days' WHERE id = $1")
        .bind(old)
        .execute(&s.pool)
        .await
        .unwrap();
    // 364 days is kept
    sqlx::query("UPDATE mf_proofs SET active_at = now() - interval '364 days' WHERE id = $1")
        .bind(recent)
        .execute(&s.pool)
        .await
        .unwrap();
    let files = s.stored.lock().unwrap().len();
    let n = makerfoundry::proofs::forget_old(&s.pool, s.storage.as_ref())
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_proofs").await, 1);
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_proofs WHERE id = {recent}"))
            .await,
        1
    );
    assert_eq!(
        s.stored.lock().unwrap().len(),
        files - 3,
        "its file, photo and thumbnail"
    );
    assert_eq!(s.get(&old_link, None).await.status, StatusCode::NOT_FOUND);
    // a customer looking counts as activity
    let link = proof_link(&s, &shop, recent).await;
    s.get(&link, None).await;
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_proofs WHERE id = {recent} AND active_at > now() - interval '1 minute'"
        ))
        .await,
        1
    );
}

// ---------------------------------------------------------------- MF-54: founding creators

#[tokio::test]
async fn makers_apply_to_be_founding_creators_and_admins_approve_numbered_badges() {
    let Some(s) = site_with(vec![1000]).await else { return };
    let admin = s.sign_up("ada%40example.com").await;
    // the public page, shareable before the launch, with a link preview for Facebook
    let public = s.get("/founding", None).await;
    assert_eq!(public.status, StatusCode::OK);
    assert!(
        public.body.contains("100 of 100 spots left") && public.body.contains("/signup?next=/founding"),
        "{}",
        public.body
    );
    assert!(
        public
            .body
            .contains(r#"property="og:url" content="https://makerfoundry.test/founding""#),
        "{}",
        public.body
    );
    // applying: checked, then waiting
    let nick = s.sign_up("nick%40example.com").await;
    let r = s
        .post(
            "/founding",
            "name=Nick&work_url=javascript%3Aalert(1)&about=Prints",
            Some(&nick),
        )
        .await;
    assert!(r.body.contains("starting https://"), "{}", r.body);
    assert!(r.body.contains("value=\"Nick\""), "the form keeps what was typed");
    let r = s
        .post(
            "/founding",
            "name=Nick+Prints&work_url=https%3A%2F%2Fmakerworld.com%2Fen%2F%40nick&about=Gridfinity+%3Cbins%3E",
            Some(&nick),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(
        s.get("/founding", Some(&nick))
            .await
            .body
            .contains("your application is in")
    );
    // only admins see and decide; forms from elsewhere refused
    assert_eq!(
        s.get("/admin/founding", Some(&nick)).await.status,
        StatusCode::NOT_FOUND
    );
    let queue = s.get("/admin/founding", Some(&admin)).await.body;
    assert!(
        queue.contains("Nick Prints") && queue.contains("Gridfinity &lt;bins&gt;") && queue.contains("waiting"),
        "{queue}"
    );
    assert!(queue.contains(r#"rel="nofollow noopener noreferrer ugc""#));
    assert_eq!(
        s.post("/admin/founding/1001/approve", "", Some(&nick)).await.status,
        StatusCode::NOT_FOUND
    );
    // approved: number 1, the badge on the account page, the maker page and their designs
    let r = s.post("/admin/founding/1001/approve", "", Some(&admin)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(
        s.get("/founding", Some(&nick))
            .await
            .body
            .contains("Founding creator #1")
    );
    assert!(
        s.get("/account", Some(&nick))
            .await
            .body
            .contains("Founding creator #1")
    );
    let id = published(&s, &nick, "nickprints", "Bin").await;
    assert!(
        s.get("/makers/nickprints", None)
            .await
            .body
            .contains("Founding creator #1")
    );
    assert!(
        s.get(&format!("/d/{id}"), None)
            .await
            .body
            .contains("Founding creator #1")
    );
    // an approved application can't be changed by sending the form again
    s.post("/founding", "name=Changed&about=x", Some(&nick)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_founding WHERE name = 'Nick Prints' AND number = 1")
            .await,
        1
    );
    // numbers: the lowest free one; a removed badge frees its number
    let bea = s.sign_up("bea%40example.com").await;
    s.post("/founding", "name=Bea&about=Embroidery", Some(&bea)).await;
    s.post("/admin/founding/1002/approve", "", Some(&admin)).await;
    assert_eq!(
        s.scalar("SELECT number::bigint FROM mf_founding WHERE account_id = 1002")
            .await,
        2
    );
    s.post("/admin/founding/1001/remove", "", Some(&admin)).await;
    assert!(!s.get(&format!("/d/{id}"), None).await.body.contains("Founding creator"));
    let cy = s.sign_up("cy%40example.com").await;
    s.post("/founding", "name=Cy&about=Laser", Some(&cy)).await;
    s.post("/admin/founding/1003/approve", "", Some(&admin)).await;
    assert_eq!(
        s.scalar("SELECT number::bigint FROM mf_founding WHERE account_id = 1003")
            .await,
        1
    );
    // declined, and back to waiting
    let di = s.sign_up("di%40example.com").await;
    s.post("/founding", "name=Di&about=Quilts", Some(&di)).await;
    s.post("/admin/founding/1004/decline", "", Some(&admin)).await;
    assert!(s.get("/founding", Some(&di)).await.body.contains("wasn't a yes"));
    s.post("/admin/founding/1004/reconsider", "", Some(&admin)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_founding WHERE account_id = 1004 AND status = 'waiting'")
            .await,
        1
    );
    // in the data download, and gone with the data
    assert!(
        s.get("/account/data.json", Some(&cy))
            .await
            .body
            .contains("founding_application")
    );
    s.post("/account/data/delete", "confirm=delete", Some(&cy)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_founding WHERE account_id = 1003")
            .await,
        0
    );
}

#[tokio::test]
async fn there_are_never_more_than_100_founding_creators() {
    let Some(s) = site_with(vec![1000]).await else { return };
    let admin = s.sign_up("ada%40example.com").await;
    // 100 approved already (made directly), one more waiting
    sqlx::query(
        "INSERT INTO mf_accounts (account_id, email) SELECT 2000 + n, 'x' || n || '@example.com' FROM generate_series(1, 100) n",
    )
    .execute(&s.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mf_founding (account_id, name, status, number) SELECT 2000 + n, 'Maker ' || n, 'approved', n FROM generate_series(1, 100) n",
    )
    .execute(&s.pool)
    .await
    .unwrap();
    let late = s.sign_up("late%40example.com").await;
    let page = s.get("/founding", Some(&late)).await.body;
    assert!(
        page.contains("All 100 spots are taken") && !page.contains("<form method=\"post\" action=\"/founding\""),
        "{page}"
    );
    sqlx::query("INSERT INTO mf_founding (account_id, name) VALUES (1001, 'Late')")
        .execute(&s.pool)
        .await
        .unwrap();
    let r = s.post("/admin/founding/1001/approve", "", Some(&admin)).await;
    assert!(r.body.contains("All 100 spots are taken"), "{}", r.body);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_founding WHERE status = 'approved'")
            .await,
        100
    );
}

#[tokio::test]
async fn the_3d_section_is_for_admins_until_switched_on() {
    let Some(s) = site_3d(vec![1000], false).await else {
        return;
    };
    let join = |email: &'static str| {
        let s = &s;
        async move {
            s.post(
                "/signup",
                &format!("email={email}&password=fire+truck+1&again=fire+truck+1&agree=yes"),
                None,
            )
            .await
            .cookie()
        }
    };
    let admin = join("admin%40example.com").await;
    let maker = join("maker%40example.com").await;
    assert_eq!(s.get("/3d", None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get("/3d", Some(&maker)).await.status, StatusCode::NOT_FOUND);
    let page = s.get("/3d", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("MakerFoundry 3D"));
    assert!(page.body.contains("Tools &amp; workshop"), "the categories are listed");
    let robots = s.get("/robots.txt", None).await.body;
    assert!(robots.contains("Disallow: /3d\n"), "{robots}");
    assert!(!s.get("/sitemap.xml", None).await.body.contains("/3d<"));

    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let page = s.get("/3d", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("3D printer parts &amp; upgrades"));
    assert!(!s.get("/robots.txt", None).await.body.contains("/3d"));
    assert!(
        s.get("/sitemap.xml", None)
            .await
            .body
            .contains("https://makerfoundry.test/3d<")
    );
}

#[tokio::test]
async fn a_3d_licence_is_refused_on_an_embroidery_design() {
    let Some(s) = site().await else { return };
    let cookie = s.sign_up("maker%40example.com").await;
    let id = add_design(&s, &cookie, sf_formats::Format::Pes, "rose.pes").await;
    let form = s.get(&format!("/designs/{id}/edit"), Some(&cookie)).await.body;
    assert!(form.contains("value=\"cc-by\""));
    assert!(
        !form.contains("gpl-3.0-or-later"),
        "only the embroidery licences are offered"
    );
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Rose&licence=gpl-3.0-or-later",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("Choose a licence."), "{}", r.body);
}

// ---------------------------------------------------------------- MakerFoundry 3D (MF-41 onwards)

impl Site {
    /// For looking at a page (CLAUDE.md › Look at what you build): with `MF_DUMP_PAGES` set to a
    /// folder, writes `html` there as `<name>.html` with the stylesheet and every picture it shows
    /// (fetched as `cookie` would see them), ready to screenshot. Does nothing otherwise.
    async fn dump_page(&self, name: &str, html: &str, cookie: Option<&str>) {
        let Ok(dir) = std::env::var("MF_DUMP_PAGES") else {
            return;
        };
        let mut html = html.replace("/static/site.css", "site.css");
        std::fs::write(format!("{dir}/site.css"), include_str!("../static/site.css")).unwrap();
        let mut sources: Vec<String> = Vec::new();
        for attr in [r#"src=""#, r#"data-mesh=""#] {
            sources.extend(
                html.split(attr)
                    .skip(1)
                    .filter_map(|t| t.split('"').next())
                    .filter(|u| u.starts_with('/'))
                    .map(String::from),
            );
        }
        for (i, src) in sources.iter().enumerate() {
            let mut req = Request::get(src.replace("&amp;", "&")).header(header::HOST, HOST);
            if let Some(c) = cookie {
                req = req.header(header::COOKIE, c);
            }
            let resp = self
                .app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let ext = if src.ends_with(".js") { "js" } else { "img" };
            let file = format!("{name}-{i}.{ext}");
            std::fs::write(format!("{dir}/{file}"), &bytes).unwrap();
            html = html
                .replace(&format!(r#"src="{src}""#), &format!(r#"src="{file}""#))
                .replace(&format!(r#"data-mesh="{src}""#), &format!(r#"data-mesh="{file}""#));
        }
        std::fs::write(format!("{dir}/{name}.html"), html).unwrap();
    }

    /// Signs up agreeing to the Terms (asked for once launched) → the session cookie.
    async fn join(&self, email: &str) -> String {
        self.post(
            "/signup",
            &format!("email={email}&password=fire+truck+1&again=fire+truck+1&agree=yes"),
            None,
        )
        .await
        .cookie()
    }
}

/// A binary STL of a cube `side` mm across.
fn stl_cube(side: f32) -> Vec<u8> {
    let p = |i: u8| {
        [
            if i & 1 != 0 { side } else { 0.0 },
            if i & 2 != 0 { side } else { 0.0 },
            if i & 4 != 0 { side } else { 0.0 },
        ]
    };
    let faces = [
        [0, 2, 3, 1],
        [4, 5, 7, 6],
        [0, 1, 5, 4],
        [2, 6, 7, 3],
        [0, 4, 6, 2],
        [1, 3, 7, 5],
    ];
    let mut out = vec![0u8; 80];
    out.extend(12u32.to_le_bytes());
    for [a, b, c, d] in faces {
        for t in [[a, b, c], [a, c, d]] {
            out.extend([0u8; 12]);
            for i in t {
                for n in p(i) {
                    out.extend(n.to_le_bytes());
                }
            }
            out.extend([0u8; 2]);
        }
    }
    out
}

fn small_png() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(16, 16, image::Rgba([200, 60, 40, 255])))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// A Bambu-style 3MF: a 20 mm cube, a plate thumbnail, settings, and sliced G-code (dropped).
fn project_3mf(model_xml: Option<&str>) -> Vec<u8> {
    let mut vertices = String::new();
    for i in 0..8u8 {
        let c = |bit: u8| if i & bit != 0 { 20 } else { 0 };
        vertices.push_str(&format!(r#"<vertex x="{}" y="{}" z="{}"/>"#, c(1), c(2), c(4)));
    }
    let mut triangles = String::new();
    for [a, b, c, d] in [
        [0, 2, 3, 1],
        [4, 5, 7, 6],
        [0, 1, 5, 4],
        [2, 6, 7, 3],
        [0, 4, 6, 2],
        [1, 3, 7, 5],
    ] {
        triangles.push_str(&format!(
            r#"<triangle v1="{a}" v2="{b}" v3="{c}"/><triangle v1="{a}" v2="{c}" v3="{d}"/>"#
        ));
    }
    let good = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"><resources><object id="1"><mesh><vertices>{vertices}</vertices><triangles>{triangles}</triangles></mesh></object></resources><build><item objectid="1"/></build></model>"#
    );
    let rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/><Relationship Target="/Metadata/plate_1.png" Id="rel1" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail"/></Relationships>"#;
    let files = vec![
        ("_rels/.rels".to_string(), rels.as_bytes().to_vec()),
        (
            "3D/3dmodel.model".to_string(),
            model_xml.unwrap_or(&good).as_bytes().to_vec(),
        ),
        ("Metadata/plate_1.png".to_string(), small_png()),
        (
            "Metadata/project_settings.config".to_string(),
            br#"{"layer_height": "0.2"}"#.to_vec(),
        ),
        ("Metadata/plate_1.gcode".to_string(), b"G28\nG1 X10\n".to_vec()),
    ];
    makerfoundry::model3d::zip::write(&files).unwrap()
}

const STEP_FILE: &str =
    "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('test'),'2;1');\nENDSEC;\nDATA;\nENDSEC;\nEND-ISO-10303-21;\n";

/// A model file as kept: name, role, key, triangles, picture, views, what was dropped.
type FileRow = (
    String,
    String,
    String,
    Option<i64>,
    Option<String>,
    Vec<String>,
    Vec<String>,
);
/// The files of one upload, by name.
type ModelUpload<'a> = Vec<(&'a str, Vec<u8>)>;

/// Uploads a model (files and images) → the answer.
async fn upload_model(s: &Site, cookie: &str, files: &[(&str, Vec<u8>)], images: &[Vec<u8>]) -> Answer {
    let mut parts: Vec<(&str, &str, Vec<u8>)> = files.iter().map(|(n, b)| ("files", *n, b.clone())).collect();
    parts.extend(images.iter().map(|b| ("images", "photo.jpg", b.clone())));
    s.send(multipart("/3d/new", &parts, cookie)).await
}

#[tokio::test]
async fn a_good_upload_makes_one_draft_with_its_files_and_images() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let form = s.get("/3d/new", Some(&cookie)).await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains(r#"name="files""#) && form.body.contains("multiple"));
    let r = upload_model(
        &s,
        &cookie,
        &[
            ("cube.stl", stl_cube(10.0)),
            ("cube project.3mf", project_3mf(None)),
            ("cube.step", STEP_FILE.as_bytes().to_vec()),
        ],
        &[photo_with_location()],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = design_id(r.location());
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 1);
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_designs WHERE id = {id} AND kind = 'model3d' AND status = 'draft'"
        ))
        .await,
        1
    );
    let files: Vec<FileRow> = sqlx::query_as(
        "SELECT file_name, role, file_key, triangles, thumb_key, view_keys, dropped FROM mf_model_files WHERE design_id = $1 ORDER BY position",
    )
    .bind(id)
    .fetch_all(&s.pool)
    .await
    .unwrap();
    let roles: Vec<_> = files.iter().map(|f| (f.0.as_str(), f.1.as_str())).collect();
    assert_eq!(
        roles,
        [
            ("cube.stl", "mesh"),
            ("cube project.3mf", "project"),
            ("cube.step", "source")
        ]
    );
    assert_eq!(files[0].3, Some(12));
    assert!(
        files[0].4.is_some() && files[0].5.len() == 3,
        "a mesh gets a picture and three views"
    );
    assert!(files[1].4.is_some(), "a 3MF's own thumbnail");
    assert!(
        files[2].4.is_none() && files[2].5.is_empty(),
        "a STEP file has no picture"
    );
    assert_eq!(files[1].6, ["Metadata/plate_1.gcode"]);
    // what's stored for the 3MF is the rebuilt file, without the G-code
    let stored_3mf = s.stored.lock().unwrap().get(&files[1].2).unwrap().1.clone();
    let a = makerfoundry::model3d::zip::Archive::open(&stored_3mf).unwrap();
    assert!(a.find("Metadata/plate_1.gcode").is_none() && a.find("3D/3dmodel.model").is_some());
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_design_photos WHERE design_id = {id}"))
            .await,
        1
    );
    // the maker's draft page lists the files and what was dropped
    let page = s.get(&format!("/designs/{id}"), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    for text in [
        "cube.stl",
        "cube project.3mf",
        "cube.step",
        "Metadata/plate_1.gcode",
        "10 × 10 × 10 mm",
    ] {
        assert!(page.body.contains(text), "{text}: {}", page.body);
    }
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND role = 'mesh'")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    for view in ["hero", "front", "side", "top"] {
        let pic = s.get(&format!("/3d/{id}/files/{fid}/{view}"), Some(&cookie)).await;
        assert_eq!(pic.status, StatusCode::OK, "{view}");
        assert_eq!(pic.headers[header::CONTENT_TYPE], "image/png");
    }
    assert_eq!(
        s.get(&format!("/3d/{id}/files/{fid}/back"), Some(&cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    // deleting the draft deletes every stored file
    let r = s.post(&format!("/designs/{id}/delete"), "", Some(&cookie)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(s.stored.lock().unwrap().is_empty(), "nothing left in storage");
}

#[tokio::test]
async fn a_bad_file_a_huge_file_or_a_malformed_3mf_is_refused_and_nothing_is_kept() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let html = b"<!doctype html><html><body><script>alert(1)</script></body></html>".to_vec();
    let broken = project_3mf(Some(
        "<model><resources><object id=\"1\"><mesh><vertices/><triangles><triangle v1=\"0\" v2=\"1\" v3=\"9\"/></triangles></mesh></object></resources><build><item objectid=\"1\"/></build></model>",
    ));
    let cases: Vec<(ModelUpload, &str)> = vec![
        (
            vec![("cube.stl", stl_cube(10.0)), ("page.stl", html.clone())],
            "page.stl",
        ),
        (vec![("cube.3mf", broken)], "cube.3mf"),
        (
            vec![("cube.stl", stl_cube(10.0)), ("setup.exe", b"MZ".to_vec())],
            "STL, 3MF, OBJ and STEP",
        ),
        (
            vec![("part.step", STEP_FILE.as_bytes().to_vec())],
            "at least one STL, 3MF or OBJ",
        ),
        (
            vec![("huge.stl", vec![0u8; 100 * 1024 * 1024 + 1])],
            "larger than 100 MB",
        ),
        (vec![], "Choose"),
    ];
    for (files, why) in cases {
        let r = upload_model(&s, &cookie, &files, &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{why}");
        assert!(r.body.contains(why), "{why}: {}", r.body);
    }
    let r = upload_model(&s, &cookie, &[("cube.stl", stl_cube(10.0))], &[html]).await;
    assert!(r.body.contains("Image 1"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 0);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_model_files").await, 0);
    assert!(s.stored.lock().unwrap().is_empty(), "nothing kept");
}

#[tokio::test]
async fn too_many_files_are_refused() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let names: Vec<String> = (0..31).map(|i| format!("part{i}.stl")).collect();
    let files: Vec<(&str, Vec<u8>)> = names.iter().map(|n| (n.as_str(), stl_cube(5.0))).collect();
    let r = upload_model(&s, &cookie, &files, &[]).await;
    assert!(r.body.contains("up to 30 files"), "{}", r.body);
    let r = upload_model(&s, &cookie, &files[..30], &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "30 is allowed");
    let images: Vec<Vec<u8>> = (0..13).map(|_| small_png()).collect();
    let r = upload_model(&s, &cookie, &files[..1], &images).await;
    assert!(r.body.contains("up to 12 images"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 1);
}

#[tokio::test]
async fn others_cant_see_or_change_a_draft() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let maker = s.join("maker%40example.com").await;
    let other = s.join("other%40example.com").await;
    let r = upload_model(&s, &maker, &[("cube.stl", stl_cube(10.0))], &[]).await;
    let id = design_id(r.location());
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    for who in [Some(other.as_str()), None] {
        assert_eq!(
            s.get(&format!("/3d/{id}/files/{fid}/hero"), who).await.status,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        s.get(&format!("/designs/{id}"), Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.post(&format!("/designs/{id}/delete"), "", Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_model_files").await, 1);
    assert_eq!(s.get("/3d/new", None).await.location(), "/signin?next=/3d/new");

    // while MakerFoundry 3D is off, only admins may upload
    let Some(s) = site_3d(vec![1000], false).await else {
        return;
    };
    let admin = s.join("admin%40example.com").await;
    let maker = s.join("maker%40example.com").await;
    assert_eq!(s.get("/3d/new", Some(&maker)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        upload_model(&s, &maker, &[("cube.stl", stl_cube(10.0))], &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.get("/3d/new", Some(&admin)).await.status, StatusCode::OK);
    assert_eq!(s.get("/3d/new", None).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_daily_limit_holds() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let cube = stl_cube(5.0);
    for i in 0..makerfoundry::limits::MODEL_UPLOAD.max {
        let r = upload_model(&s, &cookie, &[("cube.stl", cube.clone())], &[]).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "upload {i}");
    }
    let r = upload_model(&s, &cookie, &[("cube.stl", cube)], &[]).await;
    assert!(r.body.contains("a lot of models today"), "{}", r.body);
}

/// A maker (with a profile) and their uploaded model of three files → the cookie and the id.
async fn model_draft(s: &Site, email: &str, handle: &str) -> (String, i64) {
    let cookie = s.join(email).await;
    s.post("/account/maker", &format!("name=Maker&handle={handle}"), Some(&cookie))
        .await;
    let r = upload_model(
        s,
        &cookie,
        &[
            ("cube.stl", stl_cube(10.0)),
            ("cube project.3mf", project_3mf(None)),
            ("cube.step", STEP_FILE.as_bytes().to_vec()),
        ],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    (cookie, design_id(r.location()))
}

/// The same, with its details given and published.
async fn published_model(s: &Site, email: &str, handle: &str) -> (String, i64) {
    let (cookie, id) = model_draft(s, email, handle).await;
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Cube&category=household&licence=cc-by-sa&price=0",
            Some(&cookie),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let r = s
        .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    (cookie, id)
}

#[tokio::test]
async fn a_draft_needs_a_title_category_and_licence_to_publish() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = model_draft(&s, "maker%40example.com", "maker").await;
    let manage = s.get(&format!("/designs/{id}"), Some(&cookie)).await.body;
    assert!(
        manage.contains("Edit details") && manage.contains("I created this model"),
        "{manage}"
    );
    let r = s
        .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert!(r.body.contains("a title, a category and a licence"), "{}", r.body);
    let form = s.get(&format!("/designs/{id}/edit"), Some(&cookie)).await.body;
    assert!(form.contains(r#"name="category""#) && form.contains("3D printer parts &amp; upgrades"));
    assert!(form.contains("gpl-3.0-or-later"), "the 3D licences are offered");
    assert!(!form.contains(r#"name="material""#) && !form.contains(r#"name="convert""#));
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Cube&licence=cc-by-sa&price=0",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("Choose a category"), "{}", r.body);
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Cube&category=nope&licence=cc-by-sa&price=0",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("Choose a category"), "{}", r.body);
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Cube&category=household&licence=cc-by-sa&price=0",
            Some(&cookie),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let manage = s.get(&format!("/designs/{id}"), Some(&cookie)).await.body;
    assert!(manage.contains("Household"), "{manage}");
    let r = s.post(&format!("/designs/{id}/publish"), "", Some(&cookie)).await;
    assert!(r.body.contains("Confirm"), "{}", r.body);
    let r = s
        .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert_eq!(r.location(), format!("/d/{id}/cube"));
}

#[tokio::test]
async fn published_is_public_unpublished_is_not() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = published_model(&s, "maker%40example.com", "maker").await;
    let page = s.get(&format!("/d/{id}/cube"), None).await;
    assert_eq!(page.status, StatusCode::OK);
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 ORDER BY position LIMIT 1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    for text in [
        "<h1>Cube</h1>",
        &format!(r#"src="/3d/{id}/files/{fid}/hero""#),
        &format!(r#"href="/d/{id}/download""#),
        "Download all (ZIP)",
        "cube.step",
        "Household",
        "Creative Commons Attribution-ShareAlike",
    ] {
        assert!(page.body.contains(text), "{text}: {}", page.body);
    }
    s.dump_page("model-public", &page.body, None).await;
    let manage = s.get(&format!("/designs/{id}"), Some(&cookie)).await.body;
    s.dump_page("model-manage", &manage, Some(&cookie)).await;
    assert!(
        !page.body.contains("plate_1.gcode"),
        "what was dropped is for the maker only"
    );
    assert!(!page.body.contains("hoop"), "{}", page.body);
    assert_eq!(
        s.get(&format!("/3d/{id}/files/{fid}/hero"), None).await.status,
        StatusCode::OK
    );
    // it's found in browsing, as a 3D model with its picture
    let browse = s.get("/browse", None).await.body;
    assert!(
        browse.contains("3D model") && browse.contains(&format!("/3d/{id}/files/{fid}/hero")),
        "{browse}"
    );
    s.post(&format!("/designs/{id}/unpublish"), "", Some(&cookie)).await;
    assert_eq!(
        s.get(&format!("/d/{id}/cube"), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.get(&format!("/d/{id}/download"), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.get(&format!("/3d/{id}/files/{fid}/hero"), None).await.status,
        StatusCode::NOT_FOUND
    );

    // while MakerFoundry 3D is off, a model an admin published isn't shown to others yet
    let Some(s) = site_3d(vec![1000], false).await else {
        return;
    };
    let (_, id) = published_model(&s, "admin%40example.com", "admin").await;
    assert_eq!(
        s.get(&format!("/d/{id}/cube"), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!s.get("/browse", None).await.body.contains("Cube"));
    assert!(!s.get("/sitemap.xml", None).await.body.contains(&format!("/d/{id}/")));
}

#[tokio::test]
async fn the_zip_holds_exactly_the_models_files() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, id) = published_model(&s, "maker%40example.com", "maker").await;
    let r = s.get(&format!("/d/{id}/download"), None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().contains("cube.zip"), "{}", r.location());
    let zip_key: String = sqlx::query_scalar("SELECT zip_key FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let zip = s.stored.lock().unwrap().get(&zip_key).unwrap().1.clone();
    let a = makerfoundry::model3d::zip::Archive::open(&zip).unwrap();
    let names: Vec<_> = a.entries().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["cube.stl", "cube project.3mf", "cube.step"]);
    let kept: Vec<(String, String)> =
        sqlx::query_as("SELECT file_name, file_key FROM mf_model_files WHERE design_id = $1 ORDER BY position")
            .bind(id)
            .fetch_all(&s.pool)
            .await
            .unwrap();
    for (name, key) in kept {
        let original = s.stored.lock().unwrap().get(&key).unwrap().1.clone();
        assert_eq!(a.read(a.find(&name).unwrap()).unwrap(), original, "{name}");
    }
    // made once
    let files = s.stored.lock().unwrap().len();
    s.get(&format!("/d/{id}/download"), None).await;
    assert_eq!(s.stored.lock().unwrap().len(), files);
}

#[tokio::test]
async fn downloads_are_counted() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = model_draft(&s, "maker%40example.com", "maker").await;
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND format = 'step'")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    // the maker's own downloads of a draft aren't counted
    let r = s
        .get(&format!("/d/{id}/download?model_file={fid}"), Some(&cookie))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().contains("cube.step"), "{}", r.location());
    assert_eq!(
        s.scalar(&format!("SELECT downloads FROM mf_designs WHERE id = {id}"))
            .await,
        0
    );
    s.post(
        &format!("/designs/{id}/edit"),
        "title=Cube&category=household&licence=cc0&price=0",
        Some(&cookie),
    )
    .await;
    s.post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie))
        .await;
    s.get(&format!("/d/{id}/download?model_file={fid}"), None).await;
    s.get(&format!("/d/{id}/download"), None).await;
    assert_eq!(
        s.scalar(&format!("SELECT downloads FROM mf_designs WHERE id = {id}"))
            .await,
        2
    );
    // another model's file isn't reachable through this one
    let (_, other) = model_draft(&s, "other%40example.com", "other").await;
    let theirs: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 LIMIT 1")
        .bind(other)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        s.get(&format!("/d/{id}/download?model_file={theirs}"), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_page_has_its_link_preview() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, id) = published_model(&s, "maker%40example.com", "maker").await;
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 ORDER BY position LIMIT 1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let page = s.get(&format!("/d/{id}/cube"), None).await.body;
    assert!(page.contains(r#"<meta property="og:title" content="Cube">"#), "{page}");
    assert!(
        page.contains(&format!(
            r#"<meta property="og:image" content="https://makerfoundry.test/3d/{id}/files/{fid}/hero">"#
        )),
        "{page}"
    );
    assert!(page.contains("A 3D model by Maker on MakerFoundry"), "{page}");
}

#[tokio::test]
async fn uploaded_file_names_lose_their_folders() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let r = upload_model(
        &s,
        &cookie,
        &[
            ("../../etc/evil.stl", stl_cube(5.0)),
            ("C:\\Users\\me\\part.stl", stl_cube(5.0)),
        ],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let names: Vec<String> = sqlx::query_scalar("SELECT file_name FROM mf_model_files ORDER BY position")
        .fetch_all(&s.pool)
        .await
        .unwrap();
    assert_eq!(names, ["evil.stl", "part.stl"]);
}

// ---------------------------------------------------------------- bulk import (MF-52)

/// Sends files to the import → the answer.
async fn import_files(s: &Site, cookie: &str, files: &[(&str, Vec<u8>)]) -> Answer {
    let parts: Vec<(&str, &str, Vec<u8>)> = files.iter().map(|(n, b)| ("files", *n, b.clone())).collect();
    s.send(multipart("/3d/import", &parts, cookie)).await
}

fn import_id(location: &str) -> i64 {
    location.trim_start_matches("/3d/import/").parse().unwrap()
}

#[tokio::test]
async fn thirty_files_across_several_models_group_by_name() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let names: Vec<String> = (0..10)
        .flat_map(|i| {
            [
                format!("model{i}_plate1.stl"),
                format!("model{i}_plate2.stl"),
                format!("model{i}.png"),
            ]
        })
        .collect();
    let files: Vec<(&str, Vec<u8>)> = names
        .iter()
        .map(|n| {
            (
                n.as_str(),
                if n.ends_with(".png") {
                    small_png()
                } else {
                    stl_cube(5.0)
                },
            )
        })
        .collect();
    let r = import_files(&s, &cookie, &files).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let iid = import_id(r.location());
    let review = s.get(&format!("/3d/import/{iid}"), Some(&cookie)).await;
    assert_eq!(review.status, StatusCode::OK);
    assert!(
        review.body.contains("10 models") && review.body.contains("model7"),
        "{}",
        review.body
    );
    let r = s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&cookie)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_designs WHERE import_id = {iid} AND status = 'draft'"
        ))
        .await,
        10
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_model_files").await, 20);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_design_photos").await, 10);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_designs WHERE title = 'model3'").await,
        1,
        "titled by the group"
    );
    // the pending copies are gone once their drafts are made
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_import_files WHERE file_key IS NOT NULL")
            .await,
        0
    );
    // someone else can't see the import
    let other = s.join("other%40example.com").await;
    assert_eq!(
        s.get(&format!("/3d/import/{iid}"), Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&other))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_zip_with_nested_folders_groups_by_folder() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let zip = makerfoundry::model3d::zip::write(&[
        ("My models/".to_string(), Vec::new()),
        ("My models/Planter/base.stl".to_string(), stl_cube(10.0)),
        ("My models/Planter/tray.stl".to_string(), stl_cube(8.0)),
        ("My models/Planter/print.jpg".to_string(), photo_with_location()),
        ("My models/Hook/hook.3mf".to_string(), project_3mf(None)),
        ("__MACOSX/My models/._base.stl".to_string(), b"junk".to_vec()),
        ("My models/.DS_Store".to_string(), b"junk".to_vec()),
    ])
    .unwrap();
    let r = import_files(&s, &cookie, &[("models.zip", zip), ("loose.stl", stl_cube(3.0))]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let iid = import_id(r.location());
    let review = s.get(&format!("/3d/import/{iid}"), Some(&cookie)).await.body;
    for text in ["3 models", "Planter", "Hook", "loose", "base.stl", "print.jpg"] {
        assert!(review.contains(text), "{text}: {review}");
    }
    assert!(!review.contains("DS_Store") && !review.contains("MACOSX"), "{review}");
    s.dump_page("import-review", &review, Some(&cookie)).await;
    s.dump_page(
        "import-form",
        &s.get("/3d/import", Some(&cookie)).await.body,
        Some(&cookie),
    )
    .await;
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&cookie)).await;
    let planter: i64 = sqlx::query_scalar("SELECT id FROM mf_designs WHERE title = 'Planter'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_model_files WHERE design_id = {planter}"
        ))
        .await,
        2
    );
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_design_photos WHERE design_id = {planter}"
        ))
        .await,
        1
    );
}

#[tokio::test]
async fn a_zip_bomb_or_traversal_entry_is_refused() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let traversal = makerfoundry::model3d::zip::write(&[("../../evil.stl".to_string(), stl_cube(5.0))]).unwrap();
    let r = import_files(&s, &cookie, &[("good.stl", stl_cube(5.0)), ("models.zip", traversal)]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body.contains("models.zip") && r.body.contains("outside it"),
        "{}",
        r.body
    );
    let bomb = makerfoundry::model3d::zip::write(&[("big.stl".to_string(), vec![0u8; 4 * 1024 * 1024])]).unwrap();
    let r = import_files(&s, &cookie, &[("models.zip", bomb)]).await;
    assert!(r.body.contains("zip bomb"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_imports").await, 0);
    assert!(s.stored.lock().unwrap().is_empty(), "nothing kept");
}

#[tokio::test]
async fn an_oversized_batch_is_refused() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let names: Vec<String> = (0..201).map(|i| format!("part{i}.stl")).collect();
    let cube = stl_cube(2.0);
    let files: Vec<(&str, Vec<u8>)> = names.iter().map(|n| (n.as_str(), cube.clone())).collect();
    let r = import_files(&s, &cookie, &files).await;
    assert!(r.body.contains("up to 200 files"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_import_files").await, 0);
    assert!(s.stored.lock().unwrap().is_empty(), "nothing kept");
    let r = import_files(&s, &cookie, &files[..200]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "200 is allowed");
}

#[tokio::test]
async fn a_bad_file_is_reported_and_the_rest_kept() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let html = b"<!doctype html><html><body>not a model</body></html>".to_vec();
    let r = import_files(
        &s,
        &cookie,
        &[
            ("vase.stl", stl_cube(5.0)),
            ("vase_part2.stl", html),
            ("setup.exe", b"MZ".to_vec()),
            ("lamp.stl", stl_cube(5.0)),
        ],
    )
    .await;
    let iid = import_id(r.location());
    let review = s.get(&format!("/3d/import/{iid}"), Some(&cookie)).await.body;
    assert!(
        review.contains("setup.exe") && review.contains("isn&#39;t a model file"),
        "{review}"
    );
    let r = s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&cookie)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let review = s.get(&format!("/3d/import/{iid}"), Some(&cookie)).await.body;
    assert!(
        review.contains("vase_part2.stl") && review.contains("left out"),
        "{review}"
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 2, "vase and lamp");
    let vase: i64 = sqlx::query_scalar("SELECT id FROM mf_designs WHERE title = 'vase'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_model_files WHERE design_id = {vase}"))
            .await,
        1
    );
}

#[tokio::test]
async fn imports_and_their_unpublished_drafts_go_after_30_days() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let r = import_files(&s, &cookie, &[("vase.stl", stl_cube(5.0)), ("lamp.stl", stl_cube(5.0))]).await;
    let iid = import_id(r.location());
    // one becomes a draft; the other waits
    sqlx::query("UPDATE mf_import_files SET group_name = 'lamp-later' WHERE path = 'lamp.stl'")
        .execute(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&cookie)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 2);
    // a day short of 30: nothing goes
    sqlx::query("UPDATE mf_imports SET created_at = now() - interval '29 days'")
        .execute(&s.pool)
        .await
        .unwrap();
    makerfoundry::model3d::import::forget_old(&s.pool, s.storage.as_ref())
        .await
        .unwrap();
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 2);
    // past 30, unpublished imported drafts and anything still waiting go, with their files
    let lamp: i64 = sqlx::query_scalar("SELECT id FROM mf_designs WHERE title = 'lamp-later'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE mf_designs SET status = 'published', published_at = now() WHERE id = $1")
        .bind(lamp)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mf_import_files (import_id, path, file_key, file_bytes, image, position) VALUES ($1, 'late.stl', 'import/late', 1, false, 99)")
        .bind(iid)
        .execute(&s.pool)
        .await
        .unwrap();
    s.stored
        .lock()
        .unwrap()
        .insert("import/late".into(), ("application/octet-stream".into(), vec![1]));
    sqlx::query("UPDATE mf_imports SET created_at = now() - interval '31 days'")
        .execute(&s.pool)
        .await
        .unwrap();
    makerfoundry::model3d::import::forget_old(&s.pool, s.storage.as_ref())
        .await
        .unwrap();
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_designs").await,
        1,
        "the published one stays"
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_import_files WHERE file_key IS NOT NULL")
            .await,
        0
    );
    let left: Vec<String> = s.stored.lock().unwrap().keys().cloned().collect();
    let lamp_keys = makerfoundry::model3d::upload::keys(&s.pool, &[lamp]).await;
    assert!(
        left.iter().all(|k| lamp_keys.contains(k)),
        "only the published model's files are left: {left:?}"
    );
}

// ---------------------------------------------------------------- import review (MF-53)

/// An import of three loose models (vase, lamp, hook) by a maker with a profile → cookie, import.
async fn three_model_import(s: &Site) -> (String, i64) {
    let cookie = s.join("maker%40example.com").await;
    s.post("/account/maker", "name=Maker&handle=maker", Some(&cookie)).await;
    let r = import_files(
        s,
        &cookie,
        &[
            ("vase.stl", stl_cube(5.0)),
            ("vase_part2.stl", stl_cube(4.0)),
            ("lamp.stl", stl_cube(6.0)),
            ("hook.stl", stl_cube(3.0)),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    (cookie, import_id(r.location()))
}

async fn file_id(s: &Site, path: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM mf_import_files WHERE path = $1")
        .bind(path)
        .fetch_one(&s.pool)
        .await
        .unwrap()
}

async fn groups_of(s: &Site, iid: i64) -> Vec<(String, String)> {
    sqlx::query_as("SELECT group_name, path FROM mf_import_files WHERE import_id = $1 ORDER BY group_name, path")
        .bind(iid)
        .fetch_all(&s.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn files_move_between_groups_and_groups_merge_split_and_rename() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, iid) = three_model_import(&s).await;
    let review = s.get(&format!("/3d/import/{iid}"), Some(&cookie)).await.body;
    s.dump_page("import-arrange", &review, Some(&cookie)).await;
    let (vase2, hook, lamp) = (
        file_id(&s, "vase_part2.stl").await,
        file_id(&s, "hook.stl").await,
        file_id(&s, "lamp.stl").await,
    );
    assert!(review.contains(&format!(r#"name="f{vase2}" value="vase""#)), "{review}");
    // split vase_part2 out as its own model, merge hook into lamp, rename vase
    let form = format!(
        "f{vase2}=Vase+insert&f{hook}=lamp&f{lamp}=lamp&old0=hook&new0=hook&old1=lamp&new1=Desk+lamp&old2=vase&new2=Tall+vase"
    );
    let r = s.post(&format!("/3d/import/{iid}/arrange"), &form, Some(&cookie)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        groups_of(&s, iid).await,
        [
            ("Desk lamp".to_string(), "hook.stl".to_string()),
            ("Desk lamp".to_string(), "lamp.stl".to_string()),
            ("Tall vase".to_string(), "vase.stl".to_string()),
            ("Vase insert".to_string(), "vase_part2.stl".to_string()),
        ]
    );
    // an empty name keeps the file where it was; someone else can't arrange it
    s.post(
        &format!("/3d/import/{iid}/arrange"),
        &format!("f{vase2}=+"),
        Some(&cookie),
    )
    .await;
    assert_eq!(groups_of(&s, iid).await[3].0, "Vase insert");
    let other = s.join("other%40example.com").await;
    let r = s
        .post(
            &format!("/3d/import/{iid}/arrange"),
            &format!("f{vase2}=mine"),
            Some(&other),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&cookie)).await;
    let mut titles: Vec<String> = sqlx::query_scalar("SELECT title FROM mf_designs")
        .fetch_all(&s.pool)
        .await
        .unwrap();
    titles.sort();
    assert_eq!(titles, ["Desk lamp", "Tall vase", "Vase insert"]);
}

#[tokio::test]
async fn bulk_publish_needs_the_confirmation_and_every_draft_its_details() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, iid) = three_model_import(&s).await;
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&cookie)).await;
    let review = s.get(&format!("/3d/import/{iid}"), Some(&cookie)).await.body;
    assert!(
        review.contains("I confirm I created these models or have the right to share them"),
        "{review}"
    );
    // without a licence or category none can publish
    let r = s
        .post(&format!("/3d/import/{iid}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert!(r.body.contains("needs a category and a licence"), "{}", r.body);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_designs WHERE status = 'published'")
            .await,
        0
    );
    // details for every draft at once, then one keeps its own
    let lamp: i64 = sqlx::query_scalar("SELECT id FROM mf_designs WHERE title = 'lamp'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(
        &format!("/designs/{lamp}/edit"),
        "title=lamp&category=art-decor&licence=cc0&price=0",
        Some(&cookie),
    )
    .await;
    let r = s
        .post(
            &format!("/3d/import/{iid}/details"),
            "category=household&licence=cc-by",
            Some(&cookie),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_designs WHERE category = 'household' AND licence = 'cc-by'")
            .await,
        2
    );
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_designs WHERE id = {lamp} AND category = 'art-decor'"
        ))
        .await,
        1,
        "kept its own"
    );
    let r = s
        .post(
            &format!("/3d/import/{iid}/details"),
            "category=nope&licence=cc-by",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("Choose a category"), "{}", r.body);
    let r = s
        .post(
            &format!("/3d/import/{iid}/details"),
            "category=household&licence=gpl-9",
            Some(&cookie),
        )
        .await;
    assert!(r.body.contains("Choose a licence"), "{}", r.body);
    // the confirmation is needed
    let r = s.post(&format!("/3d/import/{iid}/publish"), "", Some(&cookie)).await;
    assert!(r.body.contains("Confirm"), "{}", r.body);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_designs WHERE status = 'published'")
            .await,
        0
    );
    let r = s
        .post(&format!("/3d/import/{iid}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_designs WHERE status = 'published'")
            .await,
        3
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_agreements WHERE document = 'seller-terms'")
            .await,
        1
    );
}

#[tokio::test]
async fn an_origin_address_on_another_site_is_refused_and_a_good_one_shows() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = model_draft(&s, "maker%40example.com", "maker").await;
    let form = s.get(&format!("/designs/{id}/edit"), Some(&cookie)).await.body;
    assert!(
        form.contains(r#"name="original_site""#) && form.contains("MakerWorld"),
        "{form}"
    );
    let base = "title=Cube&category=household&licence=cc-by&price=0";
    for (extra, why) in [
        (
            "&original_site=makerworld&original_url=https%3A%2F%2Fevil.example%2Fmodels%2F1",
            "isn&#39;t on MakerWorld",
        ),
        (
            "&original_site=printables&original_url=https%3A%2F%2Fmakerworld.com%2Fen%2Fmodels%2F1",
            "isn&#39;t on Printables",
        ),
        (
            "&original_site=thingiverse&original_url=http%3A%2F%2Fwww.thingiverse.com%2Fthing%3A1",
            "isn&#39;t on Thingiverse",
        ),
        (
            "&original_site=thingiverse&original_url=https%3A%2F%2Fthingiverse.com.evil.example%2Fthing%3A1",
            "isn&#39;t on Thingiverse",
        ),
        ("&original_site=myspace&original_url=", "Choose where"),
        (
            "&original_url=https%3A%2F%2Fwww.printables.com%2Fmodel%2F1",
            "Choose where",
        ),
        (
            "&original_site=printables&original_url=https%3A%2F%2Fwww.printables.com%2Fmodel%2F1&original_date=2025-02-30",
            "date",
        ),
        (
            "&original_site=printables&original_url=https%3A%2F%2Fwww.printables.com%2Fmodel%2F1&original_date=2999-01-01",
            "date",
        ),
    ] {
        let r = s
            .post(&format!("/designs/{id}/edit"), &format!("{base}{extra}"), Some(&cookie))
            .await;
        assert!(r.body.contains(why), "{extra}: {}", r.body);
    }
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            &format!("{base}&original_site=makerworld&original_url=https%3A%2F%2Fmakerworld.com%2Fen%2Fmodels%2F12345&original_date=2025-03-12"),
            Some(&cookie),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    s.post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie))
        .await;
    let page = s.get(&format!("/d/{id}/cube"), None).await.body;
    assert!(
        page.contains(r#"Originally published on <a href="https://makerworld.com/en/models/12345" rel="nofollow noopener">MakerWorld</a>, 12 March 2025"#),
        "{page}"
    );
}

#[tokio::test]
async fn draft_to_publish_end_to_end() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, iid) = three_model_import(&s).await;
    let hook = file_id(&s, "hook.stl").await;
    s.post(
        &format!("/3d/import/{iid}/arrange"),
        &format!("f{hook}=lamp"),
        Some(&cookie),
    )
    .await;
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&cookie)).await;
    s.post(
        &format!("/3d/import/{iid}/details"),
        "category=household&licence=cc-by",
        Some(&cookie),
    )
    .await;
    let r = s
        .post(&format!("/3d/import/{iid}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert_eq!(r.location(), format!("/3d/import/{iid}"));
    let review = s.get(&format!("/3d/import/{iid}"), Some(&cookie)).await.body;
    assert!(
        review.contains("2 models found") && review.contains("published"),
        "{review}"
    );
    let lamp: i64 = sqlx::query_scalar("SELECT id FROM mf_designs WHERE title = 'lamp'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_model_files WHERE design_id = {lamp}"))
            .await,
        2
    );
    let page = s.get(&format!("/d/{lamp}/lamp"), None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("hook.stl") && page.body.contains("Household"));
    s.dump_page("import-done", &review, Some(&cookie)).await;
}

// ---------------------------------------------------------------- print profiles (MF-43)

/// A project 3MF written by `application`, with these settings files in place of the fixture's.
fn project_3mf_with(application: &str, settings: &[(&str, &str)]) -> Vec<u8> {
    let base = project_3mf(None);
    let a = makerfoundry::model3d::zip::Archive::open(&base).unwrap();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for e in a.entries() {
        let mut data = a.read(e).unwrap();
        if e.name.ends_with(".config") {
            continue;
        }
        if e.name == "3D/3dmodel.model" {
            let xml = String::from_utf8(data).unwrap().replacen(
                "<resources>",
                &format!(r#"<metadata name="Application">{application}</metadata><resources>"#),
                1,
            );
            data = xml.into_bytes();
        }
        files.push((e.name.clone(), data));
    }
    files.extend(settings.iter().map(|(n, t)| (n.to_string(), t.as_bytes().to_vec())));
    makerfoundry::model3d::zip::write(&files).unwrap()
}

/// A print profile as kept: name, slicer, badges, layer height, filaments, readable.
type ProfileRow = (String, String, Vec<String>, Option<f64>, serde_json::Value, bool);

#[tokio::test]
async fn a_bambu_project_upload_keeps_its_print_profile() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let settings = r##"{"printer_model": "Bambu Lab A1 mini", "nozzle_diameter": ["0.4"], "layer_height": "0.16",
        "filament_type": ["PLA", "PETG"], "filament_colour": ["#FF0000", "#FFFFFF"], "machine_start_gcode": "G28"}"##;
    let model = r#"<config><object id="2"><metadata key="extruder" value="2"/></object><plate/></config>"#;
    let r = upload_model(
        &s,
        &cookie,
        &[
            ("cube.stl", stl_cube(10.0)),
            (
                "cube.3mf",
                project_3mf_with(
                    "BambuStudio-02.00.00.00",
                    &[
                        ("Metadata/project_settings.config", settings),
                        ("Metadata/model_settings.config", model),
                    ],
                ),
            ),
            (
                "broken.3mf",
                project_3mf_with(
                    "BambuStudio-02.00.00.00",
                    &[("Metadata/project_settings.config", "{ nope")],
                ),
            ),
        ],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let rows: Vec<ProfileRow> = sqlx::query_as(
        "SELECT p.name, p.slicer, p.printers, p.layer_mm, p.filaments, p.readable
         FROM mf_print_profiles p JOIN mf_model_files f ON f.id = p.file_id ORDER BY f.position",
    )
    .fetch_all(&s.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "one for each project, none for the STL");
    assert_eq!(rows[0].0, "0.16 mm layers, PETG, A1 mini");
    assert_eq!(
        (rows[0].1.as_str(), rows[0].2.clone(), rows[0].3),
        ("bambu", vec!["a1-mini".to_string()], Some(0.16))
    );
    assert_eq!(rows[0].4, serde_json::json!([{"type": "PETG", "colour": "#FFFFFF"}]));
    assert!(rows[0].5 && !rows[1].5, "the broken one is kept, marked unreadable");
}

#[tokio::test]
async fn a_broken_settings_file_never_fails_the_upload() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let cookie = s.join("maker%40example.com").await;
    let garbage = String::from_utf8_lossy(&[0u8, 159, 146, 150, 0, 1, 2]).into_owned();
    let prusa = "; printer_model = MK4\n; layer_height = 0.2\n; filament_type = PLA\n";
    let r = upload_model(
        &s,
        &cookie,
        &[
            (
                "broken.3mf",
                project_3mf_with("PrusaSlicer-2.8.1", &[("Metadata/Slic3r_PE.config", &garbage)]),
            ),
            (
                "good.3mf",
                project_3mf_with("PrusaSlicer-2.8.1", &[("Metadata/Slic3r_PE.config", prusa)]),
            ),
            ("cad.3mf", project_3mf_with("Autodesk Fusion", &[])),
        ],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT f.file_name, p.name, p.readable FROM mf_print_profiles p JOIN mf_model_files f ON f.id = p.file_id ORDER BY f.position",
    )
    .fetch_all(&s.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("broken.3mf".to_string(), "Print settings".to_string(), false),
            ("good.3mf".to_string(), "0.2 mm layers, PLA, MK4".to_string(), true),
        ],
        "the CAD program's 3MF has no profile, only geometry"
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_model_files").await, 3);
}

// ---------------------------------------------------------------- print profiles on pages (MF-45)

const BAMBU_SETTINGS: &str = r##"{"printer_model": "Bambu Lab A1 mini", "nozzle_diameter": ["0.4"], "layer_height": "0.2",
    "sparse_infill_density": "15%", "wall_loops": "3", "enable_support": "0",
    "filament_type": ["PLA", "PLA"], "filament_colour": ["#D03030", "#FFFFFF"]}"##;

fn bambu_3mf() -> Vec<u8> {
    project_3mf_with(
        "BambuStudio-02.00.00.00",
        &[
            ("Metadata/project_settings.config", BAMBU_SETTINGS),
            ("Metadata/model_settings.config", "<config><plate/><plate/></config>"),
        ],
    )
}

/// Adds files to a model as its maker → the answer.
async fn add_model_files(s: &Site, cookie: &str, id: i64, files: &[(&str, Vec<u8>)]) -> Answer {
    let parts: Vec<(&str, &str, Vec<u8>)> = files.iter().map(|(n, b)| ("files", *n, b.clone())).collect();
    s.send(multipart(&format!("/3d/{id}/files"), &parts, cookie)).await
}

#[tokio::test]
async fn a_model_shows_each_profile_with_badges_and_colours() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = published_model(&s, "maker%40example.com", "maker").await;
    let r = add_model_files(&s, &cookie, id, &[("a1 mini.3mf", bambu_3mf())]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let page = s.get(&format!("/d/{id}/cube"), None).await.body;
    for text in [
        "Print profiles",
        "0.2 mm layers, PLA, A1 mini",
        r#"<span class="badge printer">A1 mini</span>"#,
        "0.4 mm nozzle",
        "15% infill",
        "3 walls",
        "no supports",
        "2 plates",
        "data:image/svg+xml,",
        "%23D03030",
        "%23FFFFFF",
        "Bambu Studio",
    ] {
        assert!(page.contains(text), "{text}: {page}");
    }
    s.dump_page("model-profiles", &page, None).await;
}

#[tokio::test]
async fn each_profile_downloads_its_own_3mf() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = published_model(&s, "maker%40example.com", "maker").await;
    add_model_files(&s, &cookie, id, &[("a1 mini.3mf", bambu_3mf())]).await;
    let (fid, key): (i64, String) = sqlx::query_as(
        "SELECT f.id, f.file_key FROM mf_print_profiles p JOIN mf_model_files f ON f.id = p.file_id WHERE f.file_name = 'a1 mini.3mf'",
    )
    .fetch_one(&s.pool)
    .await
    .unwrap();
    let page = s.get(&format!("/d/{id}/cube"), None).await.body;
    assert!(
        page.contains(&format!(r#"href="/d/{id}/download?model_file={fid}""#)),
        "{page}"
    );
    let r = s.get(&format!("/d/{id}/download?model_file={fid}"), None).await;
    assert!(
        r.location().contains(&key) && r.location().contains("mini.3mf"),
        "{}",
        r.location()
    );
}

#[tokio::test]
async fn profiles_are_added_and_removed() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = published_model(&s, "maker%40example.com", "maker").await;
    // the ZIP made before a change is made again after it
    s.get(&format!("/d/{id}/download"), None).await;
    let old_zip: String = sqlx::query_scalar("SELECT zip_key FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    add_model_files(&s, &cookie, id, &[("a1 mini.3mf", bambu_3mf())]).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_print_profiles").await, 2);
    assert!(!s.stored.lock().unwrap().contains_key(&old_zip), "the old ZIP is gone");
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_designs WHERE id = {id} AND zip_key IS NULL"
        ))
        .await,
        1
    );
    // another account can't add or remove
    let other = s.join("other%40example.com").await;
    assert_eq!(
        add_model_files(&s, &other, id, &[("x.stl", stl_cube(5.0))])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE file_name = 'a1 mini.3mf'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let remove = format!("/3d/{id}/files/{fid}/delete");
    assert_eq!(s.post(&remove, "", Some(&other)).await.status, StatusCode::NOT_FOUND);
    let keys = makerfoundry::model3d::upload::keys(&s.pool, &[id]).await;
    let r = s.post(&remove, "", Some(&cookie)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_print_profiles").await, 1);
    let after = makerfoundry::model3d::upload::keys(&s.pool, &[id]).await;
    let gone: Vec<_> = keys.iter().filter(|k| !after.contains(k)).collect();
    assert!(
        gone.len() >= 2 && gone.iter().all(|k| !s.stored.lock().unwrap().contains_key(*k)),
        "its file and pictures are deleted"
    );
    // removing the main file moves the model's main file to the next one
    let main: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 ORDER BY position LIMIT 1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/3d/{id}/files/{main}/delete"), "", Some(&cookie))
        .await;
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_designs WHERE id = {id} AND file_name = 'cube project.3mf'"
        ))
        .await,
        1
    );
    // the last model file can't go
    let last: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND role = 'project'")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let r = s
        .post(&format!("/3d/{id}/files/{last}/delete"), "", Some(&cookie))
        .await;
    assert!(r.body.contains("at least one STL, 3MF or OBJ"), "{}", r.body);
    // adding checks files as an upload does
    let r = add_model_files(&s, &cookie, id, &[("page.stl", b"<html></html>".to_vec())]).await;
    assert!(r.body.contains("page.stl"), "{}", r.body);
}

#[tokio::test]
async fn an_unreadable_profile_still_shows_and_downloads() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = published_model(&s, "maker%40example.com", "maker").await;
    add_model_files(
        &s,
        &cookie,
        id,
        &[(
            "odd.3mf",
            project_3mf_with("BambuStudio-02.00", &[("Metadata/project_settings.config", "{ nope")]),
        )],
    )
    .await;
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE file_name = 'odd.3mf'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let page = s.get(&format!("/d/{id}/cube"), None).await.body;
    assert!(
        page.contains("settings couldn") && page.contains(&format!("?model_file={fid}")),
        "{page}"
    );
}

// ---------------------------------------------------------------- remixes (MF-47)

/// A maker with a profile → cookie.
async fn model_maker(s: &Site, email: &str, handle: &str) -> String {
    let cookie = s.join(email).await;
    s.post(
        "/account/maker",
        &format!("name={handle}&handle={handle}"),
        Some(&cookie),
    )
    .await;
    cookie
}

/// A model with its details given (not published) → id.
async fn detailed_model(s: &Site, cookie: &str, title: &str, licence: &str) -> i64 {
    let r = upload_model(s, cookie, &[("part.stl", stl_cube(5.0))], &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = design_id(r.location());
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            &format!("title={title}&category=household&licence={licence}&price=0"),
            Some(cookie),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    id
}

async fn licensed_model(s: &Site, cookie: &str, title: &str, licence: &str) -> i64 {
    let id = detailed_model(s, cookie, title, licence).await;
    let r = s
        .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(cookie))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    id
}

async fn add_parent(s: &Site, cookie: &str, child: i64, parent: &str) -> Answer {
    s.post(
        &format!("/3d/{child}/parents"),
        &format!("parent={}", parent.replace('/', "%2F").replace(':', "%3A")),
        Some(cookie),
    )
    .await
}

#[tokio::test]
async fn a_remix_of_nd_is_refused() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bob = model_maker(&s, "bob%40example.com", "bob").await;
    let vase = licensed_model(&s, &ann, "Vase", "cc-by-nd").await;
    let mine = detailed_model(&s, &bob, "My vase", "cc-by-nd").await;
    let r = add_parent(&s, &bob, mine, &format!("https://makerfoundry.test/d/{vase}/vase")).await;
    assert!(
        r.body.contains("Vase&#39;s licence doesn&#39;t allow remixes"),
        "{}",
        r.body
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_remixes").await, 0);
}

#[tokio::test]
async fn by_sa_forces_by_sa_and_the_link_shows_both_ways() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bob = model_maker(&s, "bob%40example.com", "bob").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by-sa").await;
    let mine = detailed_model(&s, &bob, "Bigger gear", "cc-by").await;
    let r = add_parent(&s, &bob, mine, &gear.to_string()).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let r = s
        .post(&format!("/designs/{mine}/publish"), "confirm=yes", Some(&bob))
        .await;
    assert!(
        r.body.contains("Gear is licensed") && r.body.contains("ShareAlike"),
        "{}",
        r.body
    );
    // changing the licence to one the parent forbids is refused too
    let r = s
        .post(
            &format!("/designs/{mine}/edit"),
            "title=Bigger+gear&category=household&licence=cc0&price=0",
            Some(&bob),
        )
        .await;
    assert!(r.body.contains("Gear is licensed"), "{}", r.body);
    s.post(
        &format!("/designs/{mine}/edit"),
        "title=Bigger+gear&category=household&licence=cc-by-sa&price=0",
        Some(&bob),
    )
    .await;
    let r = s
        .post(&format!("/designs/{mine}/publish"), "confirm=yes", Some(&bob))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let page = s.get(&format!("/d/{mine}/bigger-gear"), None).await.body;
    assert!(
        page.contains("Remix of") && page.contains(&format!(r#"<a href="/d/{gear}/gear">Gear</a>"#)),
        "{page}"
    );
    let parent_page = s.get(&format!("/d/{gear}/gear"), None).await.body;
    assert!(
        parent_page.contains("Remixes") && parent_page.contains("Bigger gear"),
        "{parent_page}"
    );
}

#[tokio::test]
async fn two_parents_with_no_common_licence_are_refused_naming_the_parent() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bob = model_maker(&s, "bob%40example.com", "bob").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by-sa").await;
    let hub = licensed_model(&s, &ann, "Hub", "cc-by-nc-sa").await;
    let mine = detailed_model(&s, &bob, "Wheel", "cc-by-sa").await;
    add_parent(&s, &bob, mine, &gear.to_string()).await;
    let r = add_parent(&s, &bob, mine, &hub.to_string()).await;
    assert!(r.body.contains("Hub") && r.body.contains("no licence"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_remixes").await, 1);
}

#[tokio::test]
async fn a_creator_may_remix_their_own_all_rights_reserved_model() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bob = model_maker(&s, "bob%40example.com", "bob").await;
    let base = licensed_model(&s, &ann, "Base", "all-rights-reserved").await;
    let hers = detailed_model(&s, &ann, "Base two", "cc-by").await;
    assert_eq!(
        add_parent(&s, &ann, hers, &base.to_string()).await.status,
        StatusCode::SEE_OTHER
    );
    let r = s
        .post(&format!("/designs/{hers}/publish"), "confirm=yes", Some(&ann))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let his = detailed_model(&s, &bob, "Copy", "cc-by").await;
    let r = add_parent(&s, &bob, his, &base.to_string()).await;
    assert!(r.body.contains("doesn&#39;t allow remixes"), "{}", r.body);
    // someone else's draft can't be named at all
    let draft = detailed_model(&s, &ann, "Secret", "cc0").await;
    let r = add_parent(&s, &bob, his, &draft.to_string()).await;
    assert!(r.body.contains("No published 3D model"), "{}", r.body);
}

#[tokio::test]
async fn a_cycle_cant_be_made() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let a = detailed_model(&s, &ann, "A", "cc0").await;
    let b = detailed_model(&s, &ann, "B", "cc0").await;
    let c = detailed_model(&s, &ann, "C", "cc0").await;
    assert_eq!(
        add_parent(&s, &ann, b, &a.to_string()).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        add_parent(&s, &ann, c, &b.to_string()).await.status,
        StatusCode::SEE_OTHER
    );
    for (child, parent) in [(a, c), (a, b), (a, a)] {
        let r = add_parent(&s, &ann, child, &parent.to_string()).await;
        assert!(r.body.contains("its own remix"), "{child} ← {parent}: {}", r.body);
    }
    assert_eq!(s.scalar("SELECT count(*) FROM mf_remixes").await, 2);
    // a parent can be taken off again
    let r = s.post(&format!("/3d/{c}/parents/{b}/delete"), "", Some(&ann)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_remixes").await, 1);
}

#[tokio::test]
async fn the_tree_stops_at_ten_generations() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let mut ids = Vec::new();
    for n in 0..12 {
        let r = upload_model(&s, &ann, &[("part.stl", stl_cube(5.0))], &[]).await;
        let id = design_id(r.location());
        sqlx::query("UPDATE mf_designs SET title = $2 WHERE id = $1")
            .bind(id)
            .bind(format!("Model {n:02}"))
            .execute(&s.pool)
            .await
            .unwrap();
        if let Some(parent) = ids.last() {
            assert_eq!(
                add_parent(&s, &ann, id, &format!("{parent}")).await.status,
                StatusCode::SEE_OTHER
            );
        }
        ids.push(id);
    }
    let tree = s.get(&format!("/3d/{}/tree", ids[11]), Some(&ann)).await.body;
    for n in 1..=10 {
        assert!(tree.contains(&format!("Model {n:02} by")), "{n}: {tree}");
    }
    assert!(
        !tree.contains("Model 00<"),
        "eleven generations back isn't shown: {tree}"
    );
    s.dump_page("tree", &tree, Some(&ann)).await;
    let tree = s.get(&format!("/3d/{}/tree", ids[0]), Some(&ann)).await.body;
    assert!(tree.contains("Model 10 by") && !tree.contains("Model 11 by"), "{tree}");
}

#[tokio::test]
async fn a_taken_down_parent_stays_named_without_a_link() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bob = model_maker(&s, "bob%40example.com", "bob").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let mine = detailed_model(&s, &bob, "Wheel", "cc-by").await;
    add_parent(&s, &bob, mine, &gear.to_string()).await;
    s.post(&format!("/designs/{mine}/publish"), "confirm=yes", Some(&bob))
        .await;
    sqlx::query("UPDATE mf_designs SET status = 'removed' WHERE id = $1")
        .bind(gear)
        .execute(&s.pool)
        .await
        .unwrap();
    let page = s.get(&format!("/d/{mine}/wheel"), None).await.body;
    assert!(
        page.contains("Gear") && !page.contains(&format!("/d/{gear}/")),
        "{page}"
    );
    let tree = s.get(&format!("/3d/{mine}/tree"), None).await.body;
    assert!(
        tree.contains("Gear") && !tree.contains(&format!("/d/{gear}/")),
        "{tree}"
    );
}

// ---------------------------------------------------------------- likes and community on 3D (MF-48)

#[tokio::test]
async fn likes_count_once_per_account() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let bea = s.join("bea%40example.com").await;
    let cal = s.join("cal%40example.com").await;
    assert_eq!(
        s.post(&format!("/d/{gear}/like"), "like=yes", None).await.location(),
        format!("/signin?next=/d/{gear}")
    );
    let page = s.get(&format!("/d/{gear}/gear"), Some(&bea)).await.body;
    assert!(
        page.contains(&format!(r#"action="/d/{gear}/like""#)) && page.contains(">Like<"),
        "{page}"
    );
    s.post(&format!("/d/{gear}/like"), "like=yes", Some(&bea)).await;
    s.post(&format!("/d/{gear}/like"), "like=yes", Some(&bea)).await;
    let r = s.post(&format!("/d/{gear}/like"), "like=yes", Some(&cal)).await;
    assert_eq!(r.location(), format!("/d/{gear}/gear"));
    assert_eq!(s.scalar("SELECT count(*) FROM mf_likes").await, 2);
    let page = s.get(&format!("/d/{gear}/gear"), Some(&bea)).await.body;
    assert!(page.contains("2 likes") && page.contains("Liked"), "{page}");
    s.post(&format!("/d/{gear}/like"), "like=no", Some(&bea)).await;
    assert!(s.get(&format!("/d/{gear}/gear"), None).await.body.contains("1 like"));
    // a draft can't be liked
    let draft = detailed_model(&s, &ann, "Draft", "cc0").await;
    assert_eq!(
        s.post(&format!("/d/{draft}/like"), "like=yes", Some(&bea)).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn makes_collections_follows_and_comments_work_on_3d_models() {
    let Some(s) = site_3d(vec![1000], true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    // a make
    let r = s
        .send(multipart(
            &format!("/d/{gear}/makes"),
            &[
                ("photo", "print.jpg", photo_with_location()),
                ("note", "", b"Printed in PETG".to_vec()),
                ("confirm", "", b"yes".to_vec()),
            ],
            &bea,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(
        s.get(&format!("/d/{gear}/gear"), None)
            .await
            .body
            .contains("Printed in PETG")
    );
    // a collection
    s.post(&format!("/d/{gear}/save"), "", Some(&bea)).await;
    let saved: i64 = sqlx::query_scalar("SELECT collection_id FROM mf_collection_items WHERE design_id = $1")
        .bind(gear)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let list = s.get(&format!("/collections/{saved}"), Some(&bea)).await.body;
    assert!(list.contains("Gear") && list.contains("3D model"), "{list}");
    // following its maker shows their new models
    s.post("/makers/ann/follow", "", Some(&bea)).await;
    let wheel = licensed_model(&s, &ann, "Wheel", "cc-by").await;
    let feed = s.get("/following", Some(&bea)).await.body;
    assert!(feed.contains(&format!("/d/{wheel}/wheel")), "{feed}");
    // a comment, reported, reaches the admins' queue
    s.post(&format!("/d/{gear}/comments"), "body=Nice+gear", Some(&bea))
        .await;
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_comments")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let cal = s.join("cal%40example.com").await;
    let r = s
        .post(&format!("/comments/{cid}/report"), "reason=spam", Some(&cal))
        .await;
    assert!(
        r.status == StatusCode::OK || r.status == StatusCode::SEE_OTHER,
        "{}",
        r.body
    );
    let queue = s.get("/admin", Some(&ann)).await.body;
    assert!(queue.contains("Nice gear"), "{queue}");
}

#[tokio::test]
async fn likes_are_in_the_data_download_and_go_with_the_data() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let bea = s.join("bea%40example.com").await;
    s.post(&format!("/d/{gear}/like"), "like=yes", Some(&bea)).await;
    let data = s.get("/account/data.json", Some(&bea)).await.body;
    let v: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(v["likes"][0]["design_id"], gear, "{data}");
    s.post("/account/data/delete", "confirm=DELETE", Some(&bea)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_likes").await, 0);
}

// ---------------------------------------------------------------- 3D search (MF-49)

/// Three published models with different facts, a draft and a taken-down one → (gear, hook, vase).
async fn searchable_models(s: &Site) -> (String, i64, i64, i64) {
    let ann = model_maker(s, "ann%40example.com", "ann").await;
    let publish = |id: i64, title: &'static str, category: &'static str, licence: &'static str| {
        let ann = ann.clone();
        async move {
            let r = s
                .post(
                    &format!("/designs/{id}/edit"),
                    &format!("title={title}&category={category}&licence={licence}&price=0"),
                    Some(&ann),
                )
                .await;
            assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
            let r = s
                .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&ann))
                .await;
            assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        }
    };
    let up = |files: Vec<(&'static str, Vec<u8>)>| {
        let ann = ann.clone();
        async move { design_id(upload_model(s, &ann, &files, &[]).await.location()) }
    };
    let gear = up(vec![("gear.stl", stl_cube(10.0))]).await;
    publish(gear, "Gear", "household", "cc-by").await;
    let hook = up(vec![("hook.3mf", bambu_3mf())]).await;
    publish(hook, "Hook", "tools-workshop", "cc0").await;
    let vase = up(vec![
        ("vase.stl", stl_cube(10.0)),
        ("vase.step", STEP_FILE.as_bytes().to_vec()),
    ])
    .await;
    publish(vase, "Tall vase", "art-decor", "cc-by-sa").await;
    let bea = model_maker(s, "bea%40example.com", "bea").await;
    let r = s
        .send(multipart(
            &format!("/d/{vase}/makes"),
            &[
                ("photo", "print.jpg", photo_with_location()),
                ("confirm", "", b"yes".to_vec()),
            ],
            &bea,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let draft = up(vec![("secret.stl", stl_cube(10.0))]).await;
    sqlx::query("UPDATE mf_designs SET title = 'Secret gear', category = 'household' WHERE id = $1")
        .bind(draft)
        .execute(&s.pool)
        .await
        .unwrap();
    let gone = up(vec![("gone.stl", stl_cube(10.0))]).await;
    publish(gone, "Gone gear", "household", "cc-by").await;
    sqlx::query("UPDATE mf_designs SET status = 'removed' WHERE id = $1")
        .bind(gone)
        .execute(&s.pool)
        .await
        .unwrap();
    (ann, gear, hook, vase)
}

/// The model ids a 3D search finds, in order.
async fn found(s: &Site, query: &str) -> Vec<i64> {
    let page = s.get(&format!("/3d?{query}"), None).await.body;
    page.split(r#"<li><a href="/d/"#)
        .skip(1)
        .filter_map(|t| t.split('/').next()?.parse().ok())
        .collect()
}

#[tokio::test]
async fn each_3d_filter_finds_what_it_should() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, gear, hook, vase) = searchable_models(&s).await;
    for (query, want) in [
        ("category=household", vec![gear]),
        ("licence=cc0", vec![hook]),
        ("format=step", vec![vase]),
        ("format=3mf", vec![hook]),
        ("makes=yes", vec![vase]),
        ("profile=yes", vec![hook]),
        ("printer=a1-mini", vec![hook]),
        ("multicolour=yes", vec![hook]),
        ("filament=PLA", vec![hook]),
        ("filament=PETG", vec![]),
        ("q=vase", vec![vase]),
        ("category=tools-workshop&printer=a1-mini&filament=PLA", vec![hook]),
        ("category=household&printer=a1-mini", vec![]),
        ("category=nope", vec![vase, hook, gear]),
    ] {
        assert_eq!(found(&s, query).await, want, "{query}");
    }
    let page = s.get("/3d", None).await.body;
    s.dump_page("search-3d", &page, None).await;
    for control in [
        r#"name="category""#,
        r#"name="licence""#,
        r#"name="format""#,
        r#"name="printer""#,
        r#"name="filament""#,
        r#"name="makes""#,
        r#"name="profile""#,
        r#"name="multicolour""#,
        r#"name="sort""#,
    ] {
        assert!(page.contains(control), "{control}");
    }
}

#[tokio::test]
async fn drafts_and_taken_down_never_appear() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, gear, ..) = searchable_models(&s).await;
    assert_eq!(found(&s, "q=gear").await, vec![gear]);
    assert_eq!(found(&s, "category=household").await, vec![gear]);
}

#[tokio::test]
async fn sorting_by_newest_and_popular_and_paging_by_key() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, gear, hook, vase) = searchable_models(&s).await;
    assert_eq!(found(&s, "").await, vec![vase, hook, gear], "newest first");
    // the vase has a make; two likes put the gear ahead
    for who in ["cal%40example.com", "dee%40example.com"] {
        let c = s.join(who).await;
        s.post(&format!("/d/{gear}/like"), "like=yes", Some(&c)).await;
    }
    makerfoundry::model3d::search::refresh_popularity(&s.pool)
        .await
        .unwrap();
    assert_eq!(found(&s, "sort=popular").await[0], gear);
    // pages go on from the last model shown
    let first = found(&s, "per=2").await;
    assert_eq!(first, vec![vase, hook]);
    let page = s.get("/3d?per=2", None).await.body;
    assert!(page.contains(&format!("after={hook}")), "{page}");
    assert_eq!(found(&s, &format!("per=2&after={hook}")).await, vec![gear]);
}

#[tokio::test]
async fn a_filtered_page_under_100ms_with_100000_models() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let _ = ann;
    let categories = makerfoundry::model3d::CATEGORIES
        .iter()
        .map(|c| format!("'{}'", c.id))
        .collect::<Vec<_>>()
        .join(",");
    sqlx::query(&format!(
        "INSERT INTO mf_designs (account_id, kind, file_name, file_key, file_bytes, format, facts, preview_svg, status, title,
                                 licence, category, published_at, search, formats, printers, filament_types, has_profile, multicolour, popularity)
         SELECT 1000, 'model3d', 'm.stl', 'model/k' || n, 1, 'stl', '{{}}', '', 'published', 'Model ' || n,
                (ARRAY['cc-by','cc0','cc-by-sa'])[1 + n % 3], (ARRAY[{categories}])[1 + n % 14], now() - n * interval '1 minute',
                to_tsvector('english', 'Model ' || n || CASE WHEN n % 50 = 0 THEN ' dragon' ELSE '' END),
                CASE WHEN n % 2 = 0 THEN ARRAY['stl','3mf'] ELSE ARRAY['stl'] END,
                CASE WHEN n % 4 = 0 THEN ARRAY['x1c'] WHEN n % 4 = 1 THEN ARRAY['a1-mini'] ELSE '{{}}' END,
                CASE WHEN n % 2 = 0 THEN ARRAY['PLA'] ELSE '{{}}' END, n % 2 = 0, n % 6 = 0, n % 997
         FROM generate_series(1, 100000) n"
    ))
    .execute(&s.pool)
    .await
    .unwrap();
    sqlx::query("ANALYZE").execute(&s.pool).await.unwrap();
    for query in [
        "",
        "category=household&printer=x1c",
        "format=3mf&filament=PLA&multicolour=yes&sort=popular",
        "q=dragon&licence=cc0",
        "profile=yes&printer=x1c&sort=popular",
    ] {
        // once to warm the caches, then timed
        assert!(!found(&s, query).await.is_empty(), "{query}");
        let started = std::time::Instant::now();
        let ids = found(&s, query).await;
        let took = started.elapsed();
        println!("3D search {query:?}: {} models in {took:?}", ids.len());
        assert!(took < std::time::Duration::from_millis(100), "{query}: {took:?}");
    }
}

// ---------------------------------------------------------------- creator profiles (MF-50)

const LASER_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="50mm" viewBox="0 0 100 50"><rect x="1" y="1" width="98" height="48" fill="none" stroke="red"/></svg>"#;

/// Ann (handle `ann`) with a published embroidery design, laser design and 3D model → her
/// cookie and the three ids.
async fn creator_with_three_kinds(s: &Site) -> (String, i64, i64, i64) {
    let ann = model_maker(s, "ann%40example.com", "ann").await;
    let rose = published(s, &ann, "ann", "Rose").await;
    let r = s
        .send(multipart(
            "/designs/new",
            &[("file", "coaster.svg", LASER_SVG.as_bytes().to_vec())],
            &ann,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let coaster = design_id(r.location());
    s.post(
        &format!("/designs/{coaster}/edit"),
        "title=Coaster&licence=commercial&price=0",
        Some(&ann),
    )
    .await;
    let r = s
        .post(&format!("/designs/{coaster}/publish"), "confirm=yes", Some(&ann))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let gear = licensed_model(s, &ann, "Gear", "cc-by").await;
    (ann, rose, coaster, gear)
}

#[tokio::test]
async fn a_creator_with_all_three_kinds() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, rose, coaster, gear) = creator_with_three_kinds(&s).await;
    let page = s.get("/makers/ann", None).await.body;
    for text in [
        "Embroidery designs",
        "Laser designs",
        "3D models",
        &format!("/d/{rose}/"),
        &format!("/d/{coaster}/"),
        &format!("/d/{gear}/"),
    ] {
        assert!(page.contains(text), "{text}: {page}");
    }
    let (emb, laser, models) = (
        page.find("Embroidery designs").unwrap(),
        page.find("Laser designs").unwrap(),
        page.find("3D models</h2>").unwrap(),
    );
    assert!(page[models..].contains(&format!("/d/{gear}/")) && !page[emb..laser].contains(&format!("/d/{gear}/")));
    s.dump_page("creator", &page, None).await;
    // while MakerFoundry 3D is off, others don't see the models section (Ann, an admin, made it)
    let Some(s) = site_3d(vec![1000], false).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    published(&s, &ann, "ann", "Rose").await;
    let model = detailed_model(&s, &ann, "Hidden", "cc0").await;
    sqlx::query("UPDATE mf_designs SET status = 'published', published_at = now() WHERE id = $1")
        .bind(model)
        .execute(&s.pool)
        .await
        .unwrap();
    let page = s.get("/makers/ann", None).await.body;
    assert!(!page.contains("3D models") && !page.contains("Hidden"), "{page}");
}

#[tokio::test]
async fn counts_are_right() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, _, _, gear) = creator_with_three_kinds(&s).await;
    licensed_model(&s, &creator_cookie(&s).await, "Wheel", "cc0").await;
    for who in ["bea%40example.com", "cal%40example.com"] {
        let c = s.join(who).await;
        s.post(&format!("/d/{gear}/like"), "like=yes", Some(&c)).await;
        s.get(&format!("/d/{gear}/download"), Some(&c)).await;
    }
    let page = s.get("/makers/ann", None).await.body;
    assert!(page.contains("2 designs · 2 models · 2 likes · 2 downloads"), "{page}");
}

/// Ann's cookie again (signing in).
async fn creator_cookie(s: &Site) -> String {
    s.post("/signin", "email=ann%40example.com&password=fire+truck+1", None)
        .await
        .cookie()
}

#[tokio::test]
async fn drafts_never_shown_on_a_profile() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (ann, ..) = creator_with_three_kinds(&s).await;
    detailed_model(&s, &ann, "Unfinished", "cc0").await;
    let page = s.get("/makers/ann", None).await.body;
    assert!(!page.contains("Unfinished") && page.contains("1 model"), "{page}");
}

#[tokio::test]
async fn the_founding_badge_shows_only_when_set() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    creator_with_three_kinds(&s).await;
    model_maker(&s, "bea%40example.com", "bea").await;
    assert!(!s.get("/makers/ann", None).await.body.contains("Founding creator"));
    sqlx::query("INSERT INTO mf_founding (account_id, name, status, number) VALUES (1000, 'Ann', 'approved', 7)")
        .execute(&s.pool)
        .await
        .unwrap();
    assert!(s.get("/makers/ann", None).await.body.contains("Founding creator #7"));
    assert!(!s.get("/makers/bea", None).await.body.contains("Founding creator"));
}

// ---------------------------------------------------------------- moderation for 3D (MF-51)

/// The first report waiting in the admins' queue → its id.
async fn first_report(s: &Site, admin: &str) -> String {
    let queue = s.get("/admin", Some(admin)).await.body;
    queue
        .split("/admin/reports/")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn report_takedown_counter_notice_restore_for_a_3d_model() {
    let Some(s) = site_3d(vec![1000], true).await else {
        return;
    };
    let admin = model_maker(&s, "admin%40example.com", "admin").await;
    let maker = model_maker(&s, "maker%40example.com", "maker").await;
    let (_, id) = (admin.clone(), licensed_model(&s, &maker, "Famous mouse", "cc-by").await);
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    // reported from its page
    let page = s.get(&format!("/d/{id}/famous-mouse"), None).await.body;
    assert!(page.contains(&format!("/d/{id}/report")), "{page}");
    assert_eq!(s.get(&format!("/d/{id}/report"), None).await.status, StatusCode::OK);
    let r = s
        .post(
            &format!("/d/{id}/report"),
            "reason=trademark&details=A+famous+cartoon+mouse.",
            None,
        )
        .await;
    assert!(r.body.contains("Your report is in"), "{}", r.body);
    let rid = first_report(&s, &admin).await;
    let r = s
        .post(&format!("/admin/reports/{rid}/takedown"), "", Some(&admin))
        .await;
    assert_eq!(r.location(), "/admin");
    // down: its page, files, pictures, search and the maker's page
    for path in [
        format!("/d/{id}/famous-mouse"),
        format!("/d/{id}/download"),
        format!("/d/{id}/download?model_file={fid}"),
        format!("/3d/{id}/files/{fid}/hero"),
    ] {
        assert_eq!(s.get(&path, None).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert!(!s.get("/3d?q=mouse", None).await.body.contains("Famous mouse"));
    assert!(!s.get("/makers/maker", None).await.body.contains("Famous mouse"));
    // the maker still sees it, and sends a counter-notice
    assert_eq!(
        s.get(&format!("/3d/{id}/files/{fid}/hero"), Some(&maker)).await.status,
        StatusCode::OK
    );
    let counter = "name=Maker+Person&address=2+Elm+St&phone=555-0101&email=maker%40example.com&mistake=yes&consent=yes&signature=Maker+Person";
    let r = s.post(&format!("/designs/{id}/counter"), counter, Some(&maker)).await;
    assert!(r.body.contains("Counter-notice received"), "{}", r.body);
    let rid = first_report(&s, &admin).await;
    s.post(&format!("/admin/reports/{rid}/restore"), "", Some(&admin)).await;
    assert_eq!(
        s.get(&format!("/d/{id}/famous-mouse"), None).await.status,
        StatusCode::OK
    );
    assert_eq!(
        s.get(&format!("/3d/{id}/files/{fid}/hero"), None).await.status,
        StatusCode::OK
    );
    assert_eq!(
        s.get(&format!("/d/{id}/download"), None).await.status,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn repeat_infringers_are_counted_across_kinds() {
    let Some(s) = site_3d(vec![1000], true).await else {
        return;
    };
    let admin = model_maker(&s, "admin%40example.com", "admin").await;
    let maker = model_maker(&s, "maker%40example.com", "maker").await;
    // an embroidery design taken down first
    let rose = published(&s, &maker, "maker", "Copied+rose").await;
    s.post(&format!("/d/{rose}/report"), "reason=trademark&details=Copied.", None)
        .await;
    let rid = first_report(&s, &admin).await;
    s.post(&format!("/admin/reports/{rid}/takedown"), "", Some(&admin))
        .await;
    // then a 3D model of theirs is reported: the queue says they've had one before
    let model = licensed_model(&s, &maker, "Copied gear", "cc-by").await;
    s.post(
        &format!("/d/{model}/report"),
        "reason=trademark&details=Copied+too.",
        None,
    )
    .await;
    let queue = s.get("/admin", Some(&admin)).await.body;
    assert!(
        queue.contains("Copied gear") && queue.contains("1 takedown before"),
        "{queue}"
    );
}

#[tokio::test]
async fn imports_and_likes_are_rate_limited() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    for _ in 0..makerfoundry::limits::IMPORT.max {
        let r = import_files(&s, &ann, &[("a.stl", stl_cube(2.0))]).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    }
    let r = import_files(&s, &ann, &[("a.stl", stl_cube(2.0))]).await;
    assert!(r.body.contains("imported a lot today"), "{}", r.body);
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let bea = s.join("bea%40example.com").await;
    for n in 0..makerfoundry::limits::LIKE.max {
        s.post(
            &format!("/d/{gear}/like"),
            if n % 2 == 0 { "like=yes" } else { "like=no" },
            Some(&bea),
        )
        .await;
    }
    // over the limit the like isn't counted
    s.post(&format!("/d/{gear}/like"), "like=yes", Some(&bea)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_likes").await, 0);
}

// ---------------------------------------------------------------- founding creators' early access (MF-54, 3D)

#[tokio::test]
async fn a_founding_creator_can_publish_3d_before_the_switch_and_others_cant() {
    let Some(s) = site_3d(Vec::new(), false).await else {
        return;
    };
    let nick = model_maker(&s, "nick%40example.com", "nick").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    assert_eq!(
        s.get("/3d/new", Some(&nick)).await.status,
        StatusCode::NOT_FOUND,
        "not yet a founding creator"
    );
    sqlx::query("INSERT INTO mf_founding (account_id, name, status, number) VALUES (1000, 'Nick', 'approved', 1)")
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(s.get("/3d", Some(&nick)).await.status, StatusCode::OK);
    assert_eq!(s.get("/3d/new", Some(&nick)).await.status, StatusCode::OK);
    assert_eq!(s.get("/3d/import", Some(&nick)).await.status, StatusCode::OK);
    let gear = licensed_model(&s, &nick, "Gear", "cc-by").await;
    let page = s.get(&format!("/d/{gear}/gear"), Some(&nick)).await.body;
    assert!(
        page.contains("Founding creator #1"),
        "the badge shows on 3D models too: {page}"
    );
    // everyone else waits for the switch
    for who in [Some(bea.as_str()), None] {
        assert_eq!(s.get("/3d", who).await.status, StatusCode::NOT_FOUND);
        assert_eq!(
            s.get(&format!("/d/{gear}/gear"), who).await.status,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(s.get("/3d/new", Some(&bea)).await.status, StatusCode::NOT_FOUND);
    // removing the badge removes the early access
    sqlx::query("UPDATE mf_founding SET status = 'declined', number = NULL WHERE account_id = 1000")
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(s.get("/3d/new", Some(&nick)).await.status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- showcase consent (MF-55)

const SHOWCASE_BOX: &str = r#"<input type="checkbox" name="showcase" value="yes">"#;

async fn upload_with_showcase(s: &Site, cookie: &str, ticked: bool) -> i64 {
    let mut parts = vec![("files", "cube.stl", stl_cube(5.0))];
    if ticked {
        parts.push(("showcase", "", b"yes".to_vec()));
    }
    let r = s.send(multipart("/3d/new", &parts, cookie)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    design_id(r.location())
}

#[tokio::test]
async fn consent_is_off_by_default_and_nothing_stored() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let form = s.get("/3d/new", Some(&ann)).await.body;
    s.dump_page("upload-3d", &form, Some(&ann)).await;
    assert!(
        form.contains(SHOWCASE_BOX) && form.contains("Feature my project on MakerFoundry"),
        "{form}"
    );
    upload_with_showcase(&s, &ann, false).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_showcase_consents").await, 0);
    // the make form for a model and the import's publishing step carry the box too
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    assert!(
        s.get(&format!("/d/{gear}/gear"), Some(&bea))
            .await
            .body
            .contains(SHOWCASE_BOX)
    );
    let r = import_files(&s, &ann, &[("vase.stl", stl_cube(5.0))]).await;
    let iid = import_id(r.location());
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&ann)).await;
    assert!(
        s.get(&format!("/3d/import/{iid}"), Some(&ann))
            .await
            .body
            .contains(SHOWCASE_BOX)
    );
}

#[tokio::test]
async fn ticked_it_is_stored_with_its_time() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let id = upload_with_showcase(&s, &ann, true).await;
    let given: Vec<(i64, i64, bool)> = sqlx::query_as(
        "SELECT account_id, design_id, given_at > now() - interval '1 minute' FROM mf_showcase_consents WHERE withdrawn_at IS NULL",
    )
    .fetch_all(&s.pool)
    .await
    .unwrap();
    assert_eq!(given, [(1000, id, true)]);
    // a make, ticked
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    let r = s
        .send(multipart(
            &format!("/d/{gear}/makes"),
            &[
                ("photo", "print.jpg", photo_with_location()),
                ("confirm", "", b"yes".to_vec()),
                ("showcase", "", b"yes".to_vec()),
            ],
            &bea,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_showcase_consents WHERE make_id IS NOT NULL AND account_id = 1001")
            .await,
        1
    );
    // the import's publishing step, ticked: one for each model published
    let r = import_files(&s, &ann, &[("vase.stl", stl_cube(5.0)), ("lamp.stl", stl_cube(5.0))]).await;
    let iid = import_id(r.location());
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&ann)).await;
    s.post(
        &format!("/3d/import/{iid}/details"),
        "category=household&licence=cc-by",
        Some(&ann),
    )
    .await;
    s.post(
        &format!("/3d/import/{iid}/publish"),
        "confirm=yes&showcase=yes",
        Some(&ann),
    )
    .await;
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_showcase_consents c JOIN mf_designs d ON d.id = c.design_id WHERE d.import_id = {iid}")).await,
        2
    );
}

#[tokio::test]
async fn withdrawn_keeps_its_time_and_is_no_longer_active() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    upload_with_showcase(&s, &ann, true).await;
    let page = s.get("/account/showcase", Some(&ann)).await.body;
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_showcase_consents")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert!(
        page.contains(&format!(r#"action="/account/showcase/{cid}/withdraw""#)),
        "{page}"
    );
    let r = s
        .post(&format!("/account/showcase/{cid}/withdraw"), "", Some(&ann))
        .await;
    assert_eq!(r.location(), "/account/showcase");
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_showcase_consents WHERE given_at IS NOT NULL AND withdrawn_at IS NOT NULL")
            .await,
        1
    );
    let settings = s.get("/account/showcase", Some(&ann)).await.body;
    assert!(settings.contains("Withdrawn"));
    s.dump_page("account-showcase", &settings, Some(&ann)).await;
}

#[tokio::test]
async fn nobody_else_can_withdraw_it() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    upload_with_showcase(&s, &ann, true).await;
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_showcase_consents")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let bea = s.join("bea%40example.com").await;
    assert_eq!(
        s.post(&format!("/account/showcase/{cid}/withdraw"), "", Some(&bea))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert!(!s.get("/account/showcase", Some(&bea)).await.body.contains("/withdraw"));
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_showcase_consents WHERE withdrawn_at IS NULL")
            .await,
        1
    );
}

#[tokio::test]
async fn deleting_data_withdraws_everything_and_consents_are_in_the_download() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let id = upload_with_showcase(&s, &ann, true).await;
    let data: serde_json::Value = serde_json::from_str(&s.get("/account/data.json", Some(&ann)).await.body).unwrap();
    assert_eq!(data["showcase_consents"][0]["design_id"], id);
    s.post("/account/data/delete", "confirm=DELETE", Some(&ann)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_showcase_consents WHERE withdrawn_at IS NULL")
            .await,
        0
    );
}

// ---------------------------------------------------------------- the admins' showcase queue (MF-56)

/// An admin (1000) and Bea (1001, "Bea's Prints") with: a consented published model, a consented
/// draft, a published model without consent → (admin, bea, published id, draft id, other id).
async fn showcase_site(s: &Site) -> (String, String, i64, i64, i64) {
    let admin = model_maker(s, "admin%40example.com", "admin").await;
    let bea = s.join("bea%40example.com").await;
    s.post("/account/maker", "name=Bea%27s+Prints&handle=bea", Some(&bea))
        .await;
    let shared = upload_with_showcase(s, &bea, true).await;
    s.post(
        &format!("/designs/{shared}/edit"),
        "title=Dragon&category=toys-games&licence=cc-by&price=0",
        Some(&bea),
    )
    .await;
    s.post(&format!("/designs/{shared}/publish"), "confirm=yes", Some(&bea))
        .await;
    let draft = upload_with_showcase(s, &bea, true).await;
    let other = licensed_model(s, &bea, "Private", "cc-by").await;
    (admin, bea, shared, draft, other)
}

#[tokio::test]
async fn only_opted_in_published_items_are_listed() {
    let Some(s) = site_3d(vec![1000], true).await else {
        return;
    };
    let (admin, bea, shared, draft, other) = showcase_site(&s).await;
    assert_eq!(s.get("/admin/showcase", Some(&bea)).await.status, StatusCode::NOT_FOUND);
    let queue = s.get("/admin/showcase", Some(&admin)).await.body;
    assert!(queue.contains(&format!("/d/{shared}/dragon")), "{queue}");
    assert!(
        !queue.contains(&format!("/d/{draft}/")) && !queue.contains(&format!("/d/{other}/")),
        "{queue}"
    );
    assert!(!queue.contains("@example.com"), "no email ever shows: {queue}");
    s.dump_page("showcase", &queue, Some(&admin)).await;
}

#[tokio::test]
async fn the_caption_credits_the_display_name_and_links_the_model() {
    let Some(s) = site_3d(vec![1000], true).await else {
        return;
    };
    let (admin, _, shared, ..) = showcase_site(&s).await;
    let queue = s.get("/admin/showcase", Some(&admin)).await.body;
    assert!(
        queue.contains(&format!("Dragon by Bea&#39;s Prints on MakerFoundry (Creative Commons Attribution (CC BY 4.0)): https://makerfoundry.test/d/{shared}/dragon")),
        "{queue}"
    );
}

#[tokio::test]
async fn withdrawal_removes_from_the_queue_and_flags_posted_items_until_taken_down() {
    let Some(s) = site_3d(vec![1000], true).await else {
        return;
    };
    let (admin, bea, shared, ..) = showcase_site(&s).await;
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_showcase_consents WHERE design_id = $1")
        .bind(shared)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    // the admin posts it by hand and notes where
    let r = s
        .post(
            &format!("/admin/showcase/{cid}/posted"),
            "platform=instagram&url=https%3A%2F%2Fwww.instagram.com%2Fp%2Fabc",
            Some(&admin),
        )
        .await;
    assert_eq!(r.location(), "/admin/showcase");
    let r = s
        .post(
            &format!("/admin/showcase/{cid}/posted"),
            "platform=myspace&url=https%3A%2F%2Fx",
            Some(&admin),
        )
        .await;
    assert!(r.body.contains("Choose where it was posted"), "{}", r.body);
    assert_eq!(
        s.post(
            &format!("/admin/showcase/{cid}/posted"),
            "platform=facebook&url=https%3A%2F%2Fx",
            Some(&bea)
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // Bea withdraws: off the queue, and the post is flagged
    s.post(&format!("/account/showcase/{cid}/withdraw"), "", Some(&bea))
        .await;
    let queue = s.get("/admin/showcase", Some(&admin)).await.body;
    assert!(!queue.contains(&format!("/admin/showcase/{cid}/posted")), "{queue}");
    assert!(
        queue.contains("Take these down") && queue.contains("https://www.instagram.com/p/abc"),
        "{queue}"
    );
    let pid: i64 = sqlx::query_scalar("SELECT id FROM mf_showcase_posts")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/admin/showcase/posts/{pid}/taken-down"), "", Some(&admin))
        .await;
    let queue = s.get("/admin/showcase", Some(&admin)).await.body;
    assert!(!queue.contains("Take these down"), "{queue}");
    // a model taken down after a report is off the queue too
    let cid2: i64 = {
        let r = upload_with_showcase(&s, &bea, true).await;
        s.post(
            &format!("/designs/{r}/edit"),
            "title=Robot&category=toys-games&licence=cc0&price=0",
            Some(&bea),
        )
        .await;
        s.post(&format!("/designs/{r}/publish"), "confirm=yes", Some(&bea))
            .await;
        sqlx::query("UPDATE mf_designs SET status = 'removed' WHERE id = $1")
            .bind(r)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query_scalar("SELECT id FROM mf_showcase_consents WHERE design_id = $1")
            .bind(r)
            .fetch_one(&s.pool)
            .await
            .unwrap()
    };
    assert!(
        !s.get("/admin/showcase", Some(&admin))
            .await
            .body
            .contains(&format!("/admin/showcase/{cid2}/posted"))
    );
}

// ---------------------------------------------------------------- Also by this maker (MF-57)

#[tokio::test]
async fn also_by_this_maker_lists_only_published_designs_of_other_kinds() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (ann, rose, coaster, gear) = creator_with_three_kinds(&s).await;
    // a draft embroidery design never shows
    let draft = add_design(&s, &ann, sf_formats::Format::Pes, "tulip.pes").await;
    let page = s.get(&format!("/d/{gear}/gear"), None).await.body;
    assert!(page.contains("Also by this maker"), "{page}");
    assert!(
        page.contains(&format!("/d/{rose}/")) && page.contains(&format!("/d/{coaster}/")),
        "{page}"
    );
    assert!(!page.contains(&format!("/d/{draft}/")), "{page}");
    assert!(page.contains(r#"href="/makers/ann""#), "{page}");
    assert!(
        !page.contains("Get this printed"),
        "no print orders yet (the owner's decision)"
    );
}

#[tokio::test]
async fn none_when_the_maker_has_no_other_kinds() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    licensed_model(&s, &ann, "Wheel", "cc-by").await;
    let page = s.get(&format!("/d/{gear}/gear"), None).await.body;
    assert!(!page.contains("Also by this maker"), "{page}");
}

// ---------------------------------------------------------------- the 3D viewer (MF-46)

#[tokio::test]
async fn the_page_works_with_scripts_off() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, id) = published_model(&s, "maker%40example.com", "maker").await;
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 ORDER BY position LIMIT 1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let page = s.get(&format!("/d/{id}/cube"), None).await.body;
    // the server-drawn picture and the downloads are there; the viewer waits, hidden, for its script
    assert!(
        page.contains(&format!(r#"src="/3d/{id}/files/{fid}/hero""#)) && page.contains("Download all (ZIP)"),
        "{page}"
    );
    assert!(
        page.contains(&format!(
            r#"<div class="viewer" hidden data-mesh="/3d/{id}/files/{fid}/mesh">"#
        )),
        "{page}"
    );
    assert!(
        page.contains(r#"<script src="/static/viewer.js" defer></script>"#),
        "{page}"
    );
    s.dump_page("viewer", &page, None).await;
    let js = s.get("/static/viewer.js", None).await;
    assert_eq!(js.status, StatusCode::OK);
    assert!(
        js.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
}

#[tokio::test]
async fn the_mesh_endpoint_serves_only_published_or_own() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (cookie, id) = model_draft(&s, "maker%40example.com", "maker").await;
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND role = 'mesh'")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let path = format!("/3d/{id}/files/{fid}/mesh");
    assert_eq!(s.get(&path, None).await.status, StatusCode::NOT_FOUND);
    let other = s.join("other%40example.com").await;
    assert_eq!(s.get(&path, Some(&other)).await.status, StatusCode::NOT_FOUND);
    let own = s.get(&path, Some(&cookie)).await;
    assert_eq!(own.status, StatusCode::OK);
    assert_eq!(own.headers[header::CONTENT_TYPE], "application/octet-stream");
    assert!(own.body.starts_with("MFM1"));
    s.post(
        &format!("/designs/{id}/edit"),
        "title=Cube&category=household&licence=cc0&price=0",
        Some(&cookie),
    )
    .await;
    s.post(&format!("/designs/{id}/publish"), "confirm=yes", Some(&cookie))
        .await;
    assert_eq!(s.get(&path, None).await.status, StatusCode::OK);
    // a STEP file has no mesh
    let step: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND role = 'source'")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        s.get(&format!("/3d/{id}/files/{step}/mesh"), None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn csp_on_other_pages_is_unchanged() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (_, id) = published_model(&s, "maker%40example.com", "maker").await;
    let csp = |a: &Answer| a.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().to_string();
    let model = csp(&s.get(&format!("/d/{id}/cube"), None).await);
    assert!(
        model.contains("script-src 'self'") && model.contains("connect-src 'self'"),
        "{model}"
    );
    assert!(!model.contains("unsafe"), "{model}");
    for path in ["/", "/3d", "/browse", "/terms"] {
        let other = csp(&s.get(path, None).await);
        assert!(
            !other.contains("script-src") && !other.contains("connect-src"),
            "{path}: {other}"
        );
    }
    let maker = s.join("ann%40example.com").await;
    let rose = published(&s, &maker, "ann", "Rose").await;
    assert!(!csp(&s.get(&format!("/d/{rose}/rose"), None).await).contains("script-src"));
}

// ---------------------------------------------------------------- virus scanning (issue #29)

/// What the stand-in scanner flags (the real EICAR test string isn't kept in the repository).
const TEST_VIRUS: &[u8] = b"MF-TEST-VIRUS-SIGNATURE";

/// A site with MakerFoundry 3D on and the stand-in scanner → the site and its "down" switch.
async fn scanned_site() -> Option<(Site, std::sync::Arc<std::sync::atomic::AtomicBool>)> {
    let (address, down) = makerfoundry::scan::stand_in::start(TEST_VIRUS).await.unwrap();
    let scanner = makerfoundry::scan::Scanner::new(&address);
    Some((
        site_scanned(Vec::new(), None, true, None, true, scanner, None).await?,
        down,
    ))
}

fn step_with(extra: &[u8]) -> Vec<u8> {
    let mut step = STEP_FILE.replace("ENDSEC;\nDATA;", "ENDSEC;\nDATA;\n/* ").into_bytes();
    let at = step.windows(3).position(|w| w == b"/* ").unwrap() + 3;
    let mut tail = extra.to_vec();
    tail.extend(b" */");
    step.splice(at..at, tail);
    step
}

#[tokio::test]
async fn the_eicar_test_file_is_held() {
    let Some((s, _)) = scanned_site().await else { return };
    let ann = s.join("ann%40example.com").await;
    let r = upload_model(
        &s,
        &ann,
        &[("cube.stl", stl_cube(5.0)), ("part.step", step_with(TEST_VIRUS))],
        &[],
    )
    .await;
    assert!(
        r.body.contains("part.step") && r.body.contains("virus scanner"),
        "{}",
        r.body
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 0);
    assert!(s.stored.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_clean_step_passes() {
    let Some((s, _)) = scanned_site().await else { return };
    let ann = s.join("ann%40example.com").await;
    let r = upload_model(
        &s,
        &ann,
        &[("cube.stl", stl_cube(5.0)), ("part.step", step_with(b"clean"))],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
}

#[tokio::test]
async fn the_scanner_being_down_holds_uploads() {
    let Some((s, down)) = scanned_site().await else { return };
    let ann = s.join("ann%40example.com").await;
    down.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = upload_model(&s, &ann, &[("cube.stl", stl_cube(5.0))], &[]).await;
    assert!(r.body.contains("can&#39;t be checked for viruses"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_designs").await, 0);
    // adding files to a model is held the same way
    down.store(false, std::sync::atomic::Ordering::SeqCst);
    let id = design_id(
        upload_model(&s, &ann, &[("cube.stl", stl_cube(5.0))], &[])
            .await
            .location(),
    );
    let r = add_model_files(&s, &ann, id, &[("more.step", step_with(TEST_VIRUS))]).await;
    assert!(r.body.contains("virus scanner"), "{}", r.body);
}

#[tokio::test]
async fn off_without_the_setting_and_nothing_changes() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = s.join("ann%40example.com").await;
    let r = upload_model(
        &s,
        &ann,
        &[("cube.stl", stl_cube(5.0)), ("part.step", step_with(TEST_VIRUS))],
        &[],
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::SEE_OTHER,
        "no scanner set: not scanned, {}",
        r.body
    );
}

// ---------------------------------------------------------------- Thingiverse import (issue #28)

/// A stand-in for Thingiverse's sign-in and API: Ann ("ann_makes", code "code-ann", token
/// "tok-ann") made things 1 and 2; her list also names thing 3, which Bob made.
async fn fake_thingiverse() -> String {
    use axum::extract::Path as P;
    use axum::routing::{get, post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let b = base.clone();
    let authorised =
        |h: &HeaderMap| h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some("Bearer tok-ann");
    let app = Router::new()
        .route(
            "/login/oauth/access_token",
            post(|body: String| async move {
                if body.contains("code=code-ann") && body.contains("client_secret=sec") {
                    (StatusCode::OK, "access_token=tok-ann&token_type=Bearer".to_string())
                } else {
                    (StatusCode::BAD_REQUEST, "error=bad_code".to_string())
                }
            }),
        )
        .route(
            "/users/me",
            get(move |h: HeaderMap| async move {
                if authorised(&h) { (StatusCode::OK, json!({"name": "ann_makes"}).to_string()) } else { (StatusCode::UNAUTHORIZED, "{}".into()) }
            }),
        )
        .route(
            "/users/{name}/things",
            get(move |h: HeaderMap, P(name): P<String>| async move {
                if authorised(&h) && name == "ann_makes" {
                    (StatusCode::OK, json!([{"id": 1}, {"id": 2}, {"id": 3}]).to_string())
                } else {
                    (StatusCode::UNAUTHORIZED, "[]".into())
                }
            }),
        )
        .route(
            "/things/{id}",
            get(move |h: HeaderMap, P(id): P<i64>| async move {
                if !authorised(&h) {
                    return (StatusCode::UNAUTHORIZED, "{}".to_string());
                }
                let thing = match id {
                    1 => json!({"id": 1, "name": "Cable clip", "license": "Creative Commons - Attribution",
                                "added": "2021-04-05T10:00:00+00:00", "public_url": "https://www.thingiverse.com/thing:1",
                                "creator": {"name": "ann_makes"}}),
                    2 => json!({"id": 2, "name": "Plant pot", "license": "GNU - GPL", "added": "2022-01-02T00:00:00+00:00",
                                "public_url": "https://www.thingiverse.com/thing:2", "creator": {"name": "ann_makes"}}),
                    _ => json!({"id": 3, "name": "Bobs boat", "license": "Creative Commons - Attribution",
                                "public_url": "https://www.thingiverse.com/thing:3", "creator": {"name": "bob"}}),
                };
                (StatusCode::OK, thing.to_string())
            }),
        )
        .route(
            "/things/{id}/files",
            get(move |h: HeaderMap, P(id): P<i64>| {
                let b = b.clone();
                async move {
                    if !authorised(&h) {
                        return (StatusCode::UNAUTHORIZED, "[]".to_string());
                    }
                    (StatusCode::OK, json!([{"id": id * 10, "name": format!("part{id}.stl"), "download_url": format!("{b}/download/{id}")}]).to_string())
                }
            }),
        )
        .route(
            "/download/{id}",
            get(move |h: HeaderMap| async move {
                if authorised(&h) { (StatusCode::OK, stl_cube(8.0)) } else { (StatusCode::UNAUTHORIZED, Vec::new()) }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

async fn thingiverse_site() -> Option<Site> {
    let base = fake_thingiverse().await;
    let tv = makerfoundry::model3d::thingiverse::Thingiverse::new("app-id", "sec", Some(&base), Some(&base));
    site_scanned(Vec::new(), None, true, None, true, None, tv).await
}

/// Signs in at the stand-in: the state from our redirect, sent back with Ann's code → the answer.
async fn link_thingiverse(s: &Site, cookie: &str, code: &str) -> Answer {
    let go = s.get("/3d/import/thingiverse", Some(cookie)).await;
    assert_eq!(go.status, StatusCode::SEE_OTHER, "{}", go.body);
    let to = url::Url::parse(go.location()).unwrap();
    assert!(to.path().ends_with("/login/oauth/authorize"));
    let q: std::collections::HashMap<_, _> = to.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], "app-id");
    assert_eq!(
        q["redirect_uri"],
        "https://makerfoundry.test/3d/import/thingiverse/back"
    );
    s.get(
        &format!("/3d/import/thingiverse/back?code={code}&state={}", q["state"]),
        Some(cookie),
    )
    .await
}

#[tokio::test]
async fn off_without_the_app_settings() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = s.join("ann%40example.com").await;
    assert_eq!(
        s.get("/3d/import/thingiverse", Some(&ann)).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(
        !s.get("/3d/import", Some(&ann))
            .await
            .body
            .contains("/3d/import/thingiverse")
    );
}

#[tokio::test]
async fn only_the_signed_in_accounts_own_things_are_offered() {
    let Some(s) = thingiverse_site().await else { return };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    assert!(
        s.get("/3d/import", Some(&ann))
            .await
            .body
            .contains("/3d/import/thingiverse")
    );
    let r = link_thingiverse(&s, &ann, "code-ann").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let iid = import_id(r.location());
    let review = s.get(&format!("/3d/import/{iid}"), Some(&ann)).await.body;
    assert!(
        review.contains("Cable clip") && review.contains("Plant pot") && review.contains("2 models"),
        "{review}"
    );
    assert!(
        !review.contains("Bobs boat"),
        "a thing someone else made isn't brought in: {review}"
    );
}

#[tokio::test]
async fn a_sign_in_cant_be_used_by_another_account_or_twice() {
    let Some(s) = thingiverse_site().await else { return };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bob = model_maker(&s, "bob%40example.com", "bob").await;
    let go = s.get("/3d/import/thingiverse", Some(&ann)).await;
    let state: String = url::Url::parse(go.location())
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let back = format!("/3d/import/thingiverse/back?code=code-ann&state={state}");
    assert!(
        s.get(&back, Some(&bob))
            .await
            .body
            .contains("didn&#39;t come from here")
    );
    assert_eq!(s.get(&back, Some(&ann)).await.status, StatusCode::SEE_OTHER);
    assert!(
        s.get(&back, Some(&ann))
            .await
            .body
            .contains("didn&#39;t come from here"),
        "used once"
    );
    // a wrong code brings nothing in
    let r = link_thingiverse(&s, &ann, "code-bob").await;
    assert!(r.body.contains("didn&#39;t work"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_imports").await, 1);
}

/// A draft's title, licence and where it was first published (site, address, date).
type OriginRow = (String, Option<String>, Option<String>, Option<String>, Option<String>);

#[tokio::test]
async fn licence_and_date_are_kept_and_the_origin_link_added() {
    let Some(s) = thingiverse_site().await else { return };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let iid = import_id(link_thingiverse(&s, &ann, "code-ann").await.location());
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&ann)).await;
    let rows: Vec<OriginRow> = sqlx::query_as(
        "SELECT title, licence, original_site, original_url, to_char(original_published_at, 'YYYY-MM-DD') FROM mf_designs ORDER BY title",
    )
    .fetch_all(&s.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (
                "Cable clip".into(),
                Some("cc-by".into()),
                Some("thingiverse".into()),
                Some("https://www.thingiverse.com/thing:1".into()),
                Some("2021-04-05".into())
            ),
            (
                "Plant pot".into(),
                None,
                Some("thingiverse".into()),
                Some("https://www.thingiverse.com/thing:2".into()),
                Some("2022-01-02".into())
            ),
        ],
        "the GPL licence isn't guessed at: the creator chooses"
    );
}

#[tokio::test]
async fn the_token_is_never_stored() {
    let Some(s) = thingiverse_site().await else { return };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let iid = import_id(link_thingiverse(&s, &ann, "code-ann").await.location());
    s.post(&format!("/3d/import/{iid}/drafts"), "", Some(&ann)).await;
    let found: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
            SELECT row_to_json(t)::text j FROM mf_imports t UNION ALL SELECT row_to_json(t)::text FROM mf_import_files t
            UNION ALL SELECT row_to_json(t)::text FROM mf_designs t UNION ALL SELECT row_to_json(t)::text FROM mf_oauth_states t
            UNION ALL SELECT row_to_json(t)::text FROM mf_sessions t) x WHERE j LIKE '%tok-ann%' OR j LIKE '%code-ann%'",
    )
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert_eq!(found, 0);
}

// ---------------------------------------------------------------- showcase posting (issue #30)

async fn social_site() -> Option<(Site, makerfoundry::model3d::social::stand_in::Calls)> {
    let (base, calls) = makerfoundry::model3d::social::stand_in::start().await.unwrap();
    let social = makerfoundry::model3d::social::Social::new("123", "page-token", "456", Some(&base));
    Some((services_site(social, None, None, None, false, None).await?, calls))
}

/// A 3D site with the outside services given.
async fn services_site(
    social: Option<makerfoundry::model3d::social::Social>,
    notices: Option<makerfoundry::notices::Notices>,
    step: Option<makerfoundry::model3d::step::Converter>,
    scad: Option<makerfoundry::model3d::scad::Renderer>,
    points: bool,
    stitchhub: Option<makerfoundry::shop_orders::hub::Hub>,
) -> Option<Site> {
    let pool = test_db().await?;
    let (storage, stored) = fake_storage().await;
    let storage_kept = storage.clone();
    let (url, fake) = fake_cloud().await;
    let app = makerfoundry::app(AppState {
        cloud: Cloud::new(&url).unwrap(),
        db: Some(pool.clone()),
        storage: Some(storage),
        publishers: makerfoundry::listings::Publishers::Everyone,
        admins: vec![1000],
        limits: std::sync::Arc::default(),
        stripe: None,
        launched: true,
        public_url: "https://makerfoundry.test".into(),
        api_origins: vec!["https://stitchfoundry.test".into()],
        proofs: None,
        three_d: true,
        scanner: None,
        thingiverse: None,
        social,
        notices,
        step,
        scad,
        points,
        stitchhub,
    });
    Some(Site {
        app,
        pool,
        fake,
        stored,
        storage: Some(storage_kept),
    })
}

#[tokio::test]
async fn only_active_consents_post_and_the_post_credits_only_the_display_name() {
    let Some((s, calls)) = social_site().await else { return };
    let (admin, bea, shared, ..) = showcase_site(&s).await;
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_showcase_consents WHERE design_id = $1")
        .bind(shared)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let queue = s.get("/admin/showcase", Some(&admin)).await.body;
    assert!(
        queue.contains("Post to Facebook") && queue.contains("Post to Instagram"),
        "{queue}"
    );
    assert_eq!(
        s.post(&format!("/admin/showcase/{cid}/post/facebook"), "", Some(&bea))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let r = s
        .post(&format!("/admin/showcase/{cid}/post/facebook"), "", Some(&admin))
        .await;
    assert_eq!(r.location(), "/admin/showcase");
    let sent = calls.lock().unwrap()[0].2.clone();
    assert!(
        sent.contains("Bea%27s+Prints") && sent.contains("makerfoundry.test%2Fd%2F"),
        "{sent}"
    );
    assert!(!sent.contains("example.com"), "no email: {sent}");
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_showcase_posts WHERE remote_id IS NOT NULL")
            .await,
        1
    );
    // a draft's consent, or a withdrawn one, never posts
    let draft_cid: i64 = sqlx::query_scalar(
        "SELECT c.id FROM mf_showcase_consents c JOIN mf_designs d ON d.id = c.design_id WHERE d.status = 'draft'",
    )
    .fetch_one(&s.pool)
    .await
    .unwrap();
    let r = s
        .post(&format!("/admin/showcase/{draft_cid}/post/facebook"), "", Some(&admin))
        .await;
    assert!(r.body.contains("can&#39;t be posted"), "{}", r.body);
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn withdrawn_before_posting_never_posts() {
    let Some((s, calls)) = social_site().await else { return };
    let (admin, bea, shared, ..) = showcase_site(&s).await;
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_showcase_consents WHERE design_id = $1")
        .bind(shared)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/account/showcase/{cid}/withdraw"), "", Some(&bea))
        .await;
    let r = s
        .post(&format!("/admin/showcase/{cid}/post/instagram"), "", Some(&admin))
        .await;
    assert!(r.body.contains("can&#39;t be posted"), "{}", r.body);
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn withdrawal_after_posting_deletes_or_flags() {
    let Some((s, calls)) = social_site().await else { return };
    let (admin, bea, shared, ..) = showcase_site(&s).await;
    let cid: i64 = sqlx::query_scalar("SELECT id FROM mf_showcase_consents WHERE design_id = $1")
        .bind(shared)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/admin/showcase/{cid}/post/facebook"), "", Some(&admin))
        .await;
    s.post(&format!("/admin/showcase/{cid}/post/instagram"), "", Some(&admin))
        .await;
    s.post(&format!("/account/showcase/{cid}/withdraw"), "", Some(&bea))
        .await;
    // Facebook's is deleted there; Instagram's is flagged for the admins
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.0 == "DELETE" && c.1.contains("111_"))
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_showcase_posts WHERE platform = 'facebook' AND taken_down_at IS NOT NULL")
            .await,
        1
    );
    let queue = s.get("/admin/showcase", Some(&admin)).await.body;
    assert!(
        queue.contains("Take these down") && queue.contains("instagram.com/p/test"),
        "{queue}"
    );
    assert!(!queue.contains("facebook.com/111_"), "{queue}");
}

// ---------------------------------------------------------------- email notices (issue #31)

async fn notices_site() -> Option<(
    Site,
    makerfoundry::notices::Notices,
    makerfoundry::notices::stand_in::Sent,
)> {
    let (base, sent) = makerfoundry::notices::stand_in::start().await.unwrap();
    let notices = makerfoundry::notices::Notices::new("re_test_key", None, Some(&base)).unwrap();
    Some((
        services_site(None, Some(notices.clone()), None, None, false, None).await?,
        notices,
        sent,
    ))
}

/// The digests sent, by recipient: (to, text).
fn digests(sent: &makerfoundry::notices::stand_in::Sent) -> Vec<(String, String)> {
    sent.lock()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["to"][0].as_str().unwrap().to_string(),
                v["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn notices_are_off_without_a_key_and_until_a_member_turns_them_on() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    assert_eq!(
        s.get("/account/notices", Some(&ann)).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!s.get("/account", Some(&ann)).await.body.contains("/account/notices"));
    drop(s);
    let Some((s, notices, sent)) = notices_site().await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    assert!(s.get("/account", Some(&ann)).await.body.contains("/account/notices"));
    let page = s.get("/account/notices", Some(&ann)).await.body;
    assert!(
        page.contains("Email me a daily update") && !page.contains("checked"),
        "{page}"
    );
    s.dump_page("notices", &page, Some(&ann)).await;
    // not turned on: nothing is noted
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    say(&s, gear, &bea, "Lovely").await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_notices").await, 0);
    makerfoundry::notices::send_digests(&s.pool, &notices, "https://makerfoundry.test")
        .await
        .unwrap();
    assert!(digests(&sent).is_empty());
    assert_eq!(
        s.get("/account/notices", None).await.location(),
        "/signin?next=/account/notices"
    );
}

#[tokio::test]
async fn comments_makes_remixes_and_followers_go_in_one_daily_digest() {
    let Some((s, notices, sent)) = notices_site().await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let r = s.post("/account/notices", "digest=yes", Some(&ann)).await;
    assert_eq!(r.location(), "/account/notices");
    assert!(s.get("/account/notices", Some(&ann)).await.body.contains("checked"));
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    say(&s, gear, &bea, "Lovely").await;
    // her own comment isn't news to her
    say(&s, gear, &ann, "Thanks").await;
    let r = s
        .send(multipart(
            &format!("/d/{gear}/makes"),
            &[
                ("photo", "print.jpg", photo_with_location()),
                ("confirm", "", b"yes".to_vec()),
            ],
            &bea,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // following twice, and naming a parent of a draft, are noted once, when it's published
    s.post("/makers/ann/follow", "", Some(&bea)).await;
    s.post("/makers/ann/unfollow", "", Some(&bea)).await;
    s.post("/makers/ann/follow", "", Some(&bea)).await;
    let remix = detailed_model(&s, &bea, "Big gear", "cc-by").await;
    let r = add_parent(&s, &bea, remix, &gear.to_string()).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_notices WHERE kind = 'remix'").await,
        0
    );
    s.post(&format!("/designs/{remix}/publish"), "confirm=yes", Some(&bea))
        .await;
    s.post(&format!("/designs/{remix}/unpublish"), "", Some(&bea)).await;
    s.post(&format!("/designs/{remix}/publish"), "confirm=yes", Some(&bea))
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_notices").await, 4);
    makerfoundry::notices::send_digests(&s.pool, &notices, "https://makerfoundry.test")
        .await
        .unwrap();
    let sent_now = digests(&sent);
    assert_eq!(sent_now.len(), 1, "{sent_now:?}");
    let (to, text) = &sent_now[0];
    assert_eq!(to, "ann@example.com");
    assert!(
        text.contains(&format!(
            "bea commented on Gear: https://makerfoundry.test/d/{gear}/gear"
        )),
        "{text}"
    );
    assert!(
        text.contains("bea posted a make of Gear") && text.contains("bea started following you"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "bea published a remix: Big gear: https://makerfoundry.test/d/{remix}/big-gear"
        )),
        "{text}"
    );
    assert!(
        !text.contains("bea@example.com"),
        "never someone else's address: {text}"
    );
    let headers = sent.lock().unwrap()[0]["headers"].clone();
    assert_eq!(headers["List-Unsubscribe-Post"], "List-Unsubscribe=One-Click");
    // at most once a day
    say(&s, gear, &bea, "Again").await;
    makerfoundry::notices::send_digests(&s.pool, &notices, "https://makerfoundry.test")
        .await
        .unwrap();
    assert_eq!(digests(&sent).len(), 1);
    sqlx::query("UPDATE mf_notice_prefs SET last_sent_at = now() - interval '25 hours'")
        .execute(&s.pool)
        .await
        .unwrap();
    makerfoundry::notices::send_digests(&s.pool, &notices, "https://makerfoundry.test")
        .await
        .unwrap();
    let all = digests(&sent);
    assert_eq!(all.len(), 2);
    assert!(
        all[1].1.contains("commented on Gear") && !all[1].1.contains("started following"),
        "{}",
        all[1].1
    );
}

#[tokio::test]
async fn one_click_unsubscribes_without_signing_in() {
    let Some((s, notices, sent)) = notices_site().await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    s.post("/account/notices", "digest=yes", Some(&ann)).await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    s.post("/makers/ann/follow", "", Some(&bea)).await;
    makerfoundry::notices::send_digests(&s.pool, &notices, "https://makerfoundry.test")
        .await
        .unwrap();
    let text = digests(&sent)[0].1.clone();
    let link = text
        .lines()
        .find_map(|l| l.strip_prefix("To stop these emails: https://makerfoundry.test"))
        .unwrap()
        .to_string();
    let (_, account_and_token) = link.split_at("/notices/unsubscribe/".len());
    let account = account_and_token.split('/').next().unwrap();
    // a wrong token, or another account's, does nothing
    assert_eq!(
        s.post(&format!("/notices/unsubscribe/{account}/00"), "", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let other: i64 = account.parse::<i64>().unwrap() + 1;
    let token = link.rsplit('/').next().unwrap();
    assert_eq!(
        s.post(&format!("/notices/unsubscribe/{other}/{token}"), "", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // looking at the link doesn't unsubscribe; the button does
    let page = s.get(&link, None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_notice_prefs WHERE digest").await, 1);
    let r = s.post(&link, "", None).await;
    assert!(r.body.contains("Unsubscribed"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_notice_prefs WHERE digest").await, 0);
    let cal = model_maker(&s, "cal%40example.com", "cal").await;
    s.post("/makers/ann/follow", "", Some(&cal)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_notices WHERE sent_at IS NULL").await,
        0
    );
}

#[tokio::test]
async fn notices_are_in_the_data_download_and_go_with_the_data() {
    let Some((s, ..)) = notices_site().await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    s.post("/account/notices", "digest=yes", Some(&ann)).await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    s.post("/makers/ann/follow", "", Some(&bea)).await;
    let data = s.get("/account/data.json", Some(&ann)).await.body;
    assert!(
        data.contains("\"notice_prefs\"") && data.contains("\"follow\""),
        "{data}"
    );
    let r = s.post("/account/data/delete", "confirm=DELETE", Some(&ann)).await;
    assert!(r.body.contains("Your MakerFoundry data is deleted"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_notices").await, 0);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_notice_prefs").await, 0);
}

// ---------------------------------------------------------------- STEP previews (issue #32)

#[tokio::test]
async fn a_step_file_gets_a_preview_from_the_converter_and_is_still_only_downloaded() {
    let (base, calls) = makerfoundry::model3d::step::stand_in::start().await.unwrap();
    let converter = makerfoundry::model3d::step::Converter::new(&base, Some("test-key"));
    let Some(s) = services_site(None, None, converter, None, false, None).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let r = upload_model(
        &s,
        &ann,
        &[
            ("part.stl", stl_cube(5.0)),
            ("bracket.step", STEP_FILE.as_bytes().to_vec()),
        ],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = design_id(r.location());
    assert_eq!(*calls.lock().unwrap(), 1, "only the STEP file is sent");
    let page = s.get(&format!("/designs/{id}"), Some(&ann)).await.body;
    assert!(page.contains("preview drawn from the STEP file"), "{page}");
    let step_id: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND format = 'step'")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let hero = s.get(&format!("/3d/{id}/files/{step_id}/hero"), Some(&ann)).await;
    assert_eq!(hero.status, StatusCode::OK);
    let mesh = s.get(&format!("/3d/{id}/files/{step_id}/mesh"), Some(&ann)).await;
    assert_eq!(mesh.status, StatusCode::OK);
    let (triangles, size): (Option<i64>, Option<Vec<f64>>) =
        sqlx::query_as("SELECT triangles, size_mm FROM mf_model_files WHERE id = $1")
            .bind(step_id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(triangles, Some(12));
    assert_eq!(size, Some(vec![10.0, 10.0, 10.0]));
    // the STEP file itself is kept as it came, and still only downloaded
    let kept = s
        .stored
        .lock()
        .unwrap()
        .values()
        .any(|(_, b)| b.as_slice() == STEP_FILE.as_bytes());
    assert!(kept);
    s.dump_page("step-preview", &page, Some(&ann)).await;
}

#[tokio::test]
async fn a_step_file_the_converter_cant_preview_is_still_taken() {
    let (base, calls) = makerfoundry::model3d::step::stand_in::start().await.unwrap();
    let converter = makerfoundry::model3d::step::Converter::new(&base, Some("test-key"));
    let Some(s) = services_site(None, None, converter, None, false, None).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bad = STEP_FILE.replace("'test'", "'BAD'");
    let huge = STEP_FILE.replace("'test'", "'HUGE'");
    let r = upload_model(
        &s,
        &ann,
        &[
            ("part.stl", stl_cube(5.0)),
            ("bad.step", bad.into_bytes()),
            ("huge.stp", huge.into_bytes()),
        ],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_model_files WHERE format = 'step' AND thumb_key IS NULL AND mesh_key IS NULL AND triangles IS NULL")
            .await,
        2
    );
    // without a converter, nothing is sent and STEP files have no preview, as before
    drop(s);
    let Some(s) = services_site(None, None, None, None, false, None).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let r = upload_model(
        &s,
        &ann,
        &[
            ("part.stl", stl_cube(5.0)),
            ("bracket.step", STEP_FILE.as_bytes().to_vec()),
        ],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_model_files WHERE format = 'step' AND thumb_key IS NOT NULL")
            .await,
        0
    );
}

// ---------------------------------------------------------------- public API for 3D models (issue #36)

/// A new API key for the person signed in with `cookie`.
async fn api_key(s: &Site, cookie: &str, name: &str) -> String {
    let page = s
        .post("/account/api-keys", &format!("name={name}"), Some(cookie))
        .await
        .body;
    let at = page.find("<code>mfk_").expect(&page) + "<code>".len();
    page[at..].split('<').next().unwrap().to_string()
}

async fn api_get(s: &Site, path: &str, key: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::get(path).header(header::HOST, HOST);
    if let Some(k) = key {
        req = req.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    let a = s.send(req.body(Body::empty()).unwrap()).await;
    (a.status, serde_json::from_str(&a.body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn api_keys_are_shown_once_kept_hashed_and_revoked() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    assert!(s.get("/account", Some(&ann)).await.body.contains("/account/api-keys"));
    let key = api_key(&s, &ann, "My+slicer").await;
    assert!(key.starts_with("mfk_") && key.len() == 52, "{key}");
    let page = s.get("/account/api-keys", Some(&ann)).await.body;
    assert!(
        page.contains("My slicer") && !page.contains(&key),
        "shown only once: {page}"
    );
    s.dump_page("api-keys", &page, Some(&ann)).await;
    let kept: Vec<u8> = sqlx::query_scalar("SELECT key_hash FROM mf_api_keys")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(kept.len(), 32);
    assert!(!kept.windows(4).any(|w| w == b"mfk_"));
    // no key, a wrong one, then the right one
    let (st, body) = api_get(&s, "/api/v1/models", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(body["error"]["message"].as_str().unwrap().contains("API key"));
    assert_eq!(
        api_get(&s, "/api/v1/models", Some("mfk_nope")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(api_get(&s, "/api/v1/models", Some(&key)).await.0, StatusCode::OK);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_api_keys WHERE last_used_at IS NOT NULL")
            .await,
        1
    );
    // up to five; revoked keys stop working; others can't revoke yours
    for n in 0..4 {
        api_key(&s, &ann, &format!("k{n}")).await;
    }
    let full = s.post("/account/api-keys", "name=six", Some(&ann)).await.body;
    assert!(full.contains("up to 5 keys"), "{full}");
    let id: i64 = sqlx::query_scalar("SELECT id FROM mf_api_keys WHERE name = 'My slicer'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    s.post(&format!("/account/api-keys/{id}/revoke"), "", Some(&bea)).await;
    assert_eq!(api_get(&s, "/api/v1/models", Some(&key)).await.0, StatusCode::OK);
    s.post(&format!("/account/api-keys/{id}/revoke"), "", Some(&ann)).await;
    assert_eq!(
        api_get(&s, "/api/v1/models", Some(&key)).await.0,
        StatusCode::UNAUTHORIZED
    );
    // in the data download (never the key or its hash), and gone with the data
    let data = s.get("/account/data.json", Some(&ann)).await.body;
    assert!(
        data.contains("\"api_keys\"") && data.contains("My slicer") && !data.contains("key_hash"),
        "{data}"
    );
    s.post("/account/data/delete", "confirm=DELETE", Some(&ann)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_api_keys").await, 5 - 5);
}

#[tokio::test]
async fn the_api_answers_published_models_only_with_their_files_and_profiles() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let vase = licensed_model(&s, &ann, "Vase", "cc0").await;
    let draft = detailed_model(&s, &ann, "Secret", "cc-by").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    let key = api_key(&s, &bea, "app").await;
    let (st, list) = api_get(&s, "/api/v1/models?per=1", Some(&key)).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list["models"][0]["id"], vase);
    assert_eq!(
        list["models"][0]["url"],
        format!("https://makerfoundry.test/d/{vase}/vase")
    );
    let next = list["next"].as_str().unwrap();
    assert!(
        next.starts_with("https://makerfoundry.test/api/v1/models?") && !next.contains("&amp;"),
        "{next}"
    );
    let (_, page2) = api_get(&s, next.trim_start_matches("https://makerfoundry.test"), Some(&key)).await;
    assert_eq!(page2["models"][0]["id"], gear);
    assert!(page2["next"].is_null());
    let (_, found) = api_get(&s, "/api/v1/models?q=gear&licence=cc-by", Some(&key)).await;
    assert_eq!(found["models"].as_array().unwrap().len(), 1);
    let (st, m) = api_get(&s, &format!("/api/v1/models/{gear}"), Some(&key)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(m["title"], "Gear");
    assert_eq!(m["licence"]["id"], "cc-by");
    assert_eq!(m["maker"]["url"], "https://makerfoundry.test/makers/ann");
    assert_eq!(m["files"][0]["format"], "stl");
    assert_eq!(m["files"][0]["triangles"], 12);
    assert_eq!(m["download"], format!("https://makerfoundry.test/d/{gear}/download"));
    assert!(m["published_at"].as_str().unwrap().ends_with('Z'));
    let text = m.to_string();
    assert!(
        !text.contains("@example.com") && !text.contains("file_key") && !text.contains("account"),
        "{text}"
    );
    assert_eq!(
        api_get(&s, &format!("/api/v1/models/{draft}"), Some(&key)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api_get(&s, "/api/v1/models/999999", Some(&key)).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, all) = api_get(&s, "/api/v1/models", Some(&key)).await;
    assert!(!all.to_string().contains("Secret"));
}

#[tokio::test]
async fn before_3d_opens_only_admin_keys_answer_and_keys_are_rate_limited() {
    let Some(s) = site_3d(vec![1000], false).await else {
        return;
    };
    let admin = model_maker(&s, "admin%40example.com", "admin").await;
    let key = api_key(&s, &admin, "admin").await;
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    assert_eq!(
        s.get("/account/api-keys", Some(&ann)).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!s.get("/account", Some(&ann)).await.body.contains("/account/api-keys"));
    // a key Ann had (say, from a founding period) doesn't answer before the opening
    let ann_id: i64 = sqlx::query_scalar("SELECT account_id FROM mf_makers WHERE handle = 'ann'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mf_api_keys (account_id, name, key_hash) VALUES ($1, 'old', sha256('mfk_ann'::bytea))")
        .bind(ann_id)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        api_get(&s, "/api/v1/models", Some("mfk_ann")).await.0,
        StatusCode::NOT_FOUND
    );
    for _ in 0..1000 {
        assert_eq!(
            api_get(&s, "/api/v1/models/1", Some(&key)).await.0,
            StatusCode::NOT_FOUND
        );
    }
    let (st, body) = api_get(&s, "/api/v1/models/1", Some(&key)).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

// ---------------------------------------------------------------- parametric models (issue #35)

const PARAMETRIC: &str = "// Size of the box (mm)\nsize = 40; // [10:5:100]\n// Lid\nlid = \"snap\"; // [snap, slide]\nfail = false;\n/* [Hidden] */\nsecret = 1;\nmodule box() { cube(size); }\nbox();\n";

/// A free published model with an STL and a parametric `.scad` → its id and the `.scad`'s id.
async fn parametric_model(s: &Site, cookie: &str, price: &str) -> (i64, i64) {
    let r = upload_model(
        s,
        cookie,
        &[("box.stl", stl_cube(5.0)), ("box.scad", PARAMETRIC.as_bytes().to_vec())],
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = design_id(r.location());
    s.post(
        &format!("/designs/{id}/edit"),
        &format!("title=Box&category=household&licence=cc-by&price={price}"),
        Some(cookie),
    )
    .await;
    let r = s
        .post(&format!("/designs/{id}/publish"), "confirm=yes", Some(cookie))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let file: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND format = 'scad'")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    (id, file)
}

#[tokio::test]
async fn a_parametric_model_is_customized_and_downloaded_as_a_checked_stl() {
    let (base, calls) = makerfoundry::model3d::scad::stand_in::start().await.unwrap();
    let scad = makerfoundry::model3d::scad::Renderer::new(&base, Some("test-key"));
    let Some(s) = services_site(None, None, None, scad, false, None).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let (id, file) = parametric_model(&s, &ann, "0").await;
    let model = s.get(&format!("/d/{id}/box"), None).await.body;
    assert!(model.contains(&format!("/d/{id}/customize/{file}")), "{model}");
    let page = s.get(&format!("/d/{id}/customize/{file}"), None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains(r#"name="p_size" value="40" min="10" max="100" step="5""#)
            && page.body.contains(r#"<option value="slide">slide</option>"#)
            && page.body.contains("Size of the box (mm)")
            && !page.body.contains("p_secret"),
        "{}",
        page.body
    );
    s.dump_page("customize", &page.body, None).await;
    // signing in first
    let r = s.post(&format!("/d/{id}/customize/{file}"), "p_size=55", None).await;
    assert!(r.location().starts_with("/signin"), "{}", r.location());
    let bea = s.join("bea%40example.com").await;
    let r = s
        .post(
            &format!("/d/{id}/customize/{file}"),
            "p_size=55&p_lid=slide&p_secret=9",
            Some(&bea),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.headers[header::CONTENT_TYPE], "model/stl");
    assert!(
        r.headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("box-custom.stl")
    );
    {
        let log = calls.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0]["scad"], PARAMETRIC);
        assert_eq!(log[0]["parameters"]["size"], 55.0);
        assert_eq!(log[0]["parameters"]["lid"], "slide");
        assert!(
            log[0]["parameters"].get("secret").is_none(),
            "hidden values aren't sent"
        );
    }
    // out of range, not a choice, or a render that fails: the form again, saying why
    let r = s
        .post(&format!("/d/{id}/customize/{file}"), "p_size=500", Some(&bea))
        .await;
    assert!(r.body.contains("size is out of its range"), "{}", r.body);
    let r = s
        .post(&format!("/d/{id}/customize/{file}"), "p_lid=hinge", Some(&bea))
        .await;
    assert!(r.body.contains("lid isn&#39;t one of its choices"), "{}", r.body);
    let r = s
        .post(&format!("/d/{id}/customize/{file}"), "p_fail=true", Some(&bea))
        .await;
    assert!(r.body.contains("couldn&#39;t be made"), "{}", r.body);
    assert_eq!(calls.lock().unwrap().len(), 2);
    // a file that isn't the model's .scad, or another model's
    assert_eq!(
        s.get(&format!("/d/{id}/customize/{}", file - 1), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.get(&format!("/d/{}/customize/{file}", id + 1), None).await.status,
        StatusCode::NOT_FOUND
    );
    // a form from elsewhere
    let mut req = Request::post(format!("/d/{id}/customize/{file}")).header(header::HOST, HOST);
    req = req
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, &bea)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    assert_eq!(
        s.send(req.body(Body::from("p_size=20")).unwrap()).await.status,
        StatusCode::FORBIDDEN
    );
    // rate limited: 30 an hour
    // (two renders so far; values refused before rendering aren't counted)
    for _ in 0..28 {
        s.post(&format!("/d/{id}/customize/{file}"), "p_size=20", Some(&bea))
            .await;
    }
    let r = s
        .post(&format!("/d/{id}/customize/{file}"), "p_size=20", Some(&bea))
        .await;
    assert!(r.body.contains("a lot of models this hour"), "{}", r.body);
}

#[tokio::test]
async fn customizing_is_off_without_the_service_and_for_models_for_sale() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let (id, file) = parametric_model(&s, &ann, "0").await;
    assert!(!s.get(&format!("/d/{id}/box"), None).await.body.contains("/customize/"));
    assert_eq!(
        s.get(&format!("/d/{id}/customize/{file}"), None).await.status,
        StatusCode::NOT_FOUND
    );
    drop(s);
    let (base, calls) = makerfoundry::model3d::scad::stand_in::start().await.unwrap();
    let scad = makerfoundry::model3d::scad::Renderer::new(&base, Some("test-key"));
    let Some(s) = services_site(None, None, None, scad, false, None).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    // a paid model can't be published without selling set up, so a draft stands in: not public
    let r = upload_model(
        &s,
        &ann,
        &[("box.stl", stl_cube(5.0)), ("box.scad", PARAMETRIC.as_bytes().to_vec())],
        &[],
    )
    .await;
    let draft = design_id(r.location());
    let draft_file: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND format = 'scad'")
        .bind(draft)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let bea = s.join("bea%40example.com").await;
    assert_eq!(
        s.get(&format!("/d/{draft}/customize/{draft_file}"), Some(&bea))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // its maker may try it before publishing
    assert_eq!(
        s.get(&format!("/d/{draft}/customize/{draft_file}"), Some(&ann))
            .await
            .status,
        StatusCode::OK
    );
    sqlx::query("UPDATE mf_designs SET status = 'published', price_cents = 500 WHERE id = $1")
        .bind(draft)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        s.get(&format!("/d/{draft}/customize/{draft_file}"), Some(&bea))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let r = s
        .post(&format!("/d/{draft}/customize/{draft_file}"), "p_size=20", Some(&bea))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(calls.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------- contests (issue #33)

const RULES: &str =
    "Rules+for+the+test+contest%3A+one+model+each%2C+your+own+work%2C+no+prizes.+These+are+the+owner%27s+words.";

/// A contest made and (when `rules`) opened by `admin`, closing in a day → its slug and id.
async fn contest(s: &Site, admin: &str, title: &str, judging: &str, rules: bool) -> (String, i64) {
    let closes = "2099-01-01T12%3A00";
    let r = s
        .post(
            "/admin/contests",
            &format!(
                "title={title}&theme=Things+that+hold+things&closes={closes}&judging={judging}&rules={}",
                if rules { RULES } else { "" }
            ),
            Some(admin),
        )
        .await;
    assert_eq!(r.location(), "/admin/contests", "{}", r.body);
    let (id, slug): (i64, String) = sqlx::query_as("SELECT id, slug FROM mf_contests ORDER BY id DESC LIMIT 1")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    s.post(&format!("/admin/contests/{id}/open"), "", Some(admin)).await;
    (slug, id)
}

async fn close(s: &Site, id: i64) {
    sqlx::query("UPDATE mf_contests SET closes_at = now() - interval '1 minute' WHERE id = $1")
        .bind(id)
        .execute(&s.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_contest_opens_only_with_rules_and_takes_creators_own_published_models() {
    let Some(s) = site_3d(vec![1000], true).await else {
        return;
    };
    let admin = model_maker(&s, "admin%40example.com", "admin").await;
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    assert_eq!(s.get("/admin/contests", Some(&ann)).await.status, StatusCode::NOT_FOUND);
    assert!(!s.get("/3d", None).await.body.contains("/contests"));
    // without rules it stays a draft, seen by nobody but admins
    let (draft, _) = contest(&s, &admin, "No+rules", "admins", false).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_contests WHERE status = 'draft'")
            .await,
        1
    );
    assert_eq!(
        s.get(&format!("/contests/{draft}"), Some(&ann)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.get(&format!("/contests/{draft}"), Some(&admin)).await.status,
        StatusCode::OK
    );
    let (slug, _) = contest(&s, &admin, "Hold+it", "admins", true).await;
    assert_eq!(slug, "hold-it");
    let landing = s.get("/3d", None).await.body;
    assert!(landing.contains("/contests/hold-it"), "{landing}");
    let page = s.get("/contests/hold-it", None).await.body;
    assert!(
        page.contains("Things that hold things") && page.contains("owner&#39;s words") && page.contains("Sign in"),
        "{page}"
    );
    // Ann enters her published model, not her draft nor Bea's
    let mine = licensed_model(&s, &ann, "Hook", "cc-by").await;
    let draft_model = detailed_model(&s, &ann, "Draft", "cc-by").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    let beas = licensed_model(&s, &bea, "Shelf", "cc-by").await;
    let r = s
        .post("/contests/hold-it/enter", &format!("design={mine}"), Some(&ann))
        .await;
    assert!(
        r.body.contains("agree to the rules"),
        "the box must be ticked: {}",
        r.body
    );
    let r = s
        .post(
            "/contests/hold-it/enter",
            &format!("design={mine}&agree=yes"),
            Some(&ann),
        )
        .await;
    assert_eq!(r.location(), "/contests/hold-it");
    for other in [draft_model, beas] {
        let r = s
            .post(
                "/contests/hold-it/enter",
                &format!("design={other}&agree=yes"),
                Some(&ann),
            )
            .await;
        assert!(r.body.contains("your published models"), "{}", r.body);
    }
    // up to three entries each
    for t in ["A", "B"] {
        let m = licensed_model(&s, &ann, t, "cc-by").await;
        s.post("/contests/hold-it/enter", &format!("design={m}&agree=yes"), Some(&ann))
            .await;
    }
    let fourth = licensed_model(&s, &ann, "C", "cc-by").await;
    let r = s
        .post(
            "/contests/hold-it/enter",
            &format!("design={fourth}&agree=yes"),
            Some(&ann),
        )
        .await;
    assert!(r.body.contains("up to 3 models"), "{}", r.body);
    let page = s.get("/contests/hold-it", Some(&ann)).await.body;
    assert!(
        page.contains(&format!("/d/{mine}/hook")) && page.contains("Withdraw"),
        "{page}"
    );
    s.dump_page("contest", &page, Some(&ann)).await;
    s.post(&format!("/contests/hold-it/withdraw/{mine}"), "", Some(&ann))
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_contest_entries").await, 2);
    // closed: no more entries
    let id: i64 = sqlx::query_scalar("SELECT id FROM mf_contests WHERE slug = 'hold-it'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    close(&s, id).await;
    let r = s
        .post(
            "/contests/hold-it/enter",
            &format!("design={mine}&agree=yes"),
            Some(&ann),
        )
        .await;
    assert!(r.body.contains("has closed"), "{}", r.body);
    // a form from elsewhere
    let mut req = Request::post("/contests/hold-it/enter").header(header::HOST, HOST);
    req = req
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, &ann)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    assert_eq!(
        s.send(req.body(Body::from("design=1&agree=yes")).unwrap()).await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn members_vote_once_never_for_themselves_and_admins_announce_the_winners() {
    let Some(s) = site_3d(vec![1000], true).await else {
        return;
    };
    let admin = model_maker(&s, "admin%40example.com", "admin").await;
    let (slug, id) = contest(&s, &admin, "Vote+it", "votes", true).await;
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    let hook = licensed_model(&s, &ann, "Hook", "cc-by").await;
    let shelf = licensed_model(&s, &bea, "Shelf", "cc-by").await;
    for (who, m) in [(&ann, hook), (&bea, shelf)] {
        s.post(
            &format!("/contests/{slug}/enter"),
            &format!("design={m}&agree=yes"),
            Some(who),
        )
        .await;
    }
    let r = s.post(&format!("/contests/{slug}/vote/{hook}"), "", Some(&ann)).await;
    assert!(r.body.contains("your own entry"), "{}", r.body);
    let cal = model_maker(&s, "cal%40example.com", "cal").await;
    s.post(&format!("/contests/{slug}/vote/{hook}"), "", Some(&cal)).await;
    s.post(&format!("/contests/{slug}/vote/{shelf}"), "", Some(&cal)).await;
    s.post(&format!("/contests/{slug}/vote/{shelf}"), "", Some(&ann)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_contest_votes").await,
        2,
        "one vote each, moved"
    );
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_contest_votes WHERE design_id = {shelf}"
        ))
        .await,
        2
    );
    // counts stay hidden while it's open
    let open = s.get(&format!("/contests/{slug}"), Some(&cal)).await.body;
    assert!(!open.contains("2 votes") && open.contains("Your vote"), "{open}");
    // announcing waits for the close
    let r = s
        .post(
            &format!("/admin/contests/{id}/announce"),
            &format!("place_{shelf}=1"),
            Some(&admin),
        )
        .await;
    assert!(r.body.contains("once a contest has closed"), "{}", r.body);
    close(&s, id).await;
    assert!(
        s.post(&format!("/contests/{slug}/vote/{hook}"), "", Some(&cal))
            .await
            .body
            .contains("has closed")
    );
    let queue = s.get("/admin/contests", Some(&admin)).await.body;
    assert!(
        queue.contains("Announce the winners") && queue.contains("(2 votes)"),
        "{queue}"
    );
    let r = s
        .post(
            &format!("/admin/contests/{id}/announce"),
            &format!("place_{shelf}=1&place_{hook}=1"),
            Some(&admin),
        )
        .await;
    assert!(r.body.contains("Each place goes to one entry"), "{}", r.body);
    let r = s
        .post(
            &format!("/admin/contests/{id}/announce"),
            &format!("place_{shelf}=1&place_{hook}=2"),
            Some(&admin),
        )
        .await;
    assert_eq!(r.location(), "/admin/contests");
    let done = s.get(&format!("/contests/{slug}"), None).await.body;
    assert!(
        done.contains("First place") && done.contains("Second place") && done.contains("2 votes"),
        "{done}"
    );
    assert!(done.find("Shelf") < done.find("Hook"), "the winner first");
    s.dump_page("contest-announced", &done, None).await;
    assert!(s.get("/contests", None).await.body.contains("Winners announced"));
    // in the data download, and gone with the data
    let data = s.get("/account/data.json", Some(&cal)).await.body;
    assert!(data.contains("\"contest_votes\""), "{data}");
    s.post("/account/data/delete", "confirm=DELETE", Some(&bea)).await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_contest_entries").await, 1);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_contest_votes").await,
        0,
        "votes for Bea's entry go with it"
    );
}

#[tokio::test]
async fn contests_are_hidden_while_3d_is_closed() {
    let Some(s) = site_3d(vec![1000], false).await else {
        return;
    };
    let admin = model_maker(&s, "admin%40example.com", "admin").await;
    let (slug, _) = contest(&s, &admin, "Early", "admins", true).await;
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    assert_eq!(s.get("/contests", Some(&ann)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        s.get(&format!("/contests/{slug}"), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.get(&format!("/contests/{slug}"), Some(&admin)).await.status,
        StatusCode::OK
    );
}

// ---------------------------------------------------------------- points (issue #34)

async fn make_of(s: &Site, cookie: &str, model: i64) {
    let r = s
        .send(multipart(
            &format!("/d/{model}/makes"),
            &[
                ("photo", "print.jpg", photo_with_location()),
                ("confirm", "", b"yes".to_vec()),
            ],
            cookie,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
}

#[tokio::test]
async fn points_come_only_from_what_others_do_with_your_work() {
    let Some(s) = services_site(None, None, None, None, true, None).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let ann_id: i64 = sqlx::query_scalar("SELECT account_id FROM mf_makers WHERE handle = 'ann'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let tally = || makerfoundry::model3d::points::tally(&s.pool, ann_id);
    // uploading, even many copies, earns nothing; nor do her own makes and likes
    let gear = licensed_model(&s, &ann, "Gear", "cc-by").await;
    let _copy = licensed_model(&s, &ann, "Gear again", "cc-by").await;
    make_of(&s, &ann, gear).await;
    s.post(&format!("/d/{gear}/like"), "", Some(&ann)).await;
    assert_eq!(tally().await.total(), 0);
    // Bea makes it twice (counted once), Cal makes it: 10 for the model, 3 a maker
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    let cal = model_maker(&s, "cal%40example.com", "cal").await;
    make_of(&s, &bea, gear).await;
    make_of(&s, &bea, gear).await;
    make_of(&s, &cal, gear).await;
    let t = tally().await;
    assert_eq!((t.models, t.made_by_others, t.makes), (1, 2, 0));
    assert_eq!(t.total(), 16);
    // a model liked by three others counts too
    let vase = licensed_model(&s, &ann, "Vase", "cc-by").await;
    for (n, who) in [&bea, &cal].iter().enumerate() {
        s.post(&format!("/d/{vase}/like"), "", Some(who)).await;
        assert_eq!(tally().await.models, 1, "{n}");
    }
    let dee = model_maker(&s, "dee%40example.com", "dee").await;
    s.post(&format!("/d/{vase}/like"), "", Some(&dee)).await;
    assert_eq!(tally().await.models, 2);
    // her makes of others' models: 2 each model, once
    let shelf = licensed_model(&s, &bea, "Shelf", "cc-by").await;
    make_of(&s, &ann, shelf).await;
    make_of(&s, &ann, shelf).await;
    assert_eq!(tally().await.makes, 1);
    assert_eq!(tally().await.total(), 20 + 6 + 2);
    // unpublished: it stops counting
    s.post(&format!("/designs/{gear}/unpublish"), "", Some(&ann)).await;
    assert_eq!(tally().await.total(), 10 + 2);
    let page = s.get("/account/points", Some(&ann)).await.body;
    assert!(
        page.contains("<strong>12</strong> points") && page.contains("How points are counted"),
        "{page}"
    );
    s.dump_page("points", &page, Some(&ann)).await;
    assert!(s.get("/makers/ann", None).await.body.contains("12 points"));
}

#[tokio::test]
async fn makes_of_others_models_count_up_to_twenty_a_month() {
    let Some(s) = services_site(None, None, None, None, true, None).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    let bea = model_maker(&s, "bea%40example.com", "bea").await;
    let bea_id: i64 = sqlx::query_scalar("SELECT account_id FROM mf_makers WHERE handle = 'bea'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    // 22 makes of Ann's models: rows stand in for photos, to keep the test quick
    for n in 0..22 {
        let m = licensed_model(&s, &ann, &format!("M{n}"), "cc-by").await;
        sqlx::query("INSERT INTO mf_makes (design_id, account_id, photo_key, thumb_key, width, height) VALUES ($1, $2, 'k', 'k', 1, 1)")
            .bind(m)
            .bind(bea_id)
            .execute(&s.pool)
            .await
            .unwrap();
    }
    assert_eq!(makerfoundry::model3d::points::tally(&s.pool, bea_id).await.makes, 20);
    let _ = bea;
}

#[tokio::test]
async fn points_are_off_unless_switched_on() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let ann = model_maker(&s, "ann%40example.com", "ann").await;
    assert_eq!(s.get("/account/points", Some(&ann)).await.status, StatusCode::NOT_FOUND);
    assert!(!s.get("/account", Some(&ann)).await.body.contains("/account/points"));
}

// ---------------------------------------------------------------- ordering from shops (MF-82 to MF-84)

/// A site with StitchHub (a stand-in) and Stripe (a fake) switched on → the site, its state (to run
/// the hub's round), the hub and Stripe.
async fn shops_site() -> Option<(
    Site,
    AppState,
    makerfoundry::shop_orders::hub::stand_in::Shared,
    StripeShared,
)> {
    shops_site_taxed(false).await
}

/// The same, with sales tax on (Stripe Tax) when `tax`.
async fn shops_site_taxed(
    tax: bool,
) -> Option<(
    Site,
    AppState,
    makerfoundry::shop_orders::hub::stand_in::Shared,
    StripeShared,
)> {
    let (hub_url, hub) = makerfoundry::shop_orders::hub::stand_in::start().await.unwrap();
    let (stripe, st) = fake_stripe().await;
    let stripe = if tax {
        stripe.with_tax(makerfoundry::stripe::DEFAULT_TAX_CODE).unwrap()
    } else {
        stripe
    };
    let pool = test_db().await?;
    let (storage, stored) = fake_storage().await;
    let storage_kept = storage.clone();
    let (url, fake) = fake_cloud().await;
    let state = AppState {
        cloud: Cloud::new(&url).unwrap(),
        db: Some(pool.clone()),
        storage: Some(storage),
        publishers: makerfoundry::listings::Publishers::Everyone,
        admins: vec![1000],
        limits: std::sync::Arc::default(),
        stripe: Some(stripe),
        launched: false,
        public_url: "https://makerfoundry.test".into(),
        api_origins: vec!["https://stitchfoundry.test".into()],
        proofs: None,
        three_d: true,
        scanner: None,
        thingiverse: None,
        social: None,
        notices: None,
        step: None,
        scad: None,
        points: false,
        stitchhub: makerfoundry::shop_orders::hub::Hub::new(&hub_url, "mf-test").map(|h| h.with_signer(shop_signer())),
    };
    {
        // the hub knows MakerFoundry's key, and lists the example shop as ready
        let mut h = hub.lock().unwrap();
        h.public_key = Some(shop_signer().public_spki());
        h.shops = vec![json!({"id": "shop:mainst", "name": "Main Street Threadworks", "ready": true})];
    }
    let app = makerfoundry::app(state.clone());
    Some((
        Site {
            app,
            pool,
            fake,
            stored,
            storage: Some(storage_kept),
        },
        state,
        hub,
        st,
    ))
}

/// MakerFoundry's signing key in these tests (PKCS#8 v1 from a fixed seed, RFC 8410 § 7).
fn shop_signer() -> makerfoundry::shop_orders::sign::Signer {
    use base64::Engine;
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
    ];
    der.extend_from_slice(&[42u8; 32]);
    makerfoundry::shop_orders::sign::Signer::from_base64(&base64::engine::general_purpose::STANDARD.encode(der))
        .unwrap()
}

/// The example catalogue (Main Street Threadworks, account 4217), as the hub delivers it.
fn catalog_message(id: &str) -> Value {
    let mut v: Value = serde_json::from_slice(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/contract/shop-orders/valid/catalog.json"
        ))
        .unwrap(),
    )
    .unwrap();
    v["id"] = id.into();
    v
}

/// Makes account 4217 (the example shop) able to be paid.
async fn shop_can_be_paid(s: &Site) {
    sqlx::query("INSERT INTO mf_accounts (account_id, email) VALUES (4217, 'shop@example.com') ON CONFLICT DO NOTHING")
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO mf_sellers (account_id, stripe_account, details_submitted, charges_enabled, payouts_enabled)
         VALUES (4217, 'acct_shop4217', true, true, true) ON CONFLICT DO NOTHING",
    )
    .execute(&s.pool)
    .await
    .unwrap();
    // and its holder has confirmed the shop is theirs (as the account page's button does)
    sqlx::query("UPDATE mf_shops SET account_confirmed = true WHERE account_id = 4217")
        .execute(&s.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn shops_are_missing_until_stitchhub_is_switched_on() {
    let Some(s) = site().await else { return };
    assert_eq!(s.get("/shops", None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_shop_publishes_its_catalogue_and_is_offered_once_it_can_be_paid() {
    let Some((s, state, hub, _)) = shops_site().await else {
        return;
    };
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c001"));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shops WHERE handle = 'mainst' AND account_id = 4217")
            .await,
        1
    );
    // kept, but not offered: the shop can't be paid yet
    assert!(s.get("/shops", None).await.body.contains("No shops take orders yet"));
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::NOT_FOUND);
    shop_can_be_paid(&s).await;
    let list = s.get("/shops", None).await.body;
    assert!(
        list.contains(r#"href="/shops/mainst""#)
            && list.contains("Main Street Threadworks")
            && list.contains("Elmwood"),
        "{list}"
    );
    let page = s.get("/shops/mainst", None).await.body;
    for want in [
        "Heavy cotton tee",
        "$8.00 a piece before decoration",
        "Black, Navy",
        "S, M, L, XL",
        "Embroidery",
        "left chest, back",
        "1+: $12.00 · 12+: $9.00",
        "set-up $25.00 an order",
        "Direct to garment (DTG)",
        "Screen print",
        "Shipping $8.99 an order, plus $0.50 for each piece after the first; free on orders of $300.00 or more.",
        r#"href="/shops/mainst/order/tee-heavy""#,
    ] {
        assert!(page.contains(want), "{want}: {page}");
    }
    s.dump_page("shop", &page, None).await;
    // the shop changes its prices: the page follows
    let mut again = catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c002");
    again["seq"] = 2.into();
    again["body"]["products"][0]["cents"] = 950.into();
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, again);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert!(
        s.get("/shops/mainst", None)
            .await
            .body
            .contains("$9.50 a piece before decoration")
    );
    assert_eq!(s.get("/shops/nobody", None).await.status, StatusCode::NOT_FOUND);
    // a shop's text can't reach the page as HTML
    let mut evil = catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c003");
    evil["seq"] = 3.into();
    evil["body"]["shop"]["name"] = "<script>alert(1)</script>".into();
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, evil);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let list = s.get("/shops", None).await.body;
    assert!(
        !list.contains("<script>alert") && list.contains("&lt;script&gt;"),
        "{list}"
    );
    // a shop the hub no longer lists as ready isn't offered
    hub.lock().unwrap().shops[0]["ready"] = json!(false);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::NOT_FOUND);
    hub.lock().unwrap().shops.clear();
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::NOT_FOUND);
}

/// The order form sent from our own page: text fields, and the art if given.
fn order_form(path: &str, fields: &[(&str, &str)], art: Option<(&str, &[u8])>, cookie: &str) -> Request<Body> {
    let boundary = "----mf-test-order";
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
        );
    }
    if let Some((name, bytes)) = art {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"art\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::post(path)
        .header(header::HOST, HOST)
        .header(header::ORIGIN, format!("https://{HOST}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::COOKIE, cookie)
        .body(Body::from(body))
        .unwrap()
}

/// A shops site with Main Street Threadworks' catalogue kept and the shop able to be paid.
async fn open_shop() -> Option<(
    Site,
    AppState,
    makerfoundry::shop_orders::hub::stand_in::Shared,
    StripeShared,
)> {
    open_shop_taxed(false).await
}

async fn open_shop_taxed(
    tax: bool,
) -> Option<(
    Site,
    AppState,
    makerfoundry::shop_orders::hub::stand_in::Shared,
    StripeShared,
)> {
    let (s, state, hub, st) = shops_site_taxed(tax).await?;
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c001"));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    shop_can_be_paid(&s).await;
    Some((s, state, hub, st))
}

const TEE: &str = "/shops/mainst/order/tee-heavy";

/// 10 Navy M and 2 Navy L, embroidered on the left chest, shipped: 12 pieces at $8 + $9.
fn tee_order<'a>() -> Vec<(&'a str, &'a str)> {
    vec![
        ("colour", "1"),
        ("q1", "10"),
        ("q2", "2"),
        ("decoration", "m0p0"),
        ("name", "Bea Example"),
        ("delivery", "ship"),
        ("line1", "12 Oak Street"),
        ("city", "Elmwood"),
        ("region", "OH"),
        ("postcode", "44000"),
        ("country", "US"),
        ("note", "Thread to match the logo, please."),
        ("rights", "yes"),
    ]
}

#[tokio::test]
async fn a_customer_orders_from_a_shop_pays_and_the_shop_gets_it() {
    let Some((s, state, hub, st)) = open_shop().await else {
        return;
    };
    // signing in comes first
    assert!(s.get(TEE, None).await.location().starts_with("/signin"));
    let bea = s.sign_up("bea%40example.com").await;
    let form = s.get(TEE, Some(&bea)).await.body;
    for want in [
        "Order: Heavy cotton tee",
        "Navy",
        "XL",
        "Embroidery on the left chest",
        "Direct to garment (DTG) on the front",
        "Ship it to me",
    ] {
        assert!(form.contains(want), "{want}: {form}");
    }
    s.dump_page("shop-order-form", &form, Some(&bea)).await;
    assert_eq!(
        s.get("/shops/mainst/order/nothing", Some(&bea)).await.status,
        StatusCode::NOT_FOUND
    );

    let pes = sample(sf_formats::Format::Pes);
    let made = s
        .send(order_form(TEE, &tee_order(), Some(("logo.pes", &pes)), &bea))
        .await;
    let path = made.location().to_string();
    assert!(path.starts_with("/shop-orders/mf-"), "{} {}", made.status, made.body);
    let reference = path.trim_start_matches("/shop-orders/").to_string();
    // 12 × ($8 + $9) + $25 set-up + $8.99 + 11 × 50¢ shipping; 10% and the card fee from the shop's share
    let amounts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT goods_cents, setup_cents, shipping_cents, paid_cents, fee_cents, card_fee_cents FROM mf_shop_orders",
    )
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert_eq!(amounts, (20_400, 2_500, 1_449, 24_349, 2_435, 736));
    assert_eq!(s.scalar("SELECT shop_cents FROM mf_shop_orders").await, 21_178);
    let page = s.get(&path, Some(&bea)).await.body;
    for want in [
        "Your order from Main Street Threadworks",
        "Not paid yet",
        "Pay $243.49",
        "tee-heavy, Navy: L × 2, M × 10",
        "Embroidery on the left chest (logo.pes)",
        "Shipping to 12 Oak Street, Elmwood, OH 44000",
        "$204.00",
        "$25.00",
        "$14.49",
    ] {
        assert!(page.contains(want), "{want}: {page}");
    }
    s.dump_page("shop-order", &page, Some(&bea)).await;
    // nobody else sees it; nothing goes to the shop before it's paid
    let sam = s.sign_up("sam%40example.com").await;
    assert_eq!(s.get(&path, Some(&sam)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        s.post(&format!("{path}/pay"), "", Some(&sam)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_hub_outbox").await, 0);
    let sha: String = sqlx::query_scalar("SELECT art->0->>'sha256' FROM mf_shop_orders")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert!(
        hub.lock().unwrap().blobs.is_empty(),
        "no file goes to the hub before payment"
    );

    // paying: Stripe Checkout, MakerFoundry's 10% and the card fee as the application fee
    let pay = s.post(&format!("{path}/pay"), "", Some(&bea)).await;
    assert_eq!(pay.location(), "https://checkout.stripe.com/c/pay/cs_test_1");
    let f = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    assert_eq!(f["line_items[0][price_data][unit_amount]"], "24349");
    assert_eq!(f["payment_intent_data[application_fee_amount]"], "3171");
    assert_eq!(f["payment_intent_data[transfer_data][destination]"], "acct_shop4217");
    assert_eq!(f["payment_intent_data[statement_descriptor_suffix]"], "MAKERFOUNDRY");
    assert_eq!(f["metadata[shop_order]"], reference);
    assert_eq!(f["metadata[app]"], "makerfoundry");
    // a second click goes to the same checkout, so the order can't be paid twice
    assert_eq!(
        s.post(&format!("{path}/pay"), "", Some(&bea)).await.location(),
        "https://checkout.stripe.com/c/pay/cs_test_1"
    );
    assert_eq!(st.lock().unwrap().sessions.len(), 1);
    // a session that isn't paid, or isn't this order's, changes nothing
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_so_0",
        "checkout.session.completed",
        session_json("cs_test_1", &f, false),
    )
    .await;
    let mut other = session_json("cs_test_1", &f, true);
    other["metadata"]["shop_order"] = json!("mf-0000000000000000");
    s.stripe_event(WEBHOOK_SECRET, "evt_so_1", "checkout.session.completed", other)
        .await;
    let mut short = session_json("cs_test_1", &f, true);
    short["amount_total"] = json!(100);
    s.stripe_event(WEBHOOK_SECRET, "evt_so_2", "checkout.session.completed", short)
        .await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'pending'")
            .await,
        1
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_ledger").await, 0);

    // paid: the ledger and order.placed, once
    for evt in ["evt_so_3", "evt_so_4"] {
        let a = s
            .stripe_event(
                WEBHOOK_SECRET,
                evt,
                "checkout.session.completed",
                session_json("cs_test_1", &f, true),
            )
            .await;
        assert_eq!(a.status, StatusCode::OK);
    }
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'sent' AND paid_at IS NOT NULL")
            .await,
        1
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_ledger").await, 3);
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_shop_ledger").await, 24_349);
    assert_eq!(
        s.scalar("SELECT cents FROM mf_shop_ledger WHERE kind = 'shop' AND account_id = 4217")
            .await,
        21_178
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_hub_outbox WHERE kind = 'order.placed'")
            .await,
        1
    );
    assert!(s.get(&path, Some(&bea)).await.body.contains("Sent to the shop"));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let placed = hub
        .lock()
        .unwrap()
        .received
        .iter()
        .find(|m| m["type"] == "order.placed")
        .cloned()
        .unwrap();
    assert_eq!(placed["conversation"], json!(reference));
    assert!(placed.get("from").is_none());
    assert_eq!(placed["to"], "shop:mainst");
    assert_eq!(placed["seq"], 1);
    assert_eq!(placed["body"]["fulfilment"], "ship");
    assert_eq!(placed["body"]["totals"]["paid_cents"], 24_349);
    assert_eq!(placed["body"]["totals"]["shop_gets_cents"], 21_178);
    assert_eq!(placed["body"]["customer"]["ship_to"]["city"], "Elmwood");
    assert_eq!(placed["body"]["lines"][0]["sizes"]["M"], 10);
    assert_eq!(
        placed["body"]["lines"][0]["decorations"][0]["art"]["sha256"],
        json!(sha)
    );
    assert!(
        placed["body"].get("art_urls").is_none(),
        "no addresses: the art is in the hub"
    );
    // the message passes the contract's own checks, and is signed with MakerFoundry's key
    let (env, _) = makerfoundry::shop_orders::contract::parse(placed.to_string().as_bytes()).unwrap();
    assert!(makerfoundry::shop_orders::sign::verify(
        &shop_signer().public_spki(),
        &env
    ));
    // the art went to the hub first (put, then done), for this conversation
    {
        let h = hub.lock().unwrap();
        assert_eq!(h.blobs.get(&sha), Some(&pes));
        assert!(h.done.contains(&(reference.clone(), sha.clone())));
    }
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_hub_files WHERE conversation = (SELECT ref FROM mf_shop_orders)")
            .await,
        1
    );
    assert!(s.stored.lock().unwrap().values().any(|(_, b)| *b == pes));
    // MakerFoundry no longer hands the art out itself
    assert_eq!(
        s.get(&format!("{path}/art/{sha}"), None).await.status,
        StatusCode::NOT_FOUND
    );
    // and paying again does nothing
    assert_eq!(s.post(&format!("{path}/pay"), "", Some(&bea)).await.location(), path);
}

#[tokio::test]
async fn coming_back_from_checkout_sends_a_shop_order_without_waiting_for_the_webhook() {
    let Some((s, _, _, st)) = open_shop().await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    let photo = photo_with_location();
    let mut fields = tee_order();
    fields.retain(|(k, _)| *k != "decoration" && *k != "delivery");
    fields.push(("decoration", "m1p0"));
    fields.push(("delivery", "pickup"));
    let path = s
        .send(order_form(TEE, &fields, Some(("front.jpg", &photo)), &bea))
        .await
        .location()
        .to_string();
    assert!(path.starts_with("/shop-orders/mf-"));
    // picked up: no shipping; DTG at $10 a piece, no set-up
    assert_eq!(s.scalar("SELECT paid_cents FROM mf_shop_orders").await, 12 * 1_800);
    assert!(s.get(&path, Some(&bea)).await.body.contains("Pick up at the shop"));
    s.post(&format!("{path}/pay"), "", Some(&bea)).await;
    // back from Stripe before it's paid: nothing changes
    assert!(
        s.get(&format!("{path}?session=cs_test_1"), Some(&bea))
            .await
            .body
            .contains("Not paid yet")
    );
    // another paid session of the same amount (a design bought, say) doesn't pay for this order
    let mut design = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    design.remove("metadata[shop_order]");
    design.insert("metadata[order]".into(), "1".into());
    st.lock()
        .unwrap()
        .sessions
        .insert("cs_test_other".into(), (design, true));
    assert!(
        s.get(&format!("{path}?session=cs_test_other"), Some(&bea))
            .await
            .body
            .contains("Not paid yet")
    );
    st.lock().unwrap().sessions.get_mut("cs_test_1").unwrap().1 = true;
    // someone else's visit with the session checks nothing
    let sam = s.sign_up("sam%40example.com").await;
    assert_eq!(
        s.get(&format!("{path}?session=cs_test_1"), Some(&sam)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_ledger").await, 0);
    let page = s.get(&format!("{path}?session=cs_test_1"), Some(&bea)).await.body;
    assert!(page.contains("Sent to the shop"), "{page}");
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_ledger").await, 3);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_hub_outbox").await, 1);
    // the webhook arriving later adds nothing
    let f = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_so_9",
        "checkout.session.completed",
        session_json("cs_test_1", &f, true),
    )
    .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_ledger").await, 3);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_hub_outbox").await, 1);
}

#[tokio::test]
async fn order_problems_are_explained_and_nothing_is_kept() {
    let Some((s, _, _, _)) = open_shop().await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    let pes = sample(sf_formats::Format::Pes);
    let photo = photo_with_location();
    let without = |drop: &str| {
        let mut f = tee_order();
        f.retain(|(k, _)| *k != drop);
        f
    };
    let mut no_pieces = without("q1");
    no_pieces.retain(|(k, _)| *k != "q2");
    let mut too_many = without("q1");
    too_many.push(("q1", "100000"));
    let mut no_address = without("line1");
    no_address.push(("line1", ""));
    let mut bad_choice = without("decoration");
    bad_choice.push(("decoration", "m9p0"));
    // contact details stay out of what the shop sees (the hub refuses them)
    let mut email_note = without("note");
    email_note.push(("note", "Mail me at bea@example.com"));
    let mut phone_name = without("name");
    phone_name.push(("name", "Bea 555 123 4567"));
    // issue #80: the hub takes a note of 500 characters at most; a longer one is refused, not cut
    let long = "a".repeat(501);
    let mut long_note = without("note");
    long_note.push(("note", &long));
    let longer = "a".repeat(6000);
    let mut longer_note = without("note");
    longer_note.push(("note", &longer));
    for (fields, art, want) in [
        (
            email_note,
            Some(("logo.pes", &pes[..])),
            "Leave email addresses and phone numbers out",
        ),
        (
            phone_name,
            Some(("logo.pes", &pes[..])),
            "Leave email addresses and phone numbers out",
        ),
        (
            long_note,
            Some(("logo.pes", &pes[..])),
            "Your note for the shop is 501 characters long; it can be 500 at most.",
        ),
        (
            longer_note,
            Some(("logo.pes", &pes[..])),
            "Your note for the shop is 6000 characters long; it can be 500 at most.",
        ),
        (
            without("rights"),
            Some(("logo.pes", &pes[..])),
            "Confirm the art is yours",
        ),
        (no_pieces, Some(("logo.pes", &pes[..])), "Say how many of each size"),
        (too_many, Some(("logo.pes", &pes[..])), "at most"),
        (no_address, Some(("logo.pes", &pes[..])), "Give the shipping address"),
        (bad_choice, Some(("logo.pes", &pes[..])), "Choose a decoration"),
        (tee_order(), None, "Choose your art"),
        // embroidery needs an embroidery file
        (tee_order(), Some(("photo.jpg", &photo[..])), "The art:"),
        (without("name"), Some(("logo.pes", &pes[..])), "Give your name"),
    ] {
        let a = s.send(order_form(TEE, &fields, art, &bea)).await;
        assert_eq!(a.status, StatusCode::OK, "{want}");
        assert!(a.body.contains(want), "{want}: {}", a.body);
    }
    // printing needs a picture
    let mut dtg = without("decoration");
    dtg.push(("decoration", "m1p0"));
    let a = s.send(order_form(TEE, &dtg, Some(("logo.pes", &pes)), &bea)).await;
    assert!(a.body.contains("The art:"), "{}", a.body);
    // from another site
    let mut req = order_form(TEE, &tee_order(), Some(("logo.pes", &pes)), &bea);
    req.headers_mut()
        .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
    assert_eq!(s.send(req).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_orders").await, 0);
    assert!(s.stored.lock().unwrap().is_empty());
}

/// Issue #80: a note of exactly 500 characters (the hub's limit, as it counts) is sent whole, and
/// the form says so; a browser's line ends are sent as `\n`.
#[tokio::test]
async fn a_note_of_500_characters_goes_to_the_shop_whole() {
    let Some((s, state, hub, st)) = open_shop().await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    let form = s.get(TEE, Some(&bea)).await.body;
    assert!(
        form.contains(r#"<textarea name="note" maxlength="500""#) && form.contains("500 characters at most"),
        "{form}"
    );
    // the live counter (owner's yes, 2026-11-03): our own script file, allowed by the page's policy
    let page = s.get(TEE, Some(&bea)).await;
    assert!(
        form.contains(r#"data-count="500""#)
            && form.contains(r#"<script src="/static/note-counter.js" defer></script>"#),
        "{form}"
    );
    assert_eq!(
        page.headers[header::CONTENT_SECURITY_POLICY],
        makerfoundry::NOTE_COUNTER_SECURITY_POLICY
    );
    // 249 + a line break (one space, as the hub counts) + 250 = 500
    let note = format!("{}\r\n{}", "a".repeat(249), "b".repeat(250));
    let mut fields = tee_order();
    fields.retain(|(k, _)| *k != "note");
    fields.push(("note", &note));
    let pes = sample(sf_formats::Format::Pes);
    let path = s
        .send(order_form(TEE, &fields, Some(("logo.pes", &pes)), &bea))
        .await
        .location()
        .to_string();
    assert!(path.starts_with("/shop-orders/mf-"), "{path}");
    s.post(&format!("{path}/pay"), "", Some(&bea)).await;
    let f = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_paid",
        "checkout.session.completed",
        session_json("cs_test_1", &f, true),
    )
    .await;
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let placed = sent_of(&hub, "order.placed");
    assert_eq!(placed.len(), 1);
    let sent = placed[0]["body"]["note"].as_str().unwrap();
    assert_eq!(sent, format!("{}\n{}", "a".repeat(249), "b".repeat(250)));
    assert_eq!(makerfoundry::shop_orders::contract::hub_len(sent), 500);
}

/// A paid order (the tee, shipped) from bea → bea's cookie, the order's page and its reference.
async fn paid_order(s: &Site, st: &StripeShared) -> (String, String, String) {
    let bea = s.sign_up("bea%40example.com").await;
    let pes = sample(sf_formats::Format::Pes);
    let path = s
        .send(order_form(TEE, &tee_order(), Some(("logo.pes", &pes)), &bea))
        .await
        .location()
        .to_string();
    let reference = path.trim_start_matches("/shop-orders/").to_string();
    s.post(&format!("{path}/pay"), "", Some(&bea)).await;
    let f = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_paid",
        "checkout.session.completed",
        session_json("cs_test_1", &f, true),
    )
    .await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'sent'")
            .await,
        1
    );
    (bea, path, reference)
}

/// A message from the shop (or the hub) about `order`, made from the contract's example of `kind`.
fn from_shop(kind: &str, n: u32, order: &str, seq: u64, body: Value) -> Value {
    let mut v: Value = serde_json::from_slice(
        &std::fs::read(format!(
            "{}/../../docs/contract/shop-orders/valid/{kind}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap();
    v["id"] = json!(format!("0192f0c4-7a1b-7c2d-8e3f-{n:012}"));
    if kind == "message.expired" {
        v["body"]["conversation"] = json!(order);
    } else {
        v["conversation"] = json!(order);
        v["from"] = json!("shop:mainst");
    }
    v["to"] = json!("service:makerfoundry");
    v["seq"] = json!(seq);
    v["body"] = body;
    v
}

/// A small PNG, as a shop's mock-up.
fn mockup_png() -> Vec<u8> {
    let picture = image::RgbImage::from_fn(60, 40, |x, y| image::Rgb([(x * 4) as u8, (y * 6) as u8, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    picture.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn mockup(hub: &makerfoundry::shop_orders::hub::stand_in::Shared, conversation: &str, bytes: &[u8]) -> Value {
    let sha = makerfoundry::shop_orders::hub::stand_in::put_file(hub, conversation, bytes);
    json!({ "size": bytes.len(), "sha256": sha, "type": "image/png" })
}

/// What MakerFoundry sent the hub of `kind`, oldest first.
fn sent_of(hub: &makerfoundry::shop_orders::hub::stand_in::Shared, kind: &str) -> Vec<Value> {
    hub.lock()
        .unwrap()
        .received
        .iter()
        .filter(|m| m["type"] == kind)
        .cloned()
        .collect()
}

#[tokio::test]
async fn the_shop_reports_progress_and_the_customer_answers_proofs() {
    let Some((s, state, hub, st)) = open_shop().await else {
        return;
    };
    let (bea, path, r) = paid_order(&s, &st).await;
    let deliver = |m: Value| makerfoundry::shop_orders::hub::stand_in::deliver(&hub, m);
    deliver(from_shop("order.status", 1, &r, 1, json!({"status": "received"})));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert!(s.get(&path, Some(&bea)).await.body.contains("Received by the shop"));

    // a proof: fetched from the hub, checked, re-drawn, shown only to the customer
    let png = mockup_png();
    let m1 = mockup(&hub, &r, &png);
    deliver(from_shop(
        "proof.sent",
        2,
        &r,
        2,
        json!({"proof": 1, "mockup": m1, "note": "At 3.5 inches."}),
    ));
    // another shop's word about this order, and a mock-up that isn't the one described, are left
    let mut stranger = from_shop("order.status", 3, &r, 1, json!({"status": "shipped"}));
    stranger["from"] = json!("shop:other");
    deliver(stranger);
    let mut wrong = mockup(&hub, &r, &png);
    wrong["sha256"] = json!("c".repeat(64));
    deliver(from_shop("proof.sent", 4, &r, 3, json!({"proof": 9, "mockup": wrong})));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert!(
        hub.lock()
            .unwrap()
            .queues
            .get("service:makerfoundry")
            .is_none_or(Vec::is_empty),
        "all taken"
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_proofs").await, 1);
    let page = s.get(&path, Some(&bea)).await.body;
    for want in [
        "A proof is waiting for you",
        &format!(r#"src="{path}/proofs/1/image""#),
        "Proof 1: At 3.5 inches.",
        "Approve this proof",
        "Ask for changes",
        "Cancel the order",
    ] {
        assert!(page.contains(want), "{want}: {page}");
    }
    s.dump_page("shop-order-proof", &page, Some(&bea)).await;
    let img = s.get(&format!("{path}/proofs/1/image"), Some(&bea)).await;
    assert_eq!(img.status, StatusCode::OK);
    assert_eq!(img.headers[header::CONTENT_TYPE], "image/png");
    let sam = s.sign_up("sam%40example.com").await;
    assert_eq!(
        s.get(&format!("{path}/proofs/1/image"), Some(&sam)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.post(&format!("{path}/proofs/1"), "answer=approved", Some(&sam))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // changes need a note
    s.post(&format!("{path}/proofs/1"), "answer=changes&note=", Some(&bea))
        .await;
    assert!(sent_of(&hub, "proof.answer").is_empty());
    s.post(
        &format!("{path}/proofs/1"),
        "answer=changes&note=A+little+smaller",
        Some(&bea),
    )
    .await;
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let answers = sent_of(&hub, "proof.answer");
    assert_eq!(answers.len(), 1);
    assert_eq!(
        answers[0]["body"],
        json!({"proof": 1, "answer": "changes", "note": "A little smaller"})
    );
    assert_eq!(answers[0]["seq"], 2, "after order.placed");
    // answered once
    s.post(&format!("{path}/proofs/1"), "answer=approved", Some(&bea)).await;
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(sent_of(&hub, "proof.answer").len(), 1);

    let m2 = mockup(&hub, &r, &png);
    deliver(from_shop("proof.sent", 5, &r, 4, json!({"proof": 2, "mockup": m2})));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(
        page.contains("You asked for changes:</strong> A little smaller"),
        "{page}"
    );
    // only the latest proof can be answered
    s.post(&format!("{path}/proofs/1"), "answer=approved", Some(&bea)).await;
    s.post(&format!("{path}/proofs/2"), "answer=approved", Some(&bea)).await;
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let answers = sent_of(&hub, "proof.answer");
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[1]["body"]["proof"], 2);
    assert_eq!(answers[1]["body"]["answer"], "approved");
    assert!(s.get(&path, Some(&bea)).await.body.contains("Proof approved"));

    // production: no more cancelling
    deliver(from_shop(
        "order.status",
        6,
        &r,
        5,
        json!({"status": "in_production", "note": "On the machine today."}),
    ));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(
        page.contains("Being made") && page.contains("The shop says: On the machine today."),
        "{page}"
    );
    assert!(!page.contains("Cancel the order"));
    s.post(
        &format!("{path}/cancel"),
        "reason=other&detail=changed+my+mind",
        Some(&bea),
    )
    .await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'in_production'")
            .await,
        1
    );
    deliver(from_shop(
        "order.status",
        7,
        &r,
        6,
        json!({"status": "shipped", "tracking": "1Z999AA10123456784"}),
    ));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(
        page.contains("Shipped") && page.contains("Tracking: 1Z999AA10123456784"),
        "{page}"
    );
    assert!(st.lock().unwrap().refunds.is_empty());
}

#[tokio::test]
async fn a_cancelled_order_is_refunded_and_the_shop_is_out_only_the_card_fee() {
    let Some((s, state, hub, st)) = open_shop().await else {
        return;
    };
    let (bea, path, r) = paid_order(&s, &st).await;
    st.lock().unwrap().fee_refunds_fail = true;
    // issue #80: the hub needs a reason on every cancel, so one without is sent back, explained,
    // and nothing is cancelled or sent
    let form = s.get(&path, Some(&bea)).await.body;
    assert!(
        form.contains(r#"name="reason" value="mistake" required"#) && form.contains("Another reason"),
        "{form}"
    );
    for (sent, problem) in [
        ("", "cancel-reason"),
        ("reason=other", "cancel-reason"),
        ("reason=other&detail=+++", "cancel-reason"),
        ("reason=bored", "cancel-reason"),
        ("reason=other&detail=Call+me+on+401+555+0100", "contact"),
    ] {
        let a = s.post(&format!("{path}/cancel"), sent, Some(&bea)).await;
        assert_eq!(a.location(), format!("{path}?problem={problem}"), "{sent}");
    }
    let long = format!("reason=change&detail={}", "a".repeat(600));
    let a = s.post(&format!("{path}/cancel"), &long, Some(&bea)).await;
    assert_eq!(a.location(), format!("{path}?problem=too-long"));
    let page = s.get(&format!("{path}?problem=cancel-reason"), Some(&bea)).await.body;
    assert!(page.contains("the shop needs a reason"), "{page}");
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'cancelled'")
            .await,
        0
    );
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert!(sent_of(&hub, "order.cancel").is_empty());
    s.post(
        &format!("{path}/cancel"),
        "reason=other&detail=Wrong+colour",
        Some(&bea),
    )
    .await;
    // the shop is told; the payment comes back, taken from the shop
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let cancels = sent_of(&hub, "order.cancel");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0]["body"], json!({"by": "customer", "why": "Wrong colour"}));
    let refund = st.lock().unwrap().refunds[0].clone();
    assert_eq!(refund["payment_intent"], "pi_cs_test_1");
    assert_eq!(refund["reverse_transfer"], "true");
    assert_eq!(refund["refund_application_fee"], "false");
    assert_eq!(refund["metadata[shop_order]"], r);
    assert_eq!(
        s.scalar("SELECT cents FROM mf_shop_ledger WHERE reason = 'refund'")
            .await,
        -24_349
    );
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(
        page.contains("You cancelled this order: Wrong colour.") && page.contains("Refunded: $243.49"),
        "{page}"
    );
    assert!(!page.contains("Cancel the order"));
    s.dump_page("shop-order-cancelled", &page, Some(&bea)).await;
    // Stripe couldn't give the 10% back: the sweep does, once
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_ledger WHERE reason = 'fee_return'")
            .await,
        0
    );
    st.lock().unwrap().fee_refunds_fail = false;
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    let fee_refunds = st.lock().unwrap().fee_refunds.clone();
    assert_eq!(fee_refunds.len(), 1);
    assert_eq!(fee_refunds[0].0, "fee_pi_cs_test_1");
    assert_eq!(fee_refunds[0].1["amount"], "2435");
    assert_eq!(st.lock().unwrap().refunds.len(), 1, "refunded once");
    // in the end: the customer has it all back, MakerFoundry keeps nothing but the card fee it
    // paid Stripe, and the shop is out the card fee
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_shop_ledger").await, 0);
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_shop_ledger WHERE kind = 'shop'")
            .await,
        -736
    );
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_shop_ledger WHERE kind = 'fee'")
            .await,
        0
    );
    // Stripe's charge.refunded, coming later, adds nothing
    let charge = json!({"id": "ch_1", "object": "charge", "payment_intent": "pi_cs_test_1", "refunded": true});
    s.stripe_event(WEBHOOK_SECRET, "evt_refunded", "charge.refunded", charge)
        .await;
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_ledger").await, 6);
    // and cancelling again does nothing
    s.post(&format!("{path}/cancel"), "reason=mistake", Some(&bea)).await;
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(sent_of(&hub, "order.cancel").len(), 1);
}

#[tokio::test]
async fn an_order_the_shop_never_takes_or_turns_down_is_refunded() {
    let Some((s, state, hub, st)) = open_shop().await else {
        return;
    };
    let (bea, path, r) = paid_order(&s, &st).await;
    let deliver = |m: Value| makerfoundry::shop_orders::hub::stand_in::deliver(&hub, m);
    // the hub gave up delivering it
    deliver(from_shop(
        "message.expired",
        1,
        &r,
        1,
        json!({"id": "0192f0c4-7a1b-7c2d-8e3f-0123456789ff", "type": "order.placed", "conversation": r, "reason": "expired"}),
    ));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(
        page.contains("The shop couldn't take the order, so it's refunded") && page.contains("Refunded: $243.49"),
        "{page}"
    );
    assert_eq!(st.lock().unwrap().refunds.len(), 1);
    assert_eq!(st.lock().unwrap().fee_refunds.len(), 1);

    // a second order the shop turns down before production: refunded
    sqlx::query("DELETE FROM mf_shop_ledger")
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE mf_shop_orders SET status = 'received', refund_due = false, refunded_at = NULL, fee_returned_at = NULL",
    )
    .execute(&s.pool)
    .await
    .unwrap();
    st.lock().unwrap().refunds.clear();
    deliver(from_shop(
        "order.cancel",
        2,
        &r,
        1,
        json!({"by": "shop", "why": "Out of navy tees."}),
    ));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(
        page.contains("The shop cancelled this order: Out of navy tees."),
        "{page}"
    );
    assert_eq!(st.lock().unwrap().refunds.len(), 1);
    // and after production starts, a refund is the shop's to decide
    sqlx::query("DELETE FROM mf_shop_ledger")
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE mf_shop_orders SET status = 'in_production', cancelled_by = NULL, refund_due = false, refunded_at = NULL, fee_returned_at = NULL")
        .execute(&s.pool)
        .await
        .unwrap();
    st.lock().unwrap().refunds.clear();
    deliver(from_shop(
        "order.cancel",
        3,
        &r,
        2,
        json!({"by": "shop", "why": "The machine broke."}),
    ));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    assert!(st.lock().unwrap().refunds.is_empty());
    assert!(
        s.get(&path, Some(&bea))
            .await
            .body
            .contains("a refund is the shop's to decide")
    );
}

#[tokio::test]
async fn an_order_the_hub_refuses_is_refunded() {
    let Some((s, state, hub, st)) = open_shop().await else {
        return;
    };
    // the hub doesn't know MakerFoundry's key: order.placed is refused (403), for good
    hub.lock().unwrap().public_key = None;
    let (bea, path, _) = paid_order(&s, &st).await;
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert!(sent_of(&hub, "order.placed").is_empty());
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_hub_outbox WHERE kind = 'order.placed' AND failed LIKE '%403%'")
            .await,
        1
    );
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(
        page.contains("The shop couldn't take the order, so it's refunded") && page.contains("Refunded: $243.49"),
        "{page}"
    );
    assert_eq!(st.lock().unwrap().refunds.len(), 1);
    // and only once
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    assert_eq!(st.lock().unwrap().refunds.len(), 1);
}

#[tokio::test]
async fn unpaid_orders_are_given_up_and_a_customers_data_is_exported_and_erased() {
    let Some((s, state, _, st)) = open_shop().await else {
        return;
    };
    let (bea, path, _) = paid_order(&s, &st).await;
    let pes = sample(sf_formats::Format::Pes);
    let unpaid = s
        .send(order_form(TEE, &tee_order(), Some(("logo.pes", &pes)), &bea))
        .await
        .location()
        .to_string();
    assert_eq!(s.stored.lock().unwrap().len(), 2);
    // a day old: it can't be paid any more; two days: given up and its art deleted
    sqlx::query("UPDATE mf_shop_orders SET created_at = now() - interval '21 hours' WHERE status = 'pending'")
        .execute(&s.pool)
        .await
        .unwrap();
    assert!(
        s.post(&format!("{unpaid}/pay"), "", Some(&bea))
            .await
            .body
            .contains("over a day old")
    );
    assert_eq!(st.lock().unwrap().sessions.len(), 1);
    sqlx::query("UPDATE mf_shop_orders SET created_at = now() - interval '3 days' WHERE status = 'pending'")
        .execute(&s.pool)
        .await
        .unwrap();
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'abandoned'")
            .await,
        1
    );
    assert_eq!(s.stored.lock().unwrap().len(), 1, "only the paid order's art is kept");
    // the customer's data: exported, then erased (the paid order keeps its amounts)
    let export = s.get("/account/data.json", Some(&bea)).await.body;
    for want in [
        "\"shop_orders\"",
        "Main Street Threadworks",
        "12 Oak Street",
        "logo.pes",
        "24349",
    ] {
        assert!(export.contains(want), "{want}: {export}");
    }
    // not while the paid order is still with the shop: it needs the address and art
    let r = s.post("/account/data/delete", "confirm=delete", Some(&bea)).await;
    assert!(r.body.contains("still in progress"), "{}", r.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_orders").await, 2);
    // the shop sent it, so the hub keeps a copy of order.placed: blanked with the rest
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    sqlx::query("UPDATE mf_shop_orders SET status = 'shipped' WHERE status <> 'abandoned'")
        .execute(&s.pool)
        .await
        .unwrap();
    s.post("/account/data/delete", "confirm=delete", Some(&bea)).await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders").await,
        1,
        "the unpaid one went"
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_hub_outbox WHERE kind = 'order.placed' AND envelope->'body' = '{}'::jsonb")
            .await,
        1,
        "the hub's copy is blank"
    );
    // the order still reads (for its amounts and any refund), with the address gone
    let reference = path.trim_start_matches("/shop-orders/").to_string();
    let mut conn = s.pool.acquire().await.unwrap();
    let kept = makerfoundry::shop_orders::orders::order(&mut conn, &reference)
        .await
        .expect("the erased order still loads");
    assert!(kept.customer.ship_to.is_none());
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE customer_name = '' AND ship_to IS NULL AND art = '[]'::jsonb AND paid_cents = 24349")
            .await,
        1
    );
    assert!(s.stored.lock().unwrap().is_empty());
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_ledger").await,
        3,
        "the amounts are kept"
    );
}

/// What was posted to the fake Stripe at `path`.
fn posted(st: &StripeShared, path: &str) -> Vec<HashMap<String, String>> {
    st.lock()
        .unwrap()
        .posts
        .iter()
        .filter(|(p, _)| p == path)
        .map(|(_, f)| f.clone())
        .collect()
}

async fn calc_id(s: &Site) -> String {
    sqlx::query_scalar("SELECT tax_calculation FROM mf_shop_orders")
        .fetch_one(&s.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn sales_tax_on_a_shop_order_is_makerfoundrys_and_comes_back_with_a_refund() {
    let Some((s, state, hub, st)) = open_shop_taxed(true).await else {
        return;
    };
    let (bea, path, r) = paid_order(&s, &st).await;
    // worked out before checkout, from where it ships, under the clothing tax code
    let calc = posted(&st, "/v1/tax/calculations")[0].clone();
    assert_eq!(calc["line_items[0][amount]"], "22900");
    assert_eq!(calc["line_items[0][tax_code]"], "txcd_30011000");
    assert_eq!(calc["line_items[0][tax_behavior]"], "exclusive");
    assert_eq!(calc["shipping_cost[amount]"], "1449");
    assert_eq!(calc["customer_details[address][line1]"], "12 Oak Street");
    assert_eq!(calc["customer_details[address][state]"], "OH");
    assert_eq!(calc["customer_details[address][postal_code]"], "44000");
    // $243.49 + 7% of $229.00; the 10% of the price before tax, the card fee of all that's paid
    let amounts: (i64, i64, i64, i64, i64) =
        sqlx::query_as("SELECT tax_cents, paid_cents, fee_cents, card_fee_cents, shop_cents FROM mf_shop_orders")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(amounts, (1_603, 25_952, 2_435, 783, 21_131));
    // the tax goes to MakerFoundry inside the application fee
    let session = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    assert_eq!(session["line_items[0][price_data][unit_amount]"], "25952");
    assert_eq!(
        session["payment_intent_data[application_fee_amount]"],
        (2_435 + 783 + 1_603).to_string()
    );
    assert_eq!(
        s.scalar("SELECT cents FROM mf_shop_ledger WHERE kind = 'tax' AND account_id = 0")
            .await,
        1_603
    );
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_shop_ledger").await, 25_952);
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(
        page.contains("Sales tax") && page.contains("$16.03") && page.contains("$259.52"),
        "{page}"
    );
    // the shop is told what was paid, the tax outside its share
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let placed = sent_of(&hub, "order.placed").remove(0);
    assert_eq!(placed["body"]["totals"]["tax_cents"], 1_603);
    assert_eq!(placed["body"]["totals"]["shop_gets_cents"], 21_131);
    // recorded with Stripe for its tax reports, once
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    let recorded = posted(&st, "/v1/tax/transactions/create_from_calculation");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["calculation"], calc_id(&s).await);
    assert_eq!(recorded[0]["reference"], r);

    // cancelled: the customer gets it all back, the tax transaction is reversed, and the shop is
    // still out only the card fee
    s.post(&format!("{path}/cancel"), "reason=late", Some(&bea)).await;
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    let reversal = posted(&st, "/v1/tax/transactions/create_reversal");
    assert_eq!(reversal.len(), 1);
    assert_eq!(reversal[0]["mode"], "full");
    assert_eq!(
        reversal[0]["original_transaction"],
        format!("tax_test_{}", r.replace('-', "_"))
    );
    assert_eq!(reversal[0]["reference"], format!("{r}-refund"));
    assert_eq!(
        st.lock().unwrap().fee_refunds[0].1["amount"],
        (2_435 + 1_603).to_string()
    );
    assert_eq!(s.scalar("SELECT sum(cents)::bigint FROM mf_shop_ledger").await, 0);
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_shop_ledger WHERE kind = 'tax'")
            .await,
        0
    );
    assert_eq!(
        s.scalar("SELECT sum(cents)::bigint FROM mf_shop_ledger WHERE kind = 'shop'")
            .await,
        -783
    );
    assert!(s.get(&path, Some(&bea)).await.body.contains("Refunded: $259.52"));
}

#[tokio::test]
async fn a_pick_up_order_is_taxed_where_the_shop_is_and_needs_its_address() {
    let Some((s, state, hub, st)) = open_shop_taxed(true).await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    let pes = sample(sf_formats::Format::Pes);
    let mut pickup = tee_order();
    pickup.retain(|(k, _)| *k != "delivery");
    pickup.push(("delivery", "pickup"));
    let path = s
        .send(order_form(TEE, &pickup, Some(("logo.pes", &pes)), &bea))
        .await
        .location()
        .to_string();
    assert!(path.starts_with("/shop-orders/"), "{path}");
    let calc = posted(&st, "/v1/tax/calculations")[0].clone();
    assert_eq!(calc["customer_details[address][line1]"], "210 Main Street");
    assert_eq!(calc["customer_details[address][postal_code]"], "44001");
    assert!(!calc.contains_key("shipping_cost[amount]"));
    // a shop that hasn't given its address can only ship while tax is collected
    let mut again = catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c009");
    again["seq"] = 2.into();
    again["body"]["shop"].as_object_mut().unwrap().remove("address");
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, again);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let a = s.send(order_form(TEE, &pickup, Some(("logo.pes", &pes)), &bea)).await;
    assert!(a.body.contains("can&#39;t take pick-up orders yet"), "{}", a.body);
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_orders").await, 1);
}

// ---------------------------------------------------------------- an admin deleting an account's data (#55)

#[tokio::test]
async fn an_admin_deletes_an_accounts_data_for_someone_who_cannot_sign_in() {
    // the first account made is the admin (1000); Sam (1001) has a published design and a draft
    let Some(s) = site_with(vec![1000]).await else { return };
    let admin = s.sign_up("admin%40example.com").await;
    let sam = s.sign_up("sam%40example.com").await;
    let owl = published(&s, &sam, "sams-shop", "Owl").await;
    let draft = add_design(&s, &sam, sf_formats::Format::Pes, "draft.pes").await;
    let other = s.sign_up("other%40example.com").await;

    // only admins, and only from our own pages
    assert_eq!(
        s.get("/admin/accounts", Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.post("/admin/accounts/1001/delete", "confirm=1001", Some(&other))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.get("/admin/accounts", None).await.status, StatusCode::NOT_FOUND);
    let req = Request::post("/admin/accounts/1001/delete")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "https://evil.example")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, &admin)
        .body(Body::from("confirm=1001"))
        .unwrap();
    assert_eq!(s.send(req).await.status, StatusCode::FORBIDDEN);

    // the look-up shows what's there
    let page = s.get("/admin/accounts?account=1001", Some(&admin)).await.body;
    assert!(
        page.contains("Account 1001") && page.contains("sam@example.com") && page.contains("2 (1 published)"),
        "{page}"
    );
    s.dump_page("admin-accounts", &page, Some(&admin)).await;
    assert!(
        s.get("/admin/accounts?account=99999", Some(&admin))
            .await
            .body
            .contains("no account with that number")
    );
    assert!(
        s.get("/admin/accounts?account=x1", Some(&admin))
            .await
            .body
            .contains("digits only")
    );
    // an admin's account isn't deleted from here, their own included
    assert!(
        s.get("/admin/accounts?account=1000", Some(&admin))
            .await
            .body
            .contains("admin's account")
    );
    s.post("/admin/accounts/1000/delete", "confirm=1000", Some(&admin))
        .await;
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_accounts WHERE account_id = 1000 AND email <> ''")
            .await,
        1
    );

    // the number must be typed
    let stored = s.stored.lock().unwrap().len();
    let r = s
        .post("/admin/accounts/1001/delete", "confirm=1002", Some(&admin))
        .await;
    assert!(r.body.contains("Type the account&#39;s number"), "{}", r.body);
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_designs WHERE id = {owl}"))
            .await,
        1
    );

    // done: the same as Sam's own delete, files included, and logged
    let r = s
        .post("/admin/accounts/1001/delete", "confirm=1001", Some(&admin))
        .await;
    assert!(
        r.body.contains("Account 1001&#39;s data is deleted") || r.body.contains("Account 1001's data is deleted"),
        "{}",
        r.body
    );
    assert!(s.stored.lock().unwrap().len() < stored, "files gone");
    for (sql, n) in [
        (format!("SELECT count(*) FROM mf_designs WHERE id IN ({owl}, {draft})"), 0),
        ("SELECT count(*) FROM mf_makers WHERE account_id = 1001".to_string(), 0),
        ("SELECT count(*) FROM mf_sessions WHERE account_id = 1001".to_string(), 0),
        ("SELECT count(*) FROM mf_accounts WHERE account_id = 1001 AND email = ''".to_string(), 1),
        (
            "SELECT count(*) FROM mf_moderation_log WHERE admin_account = 1000 AND action = 'delete-account-data' AND note LIKE 'account 1001,%'"
                .to_string(),
            1,
        ),
    ] {
        assert_eq!(s.scalar(&sql).await, n, "{sql}");
    }
    // Sam's session no longer works; the public page is gone
    assert_eq!(s.get("/account", Some(&sam)).await.location(), "/signin");
    assert_eq!(s.get(&format!("/d/{owl}"), None).await.status, StatusCode::NOT_FOUND);
    // and it's on the admins' overview
    assert!(
        s.get("/admin/overview", Some(&admin))
            .await
            .body
            .contains(r#"href="/admin/accounts""#)
    );
}

#[tokio::test]
async fn a_shop_is_offered_only_once_the_account_it_names_confirms_it() {
    // (code review, 2026-11-01: a catalogue could name anyone's account to be paid)
    let Some((s, state, hub, _)) = shops_site().await else {
        return;
    };
    let ann = s.sign_up("ann%40example.com").await; // account 1000
    let mut c = catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c001");
    c["body"]["shop"]["account"] = 1000.into();
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, c);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    sqlx::query(
        "INSERT INTO mf_sellers (account_id, stripe_account, details_submitted, charges_enabled, payouts_enabled)
         VALUES (1000, 'acct_ann', true, true, true)",
    )
    .execute(&s.pool)
    .await
    .unwrap();
    // payable, but not confirmed: not offered
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::NOT_FOUND);
    let account = s.get("/account", Some(&ann)).await.body;
    assert!(
        account.contains("Main Street Threadworks") && account.contains("isn&#39;t confirmed yet")
            || account.contains("isn't confirmed yet"),
        "{account}"
    );
    // someone else can't confirm it
    let bob = s.sign_up("bob%40example.com").await;
    assert_eq!(
        s.post("/account/shops/mainst/confirm", "", Some(&bob)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::NOT_FOUND);
    // its account confirms it: offered
    assert_eq!(
        s.post("/account/shops/mainst/confirm", "", Some(&ann)).await.location(),
        "/account"
    );
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::OK);
    // a new catalogue naming another account: not offered until that one confirms
    let mut c = catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c002");
    c["seq"] = 2.into();
    c["body"]["shop"]["account"] = 1001.into();
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, c);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shops WHERE account_confirmed").await,
        0
    );
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::NOT_FOUND);
    // and a shop's account can say it isn't theirs
    sqlx::query("UPDATE mf_shops SET account_id = 1000, account_confirmed = true")
        .execute(&s.pool)
        .await
        .unwrap();
    s.post("/account/shops/mainst/disown", "", Some(&ann)).await;
    assert_eq!(s.get("/shops/mainst", None).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_shops_status_never_takes_an_order_back() {
    // (code review, 2026-11-01: "received" after "shipped" made the order cancellable again)
    let Some((s, state, hub, st)) = open_shop().await else {
        return;
    };
    let (bea, path, r) = paid_order(&s, &st).await;
    let deliver = |m: Value| makerfoundry::shop_orders::hub::stand_in::deliver(&hub, m);
    deliver(from_shop(
        "order.status",
        1,
        &r,
        1,
        json!({"status": "shipped", "tracking": "1Z1"}),
    ));
    deliver(from_shop("order.status", 2, &r, 2, json!({"status": "received"})));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'shipped'")
            .await,
        1
    );
    assert!(!s.get(&path, Some(&bea)).await.body.contains("Cancel the order"));
    // between proofs it may go either way
    assert!(makerfoundry::shop_orders::after::moves_on("approved", "proof_sent"));
    assert!(makerfoundry::shop_orders::after::moves_on("proof_sent", "approved"));
    assert!(!makerfoundry::shop_orders::after::moves_on("in_production", "approved"));
    assert!(
        makerfoundry::shop_orders::after::moves_on("shipped", "shipped"),
        "a tracking update"
    );
}

#[tokio::test]
async fn a_payment_its_order_cant_take_is_refunded_and_no_second_checkout_is_made_blind() {
    // (code review, 2026-11-01)
    let Some((s, state, _, st)) = open_shop().await else {
        return;
    };
    let (_, _, r) = paid_order(&s, &st).await;
    // Stripe says another checkout for the same order was paid too: kept and refunded in full
    let f = st.lock().unwrap().sessions["cs_test_1"].0.clone();
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_stray",
        "checkout.session.completed",
        session_json("cs_test_9", &f, true),
    )
    .await;
    // (and the order's own payment, told twice, is not)
    s.stripe_event(
        WEBHOOK_SECRET,
        "evt_again",
        "checkout.session.completed",
        session_json("cs_test_1", &f, true),
    )
    .await;
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_shop_stray_payments WHERE payment_intent = 'pi_cs_test_9' AND ref = '{r}'"
        ))
        .await,
        1
    );
    assert_eq!(s.scalar("SELECT count(*) FROM mf_shop_stray_payments").await, 1);
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    let refunds = st.lock().unwrap().refunds.clone();
    let stray: Vec<_> = refunds
        .iter()
        .filter(|f| f.get("payment_intent").map(String::as_str) == Some("pi_cs_test_9"))
        .collect();
    assert_eq!(stray.len(), 1, "{refunds:?}");
    assert_eq!(stray[0]["reverse_transfer"], "true");
    assert_eq!(stray[0]["refund_application_fee"], "true");
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_stray_payments WHERE refunded_at IS NOT NULL")
            .await,
        1
    );
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    assert_eq!(st.lock().unwrap().refunds.len(), refunds.len(), "refunded once");

    // an unpaid order whose open checkout Stripe can't be asked about: no second checkout
    let bea = s.sign_up("cy%40example.com").await;
    let pes = sample(sf_formats::Format::Pes);
    let path = s
        .send(order_form(TEE, &tee_order(), Some(("logo.pes", &pes)), &bea))
        .await
        .location()
        .to_string();
    let first = s
        .post(&format!("{path}/pay"), "", Some(&bea))
        .await
        .location()
        .to_string();
    let before = st.lock().unwrap().sessions.len();
    st.lock().unwrap().sessions_unreadable = true;
    let page = s.post(&format!("{path}/pay"), "", Some(&bea)).await.body;
    assert!(page.contains("Stripe can"), "{page}");
    assert_eq!(st.lock().unwrap().sessions.len(), before, "no second checkout");
    st.lock().unwrap().sessions_unreadable = false;
    assert_eq!(
        s.post(&format!("{path}/pay"), "", Some(&bea)).await.location(),
        first,
        "the same one"
    );
    // one Stripe no longer has: a new one is made
    sqlx::query("UPDATE mf_shop_orders SET stripe_session = 'cs_unknown' WHERE status = 'pending'")
        .execute(&s.pool)
        .await
        .unwrap();
    assert!(
        s.post(&format!("{path}/pay"), "", Some(&bea))
            .await
            .location()
            .starts_with("https://checkout.stripe.com/")
    );
    assert_eq!(st.lock().unwrap().sessions.len(), before + 1);
}

// ---------------------------------------------------------------- picks of the week and galleries

#[tokio::test]
async fn picks_of_the_week_and_the_galleries_on_the_home_page() {
    // the first account made is the admin (1000)
    let Some(s) = site_with(vec![1000]).await else { return };
    let admin = s.sign_up("admin%40example.com").await;
    // nothing published yet: no picks, no galleries
    let home = s.get("/", None).await.body;
    assert!(!home.contains("Picks of the week") && !home.contains("Made by the community"));
    let maker = s.sign_up("maker%40example.com").await;
    let rose = published(&s, &maker, "maker", "Rose").await;
    let fern = published(&s, &maker, "maker", "Fern").await;

    // no admin pick: the week's most liked and made, then most downloaded, then newest (Fern),
    // kept for the week
    let home = s.get("/", None).await.body;
    assert!(
        home.contains("Picks of the week") && home.contains("Embroidery pick"),
        "{home}"
    );
    let picks = home
        .split("Picks of the week")
        .nth(1)
        .unwrap()
        .split("</section>")
        .next()
        .unwrap()
        .to_string();
    assert!(picks.contains("Fern") && !picks.contains("Rose"), "{picks}");
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_picks WHERE design_id = {fern} AND picked_by IS NULL"
        ))
        .await,
        1
    );
    s.dump_page("home-picks", &home, None).await;
    // Rose downloaded: it doesn't change this week's pick, and shows as most downloaded
    let other = s.sign_up("other%40example.com").await;
    s.get(&format!("/d/{rose}/download"), Some(&other)).await;
    let home = s.get("/", None).await.body;
    let picks = home
        .split("Picks of the week")
        .nth(1)
        .unwrap()
        .split("</section>")
        .next()
        .unwrap()
        .to_string();
    assert!(picks.contains("Fern"), "kept for the week: {picks}");
    assert!(home.contains("Most downloaded"), "{home}");

    // an admin picks Rose from its page; nobody else can
    assert!(
        !s.get(&format!("/d/{rose}"), Some(&other))
            .await
            .body
            .contains("of the week</button>")
    );
    let page = s.get(&format!("/d/{rose}"), Some(&admin)).await.body;
    assert!(page.contains("Make this the embroidery pick of the week"), "{page}");
    assert_eq!(
        s.post(&format!("/d/{rose}/pick"), "", Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    let req = Request::post(format!("/d/{rose}/pick"))
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, &admin)
        .body(Body::empty())
        .unwrap();
    assert_eq!(s.send(req).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        s.post(&format!("/d/{rose}/pick"), "", Some(&admin)).await.location(),
        format!("/d/{rose}")
    );
    let home = s.get("/", None).await.body;
    let picks = home
        .split("Picks of the week")
        .nth(1)
        .unwrap()
        .split("</section>")
        .next()
        .unwrap()
        .to_string();
    assert!(picks.contains("Rose") && !picks.contains("Fern"), "{picks}");
    assert!(
        s.get(&format!("/d/{rose}"), None)
            .await
            .body
            .contains("Embroidery pick of the week of")
    );
    let past = s.get("/picks", None).await;
    assert_eq!(past.status, StatusCode::OK);
    assert!(
        past.body.contains("The week of") && past.body.contains("Rose"),
        "{}",
        past.body
    );
    s.dump_page("picks", &past.body, None).await;

    // what people made: a gallery of their photos
    s.post("/account/maker", "name=Other&handle=other", Some(&other)).await;
    s.send(multipart(
        &format!("/d/{fern}/makes"),
        &[
            ("photo", "a.jpg", photo_with_location()),
            ("confirm", "", b"yes".to_vec()),
        ],
        &other,
    ))
    .await;
    let home = s.get("/", None).await.body;
    assert!(
        home.contains("Made by the community") && home.contains("/makes/"),
        "{home}"
    );
    s.dump_page("home-galleries", &home, None).await;

    // the week's pick taken down: the next best takes its place
    sqlx::query(&format!("UPDATE mf_designs SET status = 'removed' WHERE id = {rose}"))
        .execute(&s.pool)
        .await
        .unwrap();
    let home = s.get("/", None).await.body;
    let picks = home
        .split("Picks of the week")
        .nth(1)
        .unwrap()
        .split("</section>")
        .next()
        .unwrap()
        .to_string();
    assert!(picks.contains("Fern") && !picks.contains("Rose"), "{picks}");
    assert!(!s.get("/picks", None).await.body.contains("Rose"));
}

// ---------------------------------------------------------------- lower-detail previews of paid designs

impl Site {
    /// A GET's status, headers and body as bytes (for pictures and meshes).
    async fn raw(&self, path: &str, cookie: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut req = Request::get(path).header(header::HOST, HOST);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        (
            status,
            headers,
            resp.into_body().collect().await.unwrap().to_bytes().to_vec(),
        )
    }
}

/// `buyer` (an account id) bought design `id` from `seller`.
async fn sold(s: &Site, id: i64, seller: i64, buyer: i64) {
    sqlx::query(
        "INSERT INTO mf_orders (buyer_account, design_id, seller_account, price_cents, fee_cents,
             processing_cents, seller_cents, title, status, paid_at)
         VALUES ($1, $2, $3, 500, 50, 45, 405, 'Paid', 'paid', now())",
    )
    .bind(buyer)
    .bind(id)
    .bind(seller)
    .execute(&s.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_paid_designs_page_shows_others_only_a_lower_detail_picture() {
    let Some(s) = site().await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let stitches = published(&s, &ann, "anns", "Rose").await;
    // a laser design too
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="60mm" height="40mm" viewBox="0 0 60 40"><rect x="5" y="5" width="50" height="30" fill="none" stroke="#ff0000"/><circle cx="30" cy="20" r="7" fill="none" stroke="#0000ff"/></svg>"##;
    let r = s
        .send(multipart(
            "/designs/new",
            &[("file", "coaster.svg", svg.to_vec())],
            &ann,
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let lines = design_id(r.location());
    s.post(
        &format!("/designs/{lines}/edit"),
        "title=Coaster&licence=commercial",
        Some(&ann),
    )
    .await;
    s.post(&format!("/designs/{lines}/publish"), "confirm=yes", Some(&ann))
        .await;
    let svgs: Vec<String> = sqlx::query_scalar("SELECT preview_svg FROM mf_designs WHERE id = ANY($1) ORDER BY id")
        .bind(vec![stitches, lines])
        .fetch_all(&s.pool)
        .await
        .unwrap();
    let (rose_svg, coaster_svg) = (svgs[0].clone(), svgs[1].clone());
    assert!(rose_svg.contains("<path") && coaster_svg.contains("<path"));

    // free: everyone sees every stitch and line, as before
    for (id, svg) in [(stitches, &rose_svg), (lines, &coaster_svg)] {
        let page = s.get(&format!("/d/{id}"), None).await.body;
        assert!(page.contains(svg.as_str()), "{page}");
        assert!(!page.contains("preview.png"), "{page}");
    }

    sqlx::query("UPDATE mf_designs SET price_cents = 500 WHERE id = ANY($1)")
        .bind(vec![stitches, lines])
        .execute(&s.pool)
        .await
        .unwrap();
    let bea = s.sign_up("bea%40example.com").await;
    let cal = s.sign_up("cal%40example.com").await;
    sold(&s, stitches, 1000, 1001).await;
    sold(&s, lines, 1000, 1001).await;
    for (id, svg) in [(stitches, &rose_svg), (lines, &coaster_svg)] {
        // nobody who hasn't bought it gets the stitches or lines: no path, no viewBox of them
        for viewer in [None, Some(cal.as_str())] {
            let page = s.get(&format!("/d/{id}"), viewer).await.body;
            let figure = page
                .split(r#"<figure class="preview">"#)
                .nth(1)
                .and_then(|f| f.split("</figure>").next())
                .unwrap();
            assert!(
                !figure.contains("<svg") && !figure.contains("<path"),
                "{viewer:?}: {figure}"
            );
            // nothing of the stitches or lines anywhere on the page
            let first_path = svg.split(" d=\"").nth(1).unwrap().split('"').next().unwrap();
            assert!(!page.contains(first_path), "{page}");
            assert!(!page.contains(r#"class="stitches" viewBox"#), "{page}");
            assert!(
                page.contains(&format!(r#"<img class="stitches" src="/d/{id}/preview.png""#)),
                "{page}"
            );
            assert!(page.contains("A lower-detail picture."), "{page}");
            let (status, headers, png) = s.raw(&format!("/d/{id}/preview.png"), viewer).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "image/png");
            let img = image::load_from_memory(&png).unwrap();
            assert!(img.width().max(img.height()) <= makerfoundry::coarse::MAX_SIDE);
        }
        // the buyer, the maker and the admins see every stitch
        for viewer in [&bea, &ann] {
            let page = s.get(&format!("/d/{id}"), Some(viewer)).await.body;
            assert!(page.contains(svg.as_str()), "{page}");
        }
    }
    s.dump_page(
        "paid-preview-anonymous",
        &s.get(&format!("/d/{stitches}"), None).await.body,
        None,
    )
    .await;
    s.dump_page(
        "paid-preview-laser",
        &s.get(&format!("/d/{lines}"), None).await.body,
        None,
    )
    .await;
    s.dump_page(
        "paid-preview-buyer",
        &s.get(&format!("/d/{stitches}"), Some(&bea)).await.body,
        Some(&bea),
    )
    .await;
    // an unpublished design's picture is no one else's
    s.post(&format!("/designs/{lines}/unpublish"), "", Some(&ann)).await;
    assert_eq!(
        s.raw(&format!("/d/{lines}/preview.png"), Some(&cal)).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_paid_models_mesh_is_rough_for_people_who_havent_bought_it() {
    let Some(s) = site_3d(Vec::new(), true).await else {
        return;
    };
    let (ann, id) = published_model(&s, "ann%40example.com", "ann").await;
    let fid: i64 = sqlx::query_scalar("SELECT id FROM mf_model_files WHERE design_id = $1 AND role = 'mesh'")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let path = format!("/3d/{id}/files/{fid}/mesh");
    let (_, headers, free) = s.raw(&path, None).await;
    assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=3600");
    // the whole mesh reaches the box's corners (quantized 0 and 65535)
    let corners = |m: &[u8]| -> Vec<u16> {
        m[32..]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect()
    };
    assert!(corners(&free).contains(&0) && corners(&free).contains(&65535));

    sqlx::query("UPDATE mf_designs SET price_cents = 500 WHERE id = $1")
        .bind(id)
        .execute(&s.pool)
        .await
        .unwrap();
    let bea = s.join("bea%40example.com").await;
    let cal = s.join("cal%40example.com").await;
    sold(&s, id, 1000, 1001).await;
    for viewer in [None, Some(cal.as_str())] {
        let (status, headers, rough) = s.raw(&path, viewer).await;
        assert_eq!(status, StatusCode::OK);
        assert!(rough.starts_with(b"MFM1"));
        assert_ne!(rough, free, "{viewer:?}");
        let c = corners(&rough);
        assert!(!c.is_empty() && !c.contains(&0) && !c.contains(&65535), "{c:?}");
        assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=3600");
    }
    for viewer in [&bea, &ann] {
        let (status, headers, whole) = s.raw(&path, Some(viewer)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(whole, free);
        assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=3600");
    }
}

// ---------------------------------------------------------------- branded shops (2026-11-02)

/// A request to `path` at `host` (a shop's short address).
fn at_host(host: &str, path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::HOST, host)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn a_shop_has_its_own_branded_page_and_short_address() {
    let Some((s, state, hub, _)) = shops_site().await else {
        return;
    };
    let png = mockup_png();
    let sha = makerfoundry::shop_orders::hub::stand_in::put_file(&hub, "catalog", &png);
    let mut c = catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c001");
    c["body"]["shop"]["brand"] = json!({
        "display_name": "Main Street Threads",
        "intro": "Family shop since 1998.\n\nWe stitch every order here in <Elmwood>.",
        "logo": {"size": png.len(), "sha256": sha, "type": "image/png"},
        "accent": "#1f5fad",
        "handle": "mainst-threads",
        "website": "https://mainst.example/"
    });
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, c);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    shop_can_be_paid(&s).await;
    // the logo was fetched from the hub, drawn again and kept
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shops WHERE logo_key IS NOT NULL AND logo_type = 'image/png'")
            .await,
        1
    );

    let page = s.get("/shops/mainst", None).await.body;
    for want in [
        "<h1>Main Street Threads</h1>",
        r#"src="/shops/mainst/logo""#,
        r#"href="/shops/mainst/brand.css""#,
        "<p>Family shop since 1998.</p><p>We stitch every order here in &lt;Elmwood&gt;.</p>",
        r#"href="https://mainst.example/" rel="noopener nofollow ugc""#,
        r#"href="https://mainst-threads.makerfoundry.test/""#,
        "Sold by Main Street Threads through MakerFoundry",
        "Heavy cotton tee",
    ] {
        assert!(page.contains(want), "{want}: {page}");
    }
    s.dump_page("shop-branded", &page, None).await;
    let logo = s.get("/shops/mainst/logo", None).await;
    assert_eq!(logo.status, StatusCode::OK);
    assert_eq!(logo.headers[header::CONTENT_TYPE], "image/png");
    let css = s.get("/shops/mainst/brand.css", None).await;
    assert!(css.body.contains("--accent: #1f5fad"), "{}", css.body);

    // its short address: the page, with its stylesheet and logo from the same address
    let r = s.send(at_host("mainst-threads.makerfoundry.test", "/")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body.contains(r#"href="/brand.css""#) && r.body.contains(r#"src="/logo""#),
        "{}",
        r.body
    );
    assert!(r.headers.contains_key(header::CONTENT_SECURITY_POLICY));
    assert_eq!(
        s.send(at_host("mainst-threads.makerfoundry.test", "/logo"))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        s.send(at_host("mainst-threads.makerfoundry.test", "/static/site.css"))
            .await
            .status,
        StatusCode::OK
    );
    // anything else, ordering included, goes to MakerFoundry itself
    let r = s
        .send(at_host(
            "mainst-threads.makerfoundry.test",
            "/shops/mainst/order/tee-heavy?x=1",
        ))
        .await;
    assert_eq!(r.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        r.location(),
        "https://makerfoundry.test/shops/mainst/order/tee-heavy?x=1"
    );
    assert_eq!(
        s.send(at_host("www.makerfoundry.test", "/browse")).await.location(),
        "https://makerfoundry.test/browse"
    );
    assert_eq!(
        s.send(at_host("nobody.makerfoundry.test", "/")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.send(at_host("a.b.makerfoundry.test", "/")).await.status,
        StatusCode::NOT_FOUND
    );
    // MakerFoundry itself is unchanged
    assert_eq!(s.send(at_host(HOST, "/")).await.status, StatusCode::OK);

    // another shop can't take the same address, nor a reserved one; bad branding is left out
    let mut other = catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c099");
    other["from"] = json!("shop:other");
    other["body"]["shop"]["brand"] =
        json!({"handle": "mainst-threads", "accent": "pink", "website": "javascript:alert(1)"});
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, other);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shops WHERE party = 'shop:other' AND subdomain IS NULL")
            .await,
        1
    );
    let mut again = catalog_message("0192f0c4-7a1b-7c2d-8e3f-00000000c002");
    again["seq"] = 2.into();
    again["body"]["shop"]["brand"] = json!({"handle": "admin"});
    makerfoundry::shop_orders::hub::stand_in::deliver(&hub, again);
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    // (no handle any more: the short address is let go, and the logo with it)
    assert_eq!(
        s.scalar(
            "SELECT count(*) FROM mf_shops WHERE party = 'shop:mainst' AND subdomain IS NULL AND logo_key IS NULL"
        )
        .await,
        1
    );
    assert_eq!(
        s.send(at_host("mainst-threads.makerfoundry.test", "/")).await.status,
        StatusCode::NOT_FOUND
    );
    let page = s.get("/shops/mainst", None).await.body;
    assert!(
        page.contains("<h1>Main Street Threadworks</h1>") && !page.contains("brand.css"),
        "{page}"
    );
}

// ---------------------------------------------------------------- a purchase stays the buyer's

#[tokio::test]
async fn a_buyer_keeps_a_design_its_seller_makes_free_or_unpublishes() {
    let Some(s) = site().await else { return };
    let ann = s.sign_up("ann%40example.com").await;
    let id = published(&s, &ann, "anns", "Rose").await;
    let bea = s.sign_up("bea%40example.com").await;
    let cal = s.sign_up("cal%40example.com").await;
    sqlx::query("UPDATE mf_designs SET price_cents = 500 WHERE id = $1")
        .bind(id)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO mf_orders (buyer_account, design_id, seller_account, price_cents, fee_cents,
             processing_cents, seller_cents, title, status, paid_at)
         VALUES (1001, $1, 1000, 500, 50, 45, 405, 'Rose', 'paid', now())",
    )
    .bind(id)
    .execute(&s.pool)
    .await
    .unwrap();
    let downloads =
        |status: StatusCode, location: &str| status == StatusCode::SEE_OTHER && location.contains("X-Amz-Signature=");
    let dl = s.get(&format!("/d/{id}/download"), Some(&bea)).await;
    assert!(downloads(dl.status, dl.location()), "{}", dl.location());

    // the seller makes it free, then unpublishes it: the buyer still has it, nobody else does
    let r = s
        .post(
            &format!("/designs/{id}/edit"),
            "title=Rose&licence=commercial&price=0",
            Some(&ann),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    s.post(&format!("/designs/{id}/unpublish"), "", Some(&ann)).await;
    let (price, status): (i64, String) = sqlx::query_as("SELECT price_cents, status FROM mf_designs WHERE id = $1")
        .bind(id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!((price, status.as_str()), (0, "draft"));
    let dl = s.get(&format!("/d/{id}/download"), Some(&bea)).await;
    assert!(downloads(dl.status, dl.location()), "{:?} {}", dl.status, dl.location());
    assert!(
        s.get("/library", Some(&bea))
            .await
            .body
            .contains(&format!(r#"href="/d/{id}/download""#))
    );
    for other in [None, Some(cal.as_str())] {
        assert_eq!(
            s.get(&format!("/d/{id}/download"), other).await.status,
            StatusCode::NOT_FOUND
        );
    }

    // a takedown (copyright) does take it away
    sqlx::query("UPDATE mf_designs SET status = 'removed', removed_reason = 'copyright' WHERE id = $1")
        .bind(id)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        s.get(&format!("/d/{id}/download"), Some(&bea)).await.status,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------- deleting a design being bought

#[tokio::test]
async fn deleting_a_design_closes_its_checkouts_first() {
    let Some((s, st, seller, id)) = selling_site(Vec::new()).await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    let cal = s.sign_up("cal%40example.com").await;
    for buyer in [&bea, &cal] {
        let r = s.post(&format!("/d/{id}/buy"), "", Some(buyer)).await;
        assert!(
            r.location().starts_with("https://checkout.stripe.com/"),
            "{}",
            r.location()
        );
    }
    // cal has paid at Stripe, and it isn't confirmed here yet: the design waits
    st.lock().unwrap().sessions.get_mut("cs_test_2").unwrap().1 = true;
    s.post(&format!("/designs/{id}/unpublish"), "", Some(&seller)).await;
    let r = s.post(&format!("/designs/{id}/delete"), "", Some(&seller)).await;
    assert!(r.body.contains("still being confirmed"), "{}", r.body);
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_designs WHERE id = {id}"))
            .await,
        1
    );
    // bea's open checkout was closed at Stripe, and her order with it
    let expired: Vec<String> = st.lock().unwrap().expired.clone();
    assert_eq!(expired, ["cs_test_1"]);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE stripe_session = 'cs_test_1' AND status = 'expired'")
            .await,
        1
    );
    // once cal's checkout is over too (here: it expired without the money arriving), it goes
    {
        let mut f = st.lock().unwrap();
        f.sessions.get_mut("cs_test_2").unwrap().1 = false;
        f.expired.push("cs_test_2".into());
    }
    let r = s.post(&format!("/designs/{id}/delete"), "", Some(&seller)).await;
    assert_eq!(r.location(), "/designs", "{}", r.body);
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_designs WHERE id = {id}"))
            .await,
        0
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_orders WHERE status = 'pending'")
            .await,
        0
    );
    // a payment for it can't be started any more
    let posts = st
        .lock()
        .unwrap()
        .posts
        .iter()
        .filter(|(p, _)| p.ends_with("/expire"))
        .count();
    assert_eq!(posts, 1, "an expired session isn't expired again");
}

#[tokio::test]
async fn deleting_your_data_closes_checkouts_for_your_designs() {
    let Some((s, st, seller, id)) = selling_site(Vec::new()).await else {
        return;
    };
    let bea = s.sign_up("bea%40example.com").await;
    s.post(&format!("/d/{id}/buy"), "", Some(&bea)).await;
    // Stripe can't say whether it's paid: nothing is deleted
    st.lock().unwrap().sessions.get_mut("cs_test_1").unwrap().1 = true;
    let r = s.post("/account/data/delete", "confirm=delete", Some(&seller)).await;
    assert!(r.body.contains("Nothing was changed"), "{}", r.body);
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_designs WHERE id = {id}"))
            .await,
        1
    );
    st.lock().unwrap().sessions.get_mut("cs_test_1").unwrap().1 = false;
    let r = s.post("/account/data/delete", "confirm=delete", Some(&seller)).await;
    assert!(r.body.contains("Your MakerFoundry data is deleted"), "{}", r.body);
    assert_eq!(st.lock().unwrap().expired, ["cs_test_1"]);
    assert_eq!(
        s.scalar(&format!("SELECT count(*) FROM mf_designs WHERE id = {id}"))
            .await,
        0
    );
}

/// Issue #82: the shop's `order.status` through the hub shows on the customer's order page in the
/// shop's words, and the customer is emailed once per status with a link to this deploy's order
/// page (a staging deploy's test mailbox here, as on staging).
#[tokio::test]
async fn a_shops_status_reaches_the_customers_page_and_email() {
    let Some((s, state, hub, st)) = open_shop().await else {
        return;
    };
    let (base, sent) = makerfoundry::notices::stand_in::start().await.unwrap();
    let state = AppState {
        notices: Some(makerfoundry::notices::Notices::to_catcher("", None, &base).unwrap()),
        public_url: "https://makerfoundry-staging.test".into(),
        ..state
    };
    let (bea, path, r) = paid_order(&s, &st).await;
    let deliver = |m: Value| makerfoundry::shop_orders::hub::stand_in::deliver(&hub, m);
    deliver(from_shop("order.status", 801, &r, 1, json!({"status": "received"})));
    deliver(from_shop(
        "order.status",
        802,
        &r,
        2,
        json!({"status": "in_production"}),
    ));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert!(
        hub.lock()
            .unwrap()
            .queues
            .get("service:makerfoundry")
            .is_none_or(Vec::is_empty),
        "both taken (acked)"
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'in_production'")
            .await,
        1
    );
    // the page: in the words the stitch-e2e check looks for
    let page = s.get(&path, Some(&bea)).await.body;
    assert!(page.contains("Being made (in production)"), "{page}");

    // the emails: one per status, to the customer, linking to this deploy's order page
    let mails: Vec<Value> = sent.lock().unwrap().iter().map(|v| v["mailpit"].clone()).collect();
    assert_eq!(mails.len(), 2, "{mails:?}");
    let link = format!("https://makerfoundry-staging.test/shop-orders/{r}");
    for (m, words) in mails.iter().zip(["Received by the shop", "Being made (in production)"]) {
        assert_eq!(m["To"][0]["Email"], "bea@example.com");
        let subject = m["Subject"].as_str().unwrap();
        assert!(subject.contains("order") && subject.contains(words), "{subject}");
        let text = m["Text"].as_str().unwrap();
        assert!(text.contains(&link) && text.contains(words), "{text}");
        assert!(
            !text.contains("makerfoundry.app/") && !text.contains("localhost"),
            "{text}"
        );
        assert!(m.get("Headers").is_none(), "an order's email has no unsubscribe link");
    }

    // a message that comes again (a new id, the same seq) and another sweep: no more emails
    deliver(from_shop(
        "order.status",
        803,
        &r,
        2,
        json!({"status": "in_production"}),
    ));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(sent.lock().unwrap().len(), 2);
    // applied again by its own id (as after a crash before it was marked seen): still mailed once
    sqlx::query("DELETE FROM mf_hub_seen WHERE id LIKE '%000000000802'")
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE mf_hub_seq SET last_seq = 1 WHERE order_ref = $1")
        .bind(&r)
        .execute(&s.pool)
        .await
        .unwrap();
    deliver(from_shop(
        "order.status",
        802,
        &r,
        2,
        json!({"status": "in_production"}),
    ));
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(sent.lock().unwrap().len(), 2);
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_order_mails WHERE sent_at IS NOT NULL")
            .await,
        2
    );

    // no email sender (a deploy without one): the status is still applied, nothing is sent
    let quiet = AppState {
        notices: None,
        ..state.clone()
    };
    deliver(from_shop("order.status", 804, &r, 3, json!({"status": "ready"})));
    makerfoundry::shop_orders::sync(&quiet).await.unwrap();
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_shop_orders WHERE status = 'ready'")
            .await,
        1
    );
    assert_eq!(sent.lock().unwrap().len(), 2);
    // and the next sweep with a sender mails it
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    assert_eq!(sent.lock().unwrap().len(), 3);
}

// ---------------------------------------------------------------- copies of messages to shops

#[tokio::test]
async fn copies_of_messages_to_shops_go_30_days_after_the_order_is_done() {
    let Some((s, state, _, st)) = open_shop().await else {
        return;
    };
    let (_, _, reference) = paid_order(&s, &st).await;
    makerfoundry::shop_orders::sync(&state).await.unwrap();
    let carries = format!(
        "SELECT count(*) FROM mf_hub_outbox WHERE order_ref = '{reference}' AND envelope->'body' <> '{{}}'::jsonb"
    );
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_hub_outbox WHERE order_ref = '{reference}' AND sent_at IS NOT NULL"
        ))
        .await,
        1,
        "the hub took it"
    );
    // an earlier message of the same conversation, also taken long ago
    sqlx::query(
        "INSERT INTO mf_hub_outbox (id, kind, order_ref, seq, to_party, envelope, expires_at, sent_at)
         SELECT 'earlier', kind, order_ref, seq - 1, to_party, envelope, expires_at, sent_at
         FROM mf_hub_outbox WHERE order_ref = $1",
    )
    .bind(&reference)
    .execute(&s.pool)
    .await
    .unwrap();
    // while the order is open, or closed less than 30 days ago, they're kept as they are
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    assert_eq!(s.scalar(&carries).await, 2);
    sqlx::query("UPDATE mf_hub_outbox SET sent_at = now() - interval '40 days' WHERE order_ref = $1")
        .bind(&reference)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE mf_shop_orders SET status = 'shipped', updated_at = now() - interval '29 days' WHERE ref = $1")
        .bind(&reference)
        .execute(&s.pool)
        .await
        .unwrap();
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    assert_eq!(s.scalar(&carries).await, 2);
    // 30 days after it shipped: the earlier copy goes, the last is kept for its number, blanked
    sqlx::query("UPDATE mf_shop_orders SET updated_at = now() - interval '31 days' WHERE ref = $1")
        .bind(&reference)
        .execute(&s.pool)
        .await
        .unwrap();
    makerfoundry::shop_orders::after::sweep(&state).await.unwrap();
    assert_eq!(s.scalar(&carries).await, 0);
    assert_eq!(
        s.scalar(&format!(
            "SELECT count(*) FROM mf_hub_outbox WHERE order_ref = '{reference}'"
        ))
        .await,
        1
    );
    assert_eq!(
        s.scalar("SELECT count(*) FROM mf_hub_outbox WHERE id = 'earlier'")
            .await,
        0
    );
}
