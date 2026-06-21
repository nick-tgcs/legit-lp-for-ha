//! One full solve cycle over the module seams: real example registry, real
//! fixture payloads, RecordingHa, real HiGHS.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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
    // Preview toggle defaults OFF (existing scenarios are unaffected; c5 flips it).
    ha.states.insert(
        "input_boolean.lp_scheduler_preview".into(),
        json!({"state": "off", "attributes": {}}),
    );
    // Dehumidifier run window (the user's "Run from / Run until"), read live by the
    // LP load's hard window. Overnight 22:00 -> 11:00, so the 10:00 scenarios are
    // in-window and unchanged.
    ha.states.insert(
        "input_datetime.input_datetime_indoor_comfort_dehumidifier_window_start".into(),
        json!({"state": "22:00:00", "attributes": {}}),
    );
    ha.states.insert(
        "input_datetime.input_datetime_indoor_comfort_dehumidifier_window_end".into(),
        json!({"state": "11:00:00", "attributes": {}}),
    );
    ha.states.insert("sensor.beckton_general_forecast".into(), fixture("forecast_amber.json"));
    // Amber-shaped feed-in (export) forecast on its OWN sensor, values varying
    // per slot. Dated to the injected `now` (10:00 Sydney == 00:00 UTC).
    ha.states.insert(
        "sensor.beckton_feed_in_forecast".into(),
        json!({"state": "0.05", "attributes": {"forecasts": [
            {"per_kwh": 0.06, "start_time": "2026-06-10T00:00:00+00:00", "end_time": "2026-06-10T00:30:00+00:00"},
            {"per_kwh": 0.10, "start_time": "2026-06-10T00:30:00+00:00", "end_time": "2026-06-10T01:00:00+00:00"},
            {"per_kwh": 0.03, "start_time": "2026-06-10T01:00:00+00:00", "end_time": "2026-06-10T02:00:00+00:00"},
            {"per_kwh": 0.08, "start_time": "2026-06-10T02:00:00+00:00", "end_time": "2026-06-10T06:00:00+00:00"}
        ]}}),
    );
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
    cycle_with_preview(dry_run, Arc::new(AtomicBool::new(false)))
}

fn cycle_with_preview(dry_run: bool, preview_override: Arc<AtomicBool>) -> Cycle {
    Cycle {
        registry: registry(),
        planner: LpPlanner { grid_minutes: 15, horizon_hours: 24 },
        dry_run,
        profile_path: None,
        preview_override,
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
    assert!(!report.preview, "preview off by default: HA boolean off, no panel override");
}

#[tokio::test]
async fn c1b_unreadable_entity_ref_power_holds_the_load_observe_only() {
    // De-hardcoding fail-CLOSED guard: if a load's power_kw is entity-ref'd and the
    // sensor is unavailable, resolve -> 0 kW, which would otherwise look "free" to the
    // LP and sail past every price ceiling. The cycle must hold it observe-only +
    // surface a diagnostic, never command it.
    let mut reg = registry();
    let hw = reg.loads.iter_mut().find(|l| l.id == "hot_water").expect("hot_water load");
    hw.capability.power_kw = config::ValueRef::Entity { entity: "sensor.missing_hw_power".into() };
    let mut ha = canned_ha();
    // Grant authority so the ONLY thing that can hold the load is the fail-closed guard.
    set_state(&mut ha, "binary_sensor.hot_water_automated", "on");
    let cyc = Cycle {
        registry: reg,
        planner: LpPlanner { grid_minutes: 15, horizon_hours: 24 },
        dry_run: true,
        profile_path: None,
        preview_override: Arc::new(AtomicBool::new(false)),
    };
    let mut profiles = Profiles::default();
    let report = cyc.run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    let hw = report.loads.iter().find(|l| l.id == "hot_water").unwrap();
    assert!(hw.reason.contains("observe-only"), "held observe-only: {}", hw.reason);
    assert!(
        report.diagnostics.iter().any(|d| d.contains("power_kw")),
        "power_kw fail-closed diagnostic surfaced: {:?}",
        report.diagnostics
    );
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

#[tokio::test]
async fn c4_separate_feedin_forecast_makes_the_panel_series_vary() {
    // Regression: with feed-in published on its own sensor, the panel's feed-in
    // line was flat — only the single current value was carried forward. A
    // dedicated feed-in forecast must resample to a VARYING per-step series.
    let ha = canned_ha();
    let mut profiles = Profiles::default();
    let report = cycle(true).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    assert_eq!(report.feedin.len(), report.grid.len(), "feed-in is grid-aligned");
    assert!(
        report.feedin.windows(2).any(|w| w[0] != w[1]),
        "feed-in must vary across the horizon, not flat-line: {:?}",
        report.feedin
    );
    // The forecast values land in the series (00:30 slot -> 0.10, 01:00 -> 0.03).
    assert!(report.feedin.contains(&0.10) && report.feedin.contains(&0.03), "{:?}", report.feedin);
}

#[tokio::test]
async fn c5_preview_solves_observe_only_loads_without_controlling_them() {
    // Preview ON, in LIVE mode (not dry-run): the observe-only loads (authorities
    // are genuinely OFF in the captured state) are SOLVED — a horizon plan appears
    // for the panel — but nothing is executed and ZERO service calls reach HA.
    let mut ha = canned_ha();
    set_state(&mut ha, "input_boolean.lp_scheduler_preview", "on");
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    // Every load whose running state is known now carries a horizon plan — these
    // were unplanned (observe-only) before the preview toggle.
    let known: Vec<_> = report.loads.iter().filter(|l| l.running.is_some()).collect();
    assert!(known.len() >= 2, "fixture should have >=2 loads with a known running state");
    assert!(known.iter().all(|l| !l.on.is_empty()), "preview solves observe-only loads");
    // ...yet none is executed, a preview reason is shown, and no calls were made —
    // the per-load authority gate holds even with the global scheduler live.
    assert!(report.loads.iter().all(|l| !l.executed));
    assert!(report.loads.iter().any(|l| l.reason.contains("preview")), "preview reason shown");
    assert!(ha.calls.lock().unwrap().is_empty(), "preview must NOT call HA, even live");
    assert!(report.preview, "the HA preview boolean drives the effective preview flag");
}

#[tokio::test]
async fn c6_panel_preview_override_solves_observe_only_loads_without_controlling() {
    // The in-panel checkbox path: the HA preview boolean is OFF, but the runtime
    // override (flipped by POST /api/preview, here injected directly) is ON.
    // Effective preview = HA boolean OR override, so the observe-only loads are
    // solved for the panel — yet, exactly as the HA-boolean path, nothing is
    // executed and zero service calls reach HA even with the scheduler live.
    let ha = canned_ha(); // preview boolean defaults OFF in canned_ha()
    let override_on = Arc::new(AtomicBool::new(true));
    let mut profiles = Profiles::default();
    let report = cycle_with_preview(false, override_on)
        .run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0))
        .await;
    assert!(report.preview, "the panel override is reflected in the effective preview flag");
    let known: Vec<_> = report.loads.iter().filter(|l| l.running.is_some()).collect();
    assert!(known.len() >= 2, "fixture should have >=2 loads with a known running state");
    assert!(known.iter().all(|l| !l.on.is_empty()), "override solves observe-only loads");
    assert!(report.loads.iter().all(|l| !l.executed));
    assert!(ha.calls.lock().unwrap().is_empty(), "override must NOT call HA, even live");
}

