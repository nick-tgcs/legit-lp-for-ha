//! Hard rules → pure data the MILP consumes: per-step masks, initial
//! min-run/min-off locks, and start budgets. No solver types in here.

use crate::model::{Demand, DemandKind, LoadContract, Window};
use crate::time::{in_window, round_up_to_steps, Grid};

/// Per-step permissions for one load. Masks PERMIT; the objective prices.
#[derive(Debug, Clone, PartialEq)]
pub struct Masks {
    /// Hard-rule windows (empty config = always allowed).
    pub hard_ok: Vec<bool>,
    /// Step may carry must-have-credited runtime.
    pub ok_mh: Vec<bool>,
    /// Step may carry can-take runtime (absent can_take = all false).
    pub ok_ct: Vec<bool>,
}

fn demand_window(d: &Demand) -> Option<Window> {
    match &d.kind {
        DemandKind::Runtime { window, .. } | DemandKind::TemperatureBand { window, .. } => {
            Some(*window)
        }
        DemandKind::HumidityBelow { window, .. } => *window,
    }
}

/// Build the per-step masks.
///
/// Price asymmetry (per plan): an unknown step price never blocks required
/// work (must-have ok), but optional can-take needs a known price at/below its
/// ceiling — unless the sun pays for it: forecast surplus covering the load's
/// draw passes either mask.
pub fn masks(c: &LoadContract, grid: &Grid, price: &[Option<f64>], surplus: &[f64]) -> Masks {
    let n = grid.steps.len();
    assert_eq!(price.len(), n, "price series sized to grid");
    assert_eq!(surplus.len(), n, "surplus series sized to grid");

    let hard_ok: Vec<bool> = grid
        .steps
        .iter()
        .map(|s| {
            c.hard.windows.is_empty() || c.hard.windows.iter().any(|w| in_window(s.time(), w))
        })
        .collect();

    let in_scope = |d: &Demand, i: usize| match demand_window(d) {
        Some(w) => in_window(grid.steps[i].time(), &w),
        None => true,
    };
    let sun_pays = |i: usize| surplus[i] >= c.power_kw;

    let ok_mh = (0..n)
        .map(|i| {
            in_scope(&c.must_have, i)
                && match c.must_have.max_price {
                    None => true,
                    Some(ceil) => match price[i] {
                        None => true, // never block required work on a missing price
                        Some(p) => p <= ceil || sun_pays(i),
                    },
                }
        })
        .collect();

    // Setpoint can-take only exists while the observation actually exceeds
    // its (tighter) target — an unknown sensor means no optional work, ever.
    let ct_wanted = match &c.can_take {
        None => false,
        Some(ct) => match &ct.kind {
            DemandKind::Runtime { .. } => true,
            DemandKind::HumidityBelow { max, observed, .. } => {
                observed.map(|o| o > *max).unwrap_or(false)
            }
            DemandKind::TemperatureBand { min, max, observed, .. } => {
                observed.map(|o| o < *min || o > *max).unwrap_or(false)
            }
        },
    };

    let ok_ct = (0..n)
        .map(|i| match &c.can_take {
            None => false,
            Some(ct) => {
                ct_wanted
                    && in_scope(ct, i)
                    && match ct.max_price {
                        None => true,
                        Some(ceil) => match price[i] {
                            Some(p) => p <= ceil || sun_pays(i),
                            None => sun_pays(i), // optional work needs a known price or sun
                        },
                    }
            }
        })
        .collect();

    Masks { hard_ok, ok_mh, ok_ct }
}

/// Locks already in progress at step 0, from the observed current stretch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InitialLock {
    /// Steps (from 0) the load must stay ON (min_run not yet met).
    pub on_steps: usize,
    /// Steps (from 0) the load must stay OFF (min_off not yet met).
    pub off_steps: usize,
}

pub fn initial_lock(c: &LoadContract, grid: &Grid) -> InitialLock {
    let step = grid.step_minutes;
    match c.obs.running {
        Some(true) if c.obs.current_stretch < c.hard.min_run => {
            let remaining = c.hard.min_run - c.obs.current_stretch;
            InitialLock { on_steps: round_up_to_steps(remaining, step) as usize, off_steps: 0 }
        }
        Some(false) if c.obs.current_stretch < c.hard.min_off => {
            let remaining = c.hard.min_off - c.obs.current_stretch;
            InitialLock { on_steps: 0, off_steps: round_up_to_steps(remaining, step) as usize }
        }
        _ => InitialLock::default(),
    }
}

/// Min up/down lengths in whole steps (rounded UP — never under-enforce).
pub fn min_up_steps(c: &LoadContract, grid: &Grid) -> usize {
    round_up_to_steps(c.hard.min_run, grid.step_minutes) as usize
}

pub fn min_down_steps(c: &LoadContract, grid: &Grid) -> usize {
    round_up_to_steps(c.hard.min_off, grid.step_minutes) as usize
}

