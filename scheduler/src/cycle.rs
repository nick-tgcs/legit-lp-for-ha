//! The per-tick orchestrator: registry + live HA reads -> resolved
//! LoadContracts -> WorldState -> LP plan -> executor -> SolveReport.
//! Pure assembly; every policy lives in the modules it glues.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::config::{self, DemandCfg, LoadTypeCfg, PlanningMode, RegistryConfig, ValueRef};
use crate::executor::Executor;
use crate::forecast;
use crate::ha_client::{fold_history, on_predicate_binary, on_predicate_climate, HaApi, HaState};
use crate::lp::LpPlanner;
use crate::model::*;
use crate::profile::Profiles;
use crate::reasoning;
use crate::status::{Alert, LoadReport, Severity, SolveReport, StorageReport};
use crate::time::{in_window, local_midnight, Grid};

pub struct Cycle {
    pub registry: RegistryConfig,
    pub planner: LpPlanner,
    pub dry_run: bool,
    pub profile_path: Option<std::path::PathBuf>,
    /// Runtime preview override flipped by the in-panel checkbox (POST
    /// /api/preview). OR-combined with the optional HA preview boolean to get the
    /// effective preview: when on, observe-only loads are solved for the panel but
    /// never executed.
    pub preview_override: Arc<AtomicBool>,
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

/// A diagnostic worth promoting to a Warning alert: a configuration / sensor read
/// that failed and CHANGED behaviour (held a load fail-closed, disabled a demand,
/// unmodelled a battery). Benign notes (forecast age, optional 404s) are left in the
/// `diagnostics` bag only, so the Alerts surface stays signal, not noise.
fn diag_is_actionable(d: &str) -> bool {
    let dl = d.to_lowercase();
    [
        "unreadable",
        "unresolved",
        "unavailable",
        "holding (observe-only)",
        "demand disabled",
        "unmodelled",
    ]
    .iter()
    .any(|k| dl.contains(k))
}

/// Best-effort alert scope from a diagnostic: the device id when the message names
/// one (`load 'x': …` / `storage 'x': …`), else "scheduler". The full text is kept
/// in the alert `detail` regardless, so a "scheduler" fallback still reads clearly.
fn diag_scope(d: &str) -> String {
    for tag in ["load '", "storage '"] {
        if let Some(rest) = d.strip_prefix(tag) {
            if let Some(end) = rest.find('\'') {
                return rest[..end].to_string();
            }
        }
    }
    "scheduler".to_string()
}

async fn resolve<A: HaApi>(ha: &A, vr: &ValueRef, diags: &mut Vec<String>) -> Option<f64> {
    match vr {
        ValueRef::Plain(value) | ValueRef::Literal { value } => Some(*value),
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

/// Resolve an optional minutes ref (literal or entity) to whole minutes (ceil).
/// `None` means NOT configured. A configured-but-unreadable entity-ref fails
/// CLOSED to `Some(0)` (with a diagnostic), NOT to `None`: a can-take cap that
/// silently vanished would let a discretionary load run unbounded — the opposite
/// of the "always capped" invariant.
async fn resolve_minutes<A: HaApi>(
    ha: &A,
    vr: &Option<ValueRef>,
    diags: &mut Vec<String>,
) -> Option<u32> {
    match vr {
        None => None,
        Some(v) => match resolve(ha, v, diags).await {
            Some(m) => Some(m.ceil().max(0.0) as u32),
            None => {
                diags.push(
                    "cap minutes unresolved; failing closed to 0 (no discretionary run)".into(),
                );
                Some(0)
            }
        },
    }
}

/// Resolve a clock-time ref: a literal "HH:MM", or an HA entity's state (e.g. an
/// `input_datetime` the user edits, reporting "HH:MM:SS"). None (with a
/// diagnostic) when the entity is unreadable/unavailable or its state is not a
/// time — callers fail closed rather than guess.
async fn resolve_time<A: HaApi>(
    ha: &A,
    tr: &config::TimeRef,
    diags: &mut Vec<String>,
) -> Option<chrono::NaiveTime> {
    match tr {
        config::TimeRef::Literal(s) => config::parse_clock(s).ok(),
        config::TimeRef::Entity { entity } => {
            let st = state_of(ha, entity, diags).await?;
            if st.is_unknown() {
                diags.push(format!("{entity}: unavailable; window bound unresolved"));
                return None;
            }
            match config::parse_clock(&st.state) {
                Ok(t) => Some(t),
                Err(_) => {
                    diags.push(format!(
                        "{entity}: '{}' is not a clock time; window bound unresolved",
                        st.state
                    ));
                    None
                }
            }
        }
    }
}

/// Resolve both bounds of a window; None if either bound is unresolved.
async fn resolve_window<A: HaApi>(
    ha: &A,
    w: &config::WindowCfg,
    diags: &mut Vec<String>,
) -> Option<Window> {
    Some(Window {
        start: resolve_time(ha, &w.start, diags).await?,
        end: resolve_time(ha, &w.end, diags).await?,
    })
}

/// Read + parse a forecast sensor's slot array (provider field-map applied).
/// A rejected/missing attribute yields an empty slot set (with a diagnostic);
/// an unreadable entity yields empty (the read already logged its own).
async fn forecast_slots<A: HaApi>(
    ha: &A,
    fc: &config::ForecastConfig,
    label: &str,
    diags: &mut Vec<String>,
) -> Vec<forecast::Slot> {
    match state_of(ha, &fc.entity, diags).await {
        Some(s) => match s.attr(&fc.attribute) {
            Some(attr) => match forecast::parse_slots(attr, fc.fields.as_ref()) {
                Ok(slots) => slots,
                Err(e) => {
                    diags.push(format!("{label} rejected: {e}"));
                    vec![]
                }
            },
            None => {
                diags.push(format!("{label} attribute '{}' missing", fc.attribute));
                vec![]
            }
        },
        None => vec![],
    }
}

/// Resolve a boolean ref: a literal, or an HA entity's on/off state.
async fn resolve_bool<A: HaApi>(
    ha: &A,
    br: &config::BoolRef,
    diags: &mut Vec<String>,
) -> Option<bool> {
    match br {
        config::BoolRef::Plain(b) => Some(*b),
        config::BoolRef::Entity { entity } => {
            state_of(ha, entity, diags).await.and_then(|s| s.as_on_off())
        }
    }
}

/// Resolve one configured storage direction into its executor surface: the live
/// authority boolean (fail-closed false) plus the rate (+ optional threshold)
/// service calls. `None` when the direction isn't configured.
async fn resolve_direction<A: HaApi>(
    ha: &A,
    cfg: Option<&config::StorageDirectionCfg>,
    diags: &mut Vec<String>,
) -> Option<StorageDirection> {
    let cfg = cfg?;
    let authority = state_of(ha, &cfg.authority.enabled_entity, diags)
        .await
        .and_then(|s| s.as_on_off())
        .unwrap_or(false);
    let (rd, rs) = cfg.set_rate.split().ok()?;
    let set_rate = ServiceCall {
        domain: rd,
        service: rs,
        target_entity: cfg.set_rate.target.clone(),
        data: serde_json::Value::Null,
    };
    let set_threshold = match &cfg.set_threshold {
        Some(t) => match (
            t.split(),
            resolve(ha, &t.active, diags).await,
            resolve(ha, &t.idle, diags).await,
        ) {
            (Ok((td, ts)), Some(active), Some(idle)) => Some(StorageThreshold {
                call: ServiceCall {
                    domain: td,
                    service: ts,
                    target_entity: t.target.clone(),
                    data: serde_json::Value::Null,
                },
                active,
                idle,
            }),
            _ => {
                diags.push(format!("storage threshold '{}' unresolved; rate-only", t.target));
                None
            }
        },
        None => None,
    };
    Some(StorageDirection { authority, set_rate, set_threshold })
}

/// Resolve one storage device from config + live reads into `(planner input,
/// executor control)`. Averages the per-unit SoC into a kWh charge (clamped to
/// [reserve, max]), resolves the entity-ref specs + per-direction authority +
/// control, and zeroes a configured-but-unauthorised direction's power (so the LP
/// neither plans nor actuates it). None (with a diagnostic) if SoC or capacity is
/// unreadable this cycle — the plan proceeds without this device.
async fn build_storage<A: HaApi>(
    ha: &A,
    sc: &config::StorageConfig,
    diags: &mut Vec<String>,
) -> Option<(StorageInput, StorageControl)> {
    let mut socs = Vec::new();
    for e in &sc.soc_entities {
        if let Some(v) = f64_of(ha, e, diags).await {
            socs.push(v);
        }
    }
    if socs.is_empty() {
        diags.push(format!("storage '{}': SoC unreadable; unmodelled this cycle", sc.id));
        return None;
    }
    let avg_pct = socs.iter().sum::<f64>() / socs.len() as f64;
    // Specs are literals or live entity-refs (e.g. FullChargeCapacity, the
    // backup/export sliders). Capacity is required; the rest fall back + log.
    let capacity_kwh = match resolve(ha, &sc.capacity_kwh, diags).await {
        Some(c) if c.is_finite() && c > 0.0 => c,
        _ => {
            diags.push(format!("storage '{}': capacity unreadable/invalid; unmodelled", sc.id));
            return None;
        }
    };
    let reserve_pct =
        resolve(ha, &sc.reserve_soc_pct, diags).await.unwrap_or(0.0).clamp(0.0, 100.0);
    let efficiency =
        resolve(ha, &sc.round_trip_efficiency, diags).await.unwrap_or(0.9).clamp(0.05, 1.0);
    let cycle_cost = resolve(ha, &sc.cycle_cost_aud_per_kwh, diags).await.unwrap_or(0.001).max(0.0);
    let allow_grid_charge = resolve_bool(ha, &sc.allow_grid_charge, diags).await.unwrap_or(true);
    // Fail CLOSED: max_soc_pct is the user's charge ceiling. A literal/default always
    // resolves; only an entity-ref that reads unavailable returns None — and defaulting
    // that to 100% would let the LP charge PAST the user's (currently unknown) ceiling.
    // Instead hold the ceiling at the present SoC so this cycle cannot charge any higher,
    // and surface a diagnostic. Discharge stays available — lowering SoC is always safe
    // w.r.t. a max ceiling.
    let max_soc_pct = match resolve(ha, &sc.max_soc_pct, diags).await {
        Some(v) => v.clamp(0.0, 100.0),
        None => {
            diags.push(format!(
                "storage '{}': max_soc_pct entity unreadable; holding ceiling at current SoC ({:.0}%) this cycle (no charge past unknown ceiling)",
                sc.id, avg_pct
            ));
            avg_pct.clamp(0.0, 100.0)
        }
    };

    let mut min_kwh = reserve_pct / 100.0 * capacity_kwh;
    let max_kwh = max_soc_pct / 100.0 * capacity_kwh;
    if min_kwh > max_kwh {
        min_kwh = max_kwh; // degenerate reserve >= ceiling => freeze (fail safe)
    }
    let soc_now = (avg_pct / 100.0 * capacity_kwh).clamp(min_kwh, max_kwh);
    let available = match &sc.available_entity {
        Some(e) => state_of(ha, e, diags).await.and_then(|s| s.as_on_off()).unwrap_or(false),
        None => true,
    };

    // Per-direction control + authority. A configured-but-unauthorised direction
    // is owned by the Manual/Scheduled path, so zero its power: the LP neither
    // plans nor actuates it. An UNCONFIGURED direction keeps its limit and stays
    // advisory (planned + reported, never actuated — no control surface).
    let charge = resolve_direction(ha, sc.charge.as_ref(), diags).await;
    let discharge = resolve_direction(ha, sc.discharge.as_ref(), diags).await;
    let charge_auth = charge.as_ref().map(|d| d.authority).unwrap_or(false);
    let discharge_auth = discharge.as_ref().map(|d| d.authority).unwrap_or(false);
    let rated_charge_kw = resolve(ha, &sc.max_charge_kw, diags).await.unwrap_or(0.0).max(0.0);
    let rated_discharge_kw = resolve(ha, &sc.max_discharge_kw, diags).await.unwrap_or(0.0).max(0.0);
    // Parse-time validation only guards a LITERAL max_charge_kw > 0; an entity-ref
    // that reads unavailable/zero collapses to 0 (charging off) with no error. That's
    // fail-safe (no overcharge) but silently disables a core function — surface it.
    if sc.max_charge_kw.source().is_some() && rated_charge_kw <= 0.0 {
        diags.push(format!(
            "storage '{}': max_charge_kw entity unresolved/zero; charging disabled this cycle",
            sc.id
        ));
    }
    let max_charge_kw = if sc.charge.is_some() && !charge_auth { 0.0 } else { rated_charge_kw };
    let max_discharge_kw =
        if sc.discharge.is_some() && !discharge_auth { 0.0 } else { rated_discharge_kw };

    let mut goals = Vec::new();
    for g in &sc.goals {
        match g {
            config::StorageGoalCfg::Target { soc_pct, ready_by } => {
                if let (Some(pct), Some(rb)) =
                    (resolve(ha, soc_pct, diags).await, resolve_time(ha, ready_by, diags).await)
                {
                    goals.push(StorageGoal::Target {
                        soc_kwh: (pct / 100.0 * capacity_kwh).clamp(0.0, max_kwh),
                        ready_by: rb,
                    });
                }
            }
            config::StorageGoalCfg::Price { below, up_to_soc_pct } => {
                if let Some(b) = resolve(ha, below, diags).await {
                    let up = match up_to_soc_pct {
                        Some(v) => resolve(ha, v, diags).await.unwrap_or(100.0),
                        None => 100.0,
                    };
                    goals.push(StorageGoal::Price {
                        below: b,
                        up_to_kwh: (up / 100.0 * capacity_kwh).clamp(0.0, max_kwh),
                    });
                }
            }
        }
    }
    let input = StorageInput {
        id: sc.id.clone(),
        capacity_kwh,
        soc_now_kwh: soc_now,
        min_soc_kwh: min_kwh,
        max_soc_kwh: max_kwh,
        max_charge_kw,
        max_discharge_kw,
        round_trip_efficiency: efficiency,
        allow_grid_charge,
        available,
        cycle_cost_aud_per_kwh: cycle_cost,
        goals,
    };
    let control = StorageControl { id: sc.id.clone(), charge, discharge };
    Some((input, control))
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

        // Preview (shadow-solve) toggle: when ON, observe-only loads are solved
        // for the panel too — but never executed (the executor's authority gate
        // is the backstop). Two independent inputs, OR-combined: the in-panel
        // checkbox (runtime override) and an optional HA boolean. Either on => on.
        let preview = self.preview_override.load(Ordering::Relaxed)
            || match &g.preview_entity {
                Some(e) => {
                    state_of(ha, e, &mut diags).await.and_then(|s| s.as_on_off()).unwrap_or(false)
                }
                None => false,
            };

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
            Some(fc) => forecast_slots(ha, fc, "forecast", &mut diags).await,
            None => vec![],
        };
        let prices = forecast::resample(&slots, &grid, price_now, feedin_now);
        // Feed-in (export): an optional SEPARATE forecast (some providers, e.g.
        // Amber, publish it on its own sensor). When present it gives a per-step
        // export series; otherwise the flat current value carried by `resample`.
        let feedin = match &g.pricing.feedin_forecast {
            Some(fc) => {
                let fslots = forecast_slots(ha, fc, "feed-in forecast", &mut diags).await;
                if fslots.is_empty() {
                    prices.feedin.clone()
                } else {
                    forecast::resample_feedin(&fslots, &grid, feedin_now)
                }
            }
            None => prices.feedin.clone(),
        };

        // ---- per-load resolution ----
        let mut contracts: Vec<LoadContract> = Vec::new();
        let midnight_utc = local_midnight(now).with_timezone(&Utc);
        let now_utc = now.with_timezone(&Utc);
        for l in &self.registry.loads {
            let mut authority = state_of(ha, &l.authority.enabled_entity, &mut diags)
                .await
                .and_then(|s| s.as_on_off())
                .unwrap_or(false);
            // Resolve hard run-windows (literals, or live entities like an
            // input_datetime the user edits). Fail CLOSED: if a configured window
            // can't be read, hold the load (observe-only) rather than let it run
            // unconstrained — the absence of a window must never mean "run anytime".
            let mut hard_windows = Vec::with_capacity(l.hard_rules.windows.len());
            for w in &l.hard_rules.windows {
                match resolve_window(ha, w, &mut diags).await {
                    Some(win) => hard_windows.push(win),
                    None => {
                        diags.push(format!(
                            "load '{}': run window unresolved; holding (observe-only)",
                            l.id
                        ));
                        authority = false;
                    }
                }
            }
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
            // Setpoint dynamics resolved live (literal or entity-ref); absent = 0.
            let rates = Rates {
                change_per_hour: resolve_opt(ha, &l.capability.change_per_hour, &mut diags)
                    .await
                    .unwrap_or(0.0),
                drift_per_hour: resolve_opt(ha, &l.capability.drift_per_hour, &mut diags)
                    .await
                    .unwrap_or(0.0),
                drop_per_hour: resolve_opt(ha, &l.capability.drop_per_hour, &mut diags)
                    .await
                    .unwrap_or(0.0),
            };
            let mh = patch_ambient(mh, ambient);
            let mh = patch_rates(mh, &rates);
            let ct = ct.map(|d| patch_rates(d, &rates));
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
            // Hard-rule limits resolved live (literal or entity-ref) — never hardcoded.
            // Fail CLOSED: a literal/default always resolves; only an entity-ref that
            // reads unavailable returns None. Defaulting min_run/min_off to 0 would
            // silently DROP the short-cycle lockout — an authorised compressor could be
            // stopped before its minimum run or restarted before its minimum off. So when
            // a configured timing entity can't be read, hold the load observe-only this
            // cycle (preserve whatever state it's in) rather than actuating unprotected.
            let min_run_min = match resolve(ha, &l.hard_rules.min_run_minutes, &mut diags).await {
                Some(v) => v.max(0.0),
                None => {
                    diags.push(format!(
                        "load '{}': min_run_minutes entity unreadable; holding (observe-only) to preserve short-cycle protection",
                        l.id
                    ));
                    authority = false;
                    0.0
                }
            };
            let min_off_min = match resolve(ha, &l.hard_rules.min_off_minutes, &mut diags).await {
                Some(v) => v.max(0.0),
                None => {
                    diags.push(format!(
                        "load '{}': min_off_minutes entity unreadable; holding (observe-only) to preserve short-cycle protection",
                        l.id
                    ));
                    authority = false;
                    0.0
                }
            };
            // Daily start ceiling. A configured-but-unreadable entity-ref leaves the
            // ceiling unenforced this cycle (extra wear, not unsafe) — surface it so a
            // flaky sensor isn't silent, rather than looking like "no ceiling set".
            let max_starts = match &l.hard_rules.max_starts_per_day {
                None => None,
                Some(vr) => match resolve(ha, vr, &mut diags).await {
                    Some(v) => Some(v.max(0.0) as u32),
                    None => {
                        diags.push(format!(
                            "load '{}': max_starts_per_day unresolved; ceiling not enforced this cycle",
                            l.id
                        ));
                        None
                    }
                },
            };
            // Capability + preference magnitudes resolved live (literal or entity-ref).
            let power_kw =
                resolve(ha, &l.capability.power_kw, &mut diags).await.unwrap_or(0.0).max(0.0);
            // Fail CLOSED: a load whose draw can't be read (entity-ref unavailable) or
            // is non-positive can't be modelled — a 0-kW load looks "free" to the LP and
            // sails past every price ceiling. Hold it observe-only this cycle.
            if power_kw <= 0.0 {
                diags.push(format!(
                    "load '{}': power_kw unresolved/zero; holding (observe-only)",
                    l.id
                ));
                authority = false;
            }
            let start_cost_aud = resolve(ha, &l.preferences.start_cost_aud, &mut diags)
                .await
                .unwrap_or(0.0)
                .max(0.0);
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
                power_kw,
                authority,
                hard: HardRules {
                    min_run: std::time::Duration::from_secs((min_run_min * 60.0) as u64),
                    min_off: std::time::Duration::from_secs((min_off_min * 60.0) as u64),
                    max_starts_per_day: max_starts,
                    windows: hard_windows,
                },
                must_have: mh,
                can_take: ct,
                prefs: Preferences { start_cost_aud },
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
            let baseline_kw = resolve(ha, &p.baseline_kw, &mut diags).await.unwrap_or(0.8).max(0.0);
            baseload = profiles.baseload_curve(&grid, baseline_kw, cons_now_kw);
            pv = profiles.pv_curve(&grid, |d| if d == today { t_today } else { t_tom }, pv_now_kw);
            if let Some(path) = &self.profile_path {
                if let Err(e) = profiles.save(path) {
                    diags.push(format!("profile save: {e}"));
                }
            }
        }

        // ---- site storage (optional): live SoC reads + per-direction control ----
        let mut storage = Vec::new();
        let mut storage_controls = Vec::new();
        for sc in &g.storage {
            if let Some((s, ctrl)) = build_storage(ha, sc, &mut diags).await {
                storage.push(s);
                storage_controls.push(ctrl);
            }
        }

        let world = WorldState {
            now,
            global_enabled,
            price_now,
            import: prices.import,
            feedin,
            pv,
            baseload,
            storage,
        };
        let out = self.planner.plan_with_preview(&world, &contracts, preview);
        let executor = Executor { dry_run: self.dry_run };
        let executed = executor.execute(ha, global_enabled, &contracts, &out.decisions).await;
        // Storage: map each device's slot-0 plan to a current-step command, then
        // actuate the authorised directions (mirrors the load executor; the
        // unauthorised/unconfigured ones are skipped inside execute_storage).
        let storage_decisions: Vec<StorageDecision> = out
            .storage
            .iter()
            .map(|sp| {
                let charge_watts = sp.charge_kw.first().copied().unwrap_or(0.0) * 1000.0;
                let discharge_watts = sp.discharge_kw.first().copied().unwrap_or(0.0) * 1000.0;
                StorageDecision {
                    storage_id: sp.id.clone(),
                    charge_watts,
                    discharge_watts,
                    reason: format!(
                        "lp plan (charge {charge_watts:.0}W, discharge {discharge_watts:.0}W)"
                    ),
                }
            })
            .collect();
        let storage_executed = executor
            .execute_storage(ha, global_enabled, &storage_controls, &storage_decisions)
            .await;
        if storage_executed.iter().any(|&b| b) {
            tracing::debug!(
                "storage: commanded {} device(s)",
                storage_executed.iter().filter(|&&b| b).count()
            );
        }

        // PV surplus over baseload per step — the can-take/must-have masks need it,
        // and so does the reasoning panel's step-availability breakdown.
        let surplus: Vec<f64> =
            (0..n).map(|t| (world.pv[t] - world.baseload[t]).max(0.0)).collect();

        // ---- triaged alerts: the human-facing layer above the raw `diags` bag ----
        let mut alerts: Vec<Alert> = Vec::new();
        // Critical: the solve failed and every load was held this cycle.
        if let Some(err) = &out.solver_error {
            alerts.push(Alert::new(
                Severity::Critical,
                "scheduler",
                "Could not solve",
                format!(
                    "{err}. All loads were held; nothing was changed — the last good plan is shown, stale."
                ),
            ));
        }
        // Warning: configuration / sensor reads that failed and changed behaviour
        // (held a load fail-closed, disabled a demand, unmodelled a battery). Benign
        // notes stay in `diagnostics` only.
        for d in &diags {
            if diag_is_actionable(d) {
                alerts.push(Alert::new(
                    Severity::Warning,
                    diag_scope(d),
                    "Config or sensor issue",
                    d.clone(),
                ));
            }
        }
        // Warning: a load whose must-have can't be met inside its legal/price envelope.
        for p in &out.plans {
            if p.unmet > 1.0 {
                alerts.push(Alert::new(
                    Severity::Warning,
                    p.id.0.clone(),
                    "Demand short",
                    format!(
                        "{:.0} min short — the must-have can't be met inside its window/price cap. Widen the window, raise the price cap, or lower the requirement.",
                        p.unmet
                    ),
                ));
            }
        }
        // Info: make "nothing is being controlled" explicit while in preview/dry-run.
        if preview {
            alerts.push(Alert::new(
                Severity::Info,
                "scheduler",
                "Preview",
                "Preview mode active — decisions are advisory (what the optimiser WOULD do); no devices are being controlled.",
            ));
        } else if self.dry_run {
            alerts.push(Alert::new(
                Severity::Info,
                "scheduler",
                "Dry-run",
                "Dry-run mode — the optimiser computes its calls but does not control any devices.",
            ));
        }

        SolveReport {
            at: now.to_rfc3339(),
            solver_ms: started.elapsed().as_millis() as u64,
            dry_run: self.dry_run,
            global_enabled,
            preview,
            price_now,
            pv_now: pv_now_kw,
            consumption_now: cons_now_kw,
            grid: out.grid.iter().map(|t| t.to_rfc3339()).collect(),
            // Grid-aligned forecast context for the panel (mirrors WorldState).
            price: world.import.clone(),
            feedin: world.feedin.clone(),
            pv: world.pv.clone(),
            baseload: world.baseload.clone(),
            grid_kw: out.grid_kw.clone(),
            storage: out
                .storage
                .iter()
                .map(|b| {
                    let charge_now = b.charge_kw.first().copied().unwrap_or(0.0);
                    let discharge_now = b.discharge_kw.first().copied().unwrap_or(0.0);
                    let action = if charge_now > 1e-3 {
                        "charging"
                    } else if discharge_now > 1e-3 {
                        "discharging"
                    } else {
                        "idle"
                    };
                    // A device is "actuated" when any direction is authorised
                    // (Optimiser); otherwise the plan is advisory.
                    let authority = storage_controls
                        .iter()
                        .find(|c| c.id == b.id)
                        .map(|c| {
                            c.charge.as_ref().map(|d| d.authority).unwrap_or(false)
                                || c.discharge.as_ref().map(|d| d.authority).unwrap_or(false)
                        })
                        .unwrap_or(false);
                    let reasoning = self
                        .registry
                        .global
                        .storage
                        .iter()
                        .find(|s| s.id == b.id)
                        .map(|cfg| reasoning::for_storage(cfg, b, action, authority, &grid))
                        .unwrap_or_default();
                    StorageReport {
                        id: b.id.clone(),
                        capacity_kwh: b.capacity_kwh,
                        min_soc_kwh: b.min_soc_kwh,
                        max_soc_kwh: b.max_soc_kwh,
                        soc_now_kwh: b.soc_kwh.first().copied().unwrap_or(0.0),
                        soc_kwh: b.soc_kwh.clone(),
                        charge_kw: b.charge_kw.clone(),
                        discharge_kw: b.discharge_kw.clone(),
                        action: action.into(),
                        authority,
                        target_unmet: b.target_unmet,
                        reasoning,
                    }
                })
                .collect(),
            loads: out
                .decisions
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let plan = out.plans.iter().find(|p| p.id == d.load_id);
                    let reasoning = reasoning::for_load(
                        &self.registry.loads[i],
                        &contracts[i],
                        plan,
                        &grid,
                        &world.import,
                        &surplus,
                    );
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
                        reasoning,
                    }
                })
                .collect(),
            diagnostics: diags,
            alerts,
            // This report is a fresh solve (`stale`/carry-over is applied in the run
            // loop only when a solve fails); so the on-screen plan IS this `at`.
            stale: false,
            last_solved: now.to_rfc3339(),
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
                let cap = resolve_minutes(ha, max_minutes, diags).await;
                let mut mins = match resolve_opt(ha, amount_minutes, diags).await {
                    Some(m) => m.ceil().max(0.0) as u32,
                    None => match resolve_opt(ha, amount_hours, diags).await {
                        Some(h) => config::hours_to_minutes(h),
                        None => cap.unwrap_or(0),
                    },
                };
                let window = match resolve_window(ha, window, diags).await {
                    Some(w) => w,
                    None => {
                        diags.push("runtime window unresolved; demand disabled".into());
                        mins = 0;
                        Window { start: chrono::NaiveTime::MIN, end: chrono::NaiveTime::MIN }
                    }
                };
                Demand {
                    kind: DemandKind::Runtime {
                        minutes: mins,
                        window,
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
                let window = match window {
                    Some(w) => resolve_window(ha, w, diags).await,
                    None => None,
                };
                Demand {
                    kind: DemandKind::HumidityBelow {
                        max: target.unwrap_or(f64::INFINITY),
                        observed: if target.is_some() { observed } else { None },
                        start_hysteresis: resolve_opt(ha, start_hysteresis, diags)
                            .await
                            .unwrap_or(0.0),
                        drop_per_hour: 0.0,
                        drift_per_hour: 0.0,
                        window,
                        cap_minutes: resolve_minutes(ha, max_minutes, diags).await,
                    },
                    max_price: resolve_opt(ha, max_price, diags).await,
                }
            }
            DemandCfg::TemperatureBand { target_c, band_c, window, max_minutes, max_price } => {
                let target = resolve(ha, target_c, diags).await;
                let band = resolve(ha, band_c, diags).await;
                if target.is_none() {
                    diags.push("temperature target unresolved; demand disabled".into());
                }
                if band.is_none() {
                    diags.push("temperature band unresolved; demand disabled".into());
                }
                let window = resolve_window(ha, window, diags).await;
                if window.is_none() {
                    diags.push("temperature window unresolved; demand disabled".into());
                }
                let active = target.is_some() && band.is_some() && window.is_some();
                let t = target.unwrap_or(f64::NAN);
                let b = band.unwrap_or(0.0);
                Demand {
                    kind: DemandKind::TemperatureBand {
                        min: t - b,
                        max: t + b,
                        observed: if active { observed } else { None },
                        change_per_hour: 0.0, // patched from capability below
                        drift_per_hour: 0.0,
                        ambient: None,
                        window: window.unwrap_or(Window {
                            start: chrono::NaiveTime::MIN,
                            end: chrono::NaiveTime::MIN,
                        }),
                        cap_minutes: resolve_minutes(ha, max_minutes, diags).await,
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

/// Live-resolved setpoint dynamics (°C or %RH per hour) for one load.
struct Rates {
    change_per_hour: f64,
    drift_per_hour: f64,
    drop_per_hour: f64,
}

fn patch_rates(mut d: Demand, rates: &Rates) -> Demand {
    match &mut d.kind {
        DemandKind::TemperatureBand { change_per_hour, drift_per_hour, .. } => {
            *change_per_hour = rates.change_per_hour;
            *drift_per_hour = rates.drift_per_hour;
        }
        DemandKind::HumidityBelow { drop_per_hour, drift_per_hour, .. } => {
            *drop_per_hour = rates.drop_per_hour;
            *drift_per_hour = rates.drift_per_hour;
        }
        DemandKind::Runtime { .. } => {}
    }
    d
}

#[cfg(test)]
mod tests {
    use super::{diag_is_actionable, diag_scope};

    #[test]
    fn actionable_diags_are_the_behaviour_changing_ones() {
        // Promoted to a Warning alert: config/sensor reads that changed behaviour.
        assert!(diag_is_actionable(
            "load 'hot_water': run window unresolved; holding (observe-only)"
        ));
        assert!(diag_is_actionable(
            "load 'hot_water': power_kw unresolved/zero; holding (observe-only)"
        ));
        assert!(diag_is_actionable("storage 'sonnen': SoC unreadable; unmodelled this cycle"));
        assert!(diag_is_actionable("humidity target unresolved; demand disabled"));
        // Left in the diagnostics bag only (benign / informational).
        assert!(!diag_is_actionable("forecast 4m old"));
        assert!(!diag_is_actionable("input_boolean.lp_scheduler_preview: HTTP 404 Not Found"));
    }

    #[test]
    fn diag_scope_extracts_the_device_id_or_falls_back() {
        assert_eq!(diag_scope("load 'hot_water': run window unresolved"), "hot_water");
        assert_eq!(diag_scope("storage 'sonnen01': SoC unreadable; unmodelled"), "sonnen01");
        assert_eq!(diag_scope("humidity target unresolved; demand disabled"), "scheduler");
    }
}
