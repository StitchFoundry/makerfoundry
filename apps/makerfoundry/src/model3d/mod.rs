//! MakerFoundry 3D (MF-35…): printable models, their categories, and (in the modules beside this
//! one) the readers for the files makers upload. See docs/3d-hub-plan.md.

pub mod api;
pub mod contests;
pub mod files;
pub mod import;
pub mod likes;
pub mod mesh;
pub mod obj;
pub mod origin;
pub mod points;
pub mod profile;
pub mod remix;
pub mod render;
pub mod scad;
pub mod search;
pub mod showcase;
pub mod social;
pub mod step;
pub mod stl;
pub mod thingiverse;
pub mod threemf;
pub mod upload;
pub mod viewer;
pub mod zip;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};

use crate::AppState;
use crate::accounts::{User, current_user, private};
use crate::pages::page;

/// A 3D category: `id` is kept in the database, `name` is shown.
#[derive(Debug, PartialEq, Eq)]
pub struct Category {
    pub id: &'static str,
    pub name: &'static str,
}

/// The 14 categories (owner's decision, docs/3d-hub-plan.md § 14).
pub const CATEGORIES: &[Category] = &[
    Category {
        id: "household",
        name: "Household",
    },
    Category {
        id: "tools-workshop",
        name: "Tools & workshop",
    },
    Category {
        id: "organisers-storage",
        name: "Organisers & storage",
    },
    Category {
        id: "toys-games",
        name: "Toys & games",
    },
    Category {
        id: "cosplay-props",
        name: "Cosplay & props",
    },
    Category {
        id: "miniatures-tabletop",
        name: "Miniatures & tabletop",
    },
    Category {
        id: "art-decor",
        name: "Art & decor",
    },
    Category {
        id: "fashion-jewellery",
        name: "Fashion & jewellery",
    },
    Category {
        id: "gadgets-tech-mounts",
        name: "Gadgets & tech mounts",
    },
    Category {
        id: "education",
        name: "Education",
    },
    Category {
        id: "garden-outdoor",
        name: "Garden & outdoor",
    },
    Category {
        id: "automotive",
        name: "Automotive",
    },
    Category {
        id: "printer-parts",
        name: "3D printer parts & upgrades",
    },
    Category {
        id: "other",
        name: "Other",
    },
];

/// A category by id.
pub fn category(id: &str) -> Option<&'static Category> {
    CATEGORIES.iter().find(|c| c.id == id)
}

/// Whether `viewer` may see and use MakerFoundry 3D: everyone once it's switched on (`MF_3D=yes`);
/// before, admins and founding creators (their early access, MF-54).
pub async fn open_to(state: &AppState, viewer: Option<&User>) -> bool {
    if state.three_d {
        return true;
    }
    let Some(u) = viewer else { return false };
    if state.admins.contains(&u.account_id) {
        return true;
    }
    match state.db.as_ref() {
        Some(pool) => crate::founding::number(pool, u.account_id).await.is_some(),
        None => false,
    }
}

/// `/3d`: the 3D section's front page.
pub async fn landing(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<search::Query>,
) -> Response {
    let viewer = current_user(&state, &headers).await;
    if !open_to(&state, viewer.as_ref()).await {
        return crate::not_found().await.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return crate::not_found().await.into_response();
    };
    let (cards, next) = search::find(pool, &q).await;
    let results = if cards.is_empty() {
        r#"<p class="lead">No models found. Try fewer filters.</p>"#.to_string()
    } else {
        crate::listings::cards_html(&cards)
    };
    let more = match next {
        Some(after) => format!(r#"<p><a href="{}">More models</a></p>"#, search::next_href(&q, after)),
        None => String::new(),
    };
    let form = search::form_html(&q);
    let contests = contests::link_html(pool).await;
    let share = if viewer.is_some() {
        r#"<p class="actions"><a class="button" href="/3d/new">Share a model</a> <a class="button secondary" href="/3d/import">Import my models</a></p>"#
    } else {
        r#"<p class="actions"><a class="button" href="/signin?next=/3d/new">Share a model</a> <a class="button secondary" href="/signin?next=/3d/import">Import my models</a></p>"#
    };
    let preview = if state.three_d {
        ""
    } else {
        r#"<p class="note">Only admins see this until MakerFoundry 3D opens.</p>"#
    };
    let body = format!(
        r#"<section class="hero">
  <h1>MakerFoundry 3D</h1>
  <p class="lead">Printable models shared and sold by the makers who designed them: STL, 3MF, OBJ and STEP, with the print settings that worked.</p>
  {share}
  {contests}
  {preview}
</section>
<section class="design">
  {form}
  {results}
  {more}
</section>"#
    );
    let title = "MakerFoundry 3D · MakerFoundry";
    match viewer {
        // (the admins' look before opening isn't for caches)
        Some(_) if !state.three_d => private(title, &body, true),
        Some(_) => Html(page(title, &body, true)).into_response(),
        None => Html(page(title, &body, false)).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_category_is_accepted_and_nothing_else() {
        assert_eq!(CATEGORIES.len(), 14);
        for c in CATEGORIES {
            assert_eq!(category(c.id).map(|f| f.name), Some(c.name));
        }
        assert!(category("nope").is_none());
        assert!(category("").is_none());
        let mut ids: Vec<_> = CATEGORIES.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CATEGORIES.len());
    }
}
