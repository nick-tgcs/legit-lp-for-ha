//! Horizon/cost behaviour of the MILP: cost-shifting, honest infeasibility,
//! hardware constraints in the solution, can-take valuation, surplus.

use legit_lp_scheduler::lp::LpPlanner;
use legit_lp_scheduler::model::*;
use legit_lp_scheduler::testkit::*;

const STEPS: usize = 96;

fn planner() -> LpPlanner {
    LpPlanner { grid_minutes: 15, horizon_hours: 24 }
}

/// Price series: `base` everywhere, `cheap` on [from, to) step indices.
fn priced_world(
    now: chrono::DateTime<chrono_tz::Tz>,
    base: f64,
    cheap: f64,
    from: usize,
    to: usize,
) -> WorldState {
    let mut w = flat_world(now, STEPS, base);
    for t in from..to {
        w.import[t] = Some(cheap);
    }
    w
}

fn on_runs(on: &[bool]) -> Vec<(usize, usize)> {
    let mut runs = vec![];
    let mut start = None;
    for (i, &v) in on.iter().enumerate() {
        match (v, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, on.len()));
    }
    runs
}

#[test]
fn l1_cost_shifting_into_the_cheap_valley() {
    // 22:00 start; hot water needs 60 more min before 06:30. Cheap window
    // 02:00-03:00 (steps 16..20), expensive elsewhere. No must-have ceiling.
    let now = sydney(2026, 6, 10, 22, 0);
    // NOTE: completed_minutes belongs to the ONGOING instance only; at 22:00
    // the 00:00-06:30 window is tomorrow's fresh instance -> full amount due.
    let mut c = runtime_contract();
    if let DemandKind::Runtime { minutes, .. } = &mut c.must_have.kind {
        *minutes = 60;
    }
    let world = priced_world(now, 0.30, 0.05, 16, 20);
    let out = planner().plan(&world, &[c]);
    assert_eq!(out.decisions[0].action, Action::NoChange, "now is expensive; wait");
    let plan = &out.plans[0];
    assert_eq!(plan.unmet, 0.0);
    // The 60 minutes (4 steps) land exactly in the cheap valley.
    let on: Vec<usize> = plan.on.iter().enumerate().filter(|(_, v)| **v).map(|(i, _)| i).collect();
    assert_eq!(on, vec![16, 17, 18, 19], "runs in 02:00-03:00");
}

#[test]
fn l2_feasible_must_have_has_zero_unmet() {
    let now = sydney(2026, 6, 10, 22, 0);
    let out = planner().plan(&flat_world(now, STEPS, 0.20), &[runtime_contract()]);
    assert_eq!(out.plans[0].unmet, 0.0);
    // 90 minutes = 6 steps actually planned inside the 00:00-06:30 window.
    let planned: usize = out.plans[0].on.iter().filter(|v| **v).count();
    assert!(planned >= 6, "at least the required steps are planned, got {planned}");
}

#[test]
fn l3_infeasible_must_have_reports_unmet_not_violation() {
    // Demand 600 min into a 6.5h (390 min) window: impossible. The plan must
    // report ~210 min unmet, NOT break min_run/min_off/window to chase it.
    let now = sydney(2026, 6, 10, 22, 0);
    let mut c = runtime_contract();
    if let DemandKind::Runtime { minutes, .. } = &mut c.must_have.kind {
        *minutes = 600;
    }
    let out = planner().plan(&flat_world(now, STEPS, 0.20), &[c.clone()]);
    let plan = &out.plans[0];
    assert!(plan.unmet >= 200.0, "honest shortfall, got {}", plan.unmet);
    // Window respected: nothing on outside 00:00-06:30 (steps 8..34).
    for (i, v) in plan.on.iter().enumerate() {
        if *v {
            assert!((8..34).contains(&i), "on-step {i} escapes the window");
        }
    }
}

