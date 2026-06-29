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
fn l9_partial_window_does_not_demand_a_full_day() {
    // REGRESSION (the "12 h required / 15 min short" phantom): a daily window
    // 00:00–15:00 with NOW at 09:00. The 24 h horizon straddles two FRAGMENTS of that
    // window: the CURRENT occurrence (today 09:00–15:00, demands its full 6 h) and a
    // FUTURE fragment (tomorrow 00:00–09:00, pro-rated to its visible share). Together
    // ~9.75 h — never the doubled 12 h day, and with cheap prices met with ~no unmet.
    let now = sydney(2026, 6, 10, 9, 0);
    let mut c = runtime_contract();
    if let DemandKind::Runtime { minutes, window, .. } = &mut c.must_have.kind {
        *minutes = 360; // 6 h "per day"
        *window = Window { start: t(0, 0), end: t(15, 0) };
    }
    let out = planner().plan(&flat_world(now, STEPS, 0.05), &[c]); // cheap everywhere
    let plan = &out.plans[0];
    assert!(plan.unmet < 1.0, "no phantom shortfall, got {}", plan.unmet);
    let scheduled = plan.on.iter().filter(|v| **v).count() * 15; // minutes
    assert!(scheduled < 12 * 60, "not a doubled day, scheduled {scheduled} min");
    assert!(scheduled >= 5 * 60, "but ~one window's 6 h IS met, scheduled {scheduled} min");
}

#[test]
fn l10_full_in_horizon_window_demands_full_amount() {
    // The other side: a window WHOLLY inside the horizon (00:00–06:30 from local
    // midnight) is a FULL instance (fraction 1). Pro-rating must NOT erode it — the
    // full 90 min is still demanded and met.
    let now = sydney(2026, 6, 10, 0, 0);
    let c = runtime_contract(); // 90 min into 00:00–06:30, both inside [00:00, 24:00)
    let out = planner().plan(&flat_world(now, STEPS, 0.20), &[c]);
    let plan = &out.plans[0];
    assert_eq!(plan.unmet, 0.0, "feasible");
    let scheduled = plan.on.iter().filter(|v| **v).count() * 15;
    assert!(scheduled >= 90, "full 90 min still demanded, scheduled {scheduled} min");
}

