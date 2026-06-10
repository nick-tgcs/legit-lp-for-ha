---
name: verify-release
description: Verify a published release of the LP scheduler add-on end-to-end against a throwaway dockerised Home Assistant that mirrors the production contract surface. Use after cutting a release (or to re-verify any version) — pulls the image anonymously from GHCR exactly like a user's Supervisor, boots a seeded real HA, and drives live scenarios through the real automation/recorder chain.
---

# Verify a release against staging HA

## One command

```bash
cd staging && ./verify_release.sh            # latest GitHub release
cd staging && ./verify_release.sh 0.1.0      # specific version
KEEP=1 ./verify_release.sh                   # leave the stack up to inspect
```

Requires: docker + compose, network access to ghcr.io / api.github.com.
Runtime: ~5-10 min (HA boot dominates). Everything is throwaway: config is
copied to `staging/.runtime/`, compose volumes are removed on exit.

## What it proves, step by step

1. **Anonymous GHCR pull** — the exact pull a user's Supervisor performs.
   Fails if the package went private or the manifest/arch list regressed.
   Also prints the `io.hass.*` labels for eyeballing.
2. **Real HA boots the seed** (`staging/ha_config/`) — production entity ids
   over fake hardware: authority templates, `*_start_stop` automations,
   label-discovery sensors, all live-tuned sliders, scriptable
   price/PV/consumption, an Amber-shaped `forecasts[]` template sensor, and
   `climate.ac_0` as a real climate entity (generic_thermostat).
3. **bootstrap.sh** — headless onboarding + long-lived token into `.token`.
4. **seed_from_fixtures.py** — replays captured PRODUCTION payloads
   (`scheduler/tests/fixtures/states.json`) into the fake helpers, so the
   scheduler parses production-shaped values.
5. **Dry-run scheduler** (released image, env-var config path, registry
   self-seeded from the bundled example) → **S1/S5**: every read parses,
   3 loads reported, zero control flips, panel + `/health` + status schema.
6. **Live scheduler** → **S2** start chain (humid + cheap → input_boolean →
   automation → template switch → running sensor → recorder → next solve
   reads it back), **S3** authority flip = observe-only with the device left
   running, **S4** container restart = accumulators rebuilt from recorder
   with no duplicate start, **S6** surplus (import above every ceiling, PV
   covering the draw → can-take still runs; skipped outside 09:00-17:00).

## Reading failures

- `anonymous pull` failed → GHCR package visibility (one-time flip: package
  settings → Change visibility → Public) or the release never pushed the tag.
- `bootstrap` failed → HA didn't boot: `docker logs lp-staging-ha`.
- Scenario FAIL lines name the assertion; scheduler logs are dumped
  automatically. `KEEP=1` + `http://localhost:8123` (user `staging` /
  `staging-Pass1`) and `http://localhost:8099` to poke around.

## Known limits (by design)

- Supervisor-only surfaces are NOT covered: ingress, sidebar panel entry,
  options UI, `SUPERVISOR_TOKEN`, HEALTHCHECK-driven watchdog restarts.
  Those get a one-time pass on the real HA.
- LP semantics (cost-shifting, min-run locks, budgets) are owned by the
  crate's 90+ unit/integration tests — staging proves the *wiring* through
  a real HA, not the solver maths.
- S6 and any window-scoped behaviour follow the host wall clock
  (Australia/Sydney in compose); out-of-window scenarios self-skip.
- Stop-after-min_run isn't asserted (min_run is 30 real minutes); covered
  by the LP test suite instead.
