//! Founding creators (MF-54): MakerFoundry's first 100 creators (the owner's decision,
//! 2026-10-30). Makers apply at `/founding` (signed in: the name to know them by, a link to their
//! work, what they make); an admin approves or declines each at `/admin/founding`, up to
//! `SPOTS`; each approved maker gets a numbered badge, "Founding creator #7", shown on their maker
//! page and their designs. The page works before the launch, so it can be shared on its own.

use axum::Form;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{current_user, forbidden, private, same_origin};
use crate::limits;
use crate::listings::paragraphs;
use crate::pages::{Preview, escape};

/// How many founding creators there are (the owner's decision, 2026-10-30).
pub const SPOTS: i32 = 100;
/// The longest name, link and "what you make", in characters.
const MAX_NAME: usize = 100;
const MAX_URL: usize = 300;
const MAX_ABOUT: usize = 1000;

/// How many spots are taken.
async fn taken(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mf_founding WHERE status = 'approved'")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// How many founding spots are left, or `None` without a database (a test site).
pub async fn spots_left(state: &AppState) -> Option<i64> {
    let pool = state.db.as_ref()?;
    Some((i64::from(SPOTS) - taken(pool).await).max(0))
}

/// A founding creator's number, if `account` is one.
pub async fn number(pool: &PgPool, account: i64) -> Option<i32> {
    sqlx::query_scalar("SELECT number FROM mf_founding WHERE account_id = $1 AND status = 'approved'")
        .bind(account)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

/// The badge for founding creator `number` (HTML).
pub fn badge(number: i32) -> String {
    format!(
        r#"<span class="badge founding" title="One of MakerFoundry's first {SPOTS} creators">Founding creator #{number}</span>"#
    )
}

/// `account`'s badge, or nothing if they aren't a founding creator.
pub async fn badge_for(pool: &PgPool, account: i64) -> String {
    number(pool, account).await.map(badge).unwrap_or_default()
}

/// How many applications are waiting for an admin.
pub async fn waiting(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mf_founding WHERE status = 'waiting'")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// For an admin's account page: a "Founding: N waiting" link, when N > 0 (empty for anyone else).
pub async fn waiting_link(state: &AppState, account: i64) -> String {
    let Some(pool) = state.db.as_ref().filter(|_| state.admins.contains(&account)) else {
        return String::new();
    };
    match waiting(pool).await {
        0 => String::new(),
        n => format!(r#"<p><a href="/admin/founding">Founding: {n} waiting</a></p>"#),
    }
}

/// The email telling the admins of an application: only the name, link and what they make,
/// never the applicant's email address.
fn admin_mail(public_url: &str, name: &str, link: &str, about: &str) -> (String, String) {
    let link = if link.is_empty() { "(no link)" } else { link };
    (
        format!("MakerFoundry: new founding creator application: {name}"),
        format!(
            "New founding creator application: {name} ({link}).\n\nWhat they make:\n{about}\n\nReview: {public_url}/admin/founding\n"
        ),
    )
}

/// Tells every admin of an application (skipped, with a log line, if mail isn't set up).
async fn tell_admins(state: &AppState, pool: &PgPool, name: &str, link: &str, about: &str) {
    let Some(notices) = state.notices.as_ref() else {
        eprintln!("founding: no mail set up, admins not told of an application");
        return;
    };
    let emails: Vec<String> = sqlx::query_scalar("SELECT email FROM mf_accounts WHERE account_id = ANY($1)")
        .bind(&state.admins)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let (subject, text) = admin_mail(&state.public_url, name, link, about);
    for email in emails.iter().filter(|e| !e.trim().is_empty()) {
        if let Err(e) = notices.send_about_order(email, &subject, &text).await {
            eprintln!("founding: telling an admin: {e}");
        }
    }
}

/// Tells an applicant how it went: `number` is `Some` for an approval, `None` for a decline.
/// A transactional email (not the digest).
async fn tell_applicant(state: &AppState, pool: &PgPool, account: i64, number: Option<i32>) {
    let Some(notices) = state.notices.as_ref() else {
        return;
    };
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT a.email, m.handle FROM mf_accounts a LEFT JOIN mf_makers m ON m.account_id = a.account_id
         WHERE a.account_id = $1",
    )
    .bind(account)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((email, handle)) = row.filter(|(e, _)| !e.trim().is_empty()) else {
        return;
    };
    let (subject, text) = match number {
        Some(n) => {
            let page = match handle {
                Some(h) => format!("{}/makers/{h}", state.public_url),
                None => format!("{}/account/maker (set up your maker profile so people see your badge)", state.public_url),
            };
            (
                format!("MakerFoundry: you're Founding creator #{n}"),
                format!("Welcome aboard! You're Founding creator #{n} on MakerFoundry.\n\nYour maker page: {page}\n\nThank you for being one of the first.\n"),
            )
        }
        None => (
            "MakerFoundry: about your founding creator application".to_string(),
            "Thank you for applying to be a founding creator. This time it wasn't a yes, but you're still very welcome to share your designs on MakerFoundry.\n".to_string(),
        ),
    };
    if let Err(e) = notices.send_about_order(&email, &subject, &text).await {
        eprintln!("founding: telling an applicant: {e}");
    }
}

/// One line as typed, tidied, or why it won't do.
fn line(text: &str, what: &str, max: usize, required: bool) -> Result<String, String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if required && text.is_empty() {
        return Err(format!("Give {what}."));
    }
    if text.chars().count() > max || text.chars().any(char::is_control) {
        return Err(format!("Keep {what} to {max} characters of plain text."));
    }
    Ok(text)
}

/// A link to someone's work: empty, or an `https://` (or `http://`) address with no spaces.
fn work_link(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(String::new());
    }
    let lower = text.to_ascii_lowercase();
    let ok = (lower.starts_with("https://") || lower.starts_with("http://"))
        && text.len() <= MAX_URL
        && text.len() > "https://".len()
        && !text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '<' || c == '>');
    if ok {
        Ok(text.to_string())
    } else {
        Err("Give a link to your work starting https:// (your shop, MakerWorld, Printables, Instagram…), or leave it empty.".into())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Application {
    name: String,
    work_url: String,
    about: String,
}

/// An application as typed → name, link, what they make; or why it won't do.
fn check(f: &Application) -> Result<(String, String, String), String> {
    let name = line(&f.name, "the name to know you by", MAX_NAME, true)?;
    let link = work_link(&f.work_url)?;
    let about = f.about.trim().replace("\r\n", "\n");
    if about.is_empty() {
        return Err("Say a little about what you make.".into());
    }
    if about.chars().count() > MAX_ABOUT || about.chars().any(|c| c.is_control() && c != '\n') {
        return Err(format!("Keep what you make to {MAX_ABOUT} characters of plain text."));
    }
    Ok((name, link, about))
}

/// The public page: what founding creators are, the spots left, and the viewer's application
/// (a form, or where it stands). `problem` and `form` refill the form after a mistake.
async fn page_html(pool: &PgPool, viewer: Option<i64>, problem: &str, form: &Application) -> String {
    let left = (i64::from(SPOTS) - taken(pool).await).max(0);
    let spots = match left {
        0 => format!("All {SPOTS} spots are taken. Thank you to everyone who applied."),
        1 => format!("1 of {SPOTS} spots left."),
        n => format!("{n} of {SPOTS} spots left."),
    };
    let mine: Option<(String, Option<i32>)> = match viewer {
        Some(a) => sqlx::query_as("SELECT status, number FROM mf_founding WHERE account_id = $1")
            .bind(a)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    };
    let action = match (viewer, mine) {
        (_, Some((status, Some(n)))) if status == "approved" => {
            let profile = match viewer {
                Some(a) if crate::makers::profile(pool, a).await.is_none() => {
                    r#" <a href="/account/maker">Set up your maker profile</a> so people see it."#
                }
                _ => "",
            };
            format!(r#"<p class="lead">You're in: {}{profile}</p>"#, badge(n))
        }
        (_, Some((status, _))) if status == "declined" => {
            r#"<p class="lead">Thank you for applying. This time it wasn't a yes; you're still welcome to share your designs on MakerFoundry.</p>"#.to_string()
        }
        (_, Some(_)) => r#"<p class="lead">Thanks: your application is in. We'll look at it soon, and the badge appears on your maker page once you're approved.</p>"#.to_string(),
        (_, None) if left == 0 => String::new(),
        (None, None) => r#"<p class="actions"><a class="button" href="/signup?next=/founding">Create your free Foundry Account to apply</a> <a class="button secondary" href="/signin?next=/founding">Sign in</a></p>"#.to_string(),
        (Some(_), None) => format!(
            r#"<h2>Apply</h2>
{problem}
<form method="post" action="/founding" class="add-file">
<label>The name to know you by <small>(your maker or shop name)</small><input name="name" maxlength="{MAX_NAME}" required value="{name}"></label>
<label>A link to your work <small>(optional: your shop, MakerWorld, Printables, Instagram…)</small><input name="work_url" type="url" maxlength="{MAX_URL}" value="{url}"></label>
<label>What you make <small>(embroidery, laser, 3D printing; a few lines)</small><textarea name="about" rows="4" maxlength="{MAX_ABOUT}" required>{about}</textarea></label>
<button type="submit">Apply to be a founding creator</button>
</form>
<p class="muted">Only MakerFoundry's admins see your application.</p>"#,
            name = escape(&form.name),
            url = escape(&form.work_url),
            about = escape(&form.about),
        ),
    };
    format!(
        r#"<section class="form prose">
<h1>Founding creators</h1>
<p class="lead">MakerFoundry is looking for its first {SPOTS} creators: makers of machine-embroidery designs, laser-cut files and 3D-printable models who'll share their work here first and help shape the place.</p>
<p>Founding creators get a numbered <strong>Founding creator</strong> badge on their maker page and their designs, for good, and early access to MakerFoundry 3D, our new home for 3D-printing models.</p>
<p><strong>{spots}</strong></p>
{action}
</section>"#
    )
}

fn preview_of(state: &AppState) -> (String, String) {
    (
        format!("{}/founding", state.public_url),
        format!(
            "MakerFoundry is looking for its first {SPOTS} founding creators: embroidery, laser and 3D-printing makers. Apply for a numbered Founding creator badge."
        ),
    )
}

pub async fn page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(pool) = state.db.clone() else {
        return Redirect::to("/").into_response();
    };
    let viewer = current_user(&state, &headers).await.map(|u| u.account_id);
    let body = page_html(&pool, viewer, "", &Application::default()).await;
    let (url, description) = preview_of(&state);
    let preview = Preview {
        title: "Become a founding creator on MakerFoundry",
        description: &description,
        url: &url,
        image: None,
    };
    let html = crate::pages::page_with(
        "Founding creators · MakerFoundry",
        &body,
        viewer.is_some(),
        Some(&preview),
    );
    if viewer.is_some() {
        private("Founding creators · MakerFoundry", &body, true)
    } else {
        Html(html).into_response()
    }
}

pub async fn apply(State(state): State<AppState>, headers: HeaderMap, Form(f): Form<Application>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.clone()) else {
        return Redirect::to("/signin?next=/founding").into_response();
    };
    let problem = match check(&f) {
        Err(why) => why,
        Ok(_) if !state.limits.allow(limits::FOUNDING, &user.account_id.to_string()) => {
            "That's a lot of tries: please wait a while.".to_string()
        }
        Ok((name, link, about)) => {
            // one application an account; a waiting one can be sent again with changes
            let saved = sqlx::query(
                "INSERT INTO mf_founding (account_id, name, work_url, about) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (account_id) DO UPDATE SET name = $2, work_url = $3, about = $4, applied_at = now()
                 WHERE mf_founding.status = 'waiting'",
            )
            .bind(user.account_id)
            .bind(&name)
            .bind(&link)
            .bind(&about)
            .execute(&pool)
            .await;
            match saved {
                Ok(done) => {
                    if done.rows_affected() > 0 {
                        tell_admins(&state, &pool, &name, &link, &about).await;
                    }
                    return Redirect::to("/founding").into_response();
                }
                Err(e) => {
                    eprintln!("founding: apply {}: {e}", user.account_id);
                    "Your application couldn't be sent just now. Please try again.".to_string()
                }
            }
        }
    };
    private(
        "Founding creators · MakerFoundry",
        &page_html(&pool, Some(user.account_id), &problem, &f).await,
        true,
    )
}

// ---------------------------------------------------------------- the admins' page

/// The applications: waiting first (oldest first), then the founding creators by number.
async fn admin_html(pool: &PgPool, note: &str) -> String {
    type Row = (i64, String, String, String, String, String, Option<i32>, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT f.account_id, f.name, f.work_url, f.about, f.status, to_char(f.applied_at, 'YYYY-MM-DD'), f.number, m.handle
         FROM mf_founding f LEFT JOIN mf_makers m ON m.account_id = f.account_id
         ORDER BY CASE f.status WHEN 'waiting' THEN 0 WHEN 'approved' THEN 1 ELSE 2 END, f.number, f.applied_at
         LIMIT 2000",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let left = (i64::from(SPOTS) - taken(pool).await).max(0);
    let items: String = rows
        .iter()
        .map(|(account, name, url, about, status, at, number, handle)| {
            let link = if url.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<p><a href="{u}" rel="nofollow noopener noreferrer ugc">{u}</a></p>"#,
                    u = escape(url)
                )
            };
            let profile = match handle {
                Some(h) => format!(r#" · <a href="/makers/{h}">maker page</a>"#, h = escape(h)),
                None => " · no maker profile yet".to_string(),
            };
            let state = match (status.as_str(), number) {
                ("approved", Some(n)) => badge(*n),
                ("declined", _) => r#"<span class="muted">declined</span>"#.to_string(),
                _ => r#"<span class="badge">waiting</span>"#.to_string(),
            };
            let actions = match status.as_str() {
                "waiting" if left > 0 => format!(
                    r#"<form method="post" action="/admin/founding/{account}/approve"><button type="submit">Approve</button></form><form method="post" action="/admin/founding/{account}/decline"><button type="submit" class="secondary">Decline</button></form>"#
                ),
                "waiting" => format!(
                    r#"<form method="post" action="/admin/founding/{account}/decline"><button type="submit" class="secondary">Decline</button></form>"#
                ),
                "approved" => format!(
                    r#"<form method="post" action="/admin/founding/{account}/remove"><button type="submit" class="secondary">Remove the badge</button></form>"#
                ),
                _ => format!(
                    r#"<form method="post" action="/admin/founding/{account}/reconsider"><button type="submit" class="secondary">Back to waiting</button></form>"#
                ),
            };
            format!(
                r#"<li class="report"><p><strong>{name}</strong> {state} <span class="muted">· account {account} · applied {at}{profile}</span></p>{link}{about}<div class="actions">{actions}</div></li>"#,
                name = escape(name),
                at = escape(at),
                about = paragraphs(about),
            )
        })
        .collect();
    let note = if note.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(note))
    };
    let list = if rows.is_empty() {
        r#"<p class="lead">No applications yet.</p>"#.to_string()
    } else {
        format!(r#"<ul class="reports">{items}</ul>"#)
    };
    format!(
        r#"<section class="design"><h1>Founding creators</h1>{note}<p class="muted">{left} of {SPOTS} spots left. Approving gives the lowest free number. The public page is <a href="/founding">/founding</a>. <a href="/admin">Reports</a></p>{list}</section>"#
    )
}

