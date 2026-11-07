# Progress

- 2026-11-02 nightly maintenance: queue empty; `cargo xtask ci` green and stable over 3 test runs.
- Added money.rs edge-case tests (out-of-range prices, half-up rounding, i64 extremes, dollars round-trip); tests only.
- 2026-11-01 nightly maintenance: queue empty; CI and 3 test re-runs green, no flaky tests.
- Added edge-case tests for webhook signature headers (stripe.rs) and money bounds (money.rs); no behaviour change.
- 2026-11-05 #84: AGENTS.md (CLAUDE.md now imports it), PR template and label list added. `.claude` hooks + keep-going not done: the session's auto-mode classifier refused to write them (self-modification); needs Bruce to add or approve.
- 2026-11-07 #88 (part): home page now has a share card (og:title/description/url, twitter:card); design, maker and /founding pages already had one. Not done: og:image for home (needs an original asset), analytics events, /founding copy, footer links to sibling apps (shop ordering not on in production).
