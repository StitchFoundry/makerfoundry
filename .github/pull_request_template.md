## What and why
<!-- One or two sentences. -->

Fixes #

## How it was tested
<!-- Last line of `cargo xtask ci` and the docker build result. Screenshots for page changes. -->

## Checklist
- [ ] `cargo xtask ci` and `docker build -t makerfoundry .` pass
- [ ] Tests added or updated
- [ ] No secrets, no panics in shipped code, uploads still treated as untrusted
- [ ] Clean room respected; new assets have a row in `ASSETS.md`
- [ ] `docs/plan.md`, README Status updated; `progress/<yyyy-mm-dd>-<issue-or-pr>.md` note added (do not edit `PROGRESS.md`)
