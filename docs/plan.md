# MakerFoundry: the plan

**Status:** 2026-10-29. MF-0 to MF-25, MF-34 and MF-54 built at https://makerfoundry.app (the table below);
the legal pages are approved (2026-10-22) for a free-only soft open (`docs/launch.md`: the contact
email, admins, then `MF_LAUNCHED=yes`; the DMCA agent the week after; Stripe keys and sales tax
before paid sales). MF-7's app side is StitchFoundry's
(`docs/api.md`). Next after the soft open: the Preview & Approval Hub, MF-25 to MF-33 (planned
2026-10-28, starting with approval links), and the 3D printing hub, MF-35 to MF-59 (planned
2026-10-30, `docs/3d-hub-plan.md`). Later, each needing the owner's decision: bundles,
discount codes, pay-what-you-want. Owner: Bruce Wattendorf (SiteSentrix).

## What it is
A place, like Thingiverse for 3D prints, where makers find, share and sell files for their
machines:

- **Machine embroidery designs**: DST, PES, PEC, JEF, EXP, VP3, XXX, HUS, U01 and more.
- **Laser-cut and engrave files**: SVG, DXF, PDF, and LightBurn-friendly exports.
- **Free designs** shared openly, and **paid designs** sold through MakerFoundry, which takes a
  small fee on each sale (the owner's income from the marketplace).

Who it is for: home embroiderers and small embroidery shops (StitchFoundry's users), laser hobby
users and small laser businesses (LightFoundry's), and the designers who sell to both.

## How a sale works (first version)
1. A seller connects a **Stripe Connect** account (Express): Stripe checks who they are and pays
   them out; MakerFoundry never holds their bank details.
2. A buyer pays on Stripe Checkout. MakerFoundry keeps its **platform fee** (an application fee
   on the payment); the rest goes to the seller. Stripe's own card fee comes out too.
3. Once Stripe confirms the payment (signed webhook), the buyer's library gets the files:
   download links that last, re-downloadable any time.
4. Refunds and disputes follow a written policy (digital goods: refund when a file is broken or
   isn't what the listing said).

## The first version (MVP)
- Accounts: the StitchFoundry cloud service's accounts, called a **Foundry Account** (owner's
  decisions, 2026-10-19): one email and password, and one credit balance, for every SiteSentrix
  app. See "Accounts (MF-1)" below.
- **Upload a design**: one or more files plus photos of the stitched or cut result, a title,
  description, tags, machine/hoop size or material and size, and a licence.
- **Previews made by the site**: embroidery files drawn as stitches (from StitchFoundry's own
  open-source file readers, used as a Rust dependency), with stitch count, size, colours and
  colour changes; laser files drawn from their paths, with their size. Uploaded files are checked
  as they're read: a file that doesn't read is refused.
- **Browse and search**: by kind (embroidery / laser), tags, hoop size, newest, popular, free/paid. **Layout (#115, first slice 2026-11-07)**: filters left (folding on a phone), result count, orders Trending / Newest / Most downloaded / Most saved, Load more; design page, maker page, likes and a 7-day Trending are still to do.
- **A design page**: previews, photos, details, the licence in plain words, download or buy.
- **Seller pages**: a maker's designs; followers later.
- **Paid designs** with Stripe Connect; a buyer's library; a seller's sales and payouts page.
- **Report a listing** and a DMCA takedown page; an admin queue to act on them.
- Terms of Service, Privacy Policy and Seller Terms pages (drafted like StitchFoundry's).

## Later
- "Open in StitchFoundry" on embroidery designs and "Publish to MakerFoundry" from the app;
  the same for LightFoundry.
- The Foundry Account across LightFoundry and PatternFoundry too.
- Collections, likes, comments, makes (photos of others' sew-outs and cuts), seller follows.
- Bundles, discount codes, "pay what you want".
- Format conversion on download (buy a PES, also get a DST), using StitchFoundry's writers.
- PatternFoundry (sewing patterns) may share the marketplace engine.

## Accounts (MF-1)
The owner chose StitchFoundry's cloud accounts (cloud.stitchfoundry.app) over separate ones.
How it works, given what that service has today (`apps/stitchfoundry-cloud`, `docs/cloud.md`):

- The cloud service owns accounts: emails, Argon2id password hashes, sessions, lock-outs. It keeps
  them in one record owned by its single process, so MakerFoundry **never touches its database**;
  it goes through its API only.
- **Sign up / sign in** on MakerFoundry's own pages: the form posts to MakerFoundry, which calls
  the cloud's `POST /v1/signup` or `/v1/signin` over HTTPS and gets the account's session token.
  The password passes through MakerFoundry's server but is never stored or logged.
- The cloud token can spend the account's AI credits, so it **never goes to the browser**.
  MakerFoundry keeps it server-side in its own Postgres, under a MakerFoundry session id that the
  browser holds in a `__Host-` cookie (HttpOnly, Secure, SameSite=Lax); form posts also check the
  `Origin` header. Signing out ends both sessions (`POST /v1/signout`).
- MakerFoundry's own tables (profile, shop name, later listings and sales) are keyed by the cloud
  account's **id**.
- **Changes needed in StitchFoundry's cloud service first:**
  1. `/v1/account` (and sign-in/up) also return the account's `id`.
  2. **Password reset by email** (Resend): request a link, a single-use token that expires in an
     hour (kept only as its hash), set a new password, end the account's other sessions. Both
     sites link to it.
- Pages call it a **Foundry Account** ("Sign in with your Foundry Account"): the one login for all
  of SiteSentrix's software (owner's decision, 2026-10-19; it replaces "StitchFoundry Account",
  decided earlier the same day). The account service itself is StitchFoundry's, worked on in the
  StitchFoundry repository.
- Accounts made on MakerFoundry get the cloud's usual sign-up credits (100 AI credits, within
  the free-offer budget), the same as accounts made in StitchFoundry (owner's decision, 2026-10-19).
- Later ("One SiteSentrix account" below): a sign-in page of the account service's own, so other
  sites never see passwords.

## Licences a listing can carry
Plain-language choices (final wording is the owner's; the draft wording is in
`apps/makerfoundry/src/licences.rs`, with "small business" at 200 items until the owner says):
- **Personal use**: make it for yourself and gifts.
- **Small business**: sell what you make with it (stitched or cut items), up to a number of items.
- **Commercial**: no limit on items made.
- Free designs may also be **CC BY**, **CC BY-NC** or **CC0**.
Nobody may resell or share the files themselves.

## Tech
- **Rust** web service (axum or similar), server-rendered HTML, Postgres.
- **Files** in object storage (Cloudflare R2: no download-bandwidth charges), served through
  signed, expiring links.
- **Hosting**: Railway (the owner's Pro plan), Cloudflare in front (DNS, TLS, caching).
- **Payments**: Stripe Connect. **Email**: Resend.
- Reuse StitchFoundry's crates for reading and drawing embroidery files (`sf-formats`,
  `sf-core`), as a git dependency pinned by commit.

## Milestones and task ids
| Id | What | Done when |
|---|---|---|
| MF-0 | Workspace: Cargo workspace, CI (fmt, clippy -D warnings, tests), `/health`, a hello page deployed to Railway | the page answers on a Railway address. **Done 2026-10-19**: https://makerfoundry-production.up.railway.app |
| MF-1 | Accounts: sign up, sign in, sessions, password reset by email | a person can make an account. **Done 2026-10-19**, live (`/signup`, `/signin`, `/forgot`, `/account`) with its Postgres on Railway; reset emails wait on the account service's `RESEND_API_KEY` |
| MF-2 | Upload and preview: embroidery and laser files read, checked and drawn; photos; R2 storage | an uploaded PES shows its stitches. **Embroidery done 2026-10-19**: `/preview` reads every StitchFoundry format (`sf-formats`, pinned by commit) and draws the stitches with size, stitch count and colours. **SVG, DXF and PDF done 2026-10-19**: drawn from their shapes in mm, with size, shapes, line length and colours (layers). **Photos and keeping files built 2026-10-19**: `/designs/new` keeps a design file and up to 6 photos (decoded and saved again as JPEG, metadata dropped) in R2, as a draft only its maker sees (`/designs`); live once R2's settings are in Railway (`docs/deploy.md`) |
| MF-3 | Listings: design pages, browse, search, tags, seller pages; free downloads | a free design can be found and downloaded. **Done 2026-10-19**: maker profiles (`/account/maker`, `/makers/<handle>`), design details and licence (`/designs/<id>/edit`), publish with the maker's word it's theirs to share, public pages (`/d/<id>`) with free counted downloads, browse and search (`/browse`: words, tag, kind, newest or most downloaded), newest on the home page. Publishing is open only to `MF_PUBLISHERS` until the launch (MF-5's takedown process first) |
| MF-4 | Selling: Stripe Connect onboarding, checkout with the platform fee, library, sales page, refunds | a test-mode sale pays the seller less the fee. **Built 2026-10-19** (waiting for the Stripe test keys to try it live): sellers set up a Stripe Express account paid monthly (`/account/selling`); a design gets a price ($1 to $200, or free); buyers pay on Stripe Checkout, a destination charge with an application fee of 10% + the card fee (2.9% + 30¢), all in integer cents (`money.rs`); the sale is delivered only when Stripe confirms it, by a signed, de-duplicated webhook (`/stripe/webhook`) or by asking Stripe when the buyer comes back; every sale and refund is three ledger rows (`mf_ledger`); the buyer's library (`/library`), the seller's sales (`/account/sales`), and full refunds by an admin (`/admin/orders`). Paid designs people bought can't be deleted. No sales tax yet (open decision) |
| MF-5 | Safety: report a listing, DMCA page and process, admin queue, upload limits, rate limits | a takedown can be done in minutes. **Built 2026-10-19**: Report this design (`/d/<id>/report`), the copyright policy (`/dmca`, linked from every page), notice and counter-notice forms (17 U.S.C. § 512(c)(3), (g)(3)), the admin queue (`/admin`, accounts in `MF_ADMINS`) with one-click take down, dismiss and restore, all logged, repeat takedowns counted; rate limits on sign-up, sign-in, reset emails, reports, previews and uploads, and 500 designs an account. The policy's wording is a draft for the owner; the designated agent (`MF_DMCA_AGENT`) once registered |
| MF-6 | Launch: Terms, Privacy, Seller Terms, a landing page, makerfoundry.app live | public launch. **Ready to launch, waiting on the owner (2026-10-19)**: `/terms`, `/privacy` and `/seller-terms` are drawn from `docs/legal/*.md` (`legal.rs`); while a document has the owner's markers it's a draft, seen only by admins. The launch switch `MF_LAUNCHED=yes` opens the site only once no markers are left: the legal pages go public, signing up asks for agreement to the Terms and the Privacy Policy (18 or older), publishing and setting up selling to the Seller Terms, each agreement kept with the document's version (`mf_agreements`); publishing opens to everyone; the home page invites people in; `robots.txt` lets search engines in and `/sitemap.xml` lists published designs and makers. Before the launch `robots.txt` keeps them out. The owner's steps: `docs/launch.md` |
| MF-7 | StitchFoundry and LightFoundry integration | publish from the app. **MakerFoundry's side built 2026-10-19**: the apps' API (`POST /api/v1/designs`, `docs/api.md`): an app sends a design with its Foundry Account sign-in (checked with the account service, never kept), it's read and kept as a draft exactly as an upload, and the app opens its page, where signing in comes back to the draft; browsers may call it from the web apps' sites (`MF_API_ORIGINS`). **Next, in the apps' repositories**: StitchFoundry's Publish to MakerFoundry command (the note in `docs/api.md`), then LightFoundry's; **"Open in StitchFoundry" built 2026-10-20**: free, published embroidery designs' pages link to StitchFoundry's web app (`?open=`, its OPEN-01), which fetches the file from `GET /api/v1/designs/{id}/file/{name}` (CORS; `https://stitchwright.github.io` added to the default `MF_API_ORIGINS`) |
| MF-8 | The first version's missing pieces: free/paid and hoop-size browsing, laser material and thickness, several files per design, abandoned checkouts expiring | everything in "The first version (MVP)" is built. **Done 2026-10-19**: free/paid filter on Browse; unpaid checkouts expire (Stripe's page lasts 2 hours, `checkout.session.expired`, and any order still unpaid a day on); embroidery designs found by the hoop they fit (4×4, 5×7, 6×10, 8×8, 8×12 sewing fields, either way round; `sizes.rs`) and shown as "fits a 4×4 in hoop"; laser designs get a material and thickness, shown and searchable; a design can have up to 11 files, one a format (`files.rs`, `mf_design_files`), each read and checked like the first and of the same kind, listed on its page as Formats and downloadable by whoever may download the design (a paid design's buyers get every format, and a sold design's formats can't be removed) |
| MF-9 | Format conversion on download: an embroidery design in every format StitchFoundry writes | buy a PES, also get a DST. **Done 2026-10-19**: a design's page offers the embroidery formats its maker didn't upload (DST, EXP, JEF, PEC, VP3, XXX, U01, PCS, TAP, DSB), each converted from the maker's file by `sf_formats::write` when downloaded (`convert.rs`, `/d/<id>/download?as=dst`), not stored; the same rules as the design's own files (free for anyone, paid for its buyers), counted as downloads, 120 an hour an address; on unless the maker unticks it in the details (Seller Terms § 9 says so, for the owner) |
| MF-10 | Saved designs and collections | people keep designs to come back to, and share lists. **Done 2026-10-19**: Save on a design's page to a private Saved list (made the first time) or to your own collections (`/collections`, up to 100, 1,000 designs each), private unless made public; public ones show on the owner's maker page; a design shows how many people saved it, and Browse can sort by Most saved; unpublished designs drop out of collections (`collections.rs`) |
| MF-11 | Makes: photos of what people made from a design | a design's page shows other people's results. **Done 2026-10-19**: anyone with a maker profile (and, until the launch, allowed to publish) posts a photo and a short note on a published design, with their word it's their own photo of their own make; the photo is cleaned like a design's; the design's page shows the newest 60 and a count; the poster, the design's maker and the admins can take one down; 10 a person a design (`makes.rs`). The Terms' wording about makes is the owner's (`docs/legal/README.md`) |
| MF-12 | Reporting a make | a make that breaks the rules can be flagged and taken down from the admin queue. **Done 2026-10-20**: Report on each make (`/makes/<id>/report`: not theirs, offensive, spam, something else), shown in `/admin` with the photo and Take the make down; the removal is logged; a report of a make already gone is just closed |
| MF-13 | Following makers | people see new designs from the makers they like. **Done 2026-10-20**: Follow on a maker's page (with its follower count; who follows whom isn't shown), `/following` lists the newest 60 designs of the makers you follow, up to 1,000 follows (`follows.rs`) |
| MF-14 | Comments on designs | buyers ask makers questions and share tips under a design. **Done 2026-10-20**: comments under a published design, oldest first, the maker's marked; a maker profile needed (and, until the launch, being allowed to publish); plain text only, nothing becomes a link; 20 an hour; the author, the design's maker and the admins take one down; Report on each, with Take the comment down in `/admin` (`comments.rs`) |
| MF-15 | Your data: a copy and deleting it | what the Privacy Policy promises, done by the person. **Done 2026-10-20**: `/account/data` downloads everything kept about the account as JSON, and deletes it (typing delete to confirm): profile, designs, files and photos, makes, comments, collections, follows and sign-ins, in one transaction, then the files; kept: purchases, sales, ledger, agreements, reports and notices, and bought designs (unpublished, for their buyers) (`data.rs`) |
| MF-16 | The admins' overview | the owner sees the site's numbers at a glance. **Done 2026-10-20**: `/admin/overview` (admins only): accounts, maker profiles, sellers ready, follows; published designs by kind, paid, drafts, taken down, downloads, makes, comments; sales, refunds, and money from the ledger (paid by buyers, to sellers, MakerFoundry's fees, card fees, fees this month); what's waiting in the report queue (`overview.rs`) |
| MF-17 | Open in StitchFoundry for buyers of paid designs | buyers open what they bought in the web app too. **Done 2026-10-20**: a paid embroidery design's page shows Open in StitchFoundry to its buyers, maker and admins; `/d/<id>/open` checks that, makes a link to that design's file good for ten minutes (kept as its hash, `mf_open_links`), and sends them to the web app with it; the file address takes the link for paid designs and refuses them without one (`api.rs`) |
| MF-18 | Open in LaserFoundry: MakerFoundry's side | laser designs open in LaserFoundry's web app. **MakerFoundry's side built 2026-10-20**: SVG and DXF laser files are served at the same file address as embroidery (free for anyone, paid with a buyer's link), and the button (\"Open in LaserFoundry\") is ready but off (`LASERFOUNDRY_OPENS_LINKS`) until LaserFoundry's web app opens `?open=` links and allows makerfoundry.app in its content security policy: the note for its chat is in `docs/api.md` |
| MF-19 | Link previews | a shared design shows its title, words and photo in chats and on social sites. **Done 2026-10-21**: published designs' and makers' pages carry the standard Open Graph tags (title, a line of the description, the page's address, the first photo) and a canonical address; drafts carry none (`pages::page_with`) |
| MF-20 | Property tests of untrusted input | every reader of what people send is tested against random and damaged input, as StitchFoundry's are. **Done 2026-10-21**: `tests/untrusted_input.rs` (proptest) gives the embroidery, SVG, DXF and PDF readers, photos, typed prices and thicknesses, and Stripe's signature header random bytes, damaged copies of good files and generated content (SVG elements, DXF group codes, PDF drawing operators with extreme numbers); each must answer without panicking, and what it accepts must be within the limits (sizes, stitch and shape counts, a preview of numbers and colours only); accepted embroidery converts to every format. It found one bug, fixed: a signature header with a time far in the past overflowed the check of its age (`stripe.rs`). 128 cases a test in CI; more with `PROPTEST_CASES` **2026-10-30**: they found an SVG whose huge arcs (radius 1e30) made usvg allocate gigabytes before our limits were checked; SVGs with a drawing number beyond ±1e7 are now refused before usvg reads them (`laser::MAX_SVG_NUMBER`, `docs/debug-logs/2026-10-30-svg-arc-memory.md`) |
| MF-21 | Signing everyone out after a breach | a session copied from the database stops working. **Done 2026-10-22**: `MF_END_SESSIONS_BEFORE` (a UTC time, a date or Unix seconds) deletes the sessions made before it when the service starts; `docs/deploy.md` › After a breach lists every secret the service holds and where each is renewed |
| MF-22 | Activity | makers learn what people did with their designs without email. **Done 2026-10-22**: `/activity` lists, newest first, the comments and makes on a maker's designs by other people (who, the design, a line of what they wrote), their sales and refunds (what they got or gave back) and new followers (not named); what came since they last looked is marked new, and the account page counts it. Drawn from those tables; only when they last looked is kept (`mf_activity_seen`, in the data download and deleted with the data) (`activity.rs`) |
| MF-23 | Refunds: the seller bears the card fee | a refunded seller is out only the card fee Stripe keeps (the owner's decision, Seller Terms § 7). **Done 2026-10-22**: an admin's refund gives the buyer the whole price back and reverses the seller's transfer, without the application fee; MakerFoundry then gives its 10% back to the seller as a partial refund of that fee (`/v1/application_fees/{id}/refunds`). In the ledger the refund takes the price from the seller (`refund`) and the 10% goes back (`fee_return`); the card fee row stays. If Stripe can't give the fee back at once, `/admin/orders` offers Give the fee back (`fee_returned_at`). Activity and the sales page show what the seller lost. The restricted key needs three more permissions (`docs/deploy.md`) |
| MF-24 | Sales tax with Stripe Tax | the buyer pays the price plus the tax, which is MakerFoundry's, not the seller's. **Built 2026-10-27, off until `MF_STRIPE_TAX=yes`**: checkouts ask Stripe Tax for it (`automatic_tax`, MakerFoundry liable, billing address required, prices exclusive, tax code `txcd_10505001` or `MF_TAX_CODE`); a paid session is believed only if its subtotal is the price and its total the price plus the tax; the tax is taken back from the seller's transfer by a reversal once paid (webhook, return page, sweep; checked with Stripe so it's never taken twice); `tax` ledger rows; a taxed refund takes the price back from the seller first, then refunds the buyer everything; the buyer's library shows the tax, the admins' overview the tax collected (`selling.rs`, `docs/deploy.md` › Sales tax) |
| MF-25 | Approval links (the Preview & Approval Hub, first slice) | a shop sends a customer a private link to a design and the customer approves it. **Built 2026-10-29; its wording approved and in the Terms (§ 9) and the Privacy Policy 2026-10-30.** Admins only until the owner sets `MF_PROOF_KEY` and `MF_PROOFS=yes`: any Foundry shop (any Foundry Account; owner, 2026-10-29) sends a proof from `/proofs/new`: a design file, read and checked as an upload and drawn from what was read, with mock-up photos cleaned as a design's are, its shop name, who it's for (the shop's own note) and a message. The customer's link `/p/<id>/<token>` is an HMAC of the proof's id and link generation with `MF_PROOF_KEY`, so nothing in the database opens it; the shop sees it any time, can make a new one (the old stops working), turn it off, or delete the proof. The customer, with no account, sees the latest version (earlier ones too), comments, approves it or asks for changes under the name they type (a newer version refuses answers to the old one); the page isn't cached, indexed or passed on as a referrer. The shop sees sent / viewed / approved / changes asked for, replies, and sends new versions (up to 30). Who approved which version and when is kept. Proofs are deleted a year after their last activity (owner, 2026-10-29; the 10-minute sweep), are in the data download and go with the account's data. 500 proofs a shop, 100 proofs or versions a day, 30 answers an hour from one address (`proofs.rs`) |
| MF-26 | Product-photo previews | the design drawn onto a product photo on the server, added to a proof. **Planned**: needs MF-25, the stock photos (owner: own photos or CC0, in `ASSETS.md`), the StitchShop prototype's location and licence, a raster library with an allowed licence; placing by typed size and position unless the owner agrees to a little JavaScript for dragging |
| MF-27 | Send to customer from the apps | `POST /api/v1/proofs` and its status, with the Foundry Account token as MF-7. **Planned**: needs MF-25; the button and `stitchfoundry://` links are StitchFoundry's |
| MF-28 | Shop branding and subdomains | a shop's logo, colours and `<shop>.makerfoundry.app` (one Railway wildcard domain); sign-in cookies stay on makerfoundry.app. **Planned**: needs MF-25 |
| MF-29 | Shop plans and the For Shops page | monthly plans on Stripe, tier limits, the demo shop. **Planned**: needs MF-28 and the owner's prices |
| MF-30 | StitchShop order handoff | an approved proof becomes one StitchShop order. **Planned**: needs MF-25 and a StitchShop order API (owner: product or demo) |
| MF-31 | More tools | previews and sending for ScreenFoundry (the owner's screen printing, DTG and sublimation software: previews show ink on fabric, not stitches), PatternFoundry and LightFoundry (laser cuts and engravings; owner, 2026-10-28). **Planned**: needs MF-26, MF-27 and those apps |
| MF-32 | Custom domains | a Top-tier shop's own domain through Cloudflare for SaaS (100 hostnames included, then $0.10 each). **Planned**: needs MF-29 |
| MF-33 | Live camera preview | the design on the real garment through the phone camera. **Planned**: needs MF-26 and the owner's agreement to the JavaScript it needs |
| MF-34 | Report a bug, suggest a feature | at the foot of every page, two buttons open an email to the owner with "[MakerFoundry] Bug report: " or "[MakerFoundry] Feature request: " as the subject and a short template. **Done** (2026-10-30) |
| MF-60 | A new mark | the three crafts on one red tile (needle and thread, laser head and beam, print nozzle over printed layers) replaces the hoop with a diagonal run of stitches, which read as a "no" sign; header, favicon and the Facebook page. **Done** (2026-10-31) |
| MF-61 | 3D printing on the home page | the lead, the page description and a fourth card name 3D-print models (STL, 3MF, OBJ, STEP, in a viewer you can turn); while MakerFoundry 3D is off the card says the models open soon. The cards sit one, two or four to a row. **Done** (2026-10-31) |
| MF-62 | The home page leads with founding creators | before the launch, an invitation to become one of the first 100 founding creators (the badge, early access to MakerFoundry 3D, the spots left) with a button to `/founding`, in place of "Coming soon"; after the launch, a third button beside Browse and Share while spots are left. **Done** (2026-10-31) |
| MF-63 | Facebook link in the footer | every page's footer links to facebook.com/makerfoundryapp beside Terms, Privacy, Seller Terms and Copyright. **Done** (2026-10-31) |
| MF-90 | YouTube and Buy Me a Coffee in the footer | every page's footer also links to youtube.com/@MakerFoundryApp and buymeacoffee.com/bwattendorf (the owner's, 2026-11-01): plain links, nothing loaded from either site. **Done** (2026-11-01) |
| MF-92 | Picks of the week and galleries on the home page | the owner's request (2026-11-01). Each week (Monday, UTC) one design per craft (embroidery, laser, 3D printing while MakerFoundry 3D is open): an admin picks it with a button on the design's page, or else the week's most liked and made design of that craft (then most downloaded, then newest) is kept for the week when the home page first shows it (`mf_picks`, migration 0045). The home page shows the picks, then the newest designs, a gallery of what people made (their make photos) and the most downloaded; a pick's page says so; `/picks` keeps the last 26 weeks (in the sitemap). Original wording and layout. **Done** (2026-11-01) |
| MF-98 | Branded shops | the owner's decision (2026-11-02): each shop that sells through MakerFoundry gets a free branded page from its catalogue's optional `shop.brand` (display name, intro, logo fetched from the hub and drawn again, an accent colour used only where it reads at 4.5:1, a website link) and a short address `<handle>.makerfoundry.app` (first come first served, names reserved; signing in and checking out stay on the main address). No monthly fee: 10% plus the card fee. Migration 0046; the wildcard domain is the owner's one-time step (`docs/deploy.md`); Seller Terms wording drafted in `docs/legal/shops-draft.md` for approval. **Built** (2026-11-02) |
| MF-91 | Admins delete an account's data | `/admin/accounts` (issue #55): an admin looks up an account by number (its email and designs) and, typing the number to confirm, deletes its data with the same code as the person's own delete (files in storage too, amounts kept), recorded in the moderation log; admins' accounts are refused. For deletion requests by email and test accounts nobody can sign into. **Done** (2026-11-01) |
| MF-93 | Lower-detail previews of paid designs | someone who hasn't bought a paid design can't rebuild its file from its page (audit, 2026-11-01; the owner's decision: lower-detail previews). **Done** (2026-11-01): a paid embroidery or laser design's page shows people who may not download it (everyone but its buyers, its maker and the admins) a PNG drawn on the server from the preview (`coarse.rs`, `/d/<id>/preview.png`: at most 560 px across and 4 px a millimetre, so stitches run together), never the preview SVG with every stitch or point; a paid 3D model's viewer mesh (`/3d/<id>/files/<f>/mesh`) is, for them, the mesh with every corner moved to the middle of a 40-cell grid and at most 4,000 triangles (`viewer::coarse`), sent `private`. Free designs, buyers, makers and admins unchanged. The other places were checked: link previews use photos and the model's picture, the API and JSON give no geometry, collections and browsing show photos |
| MF-94 | A purchase stays the buyer's | a seller who makes a sold design free or unpublishes it doesn't take it from its buyers (audit, 2026-11-01). **Done** (2026-11-01): `listings::access` counts a paid order whatever the design costs now and whether or not it's published; only a takedown (`removed`, the DMCA process) or a refund ends a buyer's download |
| MF-95 | Nobody pays for a deleted design | deleting a design (or an account's data) first closes the checkouts that could still pay for it (audit, 2026-11-01). **Done** (2026-11-01): `selling::close_checkouts` asks Stripe about each unpaid checkout of the design from the last 30 days: an open one is expired at Stripe (`/v1/checkout/sessions/<id>/expire`) and its order marked expired; while one is paid but not yet confirmed here (or a bank debit is on its way), or Stripe can't be reached, the deletion waits and the maker is told why. The person's own data deletion and an admin's do the same for every design they would delete |
| MF-96 | Copies of messages to shops are not kept for ever | the outbox's copies of messages to shops (each with the customer's name, address and note) go when the order's art does (audit, 2026-11-01). **Done** (2026-11-01): the shop-order sweep (`after::forget_messages`) deletes messages the hub took or refused for good once their order has been done with (shipped, collected, cancelled, refunded, expired, abandoned) for 30 days, and blanks the contents of the last one of each conversation, which stays for its number. Blanking them when a person deletes their data is PR #63's |
| MF-97 | A message from a shop that couldn't be applied comes again | the StitchHub client keeps its cursor before a message it didn't acknowledge (StitchHub's PR #45: the hub's `next` passes messages handed out but not acknowledged). **Done** (2026-11-01): `hub::sync` saves the hub's `next` only when every message of the page was taken (applied, held for an earlier one, or unreadable); otherwise it fetches again from the cursor before the page, and the messages it took are acknowledged and don't come again. The stand-in hub now keeps a cursor as StitchHub does |
| MF-99 | Staging's email goes to a test mailbox | for the staging environment (stitch-e2e `docs/staging.md`): `MF_MAIL_CATCHER_URL` (a Mailpit service's address) turns notices on and sends every one to Mailpit's send API instead of Resend, without Resend's key, so nobody real gets staging mail and the end-to-end tests can read it. **Built** (2026-11-03); never set in production |
| MF-54 | Founding creators | MakerFoundry's first 100 creators, with a numbered badge. **Built 2026-10-30** (the owner's decisions: 100 spots, makers apply and an admin approves, a numbered badge): `/founding` explains it and shows the spots left (it works before the launch, with a link preview, for the Facebook post); signed-in makers apply (the name to know them by, an optional link to their work, what they make), one application an account; admins approve or decline at `/admin/founding`, approving giving the lowest free number up to 100 (never more, approvals one at a time); "Founding creator #7" on their maker page, their designs' pages and their account page; removing a badge frees its number; applications in the data download and deleted with the data (`founding.rs`). **#125 (2026-11-13):** a sign-up from `/founding` is remembered for 30 days, so the account page says "One more step: send your founding creator application" until one is sent, and `/admin/founding` has a "Signed up but not applied" list (last 30 days, no email addresses). Early access to MakerFoundry 3D comes with the 3D hub (#22) |

## The 3D printing hub (MF-35 onwards, planned 2026-10-30)
A section for 3D-printing models (`/3d`), built on the same engine as the designs: model pages
with several files and images, server-drawn thumbnails, Bambu Studio/OrcaSlicer print profiles
(PrusaSlicer and Cura at a basic level), remixes with their licences enforced, likes, makes,
collections, follows, comments, search with print-profile filters, moderation and DMCA, a bulk
import of a creator's own files (MakerWorld, Printables, Thingiverse), founding-creator invites,
an opt-in social showcase, and Get this printed. The comparison with Thingiverse and Printables is
`docs/3d-hub-feature-matrix.md`; the plan, its data model and the owner's decisions are
`docs/3d-hub-plan.md`; the work is the GitHub issues labelled `3d-hub` (P1 `claude-queue`, in
order; P2 and P3 `claude-proposed`). Off until `MF_3D=yes`; selling, approval links and the
StitchShop ordering work stay as they are.

Progress (test-first, from `docs/3d-hub-test-plan.md`):
- **MF-35 foundation, built 2026-10-30** (#3): designs may be `model3d` (migration 0022: a
  category, print settings, and where an imported model was first posted); the 14 categories
  (`model3d/mod.rs`); 15 more licences for 3D models only (CC BY-SA, BY-NC-SA, BY-ND, BY-NC-ND,
  GPL 2/3 or later, LGPL 2.1/3 or later, BSD 3-Clause, Apache 2.0, MIT, CERN-OHL-S/W/P 2.0, all
  rights reserved), each linked to its official text, never offered on an embroidery or laser
  design (`licences::for_kind`, `find_for`); `/3d` for admins until `MF_3D=yes`, kept out of
  robots and the sitemap until then.
- **MF-36 STL, built 2026-10-30** (#4): our own binary and ASCII reader (`model3d/stl.rs`) on a
  shared mesh with the hub's limits (`model3d/mesh.rs`: 100 MB a file, 5 million triangles kept,
  5000 mm a side, every number finite); a binary count must match the file's size, checked before
  anything is allocated; property tests with random and damaged files (20,000 cases under a 3 GB
  cap pass).
- **MF-37 OBJ, built 2026-10-30** (#5): our own reader (`model3d/obj.rs`): `v` and `f` lines only
  (every index form, negative indexes, polygons cut into triangles), every other line ignored;
  5 million vertices at most; a zero, missing or later index refused; property tests with random,
  damaged and generated files.
- **MF-38 3MF, built 2026-10-30** (#6): our own small ZIP reader and writer (`model3d/zip.rs`, on
  flate2 and crc32fast): 4,096 entries, 512 MB an entry, 1 GB in all, at most 200 times the
  archive's size, no overlapping entries, no `..`/absolute/backslash names, no encryption or
  ZIP64, every entry's length and CRC checked as it's inflated. The 3MF reader
  (`model3d/threemf.rs`): roxmltree with DTDs refused; units to millimetres; components and
  build transforms, Bambu/Orca `p:path` parts; at most 16 levels and 50 million placed triangles;
  a model part at most 256 MB unpacked. The file is rebuilt with only the model parts read, their
  relationships, PNG/JPEG pictures decoded and written again (`photos::reencode`), and slicer
  settings that read as text; scripts, programs, unknown parts and sliced G-code are dropped and
  listed. Property tests: random archives, damaged 3MFs and model XML, generated models.
- **MF-39 the file allow-list, built 2026-10-30** (#7): `model3d/files.rs` checks each file by
  what it is: STL and OBJ read as meshes, 3MF read and kept as rebuilt, STEP/STP checked to be
  ISO 10303-21 text (header, `HEADER;`, end line, no NUL bytes), `.mtl`, `.scad`, `.txt` and
  `.md` checked to be text of at most 5 MB; every other extension refused. STEP and the text
  companions are only ever downloads (`attachment_only`).
- **MF-40 thumbnails, built 2026-10-30** (#8): our own renderer (`model3d/render.rs`):
  orthographic, depth-buffered, flat-shaded, PNG with a clear background; a hero view and front,
  side and top views (each slightly turned so faces shade apart); at most 300,000 triangles drawn
  (an even sample beyond) and 400 million pixels visited, so any mesh draws in bounded time; a
  3MF's own thumbnail is preferred for the main picture (`render::hero`).
- **MF-41 model upload, built 2026-10-30** (#9): `/3d/new` (admins until `MF_3D=yes`) takes a
  model's files and images in one upload (`model3d/upload.rs`): 100 MB a file, 300 MB, 30 files and
  12 images a model, 2,000 models an account, 100 new models a day; at least one STL, 3MF or OBJ.
  Each file is read and checked off the request threads, its main picture (a 3MF's own, or drawn)
  and three views drawn, the images cleaned; all stored and noted as one draft (`mf_model_files`,
  migration 0023), or nothing kept if any part is refused. The route has its own body cap and a
  5-minute time limit. The maker's draft page lists each file with its picture, views, size,
  triangles and what a 3MF's rebuild left out; deleting the draft (or the account) deletes every
  stored file. (The whole upload is held in memory while it's read: streaming it to storage is a
  later improvement if the server's memory needs it.)
- **MF-42 details, publishing, public page and downloads, built 2026-10-30** (#10): a model's
  details take a category (required) and the 3D licences; publishing needs a title, category and
  licence and the maker's word that they created it or may share it. The public page
  (`/d/<id>/<slug>`) shows its main picture, category, files with their pictures and facts (what
  a rebuild dropped is for the maker only), images, makes, licence and comments, with a link
  preview using the main picture. Downloads: each file, or all of them as one ZIP made on the
  first download and kept (`zip_key`, migration 0024), both counted once published. Uploaded
  file names lose any folders. While `MF_3D` is off, models stay out of browsing, the home page
  and the sitemap, and their pages and files are for admins only. Browsing shows 3D models with
  their picture.
- **MF-52 bulk import, built 2026-10-30** (#20): `/3d/import` takes many files or ZIPs at once
  (200 files or 2 GB, counting what ZIPs hold; a ZIP at most 300 MB and read with the ZIP limits;
  20 imports a day). Each file is stored as a pending import file as it arrives
  (`mf_import_files`, migration 0025), so memory holds one file at a time; anything that isn't a
  model file or image is noted and left out, system files in ZIPs skipped. Files are grouped
  into models (`model3d/import.rs`): by folder in a ZIP, else by name stem (endings `_plate2`,
  `-v2`, `_part3`, `(1)` ignored), images joining their stem. The review page shows the groups
  and what was left out; "Make drafts" makes each group a draft titled by its name, a file that
  fails its checks reported and the rest kept. Pending files and unpublished imported drafts go
  after 30 days (the 10-minute sweep), and with the account's data.
- **MF-53 import review, built 2026-10-30** (#21): before drafts are made, the review page lets
  the creator rename models and move files between them (typing another model's name merges,
  a new name splits a file out; plain forms). Once drafts exist it lists them (ready, or what
  each needs), gives every draft without one a category and a licence at once, and publishes
  every ready draft after the creator ticks "I confirm I created these models or have the right
  to share them" (and the Seller Terms once launched); drafts still missing something are named.
  A model's details take "Originally published on" (`model3d/origin.rs`): MakerWorld,
  Printables or Thingiverse, the creator's own page there (https, on that site's own host, never
  fetched) and the date (from 2000 to today), shown on its page as "Originally published on
  MakerWorld, 12 March 2025" with a `nofollow` link.
- **MF-43 Bambu Studio / OrcaSlicer settings, built 2026-10-30** (#11): a 3MF project's
  print profile (`model3d/profile.rs`, `mf_print_profiles`, migration 0026): the slicer (from the
  model's `Application` metadata), printer and its badge (X1 Carbon, A1 mini, H2D, MK4…),
  nozzle, layer height, infill, walls, supports, plates, and the filaments the model's parts
  actually use (`model_settings.config` extruder slots; every loaded one when that isn't known),
  from `project_settings.config`. Only these fields are kept; numbers out of range are left out;
  settings that can't be read keep the geometry and a profile marked unreadable. To be checked
  against real files from our creators (locally, never committed). Multi-colour painting on the
  mesh itself isn't counted yet.
- **MF-44 PrusaSlicer and Cura, built 2026-10-30** (#12): the same profile from PrusaSlicer's
  `Metadata/Slic3r_PE.config` (`; key = value`) and `Slic3r_PE_model.config` (slots used), and
  from Cura's `Cura/*.cfg` (layer height, infill, walls, supports, nozzle, material type,
  printer name), each slicer's own key names mapped onto the same checked fields. A 3MF from a
  program that isn't a slicer is geometry only; a slicer's settings that can't be read never
  fail an upload.
- **MF-45 print profiles on pages, built 2026-10-30** (#13): a model's page lists each print
  profile: its name, printer badges, the slicer and file it came from, nozzle, layers, infill,
  walls, supports, plates and the filaments used as colour swatches (small SVGs in `data:`
  addresses, as pages allow no inline styles), with a download of that 3MF; a profile whose
  settings couldn't be read still shows and downloads. Makers add files to a model (another
  profile, a STEP source) and remove them (`/3d/<id>/files`), never the last printable file nor
  any file of a model people have bought; the model's ZIP is made again after any change.
- **MF-47 remixes, built 2026-10-30** (#15): a model names the models it was made from
  (`mf_remixes`, migration 0027), by address or number: published 3D models, or the maker's own.
  The licence rules of § 6 (`model3d/remix.rs`, tested over all 441 pairs of 3D licences) are
  checked when a parent is added (an ND or reserved licence refused, or parents no licence can
  satisfy, naming the parent), when the licence is changed, and at publishing; a creator may
  always remix their own. No cycles; up to 20 parents. A model's page shows "Remix of" and its
  remixes; `/3d/<id>/tree` shows ten generations each way, a taken-down model named without a
  link. The rules are still to be confirmed by the owner, ideally with a lawyer.
- **MF-48 likes and community on 3D, built 2026-10-30** (#16): likes on published 3D models
  (`mf_likes`, migration 0028; one an account, 300 an hour), counted on the page, in the data
  download and deleted with the account's data (which now also lists the creator's imports).
  Makes, collections, follows and comments work on 3D models as on designs; collections and the
  Following feed show 3D models once `MF_3D` is on; a reported comment on a model reaches the
  admins' queue.
- **MF-49 3D search, built 2026-10-30** (#17): `/3d` is the 3D section's search
  (`model3d/search.rs`): words, category, licence, file type, has makes, has print settings,
  printer, multi-colour and filament, newest or popular first, paged by key (`after=`), filters
  outside their lists ignored. What the filters need is kept on each model (migration 0029,
  refreshed whenever its files change) and indexed; popularity (likes and makes of the last 30
  days, plus downloads) is refreshed by the 10-minute sweep. With 100,000 models every filtered
  page took under 6 ms on the test database (the test asserts under 100 ms).
- **MF-50 creator profiles, built 2026-10-30** (#18): a maker's page shows their embroidery
  designs, laser designs and 3D models in sections of their own (models once MakerFoundry 3D is
  open to the viewer), with counts of published designs and models, likes and downloads; drafts
  never show; the founding badge only when set.
- **MF-51 moderation for 3D, built 2026-10-30** (#19): reports, copyright notices, takedowns,
  counter-notices and restores work on 3D models as on designs (tested end to end): while a
  model is down its page, downloads, file pictures and search results are gone for everyone but
  its maker and the admins, and come back on restore. Takedowns count across kinds for repeat
  infringers. The 3D limits are each tested: 100 models a day, 20 imports a day, 300 likes an
  hour.
- **MF-54 founding creators' early access to 3D, built 2026-10-30** (#22): while `MF_3D` is off,
  approved founding creators use MakerFoundry 3D as admins do (upload, import, publish, see their
  models), and their numbered badge shows on their 3D models; everyone else waits for the
  switch. Removing a badge removes the early access.
- **MF-55 showcase consent, built 2026-10-30** (#23): an unticked "Feature my project on
  MakerFoundry's social pages" box on the 3D upload form, on makes of 3D models and on the
  import's publishing step (`model3d/showcase.rs`, `mf_showcase_consents`, migration 0030).
  Ticking it stores a consent with its time; `/account/showcase` lists them with Withdraw on
  each (only their creator's); withdrawing keeps the times; consents are in the data download
  and deleting the data withdraws them all. **The box's wording and what it permits are the
  owner's to approve** with the Terms (#26).
- **MF-56 showcase queue, built 2026-10-30** (#24): `/admin/showcase` (admins only) lists only
  consented, published, not withdrawn projects: a picture, the creator's display name (never an
  email), the model's link and licence, and a caption crediting that name and linking the model
  (our template, for the owner to approve). Admins post by hand and note the platform and the
  post's address (`mf_showcase_posts`, migration 0031). A withdrawn consent, or a model taken
  down, takes the item off the queue and flags its posts ("Take these down") until marked taken
  down.
- **MF-57 Also by this maker, built 2026-10-30** (#25): a 3D model's page lists a few of its
  maker's published embroidery and laser designs, with a link to all their work; nothing when
  they have none. No "Get this printed" button (the owner's decision: off for now).
- **MF-46 the 3D viewer, built 2026-10-30** (#14): our own small WebGL viewer
  (`static/viewer.js`, plain JavaScript with no libraries, MIT OR Apache-2.0; the owner agreed to
  it, 2026-10-30). At upload the server writes each mesh's viewer copy (`model3d/viewer.rs`: at
  most 100,000 triangles, 16-bit quantized, `mesh_key`, migration 0032), served under the same
  rules as the file's pictures. A model's page keeps its server-drawn picture; the script, when
  it runs, swaps in a view you can turn (drag) and zoom (scroll or pinch). Only model pages allow
  `script-src 'self'` and `connect-src 'self'`; every other page keeps the old policy (tested).
  Checked in Chromium: it draws, turns, fits a phone, no console errors. Models uploaded before
  this keep their pictures only.
- **MF-58 legal wording for 3D, drafted 2026-10-30** (#26): `docs/legal/3d-draft.md`, for the
  owner's approval (and ideally a lawyer's read of the remix rules): Terms §§ 5 and 6 (3D models,
  licences, remixes, imports, rebuilt files, print settings, likes, the showcase consent and
  withdrawal) and the Privacy Policy's additions (models, likes, imports, consents). **Waiting on
  the owner**; once merged, the launch checks (#27) and `MF_3D=yes`.
- **MF-59 launch checks, done 2026-10-30** (#27): the whole suite green; all 30 property tests
  at 20,000 cases each under a 3 GB memory cap (45 s in release); every 3D page checked at 390
  and 1280 px with no sideways scrolling. The owner's steps to open it are in `docs/launch.md`
  (Opening MakerFoundry 3D): approve the wording (#26), try it as a founding creator, then
  `MF_3D=yes`.
- **#29 ClamAV scanning, built 2026-10-30, off until `MF_CLAMAV` is set**: our own client for
  clamd's documented `INSTREAM` protocol (`scan.rs`); every 3D upload's files and images (uploads,
  imports' drafts, files added to a model) are scanned before they're kept: a flagged one is
  refused by name; if clamd can't be reached, uploads are held, never kept unscanned. Tested
  against a stand-in clamd. **The owner's call**: running a ClamAV service on Railway (it costs).
- **#28 Thingiverse import, built 2026-10-30, off until the app settings are set**: through
  Thingiverse's official API and OAuth (`model3d/thingiverse.rs`): the creator signs in there
  (a one-time state, 10 minutes, this account only), `/users/me` names them, and only things
  whose creator is them are brought into an ordinary import, one model a thing, with its licence
  when it maps without guessing (CC, CC0, BSD, all rights reserved; GPL left for the creator),
  its date and its page as "Originally published on". The token is never stored. Tested against
  a stand-in Thingiverse. **The owner's steps**: check Thingiverse's API terms allow this, register
  the app, set `MF_THINGIVERSE_CLIENT_ID` and `MF_THINGIVERSE_CLIENT_SECRET`; the answers' field
  names are then checked against the live API.
- **#30 showcase posting, built 2026-10-30, off until the Page is connected**: with the Page's id
  and token (and the Instagram account's), `/admin/showcase` gets "Post to Facebook" and "Post to
  Instagram" (Meta's Graph API: a Page photo; Instagram's container then publish), only for
  active consents of published items, the caption crediting only the display name. A creator's
  withdrawal deletes their Facebook posts through the API; Instagram's API can't delete, so those
  stay flagged for the admins. YouTube takes video, so it stays by hand. Tested against a
  stand-in Graph API. **The owner's steps**: a Meta app with page-posting permission (Meta's app
  review), then the three settings.
- **#31 email notices, built 2026-10-30, off until a sending key**: with `MF_RESEND_KEY`, Account ›
  Email notices lets each member turn on a daily digest (off unless they do) of comments and makes
  on their designs, remixes of their models (once the remix is published) and new followers.
  Notices are noted only for members who turned it on, a remix or a follow once, never their own
  doing; a digest goes at most once a day and names people by maker name only. Every email has a
  one-click unsubscribe (RFC 8058 headers, and a link with an HMAC token that works without
  signing in; looking at the link doesn't unsubscribe). Turning it off deletes waiting notices;
  sent ones go after 30 days; both are in the data download and deleted with the data. Tested
  against a stand-in Resend. **The owner's steps**: a Resend account, the sending domain verified,
  `MF_RESEND_KEY` set; the Privacy Policy then names Resend as a processor.
- **#32 STEP previews, built 2026-10-30, off until a converter service exists**: with
  `MF_STEP_CONVERTER`, each uploaded or imported STEP file is sent to the service, whose STL
  answer is read by our own STL reader with its limits and drawn like any mesh (pictures, views,
  3D view, size). The STEP file is still kept as it came and only downloaded. A preview that can't
  be made (service down, an error, an STL that fails our reader) never fails the upload: the file
  just has no preview, as before. Tested against a stand-in converter. **The owner's steps**:
  decide whether it's worth running (cost), then deploy a converter as its own Railway service
  (OpenCASCADE is LGPL: as a separate program that's fine; never linked or copied), and set
  `MF_STEP_CONVERTER` (and a key).
- **#36 public API for 3D models, built 2026-10-30**: `GET /api/v1/models` (the `/3d` search,
  paged by key) and `GET /api/v1/models/{id}` (details, files, print profiles), published models
  only, with keys members make at Account › API keys (up to 5, shown once, kept hashed,
  revocable) and 1,000 requests an hour a key; only admins' keys answer until `MF_3D=yes`. In the
  data download and deleted with the data. Documented in `docs/api.md`. Publishing from slicers is
  for later.
- **#35 parametric models, built 2026-10-30, off until a render service exists**: a model's `.scad`
  file's Customizer parameters are read by our own reader of the syntax in OpenSCAD's manual
  (numbers with ranges, dropdowns, checkboxes, text, vectors, groups; hidden ones left out; nothing
  but literals), shown as a form at `/d/{id}/customize/{file}` for anyone who may download the
  model; signed-in members (30 an hour) get an STL made by the owner's sandboxed OpenSCAD service
  from values each checked against its declaration, read again by our own STL reader before it's
  sent. Tested against a stand-in service. **The owner's steps**: decide on cost; deploy OpenSCAD
  as its own Railway service (GPL: a separate program, never linked or copied; caps on time,
  memory and output; no network), set `MF_SCAD_SERVICE` (and a key); check it's fast and reliable
  before switching on, as the issue asks.
- **#33 contests, built 2026-10-30, nothing shows until an admin opens one**: admins make a
  contest at `/admin/contests` (title, theme, closing date, judged by the admins or by members'
  votes, and its rules); it opens only with rules text and a closing date ahead. Creators enter up
  to 3 of their own published 3D models, ticking that they made them and agree to the rules, and
  can withdraw while it's open. Members get one vote a contest, never for their own entry,
  changeable until it closes; counts show only after. Once closed, admins give up to three places
  and announce them; winners lead the contest page. Contests show under `/3d` and `/contests`
  while MakerFoundry 3D is open. In the data download, deleted with the data. **The owner's
  steps**: prizes and each contest's rules (legal wording: `docs/legal/contest-rules-draft.md` is a
  starting point), then an admin opens one.
- **#34 points, built 2026-10-30, off unless `MF_POINTS=yes`, with no rewards**: points are
  counted afresh from what others do with a member's work, never awarded for an action itself, so
  re-uploads and self-made activity earn nothing and anything taken down or unpublished stops
  counting: 10 for each published model someone else has made or 3 others have liked, 3 for each
  member who's made one of their models, 2 for each of someone else's models they've made (up to
  20 a month). Shown at Account › Your points and on maker profiles. **The owner's steps**:
  decide whether MakerFoundry wants points at all, what (if anything) they're exchanged for and
  the budget; then set `MF_POINTS=yes`.

## Ordering from shops (MF-80 to MF-86, issues #1 and #2)
(Numbered MF-80 to MF-86 on 2026-10-31; their commits say MF-60 to MF-66, which clashed with
numbers used on `main` for the mark, the home page and the footer.)
The owner's decisions of 2026-10-31: customers order physical goods from shops through
MakerFoundry, live (no demo); MakerFoundry takes the payment and keeps 10%; the shop pays the card
fee; orders travel through StitchHub (`docs/stitchshop-ordering-audit.md`). Built switched off
until `MF_STITCHHUB_URL` and `MF_STITCHHUB_KEY` are set, against a stand-in hub, on the message
format proposed in `docs/shop-orders-contract.md` (StitchHub owns it: stitchhub#19 and #34;
StitchShop's side is stitchbench#74).
- **MF-80 the messages, built 2026-10-31**: `src/shop_orders/contract.rs` reads and writes every
  message type, checking each field against a limit (shops' messages are untrusted); 7 valid and 25
  invalid fixtures in `docs/contract/shop-orders/`, each invalid one refused for its own reason;
  property tests with random and damaged messages.
- **MF-81 the StitchHub client, built 2026-10-31** (`src/shop_orders/hub.rs`, tables in
  `0038_stitchhub.sql`): the `/v2/hello` handshake every round (too old: sending stops, flagged
  for the admins; deprecations logged once a day); the outbox, saved in the same transaction as the
  change it reports, sent in order, tried again with growing waits (up to an hour) when the hub is
  down, a refusal final, a message that expires unsent reported as undelivered; inbound messages
  checked, applied once (by id) and in order (by `seq` per order and sender, later ones held for
  the earlier, a gap over 24 hours logged), then acknowledged; unreadable ones acknowledged and
  logged by id only. Tested against a stand-in hub (`tests/stitchhub.rs`): down, sends failing,
  refusals, too old, expiry, out-of-order and repeated messages; removing the ordering or the
  de-duplication fails them. Not run by the sweep until MF-82.
- **MF-82 shop catalogues and pages, built 2026-10-31**: the sweep talks to the hub every minute
  (when `MF_STITCHHUB_URL` and `MF_STITCHHUB_KEY` are set); each shop's `catalog` is kept as it
  came, checked; `/shops` lists the shops that can be paid (`selling::can_sell` on the shop's
  Foundry Account) and `/shops/<handle>` shows their products: colours, sizes, methods and
  placements, price breaks, set-up and shipping. A shop's text is escaped like everyone's.
  `message.expired` now comes from the `hub` party (fixture updated).
- **MF-83 ordering and checkout, built 2026-10-31** (`src/shop_orders/orders.rs`, tables in
  `migrations/0040_shop_orders.sql`): `/shops/<handle>/order/<sku>` (signed in) takes a colour,
  quantities by size, a decoration and placement, the art (an embroidery file for embroidery, a
  PNG or JPEG for printing; 10 MB, read with the same limits as uploads), a name, pick-up or a
  shipping address and the rights checkbox. The price comes from the shop's catalogue only (the
  price break for the whole order, set-up once, flat shipping); MakerFoundry's 10% and the card
  fee (2.9% + 30¢) are each rounded to the cent and the shop gets the rest. `/shop-orders/<ref>`
  shows the order; paying is Stripe Checkout with the 10% and the card fee as the application fee,
  the rest to the shop's Stripe account (a second click reuses the open session). Once the
  verified webhook or the return from Checkout shows a paid session labelled with this order, its
  amount and currency, the order is paid, three `mf_shop_ledger` rows are written and
  `order.placed` is queued for the shop, in one transaction, once. (The art goes through the hub
  since #53, below.) (Sales tax:
  MF-86.)
- **MF-84 after payment, built 2026-10-31** (`src/shop_orders/after.rs`, tables in
  `migrations/0041_shop_order_updates.sql`): the shop's `order.status` (with its note and
  tracking), `proof.sent` (the mock-up fetched from its signed link, checked by size and SHA-256,
  re-drawn and kept; shown only to the customer) and `order.cancel` are applied to the order when
  it's that shop's; anything else is logged by id and left. The customer approves the latest proof
  or asks for changes (with a note), or cancels before production; each sends its message in the
  same transaction. Refunds, as for designs: the payment back with the transfer reversed, then the
  10% back to the shop (the shop is out only the card fee), each recorded once in
  `mf_shop_ledger`; due when either side cancels before production or the hub gives up on
  `order.placed` (`message.expired`); after production a shop's cancel refunds nothing (Seller
  Terms). The minute sweep retries due refunds, gives up unpaid orders after 2 days (checkout
  closes after 20 hours) and deletes art and mock-ups 30 days after an order closes. Data export
  lists orders from shops; deleting the data removes the name, address, notes, art and proofs and
  keeps the amounts.
- **Stripe hardening, built 2026-11-05** (issue #91, the Stripe audit's fixes 3-6; `src/refunds.rs`,
  `migrations/0049_stripe_hardening.sql`; waiting for the owner's review): card statements carry
  the suffix `MAKERFOUNDRY`; new sellers' payouts wait `MF_PAYOUT_DELAY_DAYS` (14) and an admin
  task sets it on existing sellers; refunds made in the Stripe Dashboard are recorded (partial
  ones too), the seller's transfer reversed if Stripe didn't, the 10% given back in proportion;
  `account.updated` is applied only if newer, and Stripe's `disabled_reason` is shown to the
  seller and the admins.
- **Chargebacks, built 2026-11-05** (issue #90, from the Stripe Connect audit; `src/disputes.rs`,
  `migrations/0048_disputes.sql`; waiting for the owner's review, payments code):
  `charge.dispute.created` makes the order `disputed` (no downloads), takes the disputed price back
  from the seller's transfer and gives MakerFoundry's 10% of it back, as a refund does;
  `charge.dispute.closed` won (or an inquiry closed) pays the order again and transfers back what
  was taken, lost makes it a `chargeback`. Each Stripe step is checked with Stripe and idempotent,
  under a per-order lock, retried by the 10-minute sweep; a refused step never fails the webhook,
  it alerts the admins once. The seller is emailed at each step. Shop orders' disputes are
  alerted only. Seller-terms wording and evidence are out of scope (attorney item #8).
- **No program passes as a design, built 2026-11-04** (issue #86, found by the stitch-e2e
  staging deep probes; `src/designs.rs`, `src/embroidery.rs`): a design file's name must end in a
  design format's ending (DST, PES, PEC, JEF, EXP, VP3, XXX, U01, PCS, TAP, DSB, DSZ, SVG, DXF,
  PDF) or it is refused before it is read; files that begin as programs, archives, scripts or web
  pages (`MZ`, ELF, `PK`, `%PDF`, `<!`, `#!`…) are never read as embroidery; headerless EXP and
  TAP files must be whole records with mostly short, moving stitches. The kept name, and every
  download, open-in and other-format file name, ends in the ending of the format the file was
  read as, never the one it was uploaded with (files kept earlier are served the same way).
- **The customer is told what the shop reports, built 2026-11-04** (issue #82, found by the
  stitch-e2e staging release suite; `src/shop_orders/mail.rs`,
  `migrations/0047_shop_order_mails.sql`): each `order.status` applied notes one email by its hub
  message id (so a message applied twice is mailed once); the minute sweep sends it to the
  customer through the notices' sender (Resend, or staging's Mailpit) with a link to the order on
  `MF_PUBLIC_URL`, tries 5 times, and never holds up the order's messages. No unsubscribe link: it
  is about the customer's own order. The order page says "Being made (in production)" for
  `in_production`, in the shop's word.
- **MF-85 shipping by the piece, built 2026-10-31** (the owner's decision): the catalogue's
  `shipping` may add `per_extra_piece_cents` and `free_over_cents` (both optional, so v1
  catalogues still read), and `shop.address` gives the pick-up address for sales tax. The form
  and the shop's page say the shipping in words.
- **MF-86 sales tax on orders from shops, built 2026-10-31** (the owner's decision: Stripe Tax,
  MakerFoundry collecting as the marketplace; `src/shop_orders/tax.rs`,
  `migrations/0042_shop_order_tax.sql`): on with `MF_STRIPE_TAX`, like designs. The tax is worked
  out before checkout (Stripe Tax calculations, under `MF_SHOP_TAX_CODE`, default
  `txcd_30011000` Clothing & Footwear) from the ship-to address, or the shop's for pick-up (a shop
  without one can only ship), and shown on the order. It's paid to MakerFoundry inside the
  application fee and is outside the shop's share (10% of the price before tax, the card fee of
  all that's paid; a `tax` ledger row). Once paid it's recorded as a Stripe tax transaction; a
  refund reverses it, takes the price back from the shop and gives the shop the 10% and the tax
  it paid over, so the shop is still out only the card fee. Confirm the tax code and
  registrations with an accountant before the first registration.
- **StitchHub's form, built 2026-11-01** (issue #53; StitchHub's message of 2026-11-01): the
  client now speaks the form StitchHub implements. `Authorization: Bearer svk_…` and `X-Client`;
  `GET` then `POST /v2/hello` with the types read (again on `409 hello_first`); `GET /v2/shops`,
  a shop offered only while `ready` (`mf_shops.hub_ready`, `0039_shops.sql`); the envelope's
  `conversation` (`catalog` for catalogues) in place of `order`, `from` left to the hub,
  `created_at` stamped again after a wait; `order.placed` signed with Ed25519
  (`src/shop_orders/sign.rs`, `MF_STITCHHUB_SIGNING_KEY`), with `fulfilment` and no `art_urls`;
  the art put in the hub first (`/v2/files/put`, PUT, `/v2/files/done`; `mf_hub_files`) and
  MakerFoundry's own art address removed; mock-ups fetched with `/v2/files/get`; `202`
  duplicates, `id_reused` counted as delivered, `seq_taken` final; the inbox polled until empty;
  `message.expired` with `conversation` and `reason`; an `order.placed` the hub refuses is
  refunded like an expired one; email addresses and phone numbers refused in the order's name and
  note. Fixtures converted (7 valid, 34 invalid); `docs/shop-orders-contract.md` rewritten.
- **Fixes from the code review, 2026-11-01** (`migrations/0044_shop_order_review.sql`), before
  ordering opens:
  - a shop is offered only once the Foundry Account its catalogue names has confirmed the shop on
    its account page (a catalogue naming another account resets it);
  - a shop's status never takes an order back (except between proofs), and doesn't overwrite a
    cancel that came in meanwhile;
  - a held message is taken out of the held ones only once applied, and retried each round;
  - a payment its order can't take (deleted, paid through another checkout, another amount) is
    recorded and refunded in full by the sweep; no second checkout is made while Stripe can't say
    whether the first is still open;
  - deleting your data waits while an order from a shop is in progress, and blanks the hub's copies
    of finished orders' messages; an erased order still loads;
  - refunds that keep failing don't hold up the rest; seqs are taken one at a time per
    conversation; the sweep runs every minute whether or not StitchHub is on.
- **A test deploy's own name on the hub, built 2026-11-01** (StitchHub's request): the name
  MakerFoundry signs, sends and takes as comes from `MF_STITCHHUB_SERVICE` (default
  `service:makerfoundry`), so the test deploy, with its own hub key, is
  `service:makerfoundry-test` and never shares production's queue (`tests/stitchhub_service.rs`).


## ScreenFoundry (MF-87, 2026-10-31)
(Numbered MF-87 and MF-88 on 2026-10-31; their commits, and the comment in
`migrations/0043_design_app.sql`, say MF-67 and MF-68, which clashed with numbers used on `main`.
A migration that has run in production is never edited, not even its comments: sqlx checks each
one's checksum when it starts.)
- **MF-87 Open in ScreenFoundry, built 2026-10-31**: SVG designs get an **Open in ScreenFoundry**
  button (behind `SCREENFOUNDRY_OPENS_LINKS` in `src/api.rs`; **on since 2026-11-01**, when
  ScreenFoundry's web app at app.screenfoundry.app opened links, screenfoundry#28), linking to
  `https://app.screenfoundry.app/?open=<the file's address>`; paid designs through
  `/d/<id>/open?app=screenfoundry`. A design's file name in its links and download is its title's
  slug or `design-<id>`, so every address meets ScreenFoundry's rules (`docs/api.md`, tested with
  random titles). A draft from an app is titled after its file. Several Open in buttons share one
  row. Proposed, not built: a "screen printing" kind for films (see the ScreenFoundry note in
  `docs/api.md`).
- **MF-88 the app a design came from, built 2026-10-31** (the owner's decision): a design sent
  through the API keeps the app's name without its version (`mf_designs.from_app`,
  `migrations/0043_design_app.sql`); admins see how many each app sent and list them at
  `/admin/apps` (linked from the overview); it's in the maker's data download. Uploads on the site
  have none.

## PatternFoundry (MF-89, 2026-11-01)
- **MF-89 Open in PatternFoundry, built 2026-11-01** (issue #47's third item): SVG designs get an
  **Open in PatternFoundry** button linking to
  `https://sitesentrix.github.io/patternfoundry/?open=<the file's address>` (`PATTERNFOUNDRY_OPENS_LINKS`,
  on: its live web app opened a served SVG in Chromium). The web apps' switches are one `Switches`
  value in `src/api.rs`. Still open on #47: the sewing-pattern kind.

## Open decisions (the owner's)
- ~~**The platform fee**~~: **decided 2026-10-19: 10% of each sale, the card fee (2.9% + 30¢)
  also from the seller's share, payouts monthly** (research: `docs/research/marketplace-fees.md`).
- ~~**The price range**~~: **decided 2026-10-22: $1 to $200.**
- ~~**Refunds and the card fee**~~: **decided 2026-10-22: the seller gets the 10% fee back and
  bears the card fee Stripe keeps** (Seller Terms § 7; built as MF-23).
- ~~**Who may sell**~~: **decided 2026-10-22: anyone with an account (Stripe checks identity).**
- **Sales tax on digital goods**: some US states tax them and treat marketplaces as the seller
  ("marketplace facilitator" laws). Likely answer: Stripe Tax on checkout. Needs the owner's
  decision (and ideally an accountant's word) before paid sales go live. **The launch is free-only
  (owner, 2026-10-22)**: Seller Terms § 5 says paid sales aren't open yet until this is decided.
  **The code is ready (MF-24, 2026-10-27)**: Stripe Tax on checkout, switched on with
  `MF_STRIPE_TAX=yes` once the owner has registered and updated Seller Terms § 5.
- ~~**DMCA designated agent**~~: **registered 2026-11-01** with the US Copyright Office
  (DMCA-1084865, renew before November 2029); shown on `/dmca` from `MF_DMCA_AGENT`.
- ~~**Accounts**~~: **decided 2026-10-19: the StitchFoundry cloud accounts** (see "Accounts (MF-1)").
- **Domain**: makerfoundry.app (and patternfoundry.app held for the sewing-pattern app).
