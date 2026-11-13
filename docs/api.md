# MakerFoundry's API for the apps (MF-7)

**Status:** 2026-10-19. Built and tested on MakerFoundry's side (`apps/makerfoundry/src/api.rs`).
The apps' side (a **Publish to MakerFoundry** command) is StitchFoundry's and LightFoundry's work,
in their repositories, under their rules.

## How publishing from an app works
1. The person is signed in to their **Foundry Account** in the app (the app holds the account
   service's session token, `sfs_…`, as it does for AI credits).
2. The app exports the design (StitchFoundry: the embroidery file in the format the person picks;
   LightFoundry: SVG or DXF) and sends it, with any photos, to MakerFoundry with that token.
3. MakerFoundry checks the token with the account service (`POST /v1/account`), reads and checks
   the file exactly as an upload on the site, keeps it as a **draft**, and answers with its page.
4. The app opens that page in the browser. There the maker adds the title, description, tags,
   licence and price, and publishes: publishing (and agreeing to the Seller Terms) always happens
   on the site. If the browser isn't signed in to MakerFoundry, the page asks for the Foundry
   Account sign-in and comes back to the draft.

MakerFoundry uses the token for that one request and never keeps or logs it.

## `POST https://makerfoundry.app/api/v1/designs`
Headers: `Authorization: Bearer <Foundry Account session token>`,
`Content-Type: application/json`. Body:

```json
{
  "file_name": "rose.pes",
  "file": "<the design file, base64 (standard alphabet, with padding)>",
  "photos": ["<JPEG, PNG or WebP of the stitched or cut result, base64>"],
  "app": "StitchFoundry 0.9.0"
}
```

- `file_name` decides how an embroidery file is read (its extension); only its last part is kept.
- `photos` (optional): up to 6. They're saved again without their metadata (location, camera).
- `app` (optional): the app and its version, for MakerFoundry's log.
- Limits are the site's: embroidery files up to 4 MB, laser files up to 8 MB, 64 MB in all
  (before base64), 50 designs a day and 500 kept per account.

Answers (JSON):

| Status | Body | Means |
|---|---|---|
| 201 | `{"id": 42, "url": "https://makerfoundry.app/designs/42", "kind": "embroidery", "format": "PES"}` | Kept as a draft: open `url` in the browser |
| 400 | `{"error": {"message": "…"}}` | Not JSON, no file name, not base64, too many photos |
| 401 | same | No token, or the sign-in has ended: sign in again |
| 413 | (no body) | Bigger than the limit |
| 422 | same | The file or a photo couldn't be read (the message says why, for the person) |
| 429 | same | Too many designs today, too many kept, or too many failed sign-ins |
| 503 | same | MakerFoundry or Foundry Accounts can't be reached, or keeping designs is off |

Every `error.message` is a sentence to show the person as it is.

## `GET https://makerfoundry.app/api/v1`
`{"name": "MakerFoundry", "version": "0.1.0", "designs": "/api/v1/designs", "max_upload_bytes":
67108864, "max_photos": 6}`: for an app to check it's talking to MakerFoundry and read the limits.

## `GET https://makerfoundry.app/api/v1/designs/{id}/file/{name}` (Open in StitchFoundry)
A free, published embroidery design's file, for StitchFoundry's web app, which opens it from
`https://stitchwright.github.io/StitchFoundry/?open=<this address, URL-encoded>` (StitchFoundry's
OPEN-01). Design pages of free, published embroidery designs show an **Open in StitchFoundry** button
with that link; `{name}` (the design's title as a file name, with its extension, `rose.pes`) only
ends the address, because the app reads the file by its extension.

- The file itself is sent (`application/octet-stream`, `Content-Disposition: attachment;
  filename="rose.pes"`, cached 5 minutes), not a link to the storage, which another site's page
  couldn't follow; CORS for the web apps' sites, with `Content-Disposition` exposed.
- Cookies aren't read: it's only what anyone may download. A draft, a removed or taken-down
  design, a laser file or an unknown id is `404` ("That design isn't on MakerFoundry."); a design
  for sale is `403` unless the address carries a buyer's link: `?t=<link>`, made by `/d/<id>/open`
  once the person's right to the design is checked (its buyers, its maker, the admins), good for that
  design for ten minutes. 120 an hour from one address
  (`429`). Each counts as a download.

## From a web app (browsers)
The web apps' sites may call the API from the browser: `MF_API_ORIGINS` (default
`https://stitchfoundry.app,https://stitchwright.github.io`, StitchFoundry's site and its web app's; add LightFoundry's when it has one) lists them. A browser's
preflight (`OPTIONS`) is answered for those sites only, allowing `POST` with the `authorization`
and `content-type` headers. No cookies are used or allowed. Desktop apps don't need any of this.

## For StitchFoundry (a note for its chat)
StitchFoundry's `sf_core::ai::cloud::request(server, path, token, body)` already builds the
request: `request("https://makerfoundry.app", "/api/v1/designs", Some(&token), &body)` with the
body above, sent like `cloud::report` (`prompt::post_plain`), then `app.open_url(&answer["url"])`.
Suggested shape, by StitchFoundry's rules (everything is a command, wasm must build, `cargo xtask
ci` green, its own task id):

- A command `embroidery.publishToMakerFoundry` (menu: File › Publish to MakerFoundry…), enabled
  when there's an embroidery design and the person is signed in to Foundry Account; params
  `{format}` (default PES) and optionally `{photos}` from the sew-out check's photos.
- It exports the active design with the chosen writer, base64-encodes it, sends it, and on `201`
  opens `url`; on an error it shows `error.message`.
- A setting for MakerFoundry's address (default `https://makerfoundry.app`, a local address for
  testing, checked with `cloud::clean_server`).

## For LaserFoundry (a note for its chat): Open in LaserFoundry
MakerFoundry's side is built (MF-18): laser designs' SVG and DXF files (what LaserFoundry imports)
are served at the same address as embroidery files, `GET
https://makerfoundry.app/api/v1/designs/{id}/file/{name}` (free designs for anyone; a paid design
with a buyer's `?t=` link), with CORS for `https://stitchwright.github.io` and `Content-Disposition`
exposed. The **Open in LaserFoundry** button is **on** since 2026-10-31 (LaserFoundry v0.2.0;
`LASERFOUNDRY_OPENS_LINKS` in `apps/makerfoundry/src/api.rs`): it links to
`https://stitchwright.github.io/laserfoundry/?open=<the file's address, URL-encoded>`. Checked
live in Chromium: the web app read `?open=`, fetched `makerfoundry.app` with CORS allowed, and
showed MakerFoundry's own sentence for a design that isn't published.

LaserFoundry's side, by its own rules (its `CLAUDE.md`, its own task id), as StitchFoundry did
(its OPEN-01, `apps/vectorcraft-web/src/open_link.rs`, a good model):

- The web app reads `?open=<address>` once at start: only `https://` on `makerfoundry.app` or
  `www.makerfoundry.app` (no user name, no port, no spaces or control characters, look-alike hosts
  refused), fetched without cookies (`credentials: "omit"`), at most 32 MB, then opened as a dropped
  file would be (SVG or DXF, by the name's extension).
- The file's name: the end of the address's path (before `?`), or `Content-Disposition`'s
  `filename` when the answer exposes it; never a path, never empty.
- Its page's content security policy has to let the page fetch MakerFoundry:
  `apps/laserfoundry-web/index.html`'s `connect-src` gains `https://makerfoundry.app
  https://www.makerfoundry.app` (today it names only `'self'` and the account service).
- The status bar says it's opening the design, or why it couldn't (MakerFoundry's answers carry
  `{"error": {"message"}}` with a sentence to show as it is: `403` for a design for sale without a
  buyer's link, `404` for one that isn't there or can't be opened, `429` for too many).
- Tests: the address check (allowed hosts, refused look-alikes, encoded addresses), the file name.

Once it ships, tell the MakerFoundry chat to switch the button on.

## For ScreenFoundry (MF-87): Open in ScreenFoundry, and publishing
ScreenFoundry (screen-printing prepress, StitchFoundry family; its side is SC-11) opens **SVG only**.

- **The button** shows on the page of a free, published SVG design (a paid one: for its buyers,
  its maker and the admins, through `/d/<id>/open?app=screenfoundry`, which makes the ten-minute
  `?t=` link). Not on DXF, PDF, embroidery or 3D designs. On since 2026-11-01
  (`SCREENFOUNDRY_OPENS_LINKS` in `apps/makerfoundry/src/api.rs`), when ScreenFoundry's web app
  at `https://app.screenfoundry.app/` opened links. The link:
  `https://app.screenfoundry.app/?open=<the file's address, URL-encoded>`, for example
  `https://app.screenfoundry.app/?open=https%3A%2F%2Fmakerfoundry.app%2Fapi%2Fv1%2Fdesigns%2F42%2Ffile%2Ftiger-shirt.svg`
  for `https://makerfoundry.app/api/v1/designs/42/file/tiger-shirt.svg`. An SVG design shows
  Open in LaserFoundry, Open in ScreenFoundry and Open in PatternFoundry side by side.
- **The file's name** (`{name}`, and `Content-Disposition`'s `filename`) is the design's title as
  a slug (lowercase ASCII letters, digits and `-`, at most 60) and the file's extension (letters and
  digits); when the title leaves no slug (empty, only symbols, or not in Latin letters) it's
  `design-<id>`. So the path is only `a-z 0-9 - .` and the query only `t=<48 hex digits>`: every
  address meets ScreenFoundry's rules (tested against them, with random titles).
- **The file's answer:** the SVG as `application/octet-stream` with `Content-Disposition:
  attachment; filename="<name>.svg"` (never shown as a page from MakerFoundry's site), CORS for
  `MF_API_ORIGINS` (keep `https://stitchwright.github.io`) with `Content-Disposition` exposed, no
  redirects, `403`/`404`/`429` as `{"error":{"message"}}`. SVG designs are at most 8 MB, well
  under ScreenFoundry's 32 MB.
- **Publishing** is unchanged: an SVG sent with `"app": "ScreenFoundry 0.1.0"` becomes a draft of
  the laser kind. A draft from any app is now titled after its file (`Tiger_shirt-front.svg` →
  "Tiger shirt front"; "Untitled design" if that leaves nothing), so it's never left untitled.
  The draft also keeps the app's name without its version (`from_app`: "ScreenFoundry", MF-88):
  admins list designs by app at `/admin/apps`, and it's in the maker's data download. Send `app`
  as `"<Name> <version>"`; the name is its first word, letters and digits only.


## For PatternFoundry (MF-89, issue #47): Open in PatternFoundry
PatternFoundry (sewing patterns, SiteSentrix; its PF-11 and PF-12) opens **SVG only**, from
`https://sitesentrix.github.io/patternfoundry/?open=<the file's address, URL-encoded>`, exactly as
ScreenFoundry does (same file address, same names, same answers; see above). Its site is in
`MF_API_ORIGINS` (`https://sitesentrix.github.io`, set 2026-11-01).

- **The button** shows on free, published SVG designs (paid ones: through
  `/d/<id>/open?app=patternfoundry`), after LaserFoundry's and ScreenFoundry's. Its switch,
  `PATTERNFOUNDRY_OPENS_LINKS` in `apps/makerfoundry/src/api.rs`, is **on**: checked 2026-11-01 in
  Chromium against the live web app, which read `?open=`, fetched the address without cookies,
  opened the SVG under its Content-Disposition name in millimetres, cleared `?open=`, and showed
  MakerFoundry's own sentence for a design that isn't published.
- **Publishing** from PatternFoundry is the usual `POST /api/v1/designs` with
  `"app": "PatternFoundry <version>"`: an SVG lands as a laser-kind draft, titled after its file.
- **Later:** a "sewing pattern" kind (true-scale PDF, DXF-AAMA, sizes, tiles) is issue #47's other
  item, not built.

## Reading 3D models (issue #36)
Read-only JSON about **published** 3D models, for slicers and apps (`apps/makerfoundry/src/model3d/api.rs`).
Every request carries an API key a member makes at **Account › API keys** (up to 5; shown once,
kept only as its SHA-256; revoked there): `Authorization: Bearer mfk_…`. Each key may make 1,000
requests an hour. Until MakerFoundry 3D opens to everyone (`MF_3D=yes`), only admins' keys answer.
There's no CORS: keys belong in apps and servers, not in web pages.

### `GET https://makerfoundry.app/api/v1/models`
The same search as `/3d`, with the same query: `q`, `category`, `licence`, `format` (`stl`, `3mf`,
`obj`, `step`), `printer`, `filament`, `makes=yes`, `profile=yes`, `multicolour=yes`,
`sort=popular` (newest first otherwise), `per` (1 to 24) and `after` (from `next`).

```json
{"models": [{"id": 42, "title": "Gear", "url": "https://makerfoundry.app/d/42/gear", "maker": "Ann",
  "price_cents": 0, "downloads": 3, "picture": "https://makerfoundry.app/3d/42/files/7/hero",
  "api": "https://makerfoundry.app/api/v1/models/42"}],
 "next": "https://makerfoundry.app/api/v1/models?after=42"}
```

### `GET https://makerfoundry.app/api/v1/models/{id}`
One published model: `id`, `title`, `url`, `description`, `tags`, `category`, `licence` (`id`,
`name`, `url` of its text), `price_cents`, `downloads`, `likes`, `published_at` (UTC),
`maker` (`name`, `url`), `files` (`id`, `name`, `format`, `role`, `bytes`, `triangles`,
`size_mm`, `picture`), `profiles` (each 3MF's print settings: `slicer`, `printers`, `nozzle_mm`,
`layer_mm`, `infill_percent`, `walls`, `supports`, `plates`, `filaments`) and `download`: for a free
model, its download on the site, where its licence is shown; `null` for one for sale.

Errors are `{"error": {"message"}}`: `401` without a known key, `404` for a model that isn't
published (or before 3D opens), `429` past the hour's requests, `503` if MakerFoundry can't answer.
Publishing from slicers is for later.
