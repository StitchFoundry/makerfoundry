//! Privacy-friendly funnel events (issue #128). Off unless `POSTHOG_KEY` is set: with no key no
//! script is added to any page and the content security policy stays as it was.
//!
//! When on, our own `/static/analytics.js` (no library, no cookies, nothing stored in the browser)
//! sends a few named events to PostHog's capture endpoint. Anonymous visitors get a random id that
//! lives only in memory for that page, and no person profile is made. It does nothing when the
//! browser sends Do Not Track or Global Privacy Control. Only [`ALLOWED_PROPERTIES`] are ever sent;
//! never file names, design content, images or typed text. Purchases stay server-side.

use std::sync::OnceLock;

/// PostHog's US cloud, the default for `POSTHOG_HOST`.
pub const DEFAULT_HOST: &str = "https://us.i.posthog.com";

/// The only properties an event may carry (the shared list in the analytics plan).
pub const ALLOWED_PROPERTIES: &[&str] = &[
    "app",
    "app_version",
    "platform",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_content",
    "utm_term",
    "step",
    "format",
    "error_code",
    "duration_ms",
    "ok",
];

/// The event names this site sends.
pub const EVENTS: &[&str] = &[
    "landing_viewed",
    "project_started",
    "file_imported",
    "import_failed",
    "preview_viewed",
    "download_clicked",
    "error_shown",
];

/// Where events go: a project key (public by design) and a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Analytics {
    key: String,
    host: String,
}

impl Analytics {
    /// From `POSTHOG_KEY` and `POSTHOG_HOST`. `None` (analytics off) when the key is missing or
    /// the host isn't a plain `https://` origin.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let key = get("POSTHOG_KEY")?.trim().to_string();
        let host = get("POSTHOG_HOST")
            .map(|h| h.trim().trim_end_matches('/').to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_string());
        Self::new(&key, &host)
    }

    pub fn new(key: &str, host: &str) -> Option<Self> {
        let plain = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c));
        let rest = host.strip_prefix("https://")?;
        if key.is_empty() || key.len() > 200 || !plain(key) || rest.is_empty() || !plain(host) || rest.contains('/') {
            return None;
        }
        Some(Self {
            key: key.to_string(),
            host: host.to_string(),
        })
    }

    /// The script tag every page carries.
    pub fn script_tag(&self) -> String {
        format!(
            r#"<script src="/static/analytics.js" defer data-key="{}" data-host="{}"></script>"#,
            self.key, self.host
        )
    }

    /// `base` (the default policy) with our script and the capture host allowed.
    pub fn policy(&self, base: &str) -> String {
        format!("{base}; script-src 'self'; connect-src {}", self.host)
    }
}

static ACTIVE: OnceLock<Analytics> = OnceLock::new();

/// Turns analytics on for this process (called once at start-up).
pub fn install(a: Analytics) {
    let _ = ACTIVE.set(a);
}

/// The script tag for the page layout; empty while analytics is off.
pub fn page_script() -> String {
    ACTIVE.get().map(Analytics::script_tag).unwrap_or_default()
}

/// The content security policy to use for pages without their own.
pub fn page_policy(base: &str) -> String {
    ACTIVE.get().map(|a| a.policy(base)).unwrap_or_else(|| base.to_string())
}

/// Keeps only the allowed properties.
pub fn filter<'a>(props: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    props
        .iter()
        .copied()
        .filter(|(k, _)| ALLOWED_PROPERTIES.contains(k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn off_without_a_key() {
        assert_eq!(Analytics::from_env(env(&[])), None);
        assert_eq!(Analytics::from_env(env(&[("POSTHOG_KEY", "  ")])), None);
    }

    #[test]
    fn host_defaults_and_trailing_slash_goes() {
        let a = Analytics::from_env(env(&[("POSTHOG_KEY", "phc_abc")])).unwrap();
        assert!(a.script_tag().contains(r#"data-host="https://us.i.posthog.com""#));
        let b = Analytics::from_env(env(&[
            ("POSTHOG_KEY", "phc_abc"),
            ("POSTHOG_HOST", "https://eu.i.posthog.com/"),
        ]))
        .unwrap();
        assert!(b.script_tag().contains(r#"data-host="https://eu.i.posthog.com""#));
    }

    #[test]
    fn odd_settings_turn_it_off() {
        for (k, h) in [
            ("phc_a\"b", DEFAULT_HOST),
            ("phc_a", "http://x.test"),
            ("phc_a", "https://x.test/path"),
            ("phc_a", "https://x.test; script-src *"),
            ("phc_a", "javascript:alert(1)"),
        ] {
            assert_eq!(Analytics::new(k, h), None, "{k} {h}");
        }
    }

    #[test]
    fn policy_adds_only_our_script_and_the_host() {
        let a = Analytics::new("phc_a", DEFAULT_HOST).unwrap();
        let p = a.policy("default-src 'none'");
        assert_eq!(
            p,
            "default-src 'none'; script-src 'self'; connect-src https://us.i.posthog.com"
        );
    }

    #[test]
    fn only_allowed_properties_pass() {
        let kept = filter(&[
            ("step", "upload"),
            ("file_name", "secret.dst"),
            ("format", "dst"),
            ("title", "My design"),
            ("ok", "true"),
            ("email", "a@b.c"),
        ]);
        assert_eq!(kept, vec![("step", "upload"), ("format", "dst"), ("ok", "true")]);
    }

    /// The browser script keeps its own copy of the lists; they must match this one.
    #[test]
    fn script_lists_match_these() {
        let js = include_str!("../static/analytics.js");
        for p in ALLOWED_PROPERTIES {
            assert!(js.contains(&format!("'{p}'")), "script lacks property {p}");
        }
        for e in EVENTS {
            assert!(js.contains(&format!("'{e}'")), "script lacks event {e}");
        }
        assert!(js.contains("doNotTrack") && js.contains("globalPrivacyControl"));
        assert!(js.contains("$process_person_profile"));
        assert!(!js.contains("localStorage") && !js.contains("document.cookie") && !js.contains("sessionStorage"));
    }
}
