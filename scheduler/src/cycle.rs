//! The per-tick orchestrator: registry + live HA reads -> resolved
//! LoadContracts -> WorldState -> LP plan -> executor -> SolveReport.
//! Pure assembly; every policy lives in the modules it glues.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::config::{self, DemandCfg, LoadTypeCfg, PlanningMode, RegistryConfig, ValueRef};
use crate::executor::Executor;
use crate::forecast;
use crate::ha_client::{fold_history, on_predicate_binary, on_predicate_climate, HaApi, HaState};
use crate::lp::LpPlanner;
use crate::model::*;
use crate::profile::Profiles;
use crate::status::{LoadReport, SolveReport};
use crate::time::{in_window, local_midnight, Grid};

pub struct Cycle {
    pub registry: RegistryConfig,
    pub planner: LpPlanner,
    pub dry_run: bool,
    pub profile_path: Option<std::path::PathBuf>,
}

async fn state_of<A: HaApi>(ha: &A, entity: &str, diags: &mut Vec<String>) -> Option<HaState> {
    match ha.get_state(entity).await {
        Ok(s) => Some(s),
        Err(e) => {
            diags.push(format!("{entity}: {e}"));
            None
        }
    }
}

async fn f64_of<A: HaApi>(ha: &A, entity: &str, diags: &mut Vec<String>) -> Option<f64> {
    state_of(ha, entity, diags).await.and_then(|s| s.as_f64())
}

async fn resolve<A: HaApi>(ha: &A, vr: &ValueRef, diags: &mut Vec<String>) -> Option<f64> {
    match vr {
        ValueRef::Literal { value } => Some(*value),
        ValueRef::Entity { entity } => f64_of(ha, entity, diags).await,
    }
}

async fn resolve_opt<A: HaApi>(
    ha: &A,
    vr: &Option<ValueRef>,
    diags: &mut Vec<String>,
) -> Option<f64> {
    match vr {
        Some(v) => resolve(ha, v, diags).await,
        None => None,
    }
}

/// Absolute range of the CURRENT instance of a daily window, up to `now`
/// (None when now is outside the window). Overnight windows reach back to
/// yesterday's start.
fn current_window_range(w: Window, now: DateTime<Tz>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    if !in_window(now.time(), &w) {
        return None;
    }
    let midnight = local_midnight(now);
    let start_today = midnight + (w.start - chrono::NaiveTime::MIN);
    let start = if now.time() >= w.start || w.start <= w.end {
        start_today
    } else {
        start_today - chrono::Duration::days(1) // overnight window, started yesterday
    };
    Some((start.with_timezone(&Utc), now.with_timezone(&Utc)))
}

