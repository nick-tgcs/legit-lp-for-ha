//! One full solve cycle over the module seams: real example registry, real
//! fixture payloads, RecordingHa, real HiGHS.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use legit_lp_scheduler::config;
use legit_lp_scheduler::cycle::Cycle;
use legit_lp_scheduler::ha_client::{history_rows, RecordingHa};
use legit_lp_scheduler::lp::LpPlanner;
use legit_lp_scheduler::profile::Profiles;
use legit_lp_scheduler::status::Severity;
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
    // …and that fail-closed hold is promoted to a Warning alert (the triaged surface),
    // not just buried in the diagnostics bag. It is NOT a solve failure.
    assert!(
        report
            .alerts
            .iter()
            .any(|a| a.severity == Severity::Warning && a.detail.contains("power_kw")),
        "fail-closed hold surfaced as a Warning alert: {:?}",
        report.alerts
    );
    assert!(!report.is_solver_failure(), "a held load is not a scheduler failure");
}

#[tokio::test]
async fn c1c_unreadable_min_run_entity_holds_the_load_observe_only() {
    // De-hardcoding fail-CLOSED guard: min_run/min_off are the short-cycle lockout.
    // If they are entity-ref'd and the helper is unavailable, resolve -> None.
    // Defaulting to 0 would DROP the lockout (an authorised compressor could be stopped
    // before its minimum run / restarted before its minimum off). The cycle must instead
    // hold the load observe-only + surface a diagnostic, never command it.
    let mut reg = registry();
    let hw = reg.loads.iter_mut().find(|l| l.id == "hot_water").expect("hot_water load");
    hw.hard_rules.min_run_minutes =
        config::ValueRef::Entity { entity: "input_number.missing_min_run".into() };
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
        report.diagnostics.iter().any(|d| d.contains("min_run_minutes")),
        "min_run fail-closed diagnostic surfaced: {:?}",
        report.diagnostics
    );
}

#[tokio::test]
async fn c1d_unreadable_max_soc_pct_holds_ceiling_at_current_soc() {
    // De-hardcoding fail-CLOSED guard: max_soc_pct is the user's charge ceiling. If it
    // is entity-ref'd and the helper is unavailable, resolve -> None. Defaulting to 100%
    // would let the LP charge PAST the user's (now unknown) ceiling. The cycle must hold
    // the ceiling at the present SoC (no further charge) + surface a diagnostic.
    let mut reg = registry();
    let s = reg.global.storage.iter_mut().find(|s| s.id == "sonnen01").expect("sonnen01 storage");
    s.max_soc_pct = config::ValueRef::Entity { entity: "input_number.missing_max_soc".into() };
    let mut ha = canned_ha();
    set_state(&mut ha, "binary_sensor.battery_charge_automated", "on");
    let cyc = Cycle {
        registry: reg,
        planner: LpPlanner { grid_minutes: 15, horizon_hours: 24 },
        dry_run: true,
        profile_path: None,
        preview_override: Arc::new(AtomicBool::new(false)),
    };
    let mut profiles = Profiles::default();
    let report = cyc.run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    let st = report.storage.iter().find(|s| s.id == "sonnen01").unwrap();
    // SoC is 52% of 9.0 kWh ≈ 4.68; the ceiling must hold there, NOT expand to full 9.0.
    assert!(
        st.max_soc_kwh < st.capacity_kwh - 0.01,
        "ceiling must not expand to full capacity: max {} of {} kWh",
        st.max_soc_kwh,
        st.capacity_kwh
    );
    assert!(
        (st.max_soc_kwh - st.soc_now_kwh).abs() < 0.01,
        "ceiling held at current SoC: max {} vs soc_now {}",
        st.max_soc_kwh,
        st.soc_now_kwh
    );
    assert!(
        report.diagnostics.iter().any(|d| d.contains("max_soc_pct")),
        "max_soc_pct fail-closed diagnostic surfaced: {:?}",
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
    // The mode is surfaced as an Info alert so the panel can explain "nothing is being
    // controlled" — and it is NOT a failure.
    assert!(
        report.alerts.iter().any(|a| a.severity == Severity::Info
            && a.scope == "scheduler"
            && a.detail.to_lowercase().contains("preview")),
        "preview mode surfaced as an Info alert: {:?}",
        report.alerts
    );
    assert!(!report.is_solver_failure());
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
async fn c10b_report_carries_per_direction_authority_for_the_panel() {
    // Same one-sided fixture (charge Optimiser, export Manual). The view-model must
    // expose per-direction authority so the panel tags the ACTIVE direction by what
    // will actually be actuated: `execute_storage` drives charge but never discharge.
    // Device-level `authority` stays true (the cabinet IS partly controllable).
    let mut ha = canned_ha();
    set_state(&mut ha, "binary_sensor.battery_charge_automated", "on");
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 2, 0)).await;
    let s = report.storage.iter().find(|s| s.id == "sonnen01").expect("sonnen01 storage");
    assert!(s.charge_authority, "charge is in Optimiser => its direction is authorised");
    assert!(!s.discharge_authority, "export stays Manual => discharge is advisory");
    assert!(s.authority, "device-level authority is true when any direction is authorised");
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

#[tokio::test]
async fn c14_unreadable_storage_specs_fail_loud_not_to_an_invented_number() {
    // No-hardcoding fail-LOUD + SAFE: efficiency / cycle-cost are not safely guessable,
    // so an unreadable entity-ref EXCLUDES that device from the LP for the cycle (never
    // invents 0.9 / 0.001) with an actionable diagnostic; the other cabinet is unaffected.
    // Reserve, by contrast, freezes the discharge floor at the current SoC (a safe
    // non-action). The excluded device is still commanded IDLE — see c15.
    let mut reg = registry();
    let s = reg.global.storage.iter_mut().find(|s| s.id == "sonnen01").expect("sonnen01");
    s.round_trip_efficiency = config::ValueRef::Entity { entity: "sensor.missing_rte".into() };
    let cyc = Cycle {
        registry: reg,
        planner: LpPlanner { grid_minutes: 15, horizon_hours: 24 },
        dry_run: true,
        profile_path: None,
        preview_override: Arc::new(AtomicBool::new(false)),
    };
    let mut profiles = Profiles::default();
    let report = cyc.run(&canned_ha(), &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    assert!(report.storage.iter().all(|s| s.id != "sonnen01"), "sonnen01 dropped (not invented)");
    assert!(report.storage.iter().any(|s| s.id == "sonnen02"), "the other cabinet still modelled");
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.contains("round_trip_efficiency") && d.contains("unmodelled")),
        "fail-loud diagnostic surfaced: {:?}",
        report.diagnostics
    );
    assert!(!report.is_solver_failure(), "a dropped device is not a scheduler failure");
}