#[tokio::test]
async fn c7_dehumidifier_held_outside_its_run_window() {
    // The reported bug, end to end: humid + cheap + authority, but the clock is
    // OUTSIDE the user's configured run window (overnight 22:00 -> 11:00). It must
    // be HELD, never started. 14:30 is the live screenshot time; we flatten the
    // import forecast so the step-0 price is the flat current value (unambiguous).
    let mut ha = canned_ha();
    set_state(&mut ha, "sensor.humidity_average_inside", "80.0"); // humid
    set_state(&mut ha, "sensor.current_grid_cost", "0.118"); // <= mh ceiling 0.155
    set_state(&mut ha, "sensor.beckton_general_forecast", "0.118"); // flat: no forward curve
    set_state(&mut ha, "binary_sensor.dehumidifier_automated", "on"); // authority granted
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 14, 30)).await;
    let d = report.loads.iter().find(|l| l.id == "dehumidifier").unwrap();
    assert!(!d.executed, "dehumidifier ran outside its 22:00-11:00 window: {}", d.reason);
    assert!(
        ha.calls.lock().unwrap().is_empty(),
        "no service call may fire outside the window: {:?}",
        ha.calls.lock().unwrap()
    );
}

#[tokio::test]
async fn c8_dehumidifier_runs_inside_its_run_window() {
    // Guard against over-correction: the same humid + cheap + authority case, but
    // now INSIDE the window (02:00) -> it must still start.
    let mut ha = canned_ha();
    set_state(&mut ha, "sensor.humidity_average_inside", "80.0");
    set_state(&mut ha, "sensor.current_grid_cost", "0.10");
    set_state(&mut ha, "sensor.beckton_general_forecast", "0.10"); // flat: no forward curve
    set_state(&mut ha, "binary_sensor.dehumidifier_automated", "on");
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 2, 0)).await;
    let d = report.loads.iter().find(|l| l.id == "dehumidifier").unwrap();
    assert!(d.executed, "dehumidifier must run inside its 22:00-11:00 window: {}", d.reason);
}

// ---- storage control (battery) -------------------------------------------
// The example registry's two Sonnen cabinets are modelled from live entity-refs
// (capacity, round-trip efficiency, reserve floor, cycle cost) and driven
// per-direction: in Optimiser mode the LP writes the per-cabinet rate
// input_number; otherwise it stands down. `binary_sensor.battery_charge_automated`
// / `battery_export_automated` are the live authorities — OFF in the shared
// fixture, flipped on here to exercise actuation.