pub async fn admin(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (_, pool) = match crate::moderation::admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    private("Founding creators · MakerFoundry", &admin_html(&pool, "").await, true)
}

/// Approve, decline, remove the badge, or put back to waiting.
pub async fn decide(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account, action)): Path<(i64, String)>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (admin, pool) = match crate::moderation::admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let done = match action.as_str() {
        "approve" => match approve(&pool, account, admin.account_id).await {
            Ok((note, number)) => {
                if let Some(n) = number {
                    tell_applicant(&state, &pool, account, Some(n)).await;
                }
                Ok(note)
            }
            Err(e) => Err(e),
        },
        "decline" => match set_status(&pool, account, admin.account_id, "declined", "waiting").await {
            Ok(changed) => {
                if changed {
                    tell_applicant(&state, &pool, account, None).await;
                }
                Ok(String::new())
            }
            Err(e) => Err(e),
        },
        "remove" => set_status(&pool, account, admin.account_id, "declined", "approved")
            .await
            .map(|_| String::new()),
        "reconsider" => set_status(&pool, account, admin.account_id, "waiting", "declined")
            .await
            .map(|_| String::new()),
        _ => Ok(String::new()),
    };
    let note = match done {
        Ok(n) => n,
        Err(e) => {
            eprintln!("founding: {action} {account}: {e}");
            "That couldn't be done just now. Please try again.".to_string()
        }
    };
    if note.is_empty() {
        Redirect::to("/admin/founding").into_response()
    } else {
        private(
            "Founding creators · MakerFoundry",
            &admin_html(&pool, &note).await,
            true,
        )
    }
}

