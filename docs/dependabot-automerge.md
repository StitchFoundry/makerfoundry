# Dependabot auto-merge

Status: **workflow added** as `.github/workflows/dependabot-automerge.yml` (needs-review; Bruce
merges). Pilot is patternfoundry (PR #217); keep the two files identical.

## Rules
The workflow runs on `pull_request` from `dependabot[bot]` and only turns on squash auto-merge
(`gh pr merge --auto --squash`); GitHub still waits for every required check. It enables it only when
all of these hold:
- update type is `version-update:semver-patch` or `version-update:semver-minor`;
- the update is not a 0.x minor bump (0.x minors count as breaking);
- the ecosystem is not `github_actions` (workflow bumps stay manual);
- a grouped update contains no major or 0.x-minor bump.

Otherwise it does nothing except leave one short comment saying why. Permissions are
`contents: write` and `pull-requests: write` on the job only; no secrets, no `pull_request_target`.

Agent (Claude) PRs are never touched: the job is guarded by `github.actor == 'dependabot[bot]'`.

## Turning it off
Delete the workflow file, or disable it under Actions → "Dependabot auto-merge" → ⋯ → Disable.
Auto-merge already enabled on an open PR can be cancelled with `gh pr merge --disable-auto <PR>`.

## Workflow
`dependabot/fetch-metadata` is pinned to the commit of tag v3.1.0 (`25dd0e34…`). The block below is
the same as the file.

```yaml
name: Dependabot auto-merge
on: pull_request
permissions: {}
jobs:
  automerge:
    if: github.actor == 'dependabot[bot]'
    runs-on: ubuntu-latest
    permissions:
      contents: write
      pull-requests: write
    steps:
      - id: meta
        uses: dependabot/fetch-metadata@25dd0e34f4fe68f24cc83900b1fe3fe149efef98 # v3.1.0
        with:
          github-token: ${{ secrets.GITHUB_TOKEN }}
      - name: Decide
        id: decide
        env:
          TYPE: ${{ steps.meta.outputs.update-type }}
          ECO: ${{ steps.meta.outputs.package-ecosystem }}
          NEW: ${{ steps.meta.outputs.new-version }}
          GROUPS: ${{ steps.meta.outputs.dependency-group }}
          UPDATES: ${{ steps.meta.outputs.updated-dependencies-json }}
        run: |
          reason=""
          zero_minor() { echo "$1" | grep -Eq '^0\.' ; }
          if [ "$ECO" = "github_actions" ]; then reason="workflow-file bumps stay manual"
          elif [ "$TYPE" != "version-update:semver-patch" ] && [ "$TYPE" != "version-update:semver-minor" ]; then reason="not a patch or minor update ($TYPE)"
          elif [ "$TYPE" = "version-update:semver-minor" ] && zero_minor "$NEW"; then reason="0.x minor bump counts as breaking"
          elif echo "$UPDATES" | jq -e 'map(select(.updateType != "version-update:semver-patch" and .updateType != "version-update:semver-minor" or (.updateType == "version-update:semver-minor" and (.newVersion | startswith("0."))))) | length > 0' >/dev/null; then reason="group contains a major or 0.x-minor bump"
          fi
          echo "reason=$reason" >> "$GITHUB_OUTPUT"
      - name: Enable auto-merge
        if: steps.decide.outputs.reason == ''
        env:
          PR_URL: ${{ github.event.pull_request.html_url }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: gh pr merge --auto --squash "$PR_URL"
      - name: Say why it was skipped
        if: steps.decide.outputs.reason != ''
        env:
          PR_URL: ${{ github.event.pull_request.html_url }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          REASON: ${{ steps.decide.outputs.reason }}
        run: |
          if ! gh pr view "$PR_URL" --json comments -q '.comments[].body' | grep -q 'Auto-merge skipped'; then
            gh pr comment "$PR_URL" --body "Auto-merge skipped: $REASON. A human merges this one."
          fi
```
