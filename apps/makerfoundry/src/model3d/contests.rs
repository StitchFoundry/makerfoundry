//! Contests (issue #33): a theme, a closing date and rules; creators enter their published 3D
//! models; judged by the admins, or by members' votes (one a contest, changeable until it
//! closes, never for one's own entry); the admins announce up to three places once it's closed,
//! and the winners are shown. Prizes and rules are the owner's: a contest's rules are text the
//! admins give it, and it can't open without them. Nothing shows until an admin opens one.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{User, current_user, forbidden, private, same_origin};
use crate::listings::{paragraphs, slug};
use crate::pages::escape;

/// The entries one creator may have in one contest.
pub const MAX_ENTRIES: i64 = 3;

/// A contest: id, slug, title, theme, rules, judging, closing date (`YYYY-MM-DD HH:MI UTC`),
/// status, and whether it has closed.
type ContestRow = (i64, String, String, String, String, String, String, String, bool);

struct Contest {
    id: i64,
    slug: String,
    title: String,
    theme: String,
    rules: String,
    judging: String,
    closes: String,
    status: String,
    closed: bool,
}

impl Contest {
    fn from(r: ContestRow) -> Self {
        let (id, slug, title, theme, rules, judging, closes, status, closed) = r;
        Contest {
            id,
            slug,
            title,
            theme,
            rules,
            judging,
            closes,
            status,
            closed,
        }
    }

    /// Taking entries (and votes).
    fn running(&self) -> bool {
        self.status == "open" && !self.closed
    }
}