fn storage_rate_calls(ha: &RecordingHa) -> Vec<(String, f64)> {
    ha.calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| {
            c.target_entity.contains("grid_charge_rate")
                || c.target_entity.contains("grid_discharge_rate")
        })
        .map(|c| (c.target_entity.clone(), c.data["value"].as_f64().unwrap_or(f64::NAN)))
        .collect()
}

#[tokio::test]
async fn c9_storage_unauthorised_is_modelled_advisory_but_never_actuated() {
    // Shared fixture: both directions configured but their authority is OFF
    // (Manual/Scheduled). Both cabinets are still modelled from their live specs
    // and planned, yet the LP must write NO rate to either — even live.
    let ha = canned_ha();
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    assert_eq!(report.storage.len(), 2, "both cabinets modelled from entity-ref specs");
    assert!(storage_rate_calls(&ha).is_empty(), "unauthorised storage is never actuated");
}

#[tokio::test]
async fn c10_charge_authority_writes_only_the_charge_rate_per_direction() {
    // Grant charge authority (LP import) but leave export Manual: the LP writes
    // the per-cabinet CHARGE rate each cycle and never a discharge rate.
    let mut ha = canned_ha();
    set_state(&mut ha, "binary_sensor.battery_charge_automated", "on");
    let mut profiles = Profiles::default();
    cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 2, 0)).await;
    let calls = storage_rate_calls(&ha);
    assert!(
        calls.iter().any(|(t, _)| t == "input_number.input_number_sonnen01_grid_charge_rate"),
        "charge authority => the LP drives the charge rate: {calls:?}"
    );
    assert!(
        !calls.iter().any(|(t, _)| t.contains("grid_discharge_rate")),
        "export stays Manual => no discharge rate written: {calls:?}"
    );
}

#[tokio::test]
async fn c11_storage_dry_run_writes_no_rate_even_when_authorised() {
    let mut ha = canned_ha();
    set_state(&mut ha, "binary_sensor.battery_charge_automated", "on");
    set_state(&mut ha, "binary_sensor.battery_export_automated", "on");
    let mut profiles = Profiles::default();
    cycle(true).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    assert!(ha.calls.lock().unwrap().is_empty(), "dry-run never writes a storage rate");
}

#[tokio::test]
async fn c12_storage_charges_off_a_cheap_now_dear_later_spread() {
    // Both directions Optimiser. Cheap right now, dear for the rest of the horizon
    // => a clean arbitrage: charge the cabinets now to cover the dear window later.
    let mut ha = canned_ha();
    set_state(&mut ha, "binary_sensor.battery_charge_automated", "on");
    set_state(&mut ha, "binary_sensor.battery_export_automated", "on");
    set_state(&mut ha, "sensor.current_grid_cost", "0.02");
    ha.states.insert(
        "sensor.beckton_general_forecast".into(),
        json!({"state": "0.02", "attributes": {"forecasts": [
            {"per_kwh": 0.02, "start_time": "2026-06-10T00:00:00+00:00", "end_time": "2026-06-10T01:00:00+00:00"},
            {"per_kwh": 0.60, "start_time": "2026-06-10T01:00:00+00:00", "end_time": "2026-06-11T00:00:00+00:00"}
        ]}}),
    );
    let mut profiles = Profiles::default();
    cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    let charge = storage_rate_calls(&ha)
        .into_iter()
        .find(|(t, _)| t == "input_number.input_number_sonnen01_grid_charge_rate")
        .expect("charge rate written");
    assert!(charge.1 > 0.0, "cheap-now/dear-later => charges now, got {} W", charge.1);
}

#[tokio::test]
async fn c13_export_authority_writes_only_the_discharge_rate_per_direction() {
    // Mirror of c10 on the other axis: grant export authority (LP export) while
    // charge stays Manual — the LP drives the per-cabinet DISCHARGE rate and never
    // a charge rate. (Discharge economics — charges the cheap valley, discharges
    // the peak, reserve floor, charge/discharge mutex — are covered deterministically
    // in tests/lp.rs; here we prove the per-direction control wiring.)
    let mut ha = canned_ha();
    set_state(&mut ha, "binary_sensor.battery_export_automated", "on");
    let mut profiles = Profiles::default();
    cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 18, 0)).await;
    let calls = storage_rate_calls(&ha);
    assert!(
        calls.iter().any(|(t, _)| t == "input_number.input_number_sonnen01_grid_discharge_rate"),
        "export authority => the LP drives the discharge rate: {calls:?}"
    );
    assert!(
        !calls.iter().any(|(t, _)| t.contains("grid_charge_rate")),
        "charge stays Manual => no charge rate written: {calls:?}"
    );
}
