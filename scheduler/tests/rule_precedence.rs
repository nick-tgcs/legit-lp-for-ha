//! Precedence asserted on the LP's current-step output, flat prices
//! (so the decision for "now" is unambiguous).

use std::time::Duration;

use legit_lp_scheduler::lp::LpPlanner;
use legit_lp_scheduler::model::*;
use legit_lp_scheduler::testkit::*;

const STEPS: usize = 96;

fn planner() -> LpPlanner {
    LpPlanner { grid_minutes: 15, horizon_hours: 24 }
}

/// Dehumidifier needing to run: humid, cheap power, idle and free to start.
fn humid_world_and_load(price: f64) -> (WorldState, LoadContract) {
    let now = sydney(2026, 6, 10, 10, 0);
    (flat_world(now, STEPS, price), immediate_contract(Some(80.0)))
}

#[test]
fn manual_authority_blocks_scheduler() {
    let (world, mut c) = humid_world_and_load(0.05);
    c.authority = false;
    let out = planner().plan(&world, &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange);
    assert!(out.decisions[0].reason.contains("observe-only"));
    assert!(out.plans.is_empty(), "observe-only loads are not optimised");
}

#[test]
fn global_disabled_blocks_all() {
    let (mut world, c) = humid_world_and_load(0.05);
    world.global_enabled = false;
    let out = planner().plan(&world, &[c, runtime_contract()]);
    for d in &out.decisions {
        assert_eq!(d.action, Action::NoChange);
        assert!(d.reason.contains("global"));
    }
}

#[test]
fn must_have_price_ceiling_defers_with_unmet_report() {
    // Humid NOW but price 0.42 above both ceilings (0.15 mh, 0.10 ct) and no
    // surplus -> the immediate need cannot be met legally -> hold + unmet.
    let (world, c) = humid_world_and_load(0.42);
    let out = planner().plan(&world, &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange);
    assert!(out.decisions[0].reason.contains("unmet"), "{}", out.decisions[0].reason);
    assert!(out.plans[0].unmet > 0.0);
}

#[test]
fn min_off_lock_blocks_start() {
    let (world, mut c) = humid_world_and_load(0.05);
    c.obs.running = Some(false);
    c.obs.current_stretch = Duration::from_secs(5 * 60); // 5 of 15 min off
    let out = planner().plan(&world, &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange);
}

#[test]
fn start_budget_spent_blocks_start() {
    let (world, mut c) = humid_world_and_load(0.05);
    c.hard.max_starts_per_day = Some(3);
    c.obs.starts_today = 3; // manual starts count too
    let out = planner().plan(&world, &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange);
}

#[test]
fn hard_window_blocks_start() {
    let (world, mut c) = humid_world_and_load(0.05);
    c.hard.windows = vec![window(0, 0, 6, 0)]; // now (10:00) outside
    let out = planner().plan(&world, &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange);
}

#[test]
fn must_have_starts_now_when_legal() {
    let (world, c) = humid_world_and_load(0.05);
    let out = planner().plan(&world, &[c]);
    assert_eq!(out.decisions[0].action, Action::Start, "{}", out.decisions[0].reason);
    assert_eq!(out.plans[0].unmet, 0.0);
}

#[test]
fn can_take_never_runs_above_its_ceiling() {
    // Hot water with must-have already completed; in the can-take window.
    let now = sydney(2026, 6, 10, 11, 0);
    let mut c = runtime_contract();
    if let DemandKind::Runtime { completed_minutes, .. } = &mut c.must_have.kind {
        *completed_minutes = 90;
    }
    // Price above the 0.10 can-take ceiling -> nothing to do.
    let out = planner().plan(&flat_world(now, STEPS, 0.20), &[c.clone()]);
    assert_eq!(out.decisions[0].action, Action::NoChange);

    // Price below the ceiling -> can-take starts now.
    let out = planner().plan(&flat_world(now, STEPS, 0.05), &[c.clone()]);
    assert_eq!(out.decisions[0].action, Action::Start, "{}", out.decisions[0].reason);
    assert!(out.decisions[0].reason.contains("can-take"));

    // Unknown price -> optional work needs a known price (or sun): no start.
    let mut world = flat_world(now, STEPS, 0.05);
    world.price_now = None;
    world.import = vec![None; STEPS];
    let out = planner().plan(&world, &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange);
}

#[test]
fn unknown_sensor_or_running_state_never_commands() {
    // Unknown humidity: no demand signal -> idle, no panic.
    let now = sydney(2026, 6, 10, 10, 0);
    let c = immediate_contract(None);
    let out = planner().plan(&flat_world(now, STEPS, 0.05), &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange);

    // Unknown running state: observe-only.
    let mut c = immediate_contract(Some(80.0));
    c.obs.running = None;
    let out = planner().plan(&flat_world(now, STEPS, 0.05), &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange);
    assert!(out.decisions[0].reason.contains("observe-only"));
}

#[test]
fn min_run_holds_a_stop() {
    // Satisfied (humidity 50 <= 65) but only 10 min into a 30-min min_run.
    let now = sydney(2026, 6, 10, 10, 0);
    let mut c = immediate_contract(Some(50.0));
    c.obs.running = Some(true);
    c.obs.current_stretch = Duration::from_secs(10 * 60);
    let out = planner().plan(&flat_world(now, STEPS, 0.30), &[c.clone()]);
    assert_eq!(out.decisions[0].action, Action::NoChange, "{}", out.decisions[0].reason);

    // Once min_run is met, the stop lands (energy costs money, no demand).
    c.obs.current_stretch = Duration::from_secs(31 * 60);
    let out = planner().plan(&flat_world(now, STEPS, 0.30), &[c]);
    assert_eq!(out.decisions[0].action, Action::Stop);
}
