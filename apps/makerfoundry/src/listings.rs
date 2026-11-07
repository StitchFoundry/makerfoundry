//! Listings (MF-3): a draft gets its details (title, description, tags, licence) and is
//! published; published designs have a public page (`/d/<id>`), a free download, and are found
//! by browsing and searching (`/browse`), on their maker's page and on the home page.
//!
//! Until the public launch (MF-6, with the takedown process of MF-5 in place), publishing is open
//! only to the accounts in `MF_PUBLISHERS` (account ids, or `*` for everyone).

use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use sqlx::{PgPool, Row};

use crate::AppState;
use crate::accounts::{User, current_user, forbidden, private, same_origin};
use crate::designs::{self, DesignRow};
use crate::licences;
use crate::makers;
use crate::money::dollars;
use crate::pages::{escape, page};
use crate::selling;
use crate::sizes::{self, HOOPS, Hoop, MATERIALS};

pub const MAX_TITLE: usize = 100;
pub const MAX_DESCRIPTION: usize = 5000;
pub const MAX_TAGS: usize = 10;
pub const MAX_TAG: usize = 30;
/// Designs on a page of browsing.
const PAGE_SIZE: i64 = 24;

/// Who may publish.
#[derive(Clone, Debug, PartialEq)]
pub enum Publishers {
    Everyone,
    Only(Vec<i64>),
}

impl Publishers {
    /// From `MF_PUBLISHERS`: `*`, or account ids apart by commas; nobody without it.
    pub fn from_setting(value: Option<&str>) -> Publishers {
        match value.map(str::trim) {
            Some("*") => Publishers::Everyone,
            Some(list) => Publishers::Only(list.split(',').filter_map(|s| s.trim().parse().ok()).collect()),
            None => Publishers::Only(Vec::new()),
        }
    }

    pub fn allows(&self, account_id: i64) -> bool {
        match self {
            Publishers::Everyone => true,
            Publishers::Only(ids) => ids.contains(&account_id),
        }
    }
}

/// Tags as typed (apart by commas) made tidy: lower case, letters, digits, spaces and hyphens,
/// each at most `MAX_TAG`, no repeats, at most `MAX_TAGS`.
pub fn tidy_tags(text: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for raw in text.split(',') {
        let tag: String = raw
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
            .collect();
        let tag = tag
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_TAG)
            .collect::<String>();
        if !tag.is_empty() && !tags.contains(&tag) && tags.len() < MAX_TAGS {
            tags.push(tag);
        }
    }
    tags
}

/// Plain text as HTML paragraphs (a blank line apart), escaped, line breaks kept.
pub fn paragraphs(text: &str) -> String {
    text.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!("<p>{}</p>", escape(p).replace('\n', "<br>")))
        .collect()
}

/// A design's address words: its title in lower case with hyphens.
pub fn slug(title: &str) -> String {
    let s: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.split('-')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(60)
        .collect()
}

// ---------------------------------------------------------------- finding designs

/// What to find: published designs, narrowed by any of these.
#[derive(Default, Debug)]
pub struct Filter {
    pub kind: Option<String>,
    pub tag: Option<String>,
    pub words: Option<String>,
    pub account: Option<i64>,
    /// `Some(true)`: free designs only; `Some(false)`: paid only.
    pub free: Option<bool>,
    /// Embroidery designs that fit this hoop.
    pub hoop: Option<&'static Hoop>,
    /// Laser designs for this material.
    pub material: Option<String>,
    pub popular: bool,
    /// The most saved first.
    pub most_saved: bool,
    /// Designs in this collection (`collections`).
    pub collection: Option<i64>,
    /// Designs by the makers this account follows (`follows`).
    pub followed_by: Option<i64>,
    /// Whether 3D models are shown (MF-42: once MakerFoundry 3D is on).
    pub three_d: bool,
    /// Only designs by founding creators.
    pub founding_only: bool,
    pub page: i64,
}

/// A published design as a card shows it.
pub struct Card {
    pub id: i64,
    pub title: String,
    pub kind: String,
    pub format: String,
    pub photo: Option<i64>,
    /// A 3D model's first file with a picture, shown when it has no photo.
    pub model_file: Option<i64>,
    pub maker: String,
    pub downloads: i64,
    pub price: i64,
    /// The maker's founding creator number, if they are one.
    pub founding: Option<i32>,
}

pub async fn find(pool: &PgPool, f: &Filter, limit: i64) -> Vec<Card> {
    let rows = sqlx::query(
        "SELECT d.id, d.title, d.kind, d.format, d.downloads, d.price_cents, m.name AS maker,
                (SELECT p.id FROM mf_design_photos p WHERE p.design_id = d.id ORDER BY p.position LIMIT 1) AS photo,
                (SELECT f.id FROM mf_model_files f WHERE f.design_id = d.id AND f.thumb_key IS NOT NULL ORDER BY f.position LIMIT 1) AS model_file,
                (SELECT w.number FROM mf_founding w WHERE w.account_id = d.account_id AND w.status = 'approved') AS founding
         FROM mf_designs d JOIN mf_makers m ON m.account_id = d.account_id
         WHERE d.status = 'published'
           AND (d.kind <> 'model3d' OR $15)
           AND (NOT $16 OR EXISTS (SELECT 1 FROM mf_founding w WHERE w.account_id = d.account_id AND w.status = 'approved'))
           AND ($1::text IS NULL OR d.kind = $1)
           AND ($2::text IS NULL OR $2 = ANY (d.tags))
           AND ($3::text IS NULL OR d.search @@ websearch_to_tsquery('english', $3))
           AND ($4::bigint IS NULL OR d.account_id = $4)
           AND ($8::bool IS NULL OR (d.price_cents = 0) = $8)
           AND ($9::float8 IS NULL OR (d.kind = 'embroidery'
                AND ((d.width_mm <= $9 AND d.height_mm <= $10) OR (d.width_mm <= $10 AND d.height_mm <= $9))))
           AND ($11::text IS NULL OR (d.kind = 'laser' AND d.material = $11))
           AND ($12::bigint IS NULL OR d.id IN (SELECT design_id FROM mf_collection_items WHERE collection_id = $12))
           AND ($14::bigint IS NULL OR d.account_id IN (SELECT maker_account FROM mf_follows WHERE follower_account = $14))
         ORDER BY CASE WHEN $13 THEN (SELECT count(DISTINCT c.account_id) FROM mf_collection_items i
                                      JOIN mf_collections c ON c.id = i.collection_id WHERE i.design_id = d.id) END DESC NULLS LAST,
                  CASE WHEN $5 THEN d.downloads END DESC NULLS LAST,
                  CASE WHEN $3::text IS NOT NULL AND NOT $5 THEN ts_rank(d.search, websearch_to_tsquery('english', $3)) END DESC NULLS LAST,
                  d.published_at DESC
         LIMIT $6 OFFSET $7",
    )
    .bind(&f.kind)
    .bind(&f.tag)
    .bind(&f.words)
    .bind(f.account)
    .bind(f.popular)
    .bind(limit)
    .bind(f.page.max(0).saturating_mul(limit))
    .bind(f.free)
    .bind(f.hoop.map(|h| h.width_mm))
    .bind(f.hoop.map(|h| h.height_mm))
    .bind(&f.material)
    .bind(f.collection)
    .bind(f.most_saved)
    .bind(f.followed_by)
    .bind(f.three_d)
    .bind(f.founding_only)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| {
                Some(Card {
                    id: r.try_get("id").ok()?,
                    title: r.try_get("title").ok()?,
                    kind: r.try_get("kind").ok()?,
                    format: r.try_get("format").ok()?,
                    photo: r.try_get("photo").ok()?,
                    model_file: r.try_get("model_file").ok()?,
                    maker: r.try_get("maker").ok()?,
                    downloads: r.try_get("downloads").ok()?,
                    price: r.try_get("price_cents").ok()?,
                    founding: r.try_get("founding").ok()?,
                })
            })
            .collect(),
        Err(e) => {
            eprintln!("listings: {e}");
            Vec::new()
        }
    }
}

