# Staging: real HA in Docker

1. `docker compose up -d` — boots HA Core with the seeded `ha_config/`
   (same contract surface as live: authority templates, *_start_stop
   automations on fake switches, scriptable price/PV/consumption helpers).
2. `./bootstrap.sh` — onboards via the HTTP API, mints a long-lived token
   into `.token` (git-ignored).
3. Run the scheduler against it:
   `SCHED_HASS_URL=http://localhost:8123 SCHED_TOKEN=$(cat .token) \
    SCHED_LOADS_CONFIG=../addon/example.yaml cargo run`
4. Drive scenarios S1-S6 (see docs/PLAN.md) with `./scenario.sh`.

Seeding of ha_config/ is the next staging task (S-milestone in the plan);
this directory ships the harness so the config lands under test.
