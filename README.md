# MakerFoundry

A marketplace where makers share and sell **machine-embroidery designs** and **laser-cut
files**: free designs shared openly, paid designs sold through MakerFoundry. By
[SiteSentrix](https://sitesentrix.com), the makers of [StitchFoundry](https://stitchfoundry.app).

## Status
October 2026: **MF-0** (workspace) done: a Rust web service with a home page and `/health`, CI,
and a Dockerfile, live on Railway at <https://makerfoundry-production.up.railway.app>
([`docs/deploy.md`](docs/deploy.md)). **MF-1** (accounts) done and live: sign up, in and out with a Foundry Account, and a
forgotten password. **MF-2** (upload and preview) under way: an
uploaded embroidery file is drawn stitch by stitch, and an SVG, DXF or PDF laser file line by line
(`/preview`, signed in); a maker keeps designs with photos as drafts (`/designs`) 
in Cloudflare R2. **MF-3** (listings) built: maker pages, design details and licences,
publishing, public design pages with free downloads, browse and search (Browse has a filter panel, counts and four orders; design and maker page layouts still to come); publishing opens at the
launch. **MF-4** (selling) built: sellers set up Stripe payouts (monthly), designs get a price,
buyers pay on Stripe Checkout and find their designs in a library; MakerFoundry keeps 10% and the
card fee, every sale and refund in a ledger; waiting for the Stripe test keys. **MF-5** (safety) built: reporting, the copyright (DMCA) process, an admin queue for
takedowns, and rate limits. **MF-6** (launch) ready: the Terms, Privacy Policy and Seller Terms
pages, agreement on sign-up, publishing and selling, a launch switch that waits for the owner's
approval of the legal wording ([`docs/launch.md`](docs/launch.md)). **MF-7** (apps) under way: the API the apps
publish to ([`docs/api.md`](docs/api.md)); the apps' own command is next. **MF-8** (the first version's missing pieces) done:
free/paid, hoop and material browsing, and several formats per design. **MF-9** done: embroidery designs download converted
to every format StitchFoundry writes. **MF-10** done: saved designs and collections. **MF-11** done: makes, photos of what
people made from a design. **MF-12** (reporting a make) and **MF-13** (following makers) done. **MF-14** done: comments on designs. **MF-15** done: download or delete your
data. **MF-16** (the admins' overview), **MF-17** and **MF-18** (Open in StitchFoundry for bought designs,
and MakerFoundry's side of Open in LaserFoundry) and **MF-19** (link previews) done. **MF-20** done: every
reader of uploads and typed input is property-tested with random and damaged files
(`apps/makerfoundry/tests/untrusted_input.rs`). **MF-21** done: signing everyone out after a breach. **MF-22** done: a maker's
Activity page (comments, makes, sales and new followers). **MF-23** done: on a refund the seller
bears only the card fee Stripe keeps (MakerFoundry's 10% goes back to them). **MF-24** built: sales tax with
Stripe Tax, kept by MakerFoundry as the marketplace, off until `MF_STRIPE_TAX=yes`. **MF-25** built:
approval links (a shop sends a customer a proof to approve), its wording in the Terms (§ 9) and the
Privacy Policy; switched off until the owner sets `MF_PROOF_KEY` and `MF_PROOFS=yes`. **MakerFoundry 3D** (MF-35 to MF-59, issues #3 to #36) built: STL, 3MF, OBJ and STEP models with
our own readers, rebuilt 3MFs, server-drawn pictures and a 3D viewer, print profiles, remixes with
licence rules, likes, search, bulk import, the opt-in showcase, founding creators, contests, a
public API; switched off for everyone but admins and founding creators until the owner approves
its wording ([`docs/legal/3d-draft.md`](docs/legal/3d-draft.md)) and sets `MF_3D=yes`. Its
outside services (Thingiverse import, virus scanning, social posting, email notices, STEP
previews, OpenSCAD customizing) and points are built and tested against stand-ins, each off until
the owner sets it up ([`docs/deploy.md`](docs/deploy.md)). See [`docs/plan.md`](docs/plan.md).

## Develop
- `cargo run -p makerfoundry`, then open <http://localhost:8080>.
- `cargo xtask ci`: the checks every commit must pass (fmt, clippy `-D warnings`, tests, assets).
- The database tests need a Postgres they may make schemas in:
  `MF_TEST_DATABASE_URL=postgres://user:password@localhost/db cargo xtask ci` (CI has one; without
  it those tests are skipped).
- Signing in locally needs `DATABASE_URL` and, for a test account service,
  `STITCHFOUNDRY_CLOUD_URL` (see `docs/deploy.md`).

## For contributors and agents
Start with [`CLAUDE.md`](CLAUDE.md) and [`docs/start-here.md`](docs/start-here.md).

## Licence
MIT OR Apache-2.0, at your choice ([`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE)).
Designs uploaded by users stay theirs, under the licence chosen for each listing.
