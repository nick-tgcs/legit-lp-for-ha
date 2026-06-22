//! Build the per-device "Why" explanation the panel shows (Overview + Why tabs).
//!
//! Pure: takes the registry config (for source entities), the resolved contract /
//! storage input, the solved plan, and the world series, and returns a
//! `status::Reasoning`. Computed for EVERY device — including observe-only ones,
//! which the decision pass otherwise blanks — so the user can always see why.

use chrono::{DateTime, NaiveTime};
use chrono_tz::Tz;

use crate::config::{
    DemandCfg, LoadConfig, StorageConfig, StorageGoalCfg, TimeRef, ValueRef, WindowCfg,
};
use crate::lp::{LoadPlan, StoragePlan};
use crate::model::{DemandKind, LoadContract, Window};
use crate::rules;
use crate::status::{PlanBlock, ReasonFact, Reasoning, StepBucket};
use crate::time::{in_window, window_instances, Grid};

fn hm_dt(dt: &DateTime<Tz>) -> String {
    dt.format("%H:%M").to_string()
}
fn hm(t: NaiveTime) -> String {
    t.format("%H:%M").to_string()
}

/// Minutes -> "N min" or "H.h h" for readability.
fn dur_minutes(mins: f64) -> String {
    if mins >= 90.0 {
        format!("{:.1} h", mins / 60.0)
    } else {
        format!("{:.0} min", mins)
    }
}

/// The source entity for a `ValueRef` field, owned (for the view-model).
fn vr_src(vr: &ValueRef) -> Option<String> {
    vr.source().map(str::to_string)
}
fn opt_vr_src(vr: &Option<ValueRef>) -> Option<String> {
    vr.as_ref().and_then(|v| v.source()).map(str::to_string)
}
fn ref_src(vr: Option<&ValueRef>) -> Option<String> {
    vr.and_then(|v| v.source()).map(str::to_string)
}

/// The must-have demand window (resolved) for a contract, if it has one.
fn mh_window(c: &LoadContract) -> Option<Window> {
    match &c.must_have.kind {
        DemandKind::Runtime { window, .. } | DemandKind::TemperatureBand { window, .. } => {
            Some(*window)
        }
        DemandKind::HumidityBelow { window, .. } => *window,
    }
}

/// The registry window cfg (carries the TimeRef sources) for a demand.
fn cfg_window(d: &DemandCfg) -> Option<&WindowCfg> {
    match d {
        DemandCfg::Runtime { window, .. } | DemandCfg::TemperatureBand { window, .. } => {
            Some(window)
        }
        DemandCfg::HumidityBelow { window, .. } => window.as_ref(),
    }
}

fn cfg_max_price(d: &DemandCfg) -> Option<&ValueRef> {
    match d {
        DemandCfg::Runtime { max_price, .. }
        | DemandCfg::HumidityBelow { max_price, .. }
        | DemandCfg::TemperatureBand { max_price, .. } => max_price.as_ref(),
    }
}

/// Contiguous `true` runs as (start, end-exclusive) step indices.
fn runs(on: &[bool]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut t = 0;
    while t < on.len() {
        if on[t] {
            let s = t;
            while t < on.len() && on[t] {
                t += 1;
            }
            out.push((s, t));
        } else {
            t += 1;
        }
    }
    out
}

/// End time of a block running steps [s, e) on `grid` (e may be == n).
fn block_end(grid: &Grid, e: usize) -> String {
    let n = grid.steps.len();
    if e < n {
        hm_dt(&grid.steps[e])
    } else if n > 0 {
        hm_dt(&(grid.steps[n - 1] + chrono::Duration::minutes(i64::from(grid.step_minutes))))
    } else {
        "?".into()
    }
}

fn plan_blocks(on: &[bool], ct: &[bool], grid: &Grid, must: &str, opt: &str) -> Vec<PlanBlock> {
    let step_min = f64::from(grid.step_minutes);
    runs(on)
        .into_iter()
        .map(|(s, e)| {
            let all_ct = (s..e).all(|t| ct.get(t).copied().unwrap_or(false));
            PlanBlock {
                start: hm_dt(&grid.steps[s]),
                end: block_end(grid, e),
                hours: (e - s) as f64 * step_min / 60.0,
                kind: if all_ct { opt } else { must }.to_string(),
            }
        })
        .collect()
}

