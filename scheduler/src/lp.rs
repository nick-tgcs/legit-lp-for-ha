//! The planner: one MILP over the horizon, solved with HiGHS via good_lp.
//! Two lexicographic stages: (1) minimise total must-have shortfall (`unmet`),
//! (2) minimise net site cost (import/export split, can-take valued at its
//! ceiling, per-start wear cost). Execute only the current step (MPC).
//!
//! There is no fallback engine: must-have carries slack so the model is
//! always feasible; a genuine solver error returns NoChange-for-all + reason.

use good_lp::solvers::highs::highs;
use good_lp::{constraint, variable, variables, Expression, Solution, SolverModel, Variable};

use crate::model::*;
use crate::rules::{self, Masks};
use crate::time::{window_instances, Grid};

pub struct LpPlanner {
    pub grid_minutes: u32,
    pub horizon_hours: u32,
}

/// The horizon plan for one load (diagnostics + the web panel).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadPlan {
    pub id: LoadId,
    /// Planned on/off per grid step (empty for observe-only loads).
    pub on: Vec<bool>,
    /// Steps credited to can-take.
    pub ct: Vec<bool>,
    /// Total must-have shortfall (minutes for runtime; band-degree-steps for
    /// setpoint loads). 0 = fully satisfiable inside the legal space.
    pub unmet: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanOutput {
    pub decisions: Vec<Decision>,
    pub plans: Vec<LoadPlan>,
    /// Grid step starts (for rendering); empty when nothing was solved.
    pub grid: Vec<chrono::DateTime<chrono_tz::Tz>>,
}

/// Per-load variable bundle inside the MILP.
struct LoadVars {
    idx: usize, // index into the original `loads` slice
    x: Vec<Variable>,
    start: Vec<Variable>,
    stop: Vec<Variable>,
    ct: Vec<Variable>,
    unmet: Vec<Variable>,
    masks: Masks,
    running0: bool,
}

impl LpPlanner {
    pub fn plan(&self, world: &WorldState, loads: &[LoadContract]) -> PlanOutput {
        let mut decisions = vec![None; loads.len()];
        let mut plans: Vec<LoadPlan> = Vec::new();

        if !world.global_enabled {
            let decisions = loads
                .iter()
                .map(|c| Decision {
                    load_id: c.id.clone(),
                    action: Action::NoChange,
                    reason: "blocked; global scheduler disabled".into(),
                })
                .collect();
            return PlanOutput { decisions, plans, grid: vec![] };
        }

        let grid = match Grid::build(world.now, self.grid_minutes, self.horizon_hours) {
            Ok(g) => g,
            Err(e) => {
                return self.hold_all(loads, format!("solver error: {e}"));
            }
        };
        let n = grid.steps.len();
        if world.import.len() != n
            || world.feedin.len() != n
            || world.pv.len() != n
            || world.baseload.len() != n
        {
            return self.hold_all(loads, "solver error: world series not grid-sized".into());
        }
        let surplus: Vec<f64> =
            (0..n).map(|t| (world.pv[t] - world.baseload[t]).max(0.0)).collect();

        // Observe-only loads get decisions immediately; the rest enter the MILP.
        let mut milp: Vec<usize> = Vec::new();
        for (i, c) in loads.iter().enumerate() {
            if !c.authority {
                decisions[i] = Some(Decision {
                    load_id: c.id.clone(),
                    action: Action::NoChange,
                    reason: "observe-only; scheduler authority disabled".into(),
                });
            } else if c.obs.running.is_none() {
                decisions[i] = Some(Decision {
                    load_id: c.id.clone(),
                    action: Action::NoChange,
                    reason: "observe-only; running state unknown".into(),
                });
            } else {
                milp.push(i);
            }
        }

        if milp.is_empty() {
            let decisions = decisions.into_iter().map(Option::unwrap).collect();
            return PlanOutput { decisions, plans, grid: grid.steps.clone() };
        }

        // ---- stage 1: minimise total unmet ---------------------------------
        let stage1 = match self.solve(world, loads, &milp, &grid, &surplus, None) {
            Ok(s) => s,
            Err(e) => return self.hold_all(loads, format!("solver error: {e}")),
        };
        // ---- stage 2: freeze unmet, minimise net cost ----------------------
        let solved =
            match self.solve(world, loads, &milp, &grid, &surplus, Some(stage1.total_unmet)) {
                Ok(s) => s,
                Err(e) => return self.hold_all(loads, format!("solver error: {e}")),
            };

        for lp in solved.plans {
            let i = lp.idx;
            let c = &loads[i];
            let running = c.obs.running.unwrap_or(false);
            let want_on = lp.on[0];
            let action = match (running, want_on) {
                (false, true) => Action::Start,
                (true, false) => Action::Stop,
                _ => Action::NoChange,
            };
            let price = world.price_now.map(|p| format!("{p:.3}")).unwrap_or("?".into());
            let reason = match action {
                Action::Start if lp.ct[0] => format!("start; can-take step (price {price})"),
                Action::Start => format!("start; lp plan (price {price})"),
                Action::Stop => format!("stop; lp plan (price {price})"),
                Action::NoChange if lp.unmet > 1e-6 => format!(
                    "{}; must-have unmet {:.0} (legal space too tight)",
                    if running { "run" } else { "hold" },
                    lp.unmet
                ),
                Action::NoChange => {
                    format!("{}; lp plan", if running { "run" } else { "idle" })
                }
            };
            decisions[i] = Some(Decision { load_id: c.id.clone(), action, reason });
            plans.push(LoadPlan { id: c.id.clone(), on: lp.on, ct: lp.ct, unmet: lp.unmet });
        }

        let decisions = decisions.into_iter().map(Option::unwrap).collect();
        PlanOutput { decisions, plans, grid: grid.steps.clone() }
    }

