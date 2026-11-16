//! Your data (MF-15): what the Privacy Policy promises, done by the person themselves at
//! `/account/data`: a copy of everything MakerFoundry keeps about their account (JSON), and
//! deleting it.
//!
//! Deleting takes the maker profile, designs and their files and photos, makes, comments,
//! collections, follows and sign-ins. It keeps what the law asks to be kept: purchases and sales
//! and their ledger rows, agreements to the legal pages, and reports and copyright notices. A
//! design people have bought is unpublished but kept, so its buyers keep their downloads (Seller
//! Terms § 8). The Foundry Account itself (email, password, credits) is StitchFoundry's, closed
//! there.

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{COOKIE, User, current_user, forbidden, private, same_origin};
use crate::pages::escape;

fn page_html(problem: &str) -> String {
    let problem = if problem.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
    };
    format!(
        r#"<section class="form prose">
<h1>Your data</h1>
<h2>A copy</h2>
<p>Everything MakerFoundry keeps about your account, in one file (JSON): your maker profile, your designs' details, your collections, the makers you follow, your comments and makes, your purchases and sales, and the legal pages you agreed to. Your design files and photos you can download from your designs' pages.</p>
<p><a class="button" href="/account/data.json">Download a copy</a></p>
<h2>Delete your MakerFoundry data</h2>
<p>This deletes your maker profile, your designs with their files and photos, your makes, comments, collections and follows, and signs you out everywhere. It can't be undone.</p>
<p>Kept, because the law asks us to: your purchases and sales and their amounts, which legal pages you agreed to, and reports and copyright notices. A design people have bought is unpublished but kept, so they keep their downloads. Orders from shops keep their amounts; your name, address, notes, art and the shops' proofs are deleted. While an order from a shop is still in progress, it waits until the order is delivered, collected or cancelled.</p>
<p>Your Foundry Account itself (email, password, credits) is kept by StitchFoundry, and closed there.</p>
{problem}
<form method="post" action="/account/data/delete" class="add-file">
<label><span>To delete, type <strong>delete</strong></span><input name="confirm" autocomplete="off" required></label>
<button type="submit">Delete my MakerFoundry data</button>
</form>
<p><a href="/account">Your account</a></p>
</section>"#
    )
}

pub async fn page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if current_user(&state, &headers).await.is_none() {
        return Redirect::to("/signin?next=/account/data").into_response();
    }
    private("Your data · MakerFoundry", &page_html(""), true)
}

/// Rows of `sql` (taking the account id as `$1`) as JSON objects.
async fn rows(pool: &PgPool, sql: &str, account: i64) -> Result<Value, sqlx::Error> {
    let wrapped = format!("SELECT coalesce(json_agg(t), '[]'::json) FROM ({sql}) t");
    sqlx::query_scalar::<_, Value>(&wrapped)
        .bind(account)
        .fetch_one(pool)
        .await
}

