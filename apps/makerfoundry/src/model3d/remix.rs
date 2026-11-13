//! Remixes (MF-47): a 3D model may name the models it was made from (its parents), and its
//! licence must be one every parent's licence allows for adaptations (docs/3d-hub-plan.md § 6,
//! from the licences' own terms and the Creative Commons FAQ; the rules to be confirmed by the
//! owner, ideally with a lawyer). A creator may always remix their own models.

/// What a remix of a model under `parent` may be licensed as: `None` for any licence, else the
/// list (empty: adaptations can't be shared).
pub fn allowed(parent: &str) -> Option<&'static [&'static str]> {
    match parent {
        "cc0" | "mit" | "bsd-3-clause" | "apache-2.0" | "cern-ohl-p" | "cc-by" => None,
        "cc-by-sa" => Some(&["cc-by-sa"]),
        "cc-by-nc" => Some(&["cc-by-nc", "cc-by-nc-sa"]),
        "cc-by-nc-sa" => Some(&["cc-by-nc-sa"]),
        "gpl-2.0-or-later" => Some(&["gpl-2.0-or-later", "gpl-3.0-or-later"]),
        "gpl-3.0-or-later" => Some(&["gpl-3.0-or-later"]),
        "lgpl-2.1-or-later" => Some(&[
            "lgpl-2.1-or-later",
            "lgpl-3.0-or-later",
            "gpl-2.0-or-later",
            "gpl-3.0-or-later",
        ]),
        "lgpl-3.0-or-later" => Some(&["lgpl-3.0-or-later", "gpl-3.0-or-later"]),
        "cern-ohl-s" => Some(&["cern-ohl-s"]),
        "cern-ohl-w" => Some(&["cern-ohl-w", "cern-ohl-s"]),
        // ND licences, MakerFoundry's own, all rights reserved, and anything unknown
        _ => Some(&[]),
    }
}

/// Whether a remix licensed `child` may be made from a parent licensed `parent` (`None`: the
/// parent has no licence yet) by someone who did (`own`) or didn't make the parent.
pub fn may(parent: Option<&str>, child: &str, own: bool) -> bool {
    own || parent.and_then(allowed).is_none_or(|list| list.contains(&child)) && parent.is_some()
}

/// A parent as the rules need it: its title, licence, and whether the remixer made it.
pub struct Parent<'a> {
    pub title: &'a str,
    pub licence: Option<&'a str>,
    pub own: bool,
}

/// Whether some licence satisfies every parent (any parent's own aside).
pub fn possible(parents: &[Parent]) -> bool {
    crate::licences::for_kind("model3d")
        .iter()
        .any(|l| parents.iter().all(|p| may(p.licence, l.id, p.own)))
}

/// Checks a remix's licence against its parents → why not, naming the first parent it breaks.
pub fn check(parents: &[Parent], child: &str) -> Result<(), String> {
    match parents.iter().find(|p| !may(p.licence, child, p.own)) {
        None => Ok(()),
        Some(p) => {
            let name = |id: &str| crate::licences::find(id).map_or(id.to_string(), |l| l.name.to_string());
            Err(match p.licence {
                None => format!("{} has no licence yet, so it can't be remixed.", p.title),
                Some(l) => match allowed(l) {
                    Some([]) => format!("{}'s licence doesn't allow remixes to be shared.", p.title),
                    Some(list) => format!(
                        "{} is licensed {}, so a remix of it must be licensed {}.",
                        p.title,
                        name(l),
                        list.iter().map(|l| name(l)).collect::<Vec<_>>().join(" or ")
                    ),
                    None => format!("{} can't be remixed under this licence.", p.title),
                },
            })
        }
    }
}

// ---------------------------------------------------------------- remixes kept and shown

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{current_user, forbidden, private, same_origin};
use crate::pages::{escape, page};

/// How many generations the tree shows each way.
pub const GENERATIONS: i32 = 10;
/// The most parents a model may name.
pub const MAX_PARENTS: i64 = 20;

/// A model linked to another: id, title, licence, maker account, maker's name, status.
pub struct Linked {
    pub id: i64,
    pub title: String,
    pub licence: Option<String>,
    pub account: i64,
    pub maker: String,
    pub status: String,
}

type LinkedRow = (i64, String, Option<String>, i64, Option<String>, String);

