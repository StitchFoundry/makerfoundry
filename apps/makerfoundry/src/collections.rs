//! Saved designs and collections (MF-10): anyone signed in can save a published design to their
//! **Saved** list (private, made the first time) or to a collection of their own, which they may
//! make public (shown on their maker page, if they have one). A design shows how many people have
//! saved it, and browsing can put the most saved first.

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{User, current_user, forbidden, private, same_origin};
use crate::listings::{Filter, cards_html, find, slug};
use crate::pages::{escape, page};

/// Collections an account may have (Saved included).
pub const MAX_COLLECTIONS: i64 = 100;
/// Designs a collection may hold.
pub const MAX_ITEMS: i64 = 1000;
/// A collection's name, in characters.
pub const MAX_NAME: usize = 60;

/// A collection as its owner sees it in a list.
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub public: bool,
    pub saved: bool,
    pub items: i64,
}

/// `account`'s collections, Saved first.
pub async fn of(pool: &PgPool, account: i64, public_only: bool) -> Vec<Collection> {
    sqlx::query_as::<_, (i64, String, bool, bool, i64)>(
        "SELECT c.id, c.name, c.public, c.is_saved,
                (SELECT count(*) FROM mf_collection_items i JOIN mf_designs d ON d.id = i.design_id
                  WHERE i.collection_id = c.id AND d.status = 'published')
         FROM mf_collections c WHERE c.account_id = $1 AND (c.public OR NOT $2)
         ORDER BY c.is_saved DESC, c.created_at",
    )
    .bind(account)
    .bind(public_only)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, name, public, saved, items)| Collection {
        id,
        name,
        public,
        saved,
        items,
    })
    .collect()
}

/// How many people have saved design `id`.
pub async fn saves(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(DISTINCT c.account_id) FROM mf_collection_items i
         JOIN mf_collections c ON c.id = i.collection_id WHERE i.design_id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap_or(0)
}

/// The account's Saved list, made the first time.
async fn saved_list(pool: &PgPool, account: i64) -> Result<i64, sqlx::Error> {
    sqlx::query(
        "INSERT INTO mf_collections (account_id, name, is_saved) VALUES ($1, 'Saved', true)
         ON CONFLICT (account_id) WHERE is_saved DO NOTHING",
    )
    .bind(account)
    .execute(pool)
    .await?;
    sqlx::query_scalar("SELECT id FROM mf_collections WHERE account_id = $1 AND is_saved")
        .bind(account)
        .fetch_one(pool)
        .await
}

