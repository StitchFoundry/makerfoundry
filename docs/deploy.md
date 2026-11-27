# Deploying MakerFoundry on Railway

MakerFoundry runs on Railway (the owner's Pro plan) as one service built from this repository's
`Dockerfile`. `railway.toml` tells Railway to build with that Dockerfile and to check `/health`
before switching traffic to a new deploy. Nothing secret is in the repository: settings are
Railway **Variables**.

## Where it runs
- Railway project **MakerFoundry**, environment `production`, service `makerfoundry`, deploying
  `main` of `SiteSentrix/makerfoundry`.
- Address: <https://makerfoundry-production.up.railway.app> (first deployed 2026-10-19).
- `/health` answers `ok <version> <commit>` (the commit from Railway's `RAILWAY_GIT_COMMIT_SHA`),
  so `curl https://makerfoundry.app/health` shows which commit is live.
- MakerFoundry has no code signing (a server built by Docker on Railway: no GitHub `environment:`,
  no AZURE_* secrets); the desktop apps sign in their own repository's `release` environment.
- Service `Postgres` (image `postgres:16`, a volume at `/var/lib/postgresql/data`, made
  2026-10-19 for MF-1); the site reads it through `DATABASE_URL` = `${{Postgres.DATABASE_URL}}`
  over Railway's private network. Its password is a Railway variable of that service, generated
  when it was made and never written anywhere else. **Backups:** turn them on in the Postgres
  service's settings (Railway › Postgres › Backups).
- **makerfoundry.app** (bought 2026-10-19; DNS on Cloudflare) and **www.makerfoundry.app** were
  added to the service on 2026-10-19. They answer once these records are in Cloudflare's DNS for
  makerfoundry.app (each **DNS only**, the grey cloud, so Railway can issue the certificate):

  | Type | Name | Content |
  |---|---|---|
  | CNAME | `@` | `ia5fx6io.up.railway.app` |
  | TXT | `_railway-verify` | `railway-verify=2a9a13d59ed3a8efdd77e42fe0504484a5402ea13716bb305a6e7a39d2e9a7b0` |
  | CNAME | `www` | `or0wbw4h.up.railway.app` |
  | TXT | `_railway-verify.www` | `railway-verify=b0e0b032fbbc0ec44f91abba6e8d2e3e05e1528afb7d8118278525fa990049cb` |

- Claude sessions reach this one project through a Railway **project token** in the cloud
  environment's `RAILWAY_TOKEN` variable (Railway's GraphQL API, header `Project-Access-Token`).
  The token can't see the owner's other projects. To revoke it: the project's **Settings → Tokens**.

## First deploy (MF-0)

Done on 2026-10-19 through the API with the project token (the service, `PORT`, the domain). The
steps below are the same thing by hand, for a new project or if it ever needs redoing.

Do these once, in a web browser (the phone works too, but a PC is easier).

1. Go to <https://railway.com> and sign in.
2. Click **New** (top right of the dashboard), then **Project**, then **Deploy from GitHub repo**.
   *Or*, to keep it with your other projects, open an existing project and click **Create** →
   **GitHub Repo**. A project of its own is tidier: call it `makerfoundry`.
3. Pick **sitesentrix/makerfoundry**.
   - If it isn't in the list: click **Configure GitHub App**, choose the **sitesentrix**
     account/organisation, allow access to the `makerfoundry` repository, save, and come back.
4. Railway starts building straight away. The first build takes about 3–5 minutes (it compiles
   the Rust code). Wait for the deploy to show **Active** (green).
5. Click the new service (the box named `makerfoundry`), open the **Variables** tab, click
   **New Variable**, and add:
   - `PORT` = `8080`

   Railway redeploys by itself after a variable changes.
6. Open the **Settings** tab, scroll to **Networking**, and click **Generate Domain**. If it asks
   for a port, type `8080`. You get an address like `makerfoundry-production-xxxx.up.railway.app`.
7. Open `https://<that address>/health` in the browser. It should show `ok 0.1.0`.
   Then open `https://<that address>/` to see the home page.
8. Still in **Settings**, under **Deploy** (or **Source**), turn on **Wait for CI**, so a commit
   only deploys after GitHub's checks pass. Leave the branch as `main`.

Then send the `*.up.railway.app` address back in the chat: MF-0 is done when that page answers.

## After that
- Every push to `main` that passes CI deploys by itself. A failed health check keeps the old
  version running.
- **Logs**: the service's **Deployments** tab → the deploy → **View logs**.
- **Roll back**: the **Deployments** tab → an earlier deploy → **⋮** → **Redeploy**.

## Coming in later tasks
- **MF-1** (accounts): a Postgres database in the same project (**Create** → **Database** →
  **PostgreSQL**), and on the `makerfoundry` service the variable
  `DATABASE_URL` = `${{Postgres.DATABASE_URL}}`. Accounts and their emails (password resets) are
  StitchFoundry's account service's, so MakerFoundry needs no email key of its own.
- **MF-2**: Cloudflare R2 keys for file storage.
- **MF-6**: the `makerfoundry.app` domain (the owner buys it) pointed at the service through
  Cloudflare.

## Stripe (selling)
MakerFoundry uses **StitchFoundry's Stripe account** (owner's decision, 2026-10-20): one set of
books and one name on card statements for both. What keeps the two apart:

