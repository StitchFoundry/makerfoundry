//! Rate limits (MF-5): how often one address or one account may do something, counted in memory
//! over a sliding window. One server runs the site, so memory is enough; a restart forgets.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

/// What's limited, how many times, in how long.
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub name: &'static str,
    pub max: usize,
    pub per: Duration,
}

impl Rule {
    /// The rule with its `max` multiplied (staging only, `MF_RATE_LIMIT_MULTIPLIER`): the one
    /// place a limit is relaxed. Saturates; a multiplier of 1 changes nothing.
    pub const fn scaled(self, multiplier: usize) -> Rule {
        Rule {
            max: self.max.saturating_mul(multiplier),
            ..self
        }
    }
}

const fn rule(name: &'static str, max: usize, minutes: u64) -> Rule {
    Rule {
        name,
        max,
        per: Duration::from_secs(minutes * 60),
    }
}

/// Accounts made from one address an hour.
pub const SIGNUP: Rule = rule("signup", 10, 60);
/// Sign-in attempts from one address in 15 minutes (the account service also limits each email).
pub const SIGNIN: Rule = rule("signin", 30, 15);
/// Password-reset emails asked for from one address an hour.
pub const FORGOT: Rule = rule("forgot", 10, 60);
/// Files previewed by one account an hour.
pub const PREVIEW: Rule = rule("preview", 60, 60);
/// Designs added by one account a day.
pub const UPLOAD: Rule = rule("upload", 50, 24 * 60);
/// Reports and notices from one address an hour.
pub const REPORT: Rule = rule("report", 10, 60);
/// Converted downloads (MF-9) by one address an hour: each is made on the spot.
pub const CONVERT: Rule = rule("convert", 120, 60);
/// Designs opened in StitchFoundry's web app (Open in StitchFoundry) from one address an hour.
pub const OPEN: Rule = rule("open", 120, 60);
/// Comments by one account an hour.
pub const COMMENT: Rule = rule("comment", 20, 60);
/// Purchases started (Stripe checkout pages made) by one account an hour.
pub const CHECKOUT: Rule = rule("checkout", 30, 60);
/// Proofs and new versions of them (MF-25) sent by one account a day.
pub const PROOF_UPLOAD: Rule = rule("proof-upload", 100, 24 * 60);
/// Comments, approvals and requests for changes on proofs from one address an hour (customers
/// have no account).
pub const PROOF_REPLY: Rule = rule("proof-reply", 30, 60);
/// Founding-creator applications (and changes to a waiting one) by one account an hour.
pub const FOUNDING: Rule = rule("founding", 10, 60);

/// 3D models (or new versions) added by one account a day (MF-41; the owner's decision,
/// 2026-10-30).
pub const MODEL_UPLOAD: Rule = rule("model-upload", 100, 24 * 60);

/// Bulk imports (MF-52) started by one account a day.
pub const IMPORT: Rule = rule("import", 20, 24 * 60);

/// Likes and unlikes by one account an hour (MF-48).
pub const LIKE: Rule = rule("like", 300, 60);

/// Requests one API key may make an hour to the public API for 3D models (issue #36).
pub const API_READ: Rule = rule("api-read", 1000, 60);

/// Customized models (issue #35) made by one account an hour: each runs OpenSCAD.
pub const SCAD_RENDER: Rule = rule("scad-render", 30, 60);

/// The most designs one account may keep.
pub const MAX_DESIGNS: i64 = 500;

/// Every rule above (a test checks none is missing).
pub const ALL: [Rule; 18] = [
    SIGNUP,
    SIGNIN,
    FORGOT,
    PREVIEW,
    UPLOAD,
    REPORT,
    CONVERT,
    OPEN,
    COMMENT,
    CHECKOUT,
    PROOF_UPLOAD,
    PROOF_REPLY,
    FOUNDING,
    MODEL_UPLOAD,
    IMPORT,
    LIKE,
    API_READ,
    SCAD_RENDER,
];

/// Reads `MF_RATE_LIMIT_MULTIPLIER`: a whole number from 1 to 100, else 1. Ignored (1) once
/// `launched` (`MF_LAUNCHED=yes`), so production can't be relaxed. The text is the log line to
/// print, if any.
pub fn multiplier_from(setting: Option<&str>, launched: bool) -> (usize, Option<String>) {
    let Some(raw) = setting.map(str::trim).filter(|v| !v.is_empty()) else {
        return (1, None);
    };
    if launched {
        return (
            1,
            Some("MF_RATE_LIMIT_MULTIPLIER is ignored once MF_LAUNCHED=yes: limits stay as they are".into()),
        );
    }
    match raw.parse::<usize>() {
        Ok(n) if (1..=100).contains(&n) => (
            n,
            (n > 1).then(|| format!("rate limits relaxed {n} times (MF_RATE_LIMIT_MULTIPLIER; staging only)")),
        ),
        _ => (
            1,
            Some("MF_RATE_LIMIT_MULTIPLIER must be a whole number from 1 to 100: using 1".into()),
        ),
    }
}

pub struct Limits {
    hits: Mutex<HashMap<(&'static str, String), VecDeque<Instant>>>,
    multiplier: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self::with_multiplier(1)
    }
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Limits")
    }
}

impl Limits {
    /// Limits whose every rule allows `multiplier` times as many (at least 1).
    pub fn with_multiplier(multiplier: usize) -> Limits {
        Limits {
            hits: Mutex::default(),
            multiplier: multiplier.max(1),
        }
    }

