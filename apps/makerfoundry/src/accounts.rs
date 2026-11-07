//! Signing up, in and out, and a forgotten password (MF-1). Accounts are Foundry Accounts, the one
//! login for all of SiteSentrix's software, kept by StitchFoundry's account service
//! (`cloud`); MakerFoundry keeps a session of its own for each browser (`db`), named by a random
//! id in a `__Host-` cookie (HttpOnly, Secure, SameSite=Lax) and kept only as its SHA-256. Every
//! form post must come from MakerFoundry's own pages (its `Origin`, or else its `Referer`).

use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::cloud::{Account, CloudError};
use crate::db;
use crate::pages::{escape, page};

/// The session cookie. `__Host-` means a browser keeps it only for this exact site, over HTTPS.
pub const COOKIE: &str = "__Host-mf_session";

/// A signed-in person.
pub struct User {
    pub account_id: i64,
    pub email: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn session_hash(id: &str) -> String {
    hex(&Sha256::digest(id.as_bytes()))
}

/// The session id in the request's cookie, if it has one of the right shape.
fn session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.to_string())
        .filter(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Who is signed in, if anyone. A session the account service no longer knows (signed out there,
/// or the password was reset) ends here too, within `db::CHECK_MINUTES`; while the service can't
/// be reached, sessions carry on.
pub async fn current_user(state: &AppState, headers: &HeaderMap) -> Option<User> {
    let pool = state.db.as_ref()?;
    let hash = session_hash(&session_id(headers)?);
    let session = match db::session(pool, &hash).await {
        Ok(s) => s?,
        Err(e) => {
            eprintln!("sessions: {e}");
            return None;
        }
    };
    let mut user = User {
        account_id: session.account_id,
        email: session.email,
    };
    if session.stale {
        match state.cloud.account(&session.cloud_token).await {
            Ok(account) => {
                if let Err(e) = db::checked(pool, &hash, &account).await {
                    eprintln!("sessions: {e}");
                }
                user.email = account.email;
            }
            Err(e) if e.is_signed_out() => {
                if let Err(e) = db::end_session(pool, &hash).await {
                    eprintln!("sessions: {e}");
                }
                return None;
            }
            Err(_) => {}
        }
    }
    Some(user)
}

/// Whether a form post came from one of our own pages: its `Origin` (or, without one, its
/// `Referer`) names the host it was sent to. Other sites can't post forms on someone's behalf.
pub(crate) fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let from = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|h| h.to_str().ok());
    let Some(from) = from else { return false };
    let rest = from
        .strip_prefix("https://")
        .or_else(|| from.strip_prefix("http://"))
        .unwrap_or("");
    let from_host = rest.split('/').next().unwrap_or("");
    !from_host.is_empty() && from_host.eq_ignore_ascii_case(host)
}

pub(crate) fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Html(page("Not allowed · MakerFoundry", r#"<section class="hero"><h1>Not allowed</h1><p class="lead">That form has to be sent from MakerFoundry's own page. <a href="/">Go to the home page</a>.</p></section>"#, false))).into_response()
}

/// A page that isn't kept in caches (it may name the account).
pub(crate) fn private(title: &str, body: &str, signed_in: bool) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(page(title, body, signed_in)),
    )
        .into_response()
}

const TOO_MANY: &str = "Too many tries from here lately. Please wait a while and try again.";

const UNAVAILABLE: &str = "Signing in isn't available yet. Please try again later.";

/// What to tell someone when the account service said no, or couldn't be reached.
fn cloud_problem(e: &CloudError) -> String {
    match e {
        CloudError::Refused { status: 401, .. } => "The email or the password is wrong.".into(),
        CloudError::Refused { status: 409, .. } => {
            "There's already a Foundry Account with that email: sign in instead.".into()
        }
        CloudError::Refused {
            status: 400 | 429,
            message,
        } => sentence(message),
        CloudError::Refused { status: 503, .. } => "That isn't available yet. Please try again later.".into(),
        CloudError::Refused { .. } | CloudError::Unreachable => {
            "Foundry Accounts can't be reached right now. Please try again in a few minutes.".into()
        }
    }
}

/// The service's lower-case message as a sentence for a page.
fn sentence(message: &str) -> String {
    let mut c = message.chars();
    let mut s: String = c
        .next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default();
    if !s.ends_with(['.', '!', '?']) {
        s.push('.');
    }
    s
}