    fn hold_all(&self, loads: &[LoadContract], reason: String) -> PlanOutput {
        tracing::error!("{reason}; holding all loads this cycle");
        let decisions = loads
            .iter()
            .map(|c| Decision {
                load_id: c.id.clone(),
                action: Action::NoChange,
                reason: reason.clone(),
            })
            .collect();
        PlanOutput { decisions, plans: vec![], grid: vec![] }
    }

    #[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
    fn solve(
        &self,
        world: &WorldState,
        loads: &[LoadContract],
        milp: &[usize],
        grid: &Grid,
        surplus: &[f64],
        unmet_cap: Option<f64>,
    ) -> Result<SolvedStage, String> {
        let n = grid.steps.len();
        let dt_h = f64::from(self.grid_minutes) / 60.0;
        let step_min = f64::from(self.grid_minutes);
        let mut vars = variables!();
        let mut lvs: Vec<LoadVars> = Vec::new();
        let mut constraints: Vec<good_lp::Constraint> = Vec::new();
        let mut unmet_expr = Expression::from(0.0);
        let mut cost_expr = Expression::from(0.0);

        for &i in milp {
            let c = &loads[i];
            let running0 = c.obs.running.unwrap_or(false);
            let masks = rules::masks(c, grid, &world.import, surplus);
            let x: Vec<Variable> = (0..n).map(|_| vars.add(variable().binary())).collect();
            let start: Vec<Variable> = (0..n).map(|_| vars.add(variable().binary())).collect();
            let stop: Vec<Variable> = (0..n).map(|_| vars.add(variable().binary())).collect();
            let has_ct = c.can_take.is_some();
            let ct: Vec<Variable> = (0..n)
                .map(|_| vars.add(if has_ct { variable().binary() } else { variable().min(0).max(0) }))
                .collect();

            // Hard windows + per-demand price gate (x <= ok_mh + ct).
            for t in 0..n {
                if !masks.hard_ok[t] {
                    constraints.push(constraint!(x[t] <= 0));
                }
                let mh_ok = f64::from(u8::from(masks.ok_mh[t]));
                constraints.push(constraint!(x[t] <= mh_ok + ct[t]));
                if !masks.ok_ct[t] {
                    constraints.push(constraint!(ct[t] <= 0));
                }
                constraints.push(constraint!(ct[t] <= x[t]));
            }

            // start/stop linking with x[-1] = running0.
            let x_prev0 = f64::from(u8::from(running0));
            constraints.push(constraint!(start[0] >= x[0] - x_prev0));
            constraints.push(constraint!(stop[0] >= x_prev0 - x[0]));
            for t in 1..n {
                constraints.push(constraint!(start[t] >= x[t] - x[t - 1]));
                constraints.push(constraint!(stop[t] >= x[t - 1] - x[t]));
            }

            // Min up/down (aggregated window form) + initial locks.
            let up = rules::min_up_steps(c, grid).min(n);
            let down = rules::min_down_steps(c, grid).min(n);
            for t in 0..n {
                if up > 1 {
                    let lo = t.saturating_sub(up - 1);
                    let sum: Expression = (lo..=t).map(|k| start[k]).sum();
                    constraints.push(constraint!(sum <= x[t]));
                }
                if down > 1 {
                    let lo = t.saturating_sub(down - 1);
                    let sum: Expression = (lo..=t).map(|k| stop[k]).sum();
                    constraints.push(constraint!(sum <= 1 - x[t]));
                }
            }
            let lock = rules::initial_lock(c, grid);
            for t in 0..lock.on_steps.min(n) {
                constraints.push(constraint!(x[t] >= 1));
            }
            for t in 0..lock.off_steps.min(n) {
                constraints.push(constraint!(x[t] <= 0));
            }

            // Max starts per local calendar day.
            if let Some(max) = c.hard.max_starts_per_day {
                let today = grid.steps[0].date_naive();
                let mut by_day: std::collections::BTreeMap<chrono::NaiveDate, Vec<usize>> =
                    Default::default();
                for t in 0..n {
                    by_day.entry(grid.steps[t].date_naive()).or_default().push(t);
                }
                for (day, ts) in by_day {
                    let budget = if day == today {
                        f64::from(rules::starts_remaining_today(c).unwrap_or(max))
                    } else {
                        f64::from(max)
                    };
                    let sum: Expression = ts.iter().map(|&t| start[t]).sum();
                    constraints.push(constraint!(sum <= budget));
                }
            }

            // ---- demands ----------------------------------------------------
            let mut unmet_vars: Vec<Variable> = Vec::new();
            match (&c.planning, &c.must_have.kind) {
                (Planning::Runtime, DemandKind::Runtime { minutes, window, completed_minutes }) => {
                    for inst in window_instances(window, grid) {
                        let completed =
                            if inst.steps.start == 0 { f64::from(*completed_minutes) } else { 0.0 };
                        let required = f64::from(*minutes);
                        let u = vars.add(variable().min(0));
                        unmet_vars.push(u);
                        let credit: Expression =
                            inst.steps.clone().map(|t| (x[t] - ct[t]) * step_min).sum();
                        constraints.push(constraint!(credit + completed + u >= required));
                    }
                }
                (Planning::Immediate, kind) => {
                    if immediate_needs_on(kind, running0) == Some(true) {
                        let u = vars.add(variable().min(0).max(1));
                        unmet_vars.push(u);
                        constraints.push(constraint!(x[0] + u >= 1));
                    }
                }
                (Planning::Predictive, DemandKind::TemperatureBand {
                    min,
                    max,
                    observed: Some(level0),
                    change_per_hour,
                    drift_per_hour,
                    ambient,
                    window,
                    ..
                }) => {
                    {
                        let target = (min + max) / 2.0;
                        let run_dir = if *level0 > target { -1.0 } else { 1.0 };
                        let drift_dir = match ambient {
                            Some(a) if *a < *level0 => -1.0,
                            Some(_) => 1.0,
                            None => -run_dir, // unknown ambient: drift opposes the unit
                        };
                        let rate = change_per_hour * run_dir * dt_h;
                        let drift = drift_per_hour * drift_dir * dt_h;
                        let level: Vec<Variable> =
                            (0..=n).map(|_| vars.add(variable())).collect();
                        constraints.push(constraint!(level[0] == *level0));
                        for t in 0..n {
                            constraints.push(constraint!(
                                level[t + 1] == level[t] + rate * x[t] + drift * (1 - x[t])
                            ));
                        }
                        for inst in window_instances(window, grid) {
                            for t in inst.steps {
                                let u_hi = vars.add(variable().min(0));
                                let u_lo = vars.add(variable().min(0));
                                unmet_vars.push(u_hi);
                                unmet_vars.push(u_lo);
                                constraints.push(constraint!(level[t + 1] <= *max + u_hi));
                                constraints.push(constraint!(level[t + 1] >= *min - u_lo));
                            }
                        }
                    }
                }
                _ => {} // validation prevents other combinations
            }

            // Can-take cap per window instance (recorder-used minutes count).
            if let Some(ct_demand) = &c.can_take {
                let cap = ct_cap_minutes(ct_demand);
                if let (Some(cap), Some(w)) = (cap, ct_window(ct_demand)) {
                    for inst in window_instances(&w, grid) {
                        let used = if inst.steps.start == 0 {
                            c.obs.runtime_in_ct_window.as_secs() as f64 / 60.0
                        } else {
                            0.0
                        };
                        let sum: Expression =
                            inst.steps.clone().map(|t| ct[t] * step_min).sum();
                        constraints.push(constraint!(sum + used <= f64::from(cap)));
                    }
                }
                // Valuation: can-take is worth its declared ceiling.
                if let Some(value) = ct_demand.max_price {
                    for t in 0..n {
                        cost_expr += ct[t] * (-value * c.power_kw * dt_h);
                    }
                }
            }

            for u in &unmet_vars {
                unmet_expr += *u;
            }
            for t in 0..n {
                cost_expr += start[t] * c.prefs.start_cost_aud;
                // Earlier-completion tie-break (plan preference tier): far
                // below a cent, only ever decides genuine indifference.
                cost_expr += x[t] * (1e-6 * t as f64);
            }

            lvs.push(LoadVars { idx: i, x, start, stop, ct, unmet: unmet_vars, masks, running0 });
        }

        // ---- site balance: imp - exp = baseload + Σ pkw·x − pv -------------
        for t in 0..n {
            let imp = vars.add(variable().min(0));
            // Physical bound: you cannot export more than you generate. This
            // also kills the meter-arbitrage unboundedness when the import
            // price drops below feed-in (real with Amber spikes/negatives).
            let exp = vars.add(variable().min(0).max(world.pv[t].max(0.0)));
            let mut managed = Expression::from(0.0);
            for lv in &lvs {
                managed += lv.x[t] * loads[lv.idx].power_kw;
            }
            let net = world.baseload[t] - world.pv[t];
            constraints.push(constraint!(imp - exp == managed + net));
            let price = world.import[t].unwrap_or(0.0);
            cost_expr += imp * (price * dt_h) - exp * (world.feedin[t] * dt_h);
        }

        let objective =
            if unmet_cap.is_none() { unmet_expr.clone() } else { cost_expr.clone() };
        let mut model = vars.minimise(objective).using(highs);
        for cns in constraints {
            model = model.with(cns);
        }
        if let Some(cap) = unmet_cap {
            model = model.with(constraint!(unmet_expr.clone() <= cap + 1e-6));
        }
        let sol = model.solve().map_err(|e| format!("{e:?}"))?;

        let total_unmet = sol.eval(&unmet_expr);
        let plans = lvs
            .iter()
            .map(|lv| SolvedLoad {
                idx: lv.idx,
                on: lv.x.iter().map(|v| sol.value(*v) > 0.5).collect(),
                ct: lv.ct.iter().map(|v| sol.value(*v) > 0.5).collect(),
                unmet: lv.unmet.iter().map(|u| sol.value(*u)).sum(),
            })
            .collect();
        Ok(SolvedStage { total_unmet, plans })
    }
}

