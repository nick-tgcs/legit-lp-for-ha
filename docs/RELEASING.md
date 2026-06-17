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

1. On `develop`, bump `version:` in `addon/config.yaml` (the single source of
   truth — Supervisor pulls `image:<version>`, so the config version and the
   GHCR image tag must stay in lockstep).
2. **`make release`** (or Actions → release → Run workflow on `develop`). CI:
   - runs the full test gate (`make test`);
   - builds the multi-arch image (amd64 + arm64) and pushes it to GHCR as
     `ghcr.io/nick-tgcs/legit-lp-for-ha:<version>` and `:latest`, with
     `io.hass.version` stamped (`io.hass.arch`/`io.hass.type` come from the
     Dockerfile).

   Then it **stops — it does not touch `main`.**
3. **`make promote`** (locally, once the build above has published the image):
   fast-forwards `main` onto `develop`'s tip, tags `v<version>`, and creates the
   GitHub release with generated notes. Refuses if the tag already exists or the
   image isn't on GHCR yet.

Why the split: advancing `main` across `.github/workflows/` changes needs a
credential the Actions `GITHUB_TOKEN` lacks — it refuses to push workflow files
(`refusing to allow a GitHub App to … update workflow … without 'workflows'
permission`). A developer's SSH key has no such limit, so the promote runs from
the CLI. The order matters — the store reads `main`'s tip immediately, so the
image is published (step 2, in CI) *before* `main` moves (step 3). Re-running a
release for an existing tag fails fast: bump the version first.

## Protection on `main`

A repository ruleset (`protect-main`) enforces: no deletion, no force pushes,
and the `test` status check on every pushed commit. There is deliberately no
PR requirement on `main` — `make promote` fast-forwards directly, and the
promoted SHA already carries a green `test` from develop's CI, satisfying the
required check. (PR gating happens on the way into `develop`; a PR-only main
would need a deploy-key bypass — considered and declined to keep releases
simple.)

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
`docker/login-action` only run during the release's multi-arch image *push*
(`workflow_dispatch`), which no PR triggers. Their bumps still auto-merge; the
backstop is `make release`, which builds multi-arch and fails before publishing
if a bump broke it. Because the add-on ships inert and releases are manual, the
worst case is a red release you fix before `make promote` — never a bad image on
`main`. Moving the arm64 build to a native `ubuntu-24.04-arm` runner would delete
the QEMU dependency and let a native matrix build exercise the push path on PRs
too — a worthwhile follow-up, not required for safety.

Why this is safe to fully automate: only PRs authored by `dependabot[bot]`
self-merge (the actor gate — nothing else does), and they land on `develop`, not
`main`. A surprising bump is caught in integration well before any release, and a
release is a deliberate manual step on top.

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
