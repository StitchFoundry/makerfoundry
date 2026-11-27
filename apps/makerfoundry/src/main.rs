//! Runs MakerFoundry's web service. Settings come from the environment (docs/deploy.md):
//! `PORT` (default 8080), which Railway sets; `DATABASE_URL`, MakerFoundry's Postgres (without it
//! nobody can sign in); `STITCHFOUNDRY_CLOUD_URL`, the account service (default
//! `https://cloud.stitchfoundry.app`).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::ExitCode;

const DEFAULT_PORT: u16 = 8080;

#[tokio::main]
async fn main() -> ExitCode {
    let port = match std::env::var("PORT") {
        Ok(text) => match text.trim().parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                eprintln!("PORT must be a port number (1-65535), got {text:?}");
                return ExitCode::FAILURE;
            }
        },
        Err(_) => DEFAULT_PORT,
    };
    // `::` takes IPv4 too on Linux, and Railway's private network is IPv6. Hosts without IPv6
    // get IPv4 only.
    let ipv6 = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
    let ipv4 = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let (listener, addr) = match tokio::net::TcpListener::bind(ipv6).await {
        Ok(listener) => (listener, ipv6),
        Err(err6) => match tokio::net::TcpListener::bind(ipv4).await {
            Ok(listener) => (listener, ipv4),
            Err(err4) => {
                eprintln!("can't listen on {ipv6} ({err6}) or {ipv4} ({err4})");
                return ExitCode::FAILURE;
            }
        },
    };
    if let Some(c) = std::env::var("RAILWAY_GIT_COMMIT_SHA")
        .ok()
        .as_deref()
        .and_then(makerfoundry::short_commit)
    {
        let _ = makerfoundry::COMMIT.set(c);
    }
    let cloud_url =
        std::env::var("STITCHFOUNDRY_CLOUD_URL").unwrap_or_else(|_| makerfoundry::cloud::DEFAULT_URL.to_string());
    let cloud = match makerfoundry::cloud::Cloud::new(&cloud_url) {
        Ok(cloud) => cloud,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let db = match std::env::var("DATABASE_URL").ok().filter(|u| !u.trim().is_empty()) {
        Some(url) => match makerfoundry::db::connect(&url).await {
            Ok(pool) => Some(pool),
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            eprintln!("no DATABASE_URL: the site runs, but nobody can sign in");
            None
        }
    };
    // after a breach: everyone signs in again
    if let Some(v) = std::env::var("MF_END_SESSIONS_BEFORE")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        let Some(t) = makerfoundry::db::parse_time(&v) else {
            eprintln!("MF_END_SESSIONS_BEFORE is a UTC time like 2026-10-22T09:00:00Z");
            return ExitCode::FAILURE;
        };
        if let Some(pool) = &db {
            match makerfoundry::db::end_sessions_before(pool, t).await {
                Ok(n) => eprintln!("ended {n} sessions made before MF_END_SESSIONS_BEFORE"),
                Err(e) => eprintln!("sessions: {e}"),
            }
        }
    }
    eprintln!(
        "makerfoundry {} listening on {addr}; accounts at {cloud_url}",
        makerfoundry::VERSION
    );
    let storage = match makerfoundry::storage::Storage::from_env(|k| std::env::var(k).ok()) {
        Ok(storage) => storage,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    if storage.is_none() {
        eprintln!("no R2 settings: designs can be previewed but not kept");
    }
    let admins: Vec<i64> = std::env::var("MF_ADMINS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let stripe = match makerfoundry::stripe::Stripe::from_env(|k| std::env::var(k).ok()) {
        Ok(stripe) => stripe,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    match &stripe {
        None => eprintln!("no STRIPE_SECRET_KEY: every design is free"),
        Some(s) if !s.test_mode() => eprintln!("Stripe: live mode"),
        Some(_) => eprintln!("Stripe: test mode"),
    }
    let proofs = match makerfoundry::proofs::Proofs::from_env(|k| std::env::var(k).ok()) {
        Ok(proofs) => proofs,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    match &proofs {
        None => eprintln!("no MF_PROOF_KEY: approval links are off"),
        Some(p) if p.open => eprintln!("approval links: on"),
        Some(_) => eprintln!("approval links: admins only until MF_PROOFS=yes"),
    }
    // MF-6: open to everyone only once the legal pages are approved (no owner's markers left)
    let launch_asked = std::env::var("MF_LAUNCHED").is_ok_and(|v| v.trim() == "yes");
    let launched = launch_asked && makerfoundry::legal::ready();
    if launch_asked && !launched {
        for d in makerfoundry::legal::DOCS {
            if d.unfinished() > 0 {
                eprintln!(
                    "MF_LAUNCHED is set, but the {} still has {} of the owner's markers: staying closed",
                    d.title,
                    d.unfinished()
                );
            }
        }
    }
    let publishers = match (launched, std::env::var("MF_PUBLISHERS").ok()) {
        (true, None) => makerfoundry::listings::Publishers::Everyone,
        (_, setting) => makerfoundry::listings::Publishers::from_setting(setting.as_deref()),
    };
    // ordering from shops (#1, #2, #53): on only with the hub's address, MakerFoundry's service key
    // and its signing key; a missing or bad signing key keeps it off (the rest of the site runs).
    // The name it goes by on the hub: `MF_STITCHHUB_SERVICE` (a test deploy's own), else
    // service:makerfoundry; a bad one keeps ordering off.
    let service_ok = match std::env::var("MF_STITCHHUB_SERVICE")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        None => true,
        Some(name) => match makerfoundry::shop_orders::contract::set_me(name.trim()) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("stitchhub: {e}: ordering from shops stays off");
                false
            }
        },
    };
    let stitchhub = match makerfoundry::shop_orders::hub::Hub::new(
        &std::env::var("MF_STITCHHUB_URL").unwrap_or_default(),
        &std::env::var("MF_STITCHHUB_KEY").unwrap_or_default(),
    ) {
        None => None,
        Some(_) if !service_ok => None,
        Some(hub) => match std::env::var("MF_STITCHHUB_SIGNING_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
        {
            None => {
                eprintln!("stitchhub: MF_STITCHHUB_SIGNING_KEY isn't set: ordering from shops stays off");
                None
            }
            Some(key) => match makerfoundry::shop_orders::sign::Signer::from_base64(&key) {
                Ok(signer) => {
                    let hub = hub.with_signer(signer);
                    // (the public half, for the hub admin to register: not a secret)
                    eprintln!(
                        "stitchhub: on as {}; MakerFoundry's public signing key: {}",
                        makerfoundry::shop_orders::contract::me(),
                        hub.public_key().unwrap_or_default()
                    );
                    Some(hub)
                }
                Err(e) => {
                    eprintln!("stitchhub: {e}: ordering from shops stays off");
                    None
                }
            },
        },
    };
    if let Some(a) = makerfoundry::analytics::Analytics::from_env(|k| std::env::var(k).ok()) {
        makerfoundry::analytics::install(a);
    }
    let public_url = std::env::var("MF_PUBLIC_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| "https://makerfoundry.app".to_string());
    // staging: notices go to a test mailbox (MF_MAIL_CATCHER_URL), never through Resend
    let notices = match std::env::var("MF_MAIL_CATCHER_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    {
        Some(address) => match makerfoundry::notices::Notices::to_catcher(
            &std::env::var("MF_RESEND_KEY").unwrap_or_default(),
            std::env::var("MF_NOTICE_FROM").ok().as_deref(),
            &address,
        ) {
            Ok(n) => {
                eprintln!(
                    "notices: to the test mailbox at {}, never to Resend",
                    n.catcher().unwrap_or_default()
                );
                Some(n)
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        },
        None => makerfoundry::notices::Notices::new(
            &std::env::var("MF_RESEND_KEY").unwrap_or_default(),
            std::env::var("MF_NOTICE_FROM").ok().as_deref(),
            std::env::var("RESEND_API_URL").ok().as_deref(),
        ),
    };
    // staging only: relax every rate limit (ignored once MF_LAUNCHED=yes)
    let (multiplier, why) =
        makerfoundry::limits::multiplier_from(std::env::var("MF_RATE_LIMIT_MULTIPLIER").ok().as_deref(), launch_asked);
    if let Some(why) = why {
        eprintln!("{why}");
    }
    let limits = std::sync::Arc::new(makerfoundry::limits::Limits::with_multiplier(multiplier));
    let state = makerfoundry::AppState {
        cloud,
        db,
        storage,
        publishers,
        admins,
        limits,
        stripe,
        launched,
        public_url,
        api_origins: std::env::var("MF_API_ORIGINS")
            .unwrap_or_else(|_| makerfoundry::api::DEFAULT_ORIGINS.to_string())
            .split(',')
            .map(|o| o.trim().trim_end_matches('/').to_string())
            .filter(|o| !o.is_empty())
            .collect(),
        proofs,
        three_d: std::env::var("MF_3D").is_ok_and(|v| v.trim() == "yes"),
        thingiverse: makerfoundry::model3d::thingiverse::Thingiverse::new(
            &std::env::var("MF_THINGIVERSE_CLIENT_ID").unwrap_or_default(),
            &std::env::var("MF_THINGIVERSE_CLIENT_SECRET").unwrap_or_default(),
            std::env::var("THINGIVERSE_WWW_URL").ok().as_deref(),
            std::env::var("THINGIVERSE_API_URL").ok().as_deref(),
        ),
        social: makerfoundry::model3d::social::Social::new(
            &std::env::var("MF_FACEBOOK_PAGE_ID").unwrap_or_default(),
            &std::env::var("MF_FACEBOOK_PAGE_TOKEN").unwrap_or_default(),
            &std::env::var("MF_INSTAGRAM_USER_ID").unwrap_or_default(),
            std::env::var("META_GRAPH_URL").ok().as_deref(),
        ),
        notices,
        step: std::env::var("MF_STEP_CONVERTER").ok().and_then(|u| {
            makerfoundry::model3d::step::Converter::new(&u, std::env::var("MF_STEP_CONVERTER_KEY").ok().as_deref())
        }),
        scad: std::env::var("MF_SCAD_SERVICE").ok().and_then(|u| {
            makerfoundry::model3d::scad::Renderer::new(&u, std::env::var("MF_SCAD_SERVICE_KEY").ok().as_deref())
        }),
        stitchhub,
        points: std::env::var("MF_POINTS").is_ok_and(|v| v.trim() == "yes"),
        scanner: std::env::var("MF_CLAMAV")
            .ok()
            .and_then(|a| makerfoundry::scan::Scanner::new(&a)),
    };
    // every minute: one round with StitchHub, when it's switched on (MF-81)
    if state.stitchhub.is_some() {
        let hub_state = state.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                if let Err(e) = makerfoundry::shop_orders::sync(&hub_state).await {
                    eprintln!("stitchhub: {e}");
                }
            }
        });
    }
    // every minute, whether or not StitchHub is on, and never held up by it: orders from shops'
    // refunds due, stray payments, sales tax, unpaid orders and old files (MF-84)
    if state.db.is_some() {
        let sweep_state = state.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                if let Err(e) = makerfoundry::shop_orders::after::sweep(&sweep_state).await {
                    eprintln!("shop orders: {e}");
                }
            }
        });
    }
    // every 10 minutes: forget rate-limit entries over a day old and sessions past their 30 days
    // (what the privacy policy says is kept, and for how long)
    let dispute_state = state.clone();
    let (sweep_limits, sweep_db, sweep_stripe, sweep_storage, sweep_notices, sweep_url) = (
        state.limits.clone(),
        state.db.clone(),
        state.stripe.clone(),
        state.storage.clone(),
        state.notices.clone(),
        state.public_url.clone(),
    );
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(600));
        loop {
            every.tick().await;
            sweep_limits.sweep();
            if let Some(pool) = &sweep_db
                && let Err(e) = makerfoundry::db::forget_old_sessions(pool).await
            {
                eprintln!("sessions: {e}");
            }
            if let Some(pool) = &sweep_db
                && let Err(e) = makerfoundry::selling::expire_orders(pool).await
            {
                eprintln!("orders: {e}");
            }
            if let Some(pool) = &sweep_db
                && let Err(e) = makerfoundry::api::forget_old_links(pool).await
            {
                eprintln!("open links: {e}");
            }
            // MF-25: proofs a year after their last activity
            if let Some(pool) = &sweep_db
                && let Err(e) = makerfoundry::proofs::forget_old(pool, sweep_storage.as_ref()).await
            {
                eprintln!("proofs: {e}");
            }
            // MF-49: how popular 3D models are lately
            if let Some(pool) = &sweep_db
                && let Err(e) = makerfoundry::model3d::search::refresh_popularity(pool).await
            {
                eprintln!("3D popularity: {e}");
            }
            // MF-52: pending import files and unpublished imported drafts after 30 days
            if let Some(pool) = &sweep_db
                && let Err(e) = makerfoundry::model3d::import::forget_old(pool, sweep_storage.as_ref()).await
            {
                eprintln!("imports: {e}");
            }
            // issue #31: daily digests of email notices
            if let (Some(pool), Some(notices)) = (&sweep_db, &sweep_notices)
                && let Err(e) = makerfoundry::notices::send_digests(pool, notices, &sweep_url).await
            {
                eprintln!("notices: {e}");
            }
            if let (Some(pool), Some(stripe)) = (&sweep_db, &sweep_stripe)
                && let Err(e) = makerfoundry::selling::withhold_taxes(stripe, pool).await
            {
                eprintln!("sales tax: {e}");
            }
            // issue #90: disputes whose Stripe steps or emails aren't done yet
            if let Some(pool) = &sweep_db
                && let Err(e) = makerfoundry::disputes::settle_all(&dispute_state, pool).await
            {
                eprintln!("disputes: {e}");
            }
            // issue #91: refunds made outside the admin whose transfer reversal isn't checked yet
            if let Some(pool) = &sweep_db
                && let Err(e) = makerfoundry::refunds::check_all(&dispute_state, pool).await
            {
                eprintln!("refunds: {e}");
            }
        }
    });
    match axum::serve(listener, makerfoundry::app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("server stopped: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves on Ctrl-C or SIGTERM (what Railway sends on a redeploy), so requests in flight finish.
async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    eprintln!("shutting down");
}
