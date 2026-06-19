# Branching & releasing

## Model

- **`develop`** — integration branch, **PR-only** (ruleset `protect-develop`:
  PR required, no force pushes, no deletion). Every PR runs the `validate`
  workflow, which has five jobs:
  - **`test`** — the full Rust gate (fmt, clippy `-D warnings`, all
    unit/integration/e2e tests);
  - **`build`** — proves the repo-root Dockerfile still builds (amd64);
  - **`addon-lint`** — HA add-on config linter (schema/manifest sanity);
  - **`audit`** — `cargo audit`, fails on any RUSTSEC advisory in the
    dependency tree (an in-PR complement to Dependabot security updates);
  - **`coverage`** — `cargo tarpaulin`, fails if line coverage drops below the
    floor in the Makefile (`COVERAGE_FLOOR`); uploads an HTML report artifact.

  All five are **required (merge-blocking)** on `develop`. `test`/`build`/
  `addon-lint` are enforced by the `protect-develop` ruleset; `audit`/`coverage`
  by a classic branch-protection rule on `develop`. GitHub enforces the **union**
  of rulesets and branch protection, so a PR needs all five green to merge. The
  split is a token limitation, not a design choice: a classic OAuth token can
  write branch protection but not rulesets (the rulesets API 404s for it), so the
  two newer checks live in branch protection. Fold them into the ruleset with an
  admin fine-grained PAT whenever convenient — the enforcement is identical
  either way.
- **`main`** — released state only, and the repo's **default branch**. HA
  Supervisor clones a custom add-on repository's *default* branch and
  hard-resets to its tip on every store refresh — whatever is on `main` is
  immediately what users' Supervisors read. `main` is updated exclusively by
  the release pipeline.
- Pre-release channel for free: add the repo to HA as
  `https://github.com/nick-tgcs/legit-lp-for-ha#develop` (Supervisor supports
  a `#branch` URL fragment).

## Cutting a release

**Validate off `develop`, build off `main` — and your merge is the release gate.**

1. On `develop`, bump `version:` in `addon/config.yaml` (the single source of
   truth — Supervisor pulls `image:<version>`, so the config version and the
   GHCR image tag must stay in lockstep). This lands via a normal PR, so it is
   `test`-gated like anything else.
2. **`make release`** opens a `develop` → `main` PR **with your own credentials**
   (a plain `gh pr create` — no bot creates or approves it; that is the whole
   point). CI runs the `validate` workflow on the PR (`pull_request` trigger);
   the required check on `main` is `test` (per `protect-main`).
3. **You review, approve, and merge it on GitHub.** Merging `main` is a *push to
   `main`*, which triggers **`release.yml`** to build the multi-arch image (amd64
   + arm64) off `main`, push it to GHCR as
   `ghcr.io/nick-tgcs/legit-lp-for-ha:<version>` and `:latest` (with
   `io.hass.version` stamped), then tag `v<version>` and cut the GitHub release
   with generated notes.

That is it — no local build step, and no bot in the merge path. `release.yml` is
idempotent: if `v<version>` is already tagged it no-ops, so a re-run (or a push
to `main` that didn't bump the version) cannot double-publish. `make
release-build` is an escape hatch that re-runs just the build-off-`main` step
(e.g. if the push-triggered build didn't fire after a merge).

**Why a human merge, not an auto-merge bot:** the release gate is a deliberate
human approval — you merge the PR, and that merge is what releases. A bonus: a
human can land `.github/workflows/` changes on `main`, which the Actions
`GITHUB_TOKEN` refuses to *push* (`refusing to allow a GitHub App to … update
workflow … without 'workflows' permission`) — so even the workflow files release
cleanly through your merge.

**Ordering note (build off `main`):** because the image is built *after* `main`
advances, `main` advertises the new version for the build's duration. That is
harmless here — the add-on ships inert and nothing auto-pulls; Supervisor fetches
the new image only when you run `lp-setup update`, which you do *after* the
`release.yml` build is green. (The old flow published the image before moving
`main`; building off `main` inverts that, by design.)

## Protection on `main`

A repository ruleset (`protect-main`) enforces: no deletion, no force pushes,
and the `test` status check. The release goes through a `develop` → `main` PR so
there is a human approval gate and a place for CI to rerun `test`; you merge it
on green. The result is a merge commit, which the `non_fast_forward` rule allows
— that rule blocks history-rewriting force pushes, not ordinary descendant
merges. `main`'s tree then equals develop's validated tip, and that merge (a push
to `main`) triggers `release.yml` to build off `main`.

## Dependency updates (Dependabot)

Dependabot opens weekly version-update PRs (cargo, github-actions, docker) against
`develop`, never `main`. Each runs the same required gate as any PR and, once all
required checks are green, **merges itself** — `dependabot-auto-merge.yml` enables
squash auto-merge and GitHub completes it only after the checks pass (a red check
holds the PR for a human).

**Every update type self-merges, majors included**, because the gate actually
exercises the bump rather than just observing it:

- **cargo** bumps are compiled and run through the full unit/integration/e2e
  suite plus the coverage floor and `cargo audit`. A major that survives all of
  that is validated at the API and behaviour level the tests assert.
- **actions used in PR CI** (`actions/checkout`, `docker/setup-buildx-action`,
  `docker/build-push-action`) run at their bumped version — a `pull_request` run
  uses the PR's own workflow files, so the version under test is the one that
  executes.

Two actions a PR cannot exercise: `docker/setup-qemu-action` and
`docker/login-action` only run in `release.yml` (the multi-arch image *push*, on
`main`), which no PR triggers. Their bumps still auto-merge; the backstop is
`release.yml`, which builds multi-arch off `main` and fails before it tags if a
bump broke it. The worst case is a red `release.yml` after the promote merged —
`main` has advanced but no image/tag was published (never a *bad* image, just no
new one); the add-on ships inert and updates are manual, so there is no live
impact. Fix the bump on `develop` and re-release. Moving the arm64 build to a
native `ubuntu-24.04-arm` runner would delete the QEMU dependency and let a
native matrix build exercise the push path on PRs too — a worthwhile follow-up,
not required for safety.

Why this is safe to fully automate: only PRs authored by `dependabot[bot]`
self-merge (the actor gate — nothing else does), and they land on `develop`, not
`main`. A surprising bump is caught in integration well before any release, and a
release is a deliberate one-command step (`make release`) on top.

## Build notes

- `home-assistant/builder` (the legacy action) was **retired April 2026**; the
  pipeline uses plain `docker/build-push-action` (the hassio-addons org
  pattern), which means the `io.hass.*` labels are our responsibility — they
  are set in the Dockerfile/workflow as described above.
- The image is a single multi-arch manifest list under one generic name
  (no `{arch}` template) — the preferred form per the 2026 HA developer docs;
  Supervisor pulls with an explicit platform and resolves the manifest.

## First-release checklist (one-time)

- After the first publish, verify the GHCR package is **publicly pullable**:
  `docker manifest inspect ghcr.io/nick-tgcs/legit-lp-for-ha:<version>`
  without credentials. If denied, flip it once: GitHub → package →
  Package settings → Change visibility → Public. (Supervisor pulls
  anonymously.)
- Then add `https://github.com/nick-tgcs/legit-lp-for-ha` as a repository in
  HA (Settings → Add-ons → Store → ⋮ → Repositories) and install
  **Legit LP Scheduler**. It starts in `dry_run: true` and self-seeds its
  registry config on first boot.
