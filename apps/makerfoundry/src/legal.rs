//! The legal pages (MF-6): the Terms of Service, the Privacy Policy and the Seller Terms, drawn
//! from `docs/legal/*.md` when the service is built, so the page and the document the owner
//! approved are one text.
//!
//! The wording is the owner's. While a document still has the owner's markers (`[OWNER: …]`, the
//! "DRAFT for the owner" line, or a "[When …]" note) it is a draft: only admins see it, marked as
//! a draft, and the site can't launch (`ready`). Everyone else sees that it's coming.
//!
//! When someone agrees to a document (signing up, publishing, setting up selling), the agreement
//! is kept with the document's version: the start of the SHA-256 of its text.

use std::sync::OnceLock;

use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Response};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::current_user;
use crate::pages::{escape, page};

/// A legal document.
pub struct Doc {
    /// Its key in agreements, and its address (`/<id>`).
    pub id: &'static str,
    pub title: &'static str,
    pub source: &'static str,
}

pub const TERMS: Doc = Doc {
    id: "terms",
    title: "Terms of Service",
    source: include_str!("../../../docs/legal/terms.md"),
};
pub const PRIVACY: Doc = Doc {
    id: "privacy",
    title: "Privacy Policy",
    source: include_str!("../../../docs/legal/privacy.md"),
};
pub const SELLER_TERMS: Doc = Doc {
    id: "seller-terms",
    title: "Seller Terms",
    source: include_str!("../../../docs/legal/seller-terms.md"),
};

pub const DOCS: [&Doc; 3] = [&TERMS, &PRIVACY, &SELLER_TERMS];

impl Doc {
    /// How many of the owner's markers are left: 0 once the document is approved.
    pub fn unfinished(&self) -> usize {
        self.source.matches("[OWNER").count()
            + self.source.matches("DRAFT for the owner").count()
            + self.source.matches("[When ").count()
    }

    /// The version agreed to: the first 12 hex digits of the text's SHA-256.
    pub fn version(&self) -> &'static str {
        static VERSIONS: OnceLock<[String; 3]> = OnceLock::new();
        let all = VERSIONS.get_or_init(|| DOCS.map(|d| hex12(d.source)));
        DOCS.iter()
            .position(|d| d.id == self.id)
            .and_then(|i| all.get(i))
            .map_or("", String::as_str)
    }
}