impl Cycle {
    pub async fn run<A: HaApi>(
        &self,
        ha: &A,
        profiles: &mut Profiles,
        now: DateTime<Tz>,
    ) -> SolveReport {
        let started = std::time::Instant::now();
        let mut diags: Vec<String> = Vec::new();
        let g = &self.registry.global;

        let global_enabled = state_of(ha, &g.enabled_entity, &mut diags)
            .await
            .and_then(|s| s.as_on_off())
            .unwrap_or(false);

        let grid = Grid::build(now, g.planning.grid_minutes, g.planning.horizon_hours)
            .expect("validated config");
        let n = grid.steps.len();

        // ---- pricing (provider-neutral) ----
        let price_now = f64_of(ha, &g.pricing.import_entity, &mut diags).await;
        let feedin_now = match &g.pricing.feedin_entity {
            Some(e) => f64_of(ha, e, &mut diags).await,
            None => None,
        };
        let slots = match &g.pricing.forecast {
            Some(fc) => match state_of(ha, &fc.entity, &mut diags).await {
                Some(s) => match s.attr(&fc.attribute) {
                    Some(attr) => match forecast::parse_slots(attr, fc.fields.as_ref()) {
                        Ok(slots) => slots,
                        Err(e) => {
                            diags.push(format!("forecast rejected: {e}"));
                            vec![]
                        }
                    },
                    None => {
                        diags.push(format!("forecast attribute '{}' missing", fc.attribute));
                        vec![]
                    }
                },
                None => vec![],
            },
            None => vec![],
        };
        let prices = forecast::resample(&slots, &grid, price_now, feedin_now);

        // ---- per-load resolution ----
        let mut contracts: Vec<LoadContract> = Vec::new();
        let midnight_utc = local_midnight(now).with_timezone(&Utc);
        let now_utc = now.with_timezone(&Utc);
        for l in &self.registry.loads {
            let authority = state_of(ha, &l.authority.enabled_entity, &mut diags)
                .await
                .and_then(|s| s.as_on_off())
                .unwrap_or(false);
            let is_climate = matches!(l.load_type, LoadTypeCfg::Aircon);
            let pred = if is_climate { on_predicate_climate } else { on_predicate_binary };
            let (running, fold) =
                match ha.get_history(&l.state.running_entity, midnight_utc, now_utc).await {
                    Ok(rows) => {
                        let f = fold_history(&rows, midnight_utc, now_utc, pred);
                        (f.final_on, Some(f))
                    }
                    Err(e) => {
                        diags.push(format!("{}: history: {e}", l.state.running_entity));
                        (None, None)
                    }
                };
            let observed = match &l.state.observed_entity {
                Some(e) => f64_of(ha, e, &mut diags).await,
                None => None,
            };
            let mh = self.demand(ha, &l.must_have, observed, &fold, now, &mut diags).await;
            let ct = match &l.can_take {
                Some(d) => Some(self.demand(ha, d, observed, &fold, now, &mut diags).await),
                None => None,
            };
            let ambient = match &l.capability.ambient_entity {
                Some(e) => f64_of(ha, e, &mut diags).await,
                None => None,
            };
            let mh = patch_ambient(mh, ambient);
            let mh = patch_rates(mh, &l.capability);
            let ct = ct.map(|d| patch_rates(d, &l.capability));
            let (sd, ss) = (l.control.start.split().unwrap(), l.control.stop.split().unwrap());
            let obs = Observation {
                running,
                starts_today: fold.as_ref().map(|f| f.starts()).unwrap_or(0),
                runtime_in_mh_window: std::time::Duration::from_secs(
                    demand_window_of(&mh)
                        .and_then(|w| current_window_range(w, now))
                        .map(|r| fold.as_ref().map(|f| f.on_secs_within(&[r])).unwrap_or(0))
                        .unwrap_or(0),
                ),
                runtime_in_ct_window: std::time::Duration::from_secs(
                    ct.as_ref()
                        .and_then(demand_window_of)
                        .and_then(|w| current_window_range(w, now))
                        .map(|r| fold.as_ref().map(|f| f.on_secs_within(&[r])).unwrap_or(0))
                        .unwrap_or(0),
                ),
                current_stretch: fold
                    .as_ref()
                    .map(|f| f.current_stretch)
                    .unwrap_or(std::time::Duration::ZERO),
            };
            let mh = patch_completed(mh, obs.runtime_in_mh_window);
            contracts.push(LoadContract {
                id: LoadId(l.id.clone()),
                load_type: match l.load_type {
                    LoadTypeCfg::HotWater => LoadType::HotWater,
                    LoadTypeCfg::Dehumidifier => LoadType::Dehumidifier,
                    LoadTypeCfg::Aircon => LoadType::Aircon,
                },
                planning: match l.planning {
                    PlanningMode::Runtime => Planning::Runtime,
                    PlanningMode::Predictive => Planning::Predictive,
                    PlanningMode::Immediate => Planning::Immediate,
                },
                power_kw: l.capability.power_kw,
                authority,
                hard: HardRules {
                    min_run: std::time::Duration::from_secs(
                        u64::from(l.hard_rules.min_run_minutes) * 60,
                    ),
                    min_off: std::time::Duration::from_secs(
                        u64::from(l.hard_rules.min_off_minutes) * 60,
                    ),
                    max_starts_per_day: l.hard_rules.max_starts_per_day,
                    windows: l.hard_rules.windows.iter().map(|w| w.parse().unwrap()).collect(),
                },
                must_have: mh,
                can_take: ct,
                prefs: Preferences { start_cost_aud: l.preferences.start_cost_aud },
                obs,
                control: Control {
                    start: ServiceCall {
                        domain: sd.0,
                        service: sd.1,
                        target_entity: l.control.start.target.clone(),
                        data: l.control.start.data.clone().unwrap_or(serde_json::Value::Null),
                    },
                    stop: ServiceCall {
                        domain: ss.0,
                        service: ss.1,
                        target_entity: l.control.stop.target.clone(),
                        data: l.control.stop.data.clone().unwrap_or(serde_json::Value::Null),
                    },
                },
            });
        }

        // ---- site power + learned profiles ----
        let (mut pv_now_kw, mut cons_now_kw) = (None, None);
        let mut baseload = vec![0.8; n];
        let mut pv = vec![0.0; n];
        if let Some(p) = &g.power {
            cons_now_kw = f64_of(ha, &p.consumption_entity, &mut diags).await.map(|w| w / 1000.0);
            pv_now_kw = f64_of(ha, &p.pv_entity, &mut diags).await.map(|w| w / 1000.0);
            if let Some(c_kw) = cons_now_kw {
                let managed: f64 = contracts
                    .iter()
                    .filter(|c| c.obs.running == Some(true))
                    .map(|c| c.power_kw)
                    .sum();
                profiles.sample_baseload(now, (c_kw - managed).max(0.0));
            }
            if let Some(p_kw) = pv_now_kw {
                profiles.sample_pv(now, p_kw);
            }
            let today = now.date_naive();
            let (t_today, t_tom) = match &p.pv_forecast {
                Some(f) => (
                    f64_of(ha, &f.today_entity, &mut diags).await,
                    f64_of(ha, &f.tomorrow_entity, &mut diags).await,
                ),
                None => (None, None),
            };
            baseload = profiles.baseload_curve(&grid, p.baseline_kw, cons_now_kw);
            pv = profiles.pv_curve(&grid, |d| if d == today { t_today } else { t_tom }, pv_now_kw);
            if let Some(path) = &self.profile_path {
                if let Err(e) = profiles.save(path) {
                    diags.push(format!("profile save: {e}"));
                }
            }
        }

        let world = WorldState {
            now,
            global_enabled,
            price_now,
            import: prices.import,
            feedin: prices.feedin,
            pv,
            baseload,
        };
        let out = self.planner.plan(&world, &contracts);
        let executed = Executor { dry_run: self.dry_run }
            .execute(ha, global_enabled, &contracts, &out.decisions)
            .await;

        SolveReport {
            at: now.to_rfc3339(),
            solver_ms: started.elapsed().as_millis() as u64,
            dry_run: self.dry_run,
            global_enabled,
            price_now,
            pv_now: pv_now_kw,
            consumption_now: cons_now_kw,
            grid: out.grid.iter().map(|t| t.to_rfc3339()).collect(),
            loads: out
                .decisions
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let plan = out.plans.iter().find(|p| p.id == d.load_id);
                    LoadReport {
                        id: d.load_id.0.clone(),
                        planning: format!("{:?}", contracts[i].planning).to_lowercase(),
                        authority: contracts[i].authority,
                        running: contracts[i].obs.running,
                        action: format!("{:?}", d.action),
                        reason: d.reason.clone(),
                        unmet: plan.map(|p| p.unmet).unwrap_or(0.0),
                        executed: executed[i],
                        on: plan.map(|p| p.on.clone()).unwrap_or_default(),
                        ct: plan.map(|p| p.ct.clone()).unwrap_or_default(),
                    }
                })
                .collect(),
            diagnostics: diags,
        }
    }

    async fn demand<A: HaApi>(
        &self,
        ha: &A,
        d: &DemandCfg,
        observed: Option<f64>,
        _fold: &Option<crate::ha_client::Fold>,
        _now: DateTime<Tz>,
        diags: &mut Vec<String>,
    ) -> Demand {
        match d {
            DemandCfg::Runtime { amount_hours, amount_minutes, max_minutes, window, max_price } => {
                let mins = match resolve_opt(ha, amount_minutes, diags).await {
                    Some(m) => m.ceil().max(0.0) as u32,
                    None => match resolve_opt(ha, amount_hours, diags).await {
                        Some(h) => config::hours_to_minutes(h),
                        None => max_minutes.unwrap_or(0),
                    },
                };
                Demand {
                    kind: DemandKind::Runtime {
                        minutes: mins,
                        window: window.parse().unwrap(),
                        completed_minutes: 0, // patched after the fold
                    },
                    max_price: resolve_opt(ha, max_price, diags).await,
                }
            }
            DemandCfg::HumidityBelow {
                max_percent,
                target_percent,
                start_hysteresis,
                window,
                max_minutes,
                max_price,
            } => {
                let target = match resolve_opt(ha, max_percent, diags).await {
                    Some(v) => Some(v),
                    None => resolve_opt(ha, target_percent, diags).await,
                };
                if target.is_none() {
                    diags.push("humidity target unresolved; demand disabled".into());
                }
                Demand {
                    kind: DemandKind::HumidityBelow {
                        max: target.unwrap_or(f64::INFINITY),
                        observed: if target.is_some() { observed } else { None },
                        start_hysteresis: resolve_opt(ha, start_hysteresis, diags)
                            .await
                            .unwrap_or(0.0),
                        drop_per_hour: 0.0,
                        drift_per_hour: 0.0,
                        window: window.as_ref().map(|w| w.parse().unwrap()),
                        cap_minutes: *max_minutes,
                    },
                    max_price: resolve_opt(ha, max_price, diags).await,
                }
            }
            DemandCfg::TemperatureBand { target_c, band_c, window, max_minutes, max_price } => {
                let target = resolve(ha, target_c, diags).await;
                if target.is_none() {
                    diags.push("temperature target unresolved; demand disabled".into());
                }
                let t = target.unwrap_or(f64::NAN);
                Demand {
                    kind: DemandKind::TemperatureBand {
                        min: t - band_c,
                        max: t + band_c,
                        observed: if target.is_some() { observed } else { None },
                        change_per_hour: 0.0, // patched from capability below
                        drift_per_hour: 0.0,
                        ambient: None,
                        window: window.parse().unwrap(),
                        cap_minutes: *max_minutes,
                    },
                    max_price: resolve_opt(ha, max_price, diags).await,
                }
            }
        }
    }
}