fn kind_name(kind: &str) -> &'static str {
    match kind {
        "laser" => "Laser",
        "model3d" => "3D model",
        _ => "Embroidery",
    }
}

pub fn cards_html(cards: &[Card]) -> String {
    if cards.is_empty() {
        return String::new();
    }
    let items: String = cards.iter().map(|c| card_html(c, None)).collect();
    format!(r#"<ul class="cards">{items}</ul>"#)
}

/// One card, with a label above its title (`Embroidery pick`) when given.
pub fn card_html(c: &Card, label: Option<&str>) -> String {
    {
        {
            let picture = match (c.photo, c.model_file) {
                (Some(p), _) => format!(
                    r#"<img src="/designs/{}/photos/{p}/thumb" alt="{}" loading="lazy">"#,
                    c.id,
                    escape(&c.title)
                ),
                (None, Some(f)) => format!(
                    r#"<img src="/3d/{}/files/{f}/hero" alt="{}" loading="lazy">"#,
                    c.id,
                    escape(&c.title)
                ),
                _ => r#"<span class="muted">No photo</span>"#.to_string(),
            };
            format!(
                r#"<li><a href="/d/{id}/{slug}"><span class="thumb">{picture}</span>{label}<strong>{title}</strong><span class="muted">by {maker}{founding}</span><span class="muted"><span class="kind-chip">{kind}</span> {format}{downloads}</span><span class="price">{price}</span></a></li>"#,
                founding = c
                    .founding
                    .map(|n| format!(r#" <span class="badge founding" title="Founding creator #{n}">Founding</span>"#))
                    .unwrap_or_default(),
                label = label
                    .map(|l| format!(r#"<span class="pick">{}</span>"#, escape(l)))
                    .unwrap_or_default(),
                id = c.id,
                slug = slug(&c.title),
                title = escape(&c.title),
                maker = escape(&c.maker),
                kind = kind_name(&c.kind),
                format = escape(&c.format),
                downloads = match c.downloads {
                    0 => String::new(),
                    1 => " · 1 download".to_string(),
                    n => format!(" · {n} downloads"),
                },
                price = if c.price == 0 {
                    "Free".to_string()
                } else {
                    dollars(c.price)
                },
            )
        }
    }
}

/// Published designs `ids` as cards, in that order (3D models only while `three_d`).
pub async fn cards_of(pool: &PgPool, ids: &[i64], three_d: bool) -> Vec<Card> {
    let rows = sqlx::query(
        "SELECT d.id, d.title, d.kind, d.format, d.downloads, d.price_cents, m.name AS maker,
                (SELECT p.id FROM mf_design_photos p WHERE p.design_id = d.id ORDER BY p.position LIMIT 1) AS photo,
                (SELECT f.id FROM mf_model_files f WHERE f.design_id = d.id AND f.thumb_key IS NOT NULL ORDER BY f.position LIMIT 1) AS model_file
         FROM mf_designs d JOIN mf_makers m ON m.account_id = d.account_id
         WHERE d.id = ANY($1) AND d.status = 'published' AND (d.kind <> 'model3d' OR $2)",
    )
    .bind(ids)
    .bind(three_d)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut cards: Vec<Card> = rows
        .iter()
        .filter_map(|r| {
            Some(Card {
                id: r.try_get("id").ok()?,
                title: r.try_get("title").ok()?,
                kind: r.try_get("kind").ok()?,
                format: r.try_get("format").ok()?,
                photo: r.try_get("photo").ok()?,
                model_file: r.try_get("model_file").ok()?,
                maker: r.try_get("maker").ok()?,
                downloads: r.try_get("downloads").ok()?,
                price: r.try_get("price_cents").ok()?,
                founding: None,
            })
        })
        .collect();
    cards.sort_by_key(|c| ids.iter().position(|i| *i == c.id).unwrap_or(usize::MAX));
    cards
}

/// A maker's published designs, as cards.
pub async fn cards(pool: &PgPool, account: i64, limit: i64) -> String {
    cards_html(
        &find(
            pool,
            &Filter {
                account: Some(account),
                ..Filter::default()
            },
            limit,
        )
        .await,
    )
}

/// How many published designs there are (3D models only while `three_d`); 0 without a database.
pub async fn published_count(state: &AppState) -> i64 {
    let Some(pool) = state.db.as_ref() else {
        return 0;
    };
    sqlx::query_scalar("SELECT count(*) FROM mf_designs WHERE status = 'published' AND (kind <> 'model3d' OR $1)")
        .bind(state.three_d)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Fewer designs than this and the home page shows the founding invitation, not empty shelves.
pub const SPARSE: i64 = 8;

/// One home-page shelf: a heading, a grid of design cards and a link; empty while there are none.
async fn shelf(state: &AppState, title: &str, more: (&str, &str), filter: Filter) -> String {
    let Some(pool) = state.db.as_ref() else {
        return String::new();
    };
    let found = find(
        pool,
        &Filter {
            three_d: state.three_d,
            ..filter
        },
        10,
    )
    .await;
    if found.is_empty() {
        return String::new();
    }
    format!(
        r#"<section class="design shelf"><h2>{title}</h2>{}<p><a href="{}">{}</a></p></section>"#,
        cards_html(&found),
        more.0,
        more.1
    )
}

/// The home page's shelves of design cards: trending (most saved), newest, free, and founding
/// creators' designs. Empty while there are none.
pub async fn shelves(state: &AppState) -> String {
    let trending = shelf(
        state,
        "Trending",
        ("/browse?sort=saved", "See more trending designs"),
        Filter {
            most_saved: true,
            ..Filter::default()
        },
    )
    .await;
    let newest = shelf(state, "Newest", ("/browse", "Browse all designs"), Filter::default()).await;
    let free = shelf(
        state,
        "Free to download",
        ("/browse?price=free", "See all free designs"),
        Filter {
            free: Some(true),
            popular: true,
            ..Filter::default()
        },
    )
    .await;
    let founding = shelf(
        state,
        "From founding creators",
        ("/founding", "Become a founding creator"),
        Filter {
            founding_only: true,
            ..Filter::default()
        },
    )
    .await;
    format!("{trending}{newest}{free}{founding}")
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct BrowseQuery {
    q: String,
    kind: String,
    tag: String,
    sort: String,
    price: String,
    hoop: String,
    material: String,
    page: i64,
}

pub async fn browse(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<BrowseQuery>) -> Response {
    let signed_in = current_user(&state, &headers).await.is_some();
    let words = q.q.trim().chars().take(200).collect::<String>();
    let asked = match q.kind.as_str() {
        "embroidery" | "laser" => Some(q.kind.clone()),
        "3d" if state.three_d => Some("model3d".to_string()),
        _ => None,
    };
    let tag = tidy_tags(&q.tag).into_iter().next();
    let popular = q.sort == "popular";
    let most_saved = q.sort == "saved";
    let page_no = q.page.clamp(0, 1000);
    let free = match q.price.as_str() {
        "free" => Some(true),
        "paid" => Some(false),
        _ => None,
    };
    let price = match free {
        Some(true) => "free",
        Some(false) => "paid",
        None => "",
    };
    let hoop = sizes::hoop(&q.hoop);
    let material = sizes::material_name(&q.material).map(|_| q.material.clone());
    // a hoop alone means embroidery, a material alone laser; with a kind chosen, only that
    // kind's filters count (no hidden hoop left on a laser search)
    let kind = match (&asked, hoop, &material) {
        (Some(k), _, _) => Some(k.clone()),
        (None, Some(_), _) => Some("embroidery".to_string()),
        (None, None, Some(_)) => Some("laser".to_string()),
        _ => None,
    };
    let hoop = hoop.filter(|_| kind.as_deref() == Some("embroidery"));
    let material = material.filter(|_| kind.as_deref() == Some("laser"));
    let kind_param = match kind.as_deref() {
        Some("model3d") => "3d",
        Some(k) => k,
        None => "",
    };
    // the search as it stands, for links that change one part of it
    let search = format!(
        "q={q}&amp;kind={k}&amp;price={price}&amp;hoop={h}&amp;material={m}&amp;sort={s}",
        q = escape(&url_word(&words)),
        k = kind_param,
        h = hoop.map_or("", |h| h.id),
        m = escape(material.as_deref().unwrap_or("")),
        s = if popular {
            "popular"
        } else if most_saved {
            "saved"
        } else {
            "newest"
        },
    );
    let filter = Filter {
        three_d: state.three_d,
        kind: kind.clone(),
        tag: tag.clone(),
        words: (!words.is_empty()).then(|| words.clone()),
        account: None,
        free,
        hoop,
        material: material.clone(),
        popular,
        most_saved,
        collection: None,
        followed_by: None,
        founding_only: false,
        page: page_no,
    };
    let found = match state.db.as_ref() {
        Some(pool) => find(pool, &filter, PAGE_SIZE).await,
        None => Vec::new(),
    };
    let selected = |on: bool| if on { " selected" } else { "" };
    let tag_note = match &tag {
        Some(t) => format!(
            r#"<p>Tagged <strong>{t}</strong> · <a href="/browse?{search}">show all tags</a></p>"#,
            t = escape(t),
        ),
        None => String::new(),
    };
    let results = if found.is_empty() {
        r#"<p class="lead">No designs found.</p>"#.to_string()
    } else {
        cards_html(&found)
    };
    let more = if found.len() as i64 == PAGE_SIZE {
        format!(
            r#"<p><a href="/browse?{search}&amp;tag={t}&amp;page={n}">More designs</a></p>"#,
            t = escape(&url_word(tag.as_deref().unwrap_or(""))),
            n = page_no + 1,
        )
    } else {
        String::new()
    };
    let hoops = HOOPS
        .iter()
        .map(|h| {
            format!(
                r#"<option value="{}"{}>Fits {}</option>"#,
                h.id,
                selected(hoop.is_some_and(|x| x.id == h.id)),
                escape(h.name)
            )
        })
        .collect::<String>();
    let materials = MATERIALS
        .iter()
        .map(|(id, name)| {
            format!(
                r#"<option value="{id}"{}>{name}</option>"#,
                selected(material.as_deref() == Some(*id))
            )
        })
        .collect::<String>();
    Html(page(
        "Browse designs · MakerFoundry",
        &format!(
            r#"<section class="design">
<h1>Browse designs</h1>
{tabs}
<form class="search" method="get" action="/browse">
<input type="search" name="q" value="{words}" placeholder="Search designs" aria-label="Search designs" maxlength="200">
<input type="hidden" name="kind" value="{kind_param}">
<select name="price" aria-label="Price"><option value="">Free and paid</option><option value="free"{fr}>Free</option><option value="paid"{pd}>Paid</option></select>
{kind_filters}
<select name="sort" aria-label="Order"><option value="newest">Newest</option><option value="popular"{p}>Most downloaded</option><option value="saved"{ms}>Most saved</option></select>
<input type="hidden" name="tag" value="{tag}">
<button type="submit">Search</button>
</form>
{tag_note}{results}{more}
</section>"#,
            words = escape(&words),
            tabs = kind_tabs(kind_param, state.three_d, &words, price, popular, most_saved),
            kind_param = kind_param,
            kind_filters = match kind.as_deref() {
                Some("embroidery") => format!(
                    r#"<select name="hoop" aria-label="Hoop"><option value="">Any hoop</option>{hoops}</select>"#
                ),
                Some("laser") => format!(
                    r#"<select name="material" aria-label="Material"><option value="">Any material</option>{materials}</select>"#
                ),
                _ => String::new(),
            },
            p = selected(popular),
            ms = selected(most_saved),
            fr = selected(free == Some(true)),
            pd = selected(free == Some(false)),
            tag = escape(tag.as_deref().unwrap_or("")),
        ),
        signed_in,
    ))
    .into_response()
}

/// The kind tabs on `/browse`: All, Embroidery, Laser, and 3D print once models are open. A
/// tab keeps the words, price and order but drops the other kind's filters.
fn kind_tabs(current: &str, three_d: bool, words: &str, price: &str, popular: bool, most_saved: bool) -> String {
    let sort = if popular {
        "popular"
    } else if most_saved {
        "saved"
    } else {
        "newest"
    };
    let mut tabs = vec![("", "All"), ("embroidery", "Embroidery"), ("laser", "Laser")];
    if three_d {
        tabs.push(("3d", "3D print"));
    }
    let items: String = tabs
        .iter()
        .map(|(id, name)| {
            let here = *id == current;
            format!(
                r#"<li><a href="/browse?q={q}&amp;kind={id}&amp;price={price}&amp;sort={sort}"{aria}>{name}</a></li>"#,
                q = escape(&url_word(words)),
                aria = if here { r#" aria-current="page""# } else { "" },
            )
        })
        .collect();
    format!(r#"<nav aria-label="Kind of design"><ul class="kind-tabs">{items}</ul></nav>"#)
}

/// `text` for a URL's query.
pub(crate) fn url_word(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// ---------------------------------------------------------------- a design's public page

fn not_found(signed_in: bool) -> Response {
    (StatusCode::NOT_FOUND, Html(page("Not found · MakerFoundry", r#"<section class="hero"><h1>Design not found</h1><p class="lead"><a href="/browse">Browse designs</a></p></section>"#, signed_in))).into_response()
}

/// What a design fits or is made for: "fits a 4×4 in hoop", "for 3 mm plywood".
pub fn fits_text(d: &DesignRow) -> Option<String> {
    if d.kind == "laser" {
        let material = d.material.as_deref().and_then(sizes::material_name)?;
        Some(match d.thickness_mm {
            Some(t) => format!("for {} {}", sizes::thickness_text(t), material.to_lowercase()),
            None => format!("for {}", material.to_lowercase()),
        })
    } else if d.kind == "embroidery" {
        let hoop = sizes::smallest_hoop(d.width_mm?, d.height_mm?)?;
        let short = hoop.name.split(" (").next().unwrap_or(hoop.name);
        Some(format!("fits a {short} hoop"))
    } else {
        None
    }
}

/// "Also by this maker" on a 3D model's page (MF-57): the same maker's published embroidery and
/// laser designs, a few of each, and a link to all their work.
async fn also_by(pool: &PgPool, account: i64, handle: Option<&str>) -> String {
    let mut cards = Vec::new();
    for kind in ["embroidery", "laser"] {
        cards.extend(
            find(
                pool,
                &Filter {
                    account: Some(account),
                    kind: Some(kind.to_string()),
                    ..Filter::default()
                },
                4,
            )
            .await,
        );
    }
    if cards.is_empty() {
        return String::new();
    }
    let all = handle
        .map(|h| format!(r#"<p><a href="/makers/{h}">All their work</a></p>"#, h = escape(h)))
        .unwrap_or_default();
    format!(r#"<h2>Also by this maker</h2>{}{all}"#, cards_html(&cards))
}

/// Whether `viewer` may see design `d`: anyone once it's published; otherwise (a draft, or taken
/// down) only its maker and the admins.
pub fn visible(d: &DesignRow, viewer: Option<&User>, admins: &[i64]) -> bool {
    d.status == "published" || viewer.is_some_and(|u| u.account_id == d.account_id || admins.contains(&u.account_id))
}

pub async fn public_page(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    show(state, headers, id).await
}

pub async fn public_page_slug(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, _slug)): Path<(i64, String)>,
) -> Response {
    show(state, headers, id).await
}

async fn show(state: AppState, headers: HeaderMap, id: i64) -> Response {
    let viewer = current_user(&state, &headers).await;
    let signed_in = viewer.is_some();
    let Some(pool) = state.db.as_ref() else {
        return not_found(signed_in);
    };
    let d = match designs::design(pool, id).await {
        Ok(Some(d)) if visible(&d, viewer.as_ref(), &state.admins) => d,
        _ => return not_found(signed_in),
    };
    if d.kind == "model3d" && !crate::model3d::open_to(&state, viewer.as_ref()).await {
        return not_found(signed_in);
    }
    let maker = makers::profile(pool, d.account_id).await;
    let mine = viewer.as_ref().is_some_and(|u| u.account_id == d.account_id);
    let draft_note = match d.status.as_str() {
        "published" => String::new(),
        "removed" => format!(
            r#"<p class="problem">Taken down: only its maker and the admins can see this page. {}</p>"#,
            escape(d.removed_reason.as_deref().unwrap_or(""))
        ),
        _ => format!(
            r#"<p class="problem">A draft: only you can see this page. <a href="/designs/{id}">Back to the draft</a></p>"#
        ),
    };
    let founding = crate::founding::badge_for(pool, d.account_id).await;
    let byline = match &maker {
        Some(m) => format!(
            r#"by <a href="/makers/{h}">{n}</a> {founding}"#,
            h = escape(&m.handle),
            n = escape(&m.name)
        ),
        None => String::new(),
    };
    let downloads = match d.downloads {
        0 => String::new(),
        1 => " · 1 download".into(),
        n => format!(" · {n} downloads"),
    };
    let fits = match d.category.as_deref().and_then(crate::model3d::category) {
        Some(c) => format!(" · {}", escape(c.name)),
        None => fits_text(&d).map(|t| format!(" · {}", escape(&t))).unwrap_or_default(),
    };
    let made = match crate::makes::count(pool, id).await {
        0 => String::new(),
        1 => " · 1 make".into(),
        n => format!(" · {n} makes"),
    };
    let talk = match crate::comments::count(pool, id).await {
        0 => String::new(),
        1 => " · 1 comment".into(),
        n => format!(" · {n} comments"),
    };
    let saves = match crate::collections::saves(pool, id).await {
        0 => String::new(),
        1 => " · saved by 1 person".into(),
        n => format!(" · saved by {n} people"),
    };
    let licence = d.licence.as_deref().and_then(licences::find);
    let licence_html = match licence {
        Some(l) => format!(
            r#"<h2>Licence: {name}</h2><p>{plain}{link}</p>"#,
            name = escape(l.name),
            plain = escape(&l.plain()),
            link = l
                .url
                .map(|u| format!(r#" <a href="{u}" rel="noopener">The licence</a>"#))
                .unwrap_or_default(),
        ),
        None => String::new(),
    };
    let tags: String = d
        .tags
        .iter()
        .map(|t| {
            format!(
                r#"<li><a href="/browse?tag={u}">{t}</a></li>"#,
                u = escape(&url_word(t)),
                t = escape(t)
            )
        })
        .collect();
    let tags = if tags.is_empty() {
        String::new()
    } else {
        format!(r#"<ul class="tags">{tags}</ul>"#)
    };
    let buy = selling::buy_html(
        &state,
        pool,
        viewer.as_ref(),
        id,
        d.price_cents,
        &licence.map(|l| escape(l.name)).unwrap_or_default(),
    )
    .await;
    let save = if d.status == "published" && d.kind == "model3d" {
        format!(
            "{}{}",
            crate::model3d::likes::html(pool, viewer.as_ref(), id).await,
            crate::collections::save_html(pool, viewer.as_ref(), id).await
        )
    } else if d.status == "published" {
        crate::collections::save_html(pool, viewer.as_ref(), id).await
    } else {
        String::new()
    };
    // Open in StitchFoundry (embroidery), LaserFoundry (laser) or ScreenFoundry (SVG): a free
    // design's file is anyone's; a paid one opens through a link made for its buyers (and maker)
    let mut buttons = Vec::new();
    let free = d.price_cents == 0 && d.status == "published";
    // (bought: whatever it costs now)
    let paid_may = !free && d.status != "removed" && access(&state, pool, viewer.as_ref(), &d, id).await.1;
    for (app, name) in crate::api::web_apps(&d) {
        if free {
            let href = crate::api::open_in(app, &state.public_url, id, &crate::api::file_name(&d, id));
            buttons.push(format!(
                r#"<a class="button secondary" href="{}" rel="noopener">Open in {name}</a>"#,
                escape(&href)
            ));
        } else if paid_may {
            buttons.push(format!(
                r#"<a class="button secondary" href="/d/{id}/open?app={}" rel="noopener">Open in {name}</a>"#,
                name.to_ascii_lowercase()
            ));
        }
    }
    let open = if buttons.is_empty() {
        String::new()
    } else {
        format!(
            r#"<p class="actions open-in">{} <span class="muted">{}in your web browser</span></p>"#,
            buttons.join(" "),
            if free { "Free, " } else { "" }
        )
    };
    let origin = match d.original_site.as_deref() {
        Some(site) => format!(
            r#"<p class="muted">{}</p>"#,
            crate::model3d::origin::line(site, d.original_url.as_deref(), d.original_date.as_deref())
        ),
        None => String::new(),
    };
    // picks of the week (an admin's button to pick it; its weeks as a pick for everyone)
    let picked = crate::picks::badge_html(pool, id).await;
    let pick_button =
        if d.status == "published" && viewer.as_ref().is_some_and(|u| state.admins.contains(&u.account_id)) {
            crate::picks::admin_button(id, &d.kind)
        } else {
            String::new()
        };
    let intro = format!(
        r#"{draft_note}{picked}<p class="byline">{byline}{fits}{downloads}{made}{talk}{saves}</p>{origin}{buy}{open}{save}{pick_button}"#
    );
    let photos = designs::photos_html(pool, id).await;
    let (_, may_download) = access(&state, pool, viewer.as_ref(), &d, id).await;
    let first_format = d
        .facts
        .get("format")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("File");
    let others = crate::files::others(pool, id).await;
    let converted: Vec<&str> = if d.kind == "embroidery" && d.convert {
        let had: Vec<&str> = std::iter::once(first_format)
            .chain(others.iter().map(|f| f.format.as_str()))
            .collect();
        crate::convert::offered(&had).iter().map(|f| f.extension()).collect()
    } else {
        Vec::new()
    };
    let formats = if d.kind == "model3d" {
        let files = crate::model3d::upload::model_files(pool, id).await;
        let profiles = crate::model3d::upload::profiles(pool, id).await;
        format!(
            "{}{}{}{}{}",
            crate::model3d::remix::section(pool, id).await,
            also_by(pool, d.account_id, maker.as_ref().map(|m| m.handle.as_str())).await,
            crate::model3d::upload::profiles_html(id, &profiles, may_download),
            crate::model3d::scad::links_html(&state, id, &files, may_download),
            crate::model3d::upload::public_files_html(id, &files, may_download)
        )
    } else {
        crate::files::public_html(id, first_format, &others, &converted, may_download)
    };
    let noun = if d.kind == "model3d" { "model" } else { "design" };
    let edit = if mine {
        format!(r#"<p><a href="/designs/{id}">Manage this {noun}</a></p>"#)
    } else if d.status == "published" {
        format!(r#"<p class="muted"><a href="/d/{id}/report">Report this {noun}</a></p>"#)
    } else {
        String::new()
    };
    let (makes, comments) = if d.status == "published" {
        (
            crate::makes::section(&state, pool, viewer.as_ref(), id, d.account_id).await,
            crate::comments::section(&state, pool, viewer.as_ref(), id, d.account_id).await,
        )
    } else {
        (String::new(), String::new())
    };
    let footer = format!(
        r#"{desc}{formats}{photos}{makes}{tags}{licence_html}{comments}{edit}"#,
        desc = paragraphs(&d.description)
    );
    let title = if d.title.is_empty() {
        d.file_name.clone()
    } else {
        d.title.clone()
    };
    let section = if d.kind == "model3d" {
        crate::model3d::upload::public_section(pool, id, &title, &intro, &footer).await
    } else if may_download {
        designs::facts_section(&d, &title, &intro, &footer)
    } else {
        // a paid design someone hasn't bought: a lower-detail picture, never the stitches or lines
        // themselves (the owner's decision, 2026-11-01; `coarse`)
        let figure = crate::coarse::figure_html(id, &d.kind, &d.preview_svg, &title);
        designs::drawing_section(&d.kind, &d.facts, &figure, &title, &intro, &footer)
    };
    match section {
        Some(html) => {
            // a published design's link shows its title, a line of its description and a photo
            let url = format!("{}/d/{id}/{}", state.public_url, slug(&title));
            let photo: Option<i64> =
                sqlx::query_scalar("SELECT id FROM mf_design_photos WHERE design_id = $1 ORDER BY position LIMIT 1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await
                    .ok()
                    .flatten();
            let model_picture = if d.kind == "model3d" {
                crate::model3d::upload::first_picture(pool, id).await
            } else {
                None
            };
            let image = match (model_picture, photo) {
                (Some(f), _) => Some(format!("{}/3d/{id}/files/{f}/hero", state.public_url)),
                (None, Some(p)) => Some(format!("{}/designs/{id}/photos/{p}/large", state.public_url)),
                _ => None,
            };
            let by = maker
                .as_ref()
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "a maker".into());
            let description = if d.description.trim().is_empty() {
                format!(
                    "{} {} by {by} on MakerFoundry",
                    match d.kind.as_str() {
                        "laser" => "A laser-cut",
                        "model3d" => "A 3D",
                        _ => "An embroidery",
                    },
                    if d.kind == "model3d" { "model" } else { "design" }
                )
            } else {
                crate::pages::summary(&d.description, 200)
            };
            let preview = crate::pages::Preview {
                title: &title,
                description: &description,
                url: &url,
                image: image.as_deref(),
            };
            let shared = (d.status == "published").then_some(&preview);
            let mut response = Html(crate::pages::page_with(
                &format!("{} · MakerFoundry", escape(&title)),
                &html,
                signed_in,
                shared,
            ))
            .into_response();
            if d.kind == "model3d" {
                response.headers_mut().insert(
                    axum::http::header::CONTENT_SECURITY_POLICY,
                    axum::http::HeaderValue::from_static(crate::MODEL_PAGE_SECURITY_POLICY),
                );
            }
            response
        }
        None => not_found(signed_in),
    }
}

/// `GET /d/<id>/preview.png`: the lower-detail picture of an embroidery or laser design
/// (`coarse`), which its page shows to people who haven't bought it; anyone's who may see the
/// design.
pub async fn preview_png(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let viewer = current_user(&state, &headers).await;
    let Some(pool) = state.db.as_ref() else {
        return not_found(viewer.is_some());
    };
    let Ok(Some(d)) = designs::design(pool, id).await else {
        return not_found(viewer.is_some());
    };
    if !matches!(d.kind.as_str(), "embroidery" | "laser") || !access(&state, pool, viewer.as_ref(), &d, id).await.0 {
        return not_found(viewer.is_some());
    }
    let svg = d.preview_svg;
    let png = tokio::task::spawn_blocking(move || crate::coarse::picture(&svg))
        .await
        .ok()
        .flatten();
    let Some(png) = png else {
        return not_found(viewer.is_some());
    };
    let cache = if d.status == "published" {
        "public, max-age=3600"
    } else {
        "private, max-age=3600"
    };
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, cache),
        ],
        png,
    )
        .into_response()
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DownloadQuery {
    /// One of the design's other files (`files`); the first file without it.
    file: Option<i64>,
    /// The first file converted to this format (`convert`): `dst`.
    #[serde(rename = "as")]
    to: Option<String>,
    /// One of a 3D model's files (`model3d`); all of them as a ZIP without it.
    model_file: Option<i64>,
}

/// Whether `viewer` may see design `d` (`id`) and whether they may download it: a free published
/// design anyone; a paid one the people who bought it (even unpublished, unless it was taken
/// down); any its maker and the admins. A purchase counts whatever the design costs now: a
/// seller who makes it free, or unpublishes it, doesn't take it from the people who paid
/// (only a takedown does, and a refund).
pub(crate) async fn access(
    state: &AppState,
    pool: &PgPool,
    viewer: Option<&User>,
    d: &DesignRow,
    id: i64,
) -> (bool, bool) {
    let staff = viewer.is_some_and(|u| u.account_id == d.account_id || state.admins.contains(&u.account_id));
    let buyer = match viewer {
        Some(u) if d.status != "removed" && !staff => selling::bought(pool, u.account_id, id).await,
        _ => false,
    };
    let seen = visible(d, viewer, &state.admins) || buyer;
    (seen, seen && (d.price_cents == 0 || staff || buyer))
}

/// A design's file, by a short-lived link: a free published design's for anyone, a paid one's for
/// the people who bought it (even once it's unpublished, unless it was taken down), and any for its
/// maker and the admins. Downloads of published designs are counted.
pub async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    let viewer = current_user(&state, &headers).await;
    let (Some(pool), Some(storage)) = (state.db.as_ref(), state.storage.as_ref()) else {
        return not_found(viewer.is_some());
    };
    let Ok(Some(d)) = designs::design(pool, id).await else {
        return not_found(viewer.is_some());
    };
    let (seen, may) = access(&state, pool, viewer.as_ref(), &d, id).await;
    if !seen {
        return not_found(viewer.is_some());
    }
    if !may {
        // not bought: the page with the button to buy it
        return Redirect::to(&format!("/d/{id}/{}", slug(&d.title))).into_response();
    }
    if d.kind == "model3d" && !crate::model3d::open_to(&state, viewer.as_ref()).await {
        return not_found(viewer.is_some());
    }
    if let Some(ext) = &q.to {
        return converted(&state, storage, pool, &d, id, ext, &headers).await;
    }
    if d.kind == "model3d" {
        let found = match q.model_file {
            Some(fid) => crate::model3d::upload::model_file_key(pool, id, fid).await,
            None => match crate::model3d::upload::zip_for(storage, pool, id, &d).await {
                Ok(key) => Some((key, format!("{}.zip", slug(&d.title)))),
                Err(e) => {
                    eprintln!("listings: zip {id}: {e}");
                    return (
                        StatusCode::BAD_GATEWAY,
                        "The files can't be fetched just now. Please try again.",
                    )
                        .into_response();
                }
            },
        };
        let Some((key, name)) = found else {
            return not_found(viewer.is_some());
        };
        if d.status == "published" {
            let _ = sqlx::query("UPDATE mf_designs SET downloads = downloads + 1 WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await;
        }
        return Redirect::to(storage.download_url(&key, &name).as_str()).into_response();
    }
    // which file: the first, or one of the others
    // (named by the format it was read as, never the ending it was uploaded with: #86)
    let first_format = d.facts.get("format").and_then(serde_json::Value::as_str);
    let (file_key, file_name) = match q.file {
        None => (d.file_key.clone(), designs::safe_file_name(&d.file_name, first_format)),
        Some(fid) => match crate::files::others(pool, id).await.into_iter().find(|f| f.id == fid) {
            Some(f) => (f.file_key, designs::safe_file_name(&f.file_name, Some(&f.format))),
            None => return not_found(viewer.is_some()),
        },
    };
    if d.status == "published" {
        let _ = sqlx::query("UPDATE mf_designs SET downloads = downloads + 1 WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await;
    }
    let name = if d.title.is_empty() {
        file_name
    } else {
        // the title, with the file's (safe) extension
        match file_name.rsplit_once('.') {
            Some((_, ext)) => format!("{}.{ext}", slug(&d.title)),
            None => slug(&d.title),
        }
    };
    Redirect::to(storage.download_url(&file_key, &name).as_str()).into_response()
}

/// The design's first file converted to `ext`, made now and sent straight back (not stored).
async fn converted(
    state: &AppState,
    storage: &crate::storage::Storage,
    pool: &PgPool,
    d: &DesignRow,
    id: i64,
    ext: &str,
    headers: &HeaderMap,
) -> Response {
    let Some(to) = crate::convert::format(ext).filter(|_| d.kind == "embroidery" && d.convert) else {
        return not_found(true);
    };
    if !state
        .limits
        .allow(crate::limits::CONVERT, &crate::limits::client(headers))
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many converted downloads lately. Please wait a while.",
        )
            .into_response();
    }
    let bytes = match storage.get(&d.file_key).await {
        Ok(Some(b)) => b,
        Ok(None) => return not_found(true),
        Err(e) => {
            eprintln!("convert: {id}: {e}");
            return (StatusCode::BAD_GATEWAY, "The file can't be fetched just now.").into_response();
        }
    };
    let (name, label) = (d.file_name.clone(), d.title.clone());
    let made = tokio::task::spawn_blocking(move || crate::convert::convert(&bytes, &name, to, &label)).await;
    let out = match made {
        Ok(Ok(out)) => out,
        Ok(Err(why)) => {
            eprintln!("convert: {id} to {}: {why}", to.extension());
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "This design can't be converted to {}: {why}.",
                    to.extension().to_uppercase()
                ),
            )
                .into_response();
        }
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "The file couldn't be converted.").into_response(),
    };
    if d.status == "published" {
        let _ = sqlx::query("UPDATE mf_designs SET downloads = downloads + 1 WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await;
    }
    let stem = if d.title.is_empty() {
        d.file_name
            .rsplit_once('.')
            .map_or(d.file_name.as_str(), |(n, _)| n)
            .to_string()
    } else {
        slug(&d.title)
    };
    let stem: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect();
    let disposition = format!(
        "attachment; filename=\"{}.{}\"",
        if stem.is_empty() { "design" } else { &stem },
        to.extension()
    );
    (
        [
            (axum::http::header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
            (axum::http::header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        out,
    )
        .into_response()
}

// ---------------------------------------------------------------- details and publishing

/// What a design's details form shows.
struct Details<'a> {
    title: &'a str,
    description: &'a str,
    tags: &'a str,
    licence: &'a str,
    price: &'a str,
    can_sell: bool,
    /// `laser` designs also take a material and a thickness.
    kind: &'a str,
    material: &'a str,
    thickness: &'a str,
    /// An embroidery design: offered converted to other formats.
    convert: bool,
    /// A 3D model's category, and where it was first published (`model3d::origin`).
    category: &'a str,
    original: [&'a str; 3],
}

fn details_form(id: i64, f: &Details, problem: &str) -> String {
    let Details {
        title,
        description,
        tags,
        licence,
        price,
        can_sell,
        kind,
        material,
        thickness,
        convert,
        category,
        original,
    } = *f;
    let made_for = if kind == "model3d" {
        let options: String = crate::model3d::CATEGORIES
            .iter()
            .map(|c| {
                format!(
                    r#"<option value="{id}"{selected}>{name}</option>"#,
                    id = c.id,
                    selected = if c.id == category { " selected" } else { "" },
                    name = escape(c.name)
                )
            })
            .collect();
        let sites: String = crate::model3d::origin::SITES
            .iter()
            .map(|(id, name, _)| {
                format!(
                    r#"<option value="{id}"{selected}>{name}</option>"#,
                    selected = if *id == original[0] { " selected" } else { "" }
                )
            })
            .collect();
        format!(
            r#"<label>Category<select name="category" required><option value="">Choose one</option>{options}</select></label>
<fieldset><legend>Originally published on <small>(if you shared it elsewhere first: optional)</small></legend>
<label>Site<select name="original_site"><option value="">Nowhere else</option>{sites}</select></label>
<label>Your page there <small>(its address)</small><input name="original_url" type="url" value="{url}" maxlength="300"></label>
<label>Date first published <small>(YYYY-MM-DD)</small><input name="original_date" value="{date}" maxlength="10" placeholder="2025-03-12"></label>
</fieldset>"#,
            url = escape(original[1]),
            date = escape(original[2]),
        )
    } else if kind == "laser" {
        let options: String = MATERIALS
            .iter()
            .map(|(id, name)| {
                format!(
                    r#"<option value="{id}"{}>{name}</option>"#,
                    if *id == material { " selected" } else { "" }
                )
            })
            .collect();
        format!(
            r#"<label>Material <small>(what it's made to cut)</small><select name="material"><option value="">Not given</option>{options}</select></label>
<label>Thickness <small>(mm, optional)</small><input name="thickness" value="{}" inputmode="decimal" maxlength="8"></label>"#,
            escape(thickness)
        )
    } else {
        // (the hidden field says the box was on the form, so an unticked box means off)
        format!(
            r#"<input type="hidden" name="convert_shown" value="yes"><label class="choice"><input type="checkbox" name="convert" value="yes"{}> <span>Also offer it in the other embroidery formats, converted by MakerFoundry from your file when someone downloads it</span></label>"#,
            if convert { " checked" } else { "" }
        )
    };
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    };
    let choices: String = licences::for_kind(kind)
        .into_iter()
        .map(|l| {
            format!(
                r#"<label class="choice"><input type="radio" name="licence" value="{id}"{checked}> <span><strong>{name}</strong><br><span class="muted">{plain}</span></span></label>"#,
                id = l.id,
                checked = if l.id == licence { " checked" } else { "" },
                name = escape(l.name),
                plain = escape(&l.plain()),
            )
        })
        .collect();
    format!(
        r#"<section class="form">
<h1>Design details</h1>
{problem}
<form method="post" action="/designs/{id}/edit">
<label>Title<input name="title" value="{title}" required maxlength="{MAX_TITLE}"></label>
<label>Description <small>(what it is, sizes, fabrics or materials, tips)</small><textarea name="description" rows="8" maxlength="{MAX_DESCRIPTION}">{description}</textarea></label>
<label>Tags <small>(apart by commas, up to {MAX_TAGS}: flowers, christmas, coaster…)</small><input name="tags" value="{tags}" maxlength="400"></label>
{made_for}
<fieldset><legend>Licence: what people may do with what they make</legend>{choices}</fieldset>
<label>Price <small>({price_note})</small><input name="price" value="{price}" inputmode="decimal" maxlength="10"></label>
<button type="submit">Save</button>
</form>
<p><a href="/designs/{id}">Back to the design</a></p>
</section>"#,
        title = escape(title),
        description = escape(description),
        tags = escape(tags),
        price = escape(price),
        price_note = if can_sell {
            "US dollars, from $1 to $200; 0 shares it free. You get the price less 10% and the card fee."
        } else {
            r#"0 shares it free. To sell it, first <a href="/account/selling">set up selling</a>."#
        },
    )
}

async fn own(state: &AppState, headers: &HeaderMap, id: i64) -> Result<(User, PgPool, DesignRow), Response> {
    let (Some(user), Some(pool)) = (current_user(state, headers).await, state.db.clone()) else {
        return Err(Redirect::to("/signin").into_response());
    };
    match designs::design(&pool, id).await {
        Ok(Some(d)) if d.account_id == user.account_id => Ok((user, pool, d)),
        _ => Err(not_found(true)),
    }
}

pub async fn edit_page(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let (user, pool, d) = match own(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let title = if d.title.is_empty() {
        d.file_name
            .rsplit_once('.')
            .map_or(d.file_name.as_str(), |(n, _)| n)
            .to_string()
    } else {
        d.title.clone()
    };
    let price = if d.price_cents == 0 {
        "0".to_string()
    } else {
        dollars(d.price_cents).trim_start_matches('$').to_string()
    };
    let can_sell = selling::can_sell(&state, &pool, user.account_id).await;
    let thickness = d
        .thickness_mm
        .map(|t| sizes::thickness_text(t).replace(" mm", ""))
        .unwrap_or_default();
    private(
        "Design details · MakerFoundry",
        &details_form(
            id,
            &Details {
                title: &title,
                description: &d.description,
                tags: &d.tags.join(", "),
                licence: d.licence.as_deref().unwrap_or(""),
                price: &price,
                can_sell,
                kind: &d.kind,
                material: d.material.as_deref().unwrap_or(""),
                thickness: &thickness,
                convert: d.convert,
                category: d.category.as_deref().unwrap_or(""),
                original: [
                    d.original_site.as_deref().unwrap_or(""),
                    d.original_url.as_deref().unwrap_or(""),
                    d.original_date.as_deref().unwrap_or(""),
                ],
            },
            "",
        ),
        true,
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DetailsForm {
    title: String,
    description: String,
    tags: String,
    licence: String,
    price: String,
    material: String,
    thickness: String,
    convert: String,
    convert_shown: String,
    category: String,
    original_site: String,
    original_url: String,
    original_date: String,
}

pub async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<DetailsForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool, d) = match own(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let title = f.title.split_whitespace().collect::<Vec<_>>().join(" ");
    let description = f.description.trim().replace("\r\n", "\n");
    let tags = tidy_tags(&f.tags);
    let can_sell = selling::can_sell(&state, &pool, user.account_id).await;
    let again = |why: &str| {
        private(
            "Design details · MakerFoundry",
            &details_form(
                id,
                &Details {
                    title: &title,
                    description: &description,
                    tags: &f.tags,
                    licence: &f.licence,
                    price: &f.price,
                    can_sell,
                    kind: &d.kind,
                    material: &f.material,
                    thickness: &f.thickness,
                    convert: f.convert == "yes",
                    category: &f.category,
                    original: [&f.original_site, &f.original_url, &f.original_date],
                },
                why,
            ),
            true,
        )
    };
    if title.is_empty() || title.chars().count() > MAX_TITLE {
        return again(&format!("Give a title of up to {MAX_TITLE} characters."));
    }
    if description.chars().count() > MAX_DESCRIPTION {
        return again(&format!("The description can be up to {MAX_DESCRIPTION} characters."));
    }
    if licences::find_for(&d.kind, &f.licence).is_none() {
        return again("Choose a licence.");
    }
    let price = match selling::check_price(&f.price) {
        Ok(p) => p,
        Err(why) => return again(&why),
    };
    if price > 0 && !can_sell {
        return again("To sell a design, first set up selling (Account › Selling). Until then, give it a price of 0.");
    }
    // a laser design's material and thickness (an embroidery design has neither)
    let (material, thickness) = if d.kind == "laser" {
        let material = match f.material.as_str() {
            "" => None,
            m if sizes::material_name(m).is_some() => Some(m.to_string()),
            _ => return again("Choose a material from the list."),
        };
        match sizes::check_thickness(&f.thickness) {
            Ok(t) => (material, t),
            Err(why) => return again(&why),
        }
    } else {
        (None, None)
    };
    let (category, origin) = if d.kind == "model3d" {
        let category = match crate::model3d::category(&f.category) {
            Some(c) => c.id,
            None => return again("Choose a category."),
        };
        let today = crate::model3d::origin::today();
        match crate::model3d::origin::check(&f.original_site, &f.original_url, &f.original_date, &today) {
            Ok(origin) => (Some(category), origin),
            Err(why) => return again(&why),
        }
    } else {
        (None, None)
    };
    if d.kind == "model3d"
        && let Err(why) = crate::model3d::remix::check_licence(&pool, id, user.account_id, &f.licence).await
    {
        return again(&why);
    }
    let saved = sqlx::query(
        "UPDATE mf_designs SET title = $2, description = $3, tags = $4, licence = $5, price_cents = $7,
             material = $8, thickness_mm = $9, convert = $10, category = $11,
             original_site = $12, original_url = $13, original_published_at = $14::date,
             search = setweight(to_tsvector('english', $2), 'A')
                   || setweight(to_tsvector('english', array_to_string($4::text[], ' ')), 'B')
                   || setweight(to_tsvector('english', $3), 'C')
         WHERE id = $1 AND account_id = $6",
    )
    .bind(id)
    .bind(&title)
    .bind(&description)
    .bind(&tags)
    .bind(&f.licence)
    .bind(user.account_id)
    .bind(price)
    .bind(&material)
    .bind(thickness)
    .bind(if f.convert_shown == "yes" {
        f.convert == "yes"
    } else {
        d.convert
    })
    .bind(category)
    .bind(origin.as_ref().map(|o| o.site))
    .bind(origin.as_ref().and_then(|o| o.url.clone()))
    .bind(origin.as_ref().and_then(|o| o.date.clone()))
    .execute(&pool)
    .await;
    match saved {
        Ok(_) => Redirect::to(&format!("/designs/{id}")).into_response(),
        Err(e) => {
            eprintln!("listings: save {id}: {e}");
            again("The details couldn't be saved just now. Please try again.")
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct PublishForm {
    confirm: String,
}

pub async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<PublishForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool, d) = match own(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let problem = if d.status == "removed" {
        Some(
            "This design was taken down after a report, so it can't be published again. If you believe that was a mistake, send a counter-notice.",
        )
    } else if !state.publishers.allows(user.account_id) {
        Some("Publishing opens when MakerFoundry launches. Your design stays saved as a draft.")
    } else if makers::profile(&pool, user.account_id).await.is_none() {
        Some("Set up your maker profile first: it's the name people see on your designs.")
    } else if d.kind == "model3d" && (d.title.is_empty() || d.licence.is_none() || d.category.is_none()) {
        Some("Give the model a title, a category and a licence first (Edit details).")
    } else if d.title.is_empty() || d.licence.is_none() {
        Some("Give the design a title and a licence first (Edit details).")
    } else if let Some(why) = match (d.kind.as_str(), d.licence.as_deref()) {
        ("model3d", Some(l)) => crate::model3d::remix::check_licence(&pool, id, user.account_id, l)
            .await
            .err(),
        _ => None,
    } {
        return designs::manage_page(&state, &pool, &user, id, &why).await;
    } else if d.price_cents > 0 && !selling::can_sell(&state, &pool, user.account_id).await {
        Some("To sell this design, finish setting up selling first (Account › Selling), or make it free.")
    } else if f.confirm != "yes" {
        Some(if state.launched {
            "Confirm that the design is yours to share, and that you agree to the Seller Terms."
        } else {
            "Confirm that the design is yours to share."
        })
    } else {
        None
    };
    if let Some(p) = problem {
        return designs::manage_page(&state, &pool, &user, id, p).await;
    }
    let done = sqlx::query("UPDATE mf_designs SET status = 'published', published_at = coalesce(published_at, now()) WHERE id = $1 AND account_id = $2 AND status = 'draft'")
        .bind(id)
        .bind(user.account_id)
        .execute(&pool)
        .await;
    match done {
        Ok(_) => {
            if state.launched {
                crate::legal::agree(&pool, user.account_id, &[&crate::legal::SELLER_TERMS]).await;
            }
            crate::notices::remix(&state, &pool, id, user.account_id).await;
            Redirect::to(&format!("/d/{id}/{}", slug(&d.title))).into_response()
        }
        Err(e) => {
            eprintln!("listings: publish {id}: {e}");
            designs::manage_page(
                &state,
                &pool,
                &user,
                id,
                "The design couldn't be published just now. Please try again.",
            )
            .await
        }
    }
}

pub async fn unpublish(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool, _) = match own(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(e) =
        sqlx::query("UPDATE mf_designs SET status = 'draft' WHERE id = $1 AND account_id = $2 AND status = 'published'")
            .bind(id)
            .bind(user.account_id)
            .execute(&pool)
            .await
    {
        eprintln!("listings: unpublish {id}: {e}");
    }
    Redirect::to(&format!("/designs/{id}")).into_response()
}

/// The part of a design's own (maker's) page about its listing: its details, and publishing.
pub fn manage_html(d: &DesignRow, id: i64, has_profile: bool, launched: bool) -> String {
    let licence = d
        .licence
        .as_deref()
        .and_then(licences::find)
        .map(|l| escape(l.name))
        .unwrap_or_else(|| "none yet".into());
    let details = format!(
        r#"<h2>Listing</h2><dl class="facts"><div><dt>Title</dt><dd>{title}</dd></div><div><dt>Licence</dt><dd>{licence}</dd></div><div><dt>Price</dt><dd>{price}</dd></div>{made_for}<div><dt>Tags</dt><dd>{tags}</dd></div></dl><p><a href="/designs/{id}/edit">Edit details</a></p>"#,
        title = if d.title.is_empty() {
            "none yet".into()
        } else {
            escape(&d.title)
        },
        made_for = match fits_text(d) {
            _ if d.kind == "model3d" => format!(
                r#"<div><dt>Category</dt><dd>{}</dd></div>"#,
                d.category
                    .as_deref()
                    .and_then(crate::model3d::category)
                    .map_or("none yet", |c| c.name)
                    .replace('&', "&amp;")
            ),
            Some(t) if d.kind == "laser" => format!(
                r#"<div><dt>Made for</dt><dd>{}</dd></div>"#,
                escape(t.trim_start_matches("for "))
            ),
            Some(t) => format!(r#"<div><dt>Hoop</dt><dd>{}</dd></div>"#, escape(&t)),
            None if d.kind == "laser" => r#"<div><dt>Made for</dt><dd>not given</dd></div>"#.to_string(),
            None => String::new(),
        },
        price = if d.price_cents == 0 {
            "Free".to_string()
        } else {
            dollars(d.price_cents)
        },
        tags = if d.tags.is_empty() {
            "none".into()
        } else {
            escape(&d.tags.join(", "))
        },
    );
    let profile = if has_profile {
        String::new()
    } else {
        r#"<p>Before publishing, <a href="/account/maker">set up your maker profile</a>.</p>"#.to_string()
    };
    let publishing = if d.status == "removed" {
        format!(
            r#"<p class="problem">{why} Only you and the admins can see it. If you believe it was taken down by mistake, <a href="/designs/{id}/counter">send a counter-notice</a>.</p>"#,
            why = escape(d.removed_reason.as_deref().unwrap_or("Taken down after a report."))
        )
    } else if d.status == "published" {
        format!(
            r#"<p class="byline">Published: <a href="/d/{id}/{slug}">see its public page</a>.</p><form method="post" action="/designs/{id}/unpublish"><button type="submit" class="secondary">Unpublish (back to a draft)</button></form>"#,
            slug = slug(&d.title)
        )
    } else {
        let seller_terms = if launched {
            r#" I agree to the <a href="/seller-terms">Seller Terms</a>."#
        } else {
            ""
        };
        format!(
            r#"{profile}<form method="post" action="/designs/{id}/publish" class="publish">
<label class="choice"><input type="checkbox" name="confirm" value="yes"> <span>{own}{seller_terms}</span></label>
<button type="submit">Publish</button>
</form>"#,
            own = if d.kind == "model3d" {
                "I created this model, or have the right to share it, and it doesn't copy anyone else's logos, characters or designs."
            } else {
                "I made this design, or have the right to share it, and it doesn't copy anyone else's logos, characters or designs."
            }
        )
    };
    format!("{details}{publishing}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_tidied() {
        assert_eq!(
            tidy_tags(" Flowers, CHRISTMAS ,flowers,, coaster <b>,  two   words "),
            ["flowers", "christmas", "coaster b", "two words"]
        );
        assert_eq!(tidy_tags(&"a,".repeat(20)).len(), 1);
        let many: String = (0..20).map(|i| format!("t{i},")).collect();
        assert_eq!(tidy_tags(&many).len(), MAX_TAGS);
        assert_eq!(tidy_tags(&"x".repeat(50))[0].len(), MAX_TAG);
    }

    #[test]
    fn slugs_and_paragraphs() {
        assert_eq!(slug("Rose Bouquet — 4×4 hoop!"), "rose-bouquet-4-4-hoop");
        assert_eq!(slug("<script>"), "script");
        assert_eq!(
            paragraphs("One\nline two\n\n<b>Two</b>"),
            "<p>One<br>line two</p><p>&lt;b&gt;Two&lt;/b&gt;</p>"
        );
    }

    #[test]
    fn publishers_come_from_the_setting() {
        assert!(!Publishers::from_setting(None).allows(1));
        assert!(Publishers::from_setting(Some("*")).allows(42));
        let some = Publishers::from_setting(Some("3, 7,x"));
        assert!(some.allows(7) && !some.allows(4));
    }

    #[test]
    fn query_words_are_encoded() {
        assert_eq!(url_word("cat & dog"), "cat%20%26%20dog");
    }
}
