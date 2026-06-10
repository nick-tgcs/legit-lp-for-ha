#!/usr/bin/env bash
# Scenario suite (S1-S6 from docs/PLAN.md) against the staging stack.
# Asserts the closed loop the unit tests can't: real recorder, real template
# engine, real automation chain. LP semantics themselves are owned by the
# scheduler's own test suite — staging proves the wiring.
set -euo pipefail
cd "$(dirname "$0")"

HA_URL="${HA_URL:-http://localhost:8123}"
SCHED_URL="${SCHED_URL:-http://localhost:8099}"
TOKEN="$(cat .token)"
INTERVAL="${SCHED_INTERVAL_SECONDS:-15}"
PASS=0 FAIL=0

say()  { printf '\n== %s\n' "$*"; }
ok()   { echo "   PASS: $*"; PASS=$((PASS+1)); }
bad()  { echo "   FAIL: $*"; FAIL=$((FAIL+1)); }

ha() { # ha GET|POST path [json]
  curl -fsS -m 10 -X "$1" -H "Authorization: Bearer $TOKEN" \
       -H 'Content-Type: application/json' ${3:+-d "$3"} "$HA_URL$2"
}
state()    { ha GET "/api/states/$1" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])'; }
turn()     { ha POST "/api/services/input_boolean/turn_$2" "{\"entity_id\": \"input_boolean.$1\"}" >/dev/null; }
set_num()  { ha POST "/api/services/input_number/set_value" "{\"entity_id\": \"input_number.$1\", \"value\": $2}" >/dev/null; }
status()   { curl -fsS -m 10 "$SCHED_URL/api/status"; }

# Wait until the latest solve report is newer than now-at-call; crude but
# sufficient: just wait one interval + slack, then read.
solve_wait() { sleep $((INTERVAL + 5)); }

field() { # field <jq-ish python expr over the status json>
  status | python3 -c "
import json, sys
r = json.load(sys.stdin)
loads = {l['id']: l for l in r.get('loads', [])}
print($1)
"
}

say "S5 panel: health, html, status schema"
curl -fsS -m 5 "$SCHED_URL/health" >/dev/null && ok "/health 200" || bad "/health"
curl -fsS -m 5 "$SCHED_URL/" | grep -qi '<html' && ok "/ serves html" || bad "/ html"
status | python3 -c 'import json,sys; r=json.load(sys.stdin); assert "loads" in r and "timestamp" in r' \
  && ok "/api/status schema" || bad "/api/status schema"

say "S1 dry-run parse: all production-shaped reads resolve"
solve_wait
PRICE_OK=$(field "r.get('price_now') is not None")
[[ "$PRICE_OK" == True ]] && ok "price_now parsed from staging forecast chain" || bad "price_now is null"
NLOADS=$(field "len(loads)")
[[ "$NLOADS" == 3 ]] && ok "3 loads in report" || bad "expected 3 loads, got $NLOADS"
for e in input_boolean.hot_water input_boolean.dehumidifier input_boolean.aircon; do
  [[ "$(state $e)" == off ]] && ok "$e untouched (dry-run)" || bad "$e flipped in dry-run!"
done

# Everything below needs a LIVE scheduler (SCHED_DRY_RUN=false).
if [[ "${LIVE:-0}" != 1 ]]; then
  echo; echo "LIVE=1 not set — skipping S2-S4/S6 (live-action scenarios)."
  echo "result: PASS=$PASS FAIL=$FAIL"; [[ $FAIL == 0 ]]
  exit
fi

say "S2 live start chain: humid + cheap -> dehumidifier starts via real automations"
turn automate on; turn dehumidifier_auto on; turn grid_power_use_lp_scheduler on
set_num fake_humidity_inside 80   # > target 65 + hysteresis 2
set_num fake_price 0.05; set_num fake_price_future 0.05
solve_wait; solve_wait
[[ "$(state switch.fake_dehumidifier)" == on ]] \
  && ok "fake switch ON through input_boolean -> automation -> template switch" \
  || bad "dehumidifier did not start (switch.fake_dehumidifier=$(state switch.fake_dehumidifier))"
[[ "$(state binary_sensor.indoor_comfort_dehumidifiers_running)" == on ]] \
  && ok "running sensor mirrors hardware" || bad "running sensor"
solve_wait
STRETCH=$(field "loads['dehumidifier'].get('running')")
[[ "$STRETCH" == True ]] && ok "scheduler reads running=true back from recorder chain" || bad "report running=$STRETCH"

say "S3 authority flip mid-run -> observe-only, device untouched"
turn dehumidifier_auto off
solve_wait
AUTH=$(field "loads['dehumidifier'].get('authority')")
[[ "$AUTH" == False ]] && ok "authority off in report" || bad "authority=$AUTH"
[[ "$(state switch.fake_dehumidifier)" == on ]] \
  && ok "running device left alone" || bad "observe-only mode touched the device"
turn dehumidifier_auto on

say "S4 scheduler restart -> state rebuilt from recorder, no duplicate start"
BEFORE=$(field "loads['dehumidifier'].get('starts_today')")
docker compose restart scheduler >/dev/null 2>&1 || docker restart lp-staging-scheduler >/dev/null
solve_wait; solve_wait
AFTER=$(field "loads['dehumidifier'].get('starts_today')")
[[ "$AFTER" == "$BEFORE" ]] \
  && ok "starts_today stable across restart ($AFTER)" \
  || bad "starts_today changed across restart: $BEFORE -> $AFTER"
[[ "$(state switch.fake_dehumidifier)" == on ]] && ok "no spurious flip on restart" || bad "device flipped on restart"

say "S6 surplus: import above every ceiling, PV covers the load -> still runs"
HOUR=$(date +%H)
if (( HOUR >= 9 && HOUR < 17 )); then
  set_num fake_price 0.60; set_num fake_price_future 0.60
  set_num fake_pv 6000; set_num fake_consumption 800
  set_num fake_humidity_inside 60   # below must-have, above can-take target 55
  solve_wait; solve_wait
  [[ "$(state switch.fake_dehumidifier)" == on ]] \
    && ok "can-take rides the surplus despite 0.60 import" \
    || bad "surplus did not pull the load"
  set_num fake_pv 0
else
  echo "   SKIP: outside the 09:00-17:00 can-take window (wall-clock $HOUR h)"
fi

echo; echo "result: PASS=$PASS FAIL=$FAIL"
[[ $FAIL == 0 ]]
