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
    /// The "Why" explanation behind this plan (Overview + Why tabs).
    pub reasoning: Reasoning,
}

/// A single labelled fact in a Why panel. `source` is the HA entity the value came
/// from (`None` = a literal in the registry), so the user sees which control drove
/// it — the de-hardcoding made every operational value entity-traceable.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct ReasonFact {
    pub label: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl ReasonFact {
    pub fn new(label: impl Into<String>, value: impl Into<String>, source: Option<String>) -> Self {
        Self { label: label.into(), value: value.into(), source }
    }
}

/// One bucket of the per-step availability breakdown (why steps were/weren't usable).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StepBucket {
    pub label: String,
    pub count: u32,
}

/// A planned contiguous block rendered as human times.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlanBlock {
    pub start: String,
    pub end: String,
    pub hours: f64,
    /// "must-have" | "can-take" | "charge" | "discharge".
    pub kind: String,
}

/// The "Why" explanation for one device (load or storage). Built once per cycle in
/// `reasoning.rs` and serialised into the panel — works for observe-only devices too.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Reasoning {
    /// One-line plain-English overview (the Overview tab).
    pub narrative: String,
    /// The constraint that bound the outcome, when the device fell short.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// Plain "what would change it" hint when something is unmet (the user's call —
    /// the LP only reports; it never edits the settings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
    /// Outcome metrics (required/planned/unmet, or SoC/target) — label/value.
    pub metrics: Vec<ReasonFact>,
    /// Resolved live inputs the LP used, each with its source entity.
    pub inputs: Vec<ReasonFact>,
    /// Per-step availability breakdown (loads only; empty for storage).
    pub steps: Vec<StepBucket>,
    /// Planned run/charge blocks as time ranges.
    pub blocks: Vec<PlanBlock>,
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
    /// Any direction authorised (Optimiser)? false => the trajectory is advisory
    /// (the panel shows it as "(preview)"/"(dry-run)", never executed).
    pub authority: bool,
    /// Unmet target energy (kWh) across this device's deadline goals; 0 = met.
    pub target_unmet: f64,
    /// The "Why" explanation behind this device's planned trajectory.
    pub reasoning: Reasoning,
}

/// Triage level for a per-cycle issue. Serialises lowercase ("critical"/…) so the
/// panel can class rows and `main.rs` can pick the log level.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

/// A triaged, human-facing issue for this cycle — the layer above the raw
/// `diagnostics` bag. Surfaced three ways (banner, header chip, Alerts section) and
/// logged at its own level with a greppable `ALERT[<sev>]` prefix.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Alert {
    pub severity: Severity,
    /// "scheduler" | a load id | a storage id.
    pub scope: String,
    /// Short headline, e.g. "Run window unreadable".
    pub title: String,
    /// What happened + why + what the user can do about it.
    pub detail: String,
}

impl Alert {
    pub fn new(
        severity: Severity,
        scope: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self { severity, scope: scope.into(), title: title.into(), detail: detail.into() }
    }
    /// The leveled, greppable log line for this alert.
    pub fn log_line(&self) -> String {
        let sev = match self.severity {
            Severity::Critical => "critical",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        format!("ALERT[{sev}] {}: {}", self.scope, self.detail)
    }
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
    /// Triaged issues for this cycle (Critical/Warning/Info), derived from the
    /// signals behind `diagnostics` + the solve outcome. Empty = nothing to flag.
    pub alerts: Vec<Alert>,
    /// True when this report's PLAN is a carried-over previous cycle (the current
    /// solve failed). The price/now/alerts are fresh; the plan is `last_solved` old.
    pub stale: bool,
    /// Local timestamp (RFC3339) the on-screen plan was actually solved. Equals
    /// `at` for a fresh report; an earlier time when `stale`.
    pub last_solved: String,
}

impl SolveReport {
    /// True when this cycle's SOLVE failed (a scheduler-scoped Critical alert) — the
    /// signal the run loop uses to keep the last good plan on screen, marked stale.
    /// A load-scoped critical (e.g. an unreadable control) does NOT count: the rest
    /// of the plan is still valid.
    pub fn is_solver_failure(&self) -> bool {
        self.alerts.iter().any(|a| a.severity == Severity::Critical && a.scope == "scheduler")
    }

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
                reasoning: Reasoning::default(),
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
                authority: false,
                target_unmet: 0.0,
                reasoning: Reasoning::default(),
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
        // The Why panel's reasoning view-model is always serialised (object form).
        assert!(v["storage"][0]["reasoning"].is_object(), "reasoning present in storage JSON");
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
    fn alerts_serialise_with_lowercase_severity_and_greppable_log_line() {
        let a = Alert::new(
            Severity::Critical,
            "scheduler",
            "Could not solve",
            "hot_water infeasible; all loads held",
        );
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["severity"], "critical", "severity serialises lowercase for the panel");
        assert_eq!(v["scope"], "scheduler");
        assert_eq!(a.log_line(), "ALERT[critical] scheduler: hot_water infeasible; all loads held");
        // A bare report carries an empty alerts array + non-stale defaults.
        let v: serde_json::Value = serde_json::to_value(SolveReport::default()).unwrap();
        assert!(v["alerts"].as_array().unwrap().is_empty());
        assert_eq!(v["stale"], false);
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
