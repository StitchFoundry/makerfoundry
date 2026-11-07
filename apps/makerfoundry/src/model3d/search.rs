//! Finding 3D models (MF-49): words (the same weighted full-text index as `/browse`), category,
//! licence, file type, has makes, has a print profile, printer, multi-colour and filament type,
//! newest or popular first, paged by key (`after=<the last model shown>`), never by offset.
//! What the filters need is kept on each model (`refresh_facets`) so a page reads one table
//! through its indexes: under 100 ms with 100,000 models (tested).

use serde::Deserialize;
use sqlx::{PgPool, Row};

use super::profile::PRINTERS;
use crate::listings::Card;
use crate::pages::escape;

/// Models a page shows, unless asked for fewer.
const PER_PAGE: i64 = 24;

/// What a search asks for, as the form sends it.
#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct Query {
    pub q: String,
    pub category: String,
    pub licence: String,
    pub format: String,
    pub printer: String,
    pub filament: String,
    pub makes: String,
    pub profile: String,
    pub multicolour: String,
    pub sort: String,
    pub after: Option<i64>,
    pub per: Option<i64>,
}

/// The file types a search can ask for.
const FORMATS: &[(&str, &str)] = &[("stl", "STL"), ("3mf", "3MF"), ("obj", "OBJ"), ("step", "STEP")];
/// The filament types offered in the form (any other typed into the address still works).
const FILAMENTS: &[&str] = &["PLA", "PETG", "ABS", "ASA", "TPU", "PA", "PC", "PLA-CF", "PETG-CF"];

/// Notes what model `id`'s filters need from its files and profiles: run after its files change.
pub async fn refresh_facets<'e, E: sqlx::PgExecutor<'e>>(db: E, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE mf_designs d SET
            formats = coalesce((SELECT array_agg(DISTINCT f.format) FROM mf_model_files f WHERE f.design_id = d.id), '{}'),
            printers = coalesce((SELECT array_agg(DISTINCT b) FROM mf_print_profiles p, unnest(p.printers) b WHERE p.design_id = d.id), '{}'),
            filament_types = coalesce((SELECT array_agg(DISTINCT x ->> 'type') FROM mf_print_profiles p, jsonb_array_elements(p.filaments) x
                                       WHERE p.design_id = d.id), '{}'),
            has_profile = EXISTS (SELECT 1 FROM mf_print_profiles p WHERE p.design_id = d.id AND p.readable),
            multicolour = EXISTS (SELECT 1 FROM mf_print_profiles p WHERE p.design_id = d.id
                                  AND (SELECT count(DISTINCT x ->> 'colour') FROM jsonb_array_elements(p.filaments) x) > 1)
         WHERE d.id = $1",
    )
    .bind(id)
    .execute(db)
    .await
    .map(|_| ())
}

/// How popular each published model is lately: its likes and makes of the last 30 days, and its
/// downloads (the 10-minute sweep).
pub async fn refresh_popularity(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE mf_designs d SET popularity = n.score FROM (
             SELECT d2.id, d2.downloads
                    + (SELECT count(*) FROM mf_likes l WHERE l.design_id = d2.id AND l.created_at > now() - interval '30 days')
                    + (SELECT count(*) FROM mf_makes k WHERE k.design_id = d2.id AND k.created_at > now() - interval '30 days') AS score
             FROM mf_designs d2 WHERE d2.kind = 'model3d' AND d2.status = 'published') n
         WHERE d.id = n.id AND d.popularity IS DISTINCT FROM n.score",
    )
    .execute(pool)
    .await
    .map(|_| ())
}

/// A filter's value if it's one of `allowed` (anything else is ignored, not an error).
fn one_of<'a>(value: &str, allowed: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let value = value.trim();
    allowed.into_iter().find(|a| *a == value).map(str::to_string)
}

/// A query's filters, checked.
struct Checked {
    words: Option<String>,
    category: Option<String>,
    licence: Option<String>,
    format: Option<String>,
    printer: Option<String>,
    filament: Option<String>,
    makes: bool,
    profile: bool,
    multicolour: bool,
    popular: bool,
    after: Option<i64>,
    per: i64,
}

fn check(q: &Query) -> Checked {
    let words: String =
        q.q.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(100)
            .collect();
    let filament = q.filament.trim().to_ascii_uppercase();
    Checked {
        words: (!words.is_empty()).then_some(words),
        category: one_of(&q.category, super::CATEGORIES.iter().map(|c| c.id)),
        licence: one_of(&q.licence, crate::licences::for_kind("model3d").iter().map(|l| l.id)),
        format: one_of(&q.format, FORMATS.iter().map(|f| f.0)),
        printer: one_of(&q.printer, PRINTERS.iter().map(|p| p.1)),
        filament: (!filament.is_empty()
            && filament.len() <= 20
            && filament.chars().all(|c| c.is_ascii_alphanumeric() || "-+ ".contains(c)))
        .then_some(filament),
        makes: q.makes == "yes",
        profile: q.profile == "yes",
        multicolour: q.multicolour == "yes",
        popular: q.sort == "popular",
        after: q.after,
        per: q.per.unwrap_or(PER_PAGE).clamp(1, PER_PAGE),
    }
}