/// The "Why" for one load. `plan` is `None` when the load wasn't solved (e.g.
/// running state unknown) — we still surface its resolved inputs.
pub fn for_load(
    cfg: &LoadConfig,
    c: &LoadContract,
    plan: Option<&LoadPlan>,
    grid: &Grid,
    import: &[Option<f64>],
    surplus: &[f64],
) -> Reasoning {
    let n = grid.steps.len();
    let step_min = f64::from(grid.step_minutes);

    // ---- resolved live inputs (value + the entity that drove it) ----
    let mut inputs = vec![ReasonFact::new(
        "Power",
        format!("{:.2} kW", c.power_kw),
        vr_src(&cfg.capability.power_kw),
    )];
    let min_run_min = c.hard.min_run.as_secs() as f64 / 60.0;
    let min_off_min = c.hard.min_off.as_secs() as f64 / 60.0;
    inputs.push(ReasonFact::new(
        "Min run",
        dur_minutes(min_run_min),
        vr_src(&cfg.hard_rules.min_run_minutes),
    ));
    inputs.push(ReasonFact::new(
        "Min off",
        dur_minutes(min_off_min),
        vr_src(&cfg.hard_rules.min_off_minutes),
    ));
    inputs.push(ReasonFact::new(
        "Max starts",
        c.hard.max_starts_per_day.map(|m| format!("{m}/day")).unwrap_or("unbounded".into()),
        opt_vr_src(&cfg.hard_rules.max_starts_per_day),
    ));
    if let Some(w) = mh_window(c) {
        // Split start/end so BOTH bounds' source entities stay traceable (a single
        // fact could only carry one source). Sources present only when entity-backed.
        let cw = cfg_window(&cfg.must_have);
        inputs.push(ReasonFact::new(
            "Window start",
            hm(w.start),
            cw.and_then(|w| w.start.source()).map(str::to_string),
        ));
        inputs.push(ReasonFact::new(
            "Window end",
            hm(w.end),
            cw.and_then(|w| w.end.source()).map(str::to_string),
        ));
    }
    inputs.push(ReasonFact::new(
        "Price cap",
        c.must_have.max_price.map(|p| format!("${p:.3}/kWh")).unwrap_or("none".into()),
        ref_src(cfg_max_price(&cfg.must_have)),
    ));

    // ---- the plan, scheduled minutes, blocks ----
    let (on, ct, unmet) = match plan {
        Some(p) => (p.on.clone(), p.ct.clone(), p.unmet),
        None => (vec![false; n], vec![false; n], 0.0),
    };
    let scheduled_min = on.iter().filter(|&&b| b).count() as f64 * step_min;
    let blocks = plan_blocks(&on, &ct, grid, "must-have", "can-take");

    // ---- step availability (why each step was/wasn't usable) ----
    let masks = rules::masks(c, grid, import, surplus);
    let win = mh_window(c);
    let (mut available, mut outside, mut over_cap, mut hard_closed) = (0u32, 0u32, 0u32, 0u32);
    for t in 0..n {
        if !masks.hard_ok[t] {
            hard_closed += 1;
        } else {
            let in_scope = win.map(|w| in_window(grid.steps[t].time(), &w)).unwrap_or(true);
            if !in_scope {
                outside += 1;
            } else if masks.ok_mh[t] {
                available += 1;
            } else {
                over_cap += 1;
            }
        }
    }
    let mut steps = vec![StepBucket { label: "available".into(), count: available }];
    if outside > 0 {
        steps.push(StepBucket { label: "outside window".into(), count: outside });
    }
    if over_cap > 0 {
        steps.push(StepBucket { label: "over price cap".into(), count: over_cap });
    }
    if hard_closed > 0 {
        steps.push(StepBucket { label: "hard-rule window closed".into(), count: hard_closed });
    }

    // ---- metrics + narrative (runtime gets the rich treatment) ----
    let mut metrics = Vec::new();
    let mut narrative;
    let mut binding = None;
    let mut fix_hint = None;

    if let DemandKind::Runtime { minutes, completed_minutes, window } = &c.must_have.kind {
        // `minutes` is required PER window instance; the LP enforces it for each
        // instance in the horizon (lp.rs) and `unmet`/`scheduled_min` are horizon-wide
        // sums. Scale required by the instance count so Required/Planned/Unmet
        // reconcile (else a recurring daily window shows "Scheduled 11h of 6h — short").
        let instances = window_instances(window, grid).len().max(1) as f64;
        let required = f64::from(*minutes) * instances;
        let completed = f64::from(*completed_minutes); // credited to instance 0 only
        metrics.push(ReasonFact::new("Required", dur_minutes(required), None));
        if completed > 0.0 {
            metrics.push(ReasonFact::new("Already done", dur_minutes(completed), None));
        }
        metrics.push(ReasonFact::new("Planned", dur_minutes(scheduled_min), None));
        metrics.push(ReasonFact::new("Unmet", dur_minutes(unmet), None));

        if unmet > 1.0 {
            let remaining = (required - completed).max(0.0);
            // Separate WINDOW availability from PRICE: window_min counts in-window,
            // in-horizon steps regardless of price; available excludes price-capped
            // steps. Only call the window "too tight" when even the price-agnostic
            // window space can't fit the work — otherwise the binding is the price cap.
            let window_min = f64::from(available + over_cap) * step_min;
            let priced_min = f64::from(available) * step_min;
            if window_min + 1.0 < remaining {
                let wtxt = win
                    .map(|w| format!("{}\u{2013}{}", hm(w.start), hm(w.end)))
                    .unwrap_or_default();
                binding = Some(format!("must-have window {wtxt} too tight"));
                fix_hint =
                    Some("widen the window or lower required runtime (your HA settings)".into());
            } else if over_cap > 0 && priced_min + 1.0 < remaining {
                binding = Some("price cap skips dear steps".into());
                fix_hint = Some("raise the max grid price, or accept the shortfall".into());
            } else {
                binding = Some("min-off / max-starts spacing".into());
            }
            narrative = format!(
                "Scheduled {} of {} — {} short.",
                dur_minutes(scheduled_min),
                dur_minutes(required),
                dur_minutes(unmet),
            );
        } else if scheduled_min > 0.0 {
            let when = blocks.first().map(|b| format!(" at {}", b.start)).unwrap_or_default();
            narrative = format!("Runs {}{} — fully met.", dur_minutes(scheduled_min), when);
        } else {
            narrative = "Nothing required in this horizon.".into();
        }
    } else {
        // Setpoint loads (aircon/dehumidifier): band + observed + unmet.
        let (lo, hi, obs) = match &c.must_have.kind {
            DemandKind::TemperatureBand { min, max, observed, .. } => {
                (Some(*min), Some(*max), *observed)
            }
            DemandKind::HumidityBelow { max, observed, .. } => (None, Some(*max), *observed),
            _ => (None, None, None),
        };
        if let (Some(lo), Some(hi)) = (lo, hi) {
            metrics.push(ReasonFact::new("Band", format!("{lo:.1}–{hi:.1}"), None));
        } else if let Some(hi) = hi {
            metrics.push(ReasonFact::new("Keep at/below", format!("{hi:.1}"), None));
        }
        if let Some(o) = obs {
            metrics.push(ReasonFact::new("Observed", format!("{o:.1}"), None));
        }
        metrics.push(ReasonFact::new("Planned", dur_minutes(scheduled_min), None));
        if unmet > 1e-6 {
            metrics.push(ReasonFact::new("Out-of-band (deg·steps)", format!("{unmet:.1}"), None));
            binding = Some("can't hold the band within the legal/price limits".into());
            fix_hint = Some("widen the band, raise the price cap, or extend the window".into());
            narrative = "Setpoint can't be held inside the limits.".into();
        } else if scheduled_min > 0.0 {
            narrative = format!("Pre-conditions {} to hold the band.", dur_minutes(scheduled_min));
        } else {
            narrative = "Within band — no action needed.".into();
        }
    }

    if !c.authority {
        narrative = format!("{narrative} (observe-only — previewed, not executed)");
    }

    Reasoning { narrative, binding, fix_hint, metrics, inputs, steps, blocks }
}

