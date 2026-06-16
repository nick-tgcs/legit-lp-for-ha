//! SolveReport: the single per-cycle view-model behind both the log lines and
//! the web panel (serialised as JSON for ./api/status and the SSE stream).

use std::collections::VecDeque;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LoadReport {
    pub id: String,
    pub planning: String,
    pub authority: bool,
    pub running: Option<bool>,
    pub action: String,
    pub reason: String,
    pub unmet: f64,
    pub executed: bool,
    /// Planned on/off + can-take credit over the horizon (grid-aligned).
    pub on: Vec<bool>,
    pub ct: Vec<bool>,
}

/// The planned trajectory of one storage device, mirrored from the solver for
/// the panel. Energy in kWh, power in kW. `soc_kwh` has one more element than
/// the grid (the SoC entering each step, plus the end state); `charge_kw` /
/// `discharge_kw` are grid-aligned.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StorageReport {
    pub id: String,
    pub capacity_kwh: f64,
    pub min_soc_kwh: f64,
    pub max_soc_kwh: f64,
    pub soc_now_kwh: f64,
    pub soc_kwh: Vec<f64>,
    pub charge_kw: Vec<f64>,
    pub discharge_kw: Vec<f64>,
    /// Current-step action: "charging" | "discharging" | "idle".
    pub action: String,
    /// Unmet target energy (kWh) across this device's deadline goals; 0 = met.
    pub target_unmet: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct SolveReport {
    /// RFC3339 local timestamps (view-model: strings, not chrono types).
    pub at: String,
    pub solver_ms: u64,
    pub dry_run: bool,
    pub global_enabled: bool,
    /// Effective preview (shadow-solve) state this cycle: observe-only loads are
    /// solved for the panel but never executed. True when the HA preview boolean
    /// is on OR the in-panel checkbox toggled it on. The panel checkbox binds to it.
    pub preview: bool,
    pub price_now: Option<f64>,
    pub pv_now: Option<f64>,
    pub consumption_now: Option<f64>,
    pub grid: Vec<String>,
    /// Grid-aligned forecast series the plan was solved against (all len = grid,
    /// except where noted). Mirrored from the solver's WorldState so the panel
    /// can draw price/solar/grid context on the same time axis as the plan.
    /// Import price per step ($/kWh); `null` = genuinely unknown.
    pub price: Vec<Option<f64>>,
    /// Feed-in / export value per step ($/kWh).
    pub feedin: Vec<f64>,
    /// PV generation forecast per step (kW).
    pub pv: Vec<f64>,
    /// Unmanaged baseline consumption per step (kW).
    pub baseload: Vec<f64>,
    /// Net grid power per step (kW): +import / −export, from the solved balance.
    pub grid_kw: Vec<f64>,
    /// Planned trajectory per storage device (empty when none modelled).
    pub storage: Vec<StorageReport>,
    pub loads: Vec<LoadReport>,
    pub diagnostics: Vec<String>,
}

impl SolveReport {
    pub fn log_lines(&self) -> Vec<String> {
        self.loads
            .iter()
            .map(|l| {
                let exec = if l.executed {
                    ""
                } else if self.dry_run {
                    " [dry-run]"
                } else {
                    ""
                };
                format!("{}: {}{exec}", l.id, l.reason)
            })
            .chain(self.diagnostics.iter().map(|d| format!("diag: {d}")))
            .collect()
    }
}

/// Bounded decision log backing the panel (newest last).
#[derive(Debug, Default)]
pub struct DecisionLog {
    cap: usize,
    items: VecDeque<String>,
}

impl DecisionLog {
    pub fn new(cap: usize) -> Self {
        Self { cap, items: VecDeque::new() }
    }

    pub fn push(&mut self, line: String) {
        if self.items.len() == self.cap {
            self.items.pop_front();
        }
        self.items.push_back(line);
    }

    pub fn lines(&self) -> Vec<String> {
        self.items.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_render_loads_then_diagnostics() {
        let r = SolveReport {
            at: "2026-06-10T10:00:00+10:00".into(),
            loads: vec![LoadReport {
                id: "hot_water".into(),
                planning: "runtime".into(),
                authority: true,
                running: Some(false),
                action: "Start".into(),
                reason: "start; lp plan (price 0.050)".into(),
                unmet: 0.0,
                executed: true,
                on: vec![true],
                ct: vec![false],
            }],
            diagnostics: vec!["forecast 4m old".into()],
            ..Default::default()
        };
        assert_eq!(
            r.log_lines(),
            vec!["hot_water: start; lp plan (price 0.050)", "diag: forecast 4m old"]
        );
    }

    #[test]
    fn report_serializes_forecast_and_storage_series() {
        let r = SolveReport {
            grid: vec!["t0".into(), "t1".into()],
            price: vec![Some(0.10), None],
            feedin: vec![0.05, 0.05],
            pv: vec![1.0, 2.0],
            baseload: vec![0.8, 0.8],
            grid_kw: vec![-0.2, 1.2],
            storage: vec![StorageReport {
                id: "sonnen".into(),
                capacity_kwh: 10.0,
                min_soc_kwh: 1.0,
                max_soc_kwh: 10.0,
                soc_now_kwh: 5.0,
                soc_kwh: vec![5.0, 6.0],
                charge_kw: vec![4.0],
                discharge_kw: vec![0.0],
                action: "charging".into(),
                target_unmet: 0.0,
            }],
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        // Unknown price is null (a gap), not a zero.
        assert_eq!(v["price"][1], serde_json::Value::Null);
        assert_eq!(v["grid_kw"][1], 1.2);
        assert_eq!(v["pv"][1], 2.0);
        assert_eq!(v["storage"][0]["id"], "sonnen");
        assert_eq!(v["storage"][0]["action"], "charging");
        assert_eq!(v["storage"][0]["soc_kwh"][1], 6.0);
        // A no-storage report is structurally clean — storage is an empty array.
        let bare = SolveReport::default();
        let v: serde_json::Value = serde_json::to_value(&bare).unwrap();
        assert!(v["storage"].as_array().unwrap().is_empty());
        assert!(v["grid_kw"].as_array().unwrap().is_empty());
    }

    #[test]
    fn report_exposes_the_preview_flag_for_the_panel_checkbox() {
        // The in-panel preview checkbox binds to this field so it reflects the
        // server's effective preview state (HA boolean OR runtime override).
        // Unit level: the view-model contract; the endpoint that flips it and the
        // OR resolution are covered in tests/web.rs and tests/cycle.rs.
        let bare = SolveReport::default();
        let v: serde_json::Value = serde_json::to_value(&bare).unwrap();
        assert_eq!(v["preview"], false, "preview present in the JSON and defaults off");
        let on = SolveReport { preview: true, ..Default::default() };
        assert_eq!(serde_json::to_value(&on).unwrap()["preview"], true);
    }

    #[test]
    fn decision_log_caps() {
        let mut log = DecisionLog::new(2);
        for i in 0..5 {
            log.push(format!("line {i}"));
        }
        assert_eq!(log.lines(), vec!["line 3", "line 4"]);
    }
}
