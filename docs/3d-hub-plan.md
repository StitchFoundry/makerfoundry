# 3D printing hub: the plan

**Status:** 2026-10-30, Phase 1 (research and plan; no code yet). The feature comparison is in
[`3d-hub-feature-matrix.md`](3d-hub-feature-matrix.md); the work is GitHub issues labelled
`3d-hub` (P1 `claude-queue`, P2/P3 `claude-proposed`), task ids MF-35 onwards.

**The goal:** a home for 3D-printing makers that beats Thingiverse and Printables on speed, search,
licence clarity and print profiles: no ads, pages that load at once, remix licences enforced, and
Bambu Studio print profiles read properly. An opt-in showcase feeds MakerFoundry's Facebook,
Instagram and YouTube with creators' consent.

**Ground rules.** Behaviour from public documentation only (clean room); nothing copied from
Thingiverse, Printables or MakerWorld (no models, files, images, text, code, styles or branding);
every model uploaded by its creator, or imported by its creator from their own files or their own
account through an official API. No scraping, no crawling, no unofficial MakerWorld or Bambu Cloud
endpoints. Slicers' source code (Bambu Studio, OrcaSlicer and PrusaSlicer are AGPL, Cura LGPL) is
never read or copied: the 3MF settings are read from the files' own layout, learnt from public
descriptions and from sample files our creators give us.

## 1. Built on what MakerFoundry already has

The hub is a new section (`/3d`) on the same engine as embroidery and laser designs, not a second
site. A 3D model is a design of a new kind, `model3d`, so these work for it with small changes:

| Already built | Used for |
|---|---|
| Foundry Account sign-in, maker profiles and pages (MF-1, MF-3) | Creators and their profiles |
| Uploads with size caps, photo cleaning, R2 storage (MF-2) | Model files and images |
| Listings: details, tags, publishing, Postgres full-text search (MF-3) | Model pages and search |
| Makes, collections, follows, comments (MF-10, 11, 13, 14) | The community features |
| Reports, copyright notices and counter-notices, takedown, repeat infringers, `MF_DMCA_AGENT` (MF-5) | Moderation and DMCA. The request mentions a process "planned for StitchBench": MakerFoundry's own MF-5 process is that process here |
| Rate limits, data download and delete (MF-5, MF-15) | Uploads, and creators' data |
| Property tests of every reader (MF-20) | Every new file reader |

