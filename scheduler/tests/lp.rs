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