struct SolvedStage {
    total_unmet: f64,
    plans: Vec<SolvedLoad>,
}

struct SolvedLoad {
    idx: usize,
    on: Vec<bool>,
    ct: Vec<bool>,
    unmet: f64,
}

/// Does an `immediate` load need to be on at the current step?
/// Trigger above max+hysteresis; once running, hold the need until back at/
/// below max (asymmetric clear kills band-edge chatter). `None` observed →
/// no demand signal at all.
fn immediate_needs_on(kind: &DemandKind, running: bool) -> Option<bool> {
    match kind {
        DemandKind::HumidityBelow { max, observed, start_hysteresis, .. } => {
            observed.map(|o| o > max + start_hysteresis || (running && o > *max))
        }
        DemandKind::TemperatureBand { min, max, observed, .. } => {
            observed.map(|o| o < *min || o > *max)
        }
        DemandKind::Runtime { .. } => Some(false),
    }
}

fn ct_cap_minutes(d: &Demand) -> Option<u32> {
    match &d.kind {
        DemandKind::Runtime { minutes, .. } => Some(*minutes),
        DemandKind::HumidityBelow { cap_minutes, .. }
        | DemandKind::TemperatureBand { cap_minutes, .. } => *cap_minutes,
    }
}

fn ct_window(d: &Demand) -> Option<Window> {
    match &d.kind {
        DemandKind::Runtime { window, .. } | DemandKind::TemperatureBand { window, .. } => {
            Some(*window)
        }
        DemandKind::HumidityBelow { window, .. } => *window,
    }
}