const SELECT: &str = "SELECT id, slug, title, theme, rules, judging,
        to_char(closes_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI \"UTC\"'), status, closes_at <= now()
    FROM mf_contests";

async fn by_slug(pool: &PgPool, s: &str) -> Option<Contest> {
    sqlx::query_as::<_, ContestRow>(&format!("{SELECT} WHERE slug = $1"))
        .bind(s)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(Contest::from)
}

/// Whoever is looking, if contests are open to them (as MakerFoundry 3D is).
async fn viewer(state: &AppState, headers: &HeaderMap) -> Result<(Option<User>, PgPool), Response> {
    let user = current_user(state, headers).await;
    let Some(pool) = state.db.clone() else {
        return Err(crate::designs::not_found());
    };
    if !super::open_to(state, user.as_ref()).await {
        return Err(crate::designs::not_found());
    }
    Ok((user, pool))
}

/// `GET /contests`: the open contests, then the announced ones.
pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (user, pool) = match viewer(&state, &headers).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let rows: Vec<ContestRow> = sqlx::query_as(&format!(
        "{SELECT} WHERE status <> 'draft' ORDER BY status = 'announced', closes_at DESC LIMIT 100"
    ))
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    let items: String = rows
        .into_iter()
        .map(Contest::from)
        .map(|c| {
            let when = if c.running() {
                format!("Open until {}", escape(&c.closes))
            } else if c.status == "announced" {
                "Winners announced".to_string()
            } else {
                "Closed: judging".to_string()
            };
            format!(
                r#"<li><a href="/contests/{slug}"><strong>{title}</strong></a> <span class="muted">{when}</span><br>{theme}</li>"#,
                slug = escape(&c.slug),
                title = escape(&c.title),
                theme = escape(&c.theme),
            )
        })
        .collect();
    let body = if items.is_empty() {
        "<p>There are no contests yet.</p>".to_string()
    } else {
        format!(r#"<ul class="contests">{items}</ul>"#)
    };
    private(
        "Contests · MakerFoundry",
        &format!(
            r#"<section class="design"><h1>Contests</h1><p class="lead">Themed challenges for 3D models.</p>{body}</section>"#
        ),
        user.is_some(),
    )
}

/// An entry as a contest page shows it: design, title, maker, picture, place, votes.
type EntryRow = (i64, String, String, Option<i64>, Option<i32>, i64, i64);

async fn entries(pool: &PgPool, contest: i64) -> Vec<EntryRow> {
    sqlx::query_as(
        "SELECT d.id, d.title, m.name,
                (SELECT f.id FROM mf_model_files f WHERE f.design_id = d.id AND f.thumb_key IS NOT NULL ORDER BY f.position LIMIT 1),
                e.place, (SELECT count(*) FROM mf_contest_votes v WHERE v.contest_id = e.contest_id AND v.design_id = d.id),
                e.account_id
         FROM mf_contest_entries e JOIN mf_designs d ON d.id = e.design_id JOIN mf_makers m ON m.account_id = d.account_id
         WHERE e.contest_id = $1 AND d.status = 'published'
         ORDER BY e.place NULLS LAST, e.entered_at",
    )
    .bind(contest)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

fn place_name(p: i32) -> &'static str {
    match p {
        1 => "First place",
        2 => "Second place",
        _ => "Third place",
    }
}

async fn contest_page(state: &AppState, pool: &PgPool, user: Option<&User>, c: &Contest, problem: &str) -> Response {
    let list = entries(pool, c.id).await;
    let voted: Option<i64> = match user {
        Some(u) => {
            sqlx::query_scalar("SELECT design_id FROM mf_contest_votes WHERE contest_id = $1 AND account_id = $2")
                .bind(c.id)
                .bind(u.account_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
        }
        None => None,
    };
    let show_votes = c.judging == "votes" && !c.running();
    let items: String = list
        .iter()
        .map(|(id, title, maker, file, place, votes, account)| {
            let picture = match file {
                Some(f) => format!(r#"<img src="/3d/{id}/files/{f}/hero" alt="" loading="lazy">"#),
                None => r#"<span class="muted">No picture</span>"#.to_string(),
            };
            let place = match (place, c.status.as_str()) {
                (Some(p), "announced") => format!(r#"<span class="place">{}</span>"#, place_name(*p)),
                _ => String::new(),
            };
            let votes = if show_votes {
                format!(r#"<span class="muted">{votes} vote{}</span>"#, if *votes == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            let action = match user {
                Some(u) if c.running() && u.account_id == *account => format!(
                    r#"<form method="post" action="/contests/{s}/withdraw/{id}"><button type="submit" class="secondary small">Withdraw</button></form>"#,
                    s = escape(&c.slug)
                ),
                Some(_) if c.running() && c.judging == "votes" && voted == Some(*id) => {
                    r#"<span class="muted">Your vote</span>"#.to_string()
                }
                Some(_) if c.running() && c.judging == "votes" => format!(
                    r#"<form method="post" action="/contests/{s}/vote/{id}"><button type="submit" class="secondary small">Vote</button></form>"#,
                    s = escape(&c.slug)
                ),
                _ => String::new(),
            };
            format!(
                r#"<li><a href="/d/{id}/{slug}"><span class="thumb">{picture}</span><strong>{title}</strong><span class="muted">by {maker}</span></a>{place}{votes}{action}</li>"#,
                slug = slug(title),
                title = escape(title),
                maker = escape(maker),
            )
        })
        .collect();
    let entries_html = if items.is_empty() {
        "<p>No entries yet.</p>".to_string()
    } else {
        format!(r#"<ul class="cards contest-entries">{items}</ul>"#)
    };
    let enter = match user {
        Some(u) if c.running() => {
            let mine: Vec<(i64, String)> = sqlx::query_as(
                "SELECT id, title FROM mf_designs WHERE account_id = $1 AND kind = 'model3d' AND status = 'published'
                   AND id NOT IN (SELECT design_id FROM mf_contest_entries WHERE contest_id = $2)
                 ORDER BY published_at DESC LIMIT 200",
            )
            .bind(u.account_id)
            .bind(c.id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
            if mine.is_empty() {
                "<p class=\"muted\">Publish a 3D model to enter it.</p>".to_string()
            } else {
                let options: String = mine
                    .iter()
                    .map(|(id, t)| format!(r#"<option value="{id}">{}</option>"#, escape(t)))
                    .collect();
                format!(
                    r#"<form method="post" action="/contests/{s}/enter" class="form"><label>Your model <select name="design">{options}</select></label>
<label class="choice"><input type="checkbox" name="agree" value="yes"> <span>I made this model, and I agree to this contest's rules.</span></label>
<button type="submit">Enter</button></form>"#,
                    s = escape(&c.slug)
                )
            }
        }
        None if c.running() => format!(
            r#"<p><a href="/signin?next=/contests/{}">Sign in</a> to enter or vote.</p>"#,
            escape(&c.slug)
        ),
        _ => String::new(),
    };
    let when = if c.running() {
        format!(
            "Open until {}. Judged by {}.",
            escape(&c.closes),
            if c.judging == "votes" {
                "members' votes"
            } else {
                "MakerFoundry"
            }
        )
    } else if c.status == "announced" {
        "The winners are announced.".to_string()
    } else {
        "Closed: the winners will be announced soon.".to_string()
    };
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem">{}</p>"#, escape(problem))
    };
    let _ = state;
    private(
        &format!("{} · Contests · MakerFoundry", escape(&c.title)),
        &format!(
            r#"<section class="design"><p><a href="/contests">Contests</a></p><h1>{title}</h1><p class="lead">{theme}</p><p>{when}</p>{problem}
{enter}<h2>Entries</h2>{entries_html}<h2>Rules</h2>{rules}</section>"#,
            title = escape(&c.title),
            theme = escape(&c.theme),
            rules = paragraphs(&c.rules),
        ),
        user.is_some(),
    )
}

/// `GET /contests/<slug>`: open and announced contests (and drafts, for admins).
pub async fn page(State(state): State<AppState>, headers: HeaderMap, Path(s): Path<String>) -> Response {
    let (user, pool) = match viewer(&state, &headers).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let admin = user.as_ref().is_some_and(|u| state.admins.contains(&u.account_id));
    match by_slug(&pool, &s).await {
        Some(c) if c.status != "draft" || admin => contest_page(&state, &pool, user.as_ref(), &c, "").await,
        _ => crate::designs::not_found(),
    }
}

/// The signed-in member and the running contest a form is for.
async fn running(state: &AppState, headers: &HeaderMap, s: &str) -> Result<(User, PgPool, Contest), Response> {
    if !same_origin(headers) {
        return Err(forbidden());
    }
    let (user, pool) = viewer(state, headers).await?;
    let Some(user) = user else {
        return Err(Redirect::to(&format!("/signin?next=/contests/{s}")).into_response());
    };
    match by_slug(&pool, s).await {
        Some(c) if c.status != "draft" => {
            if c.running() {
                Ok((user, pool, c))
            } else {
                Err(contest_page(state, &pool, Some(&user), &c, "This contest has closed.").await)
            }
        }
        _ => Err(crate::designs::not_found()),
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct EnterForm {
    design: String,
    agree: String,
}

/// `POST /contests/<slug>/enter`: one of the member's own published 3D models.
pub async fn enter(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    axum::Form(f): axum::Form<EnterForm>,
) -> Response {
    let (user, pool, c) = match running(&state, &headers, &s).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if f.agree != "yes" {
        return contest_page(
            &state,
            &pool,
            Some(&user),
            &c,
            "Confirm you made the model and agree to the rules.",
        )
        .await;
    }
    let Ok(design) = f.design.trim().parse::<i64>() else {
        return contest_page(&state, &pool, Some(&user), &c, "Choose one of your models.").await;
    };
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mf_contest_entries WHERE contest_id = $1 AND account_id = $2")
            .bind(c.id)
            .bind(user.account_id)
            .fetch_one(&pool)
            .await
            .unwrap_or(MAX_ENTRIES);
    if count >= MAX_ENTRIES {
        return contest_page(
            &state,
            &pool,
            Some(&user),
            &c,
            &format!("You can enter up to {MAX_ENTRIES} models in a contest."),
        )
        .await;
    }
    match add_entry(&pool, c.id, design, user.account_id).await {
        Ok(Some(1)) => Redirect::to(&format!("/contests/{}", c.slug)).into_response(),
        Ok(None) => {
            contest_page(
                &state,
                &pool,
                Some(&user),
                &c,
                &format!("You can enter up to {MAX_ENTRIES} models in a contest."),
            )
            .await
        }
        Ok(_) => contest_page(&state, &pool, Some(&user), &c, "Choose one of your published models.").await,
        Err(e) => {
            eprintln!("contests: enter: {e}");
            contest_page(&state, &pool, Some(&user), &c, "The entry couldn't be saved just now.").await
        }
    }
}

/// Enters `design` in contest `contest` for `account`, holding a lock on (contest, account) so
/// requests sent at the same moment can't take them past `MAX_ENTRIES` (issue #137) → the rows
/// added (0 when it isn't their published model or is already in), or `None` when at the cap.
async fn add_entry(pool: &PgPool, contest: i64, design: i64, account: i64) -> Result<Option<u64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("mf-contest-entry-{contest}-{account}"))
        .execute(&mut *tx)
        .await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mf_contest_entries WHERE contest_id = $1 AND account_id = $2")
            .bind(contest)
            .bind(account)
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_ENTRIES {
        tx.rollback().await?;
        return Ok(None);
    }
    let added = sqlx::query(
        "INSERT INTO mf_contest_entries (contest_id, design_id, account_id)
         SELECT $1, d.id, d.account_id FROM mf_designs d
         WHERE d.id = $2 AND d.account_id = $3 AND d.kind = 'model3d' AND d.status = 'published'
         ON CONFLICT DO NOTHING",
    )
    .bind(contest)
    .bind(design)
    .bind(account)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(Some(added))
}

/// `POST /contests/<slug>/withdraw/<design>`: an entry taken back, with its votes.
pub async fn withdraw(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((s, design)): Path<(String, i64)>,
) -> Response {
    let (user, pool, c) = match running(&state, &headers, &s).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let _ = sqlx::query("DELETE FROM mf_contest_entries WHERE contest_id = $1 AND design_id = $2 AND account_id = $3")
        .bind(c.id)
        .bind(design)
        .bind(user.account_id)
        .execute(&pool)
        .await;
    Redirect::to(&format!("/contests/{}", c.slug)).into_response()
}

/// `POST /contests/<slug>/vote/<design>`: the member's one vote, moved here.
pub async fn vote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((s, design)): Path<(String, i64)>,
) -> Response {
    let (user, pool, c) = match running(&state, &headers, &s).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if c.judging != "votes" {
        return crate::designs::not_found();
    }
    if !state.limits.allow(crate::limits::LIKE, &user.account_id.to_string()) {
        return contest_page(
            &state,
            &pool,
            Some(&user),
            &c,
            "You've voted a lot this hour. Please wait a while.",
        )
        .await;
    }
    let done = sqlx::query(
        "INSERT INTO mf_contest_votes (contest_id, account_id, design_id)
         SELECT $1, $2, e.design_id FROM mf_contest_entries e JOIN mf_designs d ON d.id = e.design_id
         WHERE e.contest_id = $1 AND e.design_id = $3 AND e.account_id <> $2 AND d.status = 'published'
         ON CONFLICT (contest_id, account_id) DO UPDATE SET design_id = EXCLUDED.design_id, voted_at = now()",
    )
    .bind(c.id)
    .bind(user.account_id)
    .bind(design)
    .execute(&pool)
    .await;
    match done {
        Ok(r) if r.rows_affected() == 1 => Redirect::to(&format!("/contests/{}", c.slug)).into_response(),
        Ok(_) => contest_page(&state, &pool, Some(&user), &c, "You can't vote for your own entry.").await,
        Err(e) => {
            eprintln!("contests: vote: {e}");
            contest_page(&state, &pool, Some(&user), &c, "The vote couldn't be saved just now.").await
        }
    }
}

// ---------------------------------------------------------------- admins

async fn admin(state: &AppState, headers: &HeaderMap) -> Option<(User, PgPool)> {
    let user = current_user(state, headers).await?;
    let pool = state.db.clone()?;
    state.admins.contains(&user.account_id).then_some((user, pool))
}

async fn admin_html(pool: &PgPool, problem: &str) -> String {
    let rows: Vec<ContestRow> = sqlx::query_as(&format!("{SELECT} ORDER BY created_at DESC LIMIT 100"))
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let mut items = String::new();
    for c in rows.into_iter().map(Contest::from) {
        let action = match (c.status.as_str(), c.closed) {
            ("draft", _) => format!(
                r#"<form method="post" action="/admin/contests/{}/open"><button type="submit" class="small">Open it</button></form>"#,
                c.id
            ),
            ("open", true) => {
                let fields: String = entries(pool, c.id)
                    .await
                    .iter()
                    .map(|(id, title, maker, _, place, votes, _)| {
                        let options: String = [(0, "—"), (1, "1st"), (2, "2nd"), (3, "3rd")]
                            .iter()
                            .map(|(n, l)| {
                                format!(
                                    r#"<option value="{n}"{s}>{l}</option>"#,
                                    s = if place.unwrap_or(0) == *n { " selected" } else { "" }
                                )
                            })
                            .collect();
                        format!(
                            r#"<label>{title} by {maker} <small>({votes} votes)</small><select name="place_{id}">{options}</select></label>"#,
                            title = escape(title),
                            maker = escape(maker)
                        )
                    })
                    .collect();
                format!(
                    r#"<form method="post" action="/admin/contests/{}/announce" class="form">{fields}<button type="submit" class="small">Announce the winners</button></form>"#,
                    c.id
                )
            }
            ("open", false) => "<span class=\"muted\">Open</span>".to_string(),
            _ => "<span class=\"muted\">Announced</span>".to_string(),
        };
        items.push_str(&format!(
            r#"<li><a href="/contests/{slug}"><strong>{title}</strong></a> <span class="muted">{status}, closes {closes}, judged by {judging}</span>{action}</li>"#,
            slug = escape(&c.slug),
            title = escape(&c.title),
            status = escape(&c.status),
            closes = escape(&c.closes),
            judging = escape(&c.judging),
        ));
    }
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem">{}</p>"#, escape(problem))
    };
    format!(
        r#"<section class="design"><h1>Contests</h1>{problem}<ul class="contests">{items}</ul>
<h2>A new contest</h2>
<form method="post" action="/admin/contests" class="form">
<label>Title <input name="title" maxlength="100" required></label>
<label>Theme <input name="theme" maxlength="300" required></label>
<label>Closes (UTC) <input type="datetime-local" name="closes" required></label>
<label>Judged by <select name="judging"><option value="admins">The admins</option><option value="votes">Members' votes</option></select></label>
<label>Rules <small>(the owner's approved text, prizes included; it can't open without them)</small><textarea name="rules" rows="10" maxlength="20000"></textarea></label>
<button type="submit">Save as a draft</button>
</form></section>"#
    )
}

/// `GET /admin/contests`.
pub async fn admin_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some((_, pool)) = admin(&state, &headers).await else {
        return crate::designs::not_found();
    };
    private("Contests · MakerFoundry", &admin_html(&pool, "").await, true)
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct NewContest {
    title: String,
    theme: String,
    closes: String,
    judging: String,
    rules: String,
}

/// `POST /admin/contests`: a draft contest.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Form(f): axum::Form<NewContest>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let Some((_, pool)) = admin(&state, &headers).await else {
        return crate::designs::not_found();
    };
    let clean = |s: &str, max: usize| -> String { s.trim().chars().filter(|c| !c.is_control()).take(max).collect() };
    let (title, theme) = (clean(&f.title, 100), clean(&f.theme, 300));
    let rules: String = f
        .rules
        .trim()
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .take(20_000)
        .collect();
    let judging = if f.judging == "votes" { "votes" } else { "admins" };
    let s = slug(&title);
    if title.is_empty() || theme.is_empty() || s.is_empty() {
        return private(
            "Contests · MakerFoundry",
            &admin_html(&pool, "Give it a title and a theme.").await,
            true,
        );
    }
    let done = sqlx::query(
        "INSERT INTO mf_contests (slug, title, theme, rules, judging, closes_at)
         VALUES ($1, $2, $3, $4, $5, $6::timestamp AT TIME ZONE 'UTC')",
    )
    .bind(&s)
    .bind(&title)
    .bind(&theme)
    .bind(&rules)
    .bind(judging)
    .bind(f.closes.trim())
    .execute(&pool)
    .await;
    match done {
        Ok(_) => Redirect::to("/admin/contests").into_response(),
        Err(_) => private(
            "Contests · MakerFoundry",
            &admin_html(
                &pool,
                "It couldn't be saved: check the closing date, and that no other contest has this title.",
            )
            .await,
            true,
        ),
    }
}

/// `POST /admin/contests/<id>/open`: only with rules, and a closing date still ahead.
pub async fn open(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let Some((_, pool)) = admin(&state, &headers).await else {
        return crate::designs::not_found();
    };
    let done = sqlx::query(
        "UPDATE mf_contests SET status = 'open' WHERE id = $1 AND status = 'draft' AND length(rules) >= 50 AND closes_at > now()",
    )
    .bind(id)
    .execute(&pool)
    .await;
    if done.is_ok_and(|r| r.rows_affected() == 1) {
        Redirect::to("/admin/contests").into_response()
    } else {
        private(
            "Contests · MakerFoundry",
            &admin_html(
                &pool,
                "A contest opens only with its rules (the owner's approved text) and a closing date still ahead.",
            )
            .await,
            true,
        )
    }
}

/// `POST /admin/contests/<id>/announce`: the places (`place_<design>` = 1 to 3), once it's closed.
pub async fn announce(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    axum::Form(form): axum::Form<std::collections::HashMap<String, String>>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let Some((_, pool)) = admin(&state, &headers).await else {
        return crate::designs::not_found();
    };
    let closed: bool =
        sqlx::query_scalar("SELECT status = 'open' AND closes_at <= now() FROM mf_contests WHERE id = $1")
            .bind(id)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
    if !closed {
        return private(
            "Contests · MakerFoundry",
            &admin_html(&pool, "Winners are announced once a contest has closed.").await,
            true,
        );
    }
    let mut places: Vec<(i64, i32)> = Vec::new();
    for (k, v) in &form {
        if let (Some(design), Ok(p)) = (
            k.strip_prefix("place_").and_then(|d| d.parse::<i64>().ok()),
            v.parse::<i32>(),
        ) && (1..=3).contains(&p)
        {
            if places.iter().any(|(_, q)| *q == p) {
                return private(
                    "Contests · MakerFoundry",
                    &admin_html(&pool, "Each place goes to one entry.").await,
                    true,
                );
            }
            places.push((design, p));
        }
    }
    let result = async {
        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE mf_contest_entries SET place = NULL WHERE contest_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for (design, p) in &places {
            sqlx::query("UPDATE mf_contest_entries SET place = $3 WHERE contest_id = $1 AND design_id = $2")
                .bind(id)
                .bind(design)
                .bind(p)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE mf_contests SET status = 'announced' WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    .await;
    if let Err(e) = result {
        eprintln!("contests: announce {id}: {e}");
    }
    Redirect::to("/admin/contests").into_response()
}

/// A link to the contests for `/3d`, once there's one to see.
pub async fn link_html(pool: &PgPool) -> String {
    let open: Option<(String, String)> = sqlx::query_as(
        "SELECT slug, title FROM mf_contests WHERE status = 'open' AND closes_at > now() ORDER BY closes_at LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    match open {
        Some((s, title)) => format!(
            r#"<p><a href="/contests/{}">Contest: {}</a> · <a href="/contests">All contests</a></p>"#,
            escape(&s),
            escape(&title)
        ),
        None => {
            let any: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mf_contests WHERE status <> 'draft')")
                .fetch_one(pool)
                .await
                .unwrap_or(false);
            if any {
                r#"<p><a href="/contests">Contests</a></p>"#.to_string()
            } else {
                String::new()
            }
        }
    }
}