#[test]
fn l4_min_run_and_min_off_hold_in_the_solution() {
    let now = sydney(2026, 6, 10, 22, 0);
    let mut c = runtime_contract(); // min_run 20m (2 steps), min_off 15m (1 step)
    if let DemandKind::Runtime { minutes, .. } = &mut c.must_have.kind {
        *minutes = 120;
    }
    // Alternating prices try to bait single-step runs.
    let mut world = flat_world(now, STEPS, 0.30);
    for t in (8..34).step_by(2) {
        world.import[t] = Some(0.01);
    }
    let out = planner().plan(&world, &[c]);
    assert!(!out.plans.is_empty(), "solver failed: {}", out.decisions[0].reason);
    let runs = on_runs(&out.plans[0].on);
    assert!(!runs.is_empty());
    for (s, e) in &runs {
        assert!(e - s >= 2, "run [{s},{e}) shorter than min_run");
    }
    for w in runs.windows(2) {
        assert!(w[1].0 - w[0].1 >= 1, "gap shorter than min_off");
    }
}

#[test]
fn l5_max_starts_bounds_the_plan() {
    let now = sydney(2026, 6, 10, 22, 0);
    let mut c = runtime_contract(); // max 3 starts/day
    c.obs.starts_today = 2; // budget left today: 1
    if let DemandKind::Runtime { minutes, .. } = &mut c.must_have.kind {
        *minutes = 120;
    }
    let mut world = flat_world(now, STEPS, 0.30);
    for t in (8..34).step_by(4) {
        world.import[t] = Some(0.01); // bait many separate cheap runs
    }
    let out = planner().plan(&world, &[c]);
    let runs = on_runs(&out.plans[0].on);
    // All of today's window (00:00-06:30 is tomorrow's date though!) —
    // group runs by date: the horizon crosses midnight, so starts in the
    // 00:00-06:30 window land on the NEXT day with a fresh budget of 3.
    let mut by_day = std::collections::HashMap::new();
    for (s, _) in &runs {
        let started_running = *s == 0 && out.plans[0].on[0]; // continuation, not a start
        if !started_running {
            *by_day.entry(out.grid[*s].date_naive()).or_insert(0u32) += 1;
        }
    }
    for (day, starts) in by_day {
        let budget = if day == now.date_naive() { 1 } else { 3 };
        assert!(starts <= budget, "{day}: {starts} starts > budget {budget}");
    }
}

#[test]
fn l6_can_take_valuation_runs_only_below_ceiling_and_within_cap() {
    // Must-have done; can-take (cap 60min, ceiling 0.10) with 8 cheap steps.
    // 12h horizon from 10:00 keeps tomorrow's must-have window out of scope.
    let planner = LpPlanner { grid_minutes: 15, horizon_hours: 12 };
    let now = sydney(2026, 6, 10, 10, 0);
    let mut c = runtime_contract();
    if let DemandKind::Runtime { completed_minutes, .. } = &mut c.must_have.kind {
        *completed_minutes = 90;
    }
    let mut world = priced_world(now, 0.20, 0.05, 0, 8); // 10:00-12:00 cheap
    world.import.truncate(48);
    world.feedin.truncate(48);
    world.pv.truncate(48);
    world.baseload.truncate(48);
    let out = planner.plan(&world, &[c]);
    let plan = &out.plans[0];
    let on_steps: Vec<usize> =
        plan.on.iter().enumerate().filter(|(_, v)| **v).map(|(i, _)| i).collect();
    assert!(!on_steps.is_empty(), "cheap surplusless can-take still runs below ceiling");
    assert!(on_steps.iter().all(|t| *t < 8), "only in the cheap steps: {on_steps:?}");
    let minutes = 15 * on_steps.len();
    assert!(minutes <= 60, "cap respected, got {minutes}");
    // Every on-step is can-take-credited (must-have is complete).
    for t in &on_steps {
        assert!(plan.ct[*t]);
    }
}