**The ordering features stay as they are:** selling (MF-4, MF-23, MF-24), approval links (MF-25)
and the StitchShop order work (issues #1, #2) are untouched, and their tests run on every change.
The hub is off until `MF_3D=yes`, so nothing shows until it's ready.

**Its name is "MakerFoundry 3D"** (the owner's decision, 2026-10-30), at `/3d`. **Its categories**
(the same decision; a fixed list in code): Household; Tools & workshop; Organisers & storage;
Toys & games; Cosplay & props; Miniatures & tabletop; Art & decor; Fashion & jewellery; Gadgets &
tech mounts; Education; Garden & outdoor; Automotive; 3D printer parts & upgrades; Other.

## 2. Data model

New tables and columns (migrations from 0021). Sketch; each issue settles its own details.

```sql
-- mf_designs: kind gains 'model3d'; a model's category, print settings and origin
ALTER TABLE mf_designs DROP CONSTRAINT mf_designs_kind_check,
  ADD CONSTRAINT mf_designs_kind_check CHECK (kind IN ('embroidery', 'laser', 'model3d'));
ALTER TABLE mf_designs
  ADD COLUMN category TEXT,                    -- from a fixed list in code
  ADD COLUMN print_settings JSONB,             -- typed by the creator: layer height, infill, supports, material, notes
  ADD COLUMN original_site TEXT CHECK (original_site IN ('makerworld', 'printables', 'thingiverse')),
  ADD COLUMN original_url TEXT,                -- the creator's own page there; shown, never fetched
  ADD COLUMN original_published_at DATE;

-- a model's files: meshes, 3MF projects, STEP and other sources, each read and checked
CREATE TABLE mf_model_files (
  id BIGSERIAL PRIMARY KEY,
  design_id BIGINT NOT NULL REFERENCES mf_designs (id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (role IN ('mesh', 'project', 'source', 'other')),
  format TEXT NOT NULL,                        -- stl, 3mf, obj, step, ...
  file_name TEXT NOT NULL, file_key TEXT NOT NULL UNIQUE, file_bytes BIGINT NOT NULL,
  triangles BIGINT, size_mm DOUBLE PRECISION[], -- what reading found
  thumb_key TEXT, position INTEGER NOT NULL
);

-- print profiles: a 3MF project's settings as read, several per model
CREATE TABLE mf_print_profiles (
  id BIGSERIAL PRIMARY KEY,
  design_id BIGINT NOT NULL REFERENCES mf_designs (id) ON DELETE CASCADE,
  file_id BIGINT NOT NULL REFERENCES mf_model_files (id) ON DELETE CASCADE,
  name TEXT NOT NULL,                          -- "0.2 mm PLA, A1 mini"
  slicer TEXT NOT NULL,                        -- bambu, orca, prusa, cura, unknown
  printers TEXT[] NOT NULL DEFAULT '{}',       -- normalised: x1c, p1s, a1, a1-mini, h2d, mk4, ...
  nozzle_mm DOUBLE PRECISION, layer_mm DOUBLE PRECISION, infill_percent REAL,
  walls INTEGER, supports BOOLEAN, plates INTEGER,
  filaments JSONB NOT NULL DEFAULT '[]',       -- [{"type": "PLA", "colour": "#d03030"}], one per slot used
  colours INTEGER NOT NULL DEFAULT 1,
  settings JSONB NOT NULL DEFAULT '{}'         -- a whitelist of readable keys, never the whole file
);

CREATE TABLE mf_remixes (
  child_id BIGINT NOT NULL REFERENCES mf_designs (id) ON DELETE CASCADE,
  parent_id BIGINT NOT NULL REFERENCES mf_designs (id),
  parent_licence TEXT NOT NULL,                -- the parent's licence when linked
  PRIMARY KEY (child_id, parent_id)
);

CREATE TABLE mf_likes (account_id BIGINT NOT NULL, design_id BIGINT NOT NULL REFERENCES mf_designs (id) ON DELETE CASCADE,
  at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY (account_id, design_id));

-- bulk import: files waiting in a batch until the creator groups and confirms them
CREATE TABLE mf_import_batches (id BIGSERIAL PRIMARY KEY, account_id BIGINT NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now());
CREATE TABLE mf_import_files (id BIGSERIAL PRIMARY KEY, batch_id BIGINT NOT NULL REFERENCES mf_import_batches (id) ON DELETE CASCADE,
  path TEXT NOT NULL, format TEXT NOT NULL, file_key TEXT NOT NULL, file_bytes BIGINT NOT NULL, group_name TEXT NOT NULL);

-- the showcase: consent per upload or make, and what was posted
CREATE TABLE mf_showcase_consents (
  id BIGSERIAL PRIMARY KEY, account_id BIGINT NOT NULL,
  design_id BIGINT REFERENCES mf_designs (id) ON DELETE CASCADE,
  make_id BIGINT REFERENCES mf_makes (id) ON DELETE CASCADE,
  consented_at TIMESTAMPTZ NOT NULL, withdrawn_at TIMESTAMPTZ,
  CHECK ((design_id IS NULL) <> (make_id IS NULL))
);
CREATE TABLE mf_showcase_posts (
  id BIGSERIAL PRIMARY KEY, consent_id BIGINT NOT NULL REFERENCES mf_showcase_consents (id),
  caption TEXT NOT NULL, drafted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  platform TEXT CHECK (platform IN ('facebook', 'instagram', 'youtube')), post_url TEXT,
  posted_at TIMESTAMPTZ, takedown_flagged_at TIMESTAMPTZ, taken_down_at TIMESTAMPTZ
);

-- founding creators: invite links and the badge
CREATE TABLE mf_invites (code_hash TEXT PRIMARY KEY, badge TEXT NOT NULL, made_by BIGINT NOT NULL,
  used_by BIGINT, used_at TIMESTAMPTZ, expires_at TIMESTAMPTZ NOT NULL);
ALTER TABLE mf_makers ADD COLUMN founding BOOLEAN NOT NULL DEFAULT false;
```

## 3. Files: storage, limits and checks

Uploads are untrusted (CLAUDE.md). Every file is read by our own Rust code under limits, and
property-tested with random and damaged input, as the embroidery and laser readers are.

| Check | Limit (the owner's decision, 2026-10-30) |
|---|---|
| One model file | 100 MB |
| One model, all files | 300 MB, up to 30 files and 12 images |
| A 3MF or ZIP, uncompressed | 1 GB in all, 512 MB an entry, 4,096 entries, at most 200 times its compressed size |
| Triangles read for a preview | 5 million (files with more are kept, drawn from a reduced copy) |
| Models a creator keeps | 2,000; 100 new models or versions a day |
| A bulk import | 200 files or 2 GB at once; unpublished drafts expire after 30 days |

- **STL:** binary or ASCII, our own reader; the triangle count must match the file's size; every
  number finite; a size limit in millimetres as for laser files.
- **OBJ:** vertices and faces only (our own reader), other lines ignored; `.mtl` kept as a download.
- **3MF:** a ZIP (the 3MF core specification; Open Packaging Conventions) read in memory, never
  written out to disk by its own paths. The limits above against zip bombs; names with `..`,
  absolute paths or backslashes refused. XML read by `roxmltree` (already in the build, through
  usvg) with DTDs refused, so no external entities can be loaded. Then the file is **rebuilt**
  with only the parts we know: the model XML, `[Content_Types].xml`, relationships, thumbnails
  (PNG/JPEG, re-encoded) and the slicers' settings files. Anything else (scripts, executables,
  unknown parts) is dropped, and the rebuilt file is what people download. Sliced G-code inside
  a 3MF is dropped too (the owner's decision, 2026-10-30, for now: printers run it).
- **STEP:** checked to be ISO 10303-21 text (`ISO-10303-21;` header) and size-capped; offered as a
  download, not previewed (P3).
- **Images:** the existing photo cleaner.
- **Viruses:** strict reading and rebuilding means no file is served as it came, except STEP and
  "other" files, which are always downloads (`Content-Disposition: attachment`) from storage's own
  address, never shown on our site. A ClamAV service on Railway is P2 (cost: the owner's call).
- **Storage:** Cloudflare R2, random keys (`model/…`, `thumb/…`), signed links for downloads, as
  today. The whole model as a ZIP is made once per published version and stored.

## 4. Bambu Studio, OrcaSlicer, PrusaSlicer and Cura 3MF

A project 3MF holds the geometry (`3D/3dmodel.model`, the 3MF core) and slicer files under
`Metadata/`. What's read, by slicer (layout from public descriptions; to be confirmed on sample
files from our creators before each parser is written):

- **Bambu Studio and OrcaSlicer:** `Metadata/project_settings.config` (JSON: printer model,
  nozzle, layer height, infill, walls, supports, filament types and colours),
  `Metadata/model_settings.config` (XML: plates, the objects on each, and each object's or part's
  filament slot, giving the AMS colours actually used), and plate thumbnails (PNGs under
  `Metadata/`). Printer names are matched to badges: X1C, X1E, P1S, P1P, A1, A1 mini, H2D and
  others as they appear.
- **PrusaSlicer:** `Metadata/Slic3r_PE.config` (`; key = value` lines) and
  `Metadata/Slic3r_PE_model.config`: layer height, infill, perimeters, supports, printer, filament.
- **Cura:** the settings files under `Cura/`: what's readable (layer height, infill, supports).
- **Never fails an upload:** if settings can't be read, the 3MF is kept as geometry with "settings
  not readable", and the creator can type the print settings.
- Only a whitelist of keys is kept (`mf_print_profiles.settings`), never the whole file's text.
- **Several profiles per model**, each its own rebuilt 3MF to download, with a panel: printer
  badges, nozzle, layer height, infill, walls, supports, filament types, number of colours, plates.
- **Thumbnails:** the 3MF's own plate thumbnail when present (re-encoded); otherwise drawn.

## 5. Thumbnails and the viewer

- **Thumbnails (P1, no JavaScript):** drawn on the server by our own small software renderer
  (orthographic, z-buffered, shaded triangles, written to PNG with the `image` crate we already
  use), from a mesh reduced to a triangle budget: one hero image and three side views.
- **Viewer (P1): our own small WebGL viewer** (the owner agreed to its JavaScript, 2026-10-30, as
  CLAUDE.md asks: an interactive 3D view can't be done without it). A few hundred lines of plain
  JavaScript, MIT OR Apache-2.0, served from our own domain (`script-src 'self'`) on model pages
  only, loading a compact binary mesh the server makes: no third-party code, no dependency to
  track. The page works without JavaScript (the server-drawn images); the viewer enhances it.
  (Considered: three.js, vendored; or images only.)

## 6. Licences and remixes

**The licence picker** (the owner's decision, 2026-10-30: Creative Commons and open licences).
The standard public licences are offered by name and linked to their official texts, never
retyped; MakerFoundry's own licences, already in the approved Terms (§ 3), cover the
non-open cases.

| Group | Licences |
|---|---|
| Creative Commons 4.0 ([creativecommons.org](https://creativecommons.org/licenses/)) | CC BY, CC BY-SA, CC BY-NC, CC BY-NC-SA, CC BY-ND, CC BY-NC-ND |
| Public domain | CC0 1.0 |
| GNU ([gnu.org](https://www.gnu.org/licenses/)) | GPL v2 or later, GPL v3 or later, LGPL v2.1 or later, LGPL v3 or later |
| Permissive software licences | BSD 3-Clause ([opensource.org](https://opensource.org/license/bsd-3-clause)), Apache 2.0 ([apache.org](https://www.apache.org/licenses/LICENSE-2.0)), MIT ([opensource.org](https://opensource.org/license/mit)) |
| Open hardware ([CERN OHL v2](https://ohwr.org/cern_ohl_s_v2.txt)) | CERN-OHL-S, CERN-OHL-W, CERN-OHL-P |
| MakerFoundry's own (as for embroidery and laser designs) | Personal use, Small business (up to 200 items), Commercial |
| Reserved | All rights reserved |

Other community licences are left out until their text and author are checked. Embroidery and laser designs keep their three; paid 3D models (if ever sold) would use
MakerFoundry's own three, as paid designs do.

A remix links one or more parents. Publishing a remix checks every parent (the licences' own
terms and the Creative Commons FAQ; the rules to be confirmed by the owner, ideally with a
lawyer):

| Parent's licence | A remix may be published as |
|---|---|
| CC0, MIT, BSD 3-Clause, Apache 2.0, CERN-OHL-P | any licence (credit shown) |
| CC BY | any licence (credit shown) |
| CC BY-SA | CC BY-SA only |
| CC BY-NC | CC BY-NC or CC BY-NC-SA |
| CC BY-NC-SA | CC BY-NC-SA only |
| CC BY-ND, CC BY-NC-ND | not at all (adaptations can't be shared) |
| GPL v2 or later | GPL v2 or later, or GPL v3 or later |
| GPL v3 or later | GPL v3 or later |
| LGPL v2.1 or later | LGPL v2.1 or later, LGPL v3 or later, or a GPL |
| LGPL v3 or later | LGPL v3 or later, or GPL v3 or later |
| CERN-OHL-S | CERN-OHL-S |
| CERN-OHL-W | CERN-OHL-W or CERN-OHL-S |
| Personal use, Small business, Commercial, All rights reserved | not at all, unless the remixer made the parent |

With several parents, the remix must satisfy every one; if no licence does, it can't be published.
A creator may always remix their own models. The model page shows its parents (creator, licence,
link) and its remixes; the whole tree (both ways, up to 10 generations) on its own page, from a
recursive query. A parent taken down stays named in the tree, without a link.

## 7. Search

Postgres full-text search, as `/browse` uses today (weighted title, tags, description), with
filter columns indexed: category, licence, file formats (`TEXT[]`), has makes, has a print
profile, printers (`TEXT[]`), multi-colour, filament types (`TEXT[]`); sorted by relevance, newest
or popular (likes, downloads and makes of the last 30 days, kept as a column updated by the
sweep). Pages are paged by key, not offset. Target: under 100 ms a page with 100,000 models,
checked by a test with generated rows. No outside search service.

## 8. Import your own models

- **P1, files the creator uploads** (MakerWorld, Printables, Thingiverse or anywhere): "Import my
  models" takes many files at once or a ZIP (3MF, STL, OBJ, STEP, images). Each file is read and
  checked as any upload. Files are **grouped into models**: by folder in a ZIP, else by name (the
  same stem, ignoring endings like `_plate2`, `-v2`, `_part3`); images join the group whose name
  they share. The review page lists the groups; the creator moves files between them, merges,
  splits and renames (plain forms, no JavaScript needed). Each group becomes a **draft**; the
  creator confirms title, description, licence and category, then publishes one or all. Publishing
  requires ticking **"I confirm I created these models or have the right to share them."**
- **Originally published on:** an optional site (MakerWorld, Printables, Thingiverse), the
  creator's own page there (the address must be on that site) and the original date. Shown as
  "Originally published on MakerWorld, 12 March 2025". MakerFoundry never fetches it.
- **Ownership.** For uploaded files MakerFoundry can't prove who owns the source account; it
  relies on the creator's confirmation, the licence they choose, and the copyright takedown
  process. A stronger check is the owner's call (Decisions).
- **P2, Thingiverse account link:** through Thingiverse's official OAuth API, if its terms allow
  importing: the creator signs in to Thingiverse, `/users/me` proves the account is theirs, and
  only their own things are offered, with their licence and date. Printables and MakerWorld have
  no official API, so for them it's files only.

## 9. The opt-in showcase

- Every upload form, every make and the import's publish step get an **unticked** box: "Feature
  my project on MakerFoundry's social pages (Facebook, Instagram, YouTube)". Ticking it stores a
  consent with its time; the creator's settings list their consents with **Withdraw** on each.
- **Admin › Showcase** lists only consented, published, not withdrawn items: the image (first
  photo, or the make's photo), the creator's **display name** (their maker profile name: never
  their email or anything else), the model's link and licence. **Draft social post** makes a
  caption that credits that display name and links the model (template ours; the owner approves
  it). The admin posts by hand and records the platform and the post's address.
- **Withdrawn** consent takes the item off the queue at once, and every post already made from it
  is flagged on the admin's page ("take these down"), until marked taken down.
- Consents are in the creator's data download, and their deletion withdraws everything.
- The wording of the box and what it permits (cropping, captioning, how long) goes in the Terms
  and the Privacy Policy, for the owner's approval.
- Later (P2): the drafts feed the YouTube, Facebook and Instagram posting pipeline.

## 10. Founding creators

**Built on `main` as MF-54 (2026-10-30)**, for everyone, not only 3D (the owner's decisions:
**100 founding creators**, chosen **by application**, with a **numbered badge**): makers apply at
`/founding`, admins approve up to 100 at `/admin/founding`, and "Founding creator #7" shows on
their maker page, designs and account page. The original idea of invite links is replaced by
that. What's left for the hub (#22): founding creators use MakerFoundry 3D before `MF_3D` is on
for everyone, and the badge shows on 3D models too. Nick is one of the first, importing his
MakerWorld models with the bulk import.

## 11. Cross-sell

- **Get this printed: off for now** (the owner's decision, 2026-10-30). Nobody fulfils print
  orders yet, so no button is built. When the owner decides who prints and where it goes, it
  will be a button on a model whose licence allows printing for sale (not NonCommercial, unless
  the seller is the creator) and whose creator ticks "allow print orders", leading to
  MakerFoundry's ordering (approval links, MF-25, and the StitchShop order flow, issue #2).
- **Also by this maker:** links to the same maker's embroidery (StitchFoundry) and laser
  (LightFoundry/LaserFoundry) designs.

## 12. Moderation

MF-5 as it is, with 3D models in the same queue: reports, copyright notices and counter-notices,
one-click takedown (files stop downloading, the page says it's been taken down), repeat
infringers counted, and the comment and make reports of MF-14 and MF-12. Rate limits on uploads,
imports, likes, comments and reports.

## 13. Roadmap (P1 issues in dependency order)

| Id | Issue | GitHub | Needs |
|---|---|---|---|
| MF-35 | The 3D section's foundation: `model3d`, categories, the 3D licences, `/3d` behind `MF_3D` | [#3](https://github.com/SiteSentrix/makerfoundry/issues/3) | – |
| MF-36 | STL reader with limits and property tests | [#4](https://github.com/SiteSentrix/makerfoundry/issues/4) | MF-35 |
| MF-37 | OBJ reader with limits and property tests | [#5](https://github.com/SiteSentrix/makerfoundry/issues/5) | MF-35 |
| MF-38 | Safe 3MF reader and rebuilder (zip limits, no DTD, strip unknown parts), property tests | [#6](https://github.com/SiteSentrix/makerfoundry/issues/6) | MF-35 |
| MF-39 | STEP acceptance and the model file allow-list | [#7](https://github.com/SiteSentrix/makerfoundry/issues/7) | MF-35 |
| MF-40 | Server-drawn thumbnails and views (our renderer), embedded 3MF thumbnails | [#8](https://github.com/SiteSentrix/makerfoundry/issues/8) | MF-36, MF-38 |
| MF-41 | Model upload: several files and images, drafts, limits | [#9](https://github.com/SiteSentrix/makerfoundry/issues/9) | MF-36 to MF-40 |
| MF-42 | Model details and publishing: description, print settings, tags, category, licence; the public model page; downloads and the ZIP | [#10](https://github.com/SiteSentrix/makerfoundry/issues/10) | MF-41 |
| MF-43 | Bambu Studio/OrcaSlicer 3MF settings: printer, nozzle, plates, AMS colours, filaments | [#11](https://github.com/SiteSentrix/makerfoundry/issues/11) | MF-38 |
| MF-44 | PrusaSlicer and Cura 3MF settings, basic; never failing an upload | [#12](https://github.com/SiteSentrix/makerfoundry/issues/12) | MF-43 |
| MF-45 | Print profiles on model pages: several per model, panel, badges, per-profile download | [#13](https://github.com/SiteSentrix/makerfoundry/issues/13) | MF-42, MF-43 |
| MF-46 | The 3D viewer (after the owner's decision) | [#14](https://github.com/SiteSentrix/makerfoundry/issues/14) | MF-42 |
| MF-47 | Remixes: parents, licence rules enforced, the attribution tree | [#15](https://github.com/SiteSentrix/makerfoundry/issues/15) | MF-42 |
| MF-48 | Likes, and makes, collections, follows and comments on 3D models | [#16](https://github.com/SiteSentrix/makerfoundry/issues/16) | MF-42 |
| MF-49 | 3D search and filters, with a speed test | [#17](https://github.com/SiteSentrix/makerfoundry/issues/17) | MF-45, MF-48 |
| MF-50 | Creator profiles for 3D | [#18](https://github.com/SiteSentrix/makerfoundry/issues/18) | MF-48 |
| MF-51 | Moderation and DMCA for 3D models, upload rate limits | [#19](https://github.com/SiteSentrix/makerfoundry/issues/19) | MF-42 |
| MF-52 | Bulk import: many files or a ZIP into a batch, grouped automatically | [#20](https://github.com/SiteSentrix/makerfoundry/issues/20) | MF-41 |
| MF-53 | Bulk import review: fix groups, drafts, origin, confirmation, bulk publish | [#21](https://github.com/SiteSentrix/makerfoundry/issues/21) | MF-52, MF-42 |
| MF-54 | Founding creators' early access to MakerFoundry 3D (the rest built on `main`) | [#22](https://github.com/SiteSentrix/makerfoundry/issues/22) | MF-35 |
| MF-55 | Showcase consent on uploads, makes and imports; withdrawing from settings | [#23](https://github.com/SiteSentrix/makerfoundry/issues/23) | MF-42, MF-48 |
| MF-56 | Admin Showcase queue: list, draft caption, posted, takedown flags | [#24](https://github.com/SiteSentrix/makerfoundry/issues/24) | MF-55 |
| MF-57 | Cross-sell: Also by this maker (Get this printed is off for now) | [#25](https://github.com/SiteSentrix/makerfoundry/issues/25) | MF-42 |
| MF-58 | The 3D hub's Terms and Privacy wording, for the owner's approval | [#26](https://github.com/SiteSentrix/makerfoundry/issues/26) | MF-47, MF-55 |
| MF-59 | Launch: the checks, then `MF_3D=yes` | [#27](https://github.com/SiteSentrix/makerfoundry/issues/27) | all of the above |

**P2 (proposed, `claude-proposed`):** Thingiverse account import through its official API (#28);
ClamAV scanning (#29); the social posting pipeline (#30); email notices (#31); STEP previews (#32).
**P3 (proposed):** contests (#33); points and rewards (#34); parametric models, a sandboxed
OpenSCAD customizer (#35); a public API (#36).

Each P1 issue carries its tests: upload validation (bad, huge and malformed files, zip bombs),
licence rules for remixes, import ownership and grouping (30+ files across several models), consent
on, off and withdrawn, search filters, moderation and takedown, Bambu 3MF with AMS colours,
PrusaSlicer 3MF, broken 3MF, oversized uploads, and draft to publish.

## 14. Decisions for the owner

1. ~~**The viewer's JavaScript**~~: **decided 2026-10-30: our own WebGL viewer** (§ 5).
2. ~~**Size limits**~~: **decided 2026-10-30: as proposed** (§ 3).
3. ~~**Sliced G-code inside 3MF files**~~: **decided 2026-10-30: dropped** (printers run it).
4. ~~**ClamAV**~~: **decided 2026-10-30: later** (P2, #29).
5. ~~**Licences**~~: **decided 2026-10-30: Creative Commons and open licences** (§ 6). Still open:
   the remix rules' confirmation, ideally by a lawyer, before launch.
6. ~~**Ownership of imported files**~~: **decided 2026-10-30: the creator's confirmation and the
   takedown process** (§ 8); nothing fetched from other sites.
7. **Thingiverse import (P2):** registering an app and confirming its API terms allow it.
8. ~~**Get this printed**~~: **decided 2026-10-30: off for now.** Who prints and where the
   button goes are decided later.
9. ~~**The showcase**~~: **approach agreed 2026-10-30** (§ 9: the unticked box's wording, a
   caption crediting only the chosen display name, Terms and Privacy text drafted in #26). The
   drafted legal text still comes back to the owner before it goes into the Terms.
10. **Founding creators:** 100, by application, numbered badge (decided and built 2026-10-30).
    **Still needed:** sample 3MF
    files: two or three of Nick's own Bambu Studio 3MFs (one with AMS multi-colour) and a
    PrusaSlicer or Cura 3MF, with permission to keep them in the repository as test samples.
11. ~~**The section's name and categories**~~: **decided 2026-10-30: "MakerFoundry 3D"** and the
    categories in § 1.
