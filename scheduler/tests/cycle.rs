//! One full solve cycle over the module seams: real example registry, real
//! fixture payloads, RecordingHa, real HiGHS.

use legit_lp_scheduler::config;
use legit_lp_scheduler::cycle::Cycle;
use legit_lp_scheduler::ha_client::{history_rows, RecordingHa};
use legit_lp_scheduler::lp::LpPlanner;
use legit_lp_scheduler::profile::Profiles;
use legit_lp_scheduler::testkit::sydney;
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn registry() -> config::RegistryConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../addon/example.yaml");
    config::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// RecordingHa preloaded with the REAL captured states + forecast + history.
fn canned_ha() -> RecordingHa {
    let mut ha = RecordingHa::default();
    for (k, v) in fixture("states.json").as_object().unwrap() {
        ha.states.insert(k.clone(), v.clone());
    }
    ha.states.insert(
        "input_boolean.grid_power_use_lp_scheduler".into(),
        json!({"state": "on", "attributes": {}}),
    );
    ha.states.insert("sensor.beckton_general_forecast".into(), fixture("forecast_amber.json"));
    ha.history.insert(
        "binary_sensor.indoor_comfort_hot_water_running".into(),
        history_rows(&fixture("history_hot_water_running.json")).unwrap(),
    );
    ha.history
        .insert("climate.ac_0".into(), history_rows(&fixture("history_climate_ac0.json")).unwrap());
    ha.history.insert(
        "binary_sensor.indoor_comfort_dehumidifiers_running".into(),
        history_rows(&json!([[{"entity_id": "x", "state": "off",
            "last_changed": "2026-06-09T14:00:00+00:00"}]]))
        .unwrap(),
    );
    ha
}

fn set_state(ha: &mut RecordingHa, entity: &str, state: &str) {
    ha.states.insert(entity.into(), json!({"state": state, "attributes": {}}));
}

fn cycle(dry_run: bool) -> Cycle {
    Cycle {
        registry: registry(),
        planner: LpPlanner { grid_minutes: 15, horizon_hours: 24 },
        dry_run,
        profile_path: None,
    }
}

#[tokio::test]
async fn c1_dry_run_cycle_produces_report_and_zero_calls() {
    let ha = canned_ha();
    let mut profiles = Profiles::default();
    let report = cycle(true).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    assert_eq!(report.loads.len(), 3);
    assert!(report.dry_run && report.global_enabled);
    assert!(ha.calls.lock().unwrap().is_empty(), "dry-run made calls!");
    // climate.ac_0 was genuinely 'unavailable' at capture -> observe-only.
    let aircon = report.loads.iter().find(|l| l.id == "aircon").unwrap();
    assert!(aircon.reason.contains("observe-only"), "{}", aircon.reason);
    assert!(!report.grid.is_empty());
}

#[tokio::test]
async fn c2_live_cycle_issues_exactly_the_planned_call() {
    let mut ha = canned_ha();
    // Humid house, price between hot-water ct ceiling (0.10) and dehumidifier
    // mh ceiling (0.15): exactly ONE start is legal - the dehumidifier.
    set_state(&mut ha, "sensor.humidity_average_inside", "80.0");
    set_state(&mut ha, "sensor.current_grid_cost", "0.12");
    // The CAPTURED authority states are genuinely off (live system today);
    // grant authority for the live-action scenario.
    set_state(&mut ha, "binary_sensor.dehumidifier_automated", "on");
    set_state(&mut ha, "binary_sensor.hot_water_automated", "on");
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    let calls = ha.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "exactly one call: {calls:?}");
    assert_eq!(calls[0].domain, "input_boolean");
    assert_eq!(calls[0].service, "turn_on");
    assert_eq!(calls[0].target_entity, "input_boolean.dehumidifier");
    let d = report.loads.iter().find(|l| l.id == "dehumidifier").unwrap();
    assert!(d.executed);
}

#[tokio::test]
async fn c3_degraded_read_holds_that_load_only() {
    let mut ha = canned_ha();
    set_state(&mut ha, "sensor.humidity_average_inside", "80.0");
    set_state(&mut ha, "sensor.current_grid_cost", "0.12");
    set_state(&mut ha, "binary_sensor.dehumidifier_automated", "on");
    ha.failing.push("binary_sensor.indoor_comfort_dehumidifiers_running".into());
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    // The degraded load holds (observe-only), no call for it, diag recorded.
    let d = report.loads.iter().find(|l| l.id == "dehumidifier").unwrap();
    assert!(d.reason.contains("observe-only"), "{}", d.reason);
    assert!(!d.executed);
    assert!(report.diagnostics.iter().any(|m| m.contains("dehumidifiers_running")));
    assert!(ha.calls.lock().unwrap().is_empty());
}