/// Remaining start budget for the day containing step 0 (today): the recorder
/// already counted today's starts (manual + scheduler). Later days get the
/// full budget. `None` = unlimited.
pub fn starts_remaining_today(c: &LoadContract) -> Option<u32> {
    c.hard.max_starts_per_day.map(|m| m.saturating_sub(c.obs.starts_today))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Planning;
    use crate::testkit::*;
    use std::time::Duration;

    fn grid4() -> Grid {
        // 4 steps from 10:00: 10:00 10:15 10:30 10:45 (inside ct window 10-16).
        Grid::build(sydney(2026, 6, 10, 10, 0), 15, 1).unwrap()
    }

    #[test]
    fn price_asymmetry_and_surplus_override() {
        let mut c = runtime_contract();
        // Make must-have full-day with a ceiling so the price logic is visible.
        c.must_have.max_price = Some(0.20);
        if let crate::model::DemandKind::Runtime { window, .. } = &mut c.must_have.kind {
            *window = full_day();
        }
        let g = grid4();
        let price = [Some(0.30), Some(0.10), None, Some(0.05)].to_vec();
        let surplus = [0.0, 0.0, 5.0, 0.0].to_vec();
        let m = masks(&c, &g, &price, &surplus);

        // must-have: expensive -> false; cheap -> true; UNKNOWN -> true; cheap -> true
        assert_eq!(m.ok_mh, [false, true, true, true]);
        // can-take (ceiling 0.10): 0.30 no; 0.10 yes; unknown price BUT surplus
        // 5kW >= 3.6kW -> sun pays -> yes; 0.05 yes.
        assert_eq!(m.ok_ct, [false, true, true, true]);
    }

    #[test]
    fn unknown_price_blocks_can_take_without_surplus() {
        let c = runtime_contract();
        let g = grid4();
        let price = vec![None; 4];
        let surplus = vec![0.0; 4];
        let m = masks(&c, &g, &price, &surplus);
        assert_eq!(m.ok_ct, [false, false, false, false]);
        // ...but never blocks must-have (in-window steps only; window is
        // 00:00-06:30 and the grid is at 10:00, so scope makes these false).
        assert_eq!(m.ok_mh, [false, false, false, false]);
    }

    #[test]
    fn surplus_passes_must_have_above_ceiling() {
        let mut c = immediate_contract(Some(70.0)); // ceiling 0.15, full-day scope
        c.power_kw = 0.3;
        let g = grid4();
        let price = vec![Some(0.40); 4]; // way above ceiling all day
        let surplus = [0.0, 0.4, 0.2, 0.0].to_vec(); // covers 0.3kW only at step 1
        let m = masks(&c, &g, &price, &surplus);
        assert_eq!(m.ok_mh, [false, true, false, false]);
    }

    #[test]
    fn window_scope_gates_masks() {
        let c = runtime_contract(); // mh window 00:00-06:30, ct 10:00-16:00
        let g = Grid::build(sydney(2026, 6, 10, 22, 0), 15, 24).unwrap();
        let n = g.steps.len();
        let price = vec![Some(0.01); n]; // cheap everywhere
        let surplus = vec![0.0; n];
        let m = masks(&c, &g, &price, &surplus);
        // 22:00-24:00 out of mh scope (8 steps), then in-scope from midnight.
        assert!(!m.ok_mh[0] && !m.ok_mh[7]);
        assert!(m.ok_mh[8]); // 00:00
        assert!(m.ok_mh[8 + 25]); // 06:15 last in-window step
        assert!(!m.ok_mh[8 + 26]); // 06:30 excluded
        // ct window 10:00-16:00 tomorrow: starts at step 8+40=48.
        assert!(!m.ok_ct[47] && m.ok_ct[48]);
    }

    #[test]
    fn hard_windows_default_open_and_gate_when_set() {
        let mut c = runtime_contract();
        let g = grid4();
        let price = vec![Some(0.01); 4];
        let surplus = vec![0.0; 4];
        assert!(masks(&c, &g, &price, &surplus).hard_ok.iter().all(|&b| b));
        c.hard.windows = vec![window(10, 30, 11, 0)];
        let m = masks(&c, &g, &price, &surplus);
        assert_eq!(m.hard_ok, [false, false, true, true]);
    }

    #[test]
    fn initial_locks_from_current_stretch() {
        let g = grid4();
        let mut c = runtime_contract(); // min_run 20m, min_off 15m

        c.obs.running = Some(true);
        c.obs.current_stretch = Duration::from_secs(10 * 60); // 10 of 20 min
        assert_eq!(initial_lock(&c, &g), InitialLock { on_steps: 1, off_steps: 0 });

        c.obs.current_stretch = Duration::from_secs(25 * 60); // satisfied
        assert_eq!(initial_lock(&c, &g), InitialLock::default());

        c.obs.running = Some(false);
        c.obs.current_stretch = Duration::from_secs(5 * 60); // 5 of 15 min off
        assert_eq!(initial_lock(&c, &g), InitialLock { on_steps: 0, off_steps: 1 });

        c.obs.running = None; // unknown -> no locks (observe-only elsewhere)
        assert_eq!(initial_lock(&c, &g), InitialLock::default());
    }

    #[test]
    fn min_updown_steps_round_up_and_budget_saturates() {
        let g = grid4();
        let mut c = runtime_contract();
        assert_eq!(min_up_steps(&c, &g), 2); // 20min @15 -> 2
        assert_eq!(min_down_steps(&c, &g), 1); // 15min @15 -> 1
        c.obs.starts_today = 5; // already over the budget of 3 (manual starts)
        assert_eq!(starts_remaining_today(&c), Some(0));
        c.hard.max_starts_per_day = None;
        assert_eq!(starts_remaining_today(&c), None);
        let _ = c.planning == Planning::Runtime; // silence unused import note
    }
}