/// The "Why" for one storage device (battery / EV).
pub fn for_storage(
    cfg: &StorageConfig,
    plan: &StoragePlan,
    action: &str,
    authority: bool,
    grid: &Grid,
) -> Reasoning {
    let soc_now = plan.soc_kwh.first().copied().unwrap_or(0.0);
    let pct = if plan.capacity_kwh > 0.0 { 100.0 * soc_now / plan.capacity_kwh } else { 0.0 };

    let mut metrics = vec![
        ReasonFact::new("SoC now", format!("{soc_now:.1} kWh ({pct:.0}%)"), None),
        ReasonFact::new(
            "Usable",
            format!("{:.1}–{:.1} kWh", plan.min_soc_kwh, plan.max_soc_kwh),
            None,
        ),
    ];
    if plan.target_unmet > 0.05 {
        metrics.push(ReasonFact::new(
            "Target short",
            format!("{:.1} kWh", plan.target_unmet),
            None,
        ));
    }

    // Resolved inputs with their source entities.
    let mut inputs = vec![
        ReasonFact::new(
            "Capacity",
            format!("{:.1} kWh", plan.capacity_kwh),
            vr_src(&cfg.capacity_kwh),
        ),
        ReasonFact::new("Max charge", fmt_kw(&cfg.max_charge_kw), vr_src(&cfg.max_charge_kw)),
        ReasonFact::new(
            "Max discharge",
            fmt_kw(&cfg.max_discharge_kw),
            vr_src(&cfg.max_discharge_kw),
        ),
        ReasonFact::new(
            "Reserve floor",
            fmt_pct(&cfg.reserve_soc_pct),
            vr_src(&cfg.reserve_soc_pct),
        ),
        ReasonFact::new("Max SoC", fmt_pct(&cfg.max_soc_pct), vr_src(&cfg.max_soc_pct)),
    ];
    for g in &cfg.goals {
        match g {
            StorageGoalCfg::Target { soc_pct, ready_by } => {
                inputs.push(ReasonFact::new(
                    "Target SoC",
                    soc_pct.as_literal().map(|v| format!("{v:.0}%")).unwrap_or("live".into()),
                    vr_src(soc_pct),
                ));
                inputs.push(ReasonFact::new(
                    "Ready by",
                    match ready_by {
                        TimeRef::Literal(s) => s.clone(),
                        TimeRef::Entity { .. } => "live".into(),
                    },
                    ready_by.source().map(str::to_string),
                ));
            }
            StorageGoalCfg::Price { below, .. } => {
                inputs.push(ReasonFact::new(
                    "Charge below",
                    below.as_literal().map(|v| format!("${v:.3}/kWh")).unwrap_or("live".into()),
                    vr_src(below),
                ));
            }
        }
    }

    // Charge / discharge blocks.
    let charge_on: Vec<bool> = plan.charge_kw.iter().map(|&kw| kw > 1e-3).collect();
    let discharge_on: Vec<bool> = plan.discharge_kw.iter().map(|&kw| kw > 1e-3).collect();
    let no_ct = vec![false; grid.steps.len()];
    let mut blocks = plan_blocks(&charge_on, &no_ct, grid, "charge", "charge");
    blocks.extend(plan_blocks(&discharge_on, &no_ct, grid, "discharge", "discharge"));

    let has_target = cfg.goals.iter().any(|g| matches!(g, StorageGoalCfg::Target { .. }));
    let (mut binding, mut fix_hint) = (None, None);
    let mut narrative = if plan.target_unmet > 0.05 {
        binding = Some("deadline target can't be reached in time".into());
        fix_hint = Some(
            "allow grid charging, raise charge power, or lower the target / push the deadline"
                .into(),
        );
        format!("Short {:.1} kWh of target by the deadline.", plan.target_unmet)
    } else {
        match action {
            "charging" => "Charging now (cheap import or surplus solar).".into(),
            "discharging" => "Discharging now to avoid dear import.".into(),
            _ if has_target => "On track for the target; holding for now.".into(),
            _ => "Holding — self-arbitraging against price.".into(),
        }
    };
    if !authority {
        narrative = format!("{narrative} (advisory — planned, not actuated)");
    }

    Reasoning { narrative, binding, fix_hint, metrics, inputs, steps: Vec::new(), blocks }
}

