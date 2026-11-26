//! Keeping listings safe (MF-5): anyone can report a design; copyright owners send a notice and
//! makers a counter-notice (17 U.S.C. § 512(c)(3) and (g)(3):
//! https://www.law.cornell.edu/uscode/text/17/512, and https://www.copyright.gov/512/); admins
//! (`MF_ADMINS`) see them in one queue and take a design down, dismiss a report, or restore a
//! design, in a click each, every action logged.
//!
//! The policy's wording is a draft for the owner (legal wording is the owner's). The designated
//! agent's details come from `MF_DMCA_AGENT` once the owner has registered one with the US
//! Copyright Office (lines apart by `|`).

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use sqlx::{PgPool, Row};

use crate::AppState;
use crate::accounts::{User, current_user, forbidden, private, same_origin};
use crate::designs;
use crate::limits::{self, REPORT};
use crate::listings::slug;
use crate::pages::{escape, page};

/// Why someone reports a design: (id, what the form says).
pub const REASONS: &[(&str, &str)] = &[
    ("copyright", "It copies someone else's design (copyright)"),
    ("trademark", "It uses someone else's logo, brand or character"),
    ("broken", "The file is broken or isn't what the listing says"),
    ("offensive", "It's offensive or harmful"),
    ("spam", "It's spam or a scam"),
    ("other", "Something else"),
];

/// Why someone reports a make (MF-12): (id, what the form says).
pub const MAKE_REASONS: &[(&str, &str)] = &[
    ("not-theirs", "It isn't their photo, or not something they made"),
    ("offensive", "It's offensive or harmful"),
    ("spam", "It's spam or an advert"),
    ("other", "Something else"),
];

/// Why someone reports a comment (MF-14): (id, what the form says).
pub const COMMENT_REASONS: &[(&str, &str)] = &[
    ("rude", "It's rude, harassing or hateful"),
    ("spam", "It's spam or an advert"),
    ("other", "Something else"),
];

fn reason_text(id: &str) -> &'static str {
    match id {
        "dmca" => "A copyright notice (DMCA)",
        "counter" => "A counter-notice from the maker",
        _ => REASONS
            .iter()
            .chain(MAKE_REASONS)
            .chain(COMMENT_REASONS)
            .find(|(r, _)| *r == id)
            .map_or("Something else", |(_, t)| *t),
    }
}

fn problem_html(problem: &str) -> String {
    if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    }
}

fn done(title: &str, text: &str, signed_in: bool) -> Response {
    private(
        &format!("{title} · MakerFoundry"),
        &format!(
            r#"<section class="form"><h1>{title}</h1><p class="lead">{text}</p><p><a href="/browse">Browse designs</a></p></section>"#
        ),
        signed_in,
    )
}

fn not_found(signed_in: bool) -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(page(
            "Not found · MakerFoundry",
            r#"<section class="hero"><h1>Not found</h1></section>"#,
            signed_in,
        )),
    )
        .into_response()
}

fn too_many(signed_in: bool) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        done(
            "Please wait",
            "Too many reports have been sent from here lately. Please try again in an hour.",
            signed_in,
        ),
    )
        .into_response()
}

