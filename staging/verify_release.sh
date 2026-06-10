#!/usr/bin/env bash
# Verify a RELEASE end-to-end against a throwaway real Home Assistant:
#   pull the released image anonymously from GHCR (the Supervisor pull path),
#   boot HA with the production-mirroring seed, bootstrap auth, seed values
#   from captured production fixtures, run the scheduler, drive scenarios.
#
# Usage: ./verify_release.sh [version]   (default: latest GitHub release tag)
#        KEEP=1 ./verify_release.sh      keeps the stack up for inspection
set -euo pipefail
cd "$(dirname "$0")"

IMAGE=ghcr.io/nick-tgcs/legit-lp-for-ha
VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
  VERSION=$(curl -fsS https://api.github.com/repos/nick-tgcs/legit-lp-for-ha/releases/latest \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"].lstrip("v"))')
fi
echo "verify: release $VERSION"

cleanup() {
  if [[ "${KEEP:-0}" != 1 ]]; then
    docker compose --profile scheduler down -v >/dev/null 2>&1 || true
    rm -rf .runtime
  else
    echo "KEEP=1: stack left running (docker compose --profile scheduler down -v to stop)"
  fi
}
trap cleanup EXIT

fail() { echo "VERIFY FAILED: $*"; docker logs lp-staging-scheduler 2>&1 | tail -40 || true; exit 1; }

echo "verify: 1/6 anonymous GHCR pull (the Supervisor path)"
docker pull "$IMAGE:$VERSION" >/dev/null || fail "anonymous pull of $IMAGE:$VERSION (package private?)"
docker image inspect "$IMAGE:$VERSION" --format 'verify:   labels io.hass.version={{index .Config.Labels "io.hass.version"}} io.hass.type={{index .Config.Labels "io.hass.type"}} io.hass.arch={{index .Config.Labels "io.hass.arch"}}'

echo "verify: 2/6 fresh HA from the seed"
rm -rf .runtime && mkdir -p .runtime
cp -r ha_config .runtime/ha_config
export HA_CONFIG_DIR=./.runtime/ha_config
export LP_VERSION="$VERSION"
docker compose up -d homeassistant

echo "verify: 3/6 bootstrap auth"
rm -f .token .refresh
./bootstrap.sh || fail "bootstrap"
export SCHED_TOKEN="$(cat .token)"

echo "verify: 4/6 seed production fixture values"
python3 seed_from_fixtures.py || fail "fixture seed"

echo "verify: 5/6 scheduler (dry-run) + S1/S5"
SCHED_DRY_RUN=true docker compose --profile scheduler up -d scheduler
./scenario.sh || fail "dry-run scenarios"

echo "verify: 6/6 scheduler (live) + S2-S4/S6"
docker compose stop scheduler >/dev/null
SCHED_DRY_RUN=false docker compose --profile scheduler up -d scheduler
LIVE=1 ./scenario.sh || fail "live scenarios"

echo
echo "VERIFY PASSED: release $VERSION end-to-end against a real Home Assistant"