fn hex12(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether every document is approved, so the site may launch.
pub fn ready() -> bool {
    DOCS.iter().all(|d| d.unfinished() == 0)
}

/// Records that `account` agreed to `docs` as they are now. A failure is logged: the person has
/// agreed either way, and the pages still say what they agreed to.
pub async fn agree(pool: &PgPool, account: i64, docs: &[&Doc]) {
    for d in docs {
        let r = sqlx::query(
            "INSERT INTO mf_agreements (account_id, document, version) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(account)
        .bind(d.id)
        .bind(d.version())
        .execute(pool)
        .await;
        if let Err(e) = r {
            eprintln!("legal: agreement of {account} to {}: {e}", d.id);
        }
    }
}

/// Whether `account`'s latest agreement to `doc` is to its current version. A database error counts
/// as "no": whatever this gates stays closed (issue #96).
pub async fn accepted_current(pool: &PgPool, account: i64, doc: &Doc) -> bool {
    sqlx::query_scalar::<_, String>(
        "SELECT version FROM mf_agreements WHERE account_id = $1 AND document = $2 ORDER BY at DESC, version LIMIT 1",
    )
    .bind(account)
    .bind(doc.id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .is_some_and(|v| v == doc.version())
}

async fn show(state: &AppState, headers: &HeaderMap, doc: &Doc) -> Response {
    let viewer = current_user(state, headers).await;
    let admin = viewer.as_ref().is_some_and(|u| state.admins.contains(&u.account_id));
    let body = if state.launched || admin {
        let draft = if doc.unfinished() > 0 {
            format!(
                r#"<p class="problem">A draft for the owner, seen only by admins: {n} of the owner's decisions left (marked). Not in force.</p>"#,
                n = doc.unfinished()
            )
        } else {
            String::new()
        };
        format!(
            r#"<section class="design prose legal">{draft}{}</section>"#,
            to_html(doc.source)
        )
    } else {
        format!(
            r#"<section class="hero"><h1>{t}</h1><p class="lead">MakerFoundry hasn't opened yet. Its {t} will be here when it does.</p></section>"#,
            t = escape(doc.title)
        )
    };
    (
        [(header::CACHE_CONTROL, "no-cache")],
        Html(page(
            &format!("{} · MakerFoundry", escape(doc.title)),
            &body,
            viewer.is_some(),
        )),
    )
        .into_response()
}

pub async fn terms(State(state): State<AppState>, headers: HeaderMap) -> Response {
    show(&state, &headers, &TERMS).await
}

pub async fn privacy(State(state): State<AppState>, headers: HeaderMap) -> Response {
    show(&state, &headers, &PRIVACY).await
}

pub async fn seller_terms(State(state): State<AppState>, headers: HeaderMap) -> Response {
    show(&state, &headers, &SELLER_TERMS).await
}

// ---------------------------------------------------------------- the documents as HTML

/// The Markdown the legal documents use, as HTML: headings, paragraphs, quotes, lists, tables,
/// **bold**, `code` and links. Everything else is text, escaped.
pub fn to_html(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    while let Some(line) = lines.get(i) {
        let t = line.trim_start();
        if t.is_empty() {
            i += 1;
        } else if let Some((level, text)) = heading(t) {
            out.push_str(&format!("<h{level}>{}</h{level}>", inline(text)));
            i += 1;
        } else if t.starts_with('>') {
            let mut text = Vec::new();
            while let Some(l) = lines.get(i).map(|l| l.trim_start()).filter(|l| l.starts_with('>')) {
                text.push(l.trim_start_matches('>').trim());
                i += 1;
            }
            out.push_str(&format!("<blockquote><p>{}</p></blockquote>", inline(&text.join(" "))));
        } else if t.starts_with('|') {
            let mut rows = Vec::new();
            while let Some(l) = lines.get(i).map(|l| l.trim()).filter(|l| l.starts_with('|')) {
                rows.push(l);
                i += 1;
            }
            out.push_str(&table(&rows));
        } else if let Some(ordered) = list_item(t).map(|(o, _)| o) {
            let mut items: Vec<String> = Vec::new();
            while let Some(l) = lines.get(i) {
                let lt = l.trim_start();
                if lt.is_empty() {
                    break;
                }
                match list_item(lt) {
                    Some((o, text)) if o == ordered => items.push(text.to_string()),
                    Some(_) => break,
                    // a continuation line, indented under its item
                    None if l.starts_with(' ') => {
                        if let Some(last) = items.last_mut() {
                            last.push(' ');
                            last.push_str(lt);
                        }
                    }
                    None => break,
                }
                i += 1;
            }
            let tag = if ordered { "ol" } else { "ul" };
            let lis: String = items.iter().map(|it| format!("<li>{}</li>", inline(it))).collect();
            out.push_str(&format!("<{tag}>{lis}</{tag}>"));
        } else {
            let mut text = Vec::new();
            while let Some(l) = lines.get(i).map(|l| l.trim()) {
                if l.is_empty()
                    || heading(l).is_some()
                    || l.starts_with('>')
                    || l.starts_with('|')
                    || list_item(l).is_some()
                {
                    break;
                }
                text.push(l);
                i += 1;
            }
            out.push_str(&format!("<p>{}</p>", inline(&text.join(" "))));
        }
    }
    out
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    let text = line.get(level..)?.strip_prefix(' ')?;
    (1..=4).contains(&level).then_some((level, text.trim()))
}

/// `- text` (unordered) or `12. text` (ordered) → whether ordered, and the text.
fn list_item(line: &str) -> Option<(bool, &str)> {
    if let Some(text) = line.strip_prefix("- ") {
        return Some((false, text));
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    line.get(digits..)?.strip_prefix(". ").map(|text| (true, text))
}

fn table(rows: &[&str]) -> String {
    let cells = |row: &str| -> Vec<String> {
        row.trim()
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect()
    };
    let mut html = String::from(r#"<div class="table"><table>"#);
    for (n, row) in rows.iter().enumerate() {
        // the line under the header
        if n == 1 && row.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
            continue;
        }
        let tag = if n == 0 { "th" } else { "td" };
        let tds: String = cells(row)
            .iter()
            .map(|c| format!("<{tag}>{}</{tag}>", inline(c)))
            .collect();
        html.push_str(&format!("<tr>{tds}</tr>"));
    }
    html.push_str("</table></div>");
    html
}

/// A line's text with its inline marks as HTML, everything else escaped.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let mut bold = false;
    let mut code = false;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if code && c != '`' {
            out.push_str(&escape(&c.to_string()));
            rest = rest.get(c.len_utf8()..).unwrap_or("");
            continue;
        }
        if let Some(r) = rest.strip_prefix("**") {
            out.push_str(if bold { "</strong>" } else { "<strong>" });
            bold = !bold;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('`') {
            out.push_str(if code { "</code>" } else { "<code>" });
            code = !code;
            rest = r;
        } else if rest.starts_with("[OWNER")
            && let Some(end) = rest.find(']')
        {
            out.push_str(&format!("<mark>{}</mark>", escape(rest.get(..=end).unwrap_or(""))));
            rest = rest.get(end + 1..).unwrap_or("");
        } else if let Some((label, url, r)) = link(rest) {
            out.push_str(&format!(r#"<a href="{}">{}</a>"#, escape(url), escape(label)));
            rest = r;
        } else {
            out.push_str(&escape(&c.to_string()));
            rest = rest.get(c.len_utf8()..).unwrap_or("");
        }
    }
    if code {
        out.push_str("</code>");
    }
    if bold {
        out.push_str("</strong>");
    }
    out
}

/// `[label](url)` at the start of `text`, for a web, site or email address → label, url, the rest.
fn link(text: &str) -> Option<(&str, &str, &str)> {
    let r = text.strip_prefix('[')?;
    let (label, r) = r.split_once("](")?;
    let (url, r) = r.split_once(')')?;
    let safe = url.starts_with("https://") || url.starts_with('/') || url.starts_with("mailto:");
    (safe && !label.contains('[') && !url.contains(char::is_whitespace)).then_some((label, url, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_becomes_html_and_text_stays_text() {
        let md = "# Title\n\n> **DRAFT** note\n> more\n\nOne line\nand <two>.\n\n- **A**: x\n  continued\n- B [OWNER: decide]\n\n1. first\n2. [the dmca page](/dmca) and [bad](javascript:alert(1))\n\n| H1 | H2 |\n|---|---|\n| a & b | `c<d>` |\n";
        assert_eq!(
            to_html(md),
            "<h1>Title</h1><blockquote><p><strong>DRAFT</strong> note more</p></blockquote>\
             <p>One line and &lt;two&gt;.</p>\
             <ul><li><strong>A</strong>: x continued</li><li>B <mark>[OWNER: decide]</mark></li></ul>\
             <ol><li>first</li><li><a href=\"/dmca\">the dmca page</a> and [bad](javascript:alert(1))</li></ol>\
             <div class=\"table\"><table><tr><th>H1</th><th>H2</th></tr><tr><td>a &amp; b</td><td><code>c&lt;d&gt;</code></td></tr></table></div>"
        );
        // unclosed marks are closed
        assert_eq!(inline("**open `x"), "<strong>open <code>x</code></strong>");
    }

    #[test]
    fn the_documents_render_and_count_their_drafts() {
        for d in DOCS {
            let html = to_html(d.source);
            assert!(html.starts_with("<h1>"), "{}", d.id);
            assert!(!html.contains("<script"), "{}", d.id);
            assert_eq!(d.version().len(), 12);
        }
        assert_ne!(TERMS.version(), PRIVACY.version());
        let draft = Doc {
            id: "x",
            title: "x",
            source: "> DRAFT for the owner\n\n[OWNER: a] [When paid]",
        };
        assert_eq!(draft.unfinished(), 3);
        assert_eq!(ready(), DOCS.iter().all(|d| d.unfinished() == 0));
    }
}
