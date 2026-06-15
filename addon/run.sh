#!/usr/bin/with-contenv bashio
export RUST_LOG="$(bashio::config 'log_level')"
export SCHED_INTERVAL_SECONDS="$(bashio::config 'interval_seconds')"
export SCHED_DRY_RUN="$(bashio::config 'dry_run')"
export SCHED_TIME_ZONE="$(bashio::config 'time_zone')"
export SCHED_LOADS_CONFIG="$(bashio::config 'loads_config_path')"
# Only export the explicit HA connection when the user actually set it —
# bashio::config yields the literal "null" for an unset optional, which would
# otherwise make the scheduler build a bogus "null/api" base URL. Left unset,
# the scheduler uses the Supervisor proxy + SUPERVISOR_TOKEN.
if bashio::config.has_value 'hass_url'; then
    export SCHED_HASS_URL="$(bashio::config 'hass_url')"
fi
if bashio::config.has_value 'long_lived_token'; then
    export SCHED_TOKEN="$(bashio::config 'long_lived_token')"
fi
# SUPERVISOR_TOKEN is provided by the Supervisor automatically.
exec /usr/local/bin/legit-lp-scheduler
