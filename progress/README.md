# progress/

One note file per PR, so PRs never conflict on `PROGRESS.md`.

## For a PR
Do **not** edit `PROGRESS.md`. Add one new file:

`progress/<yyyy-mm-dd>-<issue-or-pr-number>.md`, for example `progress/2026-11-22-166.md`.
If a PR has several notes on one day, add a letter: `2026-11-22-166b.md`.

Contents, short (a few lines):
- **Done:** what changed and how it was checked (or that CI runs the gate).
- **Next 3 steps:** what is left, in order.

## Roll-up (nightly/maintenance agent, daily or weekly)
1. Branch from current `main`; open one small PR titled `Progress roll-up <date>`.
2. Fold each `progress/*.md` note (not this README) into `PROGRESS.md` as a dated entry, newest at the top.
3. Delete the folded note files in the same PR.
4. The roll-up PR is the only PR that edits `PROGRESS.md`.