/// Everything kept about `user`'s account.
pub async fn export(pool: &PgPool, user: &User) -> Result<Value, sqlx::Error> {
    let a = user.account_id;
    Ok(json!({
        "account": { "id": a, "email": user.email },
        "maker_profile": rows(pool, "SELECT handle, name, bio FROM mf_makers WHERE account_id = $1", a).await?,
        "designs": rows(pool,
            "SELECT d.id, d.kind, d.format, d.file_name, d.title, d.description, d.tags, d.licence, d.price_cents,
                    d.status, d.material, d.thickness_mm, d.convert, d.from_app, d.downloads, d.created_at, d.published_at,
                    (SELECT count(*) FROM mf_design_photos p WHERE p.design_id = d.id) AS photos,
                    (SELECT json_agg(f.format) FROM mf_design_files f WHERE f.design_id = d.id) AS other_formats
             FROM mf_designs d WHERE d.account_id = $1 ORDER BY d.id", a).await?,
        "collections": rows(pool,
            "SELECT c.name, c.public, c.is_saved AS saved_list, c.created_at,
                    (SELECT json_agg(i.design_id) FROM mf_collection_items i WHERE i.collection_id = c.id) AS designs
             FROM mf_collections c WHERE c.account_id = $1 ORDER BY c.id", a).await?,
        "following": rows(pool,
            "SELECT m.handle, f.at FROM mf_follows f JOIN mf_makers m ON m.account_id = f.maker_account
             WHERE f.follower_account = $1", a).await?,
        "showcase_consents": rows(pool, "SELECT design_id, make_id, given_at, withdrawn_at FROM mf_showcase_consents WHERE account_id = $1 ORDER BY given_at", a).await?,
        "likes": rows(pool, "SELECT design_id, created_at FROM mf_likes WHERE account_id = $1 ORDER BY created_at", a).await?,
        "contest_entries": rows(pool, "SELECT contest_id, design_id, entered_at, place FROM mf_contest_entries WHERE account_id = $1 ORDER BY entered_at", a).await?,
        "contest_votes": rows(pool, "SELECT contest_id, design_id, voted_at FROM mf_contest_votes WHERE account_id = $1 ORDER BY voted_at", a).await?,
        "api_keys": rows(pool, "SELECT name, created_at, last_used_at, revoked_at FROM mf_api_keys WHERE account_id = $1 ORDER BY created_at", a).await?,
        "notice_prefs": rows(pool, "SELECT digest, last_sent_at FROM mf_notice_prefs WHERE account_id = $1", a).await?,
        "notices": rows(pool, "SELECT kind, design_id, created_at, sent_at FROM mf_notices WHERE account_id = $1 ORDER BY created_at", a).await?,
        "imports": rows(pool, "SELECT i.id, i.created_at, f.path, f.problem FROM mf_imports i JOIN mf_import_files f ON f.import_id = i.id WHERE i.account_id = $1 ORDER BY i.id, f.position", a).await?,
        "comments": rows(pool, "SELECT design_id, body, created_at FROM mf_comments WHERE account_id = $1 ORDER BY id", a).await?,
        "makes": rows(pool, "SELECT design_id, note, created_at FROM mf_makes WHERE account_id = $1 ORDER BY id", a).await?,
        "shop_orders": rows(pool,
            "SELECT o.ref, s.name AS shop, o.status, o.lines, o.customer_name, o.ship_to, o.note, o.goods_cents,
                    o.setup_cents, o.shipping_cents, o.tax_cents, o.paid_cents, o.tracking, o.shop_note, o.cancelled_by,
                    o.cancel_why, o.created_at, o.paid_at, o.refunded_at,
                    (SELECT json_agg(json_build_object('proof', p.proof, 'note', p.note, 'answer', p.answer,
                         'answer_note', p.answer_note, 'sent', p.sent_at, 'answered', p.answered_at) ORDER BY p.proof)
                       FROM mf_shop_proofs p WHERE p.order_id = o.id) AS proofs
             FROM mf_shop_orders o LEFT JOIN mf_shops s ON s.party = o.shop_party
             WHERE o.buyer_account = $1 ORDER BY o.id", a).await?,
        "purchases": rows(pool,
            "SELECT id, design_id, title, price_cents, status, created_at, paid_at, refunded_at
             FROM mf_orders WHERE buyer_account = $1 ORDER BY id", a).await?,
        "sales": rows(pool,
            "SELECT id, design_id, title, price_cents, fee_cents, processing_cents, seller_cents, status, paid_at, refunded_at
             FROM mf_orders WHERE seller_account = $1 AND status IN ('paid', 'refunded', 'disputed', 'chargeback') ORDER BY id", a).await?,
        "selling": rows(pool, "SELECT stripe_account, charges_enabled, payouts_enabled, created_at FROM mf_sellers WHERE account_id = $1", a).await?,
        "agreements": rows(pool, "SELECT document, version, at FROM mf_agreements WHERE account_id = $1 ORDER BY at", a).await?,
        "activity_seen": rows(pool, "SELECT seen_at FROM mf_activity_seen WHERE account_id = $1", a).await?,
        "proofs": rows(pool,
            "SELECT p.id, p.title, p.shop_name, p.customer, p.message, p.status, p.created_at, p.active_at,
                    (SELECT json_agg(json_build_object('number', v.number, 'file_name', v.file_name, 'format', v.format, 'note', v.note, 'sent', v.created_at) ORDER BY v.number)
                       FROM mf_proof_versions v WHERE v.proof_id = p.id) AS versions,
                    (SELECT json_agg(json_build_object('version', r.version, 'by_shop', r.by_shop, 'name', r.name, 'body', r.body, 'decision', r.decision, 'at', r.at) ORDER BY r.at)
                       FROM mf_proof_replies r WHERE r.proof_id = p.id) AS replies
             FROM mf_proofs p WHERE p.account_id = $1 ORDER BY p.id", a).await?,
        "founding_application": rows(pool, "SELECT name, work_url, about, applied_at, status, number FROM mf_founding WHERE account_id = $1", a).await?,
        "reports_sent": rows(pool, "SELECT kind, reason, details, created_at, status FROM mf_reports WHERE reporter_account = $1 ORDER BY id", a).await?,
    }))
}

pub async fn download(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.as_ref()) else {
        return Redirect::to("/signin?next=/account/data").into_response();
    };
    match export(pool, &user).await {
        Ok(v) => (
            [
                (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"makerfoundry-data.json\"",
                ),
                (header::CACHE_CONTROL, "private, no-store"),
            ],
            serde_json::to_string_pretty(&v).unwrap_or_default(),
        )
            .into_response(),
        Err(e) => {
            eprintln!("data: export {}: {e}", user.account_id);
            private(
                "Your data · MakerFoundry",
                &page_html("The copy couldn't be made just now. Please try again."),
                true,
            )
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DeleteForm {
    confirm: String,
}

pub async fn delete(State(state): State<AppState>, headers: HeaderMap, Form(f): Form<DeleteForm>) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (Some(user), Some(pool)) = (current_user(&state, &headers).await, state.db.as_ref()) else {
        return Redirect::to("/signin?next=/account/data").into_response();
    };
    if !f.confirm.trim().eq_ignore_ascii_case("delete") {
        return private("Your data · MakerFoundry", &page_html("Type delete to confirm."), true);
    }
    match orders_in_progress(pool, user.account_id).await {
        Ok(0) => {}
        Ok(_) => return private("Your data · MakerFoundry", &page_html(ORDERS_IN_PROGRESS), true),
        Err(e) => {
            eprintln!("data: delete {}: {e}", user.account_id);
            return private(
                "Your data · MakerFoundry",
                &page_html("Your data couldn't be deleted just now. Nothing was changed. Please try again."),
                true,
            );
        }
    }
    // (a checkout still open for one of the designs is closed first: nobody pays for a design
    // that's gone)
    if let Err(why) = crate::selling::close_account_checkouts(&state, pool, user.account_id).await {
        return private("Your data · MakerFoundry", &page_html(why), true);
    }
    let keys = match erase(pool, user.account_id).await {
        Ok(keys) => keys,
        Err(e) => {
            eprintln!("data: delete {}: {e}", user.account_id);
            return private(
                "Your data · MakerFoundry",
                &page_html("Your data couldn't be deleted just now. Nothing was changed. Please try again."),
                true,
            );
        }
    };
    if let Some(storage) = &state.storage {
        for key in keys {
            // (the records are gone either way; a file left behind costs a little storage)
            if let Err(e) = storage.delete(&key).await {
                eprintln!("data: delete a file of {}: {e}", user.account_id);
            }
        }
    }
    let mut resp = private(
        "Deleted · MakerFoundry",
        r#"<section class="hero"><h1>Your MakerFoundry data is deleted</h1><p class="lead">You're signed out. Your Foundry Account itself is kept by StitchFoundry; you can still sign in with it, or close it there.</p></section>"#,
        false,
    );
    if let Ok(v) =
        header::HeaderValue::from_str(&format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"))
    {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

/// How many of `account`'s orders from shops are still with a shop or have a refund on its way:
/// while any are, the data isn't deleted (the shop still needs the address and art, and the
/// refund its order). A disputed order counts: won, it goes back to the shop (issue #150).
pub async fn orders_in_progress(pool: &PgPool, account: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM mf_shop_orders WHERE buyer_account = $1
           AND (status IN ('paid', 'sent', 'received', 'proof_sent', 'approved', 'in_production', 'ready',
                           'disputed')
                OR (refund_due AND fee_returned_at IS NULL))",
    )
    .bind(account)
    .fetch_one(pool)
    .await
}

/// What's said when orders from shops are still in progress.
pub const ORDERS_IN_PROGRESS: &str = "An order from a shop is still in progress (or its refund is on its way), and the shop needs your address and art to finish it. Your data can be deleted once it's delivered, collected or cancelled.";

/// Deletes `account`'s data in one transaction → the storage keys of the files it had.
pub async fn erase(pool: &PgPool, account: i64) -> Result<Vec<String>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // designs nobody has bought go, with everything on them; bought ones are unpublished and kept
    let gone: Vec<i64> = sqlx::query_scalar(
        "SELECT d.id FROM mf_designs d WHERE d.account_id = $1
           AND NOT EXISTS (SELECT 1 FROM mf_orders o WHERE o.design_id = d.id AND o.status IN ('paid', 'refunded', 'disputed', 'chargeback'))",
    )
    .bind(account)
    .fetch_all(&mut *tx)
    .await?;
    let mut keys: Vec<String> = sqlx::query_scalar(
        "SELECT file_key FROM mf_designs WHERE id = ANY($1)
         UNION ALL SELECT file_key FROM mf_design_files WHERE design_id = ANY($1)
         UNION ALL SELECT zip_key FROM mf_designs WHERE id = ANY($1) AND zip_key IS NOT NULL
         UNION ALL SELECT file_key FROM mf_model_files WHERE design_id = ANY($1)
         UNION ALL SELECT thumb_key FROM mf_model_files WHERE design_id = ANY($1) AND thumb_key IS NOT NULL
         UNION ALL SELECT unnest(view_keys) FROM mf_model_files WHERE design_id = ANY($1)
         UNION ALL SELECT mesh_key FROM mf_model_files WHERE design_id = ANY($1) AND mesh_key IS NOT NULL
         UNION ALL SELECT photo_key FROM mf_design_photos WHERE design_id = ANY($1)
         UNION ALL SELECT thumb_key FROM mf_design_photos WHERE design_id = ANY($1)
         UNION ALL SELECT photo_key FROM mf_makes WHERE design_id = ANY($1) OR account_id = $2
         UNION ALL SELECT thumb_key FROM mf_makes WHERE design_id = ANY($1) OR account_id = $2
         UNION ALL SELECT f.file_key FROM mf_import_files f JOIN mf_imports i ON i.id = f.import_id
                   WHERE i.account_id = $2 AND f.file_key IS NOT NULL",
    )
    .bind(&gone)
    .bind(account)
    .fetch_all(&mut *tx)
    .await?;
    // proofs go whole (MF-25)
    let proofs: Vec<i64> = sqlx::query_scalar("SELECT id FROM mf_proofs WHERE account_id = $1")
        .bind(account)
        .fetch_all(&mut *tx)
        .await?;
    keys.extend(crate::proofs::erase(&mut tx, &proofs).await?);
    // orders from shops (MF-84): the art, mock-ups and the customer's details go; unpaid orders go
    // whole; paid ones keep their amounts (the ledger)
    let arts: Vec<Value> = sqlx::query_scalar("SELECT art FROM mf_shop_orders WHERE buyer_account = $1")
        .bind(account)
        .fetch_all(&mut *tx)
        .await?;
    keys.extend(crate::shop_orders::after::art_keys(arts.iter()));
    keys.extend(
        sqlx::query_scalar::<_, String>(
            "DELETE FROM mf_shop_proofs WHERE order_id IN (SELECT id FROM mf_shop_orders WHERE buyer_account = $1)
             RETURNING image_key",
        )
        .bind(account)
        .fetch_all(&mut *tx)
        .await?,
    );
    keys.sort();
    keys.dedup();
    for sql in [
        "UPDATE mf_orders SET design_id = NULL WHERE design_id = ANY($1) AND status IN ('pending', 'expired')",
        "DELETE FROM mf_designs WHERE id = ANY($1)",
    ] {
        sqlx::query(sql).bind(&gone).execute(&mut *tx).await?;
    }
    for sql in [
        "UPDATE mf_designs SET status = 'draft' WHERE account_id = $1 AND status = 'published'",
        "DELETE FROM mf_makes WHERE account_id = $1",
        "DELETE FROM mf_comments WHERE account_id = $1",
        "DELETE FROM mf_collections WHERE account_id = $1",
        "DELETE FROM mf_follows WHERE follower_account = $1 OR maker_account = $1",
        "DELETE FROM mf_makers WHERE account_id = $1",
        "DELETE FROM mf_sessions WHERE account_id = $1",
        "DELETE FROM mf_activity_seen WHERE account_id = $1",
        "DELETE FROM mf_founding WHERE account_id = $1",
        "DELETE FROM mf_likes WHERE account_id = $1",
        "DELETE FROM mf_api_keys WHERE account_id = $1",
        "DELETE FROM mf_contest_votes WHERE account_id = $1",
        "DELETE FROM mf_contest_entries WHERE account_id = $1",
        "DELETE FROM mf_notices WHERE account_id = $1",
        "DELETE FROM mf_notice_prefs WHERE account_id = $1",
        "UPDATE mf_notices SET actor_account = NULL WHERE actor_account = $1",
        "UPDATE mf_showcase_consents SET withdrawn_at = now() WHERE account_id = $1 AND withdrawn_at IS NULL",
        "UPDATE mf_designs SET import_id = NULL WHERE import_id IN (SELECT id FROM mf_imports WHERE account_id = $1)",
        "DELETE FROM mf_imports WHERE account_id = $1",
        // (the copies of finished orders' messages kept for the hub: their contents blanked)
        "UPDATE mf_hub_outbox SET envelope = jsonb_set(envelope, '{body}', '{}'::jsonb)
         WHERE (sent_at IS NOT NULL OR failed IS NOT NULL)
           AND order_ref IN (SELECT ref FROM mf_shop_orders WHERE buyer_account = $1)",
        "DELETE FROM mf_hub_held WHERE order_ref IN (SELECT ref FROM mf_shop_orders WHERE buyer_account = $1)",
        // (an unpaid order goes; a payment that still comes for it is refunded)
        "DELETE FROM mf_shop_orders WHERE buyer_account = $1 AND status IN ('pending', 'abandoned')",
        "UPDATE mf_shop_orders SET customer_name = '', ship_to = NULL, note = '', cancel_why = '', art = '[]'::jsonb
         WHERE buyer_account = $1",
        "DELETE FROM mf_shops WHERE account_id = $1",
        "UPDATE mf_accounts SET email = '' WHERE account_id = $1",
    ] {
        sqlx::query(sql).bind(account).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(keys)
}

// ---------------------------------------------------------------- an admin deleting an account's data (#55)

#[derive(Deserialize, Default)]
pub struct AccountQuery {
    #[serde(default)]
    account: String,
}

#[derive(Deserialize)]
pub struct AdminDeleteForm {
    #[serde(default)]
    confirm: String,
}

/// What `account` has: (email, designs, published designs), or `None` if there's no such account.
async fn account_summary(pool: &PgPool, account: i64) -> Result<Option<(String, i64, i64)>, sqlx::Error> {
    let email: Option<String> = sqlx::query_scalar("SELECT email FROM mf_accounts WHERE account_id = $1")
        .bind(account)
        .fetch_optional(pool)
        .await?;
    let Some(email) = email else {
        return Ok(None);
    };
    let (all, published): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE status = 'published') FROM mf_designs WHERE account_id = $1",
    )
    .bind(account)
    .fetch_one(pool)
    .await?;
    Ok(Some((email, all, published)))
}

fn admin_accounts_html(lookup: &str, found: &str) -> String {
    format!(
        r#"<section class="form prose">
<h1>Delete an account's data</h1>
<p class="muted"><a href="/admin">Reports</a> · <a href="/admin/overview">Overview</a></p>
<p>For someone who can't sign in to use <strong>Your data › Delete</strong> themselves (a deletion request to support, or a test account). It does exactly what their own delete does: the maker profile, designs with their files and photos, makes, comments, collections, follows and sign-ins go; purchases, sales and their amounts, agreements, reports and copyright notices are kept. It's recorded in the moderation log. It can't be undone.</p>
<form method="get" action="/admin/accounts">
<label>Account number <input name="account" inputmode="numeric" value="{lookup}" required maxlength="19"></label>
<button type="submit" class="secondary">Look it up</button>
</form>
{found}
</section>"#,
        lookup = escape(lookup),
    )
}

/// `GET /admin/accounts?account=<n>` (admins only): an account's summary, and the form to delete
/// its data.
pub async fn admin_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<AccountQuery>,
) -> Response {
    let (admin, pool) = match crate::moderation::admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let lookup = q.account.trim();
    let found = if lookup.is_empty() {
        String::new()
    } else {
        match lookup.parse::<i64>() {
            Err(_) => r#"<p class="problem" role="alert">An account number is digits only.</p>"#.to_string(),
            Ok(account) => found_html(&state, &pool, admin.account_id, account, "").await,
        }
    };
    private(
        "Delete an account's data · MakerFoundry",
        &admin_accounts_html(lookup, &found),
        true,
    )
}

/// The summary of `account` and its delete form, or why it can't be deleted.
async fn found_html(state: &AppState, pool: &PgPool, me: i64, account: i64, problem: &str) -> String {
    if account == me || state.admins.contains(&account) {
        return r#"<p class="problem" role="alert">That's an admin's account: admins' accounts aren't deleted from here.</p>"#
            .to_string();
    }
    match account_summary(pool, account).await {
        Err(e) => {
            eprintln!("data: admin look-up {account}: {e}");
            r#"<p class="problem" role="alert">That couldn't be looked up just now. Please try again.</p>"#.to_string()
        }
        Ok(None) => r#"<p class="problem" role="alert">There's no account with that number.</p>"#.to_string(),
        Ok(Some((email, all, published))) => {
            let problem = if problem.is_empty() {
                String::new()
            } else {
                format!(r#"<p class="problem" role="alert">{}</p>"#, escape(problem))
            };
            let email = if email.is_empty() {
                "its data was already deleted".to_string()
            } else {
                email
            };
            format!(
                r#"<h2>Account {account}</h2>
<dl class="facts"><div><dt>Email</dt><dd>{email}</dd></div><div><dt>Designs</dt><dd>{all} ({published} published)</dd></div></dl>
<form method="post" action="/admin/accounts/{account}/delete">
<label>To delete this account's data, type its number <input name="confirm" inputmode="numeric" required maxlength="19" autocomplete="off"></label>
{problem}
<button type="submit">Delete account {account}'s data</button>
</form>"#,
                email = escape(&email),
            )
        }
    }
}

/// `POST /admin/accounts/<n>/delete` (admins only, typed confirmation): deletes the account's
/// data as its own delete does, and records it in the moderation log.
pub async fn admin_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(account): axum::extract::Path<i64>,
    Form(f): Form<AdminDeleteForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (admin, pool) = match crate::moderation::admin(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let title = "Delete an account's data · MakerFoundry";
    let lookup = account.to_string();
    if account == admin.account_id || state.admins.contains(&account) {
        let found = found_html(&state, &pool, admin.account_id, account, "").await;
        return private(title, &admin_accounts_html(&lookup, &found), true);
    }
    if f.confirm.trim() != lookup {
        let found = found_html(
            &state,
            &pool,
            admin.account_id,
            account,
            "Type the account's number to confirm.",
        )
        .await;
        return private(title, &admin_accounts_html(&lookup, &found), true);
    }
    match orders_in_progress(&pool, account).await {
        Ok(0) => {}
        Ok(_) => {
            let found = found_html(&state, &pool, admin.account_id, account, ORDERS_IN_PROGRESS).await;
            return private(title, &admin_accounts_html(&lookup, &found), true);
        }
        Err(e) => {
            eprintln!("data: admin delete {account}: {e}");
            return private(
                title,
                &admin_accounts_html(
                    &lookup,
                    r#"<p class="problem" role="alert">That couldn't be done just now. Nothing was changed. Please try again.</p>"#,
                ),
                true,
            );
        }
    }
    match account_summary(&pool, account).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            let found = found_html(&state, &pool, admin.account_id, account, "").await;
            return private(title, &admin_accounts_html(&lookup, &found), true);
        }
        Err(e) => {
            eprintln!("data: admin delete {account}: {e}");
            return private(
                title,
                &admin_accounts_html(
                    &lookup,
                    r#"<p class="problem" role="alert">That couldn't be done just now. Nothing was changed. Please try again.</p>"#,
                ),
                true,
            );
        }
    }
    if let Err(why) = crate::selling::close_account_checkouts(&state, &pool, account).await {
        let found = found_html(&state, &pool, admin.account_id, account, why).await;
        return private(title, &admin_accounts_html(&lookup, &found), true);
    }
    let keys = match erase(&pool, account).await {
        Ok(keys) => keys,
        Err(e) => {
            eprintln!("data: admin delete {account}: {e}");
            return private(
                title,
                &admin_accounts_html(
                    &lookup,
                    r#"<p class="problem" role="alert">That couldn't be done just now. Nothing was changed. Please try again.</p>"#,
                ),
                true,
            );
        }
    };
    let files = keys.len();
    if let Some(storage) = &state.storage {
        for key in keys {
            if let Err(e) = storage.delete(&key).await {
                eprintln!("data: admin delete a file of {account}: {e}");
            }
        }
    }
    let logged = sqlx::query(
        "INSERT INTO mf_moderation_log (admin_account, action, note) VALUES ($1, 'delete-account-data', $2)",
    )
    .bind(admin.account_id)
    .bind(format!("account {account}, {files} files"))
    .execute(&pool)
    .await;
    if let Err(e) = logged {
        eprintln!("data: log admin delete {account}: {e}");
    }
    private(
        title,
        &admin_accounts_html(
            "",
            &format!(
                r#"<p class="lead" role="status">Account {account}'s data is deleted ({files} files). Its Foundry Account itself is StitchFoundry's.</p>"#
            ),
        ),
        true,
    )
}
