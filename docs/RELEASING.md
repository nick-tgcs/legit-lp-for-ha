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

  Required (merge-blocking) checks: `test`, `build`, `addon-lint`. `audit` and
  `coverage` run on every PR but are **advisory until added to the ruleset's
  required list** (the ruleset write needs an admin fine-grained PAT / the web
  UI — a classic OAuth token gets a masked 404). Add them under
  Settings → Rules → `protect-develop` → Require status checks.
- **`main`** — released state only, and the repo's **default branch**. HA
  Supervisor clones a custom add-on repository's *default* branch and
  hard-resets to its tip on every store refresh — whatever is on `main` is
  immediately what users' Supervisors read. `main` is updated exclusively by
  the release pipeline.
- Pre-release channel for free: add the repo to HA as
  `https://github.com/nick-tgcs/legit-lp-for-ha#develop` (Supervisor supports
  a `#branch` URL fragment).

## Cutting a release

1. On `develop`, bump `version:` in `addon/config.yaml` (the version is the
   single source of truth — Supervisor pulls `image:<version>`, so the config
   version and the GHCR image tag must be in lockstep).
2. Run the **release** workflow on the `develop` branch
   (Actions → release → Run workflow, or `make release`).

The pipeline then, in this order (the order matters — the store reads `main`'s
tip immediately, so the image must be pullable *before* the version lands on
`main`):

1. full test gate (`make test`) — this is the `test` status check `main`'s
   ruleset requires, so the promotion push below is self-authorising;
2. multi-arch image (amd64 + arm64) built and pushed to GHCR as
   `ghcr.io/nick-tgcs/legit-lp-for-ha:<version>` and `:latest`, with
   `io.hass.version` stamped (`io.hass.arch`/`io.hass.type` come from the
   Dockerfile);
3. fast-forward push of `develop` onto `main`;
4. git tag `v<version>` + GitHub release with generated notes.

Re-running a release for an existing tag fails fast: bump the version first.

## Protection on `main`

A repository ruleset (`protect-main`) enforces: no deletion, no force pushes,
and the `test` status check on every pushed commit. There is deliberately no
PR requirement on `main` — the release pipeline pushes directly, and its own
`test` job satisfies the required check on the promoted SHA. (PR gating
happens on the way into `develop`; a PR-only main would need a write deploy
key as a ruleset bypass — considered and declined to keep releases simple.)

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