#[test]
fn l7_predictive_dynamics_keep_the_band_or_report() {
    // Aircon at 27°C, band [19,25], hot ambient, cheap flat power: the plan
    // must cool into the band (unmet 0) and actually run.
    let now = sydney(2026, 6, 10, 10, 0);
    let c = predictive_contract(Some(27.0), Some(35.0));
    let out = planner().plan(&flat_world(now, STEPS, 0.05), std::slice::from_ref(&c));
    // Cooling 27 -> 25 takes time: the transient band violation is REAL and
    // honestly reported as unmet, but it must be small and the unit must run.
    let transient = out.plans[0].unmet;
    assert!(transient > 0.0 && transient < 20.0, "transient only, got {transient}");
    assert!(out.plans[0].on.iter().any(|v| *v));
    assert_eq!(out.decisions[0].action, Action::Start, "27 > 25: cooling starts");

    // Broken dynamics (rate 0): the shortfall must dwarf the transient.
    let mut c2 = predictive_contract(Some(27.0), Some(35.0));
    if let DemandKind::TemperatureBand { change_per_hour, .. } = &mut c2.must_have.kind {
        *change_per_hour = 0.0;
    }
    let out = planner().plan(&flat_world(now, STEPS, 0.05), &[c2]);
    assert!(out.plans[0].unmet > 10.0 * transient);
}

#[test]
fn l8_immediate_mode_forces_current_step_only() {
    let now = sydney(2026, 6, 10, 10, 0);
    let c = immediate_contract(Some(80.0));
    let out = planner().plan(&flat_world(now, STEPS, 0.05), &[c]);
    assert_eq!(out.decisions[0].action, Action::Start);
    // min_run (30m -> 2 steps) binds the immediate start; beyond that the
    // only future running is can-take inside its window/ceiling.
    let plan = &out.plans[0];
    for (t, v) in plan.on.iter().enumerate().skip(2) {
        if *v {
            assert!(plan.ct[t], "future step {t} on without can-take credit");
        }
    }
}

#[test]
fn l11_surplus_pulls_load_when_grid_is_expensive() {
    let planner = LpPlanner { grid_minutes: 15, horizon_hours: 12 };
    // Import 0.40 all day (above every ceiling). Midday PV surplus covers the
    // 3.6 kW heater for 4 steps -> can-take runs there, and ONLY there.
    let now = sydney(2026, 6, 10, 10, 0);
    let mut c = runtime_contract();
    if let DemandKind::Runtime { completed_minutes, .. } = &mut c.must_have.kind {
        *completed_minutes = 90; // must-have done; only can-take remains
    }
    let mut world = flat_world(now, 48, 0.40);
    for t in 8..12 {
        world.pv[t] = 5.0; // 12:00-13:00, baseload 0.8 -> surplus 4.2 >= 3.6
    }
    let out = planner.plan(&world, &[c.clone()]);
    let on: Vec<usize> =
        out.plans[0].on.iter().enumerate().filter(|(_, v)| **v).map(|(i, _)| i).collect();
    assert_eq!(on, vec![8, 9, 10, 11], "runs exactly in the surplus window");

    // Without the PV window nothing runs at all.
    let out = planner.plan(&flat_world(now, 48, 0.40), &[c]);
    assert!(out.plans[0].on.iter().all(|v| !*v));
}

#[test]
fn l12_two_loads_compete_for_one_surplus() {
    let planner = LpPlanner { grid_minutes: 15, horizon_hours: 12 };
    // Two 3.6kW runtime loads, surplus 4.2kW: room for ONE at a time. The
    // marginal load would pay 0.40 import against a 0.10 ceiling -> never.
    let now = sydney(2026, 6, 10, 10, 0);
    let mk = |id: &str| {
        let mut c = runtime_contract();
        c.id = LoadId(id.into());
        if let DemandKind::Runtime { completed_minutes, .. } = &mut c.must_have.kind {
            *completed_minutes = 90;
        }
        c
    };
    let mut world = flat_world(now, 48, 0.40);
    for t in 8..16 {
        world.pv[t] = 5.0;
    }
    let out = planner.plan(&world, &[mk("a"), mk("b")]);
    for t in 0..48 {
        let both = out.plans.iter().filter(|p| p.on[t]).count();
        assert!(both <= 1, "step {t}: both loads on would breach the surplus");
    }
}