#[test]
fn l11_clipped_current_window_still_schedules_its_remaining_work() {
    // REGRESSION (Codex PR #40 P1): when NOW is already INSIDE a must-have window, the
    // current (front-clipped) instance must still demand its FULL runtime —
    // completed_minutes covers only what already ran, the rest is still due before the
    // deadline. 00:00–06:30 window, 90 min required, 40 already done, NOW 04:00 → 50 min
    // still owed with 2.5 h of room. The bug pro-rated the current instance, credited the
    // 40 min against the reduced target, and scheduled NOTHING before 06:30.
    let now = sydney(2026, 6, 10, 4, 0);
    let mut c = runtime_contract(); // 90 min into 00:00–06:30
    if let DemandKind::Runtime { completed_minutes, .. } = &mut c.must_have.kind {
        *completed_minutes = 40;
    }
    let out = planner().plan(&flat_world(now, STEPS, 0.05), &[c]); // cheap
    let plan = &out.plans[0];
    assert!(plan.unmet < 1.0, "feasible — 50 min fits in 2.5 h, got unmet {}", plan.unmet);
    // The remaining ~50 min must land in the CURRENT window: the first 10 steps cover
    // 04:00–06:15 (06:30 exclusive), the only steps that count toward this instance.
    let in_window_min = plan.on.iter().take(10).filter(|v| **v).count() * 15;
    assert!(
        in_window_min >= 50,
        "remaining work scheduled before the deadline, got {in_window_min} min"
    );
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

// ---- preview / shadow planning of observe-only loads -------------------

#[test]
fn p1_preview_plans_observe_only_loads_without_commanding_them() {
    // An observe-only (authority off) hot-water load at 22:00, runtime due
    // overnight, with a cheap valley to shift into. By default it is NOT solved
    // (observe-only); with preview ON it IS solved for the panel, yet the
    // current-step decision stays NoChange — preview never commands a device.
    let now = sydney(2026, 6, 10, 22, 0);
    let mut c = runtime_contract();
    c.authority = false; // observe-only
    let world = priced_world(now, 0.30, 0.05, 16, 20);

    // Default: not planned at all.
    let out = planner().plan(&world, std::slice::from_ref(&c));
    assert!(out.plans.is_empty(), "observe-only load is not planned by default");
    assert_eq!(out.decisions[0].action, Action::NoChange);
    assert!(out.decisions[0].reason.contains("authority disabled"), "{}", out.decisions[0].reason);

    // Preview ON: a plan appears (for the panel) but the decision is NoChange.
    let out = planner().plan_with_preview(&world, std::slice::from_ref(&c), true);
    let plan = out.plans.iter().find(|p| p.id == c.id).expect("preview plan present");
    assert!(plan.on.iter().any(|v| *v), "preview actually schedules the runtime");
    assert_eq!(plan.unmet, 0.0, "feasible overnight");
    assert_eq!(out.decisions[0].action, Action::NoChange, "preview must NOT command");
    assert!(out.decisions[0].reason.contains("preview"), "{}", out.decisions[0].reason);
}

#[test]
fn p1b_preview_reports_the_real_would_stop_intent() {
    // The whole point of preview: show what the optimiser WOULD do. An observe-only
    // load that is running, with min_run already met and nothing required now at a
    // dear price, WOULD be stopped. Preview must surface that real intent (action
    // Stop) so the panel can show "STOP (preview)" — the executor's authority gate
    // still guarantees it is never actually commanded.
    let now = sydney(2026, 6, 10, 10, 0); // outside the 00:00-06:30 must-have window
    let mut c = runtime_contract();
    c.authority = false; // observe-only
    c.obs.running = Some(true); // running (switched on by hand); min_run long since met
    let world = flat_world(now, STEPS, 0.30); // dear; can-take ceiling 0.10 -> nothing optional
    let out = planner().plan_with_preview(&world, std::slice::from_ref(&c), true);
    assert_eq!(out.decisions[0].action, Action::Stop, "preview surfaces the real would-be stop");
    assert!(out.decisions[0].reason.contains("preview"), "{}", out.decisions[0].reason);
    assert!(out.decisions[0].reason.contains("not executed"), "{}", out.decisions[0].reason);
}

#[test]
fn p2_preview_is_additive_authorised_loads_still_command() {
    // Preview only widens WHICH loads are solved; an AUTHORISED load still plans
    // and commands exactly as before.
    let now = sydney(2026, 6, 10, 10, 0);
    let c = immediate_contract(Some(80.0)); // authority=true, humid -> start now
    let out = planner().plan_with_preview(&flat_world(now, STEPS, 0.05), &[c], true);
    assert_eq!(out.decisions[0].action, Action::Start, "authorised load still commands");
    assert!(!out.plans.is_empty());
}

#[test]
fn p3_preview_off_with_unknown_running_state_is_still_observe_only() {
    // Preview cannot solve a load whose current running state is unknown — there
    // is no starting condition to plan from. It stays observe-only either way.
    let now = sydney(2026, 6, 10, 22, 0);
    let mut c = runtime_contract();
    c.authority = false;
    c.obs.running = None; // unknown
    let out = planner().plan_with_preview(&flat_world(now, STEPS, 0.20), &[c], true);
    assert!(out.plans.is_empty(), "no plan without a known running state");
    assert_eq!(out.decisions[0].action, Action::NoChange);
    assert!(
        out.decisions[0].reason.contains("running state unknown"),
        "{}",
        out.decisions[0].reason
    );
}

#[test]
fn p4_preview_running_outside_window_stays_feasible() {
    // REGRESSION (the "preview crashes" bug): a manually-run observe-only load
    // can be ON outside its must-have window AND over its price ceiling, with
    // min_run NOT yet met. The initial min-run lock forced `x[0] >= 1` while the
    // per-step price gate forced `x[0] <= 0` — an INFEASIBLE MILP. `solve()`
    // returned Err, the planner fell back to `hold_all`, and the panel blanked to
    // "no plan solved yet" with every load + the battery at 0. That is the crash
    // the user saw; preview is the trigger because it is the only path that pulls
    // an observe-only load (which a human can leave running anywhere) into the
    // solve. Fix: min_run outranks the price ceiling, so the locked step is
    // exempt from the price gate — the model stays feasible.
    //
    // now = 14:00, OUTSIDE the must-have window (00:00-06:30), price 0.30 above
    // the 0.10 ceiling everywhere, running 5 min into a 20 min min_run.
    let now = sydney(2026, 6, 10, 14, 0);
    let mut c = runtime_contract();
    c.authority = false; // observe-only — manually switched on by the human
    c.must_have.max_price = Some(0.10); // a ceiling current price exceeds
    c.obs.running = Some(true);
    c.obs.current_stretch = std::time::Duration::from_secs(5 * 60); // < 20m min_run -> lock armed
    let world = flat_world(now, STEPS, 0.30); // 0.30 > 0.10 ceiling everywhere

    let out = planner().plan_with_preview(&world, std::slice::from_ref(&c), true);

    // FEASIBLE: a real grid is produced (not the empty grid `hold_all` emits),
    // so the panel renders a plan instead of crashing.
    assert!(!out.grid.is_empty(), "preview must not blank the plan (hold_all)");
    assert_eq!(out.grid.len(), STEPS, "full horizon solved");
    assert_eq!(out.decisions[0].action, Action::NoChange, "observe-only -> never commands");
    assert!(
        !out.decisions[0].reason.contains("solver error"),
        "must not be a solver error: {}",
        out.decisions[0].reason
    );
    // Precedence, visible in the shadow plan: min_run HOLDS the in-progress run
    // for its remaining step (step 0 stays on, over the ceiling), then the
    // envelope — not a crash — releases it (off once the lock clears).
    let plan = out.plans.iter().find(|p| p.id == c.id).expect("preview plan present");
    assert!(plan.on[0], "min_run holds the running load through the locked step");
    assert!(!plan.on[1], "past the min_run lock the price/window envelope turns it off");
}

// ---- site balance reporting (grid_kw) ----------------------------------

#[test]
fn g1_grid_kw_imports_baseload_when_no_pv_or_loads() {
    // Observe-only world: no managed loads, no PV. Net grid = baseload (import).
    let now = sydney(2026, 6, 10, 12, 0);
    let out = planner().plan(&flat_world(now, STEPS, 0.20), &[]);
    assert_eq!(out.grid_kw.len(), STEPS, "grid_kw is grid-sized even with no loads");
    for g in &out.grid_kw {
        assert!((g - 0.8).abs() < 1e-6, "imports the 0.8 kW baseload, got {g}");
    }
    assert!(out.storage.is_empty(), "no storage configured");
}

#[test]
fn g2_grid_kw_exports_pv_surplus() {
    // PV 3 kW over 0.8 kW baseload -> 2.2 kW exported (grid_kw negative).
    let now = sydney(2026, 6, 10, 12, 0);
    let mut world = flat_world(now, STEPS, 0.20);
    world.pv.iter_mut().for_each(|p| *p = 3.0);
    let out = planner().plan(&world, &[]);
    for g in &out.grid_kw {
        assert!((g - (0.8 - 3.0)).abs() < 1e-6, "exports the 2.2 kW surplus, got {g}");
    }
}

// ---- storage device model ----------------------------------------------

/// flat_world + one home battery, then a cheap and an expensive price window.
fn battery_arb_world(now: chrono::DateTime<chrono_tz::Tz>) -> WorldState {
    let mut w = flat_world(now, STEPS, 0.20);
    for t in 4..8 {
        w.import[t] = Some(0.05); // cheap valley
    }
    for t in 40..44 {
        w.import[t] = Some(0.50); // expensive peak
    }
    w.storage = vec![test_storage()];
    w
}

#[test]
fn b1_battery_charges_cheap_and_discharges_expensive() {
    let now = sydney(2026, 6, 10, 0, 0);
    let out = planner().plan(&battery_arb_world(now), &[]);
    let b = out.storage.first().expect("storage plan present");
    assert_eq!(b.id, "battery");
    assert_eq!(b.soc_kwh.len(), STEPS + 1, "soc trajectory is grid+1");
    assert_eq!(b.charge_kw.len(), STEPS);
    assert!(b.charge_kw[4..8].iter().any(|&c| c > 0.1), "charges across the cheap valley");
    assert!(b.discharge_kw[40..44].iter().any(|&d| d > 0.1), "discharges across the peak");
    assert!(b.soc_kwh[8] > b.soc_kwh[4] + 0.5, "SoC climbs over the cheap valley");
    assert!(b.soc_kwh[44] < b.soc_kwh[40] - 0.5, "SoC falls over the peak");
}

#[test]
fn b2_battery_stays_within_soc_bounds() {
    let now = sydney(2026, 6, 10, 0, 0);
    let out = planner().plan(&battery_arb_world(now), &[]);
    let b = &out.storage[0];
    for s in &b.soc_kwh {
        assert!(*s >= b.min_soc_kwh - 1e-6 && *s <= b.max_soc_kwh + 1e-6, "SoC {s} out of bounds");
    }
}

#[test]
fn b3_battery_never_charges_and_discharges_at_once() {
    let now = sydney(2026, 6, 10, 0, 0);
    // Add a price crossover (feed-in above import) — the classic bait for
    // simultaneous charge+discharge. The mutex must forbid it.
    let mut world = battery_arb_world(now);
    for t in 60..64 {
        world.import[t] = Some(0.02);
        world.feedin[t] = 0.40;
    }
    let out = planner().plan(&world, &[]);
    let b = &out.storage[0];
    for t in 0..STEPS {
        assert!(
            !(b.charge_kw[t] > 1e-3 && b.discharge_kw[t] > 1e-3),
            "step {t}: charging {} and discharging {} at once",
            b.charge_kw[t],
            b.discharge_kw[t]
        );
    }
}

#[test]
fn b4_flat_prices_hold_soc_flat_no_dump_no_churn() {
    // No arbitrage signal: the terminal valuation + wear cost must keep the
    // pack where it started — neither dumped to the floor nor cycled.
    let now = sydney(2026, 6, 10, 0, 0);
    let mut world = flat_world(now, STEPS, 0.20);
    world.storage = vec![test_storage()];
    let out = planner().plan(&world, &[]);
    let b = &out.storage[0];
    assert!((b.soc_kwh[STEPS] - b.soc_kwh[0]).abs() < 0.1, "end SoC ~ start SoC under flat price");
    assert!(b.charge_kw.iter().all(|&c| c < 1e-2), "no charging without a price signal");
    assert!(b.discharge_kw.iter().all(|&d| d < 1e-2), "no discharging without a price signal");
}

#[test]
fn b5_grid_charge_policy_gates_charging_from_the_grid() {
    // Cheap night, no solar. With grid-charging ON the pack fills from the grid;
    // with it OFF (solar-only) the pack cannot charge at all.
    let now = sydney(2026, 6, 10, 0, 0);
    let mut world = flat_world(now, STEPS, 0.30);
    for t in 4..8 {
        world.import[t] = Some(0.02); // cheap window, but pv is 0 everywhere
    }
    for t in 40..44 {
        world.import[t] = Some(0.60); // peak to make arbitrage worthwhile
    }

    let mut on = test_storage();
    on.allow_grid_charge = true;
    on.soc_now_kwh = 1.0; // start empty so charging is the only way to arbitrage
    let mut w_on = world.clone();
    w_on.storage = vec![on];
    let plan = planner().plan(&w_on, &[]);
    let charged: f64 = plan.storage[0].charge_kw.iter().sum();
    assert!(charged > 1.0, "grid-charging ON should fill from the cheap grid, got {charged}");

    let mut off = test_storage();
    off.allow_grid_charge = false;
    off.soc_now_kwh = 1.0;
    let mut w_off = world;
    w_off.storage = vec![off];
    let out = planner().plan(&w_off, &[]);
    for (t, &c) in out.storage[0].charge_kw.iter().enumerate() {
        assert!(c < 1e-3, "solar-only battery must not grid-charge; step {t} charged {c}");
    }
}

#[test]
fn b6_battery_discharge_exports_beyond_pv() {
    // pv is 0, but a feed-in spike makes exporting stored energy worthwhile.
    // The export bound must allow grid export to exceed PV by the discharge.
    let now = sydney(2026, 6, 10, 0, 0);
    let mut world = flat_world(now, STEPS, 0.10);
    world.feedin.iter_mut().for_each(|f| *f = 0.02);
    for t in 40..44 {
        world.feedin[t] = 0.50; // export premium
    }
    let mut bat = test_storage();
    bat.soc_now_kwh = 10.0; // full, ready to export
    world.storage = vec![bat];
    let out = planner().plan(&world, &[]);
    let b = &out.storage[0];
    assert!(b.discharge_kw[40..44].iter().any(|&d| d > 0.5), "discharges into the export premium");
    // pv is 0 yet net grid is a strong export at the spike -> bound fix works.
    assert!(
        out.grid_kw[40..44].iter().any(|&g| g < -1.0),
        "net export exceeds PV (which is zero) via battery discharge: {:?}",
        &out.grid_kw[40..44]
    );
}

#[test]
fn b7_solar_charges_battery_when_grid_charge_disallowed() {
    // Expensive grid all day, a midday PV surplus, evening is still expensive:
    // a solar-only battery soaks the surplus (charging only where pv > 0).
    let now = sydney(2026, 6, 10, 0, 0);
    let mut world = flat_world(now, STEPS, 0.45);
    for t in 48..56 {
        world.pv[t] = 4.0; // midday surplus over the 0.8 baseload
    }
    let mut bat = test_storage();
    bat.allow_grid_charge = false;
    bat.soc_now_kwh = 1.0;
    world.storage = vec![bat];
    let out = planner().plan(&world, &[]);
    let b = &out.storage[0];
    assert!(b.charge_kw[48..56].iter().any(|&c| c > 0.1), "soaks the midday solar surplus");
    for t in 0..STEPS {
        let pv = world.pv[t].max(0.0);
        assert!(b.charge_kw[t] <= pv + 1e-6, "step {t}: charge {} exceeds PV {pv}", b.charge_kw[t]);
    }
}

/// A charge-only EV (no discharge), empty, plugged in.
fn test_ev() -> StorageInput {
    StorageInput {
        id: "ev".into(),
        capacity_kwh: 60.0,
        soc_now_kwh: 6.0,
        min_soc_kwh: 0.0,
        max_soc_kwh: 60.0,
        max_charge_kw: 7.0,
        max_discharge_kw: 0.0, // charge-only
        round_trip_efficiency: 0.95,
        allow_grid_charge: true,
        available: true,
        cycle_cost_aud_per_kwh: 0.0,
        goals: vec![],
    }
}

#[test]
fn b8_charge_only_ev_without_goals_does_not_charge() {
    // No discharge => no terminal value => no economic reason to charge. A
    // goal-less EV must just sit (charging it would be a pure loss).
    let now = sydney(2026, 6, 10, 22, 0);
    let mut world = flat_world(now, STEPS, 0.20);
    for t in 8..16 {
        world.import[t] = Some(0.02); // cheap, but no goal wants it
    }
    world.storage = vec![test_ev()];
    let out = planner().plan(&world, &[]);
    assert!(out.storage[0].charge_kw.iter().all(|&c| c < 1e-2), "no goal => no charging");
}

#[test]
fn b9_ev_target_charges_to_percent_by_deadline_as_cheap_as_possible() {
    // EV at 22:00 must reach 50% (30 kWh) of 60 kWh by 07:00. Cheap window
    // 02:00-04:00 (steps 16..24); expensive elsewhere. It should fill in the
    // cheap window and arrive at/above target by the deadline step.
    let now = sydney(2026, 6, 10, 22, 0);
    let mut world = flat_world(now, STEPS, 0.40);
    for t in 16..32 {
        world.import[t] = Some(0.05); // 02:00-06:00 cheap (ample to fill before 07:00)
    }
    let mut ev = test_ev();
    ev.goals = vec![StorageGoal::Target { soc_kwh: 30.0, ready_by: t(7, 0) }];
    world.storage = vec![ev];
    let out = planner().plan(&world, &[]);
    let b = &out.storage[0];
    // 07:00 is step 36 from 22:00 at 15-min steps; SoC there must reach target.
    assert!(b.soc_kwh[36] >= 30.0 - 0.2, "reaches 30 kWh by 07:00, got {}", b.soc_kwh[36]);
    assert!(b.target_unmet < 0.2, "target met (slack ~ 0), got {}", b.target_unmet);
    // The charging concentrates in the cheap window, not the pricey steps.
    let cheap: f64 = b.charge_kw[16..32].iter().sum();
    let total: f64 = b.charge_kw.iter().sum();
    assert!(cheap > 0.9 * total, "fills in the cheap window: {cheap} of {total}");
}

#[test]
fn b10_ev_target_reports_unmet_when_unreachable() {
    // Plugged in only at 06:00 (now), 7 kW max, 1h to 07:00 -> at most ~7 kWh,
    // but target is 30 kWh: honest shortfall, never forced.
    let now = sydney(2026, 6, 10, 6, 0);
    let mut world = flat_world(now, STEPS, 0.20);
    let mut ev = test_ev();
    ev.soc_now_kwh = 0.0;
    ev.goals = vec![StorageGoal::Target { soc_kwh: 30.0, ready_by: t(7, 0) }];
    world.storage = vec![ev];
    let out = planner().plan(&world, &[]);
    assert!(out.storage[0].target_unmet > 15.0, "big honest shortfall");
}

#[test]
fn b11_price_goal_charges_when_cheap_up_to_cap() {
    // Charge-only EV with a price goal: top up while import < 0.10, up to 18 kWh.
    let now = sydney(2026, 6, 10, 22, 0);
    let mut world = flat_world(now, STEPS, 0.30); // mostly above the ceiling
    for t in 8..20 {
        world.import[t] = Some(0.05); // a long cheap window below 0.10
    }
    let mut ev = test_ev();
    ev.soc_now_kwh = 6.0;
    ev.goals = vec![StorageGoal::Price { below: 0.10, up_to_kwh: 18.0 }];
    world.storage = vec![ev];
    let out = planner().plan(&world, &[]);
    let b = &out.storage[0];
    assert!(
        b.soc_kwh[STEPS] >= 17.0,
        "fills toward the 18 kWh cap when cheap, got {}",
        b.soc_kwh[STEPS]
    );
    // Charging happens in the cheap window, not the expensive baseline.
    let cheap: f64 = b.charge_kw[8..20].iter().sum();
    let total: f64 = b.charge_kw.iter().sum();
    assert!(total > 9.0 && cheap > 0.9 * total, "charges only while cheap: {cheap} of {total}");
}

#[test]
fn b12_two_devices_are_planned_independently() {
    // A home battery (arbitrages) and a charge-only EV (target) coexist; both
    // get plans and both feed the one site balance.
    let now = sydney(2026, 6, 10, 22, 0);
    let mut world = flat_world(now, STEPS, 0.40);
    for t in 16..24 {
        world.import[t] = Some(0.05);
    }
    for t in 40..44 {
        world.import[t] = Some(0.60); // peak for the battery to discharge into
    }
    let mut ev = test_ev();
    ev.goals = vec![StorageGoal::Target { soc_kwh: 30.0, ready_by: t(7, 0) }];
    world.storage = vec![test_storage(), ev];
    let out = planner().plan(&world, &[]);
    assert_eq!(out.storage.len(), 2);
    let battery = out.storage.iter().find(|s| s.id == "battery").unwrap();
    let car = out.storage.iter().find(|s| s.id == "ev").unwrap();
    assert!(battery.discharge_kw[40..44].iter().any(|&d| d > 0.1), "battery arbitrages the peak");
    assert!(car.soc_kwh[36] >= 30.0 - 0.3, "EV still meets its 07:00 target");
    assert!(car.discharge_kw.iter().all(|&d| d < 1e-6), "charge-only EV never discharges");
}

#[test]
fn b13_unavailable_device_stays_idle() {
    // An unplugged EV (available=false) can neither charge nor discharge, even
    // with a target it would otherwise chase.
    let now = sydney(2026, 6, 10, 22, 0);
    let mut world = flat_world(now, STEPS, 0.05); // dirt cheap everywhere
    let mut ev = test_ev();
    ev.available = false;
    ev.goals = vec![StorageGoal::Target { soc_kwh: 40.0, ready_by: t(7, 0) }];
    world.storage = vec![ev];
    let out = planner().plan(&world, &[]);
    let b = &out.storage[0];
    assert!(b.charge_kw.iter().all(|&c| c < 1e-6), "unplugged EV cannot charge");
    assert!(b.target_unmet > 30.0, "and its target is honestly unmet");
}

// ---- D3: threshold "at-or-above" (humidifier) + program (run-once block) ----

/// A humidifier: keep humidity AT OR ABOVE a limit (the `above` threshold).
fn humidifier_contract(observed: Option<f64>) -> LoadContract {
    let mut c = immediate_contract(observed);
    c.id = LoadId("humidifier".into());
    c.can_take = None;
    c.must_have = Demand {
        kind: DemandKind::Threshold {
            dir: ThresholdDir::Above,
            limit: 40.0,
            observed,
            start_hysteresis: 2.0,
            drop_per_hour: 0.0,
            drift_per_hour: 0.0,
            window: None,
            cap_minutes: None,
        },
        max_price: Some(0.50), // generous cap: price never blocks here
    };
    c
}

#[test]
fn p1_threshold_above_runs_when_observed_is_below_the_limit() {
    let now = sydney(2026, 6, 10, 12, 0);
    // observed 35 < limit 40 - hysteresis 2 -> the humidifier needs to run now.
    let out = planner().plan(&flat_world(now, STEPS, 0.20), &[humidifier_contract(Some(35.0))]);
    assert!(out.plans[0].on[0], "humidifier turns on when humidity is below target");
}

#[test]
fn p2_threshold_above_idles_when_observed_at_or_above_the_limit() {
    let now = sydney(2026, 6, 10, 12, 0);
    // observed 45 > limit 40 -> already satisfied, nothing to do.
    let out = planner().plan(&flat_world(now, STEPS, 0.20), &[humidifier_contract(Some(45.0))]);
    assert!(!out.plans[0].on[0], "humidifier stays off when already above target");
}

/// A fixed program (washing machine): one contiguous 60-min block (min_run forced
/// to the length, single start) inside 00:00-12:00 — the engine shape a config
/// `kind: program` lowers to.
fn program_contract() -> LoadContract {
    let mut c = runtime_contract();
    c.id = LoadId("washing_machine".into());
    c.can_take = None;
    c.hard.min_run = std::time::Duration::from_secs(60 * 60); // 4 steps, contiguous
    c.hard.min_off = std::time::Duration::from_secs(0);
    c.hard.max_starts_per_day = Some(1);
    c.must_have = Demand {
        kind: DemandKind::Runtime {
            minutes: 60,
            window: window(0, 0, 12, 0),
            completed_minutes: 0,
            exact: true, // a program is held to EXACTLY its block length
        },
        max_price: Some(0.30),
    };
    c
}

#[test]
fn p3_program_runs_as_one_contiguous_block_at_the_cheapest_start() {
    let now = sydney(2026, 6, 10, 0, 0);
    // Cheap valley at steps 16..20 (02:00-03:00), inside the 00:00-12:00 window.
    let out = planner().plan(&priced_world(now, 0.25, 0.05, 16, 20), &[program_contract()]);
    let runs = on_runs(&out.plans[0].on);
    assert_eq!(runs.len(), 1, "a program runs exactly once (one contiguous run)");
    let (s, e) = runs[0];
    assert_eq!(e - s, 4, "the run is exactly the 60-min block — not fragmented");
    assert_eq!((s, e), (16, 20), "and is placed in the cheapest contiguous slot");
}

#[test]
fn p5_program_is_capped_at_its_length_even_when_extra_runtime_is_profitable() {
    // Codex P2: a program must run EXACTLY its declared block, not just "at least". Under a flat
    // NEGATIVE import price, every extra on-step lowers cost, so a lower-bound-only model would
    // keep a 60-min program on for the whole 00:00–12:00 window. The `exact` upper bound holds it
    // to its 4-step (60-min) block. (A deferrable runtime load, exact:false, is intentionally free
    // to soak up the cheap window — the difference is the whole point of the flag.)
    let now = sydney(2026, 6, 10, 0, 0);
    let w = flat_world(now, STEPS, -0.10); // negative: running is profitable everywhere
    let out = planner().plan(&w, &[program_contract()]);
    let runs = on_runs(&out.plans[0].on);
    assert_eq!(runs.len(), 1, "still one contiguous run");
    let (s, e) = runs[0];
    assert_eq!(
        e - s,
        4,
        "capped at the 60-min block, not extended by cheap power (run {}..{})",
        s,
        e
    );
    assert!(out.plans[0].unmet <= 0.0001, "the block is fully met, not unmet");
}

#[test]
fn p6_in_progress_program_with_nonaligned_completed_stays_feasible() {
    // Codex round-10: a program already part-run with a NON-grid-aligned `completed` (50 of a
    // 60-min block) and the min-run lock armed (current stretch < block) must stay FEASIBLE.
    // Capping the FUTURE credit on the REMAINING runtime leaves room for the forced 15-min step;
    // capping the TOTAL would have allowed only 10 min vs a forced 15-min step -> infeasible ->
    // solver-error/hold-all. Now it simply finishes the locked step and stops near the block.
    let now = sydney(2026, 6, 10, 1, 0); // inside the 00:00-12:00 program window
    let mut c = program_contract();
    c.obs.running = Some(true);
    c.obs.current_stretch = std::time::Duration::from_secs(50 * 60); // < 60-min block -> lock armed
    if let DemandKind::Runtime { completed_minutes, .. } = &mut c.must_have.kind {
        *completed_minutes = 50;
    }
    let out = planner().plan(&flat_world(now, STEPS, 0.05), &[c]);
    assert!(
        !out.plans.is_empty(),
        "must stay feasible, not solver-error/hold-all: {}",
        out.decisions[0].reason
    );
    assert!(
        !out.decisions[0].reason.contains("solver error"),
        "no solver error: {}",
        out.decisions[0].reason
    );
    // Finishes ~the remaining step(s) and stops — not the whole window.
    let total: usize = on_runs(&out.plans[0].on).iter().map(|(s, e)| e - s).sum();
    assert!(
        (1..=2).contains(&total),
        "finishes the remaining step(s), not the window (total {total})"
    );
}

#[test]
fn p7_program_already_complete_but_locked_on_finishes_safely() {
    // Codex round-11: recorder history with multiple on-spans can leave a program with its full
    // runtime already accrued (completed >= block) WHILE current_stretch < min_run, so the initial
    // lock still forces steps. remaining==0 must not cap credit to 0 (the forced step would then be
    // infeasible -> hold-all); the cap floors at the forced-lock minutes, so it honors the lock and
    // stops — even under a negative price that would otherwise extend it across the whole window.
    let now = sydney(2026, 6, 10, 1, 0);
    let mut c = program_contract(); // 60-min block, min_run 60
    c.obs.running = Some(true);
    c.obs.current_stretch = std::time::Duration::from_secs(15 * 60); // < 60 -> lock forces ~3 steps
    if let DemandKind::Runtime { completed_minutes, .. } = &mut c.must_have.kind {
        *completed_minutes = 60; // full block already accrued across earlier spans
    }
    let out = planner().plan(&flat_world(now, STEPS, -0.10), &[c]); // negative: uncapped would over-run
    assert!(!out.plans.is_empty(), "feasible, not hold-all: {}", out.decisions[0].reason);
    assert!(
        !out.decisions[0].reason.contains("solver error"),
        "no solver error: {}",
        out.decisions[0].reason
    );
    // Honors the forced lock then STOPS — does not run the whole window under the negative price.
    let total: usize = on_runs(&out.plans[0].on).iter().map(|(s, e)| e - s).sum();
    assert!(total <= 4, "honors the lock then stops, not the whole window (total {total})");
}

#[test]
fn p4_program_is_all_or_nothing_under_the_price_cap() {
    let now = sydney(2026, 6, 10, 0, 0);
    // Base above the cap, with only SCATTERED single cheap steps — no 4-in-a-row
    // fits under the cap, so the whole program waits (never runs a partial block).
    let mut w = flat_world(now, STEPS, 0.50);
    for &t in &[2usize, 8, 14, 20] {
        w.import[t] = Some(0.05);
    }
    let out = planner().plan(&w, &[program_contract()]);
    let runs = on_runs(&out.plans[0].on);
    assert!(runs.is_empty(), "no contiguous block fits under the cap -> program waits");
    assert!(out.plans[0].unmet > 0.0, "the unmet runtime is reported honestly");
}