#[tokio::test]
async fn c15_unmodelled_storage_is_commanded_idle_not_left_on_its_last_command() {
    // PR #41 review (Codex P1): a device we cannot model (sonnen01's efficiency entity is
    // unreadable) is excluded from the LP — but it must still be driven IDLE, or a prior
    // cycle's active charge/discharge command persists in HA (fail-loud but NOT fail-safe).
    // Live (dry_run=false) so the executor actually writes: assert a zero-rate command for
    // sonnen01's charge AND discharge rate entities.
    let mut reg = registry();
    reg.global
        .storage
        .iter_mut()
        .find(|s| s.id == "sonnen01")
        .expect("sonnen01")
        .round_trip_efficiency = config::ValueRef::Entity { entity: "sensor.missing_rte".into() };
    let cyc = Cycle {
        registry: reg,
        planner: LpPlanner { grid_minutes: 15, horizon_hours: 24 },
        dry_run: false,
        profile_path: None,
        preview_override: Arc::new(AtomicBool::new(false)),
    };
    let mut ha = canned_ha();
    set_state(&mut ha, "binary_sensor.battery_charge_automated", "on");
    set_state(&mut ha, "binary_sensor.battery_export_automated", "on");
    let mut profiles = Profiles::default();
    let _ = cyc.run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;

    let calls = ha.calls.lock().unwrap();
    let idle_rate_writes: Vec<_> = calls
        .iter()
        .filter(|c| c.target_entity.contains("sonnen01") && c.target_entity.contains("rate"))
        .collect();
    assert!(!idle_rate_writes.is_empty(), "sonnen01 must be commanded idle, got calls: {calls:?}");
    assert!(
        idle_rate_writes.iter().all(|c| c.data == json!({ "value": 0.0 })),
        "idle = zero rate, got: {idle_rate_writes:?}"
    );
}

