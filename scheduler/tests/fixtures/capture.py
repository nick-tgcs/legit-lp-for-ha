#!/usr/bin/env python3
"""Capture REAL Home Assistant payloads as test fixtures.

Auth pattern matches the repo's pull_ha_dashboard.sh: read a refresh token from
/config/.storage/auth over SSH, exchange it for a short-lived access token.
No secrets are written to the fixtures.

Usage: python3 capture.py  (writes *.json next to itself)
"""

import json
import pathlib
import subprocess
import sys
import urllib.parse
import urllib.request
from datetime import datetime, time, timedelta

REMOTE = "root@ha.ngura.agren.au"
API = "http://192.168.51.10:8123"
OUT = pathlib.Path(__file__).parent

STATE_ENTITIES = [
    "sensor.current_grid_cost",
    "sensor.amber_electric_feedin",
    "sensor.current_sonnen_consumption",
    "sensor.current_sonnen_production",
    "sensor.energy_production_today",
    "sensor.energy_production_tomorrow",
    "sensor.power_production_now",
    "binary_sensor.hot_water_automated",
    "binary_sensor.dehumidifier_automated",
    "binary_sensor.aircon_automated",
    "binary_sensor.indoor_comfort_hot_water_running",
    "binary_sensor.indoor_comfort_dehumidifiers_running",
    "climate.ac_0",
    "sensor.humidity_average_inside",
    "sensor.temp_average_inside",
    "sensor.temp_outside",
    "input_number.input_number_hot_water_runtime",
    "input_number.input_number_climate_aircon_target_temp",
    "input_number.input_number_climate_aircon_run_below_price_kwh",
    "input_number.input_number_indoor_comfort_dehumidifier_max_price_kwh",
    "input_number.input_number_indoor_comfort_humidity_target_percent",
    "input_number.input_number_indoor_comfort_humidity_start_hysteresis_percent",
]

HISTORY_ENTITIES = {
    "history_hot_water_running.json": "binary_sensor.indoor_comfort_hot_water_running",
    "history_climate_ac0.json": "climate.ac_0",
}


def token() -> str:
    auth = json.loads(
        subprocess.run(
            ["ssh", REMOTE, "cat /config/.storage/auth"],
            capture_output=True, text=True, check=True,
        ).stdout
    )
    rt = next(
        t for t in auth["data"]["refresh_tokens"]
        if t.get("token_type") == "normal" and t.get("client_id")
    )
    data = urllib.parse.urlencode({
        "grant_type": "refresh_token",
        "refresh_token": rt["token"],
        "client_id": rt["client_id"],
    }).encode()
    with urllib.request.urlopen(f"{API}/auth/token", data=data) as r:
        return json.load(r)["access_token"]


def get(tok: str, path: str):
    req = urllib.request.Request(
        f"{API}{path}", headers={"Authorization": f"Bearer {tok}"}
    )
    with urllib.request.urlopen(req) as r:
        return json.load(r)


def main() -> None:
    tok = token()

    # Forecast: the provider attribute blob (Amber shape -> field-map fixture).
    forecast = get(tok, "/api/states/sensor.beckton_general_forecast")
    (OUT / "forecast_amber.json").write_text(json.dumps(forecast, indent=2))

    # States: one bundle keyed by entity id.
    states = {e: get(tok, f"/api/states/{e}") for e in STATE_ENTITIES}
    (OUT / "states.json").write_text(json.dumps(states, indent=2))

    # History since local midnight for the running entities.
    midnight = datetime.combine(datetime.now().date(), time()).astimezone()
    end = datetime.now().astimezone()
    for fname, entity in HISTORY_ENTITIES.items():
        q = urllib.parse.urlencode({
            "filter_entity_id": entity,
            "end_time": end.isoformat(),
            "minimal_response": "",
        })
        hist = get(tok, f"/api/history/period/{midnight.isoformat()}?{q}")
        (OUT / fname).write_text(json.dumps(hist, indent=2))

    # Yesterday's full day too — a closed 24h window exercises the fold better.
    y_start = midnight - timedelta(days=1)
    q = urllib.parse.urlencode({
        "filter_entity_id": HISTORY_ENTITIES["history_hot_water_running.json"],
        "end_time": midnight.isoformat(),
        "minimal_response": "",
    })
    hist = get(tok, f"/api/history/period/{y_start.isoformat()}?{q}")
    (OUT / "history_hot_water_yesterday.json").write_text(json.dumps(hist, indent=2))

    for f in sorted(OUT.glob("*.json")):
        print(f"{f.name}: {f.stat().st_size} bytes")


if __name__ == "__main__":
    sys.exit(main())
