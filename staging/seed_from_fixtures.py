#!/usr/bin/env python3
"""Seed the staging world from the CAPTURED production payloads, so the
scheduler sees production-shaped values, not synthetic happy-path numbers.

Reads scheduler/tests/fixtures/states.json and writes the fake_* helpers
(and the real slider input_numbers) via the staging HA REST API.
"""

import json
import pathlib
import sys
import urllib.request

HERE = pathlib.Path(__file__).parent
HA_URL = "http://localhost:8123"
FIXTURES = HERE / ".." / "scheduler" / "tests" / "fixtures" / "states.json"

# fixture entity -> staging input_number to drive
MAP = {
    "sensor.current_grid_cost": "input_number.fake_price",
    "sensor.current_sonnen_consumption": "input_number.fake_consumption",
    "sensor.current_sonnen_production": "input_number.fake_pv",
    "sensor.energy_production_today": "input_number.fake_pv_today_kwh",
    "sensor.energy_production_tomorrow": "input_number.fake_pv_tomorrow_kwh",
    "sensor.humidity_average_inside": "input_number.fake_humidity_inside",
    "sensor.temp_average_inside": "input_number.fake_temp_inside",
    "sensor.temp_outside": "input_number.fake_temp_outside",
    # live-tuned sliders keep their production entity ids in staging
    "input_number.input_number_hot_water_runtime": "input_number.input_number_hot_water_runtime",
    "input_number.input_number_climate_aircon_target_temp": "input_number.input_number_climate_aircon_target_temp",
    "input_number.input_number_climate_aircon_run_below_price_kwh": "input_number.input_number_climate_aircon_run_below_price_kwh",
    "input_number.input_number_indoor_comfort_dehumidifier_max_price_kwh": "input_number.input_number_indoor_comfort_dehumidifier_max_price_kwh",
    "input_number.input_number_indoor_comfort_humidity_target_percent": "input_number.input_number_indoor_comfort_humidity_target_percent",
    "input_number.input_number_indoor_comfort_humidity_start_hysteresis_percent": "input_number.input_number_indoor_comfort_humidity_start_hysteresis_percent",
}


def main() -> int:
    token = (HERE / ".token").read_text().strip()
    states = json.loads(FIXTURES.read_text())
    seeded, skipped = [], []
    for src, dst in MAP.items():
        raw = states.get(src, {}).get("state")
        try:
            value = float(raw)
        except (TypeError, ValueError):
            skipped.append(f"{src}={raw!r}")
            continue
        req = urllib.request.Request(
            f"{HA_URL}/api/services/input_number/set_value",
            data=json.dumps({"entity_id": dst, "value": value}).encode(),
            headers={"Authorization": f"Bearer {token}",
                     "Content-Type": "application/json"},
        )
        urllib.request.urlopen(req, timeout=10).read()
        seeded.append(f"{dst} = {value} (from {src})")
    # future steps default to the captured current price
    cur = states.get("sensor.current_grid_cost", {}).get("state")
    try:
        value = float(cur)
        req = urllib.request.Request(
            f"{HA_URL}/api/services/input_number/set_value",
            data=json.dumps({"entity_id": "input_number.fake_price_future",
                             "value": value}).encode(),
            headers={"Authorization": f"Bearer {token}",
                     "Content-Type": "application/json"},
        )
        urllib.request.urlopen(req, timeout=10).read()
        seeded.append(f"input_number.fake_price_future = {value}")
    except (TypeError, ValueError):
        pass
    print(f"seeded {len(seeded)}:")
    for line in seeded:
        print(f"  {line}")
    if skipped:
        print(f"skipped (non-numeric in production fixture, as on live): {skipped}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