/// Approves a waiting application with the lowest free number → a note if it couldn't be.
async fn approve(pool: &PgPool, account: i64, admin: i64) -> Result<(String, Option<i32>), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // one approval at a time, so two admins can't hand out the same number or go past SPOTS
    sqlx::query("LOCK TABLE mf_founding IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let free: Option<i32> = sqlx::query_scalar(
        "SELECT n FROM generate_series(1, $1) AS n
         WHERE NOT EXISTS (SELECT 1 FROM mf_founding f WHERE f.number = n) ORDER BY n LIMIT 1",
    )
    .bind(SPOTS)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(n) = free else {
        return Ok((format!("All {SPOTS} spots are taken."), None));
    };
    let changed = sqlx::query(
        "UPDATE mf_founding SET status = 'approved', number = $2, decided_at = now(), decided_by = $3
         WHERE account_id = $1 AND status = 'waiting'",
    )
    .bind(account)
    .bind(n)
    .bind(admin)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(if changed == 0 {
        ("That application isn't waiting any more.".to_string(), None)
    } else {
        (String::new(), Some(n))
    })
}

/// → whether an application was changed.
async fn set_status(pool: &PgPool, account: i64, admin: i64, to: &str, from: &str) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "UPDATE mf_founding SET status = $2, number = NULL, decided_at = now(), decided_by = $4
         WHERE account_id = $1 AND status = $3",
    )
    .bind(account)
    .bind(to)
    .bind(from)
    .bind(admin)
    .execute(pool)
    .await
    .map(|r| r.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applications_are_checked() {
        let ok = Application {
            name: "  Nick   Prints ".into(),
            work_url: "https://makerworld.com/en/@nick".into(),
            about: "3D-printed organisers.".into(),
        };
        assert_eq!(
            check(&ok),
            Ok((
                "Nick Prints".into(),
                "https://makerworld.com/en/@nick".into(),
                "3D-printed organisers.".into()
            ))
        );
        let with = |name: &str, url: &str, about: &str| Application {
            name: name.into(),
            work_url: url.into(),
            about: about.into(),
        };
        assert!(check(&with("", "", "x")).is_err());
        assert!(check(&with("Nick", "", "")).is_err());
        assert!(check(&with("Nick", "javascript:alert(1)", "x")).is_err());
        assert!(check(&with("Nick", "https://a.b/\"><script>", "x")).is_err());
        assert!(check(&with("Nick", "ftp://a.b", "x")).is_err());
        assert_eq!(check(&with("Nick", "", "x")).map(|a| a.1), Ok(String::new()));
        assert!(check(&with(&"n".repeat(101), "", "x")).is_err());
        assert!(check(&with("Nick", "", &"a".repeat(1001))).is_err());
    }

    #[test]
    fn the_badge_carries_its_number() {
        assert!(badge(7).contains("Founding creator #7"));
    }
}