#[tokio::test]
async fn c16_unreadable_baseline_does_not_become_free_pv_headroom() {
    // PR #41 review (Codex P2): when power.baseline_kw is an unreadable entity-ref, the
    // engine must NOT treat the unknown baseload as free PV headroom (that would let the
    // LP's surplus open price-capped loads above their ceiling). Fail-loud (actionable
    // diagnostic) + fail-safe (no panic, a valid plan); the safe degradation is enforced
    // by baseload = pv (zero surplus) in cycle.rs.
    let mut reg = registry();
    reg.global.power.as_mut().expect("power").baseline_kw =
        config::ValueRef::Entity { entity: "sensor.missing_baseline".into() };
    let cyc = Cycle {
        registry: reg,
        planner: LpPlanner { grid_minutes: 15, horizon_hours: 24 },
        dry_run: true,
        profile_path: None,
        preview_override: Arc::new(AtomicBool::new(false)),
    };
    let mut profiles = Profiles::default();
    let report = cyc.run(&canned_ha(), &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    assert!(
        report.diagnostics.iter().any(|d| d.contains("baseline_kw") && d.contains("unreadable")),
        "fail-loud diagnostic surfaced: {:?}",
        report.diagnostics
    );
    assert!(!report.is_solver_failure(), "an unreadable baseline still yields a valid plan");
}

/// Cheap-now / dear-later forecast (mirrors c12): one cheap hour at the injected `now`
/// (10:00 Sydney == 00:00 UTC), dear for the rest of the horizon — a clean arbitrage.
fn cheap_now_dear_later(ha: &mut RecordingHa) {
    set_state(ha, "sensor.current_grid_cost", "0.02");
    ha.states.insert(
        "sensor.beckton_general_forecast".into(),
        json!({"state": "0.02", "attributes": {"forecasts": [
            {"per_kwh": 0.02, "start_time": "2026-06-10T00:00:00+00:00", "end_time": "2026-06-10T01:00:00+00:00"},
            {"per_kwh": 0.60, "start_time": "2026-06-10T01:00:00+00:00", "end_time": "2026-06-11T00:00:00+00:00"}
        ]}}),
    );
}

#[tokio::test]
async fn c17_unauthorised_storage_previews_its_full_plan_yet_actuates_nothing() {
    // The user's demand: a Scheduled battery (BOTH directions Manual) must still PREVIEW
    // its intended trajectory. Planning is decoupled from authority — the advisory panel
    // plans the cheap-now/dear-later charge — while not a single rate is written (authority
    // gates actuation). This is the symptom that started this whole thread: the cards used
    // to show "short, never plans" because an unauthorised device was modelled frozen.
    let mut ha = canned_ha(); // both authority sensors default OFF
    cheap_now_dear_later(&mut ha);
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;
    let s = report.storage.iter().find(|s| s.id == "sonnen01").expect("sonnen01 modelled");
    assert!(
        !s.charge_authority && !s.discharge_authority,
        "both directions are advisory (Scheduled)"
    );
    assert!(
        s.charge_kw.iter().any(|&kw| kw > 0.1),
        "preview shows the charge plan even while unauthorised: {:?}",
        s.charge_kw
    );
    assert!(
        s.reasoning.narrative.contains("advisory"),
        "an unactuated plan must read as advisory, not live: {:?}",
        s.reasoning.narrative
    );
    assert!(
        !s.action_actuated,
        "unauthorised => the shown action is not committed (pill: advisory)"
    );
    assert!(
        storage_rate_calls(&ha).is_empty(),
        "unauthorised storage is never actuated: {:?}",
        storage_rate_calls(&ha)
    );
}

#[tokio::test]
async fn c18_charge_authorised_discharge_not_never_commits_an_arbitrage_charge() {
    // P1 guard (the regression #43 introduced and #45 reverted): planning at rated power
    // must not let a COMMITTED charge lean on a future discharge the executor will skip.
    // Charge is Optimiser, export stays Manual; cheap-now/dear-later with no target goal,
    // so the ONLY reason to charge now is arbitrage that NEEDS the (unauthorised) discharge.
    // The advisory panel still shows the charge plan, but the actuation model has every
    // unauthorised direction zeroed — a charge-only cabinet with no dischargeable payoff —
    // so the committed charge rate is ~0. A real charge command here would be the P1.
    let mut ha = canned_ha();
    set_state(&mut ha, "binary_sensor.battery_charge_automated", "on"); // export stays Manual
    cheap_now_dear_later(&mut ha);
    let mut profiles = Profiles::default();
    let report = cycle(false).run(&ha, &mut profiles, sydney(2026, 6, 10, 10, 0)).await;

    // Advisory: the panel still plans the arbitrage charge (rated power, both directions).
    let s = report.storage.iter().find(|s| s.id == "sonnen01").expect("sonnen01 modelled");
    assert!(s.charge_authority && !s.discharge_authority, "charge Optimiser, export Manual");
    assert!(
        s.charge_kw.iter().any(|&kw| kw > 0.1),
        "advisory panel shows the charge plan: {:?}",
        s.charge_kw
    );
    // P2 (Codex on #46): charge IS authorised, but the shown charge isn't committed this
    // step, so the narrative must be tagged advisory — not described as a live "charging now".
    assert!(
        s.reasoning.narrative.contains("advisory"),
        "an authorised-but-uncommitted charge must read as advisory: {:?}",
        s.reasoning.narrative
    );
    assert!(
        !s.action_actuated,
        "charge authorised but not committed this step => pill must read advisory, not live"
    );

    // Actuation: the executor runs (charge is authorised) but the gated model commits ~0 W —
    // no arbitrage charge that depends on the skipped discharge leg.
    let calls = storage_rate_calls(&ha);
    let charge =
        calls.iter().find(|(t, _)| t == "input_number.input_number_sonnen01_grid_charge_rate");
    assert!(
        matches!(charge, Some((_, w)) if *w < 1.0),
        "charge with no dischargeable payoff must commit ~0 W, got {charge:?} (all: {calls:?})"
    );
    assert!(
        !calls.iter().any(|(t, _)| t.contains("grid_discharge_rate")),
        "export stays Manual => no discharge rate written: {calls:?}"
    );
}