fn fmt_kw(vr: &ValueRef) -> String {
    vr.as_literal().map(|v| format!("{v:.1} kW")).unwrap_or("live".into())
}
fn fmt_pct(vr: &ValueRef) -> String {
    vr.as_literal().map(|v| format!("{v:.0}%")).unwrap_or("live".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DemandKind;
    use crate::testkit::*;

    fn hot_water_cfg() -> LoadConfig {
        // Entity-refs on the tunables so the Why panel can show their source.
        let yaml = "
global:
  enabled_entity: input_boolean.x
  pricing: { import_entity: sensor.p }
loads:
  - id: hot_water
    type: hot_water
    planning: runtime
    authority: { enabled_entity: binary_sensor.hw_auto }
    control:
      start: { service: input_boolean.turn_on, target: input_boolean.hot_water }
      stop: { service: input_boolean.turn_off, target: input_boolean.hot_water }
    state: { running_entity: binary_sensor.hw_running }
    capability: { power_kw: { entity: sensor.hw_power } }
    hard_rules:
      min_run_minutes: { entity: input_number.hw_min_run }
      min_off_minutes: 15
      max_starts_per_day: 3
    must_have:
      kind: runtime
      amount_hours: { entity: input_number.hw_runtime }
      window: { start: \"00:00\", end: { entity: input_datetime.hw_deadline } }
    preferences: { start_cost_aud: 0.02 }
";
        crate::config::parse(yaml).expect("registry parses").loads.remove(0)
    }

    fn grid_4am() -> Grid {
        Grid::build(sydney(2026, 6, 10, 4, 0), 15, 24).unwrap()
    }

    #[test]
    fn observe_only_load_still_gets_reasoning_with_sources() {
        let cfg = hot_water_cfg();
        let mut c = runtime_contract();
        c.authority = false; // observe-only — the old code blanked these
        let grid = grid_4am();
        let n = grid.steps.len();
        let r = for_load(&cfg, &c, None, &grid, &vec![Some(0.17); n], &vec![0.0; n]);

        assert!(r.narrative.contains("observe-only"), "narrative: {}", r.narrative);
        let min_run = r.inputs.iter().find(|f| f.label == "Min run").expect("min run input");
        assert_eq!(min_run.source.as_deref(), Some("input_number.hw_min_run"));
        let win = r.inputs.iter().find(|f| f.label == "Window end").expect("window end input");
        assert_eq!(win.source.as_deref(), Some("input_datetime.hw_deadline"));
        assert!(r.steps.iter().any(|b| b.label == "available"));
        // 24 h horizon + a daily 00:00–06:30 window = 2 instances → Required is the
        // horizon-wide 2×90 min = 3.0 h (reconciles with horizon-wide Planned/Unmet).
        let req = r.metrics.iter().find(|m| m.label == "Required").expect("required metric");
        assert_eq!(req.value, "3.0 h", "required scaled by instance count");
    }

    #[test]
    fn shortfall_names_the_binding_window_and_a_fix() {
        let cfg = hot_water_cfg();
        let mut c = runtime_contract();
        if let DemandKind::Runtime { minutes, .. } = &mut c.must_have.kind {
            *minutes = 360; // 6 h required into a 00:00–06:30 window, at 04:00
        }
        // Short horizon: only 04:00–06:30 (150 min) is in-window — genuinely too tight.
        let grid = Grid::build(sydney(2026, 6, 10, 4, 0), 15, 4).unwrap();
        let n = grid.steps.len();
        let mut on = vec![false; n];
        on[0] = true; // 15 min scheduled
        let plan = LoadPlan { id: c.id.clone(), on, ct: vec![false; n], unmet: 345.0 };
        let r = for_load(&cfg, &c, Some(&plan), &grid, &vec![Some(0.17); n], &vec![0.0; n]);

        assert!(r.narrative.contains("short"), "narrative: {}", r.narrative);
        assert!(r.binding.as_deref().unwrap_or("").contains("window"), "binding: {:?}", r.binding);
        assert!(r.fix_hint.is_some(), "should suggest a fix");
        assert!(r.metrics.iter().any(|m| m.label == "Unmet"));
        assert!(r.steps.iter().any(|b| b.label == "outside window" && b.count > 0));
    }

    #[test]
    fn setpoint_load_reasoning_reports_band_and_observed() {
        // Aircon (predictive temperature band) — the setpoint arm of for_load.
        let yaml = "
global:
  enabled_entity: input_boolean.x
  pricing: { import_entity: sensor.p }
loads:
  - id: aircon
    type: aircon
    planning: predictive
    authority: { enabled_entity: binary_sensor.ac_auto }
    control:
      start: { service: input_boolean.turn_on, target: input_boolean.aircon }
      stop: { service: input_boolean.turn_off, target: input_boolean.aircon }
    state: { running_entity: climate.ac_0, observed_entity: sensor.temp_inside }
    capability:
      power_kw: { entity: sensor.ac_power }
      change_per_hour: 1.5
      drift_per_hour: 1.0
      ambient_entity: sensor.temp_outside
    hard_rules: { min_run_minutes: 20, min_off_minutes: 10 }
    must_have:
      kind: temperature_band
      target_c: { entity: input_number.ac_target }
      band_c: { entity: input_number.ac_band }
      window: { start: \"07:00\", end: \"22:00\" }
      max_price: { entity: input_number.ac_max_price }
    preferences: { start_cost_aud: 0.05 }
";
        let cfg = crate::config::parse(yaml).expect("aircon registry parses").loads.remove(0);
        let c = predictive_contract(Some(27.0), Some(31.0)); // observed 27, band 19–25
        let grid = Grid::build(sydney(2026, 6, 10, 12, 0), 15, 6).unwrap();
        let n = grid.steps.len();
        let mut on = vec![false; n];
        on[0] = true;
        on[1] = true;
        let plan = LoadPlan { id: c.id.clone(), on, ct: vec![false; n], unmet: 0.0 };
        let r = for_load(&cfg, &c, Some(&plan), &grid, &vec![Some(0.20); n], &vec![0.0; n]);

        assert!(r.metrics.iter().any(|m| m.label == "Band"), "band metric present");
        assert!(r.metrics.iter().any(|m| m.label == "Observed"), "observed metric present");
        // power_kw is entity-backed → its source surfaces in the inputs
        let pw = r.inputs.iter().find(|f| f.label == "Power").expect("power input");
        assert_eq!(pw.source.as_deref(), Some("sensor.ac_power"));
        assert!(!r.narrative.is_empty());
    }

    fn storage_cfg() -> StorageConfig {
        let yaml = "
global:
  enabled_entity: input_boolean.x
  pricing: { import_entity: sensor.p }
  storage:
    - id: sonnen01
      soc_entities: [sensor.soc]
      capacity_kwh: { entity: sensor.cap }
      max_charge_kw: 4.0
      max_discharge_kw: 4.0
      reserve_soc_pct: { entity: input_number.reserve }
      goals:
        - kind: target
          soc_pct: { entity: input_number.target_soc }
          ready_by: { entity: input_datetime.peak_start }
        - kind: price
          below: { value: 0.10 }
loads: []
";
        crate::config::parse(yaml).expect("storage registry parses").global.storage.remove(0)
    }

    fn storage_plan(target_unmet: f64, charging: bool) -> StoragePlan {
        let n = 8;
        let mut charge_kw = vec![0.0; n];
        if charging {
            charge_kw[0] = 4.0;
            charge_kw[1] = 4.0;
        }
        StoragePlan {
            id: "sonnen01".into(),
            soc_kwh: vec![4.9; n + 1],
            charge_kw,
            discharge_kw: vec![0.0; n],
            capacity_kwh: 9.0,
            min_soc_kwh: 0.9,
            max_soc_kwh: 9.0,
            target_unmet,
        }
    }

    #[test]
    fn storage_reasoning_explains_a_target_shortfall_with_sources() {
        let cfg = storage_cfg();
        let grid = Grid::build(sydney(2026, 6, 10, 4, 0), 15, 2).unwrap();
        let plan = storage_plan(2.0, true); // 2 kWh short of target, charging
        let r = for_storage(&cfg, &plan, "idle", false, &grid);

        assert!(r.narrative.contains("Short") && r.narrative.contains("advisory"));
        assert!(r.binding.is_some() && r.fix_hint.is_some());
        assert!(r.metrics.iter().any(|m| m.label == "Target short"));
        // both goal arms surfaced, with the deadline entity as the ready-by source
        let rb = r.inputs.iter().find(|f| f.label == "Ready by").expect("ready by input");
        assert_eq!(rb.source.as_deref(), Some("input_datetime.peak_start"));
        assert!(r.inputs.iter().any(|f| f.label == "Charge below"));
        assert!(r.blocks.iter().any(|b| b.kind == "charge"), "a charge block is reported");
    }

    #[test]
    fn storage_reasoning_on_track_when_authorised_and_charging() {
        let cfg = storage_cfg();
        let grid = Grid::build(sydney(2026, 6, 10, 4, 0), 15, 2).unwrap();
        let plan = storage_plan(0.0, true);
        let r = for_storage(&cfg, &plan, "charging", true, &grid);

        assert!(r.narrative.contains("Charging"), "narrative: {}", r.narrative);
        assert!(!r.narrative.contains("advisory"), "authorised devices aren't advisory");
        assert!(r.binding.is_none());
    }
}
