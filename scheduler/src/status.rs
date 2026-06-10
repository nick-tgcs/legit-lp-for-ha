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

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct SolveReport {
    /// RFC3339 local timestamps (view-model: strings, not chrono types).
    pub at: String,
    pub solver_ms: u64,
    pub dry_run: bool,
    pub global_enabled: bool,
    pub price_now: Option<f64>,
    pub pv_now: Option<f64>,
    pub consumption_now: Option<f64>,
    pub grid: Vec<String>,
    pub loads: Vec<LoadReport>,
    pub diagnostics: Vec<String>,
}

impl SolveReport {
    pub fn log_lines(&self) -> Vec<String> {
        self.loads
            .iter()
            .map(|l| {
                let exec = if l.executed { "" } else if self.dry_run { " [dry-run]" } else { "" };
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
    fn decision_log_caps() {
        let mut log = DecisionLog::new(2);
        for i in 0..5 {
            log.push(format!("line {i}"));
        }
        assert_eq!(log.lines(), vec!["line 3", "line 4"]);
    }
}
