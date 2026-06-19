//! Gating + action: the only place service calls are issued. Defence in
//! depth — re-checks global/authority even though the planner already
//! excluded such loads. Dry-run logs the intended call and does nothing.

use crate::ha_client::HaApi;
use crate::model::{
    Action, Decision, LoadContract, StorageControl, StorageDecision, StorageDirection,
};

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

    /// Execute the current-step STORAGE commands. Same shape as `execute`: global
    /// gate, then per-DIRECTION authority, dry-run logs and does nothing. Each
    /// authorised direction sets its per-cabinet rate (watts) and, if configured,
    /// a price threshold (`active` while driving, `idle` while idle). Returns a
    /// per-device "a real call was made" flag.
    pub async fn execute_storage<A: HaApi>(
        &self,
        ha: &A,
        global_enabled: bool,
        controls: &[StorageControl],
        decisions: &[StorageDecision],
    ) -> Vec<bool> {
        let mut executed = vec![false; decisions.len()];
        if !global_enabled {
            return executed;
        }
        for (i, d) in decisions.iter().enumerate() {
            let Some(c) = controls.iter().find(|c| c.id == d.storage_id) else { continue };
            let mut made = false;
            if let Some(dir) = &c.charge {
                made |= self.drive(ha, &c.id, "charge", dir, d.charge_watts).await;
            }
            if let Some(dir) = &c.discharge {
                made |= self.drive(ha, &c.id, "discharge", dir, d.discharge_watts).await;
            }
            executed[i] = made;
        }
        executed
    }

    /// Drive ONE storage direction: authority gate, set the rate (watts) and the
    /// threshold (active while acting, idle otherwise). `true` if a real call ran.
    async fn drive<A: HaApi>(
        &self,
        ha: &A,
        id: &str,
        dir: &str,
        d: &StorageDirection,
        watts: f64,
    ) -> bool {
        if !d.authority {
            return false; // defence in depth: external path owns this direction
        }
        let watts = watts.max(0.0).round();
        let acting = watts >= 1.0; // sub-watt = idle
        let mut made = false;
        if self.dry_run {
            tracing::info!(
                "DRY-RUN storage {id} {dir}: would set rate {watts:.0} W on {}",
                d.set_rate.target_entity
            );
        } else {
            let mut call = d.set_rate.clone();
            call.data = serde_json::json!({ "value": watts });
            match ha.call_service(&call).await {
                Ok(()) => made = true,
                Err(e) => tracing::error!("storage {id} {dir}: set_rate failed: {e}"),
            }
        }
        if let Some(t) = &d.set_threshold {
            let value = if acting { t.active } else { t.idle };
            if self.dry_run {
                tracing::info!(
                    "DRY-RUN storage {id} {dir}: would set threshold {value} on {}",
                    t.call.target_entity
                );
            } else {
                let mut call = t.call.clone();
                call.data = serde_json::json!({ "value": value });
                match ha.call_service(&call).await {
                    Ok(()) => made = true,
                    Err(e) => tracing::error!("storage {id} {dir}: set_threshold failed: {e}"),
                }
            }
        }
        made
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
