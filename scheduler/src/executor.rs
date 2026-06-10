//! Gating + action: the only place service calls are issued. Defence in
//! depth — re-checks global/authority even though the planner already
//! excluded such loads. Dry-run logs the intended call and does nothing.

use crate::ha_client::HaApi;
use crate::model::{Action, Decision, LoadContract};

pub struct Executor {
    pub dry_run: bool,
}

impl Executor {
    /// Execute the current-step decisions. Returns per-decision "a real call
    /// was made" flags (always all-false in dry-run).
    pub async fn execute<A: HaApi>(
        &self,
        ha: &A,
        global_enabled: bool,
        loads: &[LoadContract],
        decisions: &[Decision],
    ) -> Vec<bool> {
        let mut executed = vec![false; decisions.len()];
        if !global_enabled {
            return executed;
        }
        for (i, d) in decisions.iter().enumerate() {
            let Some(c) = loads.iter().find(|c| c.id == d.load_id) else { continue };
            if !c.authority {
                continue; // defence in depth: never trust the planner blindly
            }
            let call = match d.action {
                Action::Start => &c.control.start,
                Action::Stop => &c.control.stop,
                Action::NoChange => continue,
            };
            if self.dry_run {
                tracing::info!(
                    "DRY-RUN {}: would call {}.{} on {}",
                    c.id.0,
                    call.domain,
                    call.service,
                    call.target_entity
                );
                continue;
            }
            match ha.call_service(call).await {
                Ok(()) => executed[i] = true,
                Err(e) => tracing::error!("{}: service call failed: {e}", c.id.0),
            }
        }
        executed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha_client::RecordingHa;
    use crate::model::LoadId;
    use crate::testkit::*;

    fn start_decision(id: &str) -> Decision {
        Decision { load_id: LoadId(id.into()), action: Action::Start, reason: "t".into() }
    }

    #[tokio::test]
    async fn dry_run_makes_zero_calls() {
        let ha = RecordingHa::default();
        let ex = Executor { dry_run: true };
        let done =
            ex.execute(&ha, true, &[runtime_contract()], &[start_decision("hot_water")]).await;
        assert_eq!(done, vec![false]);
        assert!(ha.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn happy_path_issues_exactly_the_contract_start_call() {
        let ha = RecordingHa::default();
        let ex = Executor { dry_run: false };
        let c = runtime_contract();
        let done =
            ex.execute(&ha, true, std::slice::from_ref(&c), &[start_decision("hot_water")]).await;
        assert_eq!(done, vec![true]);
        let calls = ha.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], c.control.start);
    }

    #[tokio::test]
    async fn global_off_and_authority_off_block_calls() {
        let ha = RecordingHa::default();
        let ex = Executor { dry_run: false };
        let c = runtime_contract();
        ex.execute(&ha, false, std::slice::from_ref(&c), &[start_decision("hot_water")]).await;
        let mut c2 = c.clone();
        c2.authority = false;
        ex.execute(&ha, true, &[c2], &[start_decision("hot_water")]).await;
        assert!(ha.calls.lock().unwrap().is_empty());
    }
}