/// The save part of a design's page: the button and where the viewer has saved it.
pub async fn save_html(pool: &PgPool, viewer: Option<&User>, id: i64) -> String {
    let Some(user) = viewer else {
        return format!(r#"<p class="muted"><a href="/signin?next=/d/{id}">Sign in to save this design</a></p>"#);
    };
    let mine = of(pool, user.account_id, false).await;
    let holding: Vec<(i64, String)> = sqlx::query_as(
        "SELECT c.id, c.name FROM mf_collection_items i JOIN mf_collections c ON c.id = i.collection_id
         WHERE i.design_id = $1 AND c.account_id = $2 ORDER BY c.is_saved DESC, c.created_at",
    )
    .bind(id)
    .bind(user.account_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let options: String = std::iter::once(r#"<option value="">Saved</option>"#.to_string())
        .chain(
            mine.iter()
                .filter(|c| !c.saved)
                .map(|c| format!(r#"<option value="{}">{}</option>"#, c.id, escape(&c.name))),
        )
        .collect();
    let held = if holding.is_empty() {
        String::new()
    } else {
        let links: Vec<String> = holding
            .iter()
            .map(|(cid, name)| format!(r#"<a href="/collections/{cid}">{}</a>"#, escape(name)))
            .collect();
        format!(r#"<p class="muted">In your {}.</p>"#, links.join(", "))
    };
    format!(
        r#"<form method="post" action="/d/{id}/save" class="save"><select name="collection" aria-label="Save to">{options}</select><button type="submit" class="secondary">Save</button></form>{held}"#
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SaveForm {
    collection: String,
}

/// Saves design `id` to the viewer's Saved list, or to one of their collections.
pub async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<SaveForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.as_ref()) else {
        return Redirect::to(&format!("/signin?next=/d/{id}")).into_response();
    };
    let published: Option<String> =
        sqlx::query_scalar("SELECT title FROM mf_designs WHERE id = $1 AND status = 'published'")
            .bind(id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let Some(title) = published else {
        return Redirect::to("/browse").into_response();
    };
    let collection = match f.collection.parse::<i64>() {
        Ok(cid) => {
            let own: Option<i64> =
                sqlx::query_scalar("SELECT id FROM mf_collections WHERE id = $1 AND account_id = $2")
                    .bind(cid)
                    .bind(user.account_id)
                    .fetch_optional(pool)
                    .await
                    .ok()
                    .flatten();
            match own {
                Some(c) => c,
                None => return Redirect::to(&format!("/d/{id}")).into_response(),
            }
        }
        Err(_) => match saved_list(pool, user.account_id).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("collections: saved list: {e}");
                return Redirect::to(&format!("/d/{id}")).into_response();
            }
        },
    };
    if let Err(e) = add_item(pool, collection, id).await {
        eprintln!("collections: save {id}: {e}");
    }
    Redirect::to(&format!("/d/{id}/{}", slug(&title))).into_response()
}

// ---------------------------------------------------------------- the viewer's collections

fn list_html(mine: &[Collection], problem: &str) -> String {
    let items: String = mine
        .iter()
        .map(|c| {
            format!(
                r#"<li><a href="/collections/{id}">{name}</a> <span class="muted">{n} design{s}{public}</span></li>"#,
                id = c.id,
                name = escape(&c.name),
                n = c.items,
                s = if c.items == 1 { "" } else { "s" },
                public = if c.public { " · public" } else { " · private" },
            )
        })
        .collect();
    let list = if mine.is_empty() {
        r#"<p class="lead">Nothing saved yet. Save a design from its page.</p>"#.to_string()
    } else {
        format!(r#"<ul class="files">{items}</ul>"#)
    };
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    };
    format!(
        r#"<section class="design"><h1>Your collections</h1>{list}
<h2>A new collection</h2>{problem}
<form method="post" action="/collections" class="add-file">
<label>Name<input name="name" required maxlength="{MAX_NAME}"></label>
<label class="choice"><input type="checkbox" name="public" value="yes"> <span>Public: anyone can see it, on your maker page too</span></label>
<button type="submit">Make it</button>
</form></section>"#
    )
}

pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.as_ref()) else {
        return Redirect::to("/signin?next=/collections").into_response();
    };
    private(
        "Your collections · MakerFoundry",
        &list_html(&of(pool, user.account_id, false).await, ""),
        true,
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CollectionForm {
    name: String,
    public: String,
}

/// A name as typed, tidied, or why it won't do.
fn clean_name(name: &str) -> Result<String, String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        Err(format!("Give a name of up to {MAX_NAME} characters."))
    } else {
        Ok(name)
    }
}

pub async fn create(State(state): State<AppState>, headers: HeaderMap, Form(f): Form<CollectionForm>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.as_ref()) else {
        return Redirect::to("/signin").into_response();
    };
    let mine = of(pool, user.account_id, false).await;
    let again = |why: &str| private("Your collections · MakerFoundry", &list_html(&mine, why), true);
    let name = match clean_name(&f.name) {
        Ok(n) => n,
        Err(why) => return again(&why),
    };
    let made = make(pool, user.account_id, &name, f.public == "yes").await;
    match made {
        Ok(Some(id)) => Redirect::to(&format!("/collections/{id}")).into_response(),
        Ok(None) => again(&format!("You can have up to {MAX_COLLECTIONS} collections.")),
        Err(e) => {
            eprintln!("collections: create: {e}");
            again("The collection couldn't be made just now. Please try again.")
        }
    }
}

/// Adds design `id` to `collection` (once), holding a lock on the collection so saves sent at the
/// same moment can't take it past `MAX_ITEMS` (issue #158). At the cap, nothing is added.
async fn add_item(pool: &PgPool, collection: i64, id: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("mf-collection-items-{collection}"))
        .execute(&mut *tx)
        .await?;
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_collection_items WHERE collection_id = $1")
        .bind(collection)
        .fetch_one(&mut *tx)
        .await?;
    if held < MAX_ITEMS {
        sqlx::query(
            "INSERT INTO mf_collection_items (collection_id, design_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(collection)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Makes a collection for `account`, holding a lock on the account so collections made at the
/// same moment can't take it past `MAX_COLLECTIONS` (issue #158) → its id, or `None` at the cap.
async fn make(pool: &PgPool, account: i64, name: &str, public: bool) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("mf-collections-{account}"))
        .execute(&mut *tx)
        .await?;
    let made: i64 = sqlx::query_scalar("SELECT count(*) FROM mf_collections WHERE account_id = $1")
        .bind(account)
        .fetch_one(&mut *tx)
        .await?;
    if made >= MAX_COLLECTIONS {
        tx.rollback().await?;
        return Ok(None);
    }
    let id: i64 =
        sqlx::query_scalar("INSERT INTO mf_collections (account_id, name, public) VALUES ($1, $2, $3) RETURNING id")
            .bind(account)
            .bind(name)
            .bind(public)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Some(id))
}

// ---------------------------------------------------------------- one collection

fn not_found(signed_in: bool) -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(page(
            "Not found · MakerFoundry",
            r#"<section class="hero"><h1>Collection not found</h1></section>"#,
            signed_in,
        )),
    )
        .into_response()
}

/// Collection `cid`: (owner, name, public, is Saved).
async fn collection(pool: &PgPool, cid: i64) -> Option<(i64, String, bool, bool)> {
    sqlx::query_as("SELECT account_id, name, public, is_saved FROM mf_collections WHERE id = $1")
        .bind(cid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

pub async fn show(State(state): State<AppState>, headers: HeaderMap, Path(cid): Path<i64>) -> Response {
    let viewer = current_user(&state, &headers).await;
    let signed_in = viewer.is_some();
    let Some(pool) = state.db.as_ref() else {
        return not_found(signed_in);
    };
    let Some((owner, name, public, saved)) = collection(pool, cid).await else {
        return not_found(signed_in);
    };
    let mine = viewer.as_ref().is_some_and(|u| u.account_id == owner);
    if !public && !mine {
        return not_found(signed_in);
    }
    let found = find(
        pool,
        &Filter {
            collection: Some(cid),
            three_d: state.three_d,
            ..Filter::default()
        },
        MAX_ITEMS,
    )
    .await;
    let maker = crate::makers::profile(pool, owner).await;
    let byline = match (&maker, mine) {
        (_, true) => format!(
            r#"<p class="byline">Yours · {} · <a href="/collections">your collections</a></p>"#,
            if public { "public" } else { "private: only you see it" }
        ),
        (Some(m), false) => format!(
            r#"<p class="byline">A collection by <a href="/makers/{h}">{n}</a></p>"#,
            h = escape(&m.handle),
            n = escape(&m.name)
        ),
        (None, false) => String::new(),
    };
    let cards = if found.is_empty() {
        r#"<p class="lead">No designs in it yet.</p>"#.to_string()
    } else {
        cards_html(&found)
    };
    let manage = if mine {
        let removes: String = found
            .iter()
            .map(|c| {
                format!(
                    r#"<li>{t} <form method="post" action="/collections/{cid}/remove/{id}"><button type="submit" class="secondary">Remove</button></form></li>"#,
                    t = escape(&c.title),
                    id = c.id
                )
            })
            .collect();
        let settings = if saved {
            String::new()
        } else {
            format!(
                r#"<h2>Name and who sees it</h2><form method="post" action="/collections/{cid}/edit" class="add-file">
<label>Name<input name="name" value="{n}" required maxlength="{MAX_NAME}"></label>
<label class="choice"><input type="checkbox" name="public" value="yes"{p}> <span>Public: anyone can see it, on your maker page too</span></label>
<button type="submit" class="secondary">Save</button></form>
<form method="post" action="/collections/{cid}/delete"><button type="submit" class="secondary">Delete this collection</button></form>"#,
                n = escape(&name),
                p = if public { " checked" } else { "" },
            )
        };
        let removes = if removes.is_empty() {
            String::new()
        } else {
            format!(r#"<h2>Take designs out</h2><ul class="files">{removes}</ul>"#)
        };
        format!("{removes}{settings}")
    } else {
        String::new()
    };
    let body = format!(
        r#"<section class="design"><h1>{n}</h1>{byline}{cards}{manage}</section>"#,
        n = escape(&name)
    );
    if public {
        Html(page(&format!("{} · MakerFoundry", escape(&name)), &body, signed_in)).into_response()
    } else {
        private(&format!("{} · MakerFoundry", escape(&name)), &body, signed_in)
    }
}

/// The viewer's own collection `cid`, or the response to give instead.
async fn own(state: &AppState, headers: &HeaderMap, cid: i64) -> Result<(PgPool, bool), Response> {
    if !same_origin(headers) {
        return Err(forbidden());
    }
    let (Some(user), Some(pool)) = (current_user(state, headers).await, state.db.clone()) else {
        return Err(Redirect::to("/signin").into_response());
    };
    match collection(&pool, cid).await {
        Some((owner, _, _, saved)) if owner == user.account_id => Ok((pool, saved)),
        _ => Err(not_found(true)),
    }
}

pub async fn remove(State(state): State<AppState>, headers: HeaderMap, Path((cid, id)): Path<(i64, i64)>) -> Response {
    let pool = match own(&state, &headers, cid).await {
        Ok((p, _)) => p,
        Err(r) => return r,
    };
    if let Err(e) = sqlx::query("DELETE FROM mf_collection_items WHERE collection_id = $1 AND design_id = $2")
        .bind(cid)
        .bind(id)
        .execute(&pool)
        .await
    {
        eprintln!("collections: remove: {e}");
    }
    Redirect::to(&format!("/collections/{cid}")).into_response()
}

pub async fn edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(cid): Path<i64>,
    Form(f): Form<CollectionForm>,
) -> Response {
    let (pool, saved) = match own(&state, &headers, cid).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    // (Saved keeps its name and stays private)
    if let (false, Ok(name)) = (saved, clean_name(&f.name))
        && let Err(e) = sqlx::query("UPDATE mf_collections SET name = $2, public = $3 WHERE id = $1")
            .bind(cid)
            .bind(&name)
            .bind(f.public == "yes")
            .execute(&pool)
            .await
    {
        eprintln!("collections: edit: {e}");
    }
    Redirect::to(&format!("/collections/{cid}")).into_response()
}

pub async fn delete(State(state): State<AppState>, headers: HeaderMap, Path(cid): Path<i64>) -> Response {
    let (pool, saved) = match own(&state, &headers, cid).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    if !saved
        && let Err(e) = sqlx::query("DELETE FROM mf_collections WHERE id = $1")
            .bind(cid)
            .execute(&pool)
            .await
    {
        eprintln!("collections: delete: {e}");
    }
    Redirect::to("/collections").into_response()
}

/// A maker's public collections, for their page (empty without any).
pub async fn public_html(pool: &PgPool, account: i64) -> String {
    let public = of(pool, account, true).await;
    if public.is_empty() {
        return String::new();
    }
    let items: String = public
        .iter()
        .map(|c| {
            format!(
                r#"<li><a href="/collections/{id}">{name}</a> <span class="muted">{n} design{s}</span></li>"#,
                id = c.id,
                name = escape(&c.name),
                n = c.items,
                s = if c.items == 1 { "" } else { "s" },
            )
        })
        .collect();
    format!(r#"<h2>Collections</h2><ul class="files">{items}</ul>"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_tidied_and_checked() {
        assert_eq!(clean_name("  Christmas   ideas "), Ok("Christmas ideas".to_string()));
        assert!(clean_name("").is_err());
        assert!(clean_name(&"x".repeat(61)).is_err());
        assert!(clean_name("a\u{7}b").is_err());
    }
}
