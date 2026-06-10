# Staging: release verification against a real HA in Docker

**One command:** `./verify_release.sh [version]` — pulls the released image
anonymously from GHCR (the Supervisor pull path), boots a throwaway real HA
from the production-mirroring seed, bootstraps auth, replays captured
production fixture values, and drives scenarios S1-S6 (dry-run, then live
against the fake hardware). `KEEP=1` leaves the stack up for inspection.
See `.claude/skills/verify-release/SKILL.md` for the full contract.

## Pieces (composable by hand for development)

1. `docker compose up -d homeassistant` — real HA Core with `ha_config/`:
   production entity ids over fake hardware (authority templates,
   `*_start_stop` automations, label-discovery sensors, live-tuned sliders,
   scriptable price/PV/consumption, Amber-shaped forecast template,
   `climate.ac_0` via generic_thermostat).
2. `./bootstrap.sh` — headless onboarding + long-lived token -> `.token`.
3. `python3 seed_from_fixtures.py` — production values from the captured
   fixtures into the fake helpers.
4. Scheduler, either the released image
   (`export SCHED_TOKEN=$(cat .token); docker compose --profile scheduler up scheduler`)
   or a local build:
   `SCHED_HASS_URL=http://localhost:8123 SCHED_TOKEN=$(cat .token) \
    SCHED_LOADS_CONFIG=../addon/example.yaml cargo run`
5. `./scenario.sh` (S1/S5; add `LIVE=1` for S2-S4/S6 with a live scheduler).

Sanity-check the seed without booting:
`docker run --rm -v "$PWD/ha_config:/config:ro" ghcr.io/home-assistant/home-assistant:stable \
   python -m homeassistant --script check_config -c /config`

## What this tier cannot test

Supervisor-only surfaces (ingress, sidebar panel, options UI,
`SUPERVISOR_TOKEN`, watchdog-via-HEALTHCHECK) — one-time pass on the real HA.
Wall-clock-dependent windows self-skip outside their hours; LP semantics are
owned by the crate's test suite, staging proves the wiring.
