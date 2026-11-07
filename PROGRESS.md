# Progress

- 2026-11-07 nightly sweep: CI green (DB tests included), cargo audit clean; filed #105 (S1 double charge on a second Buy) and #106 (S2 shop-order Dashboard refunds).
- Fixed #105: Buy closes the buyer's earlier open checkout at Stripe and refuses while an earlier payment is still being confirmed.
- 2026-11-02 nightly maintenance: queue empty; `cargo xtask ci` green and stable over 3 test runs.
- Added money.rs edge-case tests (out-of-range prices, half-up rounding, i64 extremes, dollars round-trip); tests only.
- 2026-11-01 nightly maintenance: queue empty; CI and 3 test re-runs green, no flaky tests.
- Added edge-case tests for webhook signature headers (stripe.rs) and money bounds (money.rs); no behaviour change.
- 2026-11-05 #84: AGENTS.md (CLAUDE.md now imports it), PR template and label list added. `.claude` hooks + keep-going not done: the session's auto-mode classifier refused to write them (self-modification); needs Bruce to add or approve.
- 2026-11-07 #111 (first slice): search box and Upload button in every header, kind chips, home shelves of design cards (Trending, Newest, Free, From founding creators) with a founding invitation while there are fewer than 8 designs; cards show alt text, kind, founding badge, price.
- 2026-11-07 #109: founding applications email the admins (name, link, what they make; never the applicant's address); approve and decline email the applicant; admins see "Founding: N waiting" on their account page and a link from the overview.
- 2026-11-07 #110: `/browse` has kind tabs (All, Embroidery, Laser, 3D print once open) and shows only that kind's filters (hoop for embroidery, material for laser); other kinds' filters are ignored.
- 2026-11-07 #88 (part): home page now has a share card (og:title/description/url, twitter:card); design, maker and /founding pages already had one. Not done: og:image for home (needs an original asset), analytics events, /founding copy, footer links to sibling apps (shop ordering not on in production).