fn demand_window_of(d: &Demand) -> Option<Window> {
    match &d.kind {
        DemandKind::Runtime { window, .. } | DemandKind::TemperatureBand { window, .. } => {
            Some(*window)
        }
        DemandKind::HumidityBelow { window, .. } => *window,
    }
}

fn patch_completed(mut d: Demand, done: std::time::Duration) -> Demand {
    if let DemandKind::Runtime { completed_minutes, .. } = &mut d.kind {
        *completed_minutes = (done.as_secs() / 60) as u32;
    }
    d
}

fn patch_ambient(mut d: Demand, amb: Option<f64>) -> Demand {
    if let DemandKind::TemperatureBand { ambient, .. } = &mut d.kind {
        *ambient = amb;
    }
    d
}

fn patch_rates(mut d: Demand, cap: &crate::config::CapabilityCfg) -> Demand {
    match &mut d.kind {
        DemandKind::TemperatureBand { change_per_hour, drift_per_hour, .. } => {
            *change_per_hour = cap.change_per_hour.unwrap_or(0.0);
            *drift_per_hour = cap.drift_per_hour.unwrap_or(0.0);
        }
        DemandKind::HumidityBelow { drop_per_hour, drift_per_hour, .. } => {
            *drop_per_hour = cap.drop_per_hour.unwrap_or(0.0);
            *drift_per_hour = cap.drift_per_hour.unwrap_or(0.0);
        }
        DemandKind::Runtime { .. } => {}
    }
    d
}