/// A short single line from a form field.
fn line(text: &str, max: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// A longer text from a form field.
fn text(t: &str, max: usize) -> String {
    t.trim().replace("\r\n", "\n").chars().take(max).collect()
}

// ---------------------------------------------------------------- reporting a design

fn report_form(id: i64, title: &str, reason: &str, details: &str, problem: &str) -> String {
    let choices: String = REASONS
        .iter()
        .map(|(r, t)| {
            format!(
                r#"<label class="choice"><input type="radio" name="reason" value="{r}"{c}> <span>{t}</span></label>"#,
                c = if *r == reason { " checked" } else { "" },
                t = escape(t)
            )
        })
        .collect();
    format!(
        r#"<section class="form">
<h1>Report a design</h1>
<p class="lead">What's wrong with <strong>{title}</strong>? We read every report.</p>
{problem}
<form method="post" action="/d/{id}/report">
<fieldset><legend>The problem</legend>{choices}</fieldset>
<label>Details <small>(what you saw; for copyright, where the original is)</small><textarea name="details" rows="5" maxlength="2000">{details}</textarea></label>
<label>Your email <small>(optional: only if you'd like an answer)</small><input type="email" name="email" maxlength="254"></label>
<button type="submit">Send the report</button>
</form>
<p class="muted">If you own the copyright in a design copied here, you can also send a formal <a href="/dmca/notice">copyright notice</a>.</p>
</section>"#,
        title = escape(title),
        problem = problem_html(problem),
        details = escape(details),
    )
}

/// The published design `id`, for reporting: its title.
async fn reportable(pool: &PgPool, id: i64) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT CASE WHEN title = '' THEN file_name ELSE title END FROM mf_designs WHERE id = $1 AND status = 'published'")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

pub async fn report_page(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let signed_in = current_user(&state, &headers).await.is_some();
    let Some(pool) = state.db.as_ref() else {
        return not_found(signed_in);
    };
    let Some(title) = reportable(pool, id).await else {
        return not_found(signed_in);
    };
    private(
        "Report a design · MakerFoundry",
        &report_form(id, &title, "", "", ""),
        signed_in,
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ReportForm {
    reason: String,
    details: String,
    email: String,
}

pub async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<ReportForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let user = current_user(&state, &headers).await;
    let signed_in = user.is_some();
    let Some(pool) = state.db.as_ref() else {
        return not_found(signed_in);
    };
    let Some(title) = reportable(pool, id).await else {
        return not_found(signed_in);
    };
    let details = text(&f.details, 2000);
    if !REASONS.iter().any(|(r, _)| *r == f.reason) {
        return private(
            "Report a design · MakerFoundry",
            &report_form(id, &title, "", &details, "Choose what the problem is."),
            signed_in,
        );
    }
    if !state.limits.allow(REPORT, &limits::client(&headers)) {
        return too_many(signed_in);
    }
    let saved = sqlx::query("INSERT INTO mf_reports (design_id, kind, reason, details, reporter_account, contact_email) VALUES ($1, 'report', $2, $3, $4, $5)")
        .bind(id)
        .bind(&f.reason)
        .bind(&details)
        .bind(user.map(|u| u.account_id))
        .bind(line(&f.email, 254))
        .execute(pool)
        .await;
    match saved {
        Ok(_) => done(
            "Thank you",
            "Your report is in. We'll look at the design and act if it breaks the rules.",
            signed_in,
        ),
        Err(e) => {
            eprintln!("moderation: report {id}: {e}");
            private(
                "Report a design · MakerFoundry",
                &report_form(
                    id,
                    &title,
                    &f.reason,
                    &details,
                    "The report couldn't be sent just now. Please try again.",
                ),
                signed_in,
            )
        }
    }
}

// ---------------------------------------------------------------- reporting a make

fn make_form(mid: i64, title: &str, reason: &str, details: &str, problem: &str) -> String {
    let choices: String = MAKE_REASONS
        .iter()
        .map(|(r, t)| {
            format!(
                r#"<label class="choice"><input type="radio" name="reason" value="{r}"{c}> <span>{t}</span></label>"#,
                c = if *r == reason { " checked" } else { "" },
                t = escape(t)
            )
        })
        .collect();
    format!(
        r#"<section class="form">
<h1>Report a make</h1>
<p class="lead">What's wrong with this photo on <strong>{title}</strong>? We read every report.</p>
<p><img src="/makes/{mid}/thumb" alt="The make reported" class="reported"></p>
{problem}
<form method="post" action="/makes/{mid}/report">
<fieldset><legend>The problem</legend>{choices}</fieldset>
<label>Details <small>(optional)</small><textarea name="details" rows="4" maxlength="2000">{details}</textarea></label>
<label>Your email <small>(optional: only if you'd like an answer)</small><input type="email" name="email" maxlength="254"></label>
<button type="submit">Send the report</button>
</form>
</section>"#,
        title = escape(title),
        problem = problem_html(problem),
        details = escape(details),
    )
}

/// A make on a published design, for reporting: its design and the design's title.
async fn reportable_make(pool: &PgPool, mid: i64) -> Option<(i64, String)> {
    sqlx::query_as(
        "SELECT d.id, CASE WHEN d.title = '' THEN d.file_name ELSE d.title END
         FROM mf_makes k JOIN mf_designs d ON d.id = k.design_id WHERE k.id = $1 AND d.status = 'published'",
    )
    .bind(mid)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

pub async fn make_report_page(State(state): State<AppState>, headers: HeaderMap, Path(mid): Path<i64>) -> Response {
    let signed_in = current_user(&state, &headers).await.is_some();
    let Some(pool) = state.db.as_ref() else {
        return not_found(signed_in);
    };
    let Some((_, title)) = reportable_make(pool, mid).await else {
        return not_found(signed_in);
    };
    private(
        "Report a make · MakerFoundry",
        &make_form(mid, &title, "", "", ""),
        signed_in,
    )
}

pub async fn make_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<i64>,
    Form(f): Form<ReportForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let user = current_user(&state, &headers).await;
    let signed_in = user.is_some();
    let Some(pool) = state.db.as_ref() else {
        return not_found(signed_in);
    };
    let Some((design, title)) = reportable_make(pool, mid).await else {
        return not_found(signed_in);
    };
    let details = text(&f.details, 2000);
    if !MAKE_REASONS.iter().any(|(r, _)| *r == f.reason) {
        return private(
            "Report a make · MakerFoundry",
            &make_form(mid, &title, "", &details, "Choose what the problem is."),
            signed_in,
        );
    }
    if !state.limits.allow(REPORT, &limits::client(&headers)) {
        return too_many(signed_in);
    }
    let saved = sqlx::query("INSERT INTO mf_reports (design_id, make_id, kind, reason, details, reporter_account, contact_email) VALUES ($1, $2, 'make', $3, $4, $5, $6)")
        .bind(design)
        .bind(mid)
        .bind(&f.reason)
        .bind(&details)
        .bind(user.map(|u| u.account_id))
        .bind(line(&f.email, 254))
        .execute(pool)
        .await;
    match saved {
        Ok(_) => done(
            "Thank you",
            "Your report is in. We'll look at the photo and take it down if it breaks the rules.",
            signed_in,
        ),
        Err(e) => {
            eprintln!("moderation: make report {mid}: {e}");
            private(
                "Report a make · MakerFoundry",
                &make_form(
                    mid,
                    &title,
                    &f.reason,
                    &details,
                    "The report couldn't be sent just now. Please try again.",
                ),
                signed_in,
            )
        }
    }
}

/// Takes down the make a report is about, and closes its reports.
pub async fn remove_make(State(state): State<AppState>, headers: HeaderMap, Path(rid): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (me, pool) = match admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let make: Option<(Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT design_id, make_id FROM mf_reports WHERE id = $1 AND status = 'open' AND kind = 'make'")
            .bind(rid)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
    if let Some((design, Some(mid))) = make {
        if let Err(e) = crate::makes::remove(&state, &pool, mid).await {
            eprintln!("moderation: remove make {mid}: {e}");
            return Redirect::to("/admin").into_response();
        }
        // (its other open reports went with it: the make is gone)
        let _ = sqlx::query(
            "UPDATE mf_reports SET status = 'actioned', resolved_at = now(), resolved_by = $2 WHERE id = $1",
        )
        .bind(rid)
        .bind(me.account_id)
        .execute(&pool)
        .await;
        log(
            &pool,
            design,
            Some(rid),
            me.account_id,
            "remove-make",
            &format!("make {mid}"),
        )
        .await;
    }
    Redirect::to("/admin").into_response()
}

// ---------------------------------------------------------------- reporting a comment

fn comment_form(cid: i64, title: &str, body: &str, reason: &str, problem: &str) -> String {
    let choices: String = COMMENT_REASONS
        .iter()
        .map(|(r, t)| {
            format!(
                r#"<label class="choice"><input type="radio" name="reason" value="{r}"{c}> <span>{t}</span></label>"#,
                c = if *r == reason { " checked" } else { "" },
                t = escape(t)
            )
        })
        .collect();
    format!(
        r#"<section class="form">
<h1>Report a comment</h1>
<p class="lead">What's wrong with this comment on <strong>{title}</strong>?</p>
<blockquote class="quoted">{body}</blockquote>
{problem}
<form method="post" action="/comments/{cid}/report">
<fieldset><legend>The problem</legend>{choices}</fieldset>
<label>Details <small>(optional)</small><textarea name="details" rows="3" maxlength="2000"></textarea></label>
<button type="submit">Send the report</button>
</form>
</section>"#,
        title = escape(title),
        body = crate::listings::paragraphs(body),
        problem = problem_html(problem),
    )
}

/// A comment on a published design, for reporting: its design, the design's title, the comment.
async fn reportable_comment(pool: &PgPool, cid: i64) -> Option<(i64, String, String)> {
    sqlx::query_as(
        "SELECT d.id, CASE WHEN d.title = '' THEN d.file_name ELSE d.title END, c.body
         FROM mf_comments c JOIN mf_designs d ON d.id = c.design_id WHERE c.id = $1 AND d.status = 'published'",
    )
    .bind(cid)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

pub async fn comment_report_page(State(state): State<AppState>, headers: HeaderMap, Path(cid): Path<i64>) -> Response {
    let signed_in = current_user(&state, &headers).await.is_some();
    let Some(pool) = state.db.as_ref() else {
        return not_found(signed_in);
    };
    let Some((_, title, body)) = reportable_comment(pool, cid).await else {
        return not_found(signed_in);
    };
    private(
        "Report a comment · MakerFoundry",
        &comment_form(cid, &title, &body, "", ""),
        signed_in,
    )
}

pub async fn comment_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(cid): Path<i64>,
    Form(f): Form<ReportForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let user = current_user(&state, &headers).await;
    let signed_in = user.is_some();
    let Some(pool) = state.db.as_ref() else {
        return not_found(signed_in);
    };
    let Some((design, title, body)) = reportable_comment(pool, cid).await else {
        return not_found(signed_in);
    };
    if !COMMENT_REASONS.iter().any(|(r, _)| *r == f.reason) {
        return private(
            "Report a comment · MakerFoundry",
            &comment_form(cid, &title, &body, "", "Choose what the problem is."),
            signed_in,
        );
    }
    if !state.limits.allow(REPORT, &limits::client(&headers)) {
        return too_many(signed_in);
    }
    let saved = sqlx::query("INSERT INTO mf_reports (design_id, comment_id, kind, reason, details, reporter_account) VALUES ($1, $2, 'comment', $3, $4, $5)")
        .bind(design)
        .bind(cid)
        .bind(&f.reason)
        .bind(text(&f.details, 2000))
        .bind(user.map(|u| u.account_id))
        .execute(pool)
        .await;
    match saved {
        Ok(_) => done(
            "Thank you",
            "Your report is in. We'll read the comment and take it down if it breaks the rules.",
            signed_in,
        ),
        Err(e) => {
            eprintln!("moderation: comment report {cid}: {e}");
            private(
                "Report a comment · MakerFoundry",
                &comment_form(
                    cid,
                    &title,
                    &body,
                    &f.reason,
                    "The report couldn't be sent just now. Please try again.",
                ),
                signed_in,
            )
        }
    }
}

/// Takes down the comment a report is about.
pub async fn remove_comment(State(state): State<AppState>, headers: HeaderMap, Path(rid): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (me, pool) = match admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT design_id, comment_id FROM mf_reports WHERE id = $1 AND status = 'open' AND kind = 'comment'",
    )
    .bind(rid)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten();
    if let Some((design, Some(cid))) = row {
        if let Err(e) = crate::comments::remove(&pool, cid).await {
            eprintln!("moderation: remove comment {cid}: {e}");
            return Redirect::to("/admin").into_response();
        }
        let _ = sqlx::query(
            "UPDATE mf_reports SET status = 'actioned', resolved_at = now(), resolved_by = $2 WHERE id = $1",
        )
        .bind(rid)
        .bind(me.account_id)
        .execute(&pool)
        .await;
        log(
            &pool,
            design,
            Some(rid),
            me.account_id,
            "remove-comment",
            &format!("comment {cid}"),
        )
        .await;
    }
    Redirect::to("/admin").into_response()
}

// ---------------------------------------------------------------- copyright

/// The designated agent's lines, from `MF_DMCA_AGENT`.
fn agent_html() -> String {
    match std::env::var("MF_DMCA_AGENT").ok().filter(|a| !a.trim().is_empty()) {
        Some(agent) => format!("<address>{}</address>", agent.split('|').map(|l| escape(l.trim())).collect::<Vec<_>>().join("<br>")),
        None => "<p>Our designated agent's registration with the US Copyright Office is being completed; until it is, please use the form.</p>".into(),
    }
}

pub async fn policy(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let signed_in = current_user(&state, &headers).await.is_some();
    Html(page(
        "Copyright · MakerFoundry",
        &format!(
            r#"<section class="form prose">
<h1>Copyright and takedowns</h1>
<p class="lead">Makers on MakerFoundry share and sell designs they made or have the right to share. If a design here copies your work, tell us and we'll take it down.</p>
<h2>Send a copyright notice</h2>
<p><a class="button" href="/dmca/notice">Send a notice</a></p>
<p>Or write to our designated agent:</p>
{agent}
<p>A notice needs (17 U.S.C. § 512(c)(3)):</p>
<ol>
<li>your physical or electronic signature;</li>
<li>which of your works you believe was copied;</li>
<li>the address of each design on MakerFoundry that copies it;</li>
<li>your name, postal address, phone number and email;</li>
<li>a statement that you believe in good faith the use isn't authorised by you, your agent or the law;</li>
<li>a statement that the notice is accurate and, under penalty of perjury, that you are the owner or authorised to act for them.</li>
</ol>
<h2>What happens next</h2>
<p>We take the design down quickly and tell its maker. The maker may send a counter-notice if they believe it was removed by mistake. We then tell you, and put the design back after 10 to 14 business days unless you tell us you've gone to court to stop it.</p>
<p>Accounts of makers who repeatedly infringe are closed.</p>
<p>Knowingly misrepresenting that something infringes, or was removed by mistake, can make you liable for damages (17 U.S.C. § 512(f)).</p>
<h2>Anything else</h2>
<p>For other problems with a design (someone else's logo or character, a broken file, something offensive), use <strong>Report this design</strong> on its page.</p>
</section>"#,
            agent = agent_html(),
        ),
        signed_in,
    ))
    .into_response()
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NoticeForm {
    name: String,
    email: String,
    address: String,
    phone: String,
    original: String,
    designs: String,
    good_faith: String,
    accurate: String,
    signature: String,
}

/// The design ids in what someone pasted: MakerFoundry design addresses (`/d/<id>`), one a line.
pub fn design_ids(text: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    for word in text.split_whitespace() {
        if let Some(rest) = word.split("/d/").nth(1) {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(id) = digits.parse::<i64>()
                && !ids.contains(&id)
                && ids.len() < 50
            {
                ids.push(id);
            }
        }
    }
    ids
}

fn notice_form(f: &NoticeForm, problem: &str) -> String {
    format!(
        r#"<section class="form">
<h1>Copyright notice</h1>
<p class="lead">For the owner of a copyright, or someone acting for them. Every field is needed.</p>
{problem}
<form method="post" action="/dmca/notice">
<label>Your full name<input name="name" value="{name}" required maxlength="200"></label>
<label>Email<input type="email" name="email" value="{email}" required maxlength="254"></label>
<label>Postal address<textarea name="address" rows="3" required maxlength="500">{address}</textarea></label>
<label>Phone<input type="tel" name="phone" value="{phone}" required maxlength="50"></label>
<label>Your work that was copied <small>(what it is, and where it can be seen)</small><textarea name="original" rows="4" required maxlength="2000">{original}</textarea></label>
<label>The designs on MakerFoundry that copy it <small>(their addresses, one a line)</small><textarea name="designs" rows="3" required maxlength="2000">{designs}</textarea></label>
<label class="choice"><input type="checkbox" name="good_faith" value="yes"> <span>I believe in good faith that the use of the material in the way complained of is not authorised by the copyright owner, its agent, or the law.</span></label>
<label class="choice"><input type="checkbox" name="accurate" value="yes"> <span>The information in this notice is accurate, and under penalty of perjury, I am the owner, or authorised to act on behalf of the owner, of an exclusive right that is allegedly infringed.</span></label>
<label>Signature <small>(type your full name)</small><input name="signature" value="{signature}" required maxlength="200"></label>
<button type="submit">Send the notice</button>
</form>
</section>"#,
        problem = problem_html(problem),
        name = escape(&f.name),
        email = escape(&f.email),
        address = escape(&f.address),
        phone = escape(&f.phone),
        original = escape(&f.original),
        designs = escape(&f.designs),
        signature = escape(&f.signature),
    )
}

pub async fn notice_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let signed_in = current_user(&state, &headers).await.is_some();
    private(
        "Copyright notice · MakerFoundry",
        &notice_form(&NoticeForm::default(), ""),
        signed_in,
    )
}

pub async fn notice(State(state): State<AppState>, headers: HeaderMap, Form(f): Form<NoticeForm>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let user = current_user(&state, &headers).await;
    let signed_in = user.is_some();
    let again = |why: &str| private("Copyright notice · MakerFoundry", &notice_form(&f, why), signed_in);
    let Some(pool) = state.db.as_ref() else {
        return again("Notices can't be sent just now. Please write to our designated agent.");
    };
    let (name, email, phone, signature) = (
        line(&f.name, 200),
        line(&f.email, 254),
        line(&f.phone, 50),
        line(&f.signature, 200),
    );
    let (address, original) = (text(&f.address, 500), text(&f.original, 2000));
    if [&name, &email, &phone, &signature, &address, &original]
        .iter()
        .any(|v| v.is_empty())
        || !email.contains('@')
    {
        return again("Every field is needed, and a real email address.");
    }
    if f.good_faith != "yes" || f.accurate != "yes" {
        return again("Both statements are needed for a copyright notice.");
    }
    let ids = design_ids(&f.designs);
    let mut found = Vec::new();
    for id in ids {
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM mf_designs WHERE id = $1 AND status <> 'draft'")
            .bind(id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
        found.extend(exists);
    }
    if found.is_empty() {
        return again(
            "Give the address of at least one design on MakerFoundry (it looks like makerfoundry.app/d/123).",
        );
    }
    if !state.limits.allow(REPORT, &limits::client(&headers)) {
        return too_many(signed_in);
    }
    for id in &found {
        let saved = sqlx::query(
            "INSERT INTO mf_reports (design_id, kind, reason, details, reporter_account, contact_name, contact_email, contact_address, contact_phone, original_work, signature)
             VALUES ($1, 'dmca', 'dmca', '', $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(user.as_ref().map(|u| u.account_id))
        .bind(&name)
        .bind(&email)
        .bind(&address)
        .bind(&phone)
        .bind(&original)
        .bind(&signature)
        .execute(pool)
        .await;
        if let Err(e) = saved {
            eprintln!("moderation: notice: {e}");
            return again("The notice couldn't be sent just now. Please try again.");
        }
    }
    done(
        "Notice received",
        &format!(
            "We've received your notice about {} design{}. We'll act on it promptly and write to you at the email you gave.",
            found.len(),
            if found.len() == 1 { "" } else { "s" }
        ),
        signed_in,
    )
}

// ---------------------------------------------------------------- counter-notice

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CounterForm {
    name: String,
    address: String,
    phone: String,
    email: String,
    mistake: String,
    consent: String,
    signature: String,
}

fn counter_form(id: i64, f: &CounterForm, problem: &str) -> String {
    format!(
        r#"<section class="form">
<h1>Counter-notice</h1>
<p class="lead">If you believe your design was taken down by mistake or misidentification, you can send a counter-notice (17 U.S.C. § 512(g)(3)). We send it to whoever sent the notice.</p>
{problem}
<form method="post" action="/designs/{id}/counter">
<label>Your full name<input name="name" value="{name}" required maxlength="200"></label>
<label>Postal address<textarea name="address" rows="3" required maxlength="500">{address}</textarea></label>
<label>Phone<input type="tel" name="phone" value="{phone}" required maxlength="50"></label>
<label>Email<input type="email" name="email" value="{email}" required maxlength="254"></label>
<label class="choice"><input type="checkbox" name="mistake" value="yes"> <span>Under penalty of perjury, I have a good faith belief that the material was removed or disabled as a result of mistake or misidentification of the material to be removed or disabled.</span></label>
<label class="choice"><input type="checkbox" name="consent" value="yes"> <span>I consent to the jurisdiction of the Federal District Court for the judicial district in which my address is located (or, if my address is outside the United States, any judicial district in which MakerFoundry may be found), and I will accept service of process from the person who sent the notice, or their agent.</span></label>
<label>Signature <small>(type your full name)</small><input name="signature" value="{signature}" required maxlength="200"></label>
<button type="submit">Send the counter-notice</button>
</form>
</section>"#,
        problem = problem_html(problem),
        name = escape(&f.name),
        address = escape(&f.address),
        phone = escape(&f.phone),
        email = escape(&f.email),
        signature = escape(&f.signature),
    )
}

/// The maker's own removed design.
async fn own_removed(state: &AppState, headers: &HeaderMap, id: i64) -> Result<(User, PgPool), Response> {
    let (Some(user), Some(pool)) = (current_user(state, headers).await, state.db.clone()) else {
        return Err(Redirect::to("/signin").into_response());
    };
    match designs::design(&pool, id).await {
        Ok(Some(d)) if d.account_id == user.account_id && d.status == "removed" => Ok((user, pool)),
        _ => Err(not_found(true)),
    }
}

pub async fn counter_page(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if let Err(r) = own_removed(&state, &headers, id).await {
        return r;
    }
    private(
        "Counter-notice · MakerFoundry",
        &counter_form(id, &CounterForm::default(), ""),
        true,
    )
}

pub async fn counter(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<CounterForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool) = match own_removed(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let again = |why: &str| private("Counter-notice · MakerFoundry", &counter_form(id, &f, why), true);
    let (name, email, phone, signature, address) = (
        line(&f.name, 200),
        line(&f.email, 254),
        line(&f.phone, 50),
        line(&f.signature, 200),
        text(&f.address, 500),
    );
    if [&name, &email, &phone, &signature, &address]
        .iter()
        .any(|v| v.is_empty())
        || !email.contains('@')
    {
        return again("Every field is needed, and a real email address.");
    }
    if f.mistake != "yes" || f.consent != "yes" {
        return again("Both statements are needed for a counter-notice.");
    }
    let saved = sqlx::query(
        "INSERT INTO mf_reports (design_id, kind, reason, reporter_account, contact_name, contact_email, contact_address, contact_phone, signature)
         VALUES ($1, 'counter', 'counter', $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(user.account_id)
    .bind(&name)
    .bind(&email)
    .bind(&address)
    .bind(&phone)
    .bind(&signature)
    .execute(&pool)
    .await;
    match saved {
        Ok(_) => done(
            "Counter-notice received",
            "We'll send it to whoever sent the notice. If they don't tell us within 10 to 14 business days that they've gone to court, we'll put your design back.",
            true,
        ),
        Err(e) => {
            eprintln!("moderation: counter {id}: {e}");
            again("The counter-notice couldn't be sent just now. Please try again.")
        }
    }
}

// ---------------------------------------------------------------- the admin queue

/// The signed-in admin, or "not found" for anyone else (the queue isn't advertised).
pub(crate) async fn admin(state: &AppState, headers: &HeaderMap) -> Result<(User, PgPool), Response> {
    let (Some(user), Some(pool)) = (current_user(state, headers).await, state.db.clone()) else {
        return Err(not_found(false));
    };
    if !state.admins.contains(&user.account_id) {
        return Err(not_found(true));
    }
    Ok((user, pool))
}

async fn log(pool: &PgPool, design: Option<i64>, report: Option<i64>, admin: i64, action: &str, note: &str) {
    let r = sqlx::query(
        "INSERT INTO mf_moderation_log (design_id, report_id, admin_account, action, note) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(design)
    .bind(report)
    .bind(admin)
    .bind(action)
    .bind(note)
    .execute(pool)
    .await;
    if let Err(e) = r {
        eprintln!("moderation: log: {e}");
    }
}

pub async fn queue(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (_, pool) = match admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let rows = sqlx::query(
        "SELECT r.id, r.design_id, r.make_id, r.comment_id, (SELECT body FROM mf_comments WHERE id = r.comment_id) AS comment, r.kind, r.reason, r.details, r.contact_name, r.contact_email, r.contact_address, r.contact_phone,
                r.original_work, r.signature, to_char(r.created_at, 'YYYY-MM-DD HH24:MI') AS at,
                d.title, d.file_name, d.status, d.account_id,
                m.name AS maker, m.handle,
                (SELECT count(*) FROM mf_moderation_log l JOIN mf_designs d2 ON d2.id = l.design_id
                  WHERE d2.account_id = d.account_id AND l.action = 'takedown') AS takedowns
         FROM mf_reports r
         LEFT JOIN mf_designs d ON d.id = r.design_id
         LEFT JOIN mf_makers m ON m.account_id = d.account_id
         WHERE r.status = 'open'
         ORDER BY CASE r.kind WHEN 'dmca' THEN 0 WHEN 'counter' THEN 1 ELSE 2 END, r.created_at
         LIMIT 200",
    )
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    let items: String = rows
        .iter()
        .map(|r| {
            let get = |c: &str| r.try_get::<Option<String>, _>(c).ok().flatten().unwrap_or_default();
            let rid: i64 = r.try_get("id").unwrap_or(0);
            let design: Option<i64> = r.try_get("design_id").ok().flatten();
            let make: Option<i64> = r.try_get("make_id").ok().flatten();
            let kind = get("kind");
            let status = get("status");
            let title = if get("title").is_empty() { get("file_name") } else { get("title") };
            let takedowns: i64 = r.try_get("takedowns").unwrap_or(0);
            let contact = if kind == "report" {
                if get("contact_email").is_empty() { String::new() } else { format!("<p>Reply to: {}</p>", escape(&get("contact_email"))) }
            } else {
                format!(
                    r#"<p><strong>From:</strong> {name}, {email}, {phone}<br>{address}<br><strong>Signed:</strong> {sig}</p>{original}"#,
                    name = escape(&get("contact_name")),
                    email = escape(&get("contact_email")),
                    phone = escape(&get("contact_phone")),
                    address = escape(&get("contact_address")).replace('\n', "<br>"),
                    sig = escape(&get("signature")),
                    original = if get("original_work").is_empty() { String::new() } else { format!("<p><strong>Their work:</strong> {}</p>", escape(&get("original_work"))) },
                )
            };
            let design_html = match design {
                Some(id) => format!(
                    r#"<p><a href="/d/{id}/{slug}">{title}</a> by {maker} <span class="muted">({status}{repeat})</span></p>"#,
                    slug = slug(&title),
                    title = escape(&title),
                    maker = if get("handle").is_empty() { "a maker".to_string() } else { format!(r#"<a href="/makers/{h}">{n}</a>"#, h = escape(&get("handle")), n = escape(&get("maker"))) },
                    repeat = if takedowns > 0 { format!(", {takedowns} takedown{} before", if takedowns == 1 { "" } else { "s" }) } else { String::new() },
                ),
                None => "<p class=\"muted\">The design has been deleted.</p>".to_string(),
            };
            let comment: Option<i64> = r.try_get("comment_id").ok().flatten();
            let comment_html = match (kind.as_str(), comment) {
                ("comment", Some(_)) => format!(r#"<blockquote class="quoted">{}</blockquote>"#, crate::listings::paragraphs(&get("comment"))),
                ("comment", None) => r#"<p class="muted">The comment has been taken down already.</p>"#.to_string(),
                _ => String::new(),
            };
            let make_html = match (kind.as_str(), make) {
                ("make", Some(mid)) => format!(r#"<p><a href="/makes/{mid}/large"><img src="/makes/{mid}/thumb" alt="The make reported" class="reported"></a></p>"#),
                ("make", None) => r#"<p class="muted">The make has been taken down already.</p>"#.to_string(),
                _ => String::new(),
            };
            let actions = match (design, kind.as_str(), status.as_str()) {
                (_, "comment", _) if comment.is_some() => format!(
                    r#"<form method="post" action="/admin/reports/{rid}/remove-comment"><button type="submit">Take the comment down</button></form><form method="post" action="/admin/reports/{rid}/dismiss"><button type="submit" class="secondary">Dismiss</button></form>"#
                ),
                (_, "comment", _) => format!(r#"<form method="post" action="/admin/reports/{rid}/dismiss"><button type="submit" class="secondary">Close</button></form>"#),
                (_, "make", _) if make.is_some() => format!(
                    r#"<form method="post" action="/admin/reports/{rid}/remove-make"><button type="submit">Take the make down</button></form><form method="post" action="/admin/reports/{rid}/dismiss"><button type="submit" class="secondary">Dismiss</button></form>"#
                ),
                (_, "make", _) => format!(r#"<form method="post" action="/admin/reports/{rid}/dismiss"><button type="submit" class="secondary">Close</button></form>"#),
                (Some(_), "counter", "removed") => format!(
                    r#"<form method="post" action="/admin/reports/{rid}/restore"><button type="submit">Restore the design</button></form><form method="post" action="/admin/reports/{rid}/dismiss"><button type="submit" class="secondary">Keep it down</button></form>"#
                ),
                (Some(_), _, "published") => format!(
                    r#"<form method="post" action="/admin/reports/{rid}/takedown"><button type="submit">Take it down</button></form><form method="post" action="/admin/reports/{rid}/dismiss"><button type="submit" class="secondary">Dismiss</button></form>"#
                ),
                _ => format!(r#"<form method="post" action="/admin/reports/{rid}/dismiss"><button type="submit" class="secondary">Close</button></form>"#),
            };
            format!(
                r#"<li class="report"><p><strong>{what}</strong> <span class="muted">· #{rid} · {at}</span></p>{design_html}{make_html}{comment_html}{details}{contact}<div class="actions">{actions}</div></li>"#,
                what = escape(reason_text(&get("reason"))),
                at = escape(&get("at")),
                details = if get("details").is_empty() { String::new() } else { format!("<p>{}</p>", escape(&get("details"))) },
            )
        })
        .collect();
    let body = if rows.is_empty() {
        r#"<p class="lead">Nothing waiting. 🎉</p>"#.to_string()
    } else {
        format!(r#"<ul class="reports">{items}</ul>"#)
    };
    private(
        "Reports · MakerFoundry",
        &format!(
            r#"<section class="design"><h1>Reports</h1><p class="muted"><a href="/admin/overview">Overview</a> · <a href="/admin/orders">Orders</a> · <a href="/admin/founding">Founding creators</a></p>{body}</section>"#
        ),
        true,
    )
}

/// The open report `rid`: its design and kind.
async fn open_report(pool: &PgPool, rid: i64) -> Option<(Option<i64>, String, String)> {
    sqlx::query_as("SELECT design_id, kind, reason FROM mf_reports WHERE id = $1 AND status = 'open'")
        .bind(rid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

async fn close(pool: &PgPool, rid: i64, status: &str, admin: i64) {
    let r = sqlx::query("UPDATE mf_reports SET status = $2, resolved_at = now(), resolved_by = $3 WHERE id = $1")
        .bind(rid)
        .bind(status)
        .bind(admin)
        .execute(pool)
        .await;
    if let Err(e) = r {
        eprintln!("moderation: close {rid}: {e}");
    }
}

/// Takes the reported design down: off every public page, its maker told why.
pub async fn takedown(State(state): State<AppState>, headers: HeaderMap, Path(rid): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (me, pool) = match admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    if let Some((Some(design), _, reason)) = open_report(&pool, rid).await {
        let why = format!("Taken down after a report: {}", reason_text(&reason).to_lowercase());
        let r = sqlx::query("UPDATE mf_designs SET status = 'removed', removed_reason = $2 WHERE id = $1")
            .bind(design)
            .bind(&why)
            .execute(&pool)
            .await;
        if let Err(e) = r {
            eprintln!("moderation: takedown {design}: {e}");
            return Redirect::to("/admin").into_response();
        }
        // the other open reports of the same design are answered by the same takedown
        let _ = sqlx::query("UPDATE mf_reports SET status = 'actioned', resolved_at = now(), resolved_by = $2 WHERE design_id = $1 AND status = 'open' AND kind <> 'counter'")
            .bind(design)
            .bind(me.account_id)
            .execute(&pool)
            .await;
        log(&pool, Some(design), Some(rid), me.account_id, "takedown", &why).await;
        // a checkout still open for it is closed, so nobody pays for a design they can't
        // download (issue #192); the takedown itself never waits on Stripe
        if crate::selling::close_checkouts(&state, &pool, design).await.is_err() {
            return private(
                "Taken down · MakerFoundry",
                &format!(
                    r#"<section class="form"><h1>Taken down</h1><p class="problem" role="alert">The design is down, but a checkout for it couldn't be closed at Stripe (or a payment for it is on its way). Check <a href="/admin/orders">Orders</a> later and refund any payment for design {design} once it's confirmed.</p><p><a href="/admin">Back to reports</a></p></section>"#
                ),
                true,
            );
        }
    }
    Redirect::to("/admin").into_response()
}

pub async fn dismiss(State(state): State<AppState>, headers: HeaderMap, Path(rid): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (me, pool) = match admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    if let Some((design, _, _)) = open_report(&pool, rid).await {
        close(&pool, rid, "dismissed", me.account_id).await;
        log(&pool, design, Some(rid), me.account_id, "dismiss", "").await;
    }
    Redirect::to("/admin").into_response()
}

/// Puts a removed design back after a counter-notice the complainant didn't answer with a suit.
pub async fn restore(State(state): State<AppState>, headers: HeaderMap, Path(rid): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (me, pool) = match admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    if let Some((Some(design), kind, _)) = open_report(&pool, rid).await
        && kind == "counter"
    {
        let r = sqlx::query(
            "UPDATE mf_designs SET status = 'published', removed_reason = NULL WHERE id = $1 AND status = 'removed'",
        )
        .bind(design)
        .execute(&pool)
        .await;
        if let Err(e) = r {
            eprintln!("moderation: restore {design}: {e}");
            return Redirect::to("/admin").into_response();
        }
        close(&pool, rid, "actioned", me.account_id).await;
        log(
            &pool,
            Some(design),
            Some(rid),
            me.account_id,
            "restore",
            "after a counter-notice",
        )
        .await;
    }
    Redirect::to("/admin").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_addresses_are_found_in_what_was_pasted() {
        let pasted = "https://makerfoundry.app/d/12/rose-bouquet\nmakerfoundry.app/d/7 and /d/12 again, /d/x, https://other.example/x";
        assert_eq!(design_ids(pasted), [12, 7]);
        assert!(design_ids("nothing here").is_empty());
    }

    #[test]
    fn form_fields_are_trimmed_and_capped() {
        assert_eq!(line("  Ann \n  Smith  ", 200), "Ann Smith");
        assert_eq!(line(&"x".repeat(300), 200).len(), 200);
        assert_eq!(text(" a\r\nb ", 10), "a\nb");
    }
}
