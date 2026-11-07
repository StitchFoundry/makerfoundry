//! The admins' overview (MF-16): the site's numbers on one page at `/admin/overview`: accounts,
//! designs, downloads, sales and MakerFoundry's fees (from the ledger), and what's waiting in the
//! report queue. Only admins (`MF_ADMINS`) see it.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::private;
use crate::moderation::admin;
use crate::money::dollars;
use crate::pages::escape;

/// One number, or 0 if it can't be counted just now (logged).
async fn n(pool: &PgPool, sql: &str) -> i64 {
    match sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("overview: {e}");
            0
        }
    }
}

fn group(title: &str, facts: &[(&str, String)]) -> String {
    let items: String = facts
        .iter()
        .map(|(label, value)| format!("<div><dt>{}</dt><dd>{}</dd></div>", escape(label), escape(value)))
        .collect();
    format!(r#"<h2>{}</h2><dl class="facts">{items}</dl>"#, escape(title))
}

pub async fn overview(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (_, pool) = match admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let p = &pool;
    let count = |v: i64| v.to_string();
    let people = group(
        "People",
        &[
            (
                "Accounts",
                count(n(p, "SELECT count(*) FROM mf_accounts WHERE email <> ''").await),
            ),
            (
                "New this week",
                count(
                    n(
                        p,
                        "SELECT count(*) FROM mf_accounts WHERE created_at > now() - interval '7 days'",
                    )
                    .await,
                ),
            ),
            ("Maker profiles", count(n(p, "SELECT count(*) FROM mf_makers").await)),
            (
                "Sellers ready",
                count(n(p, "SELECT count(*) FROM mf_sellers WHERE charges_enabled").await),
            ),
            ("Follows", count(n(p, "SELECT count(*) FROM mf_follows").await)),
        ],
    );
    let designs = group(
        "Designs",
        &[
            (
                "Published",
                count(n(p, "SELECT count(*) FROM mf_designs WHERE status = 'published'").await),
            ),
            (
                "Embroidery",
                count(
                    n(
                        p,
                        "SELECT count(*) FROM mf_designs WHERE status = 'published' AND kind = 'embroidery'",
                    )
                    .await,
                ),
            ),
            (
                "Laser",
                count(
                    n(
                        p,
                        "SELECT count(*) FROM mf_designs WHERE status = 'published' AND kind = 'laser'",
                    )
                    .await,
                ),
            ),
            (
                "Paid",
                count(
                    n(
                        p,
                        "SELECT count(*) FROM mf_designs WHERE status = 'published' AND price_cents > 0",
                    )
                    .await,
                ),
            ),
            (
                "Drafts",
                count(n(p, "SELECT count(*) FROM mf_designs WHERE status = 'draft'").await),
            ),
            (
                "Taken down",
                count(n(p, "SELECT count(*) FROM mf_designs WHERE status = 'removed'").await),
            ),
            (
                "New this week",
                count(
                    n(
                        p,
                        "SELECT count(*) FROM mf_designs WHERE published_at > now() - interval '7 days'",
                    )
                    .await,
                ),
            ),
            (
                "Downloads",
                count(n(p, "SELECT coalesce(sum(downloads), 0)::bigint FROM mf_designs").await),
            ),
            ("Makes", count(n(p, "SELECT count(*) FROM mf_makes").await)),
            ("Comments", count(n(p, "SELECT count(*) FROM mf_comments").await)),
        ],
    );
    // money, from the ledger (refunds already reversed there)
    let sales = group(
        "Sales",
        &[
            (
                "Sales",
                count(n(p, "SELECT count(*) FROM mf_orders WHERE status = 'paid'").await),
            ),
            (
                "Refunded",
                count(n(p, "SELECT count(*) FROM mf_orders WHERE status = 'refunded'").await),
            ),
            (
                "Disputed",
                count(n(p, "SELECT count(*) FROM mf_orders WHERE dispute_id IS NOT NULL").await),
            ),
            (
                "Charged back",
                count(n(p, "SELECT count(*) FROM mf_orders WHERE status = 'chargeback'").await),
            ),
            (
                "Paid by buyers",
                dollars(n(p, "SELECT coalesce(sum(cents), 0)::bigint FROM mf_ledger").await),
            ),
            (
                "To sellers",
                dollars(n(p, "SELECT coalesce(sum(cents), 0)::bigint FROM mf_ledger WHERE kind = 'seller'").await),
            ),
            (
                "MakerFoundry's fees",
                dollars(n(p, "SELECT coalesce(sum(cents), 0)::bigint FROM mf_ledger WHERE kind = 'fee'").await),
            ),
            (
                "Card fees",
                dollars(n(p, "SELECT coalesce(sum(cents), 0)::bigint FROM mf_ledger WHERE kind = 'processing'").await),
            ),
            (
                "Sales tax collected",
                dollars(n(p, "SELECT coalesce(sum(cents), 0)::bigint FROM mf_ledger WHERE kind = 'tax'").await),
            ),
            (
                "Fees this month",
                dollars(
                    n(
                        p,
                        "SELECT coalesce(sum(cents), 0)::bigint FROM mf_ledger WHERE kind = 'fee' AND at >= date_trunc('month', now())",
                    )
                    .await,
                ),
            ),
        ],
    );
    let reports = group(
        "Waiting in the queue",
        &[
            ("Copyright notices", count(n(p, "SELECT count(*) FROM mf_reports WHERE status = 'open' AND kind = 'dmca'").await)),
            ("Counter-notices", count(n(p, "SELECT count(*) FROM mf_reports WHERE status = 'open' AND kind = 'counter'").await)),
            ("Reports", count(n(p, "SELECT count(*) FROM mf_reports WHERE status = 'open' AND kind IN ('report', 'make', 'comment')").await)),
        ],
    );
    private(
        "Overview · MakerFoundry",
        &format!(
            r#"<section class="design"><h1>Overview</h1><p class="muted"><a href="/admin">Reports</a> · <a href="/admin/orders">Orders</a> · <a href="/admin/founding">Founding creators</a> · <a href="/admin/apps">Designs from apps</a> · <a href="/admin/accounts">Delete an account's data</a></p>{people}{designs}{sales}{reports}</section>"#
        ),
        true,
    )
    .into_response()
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct AppQuery {
    app: String,
}

/// `GET /admin/apps?app=<name>` (MF-88): how many designs each app has sent, and the latest 100 of
/// one app's (any app's without `app`), drafts included.
pub async fn apps(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<AppQuery>) -> Response {
    let (_, pool) = match admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let counts: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT from_app, count(*), count(*) FILTER (WHERE status = 'published')
         FROM mf_designs WHERE from_app IS NOT NULL GROUP BY from_app ORDER BY count(*) DESC, from_app LIMIT 50",
    )
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    let apps: String = counts
        .iter()
        .map(|(app, all, published)| {
            format!(
                r#"<li><a href="/admin/apps?app={link}">{name}</a> <span class="muted">{all} sent, {published} published</span></li>"#,
                link = crate::listings::url_word(app),
                name = escape(app),
            )
        })
        .collect();
    let chosen = crate::api::app_name(&q.app);
    let rows: Vec<(i64, String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT id, title, kind, format, status, from_app, to_char(created_at, 'YYYY-MM-DD') FROM mf_designs
         WHERE from_app IS NOT NULL AND ($1::text IS NULL OR from_app = $1)
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(&chosen)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    let list: String = rows
        .iter()
        .map(|(id, title, kind, format, status, app, at)| {
            format!(
                r#"<tr><td><a href="/d/{id}">{id}</a></td><td>{}</td><td>{} {}</td><td>{}</td><td>{}</td><td>{at}</td></tr>"#,
                escape(if title.is_empty() { "(untitled)" } else { title }),
                escape(kind),
                escape(format),
                escape(status),
                escape(app),
            )
        })
        .collect();
    let heading = match &chosen {
        Some(app) => format!("Latest from {}", escape(app)),
        None => "Latest from any app".to_string(),
    };
    private(
        "Designs from apps · MakerFoundry",
        &format!(
            r#"<section class="design"><h1>Designs from apps</h1><p class="muted"><a href="/admin/overview">Overview</a> · <a href="/admin/apps">All apps</a></p>
<ul>{apps}</ul><h2>{heading}</h2><div class="admin-table"><table><tr><th>Design</th><th>Title</th><th>Kind</th><th>Status</th><th>App</th><th>Sent</th></tr>{list}</table></div></section>"#,
            apps = if apps.is_empty() { "<li>No designs have come from an app yet.</li>".to_string() } else { apps },
        ),
        true,
    )
    .into_response()
}
