//! Fixture sanity — pins the shape of the captured real-HA payloads so a bad
//! `capture.py` refresh fails fast, before forecast/history modules consume
//! them. Shape only, never values (fixtures get refreshed).

use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

#[test]
fn forecast_has_provider_slots_in_field_map_shape() {
    let f = fixture("forecast_amber.json");
    let slots = f["attributes"]["forecasts"].as_array().expect("attributes.forecasts is an array");
    assert!(!slots.is_empty(), "forecast slots present");
    for s in slots {
        // The exact fields the configured field-map renames into the
        // canonical schema: start/end/import_per_kwh.
        let start = s["start_time"].as_str().expect("slot.start_time");
        let end = s["end_time"].as_str().expect("slot.end_time");
        assert!(
            chrono::DateTime::parse_from_rfc3339(start).is_ok(),
            "start_time is RFC3339: {start}"
        );
        assert!(chrono::DateTime::parse_from_rfc3339(end).is_ok(), "end_time is RFC3339: {end}");
        assert!(s["per_kwh"].as_f64().is_some(), "slot.per_kwh numeric");
        assert!(end > start, "end after start");
    }
    // Sorted by start: required by the canonical contract's parser rules.
    let starts: Vec<&str> = slots.iter().map(|s| s["start_time"].as_str().unwrap()).collect();
    assert!(starts.windows(2).all(|p| p[0] <= p[1]), "slots sorted by start");
}

fn assert_history_shape(name: &str) {
    let h = fixture(name);
    let lists = h.as_array().expect("history is an array of entity lists");
    assert_eq!(lists.len(), 1, "one entity requested");
    let rows = lists[0].as_array().expect("entity history is an array");
    assert!(!rows.is_empty(), "history has rows");
    assert!(rows[0]["entity_id"].as_str().is_some(), "first row carries entity_id");
    for r in rows {
        assert!(r["state"].as_str().is_some(), "row has state");
        let ts = r["last_changed"].as_str().or(r["last_updated"].as_str());
        let ts = ts.expect("row has a timestamp");
        assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "timestamp RFC3339");
    }
}

#[test]
fn hot_water_history_today_shape() {
    assert_history_shape("history_hot_water_running.json");
}

#[test]
fn hot_water_history_yesterday_shape() {
    assert_history_shape("history_hot_water_yesterday.json");
}

#[test]
fn climate_history_shape_even_when_unavailable() {
    // climate.ac_0 was genuinely 'unavailable' at capture time — that is the
    // degraded-state fixture and it must still satisfy the base shape.
    assert_history_shape("history_climate_ac0.json");
}

#[test]
fn states_bundle_covers_every_contract_entity() {
    let s = fixture("states.json");
    let required = [
        // pricing
        "sensor.current_grid_cost",
        "sensor.amber_electric_feedin",
        // site power + solar forecast
        "sensor.current_sonnen_consumption",
        "sensor.current_sonnen_production",
        "sensor.energy_production_today",
        "sensor.energy_production_tomorrow",
        "sensor.power_production_now",
        // authority
        "binary_sensor.hot_water_automated",
        "binary_sensor.dehumidifier_automated",
        "binary_sensor.aircon_automated",
        // running + observed state
        "binary_sensor.indoor_comfort_hot_water_running",
        "binary_sensor.indoor_comfort_dehumidifiers_running",
        "climate.ac_0",
        "sensor.humidity_average_inside",
        "sensor.temp_average_inside",
        "sensor.temp_outside",
        // live-tuned ValueRefs
        "input_number.input_number_hot_water_runtime",
        "input_number.input_number_climate_aircon_target_temp",
        "input_number.input_number_climate_aircon_run_below_price_kwh",
        "input_number.input_number_indoor_comfort_dehumidifier_max_price_kwh",
        "input_number.input_number_indoor_comfort_humidity_target_percent",
        "input_number.input_number_indoor_comfort_humidity_start_hysteresis_percent",
    ];
    for e in required {
        let body = &s[e];
        assert!(body.is_object(), "states bundle has {e}");
        assert!(body["state"].as_str().is_some(), "{e} has a state string");
        assert!(body["attributes"].is_object(), "{e} has attributes");
    }
}