fn problem_html(problem: &str) -> String {
    if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Credentials {
    email: String,
    password: String,
    again: String,
    agree: String,
    /// Where to go once signed in (a page of this site).
    next: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Next {
    next: String,
}

/// `next` if it's a page of this site (`/…`, not `//…`), else the account page.
pub(crate) fn safe_next(next: &str) -> &str {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && next.len() <= 200
        && next.bytes().all(|b| b.is_ascii_graphic() && b != b'\\');
    if ok { next } else { "/account" }
}

/// Starts a session for an account the service just signed in: the cookie, then the account page.
async fn start_session(state: &AppState, token: &str, account: &Account, next: &str) -> Result<Response, String> {
    let pool = state.db.as_ref().ok_or(UNAVAILABLE)?;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no randomness: {e}"))?;
    let id = hex(&bytes);
    db::remember_account(pool, account)
        .await
        .map_err(|e| format!("sessions: {e}"))?;
    db::create_session(pool, &session_hash(&id), account.id, token)
        .await
        .map_err(|e| format!("sessions: {e}"))?;
    let cookie = format!(
        "{COOKIE}={id}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={}",
        db::SESSION_DAYS * 24 * 3600
    );
    let cookie = HeaderValue::from_str(&cookie).map_err(|e| e.to_string())?;
    let mut resp = Redirect::to(safe_next(next)).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(resp)
}

fn signin_form(email: &str, problem: &str, next: &str) -> String {
    format!(
        r#"<section class="form">
<h1>Sign in</h1>
<p class="lead">Sign in with your Foundry Account: the same email and password as in StitchFoundry and the other Foundry apps.</p>
{problem}
<form method="post" action="/signin">
<input type="hidden" name="next" value="{next}">
<label>Email<input type="email" name="email" value="{email}" autocomplete="email" required maxlength="254"></label>
<label>Password<input type="password" name="password" autocomplete="current-password" required maxlength="200"></label>
<button type="submit">Sign in</button>
</form>
<p><a href="/forgot">Forgot your password?</a></p>
<p>No account yet? <a href="/signup?next={next}">Make a Foundry Account</a>.</p>
</section>"#,
        email = escape(email),
        problem = problem_html(problem),
        next = escape(safe_next(next)),
    )
}

fn signup_form(email: &str, problem: &str, launched: bool, next: &str) -> String {
    let agree = if launched {
        r#"<label class="choice"><input type="checkbox" name="agree" value="yes" required> <span>I'm 18 or older and agree to the <a href="/terms">Terms of Service</a> and the <a href="/privacy">Privacy Policy</a>.</span></label>"#
    } else {
        ""
    };
    format!(
        r#"<section class="form">
<h1>Make an account</h1>
<p class="lead">A Foundry Account is one email and password for MakerFoundry, StitchFoundry and the other Foundry apps.</p>
{problem}
<form method="post" action="/signup">
<input type="hidden" name="next" value="{next}">
<label>Email<input type="email" name="email" value="{email}" autocomplete="email" required maxlength="254"></label>
<label>Password <small>(at least 8 characters)</small><input type="password" name="password" autocomplete="new-password" required minlength="8" maxlength="200"></label>
<label>The same password again<input type="password" name="again" autocomplete="new-password" required minlength="8" maxlength="200"></label>
{agree}
<button type="submit">Make the account</button>
</form>
<p>Already have a Foundry Account, from StitchFoundry say? <a href="/signin?next={next}">Sign in</a>.</p>
</section>"#,
        email = escape(email),
        problem = problem_html(problem),
        next = escape(safe_next(next)),
    )
}

pub async fn signin_page(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<Next>) -> Response {
    if current_user(&state, &headers).await.is_some() {
        return Redirect::to(safe_next(&q.next)).into_response();
    }
    let problem = if state.db.is_none() { UNAVAILABLE } else { "" };
    private("Sign in · MakerFoundry", &signin_form("", problem, &q.next), false)
}

pub async fn signin(State(state): State<AppState>, headers: HeaderMap, Form(form): Form<Credentials>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    if state.db.is_none() {
        return private(
            "Sign in · MakerFoundry",
            &signin_form(&form.email, UNAVAILABLE, &form.next),
            false,
        );
    }
    if !state
        .limits
        .allow(crate::limits::SIGNIN, &crate::limits::client(&headers))
    {
        return private(
            "Sign in · MakerFoundry",
            &signin_form(&form.email, TOO_MANY, &form.next),
            false,
        );
    }
    let result = match state.cloud.signin(form.email.trim(), &form.password).await {
        Ok((token, account)) => start_session(&state, &token, &account, &form.next).await,
        Err(e) => Err(cloud_problem(&e)),
    };
    result.unwrap_or_else(|problem| {
        private(
            "Sign in · MakerFoundry",
            &signin_form(&form.email, &problem, &form.next),
            false,
        )
    })
}

pub async fn signup_page(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<Next>) -> Response {
    if current_user(&state, &headers).await.is_some() {
        return Redirect::to(safe_next(&q.next)).into_response();
    }
    let problem = if state.db.is_none() { UNAVAILABLE } else { "" };
    private(
        "Make an account · MakerFoundry",
        &signup_form("", problem, state.launched, &q.next),
        false,
    )
}

pub async fn signup(State(state): State<AppState>, headers: HeaderMap, Form(form): Form<Credentials>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let again = |problem: &str| {
        private(
            "Make an account · MakerFoundry",
            &signup_form(&form.email, problem, state.launched, &form.next),
            false,
        )
    };
    if state.db.is_none() {
        return again(UNAVAILABLE);
    }
    if form.password != form.again {
        return again("The two passwords aren't the same.");
    }
    if state.launched && form.agree != "yes" {
        return again("To make an account, agree to the Terms of Service and the Privacy Policy.");
    }
    if !state
        .limits
        .allow(crate::limits::SIGNUP, &crate::limits::client(&headers))
    {
        return again(TOO_MANY);
    }
    let result = match state.cloud.signup(form.email.trim(), &form.password).await {
        Ok((token, account)) => {
            let started = start_session(&state, &token, &account, &form.next).await;
            if let (true, Ok(_), Some(pool)) = (state.launched, &started, state.db.as_ref()) {
                crate::legal::agree(pool, account.id, &[&crate::legal::TERMS, &crate::legal::PRIVACY]).await;
            }
            started
        }
        Err(e) => Err(cloud_problem(&e)),
    };
    result.unwrap_or_else(|problem| again(&problem))
}

/// Ends the session here and at the account service, and forgets the cookie.
pub async fn signout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    if let (Some(pool), Some(id)) = (state.db.as_ref(), session_id(&headers)) {
        match db::end_session(pool, &session_hash(&id)).await {
            Ok(Some(token)) => {
                // (signed out here whatever the service says; its session also ends on its own)
                let _ = state.cloud.signout(&token).await;
            }
            Ok(None) => {}
            Err(e) => eprintln!("sessions: {e}"),
        }
    }
    let mut resp = Redirect::to("/").into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("__Host-mf_session=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"),
    );
    resp
}

