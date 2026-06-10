#!/usr/bin/with-contenv bashio
export RUST_LOG="$(bashio::config 'log_level')"
export SCHED_INTERVAL_SECONDS="$(bashio::config 'interval_seconds')"
export SCHED_DRY_RUN="$(bashio::config 'dry_run')"
export SCHED_TIME_ZONE="$(bashio::config 'time_zone')"
export SCHED_LOADS_CONFIG="$(bashio::config 'loads_config_path')"
export SCHED_HASS_URL="$(bashio::config 'hass_url')"
export SCHED_TOKEN="$(bashio::config 'long_lived_token')"
# SUPERVISOR_TOKEN is provided by the Supervisor automatically.
exec /usr/local/bin/legit-lp-scheduler