    /// Counts one more for `key` under `rule`: `true` if it's allowed, `false` if over the limit
    /// (and then it isn't counted).
    pub fn allow(&self, rule: Rule, key: &str) -> bool {
        self.allow_at(rule, key, Instant::now())
    }

    /// Forgets every key not used within the longest window (a day): what the privacy policy
    /// promises about network addresses. Run every few minutes.
    pub fn sweep(&self) {
        self.sweep_at(Instant::now());
    }

    fn sweep_at(&self, now: Instant) {
        let mut hits = self.hits.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        hits.retain(|_, times| times.back().is_some_and(|t| now.duration_since(*t) < UPLOAD.per));
    }

    fn allow_at(&self, rule: Rule, key: &str, now: Instant) -> bool {
        let rule = rule.scaled(self.multiplier);
        // (a panic elsewhere while holding the lock leaves only counts behind: carry on)
        let mut hits = self.hits.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if hits.len() > 100_000 {
            hits.retain(|(name, _), times| {
                let per = match *name {
                    "upload" | "model-upload" | "import" => UPLOAD.per,
                    _ => Duration::from_secs(3600),
                };
                times.back().is_some_and(|t| now.duration_since(*t) < per)
            });
        }
        let times = hits.entry((rule.name, key.to_string())).or_default();
        while times.front().is_some_and(|t| now.duration_since(*t) >= rule.per) {
            times.pop_front();
        }
        if times.len() >= rule.max {
            return false;
        }
        times.push_back(now);
        true
    }
}

/// The address a request came from: Railway's proxy gives it in `X-Real-IP`, or first in
/// `X-Forwarded-For`; "unknown" without either (local runs, tests).
pub fn client(headers: &HeaderMap) -> String {
    let real = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    real.or(forwarded).unwrap_or("unknown").chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_holds_for_its_window_then_lets_go() {
        let limits = Limits::default();
        let r = Rule {
            name: "t",
            max: 3,
            per: Duration::from_secs(60),
        };
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(limits.allow_at(r, "a", t0 + Duration::from_secs(i)));
        }
        assert!(
            !limits.allow_at(r, "a", t0 + Duration::from_secs(10)),
            "a fourth within the minute"
        );
        assert!(
            limits.allow_at(r, "b", t0 + Duration::from_secs(10)),
            "another key has its own count"
        );
        assert!(
            limits.allow_at(r, "a", t0 + Duration::from_secs(61)),
            "the first has left the window"
        );
        // a day later, nothing of either address is kept
        limits.sweep_at(t0 + Duration::from_secs(61) + UPLOAD.per);
        assert!(limits.hits.lock().unwrap().is_empty());
    }

    fn signups(limits: &Limits, n: u64) -> Vec<bool> {
        let t0 = Instant::now();
        (0..n)
            .map(|i| limits.allow_at(SIGNUP, "a", t0 + Duration::from_secs(i)))
            .collect()
    }

    #[test]
    fn by_default_the_eleventh_signup_an_hour_is_refused() {
        let got = signups(&Limits::default(), 11);
        assert!(got[..10].iter().all(|ok| *ok));
        assert!(!got[10]);
    }

    #[test]
    fn a_multiplier_of_five_allows_fifty_signups() {
        let got = signups(&Limits::with_multiplier(5), 51);
        assert!(got[..50].iter().all(|ok| *ok), "the 11th to 50th are allowed");
        assert!(!got[50], "the 51st is refused");
    }

    #[test]
    fn the_setting_is_read_safely() {
        assert_eq!(multiplier_from(None, false), (1, None));
        assert_eq!(multiplier_from(Some("  "), false), (1, None));
        assert_eq!(multiplier_from(Some("20"), false).0, 20);
        assert_eq!(multiplier_from(Some("100"), false).0, 100);
        for bad in ["0", "101", "-3", "2.5", "many", "99999999999999999999999"] {
            let (n, why) = multiplier_from(Some(bad), false);
            assert_eq!(n, 1, "{bad}");
            assert!(why.is_some(), "{bad} is logged");
        }
        let (n, why) = multiplier_from(Some("20"), true);
        assert_eq!(n, 1, "ignored once launched");
        assert!(why.is_some());
    }

    #[test]
    fn the_multiplier_reaches_every_rule() {
        let source = include_str!("limits.rs");
        let rules = source
            .lines()
            .filter(|l| l.starts_with("pub const ") && l.contains(": Rule ="))
            .count();
        assert_eq!(ALL.len(), rules, "ALL lists every rule");
        for r in ALL {
            assert_eq!(r.scaled(1).max, r.max, "{}", r.name);
            assert_eq!(r.scaled(7).max, r.max * 7, "{}", r.name);
            assert_eq!(r.scaled(7).per, r.per, "{}", r.name);
        }
        assert_eq!(rule("x", usize::MAX, 1).scaled(100).max, usize::MAX);
    }

    #[test]
    fn the_client_address_comes_from_the_proxy() {
        let mut h = HeaderMap::new();
        assert_eq!(client(&h), "unknown");
        h.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        assert_eq!(client(&h), "203.0.113.9");
        h.insert("x-real-ip", "198.51.100.7".parse().unwrap());
        assert_eq!(client(&h), "198.51.100.7");
    }
}