fn linked(rows: Vec<LinkedRow>) -> Vec<Linked> {
    rows.into_iter()
        .map(|(id, title, licence, account, maker, status)| Linked {
            id,
            title,
            licence,
            account,
            maker: maker.unwrap_or_default(),
            status,
        })
        .collect()
}

/// A model's parents.
pub async fn parents(pool: &PgPool, id: i64) -> Vec<Linked> {
    linked(
        sqlx::query_as(
            "SELECT d.id, d.title, d.licence, d.account_id, m.name, d.status
             FROM mf_remixes r JOIN mf_designs d ON d.id = r.parent_id LEFT JOIN mf_makers m ON m.account_id = d.account_id
             WHERE r.child_id = $1 ORDER BY r.created_at",
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap_or_default(),
    )
}

/// A model's published remixes.
pub async fn remixes(pool: &PgPool, id: i64) -> Vec<Linked> {
    linked(
        sqlx::query_as(
            "SELECT d.id, d.title, d.licence, d.account_id, m.name, d.status
             FROM mf_remixes r JOIN mf_designs d ON d.id = r.child_id LEFT JOIN mf_makers m ON m.account_id = d.account_id
             WHERE r.parent_id = $1 AND d.status = 'published' ORDER BY d.published_at DESC LIMIT 100",
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap_or_default(),
    )
}

/// Checks a licence for model `id` (made by `account`) against its parents.
pub async fn check_licence(pool: &PgPool, id: i64, account: i64, licence: &str) -> Result<(), String> {
    let list = parents(pool, id).await;
    let rules: Vec<Parent> = list
        .iter()
        .map(|p| Parent {
            title: &p.title,
            licence: p.licence.as_deref(),
            own: p.account == account,
        })
        .collect();
    check(&rules, licence)
}

/// A linked model as a line: its title (a link unless it's taken down or not public), its maker
/// and licence.
fn line(l: &Linked) -> String {
    let title = if l.status == "published" {
        format!(
            r#"<a href="/d/{id}/{slug}">{t}</a>"#,
            id = l.id,
            slug = crate::listings::slug(&l.title),
            t = escape(&l.title)
        )
    } else {
        escape(&l.title)
    };
    let by = if l.maker.is_empty() {
        String::new()
    } else {
        format!(" by {}", escape(&l.maker))
    };
    let licence = l
        .licence
        .as_deref()
        .and_then(crate::licences::find)
        .map(|x| format!(r#" <span class="muted">· {}</span>"#, escape(x.name)))
        .unwrap_or_default();
    format!("{title}{by}{licence}")
}

/// "Remix of …" and "Remixes" for a model's page, with a link to the whole tree.
pub async fn section(pool: &PgPool, id: i64) -> String {
    let (ps, rs) = (parents(pool, id).await, remixes(pool, id).await);
    if ps.is_empty() && rs.is_empty() {
        return String::new();
    }
    let list = |v: &[Linked]| v.iter().map(|l| format!("<li>{}</li>", line(l))).collect::<String>();
    let mut html = String::new();
    if !ps.is_empty() {
        html.push_str(&format!(r#"<h2>Remix of</h2><ul class="remixes">{}</ul>"#, list(&ps)));
    }
    if !rs.is_empty() {
        html.push_str(&format!(r#"<h2>Remixes</h2><ul class="remixes">{}</ul>"#, list(&rs)));
    }
    html.push_str(&format!(r#"<p><a href="/3d/{id}/tree">The whole remix tree</a></p>"#));
    html
}

/// The maker's part of their model's page: its parents, taking one off, and naming another.
pub async fn manage_section(pool: &PgPool, id: i64) -> String {
    let ps = parents(pool, id).await;
    let items: String = ps
        .iter()
        .map(|p| {
            format!(
                r#"<li>{line} <form method="post" action="/3d/{id}/parents/{pid}/delete" class="inline"><button type="submit" class="secondary small">Take off</button></form></li>"#,
                line = line(p),
                pid = p.id
            )
        })
        .collect();
    let list = if items.is_empty() {
        String::new()
    } else {
        format!(r#"<ul class="remixes">{items}</ul>"#)
    };
    format!(
        r#"<h2>Remix of</h2>{list}<form method="post" action="/3d/{id}/parents" class="form">
<label>Made from another model? <small>(its address on MakerFoundry, or its number)</small><input name="parent" required maxlength="300"></label>
<button type="submit" class="secondary">Add it</button>
</form>"#
    )
}

/// A model's number from what was typed: `123`, or an address with `/d/123`.
fn model_number(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(n) = text.parse::<i64>() {
        return Some(n);
    }
    let after = text.split("/d/").nth(1)?;
    after.split(['/', '?', '#']).next()?.parse().ok()
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct ParentForm {
    parent: String,
}

/// `POST /3d/<id>/parents`: names a model this one was made from: a published 3D model, or one of
/// the maker's own; never itself or one of its own remixes; only if some licence still satisfies
/// every parent.
pub async fn add_parent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    axum::Form(f): axum::Form<ParentForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool, model) = match super::upload::own_model(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let again = |why: String| {
        let (state, pool, user) = (&state, &pool, &user);
        async move { crate::designs::manage_page(state, pool, user, id, &why).await }
    };
    let found = match model_number(&f.parent) {
        Some(n) => crate::designs::design(&pool, n).await.ok().flatten().map(|d| (n, d)),
        None => None,
    };
    let Some((pid, parent)) = found.filter(|(_, d)| {
        d.kind == "model3d" && (d.status == "published" || (d.account_id == user.account_id && d.status != "removed"))
    }) else {
        return again("No published 3D model has that address or number.".into()).await;
    };
    // a cycle: the parent is this model, or descends from it
    let cycle: bool = sqlx::query_scalar(
        "WITH RECURSIVE up(id, depth) AS (
             SELECT parent_id, 1 FROM mf_remixes WHERE child_id = $2
             UNION SELECT r.parent_id, up.depth + 1 FROM mf_remixes r JOIN up ON r.child_id = up.id WHERE up.depth < 100)
         SELECT $1 = $2 OR EXISTS (SELECT 1 FROM up WHERE id = $1)",
    )
    .bind(id)
    .bind(pid)
    .fetch_one(&pool)
    .await
    .unwrap_or(true);
    if cycle {
        return again("A model can't be named as made from itself or from its own remix.".into()).await;
    }
    let mut list = parents(&pool, id).await;
    if list.iter().any(|p| p.id == pid) {
        return Redirect::to(&format!("/designs/{id}")).into_response();
    }
    if list.len() as i64 >= MAX_PARENTS {
        return again(format!("A model can name up to {MAX_PARENTS} models it was made from.")).await;
    }
    let own = parent.account_id == user.account_id;
    if !own && allowed(parent.licence.as_deref().unwrap_or("")).is_some_and(<[&str]>::is_empty) {
        return again(format!(
            "{}'s licence doesn't allow remixes to be shared.",
            parent.title
        ))
        .await;
    }
    list.push(Linked {
        id: pid,
        title: parent.title.clone(),
        licence: parent.licence.clone(),
        account: parent.account_id,
        maker: String::new(),
        status: parent.status.clone(),
    });
    let rules: Vec<Parent> = list
        .iter()
        .map(|p| Parent {
            title: &p.title,
            licence: p.licence.as_deref(),
            own: p.account == user.account_id,
        })
        .collect();
    if !possible(&rules) {
        return again(format!(
            "There's no licence a remix of all of these could have, so {} can't be added.",
            parent.title
        ))
        .await;
    }
    // a model already out (not a draft, which is checked when published) must keep to every
    // parent's terms under the licence it has now (issue #135)
    if model.status != "draft"
        && let Err(why) = check(&rules, model.licence.as_deref().unwrap_or(""))
    {
        return again(why).await;
    }
    if let Err(e) = sqlx::query("INSERT INTO mf_remixes (child_id, parent_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(pid)
        .execute(&pool)
        .await
    {
        eprintln!("remix {id} ← {pid}: {e}");
    } else {
        crate::notices::remix(&state, &pool, id, user.account_id).await;
    }
    Redirect::to(&format!("/designs/{id}")).into_response()
}

/// `POST /3d/<id>/parents/<parent>/delete`.
pub async fn remove_parent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, parent)): Path<(i64, i64)>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (_, pool, _) = match super::upload::own_model(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let _ = sqlx::query("DELETE FROM mf_remixes WHERE child_id = $1 AND parent_id = $2")
        .bind(id)
        .bind(parent)
        .execute(&pool)
        .await;
    Redirect::to(&format!("/designs/{id}")).into_response()
}

/// One model in a tree: generations away (negative: ancestors), and the model.
type TreeRow = (i32, i64, String, Option<String>, i64, Option<String>, String);

/// `GET /3d/<id>/tree`: a model's ancestors and descendants, up to ten generations each way;
/// those not public to the viewer left out, taken-down ones named without a link.
pub async fn tree(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let viewer = current_user(&state, &headers).await;
    let Some(pool) = state.db.as_ref() else {
        return crate::designs::not_found();
    };
    let d = match crate::designs::design(pool, id).await {
        Ok(Some(d)) if d.kind == "model3d" && crate::listings::visible(&d, viewer.as_ref(), &state.admins) => d,
        _ => return crate::designs::not_found(),
    };
    if !super::open_to(&state, viewer.as_ref()).await {
        return crate::designs::not_found();
    }
    let rows: Vec<TreeRow> = sqlx::query_as(
        "WITH RECURSIVE
           up(id, gen) AS (SELECT parent_id, -1 FROM mf_remixes WHERE child_id = $1
                           UNION SELECT r.parent_id, up.gen - 1 FROM mf_remixes r JOIN up ON r.child_id = up.id WHERE up.gen > -$2),
           down(id, gen) AS (SELECT child_id, 1 FROM mf_remixes WHERE parent_id = $1
                             UNION SELECT r.child_id, down.gen + 1 FROM mf_remixes r JOIN down ON r.parent_id = down.id WHERE down.gen < $2),
           near AS (SELECT id, max(gen) AS gen FROM up GROUP BY id
                    UNION ALL SELECT id, min(gen) FROM down GROUP BY id)
         SELECT near.gen, d.id, d.title, d.licence, d.account_id, m.name, d.status
         FROM near JOIN mf_designs d ON d.id = near.id LEFT JOIN mf_makers m ON m.account_id = d.account_id
         ORDER BY near.gen, d.title LIMIT 500",
    )
    .bind(id)
    .bind(GENERATIONS)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let viewer_id = viewer.as_ref().map(|u| u.account_id);
    let show = |r: &&TreeRow| r.6 == "published" || r.6 == "removed" || Some(r.4) == viewer_id;
    let list = |rows: Vec<&TreeRow>| -> String {
        rows.iter()
            .map(|r| {
                let l = Linked {
                    id: r.1,
                    title: r.2.clone(),
                    licence: r.3.clone(),
                    account: r.4,
                    maker: r.5.clone().unwrap_or_default(),
                    status: r.6.clone(),
                };
                let n = r.0.abs();
                format!(
                    r#"<li><span class="muted">{n} generation{s} {way}:</span> {line}</li>"#,
                    s = if n == 1 { "" } else { "s" },
                    way = if r.0 < 0 { "back" } else { "on" },
                    line = line(&l)
                )
            })
            .collect()
    };
    let ancestors: Vec<&TreeRow> = rows.iter().filter(|r| r.0 < 0).filter(show).collect();
    let descendants: Vec<&TreeRow> = rows.iter().filter(|r| r.0 > 0).filter(show).collect();
    let part = |title: &str, rows: Vec<&TreeRow>| {
        if rows.is_empty() {
            String::new()
        } else {
            format!(r#"<h2>{title}</h2><ul class="remixes">{}</ul>"#, list(rows))
        }
    };
    let body = format!(
        r#"<section class="design"><h1>Remix tree: {title}</h1><p><a href="/d/{id}/{slug}">Back to the model</a></p>{a}{b}{none}</section>"#,
        title = escape(&d.title),
        slug = crate::listings::slug(&d.title),
        a = part("Made from", ancestors.clone()),
        b = part("Remixed into", descendants.clone()),
        none = if ancestors.is_empty() && descendants.is_empty() {
            "<p>No remixes yet.</p>"
        } else {
            ""
        },
    );
    let title = format!("Remix tree: {} · MakerFoundry", escape(&d.title));
    if d.status == "published" {
        Html(page(&title, &body, viewer.is_some())).into_response()
    } else {
        private(&title, &body, viewer.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules table of docs/3d-hub-plan.md § 6, written out again by hand.
    fn table(parent: &str, child: &str) -> bool {
        let any = ["cc0", "mit", "bsd-3-clause", "apache-2.0", "cern-ohl-p", "cc-by"];
        if any.contains(&parent) {
            return true;
        }
        let list: &[&str] = match parent {
            "cc-by-sa" => &["cc-by-sa"],
            "cc-by-nc" => &["cc-by-nc", "cc-by-nc-sa"],
            "cc-by-nc-sa" => &["cc-by-nc-sa"],
            "gpl-2.0-or-later" => &["gpl-2.0-or-later", "gpl-3.0-or-later"],
            "gpl-3.0-or-later" => &["gpl-3.0-or-later"],
            "lgpl-2.1-or-later" => &[
                "lgpl-2.1-or-later",
                "lgpl-3.0-or-later",
                "gpl-2.0-or-later",
                "gpl-3.0-or-later",
            ],
            "lgpl-3.0-or-later" => &["lgpl-3.0-or-later", "gpl-3.0-or-later"],
            "cern-ohl-s" => &["cern-ohl-s"],
            "cern-ohl-w" => &["cern-ohl-w", "cern-ohl-s"],
            _ => &[],
        };
        list.contains(&child)
    }

    #[test]
    fn every_pair_of_licences_follows_the_table() {
        let all = crate::licences::for_kind("model3d");
        let mut checked = 0;
        for p in &all {
            for c in &all {
                assert_eq!(may(Some(p.id), c.id, false), table(p.id, c.id), "{} → {}", p.id, c.id);
                assert!(may(Some(p.id), c.id, true), "their own: {} → {}", p.id, c.id);
                checked += 1;
            }
        }
        assert_eq!(checked, 21 * 21);
    }

    #[test]
    fn the_rows_of_the_table() {
        // a remix of an ND model is refused, whatever its licence
        assert!(
            crate::licences::for_kind("model3d")
                .iter()
                .all(|c| !may(Some("cc-by-nd"), c.id, false))
        );
        assert!(!may(Some("cc-by-nc-nd"), "cc-by-nc-nd", false));
        // BY-SA forces BY-SA
        assert!(may(Some("cc-by-sa"), "cc-by-sa", false) && !may(Some("cc-by-sa"), "cc-by", false));
        // GPL 3 refuses GPL 2; GPL 2 or later allows 3
        assert!(!may(Some("gpl-3.0-or-later"), "gpl-2.0-or-later", false));
        assert!(may(Some("gpl-2.0-or-later"), "gpl-3.0-or-later", false));
        // CERN W allows W or S, not P
        assert!(may(Some("cern-ohl-w"), "cern-ohl-w", false) && may(Some("cern-ohl-w"), "cern-ohl-s", false));
        assert!(!may(Some("cern-ohl-w"), "cern-ohl-p", false));
        // all rights reserved and MakerFoundry's own: only their maker
        for l in ["all-rights-reserved", "personal", "small-business", "commercial"] {
            assert!(!may(Some(l), "cc-by", false) && may(Some(l), "cc-by", true), "{l}");
        }
        // no licence yet: not remixable by others
        assert!(!may(None, "cc0", false) && may(None, "cc0", true));
    }

    #[test]
    fn model_numbers_come_from_addresses() {
        assert_eq!(model_number("123"), Some(123));
        assert_eq!(model_number(" https://makerfoundry.app/d/45/a-gear?x=1 "), Some(45));
        assert_eq!(model_number("https://makerfoundry.app/d/7"), Some(7));
        assert_eq!(model_number("https://example.com/things/9"), None);
        assert_eq!(model_number(""), None);
    }

    #[test]
    fn parents_with_no_common_licence_are_named() {
        let sa = Parent {
            title: "Gear",
            licence: Some("cc-by-sa"),
            own: false,
        };
        let nc_sa = Parent {
            title: "Hub",
            licence: Some("cc-by-nc-sa"),
            own: false,
        };
        assert!(!possible(&[sa, nc_sa]));
        let sa = Parent {
            title: "Gear",
            licence: Some("cc-by-sa"),
            own: false,
        };
        let by = Parent {
            title: "Base",
            licence: Some("cc-by"),
            own: false,
        };
        assert!(possible(&[sa, by]));
        let parents = [
            Parent {
                title: "Base",
                licence: Some("cc-by"),
                own: false,
            },
            Parent {
                title: "Gear",
                licence: Some("cc-by-sa"),
                own: false,
            },
        ];
        assert!(check(&parents, "cc-by-sa").is_ok());
        let why = check(&parents, "cc-by").unwrap_err();
        assert!(why.contains("Gear") && why.contains("ShareAlike"), "{why}");
        let nd = [Parent {
            title: "Vase",
            licence: Some("cc-by-nd"),
            own: false,
        }];
        assert!(
            check(&nd, "cc-by-nd")
                .unwrap_err()
                .contains("Vase's licence doesn't allow remixes")
        );
    }
}
