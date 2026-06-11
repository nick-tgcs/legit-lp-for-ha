# Contributing

Thanks for looking. This project has a strong opinion about *how* it's built, so
this file is mostly about that. The short version: **tests come first, hard rules
are absolute, and the LP is never mocked.**

## Test-driven development is the workflow, not a phase

Every change follows **red → green → refactor**:

1. **Red** — write the test(s) that describe the behaviour you want. Run them;
   watch them fail for the right reason.
2. **Green** — write the minimum code to make them pass.
3. **Refactor** — clean up under green.

Non-negotiables:

- **Every bug fix starts with a failing test** that reproduces it. No exceptions
  — a fix without a regression test isn't done.
- **Real fixtures only.** Parsing/behaviour tests use payloads captured from a
  real Home Assistant (`scheduler/tests/fixtures/`: real Amber forecast blobs,
  real `/history/period` responses, real `/states` bodies). Don't hand-invent
  response shapes — parsing bugs hide in the real ones. New shapes get captured,
  not imagined (see `scheduler/tests/fixtures/capture.py`).
- **The LP is never mocked.** Its behaviour *is* the product. Integration tests
  run the real HiGHS solver against synthetic worlds and assert on the decision.
  Only Home Assistant I/O is doubled (`RecordingHa` / wiremock).
- **Determinism.** `now` is injected — no module outside `main` reads the wall
  clock. The MILP is deterministic given fixed inputs; timer/SSE tests use
  tokio's paused clock. A flaky test is a bug.

## The test pyramid

| Layer | Lives in | Doubles | Runs |
|---|---|---|---|
| **Unit** | `#[cfg(test)]` in each module | none — pure functions | `cargo test`, ms |
| **Integration** | `scheduler/tests/*.rs` | mock `HaApi` (`RecordingHa`); **real HiGHS** | `cargo test` |
| **E2E** | `scheduler/tests/e2e.rs` | the real release binary + wiremock stub HA | `cargo test` |
| **Staging** | `staging/` | dockerised **real** HA, fake hardware | `staging/scenario.sh` |
| **Release** | `.claude/skills/verify-release` | the released GHCR image + throwaway HA | the skill |

Inside-out: pure core first, then module seams, then the whole binary, then a
real HA. The promotion path is:

> CI (unit + integration + stub e2e) → staging compose (S1–S6) → live HA in
> `dry_run: true` for a day of watched decisions → live.

## Running the tests

```bash
# the full gate CI runs — run this before AND after you change anything
make test          # cd scheduler && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

# while iterating
cd scheduler
cargo test                      # everything
cargo test --test lp            # one integration file
cargo test forecast             # one module's unit tests by name
```

Prerequisites: a Rust toolchain, plus **cmake + a C++ compiler** — `highs-sys`
compiles HiGHS from source. (`build-essential cmake clang libclang-dev` on
Debian/Ubuntu.)

### Staging — against a real HA in Docker

```bash
cd staging
docker compose up -d            # boots home-assistant:stable with the seeded config
./bootstrap.sh                  # onboards, mints a long-lived token → .token (git-ignored)
./scenario.sh                   # drives S1–S6 over the REST API
```

Staging mirrors the live contract surface with **fake hardware** (template
switches over helper booleans, scriptable price/PV/consumption), so a scheduler
`turn_on` produces a real state change the recorder logs and the next solve reads
back — with zero risk to any real device. What staging *can't* test (ingress, the
sidebar panel, the add-on options schema) gets one pre-release pass in HA's
add-on devcontainer.

## Code conventions

- **`thiserror` in the core, `anyhow` only at the `main.rs`/config boundary.**
  Core modules return typed `SchedulerError`.
- **Hard rules are constraints, never penalties.** If you find yourself adding a
  cost to discourage an *illegal* action, stop — it belongs in the constraint
  set. Penalties/preferences only ever rank *legal* options. The
  [explicit prohibitions](ARCHITECTURE.md#11-explicit-prohibitions) list is
  binding.
- **The planner consumes contracts, not brands.** No device-brand or
  integration-specific logic in the engine; it branches on `load_type`, never on
  a vendor.
- **Formatting** is enforced (`rustfmt.toml`); **clippy is `-D warnings`**. Both
  are in `make test`.
- Keep YAML declarative — no logic language in load contracts.

## Branching & PRs

- **`develop`** is the integration branch — open PRs against it. Every PR runs
  the `validate` workflow (test gate + Dockerfile build + HA add-on lint); it
  must be green to merge.
- **`main`** is released state only (the repo default branch HA Supervisor
  reads). It is updated **exclusively** by the release pipeline — don't target
  PRs at it.
- Branch names: `feat/…`, `fix/…`, `docs/…`. Keep commits focused; write a
  message that says *why*.
- Releases (version bump + image build + `develop` → `main` promotion + tag) are
  driven by CI — see [docs/RELEASING.md](docs/RELEASING.md).

## Definition of done

- [ ] New/changed behaviour is covered by a test that failed before your change.
- [ ] `make test` is green (fmt, clippy `-D warnings`, all test layers).
- [ ] Real fixtures used for any parsing/observation behaviour.
- [ ] No hard rule expressed as a penalty; no brand logic in the engine.
- [ ] Docs/`example.yaml` updated if you changed the contract or options surface.

When in doubt, the design of record is [docs/PLAN.md](docs/PLAN.md) and the
governing spec is [ARCHITECTURE.md](ARCHITECTURE.md).