fn forgot_form(email: &str, problem: &str) -> String {
    format!(
        r#"<section class="form">
<h1>Forgot your password?</h1>
<p class="lead">Give the email of your Foundry Account, and we'll send a link to choose a new password.</p>
{problem}
<form method="post" action="/forgot">
<label>Email<input type="email" name="email" value="{email}" autocomplete="email" required maxlength="254"></label>
<button type="submit">Send the link</button>
</form>
<p><a href="/signin">Back to sign in</a></p>
</section>"#,
        email = escape(email),
        problem = problem_html(problem),
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Forgot {
    email: String,
}

pub async fn forgot_page() -> Response {
    private("Forgot your password · MakerFoundry", &forgot_form("", ""), false)
}

pub async fn forgot(State(state): State<AppState>, headers: HeaderMap, Form(form): Form<Forgot>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    if !state
        .limits
        .allow(crate::limits::FORGOT, &crate::limits::client(&headers))
    {
        return private(
            "Forgot your password · MakerFoundry",
            &forgot_form(&form.email, TOO_MANY),
            false,
        );
    }
    match state.cloud.forgot(form.email.trim()).await {
        Ok(message) => private(
            "Check your email · MakerFoundry",
            &format!(
                r#"<section class="form"><h1>Check your email</h1><p class="lead">{}</p><p>The link opens a page where you choose the new password; then <a href="/signin">sign in</a> here with it.</p></section>"#,
                escape(&message)
            ),
            false,
        ),
        Err(e) => private(
            "Forgot your password · MakerFoundry",
            &forgot_form(&form.email, &cloud_problem(&e)),
            false,
        ),
    }
}

pub async fn account_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return Redirect::to("/signin").into_response();
    };
    let founding = match state.db.as_ref() {
        Some(pool) => match crate::founding::number(pool, user.account_id).await {
            Some(n) => format!("<p>{}</p>", crate::founding::badge(n)),
            None => r#"<p><a href="/founding">Founding creators</a>: apply to be one of MakerFoundry's first 100 creators.</p>"#.to_string(),
        },
        None => String::new(),
    };
    let founding_waiting = crate::founding::waiting_link(&state, user.account_id).await;
    let proofs = if crate::proofs::available(&state, &user) {
        r#"<p><a href="/proofs">Proofs</a>: send a customer a design to approve before you stitch or cut it.</p>"#
    } else {
        ""
    };
    let activity = match state.db.as_ref() {
        Some(pool) => crate::activity::link_html(pool, user.account_id).await,
        None => String::new(),
    };
    let api_keys = if crate::model3d::open_to(&state, Some(&user)).await {
        r#"<p><a href="/account/api-keys">API keys</a>: for apps and slicers that read published 3D models.</p>"#
    } else {
        ""
    };
    let points = if state.points {
        r#"<p><a href="/account/points">Your points</a>: from what others make and like of your models.</p>"#
    } else {
        ""
    };
    let notices = if state.notices.is_some() {
        r#"<p><a href="/account/notices">Email notices</a>: a daily email about comments, makes, remixes and new followers.</p>"#
    } else {
        ""
    };
    let shops = match (state.db.as_ref(), state.stitchhub.is_some()) {
        (Some(pool), true) => crate::shop_orders::shops::claims_html(pool, user.account_id).await,
        _ => String::new(),
    };
    private(
        "Your account · MakerFoundry",
        &format!(
            r#"<section class="form">
<h1>Your account</h1>
<p class="lead">You're signed in as <strong>{email}</strong>, with your Foundry Account.</p>
<p><a href="/designs">Your designs</a>: add a design file and photos of what it makes.</p>
{founding}
{founding_waiting}
{activity}
{proofs}
<p><a href="/account/maker">Your maker profile</a>: the name people see on your designs.</p>
<p><a href="/preview">See your design</a>: upload an embroidery or laser file and see it drawn.</p>
<p><a href="/library">Your library</a>: the designs you've bought. <a href="/collections">Your collections</a>: the designs you've saved. <a href="/following">Following</a>: new designs from makers you follow.</p>
<p><a href="/account/selling">Selling</a>: get paid for your designs, and <a href="/account/sales">your sales</a>.</p>
<p>To change your password, <a href="/forgot">ask for a reset link</a>.</p>
<p><a href="/account/data">Your data</a>: download a copy of it, or delete it.</p>
<p><a href="/account/showcase">Featuring your projects</a>: what you've said MakerFoundry may feature on its social pages.</p>
{notices}
{api_keys}
{points}
{shops}
<form method="post" action="/signout"><button type="submit" class="secondary">Sign out</button></form>
</section>"#,
            email = escape(&user.email)
        ),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn form_posts_must_come_from_our_own_pages() {
        let ok = headers(&[
            (header::HOST, "makerfoundry.app"),
            (header::ORIGIN, "https://makerfoundry.app"),
        ]);
        assert!(same_origin(&ok));
        let by_referer = headers(&[
            (header::HOST, "makerfoundry.app"),
            (header::REFERER, "https://makerfoundry.app/signin"),
        ]);
        assert!(same_origin(&by_referer));
        for bad in [
            headers(&[
                (header::HOST, "makerfoundry.app"),
                (header::ORIGIN, "https://evil.example"),
            ]),
            headers(&[
                (header::HOST, "makerfoundry.app"),
                (header::ORIGIN, "https://makerfoundry.app.evil.example"),
            ]),
            headers(&[(header::HOST, "makerfoundry.app"), (header::ORIGIN, "null")]),
            headers(&[(header::HOST, "makerfoundry.app")]),
            headers(&[(header::ORIGIN, "https://makerfoundry.app")]),
        ] {
            assert!(!same_origin(&bad), "{bad:?}");
        }
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let id = "ab".repeat(32);
        let h = headers(&[(header::COOKIE, &format!("theme=dark; {COOKIE}={id}; other=1"))]);
        assert_eq!(session_id(&h), Some(id));
        assert_eq!(
            session_id(&headers(&[(header::COOKIE, &format!("{COOKIE}=short"))])),
            None
        );
        assert_eq!(
            session_id(&headers(&[(header::COOKIE, &format!("{COOKIE}={}", "zz".repeat(32)))])),
            None
        );
        assert_eq!(session_id(&HeaderMap::new()), None);
    }

    #[test]
    fn the_services_messages_read_as_sentences() {
        assert_eq!(sentence("give a real email address"), "Give a real email address.");
        assert_eq!(sentence("wait 15 minutes!"), "Wait 15 minutes!");
        let refused = |status| CloudError::Refused {
            status,
            message: "a password needs at least 8 characters".into(),
        };
        assert_eq!(cloud_problem(&refused(400)), "A password needs at least 8 characters.");
        assert_eq!(cloud_problem(&refused(401)), "The email or the password is wrong.");
        assert!(cloud_problem(&CloudError::Unreachable).contains("can't be reached"));
    }
}