- **Its own restricted key**, not StitchFoundry's secret key. In the Stripe Dashboard (test mode
  first): **Developers › API keys › Create restricted key**, named `makerfoundry`, with only these
  permissions, everything else **None**:
  - **Checkout Sessions:** Write (makes and reads a buyer's checkout page)
  - **Refunds:** Write (an admin's full refund)
  - **PaymentIntents:** Read and **Charges:** Read (a refunded sale's application fee)
  - **Connect › Application Fees:** Write (MakerFoundry's 10% given back to the seller on a refund)
  - **Connect › Accounts:** Write (sellers' Express accounts, their set-up and dashboard links)
  - **Connect › Account Links:** Write (the "Set up payouts with Stripe" onboarding link; without
    it a seller sees "Stripe can't be reached just now" and the log says `Stripe refused (403)`,
    issue #76)
  - **Connect › Transfers:** Write (the tax taken back from the seller's transfer with sales tax
    on, MF-24, and a taxed sale's price before its refund; for a dispute, issue #90, the disputed
    price taken back and, if MakerFoundry wins, given back)
  - **Disputes:** Read (issue #90; Write only if MakerFoundry ever submits evidence through the API)

  It goes in `STRIPE_SECRET_KEY` (`rk_test_…`, later `rk_live_…`). If it ever leaked, it couldn't
  touch StitchFoundry's products, prices or payouts settings, and it can be rolled on its own.
- **Its own webhook endpoints**, each with its own signing secret, at
  `https://makerfoundry.app/stripe/webhook` (below). StitchFoundry's endpoint stays as it is.
- **Each side ignores the other's payments.** Both endpoints receive every event of the account.
  MakerFoundry labels its checkouts `metadata[app]=makerfoundry` and acts only on those, and only
  for an order of its own at its amount; StitchFoundry adds credits only for checkouts carrying its
  `account` and `credits` labels, which MakerFoundry's never have. Tested both ways
  (`a_shared_stripe_accounts_other_payments_are_left_alone`).
- **Connect** must be on in the account: Marketplace, **Express** accounts, US; the platform's
  name and icon are what sellers see while setting up.
- Everything travels over HTTPS; the keys and secrets live only in Railway's variables.

### Disputes (chargebacks, issue #90)
A sale is a destination charge with no `on_behalf_of`, so MakerFoundry is the merchant of record:
Stripe takes a disputed payment and its dispute fee from MakerFoundry's balance, not the seller's.
What MakerFoundry does about it, from the webhook's `charge.dispute.created` and
`charge.dispute.closed` (both must be on the account endpoint above):
- **Created:** the order is `disputed` and the buyer can't download it; the disputed price is
  taken back from the seller's transfer (a transfer reversal), then MakerFoundry's 10% of it goes
  back to the seller, as on a refund (the seller is out the card fee). The seller is emailed.
- **Closed, won** (or an inquiry closed): the order is paid again and what was taken goes back to
  the seller by a transfer. **Lost:** the order is a `chargeback` and stays blocked. The seller is
  emailed either way.
- Each Stripe step is checked with Stripe first and sent with an idempotency key, so none is ever
  made twice; if Stripe refuses one (the seller's balance too low, say), the webhook still answers,
  the log says `ALERT selling: dispute of order …`, the admins are emailed once (with email
  notices on) and `/admin/orders` shows the dispute until the 10-minute sweep has settled it.
- Stripe's dispute fee stays with MakerFoundry (it isn't passed on to the seller). Evidence is answered
  in the Stripe Dashboard (**Payments › Disputes**). A dispute on an order from a shop is only
  logged and alerted, to settle by hand.

### Refunds, payouts and statements (issue #91)
- **Refund from the admin's Orders page** whenever possible. If a refund is made in the Stripe
  Dashboard instead, **tick "Reverse transfer"** and leave "Refund application fee" unticked.
  If "Reverse transfer" was missed, MakerFoundry reverses the refunded price from the seller's
  transfer itself when `charge.refunded` arrives (checked with Stripe, once; retried by the sweep;
  admins alerted if Stripe refuses). Partial refunds are recorded (the sale stays paid) and
  MakerFoundry's 10% of the refunded part goes back to the seller.
- **Payout delay:** new sellers' Express accounts hold each sale `MF_PAYOUT_DELAY_DAYS` days
  (default 14) before the monthly payout, so most refunds and disputes come while the money is
  still in the seller's Stripe balance. For sellers set up before this, an admin presses **Set the
  payout delay on every seller's Stripe account** on `/admin/orders` once (it only changes the ones
  not yet set).
- **Card statements** show the Stripe account's name plus the suffix `MAKERFOUNDRY`
  (`statement_descriptor_suffix`), since the account is shared with StitchFoundry. Check the
  account's **shortened descriptor** (Settings > Business > Public details) so the two together fit
  in 22 characters.
- **account.updated** is applied only if it's newer than the last one applied (Stripe doesn't send
  events in order); why Stripe paused a seller (`requirements.disabled_reason`) is shown to the
  seller on `/account/selling` and to admins on `/admin/orders`.

## Sales tax (MF-24)
Off until `MF_STRIPE_TAX=yes`. MakerFoundry is the marketplace, so it collects and pays the tax
(https://docs.stripe.com/tax/tax-for-marketplaces), never the seller:
1. **Stripe Tax on** in the Stripe Dashboard (**Tax › Get started**): the business address (the
   origin), the default product tax code, and prices **exclusive** of tax.
2. **Registrations** (**Tax › Registrations**): add each state MakerFoundry is registered in with
   the state itself (Rhode Island first). Stripe only collects where a registration is active, and
   its monitoring says when sales in another state come near that state's threshold.
3. **Seller Terms § 5** says how tax works, in place of "Paid sales aren't open yet" (the owner's
   wording, ideally with an accountant's word), before `MF_STRIPE_TAX=yes`.
4. **The restricted key** gets **Connect › Transfers: Write** (above).

What it does: the checkout asks Stripe Tax for the tax (`automatic_tax`, MakerFoundry liable,
the buyer's billing address required, prices exclusive of tax); the buyer pays the price plus the
tax, and the seller's share is worked out from the price alone. A destination charge moves the
whole payment to the seller's account, so once the sale is paid the tax is taken back with a
transfer reversal (from the webhook, the buyer's return, and the 10-minute sweep until it's done;
checked with Stripe first, so never twice). The ledger keeps the tax as MakerFoundry's (`tax`
rows); a refund gives the buyer everything back, the tax too, taking the price back from the
seller first. Stripe records the tax for its reports; filing it with the states is the owner's
(or Stripe's filing add-on).

## Shops' short addresses (`<shop>.makerfoundry.app`)
Branded shops (2026-11-02) answer at `<handle>.makerfoundry.app` once, and only once, the wildcard
domain is set up; until then their pages are at `makerfoundry.app/shops/<id>` as before. The
owner's steps, once:
1. Railway › MakerFoundry › production › makerfoundry › Settings › Networking › Custom domain:
   add `*.makerfoundry.app` (port 8080, as the main domain).
2. Railway shows the DNS records it needs: put them in Cloudflare › makerfoundry.app › DNS exactly
   as shown (a `CNAME` for `*`, and the `_acme-challenge` record it asks for so it can issue the
   wildcard certificate), with the proxy **off** (grey cloud), as for the main domain.
3. Wait for Railway to show the domain as verified, then open a shop's short address.

The service needs nothing else: it answers any `<label>.makerfoundry.app` from `MF_PUBLIC_URL`'s
host, sends `www.` and every other path to the main address (signing in and checking out stay on
`makerfoundry.app`), and says "not found" for an address no shop has. The sign-in cookie is a
`__Host-` cookie, so it's never sent to a shop's address.

## Settings the service reads
| Variable | Needed | What |
|---|---|---|
| `PORT` | no (default `8080`) | the port to listen on; Railway sets it |
| `DATABASE_URL` | for accounts | MakerFoundry's Postgres, `${{Postgres.DATABASE_URL}}` on Railway; tables are made when the service starts. Without it the site runs but nobody can sign in |
| `R2_ACCOUNT_ID`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`, `R2_BUCKET` | to keep designs | Cloudflare R2: the account's id, an API token's keys (Object Read & Write on the bucket only), and the bucket (`makerfoundry-files`). Without them designs can be previewed but not kept |
| `MF_ADMINS` | for the report queue | account ids apart by commas: who sees `/admin` and can take designs down |
| `MF_DMCA_AGENT` | before launch | the designated agent registered with the US Copyright Office, as shown on `/dmca`: lines apart by `|` (name, address, email, phone) |
| `MF_API_ORIGINS` | no (default `https://stitchfoundry.app,https://stitchwright.github.io`) | the web apps' sites that may call the apps' API from a browser (`docs/api.md`), apart by commas |
| `MF_LAUNCHED` | at the launch | `yes` opens MakerFoundry to everyone (`docs/launch.md`), but only once the legal pages have none of the owner's markers left; until then the log says what's left |
| `MF_PUBLISHERS` | no (nobody; everyone once launched) | who may publish designs until the launch: account ids apart by commas, or `*` for everyone |
| `STRIPE_SECRET_KEY` | to sell | MakerFoundry's restricted key in StitchFoundry's Stripe account (`rk_test_…` until launch; see "Stripe" above). Without it every design is free |
| `STRIPE_WEBHOOK_SECRET` | with the key | the signing secret (`whsec_…`) of the webhook endpoint `https://makerfoundry.app/stripe/webhook` listening to **your account's** events `checkout.session.completed`, `checkout.session.async_payment_succeeded`, `checkout.session.expired`, `charge.refunded`, `charge.dispute.created`, `charge.dispute.closed` (the last two: chargebacks, issue #90, "Disputes" below) |
| `STRIPE_CONNECT_WEBHOOK_SECRET` | with the key | the signing secret of a second endpoint at the same address listening to **connected accounts'** event `account.updated` (sellers' set-up finishing) |
| `MF_PAYOUT_DELAY_DAYS` | no (14) | days each sale is held before a seller's monthly payout, 2 to 90 (issue #91, "Refunds, payouts and statements" below) |
| `MF_STRIPE_TAX` | no (off) | `yes` charges sales tax with Stripe Tax (MF-24, "Sales tax" below) |
| `MF_TAX_CODE` | no (`txcd_10505001`) | the Stripe tax code designs are sold under: Digital Finished Artwork, downloaded, with permanent rights |
| `MF_SHOP_TAX_CODE` | no (`txcd_30011000`) | the Stripe tax code goods ordered from shops are sold under (MF-86): Clothing & Footwear |
| `MF_PROOF_KEY` | for approval links | the key proofs' customer links are made with (MF-25): 64 hex digits, `openssl rand -hex 32`. A secret: whoever has it can open any proof. Without it there are no proofs. A new key ends every customer link (shops then send their links again) |
| `MF_PROOFS` | no (admins only) | `yes` lets every account send proofs (their wording is in the Terms § 9 and the Privacy Policy since 2026-10-30) |
| `MF_THINGIVERSE_CLIENT_ID`, `MF_THINGIVERSE_CLIENT_SECRET` | no (off) | a Thingiverse app's id and secret (registered by the owner at thingiverse.com/developers, its callback `https://makerfoundry.app/3d/import/thingiverse/back`): lets creators bring in their own Thingiverse things. The secret is a secret |
| `MF_FACEBOOK_PAGE_ID`, `MF_FACEBOOK_PAGE_TOKEN`, `MF_INSTAGRAM_USER_ID` | no (posting by hand) | MakerFoundry's Facebook Page id and a Page access token with permission to post (and the linked Instagram professional account's id): admins then post showcase items with one button, and a creator's withdrawal deletes the Facebook posts. The token is a secret |
| `MF_RESEND_KEY` (and `MF_NOTICE_FROM`) | no (no email notices) | A Resend API key (<https://resend.com/docs/api-reference/emails/send-email>) for email notices: members can then turn on a daily digest (Account › Email notices). `MF_NOTICE_FROM` is the sender (default `MakerFoundry <notices@makerfoundry.app>`; the domain must be verified at Resend). The key is a secret |
| `MF_MAIL_CATCHER_URL` | **staging only** | a Mailpit test mailbox's address (`http://mailpit.railway.internal:8025`, or `http://user:password@host:port` when its send API has a password): notices are on and every one goes there instead of Resend, never with Resend's key (MF-99). `MF_RESEND_KEY`, if set, only makes the unsubscribe links. Never set in production |
| `MF_STEP_CONVERTER` (and `MF_STEP_CONVERTER_KEY`) | no (STEP files have no preview) | The address of a STEP-to-STL converter service (a separate program, e.g. one built on OpenCASCADE, never linked into ours), answering `POST /convert` with a binary STL (contract in `src/model3d/step.rs`): STEP files then get pictures and a 3D view. The key, if set, is sent as a bearer token and is a secret |
| `MF_SCAD_SERVICE` (and `MF_SCAD_SERVICE_KEY`) | no (no customizing) | The address of a sandboxed OpenSCAD render service (its own Railway service: OpenSCAD is GPL and runs there as a separate program, with time, memory and output caps and no network), answering `POST /render` with a binary STL (contract in `src/model3d/scad.rs`): free models' `.scad` files then get a Customize page. The key, if set, is sent as a bearer token and is a secret |
| `MF_POINTS` | no (no points) | `yes` shows members' points (Account › Your points, maker profiles); there are no rewards until the owner decides on them |
| `MF_STITCHHUB_URL`, `MF_STITCHHUB_KEY`, `MF_STITCHHUB_SIGNING_KEY` | no (no ordering from shops) | StitchHub's address, the service key its admin issues to MakerFoundry (`svk_…`), and MakerFoundry's Ed25519 key for signing orders (base64 PKCS#8 DER): customers can then order from shops that opted in (`docs/shop-orders-contract.md`); MakerFoundry talks to the hub every minute. Make the signing key on your own computer with `openssl genpkey -algorithm ed25519 -outform DER \| base64` and paste it into Railway only; its public half, for the hub admin to register, is in the deploy log ("stitchhub: on; MakerFoundry's public signing key: …"), or `openssl pkey -inform DER -pubout -outform DER \| base64` on the decoded key. Without a valid signing key ordering stays off and the log says why. `MF_STITCHHUB_SERVICE` (optional) is the name this deploy goes by on the hub, default `service:makerfoundry`; a test deploy with its own hub key sets it to that key's name (e.g. `service:makerfoundry-test`), so what it signs, sends and takes is in that name. Needs Stripe too: a shop is offered only once its Foundry Account can be paid and the hub lists it as ready. Both keys are secrets |
| `MF_CLAMAV` | no (not scanned) | `host:port` of a ClamAV daemon (`clamd`, e.g. a Railway service on the private network): every 3D upload's files and images are scanned before they're kept; a flagged one is refused, and uploads are paused while the scanner can't be reached. Its `StreamMaxLength` must be at least 100M |
| `MF_3D` | no (admins only) | `yes` opens MakerFoundry 3D (`/3d`, MF-35 onwards) to everyone, after its launch checks (MF-59) |
| `MF_PUBLIC_URL` | no (default `https://makerfoundry.app`) | the site's address, for the links Stripe sends people back to and the sitemap |
| `STRIPE_API_URL` | no | a stand-in for Stripe's API (local tests) |
| `THINGIVERSE_WWW_URL`, `THINGIVERSE_API_URL`, `META_GRAPH_URL`, `RESEND_API_URL` | no | stand-ins for Thingiverse's sign-in and API, Meta's Graph API and Resend's API (local tests); leave unset in production |
| `STORAGE_ENDPOINT` | no | an S3-compatible address in place of R2's (local tests) |
| `STITCHFOUNDRY_CLOUD_URL` | no (default `https://cloud.stitchfoundry.app`) | StitchFoundry's account service, which MakerFoundry's accounts are |
| `MF_END_SESSIONS_BEFORE` | after a breach | sessions made before this UTC time (`2026-10-22T09:00:00Z`, a date, or Unix seconds) end when the service starts: everyone signs in again (see below) |

## After a breach

If Railway, a key or the database may have been exposed, give every secret a new value and sign
everyone out. For each: make the new one, put it in the Railway variable, let the service
redeploy, then revoke the old one.

1. `STRIPE_SECRET_KEY`: the restricted key rolled in Stripe (Developers › API keys).
2. `STRIPE_WEBHOOK_SECRET` and `STRIPE_CONNECT_WEBHOOK_SECRET`: each endpoint's signing secret
   rolled (Developers › Webhooks): two endpoints, two secrets.
3. `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`: a new R2 API token (Object Read & Write on the
   bucket only), the old one deleted (Cloudflare › R2 › Manage API tokens).
4. `DATABASE_URL`: the Postgres credentials regenerated in Railway.
5. `MF_PROOF_KEY` (once proofs are used, MF-25): a new `openssl rand -hex 32`. Every customer link
   changes: shops copy their proofs' new links from `/proofs` and send them again.
6. `MF_END_SESSIONS_BEFORE` set to the time of the change: sessions made before it end on the
   next start. The sessions hold StitchFoundry account tokens too, so end StitchFoundry's sessions
   the same way (`SF_CLOUD_END_SESSIONS_BEFORE`, StitchFoundry's `docs/cloud.md`).
7. Railway itself: a new password, two-factor sign-in, its API tokens revoked, the project's
   members and deploys checked.

## Building the image yourself
From the repository root: `docker build -t makerfoundry .` then
`docker run --rm -p 8080:8080 makerfoundry` and open <http://localhost:8080>.
Without Docker: `cargo run -p makerfoundry`.