/// One page of published 3D models → the cards, and the id to go on after if there's more.
pub async fn find(pool: &PgPool, q: &Query) -> (Vec<Card>, Option<i64>) {
    let c = check(q);
    // (only these two fixed orders are ever put into the query's text)
    let (order, after) = if c.popular {
        (
            "d.popularity DESC, d.id DESC",
            "(d.popularity, d.id) < ((SELECT popularity FROM mf_designs WHERE id = $11), $11)",
        )
    } else {
        (
            "d.published_at DESC, d.id DESC",
            "(d.published_at, d.id) < ((SELECT published_at FROM mf_designs WHERE id = $11), $11)",
        )
    };
    let sql = format!(
        "SELECT d.id, d.title, d.kind, d.format, d.downloads, d.price_cents, m.name AS maker,
                (SELECT p.id FROM mf_design_photos p WHERE p.design_id = d.id ORDER BY p.position LIMIT 1) AS photo,
                (SELECT f.id FROM mf_model_files f WHERE f.design_id = d.id AND f.thumb_key IS NOT NULL ORDER BY f.position LIMIT 1) AS model_file
         FROM mf_designs d JOIN mf_makers m ON m.account_id = d.account_id
         WHERE d.kind = 'model3d' AND d.status = 'published'
           AND ($1::text IS NULL OR d.search @@ websearch_to_tsquery('english', $1))
           AND ($2::text IS NULL OR d.category = $2)
           AND ($3::text IS NULL OR d.licence = $3)
           AND ($4::text IS NULL OR d.formats @> ARRAY[$4])
           AND ($5::text IS NULL OR d.printers @> ARRAY[$5])
           AND ($6::text IS NULL OR d.filament_types @> ARRAY[$6])
           AND (NOT $7 OR EXISTS (SELECT 1 FROM mf_makes k WHERE k.design_id = d.id))
           AND (NOT $8 OR d.has_profile)
           AND (NOT $9 OR d.multicolour)
           AND ($11::bigint IS NULL OR {after})
         ORDER BY {order}
         LIMIT $10 + 1"
    );
    let rows = sqlx::query(&sql)
        .bind(&c.words)
        .bind(&c.category)
        .bind(&c.licence)
        .bind(&c.format)
        .bind(&c.printer)
        .bind(&c.filament)
        .bind(c.makes)
        .bind(c.profile)
        .bind(c.multicolour)
        .bind(c.per)
        .bind(c.after)
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| {
            eprintln!("3D search: {e}");
            Vec::new()
        });
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
    let more = cards.len() as i64 > c.per;
    cards.truncate(usize::try_from(c.per).unwrap_or(0));
    let next = if more { cards.last().map(|c| c.id) } else { None };
    (cards, next)
}

/// The search form, filled in as asked.
pub fn form_html(q: &Query) -> String {
    let c = check(q);
    let options = |list: Vec<(String, String)>, chosen: &Option<String>, any: &str| -> String {
        let mut out = format!(r#"<option value="">{any}</option>"#);
        for (value, name) in list {
            let selected = if chosen.as_deref() == Some(value.as_str()) {
                " selected"
            } else {
                ""
            };
            out.push_str(&format!(
                r#"<option value="{}"{selected}>{}</option>"#,
                escape(&value),
                escape(&name)
            ));
        }
        out
    };
    let categories = options(
        super::CATEGORIES.iter().map(|x| (x.id.into(), x.name.into())).collect(),
        &c.category,
        "Every category",
    );
    let licences = options(
        crate::licences::for_kind("model3d")
            .iter()
            .map(|l| (l.id.into(), l.name.into()))
            .collect(),
        &c.licence,
        "Any licence",
    );
    let formats = options(
        FORMATS.iter().map(|(v, n)| ((*v).into(), (*n).into())).collect(),
        &c.format,
        "Any file type",
    );
    let printers = options(
        PRINTERS.iter().map(|(_, v, n)| ((*v).into(), (*n).into())).collect(),
        &c.printer,
        "Any printer",
    );
    let filaments = options(
        FILAMENTS.iter().map(|f| ((*f).into(), (*f).into())).collect(),
        &c.filament,
        "Any filament",
    );
    let tick = |name: &str, on: bool, label: &str| {
        format!(
            r#"<label class="choice"><input type="checkbox" name="{name}" value="yes"{}> <span>{label}</span></label>"#,
            if on { " checked" } else { "" }
        )
    };
    format!(
        r#"<form method="get" action="/3d" class="search">
<input name="q" value="{q}" placeholder="Search models" aria-label="Search" maxlength="100">
<select name="category" aria-label="Category">{categories}</select>
<select name="licence" aria-label="Licence">{licences}</select>
<select name="format" aria-label="File type">{formats}</select>
<select name="printer" aria-label="Printer">{printers}</select>
<select name="filament" aria-label="Filament">{filaments}</select>
{makes}{profile}{multicolour}
<select name="sort" aria-label="Sort"><option value="newest">Newest</option><option value="popular"{popular}>Popular</option></select>
<button type="submit">Search</button>
</form>"#,
        q = escape(c.words.as_deref().unwrap_or("")),
        makes = tick("makes", c.makes, "Has makes"),
        profile = tick("profile", c.profile, "Has print settings"),
        multicolour = tick("multicolour", c.multicolour, "Multi-colour"),
        popular = if c.popular { " selected" } else { "" },
    )
}

/// The address of the next page: the same search, after `after`.
pub fn next_href(q: &Query, after: i64) -> String {
    let c = check(q);
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut add = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            parts.push((k.to_string(), v));
        }
    };
    add("q", c.words);
    add("category", c.category);
    add("licence", c.licence);
    add("format", c.format);
    add("printer", c.printer);
    add("filament", c.filament);
    add("makes", c.makes.then(|| "yes".into()));
    add("profile", c.profile.then(|| "yes".into()));
    add("multicolour", c.multicolour.then(|| "yes".into()));
    add("sort", c.popular.then(|| "popular".into()));
    add("per", (c.per != PER_PAGE).then(|| c.per.to_string()));
    add("after", Some(after.to_string()));
    let query: String = parts
        .iter()
        .map(|(k, v)| format!("{k}={}", crate::listings::url_word(v)))
        .collect::<Vec<_>>()
        .join("&amp;");
    format!("/3d?{query}")
}
