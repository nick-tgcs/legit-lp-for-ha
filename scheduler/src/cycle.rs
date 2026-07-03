//! The per-tick orchestrator: registry + live HA reads -> resolved
//! LoadContracts -> WorldState -> LP plan -> executor -> SolveReport.
//! Pure assembly; every policy lives in the modules it glues.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::config::{self, DemandCfg, PlanningMode, RegistryConfig, ValueRef};
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

/// Whether a load's running-state entity should be read as a thermostat (climate)
/// rather than a plain on/off device. Derived from the HA domain of the entity id
/// (`climate.*`), so the engine works for ANY device kind without a closed
/// device-type enum: a comfort load wired to a `climate.*` entity reads its
/// hvac-mode ("running" = not off); one wired to a `switch`/`binary_sensor`/
/// `input_boolean` reads on/off.
pub fn reads_as_climate(running_entity: &str) -> bool {
    running_entity.split_once('.').map(|(domain, _)| domain == "climate").unwrap_or(false)
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

/// Build the triaged alert list (the layer above the raw `diags` bag) from a cycle's
/// outcome. Pure, so it is unit-tested directly. Precedence: Critical (the solve
/// failed) > Warning (a fail-closed config/sensor read, or an unmet must-have) > Info
/// (preview / dry-run mode — "nothing is being controlled").
fn derive_alerts(
    solver_error: &Option<String>,
    diags: &[String],
    plans: &[crate::lp::LoadPlan],
    preview: bool,
    dry_run: bool,
) -> Vec<Alert> {
    let mut alerts: Vec<Alert> = Vec::new();
    if let Some(err) = solver_error {
        alerts.push(Alert::new(
            Severity::Critical,
            "scheduler",
            "Could not solve",
            format!(
                "{err}. All loads were held; nothing was changed — the last good plan is shown, stale."
            ),
        ));
    }
    // Config / sensor reads that failed and changed behaviour (held a load fail-closed,
    // disabled a demand, unmodelled a battery). Benign notes stay in `diagnostics` only.
    for d in diags {
        if diag_is_actionable(d) {
            alerts.push(Alert::new(
                Severity::Warning,
                diag_scope(d),
                "Config or sensor issue",
                d.clone(),
            ));
        }
    }
    // A load whose must-have can't be met inside its legal/price envelope.
    for p in plans {
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
    // Make "nothing is being controlled" explicit while in preview / dry-run.
    if preview {
        alerts.push(Alert::new(
            Severity::Info,
            "scheduler",
            "Preview",
            "Preview mode active — decisions are advisory (what the optimiser WOULD do); no devices are being controlled.",
        ));
    } else if dry_run {
        alerts.push(Alert::new(
            Severity::Info,
            "scheduler",
            "Dry-run",
            "Dry-run mode — the optimiser computes its calls but does not control any devices.",
        ));
    }
    alerts
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

/// Decide a single grid-import cap from its live-resolved parts. Pure (no HA), so
/// the fail-loud-but-safe policy is unit-testable without a mock cycle. `Ok` = a
/// usable cap; `Err(diagnostic)` = skip it this cycle, never invent one: either a
/// part didn't resolve, or a magnitude came back invalid — negative OR non-finite.
/// A miswired entity can report `NaN`/`inf` (which `HaState::as_f64` parses into an
/// `f64`); accepting it would poison the LP objective/constraints into a solver
/// error, and clamping a negative would forge a cap the user never wrote. Both are
/// rejected here, mirroring the parse-time literal check in `config::validate`.
fn grid_import_cap_from_resolved(
    idx: usize,
    window: Option<Window>,
    max_kw: Option<f64>,
    penalty: Option<f64>,
) -> Result<GridImportCapInput, String> {
    let valid = |v: f64| v.is_finite() && v >= 0.0;
    match (window, max_kw, penalty) {
        (Some(window), Some(max_kw), Some(penalty)) => {
            if valid(max_kw) && valid(penalty) {
                Ok(GridImportCapInput { window, max_kw, penalty_aud_per_kwh: penalty })
            } else {
                Err(format!(
                    "grid_import_caps[{idx}] resolved to an invalid magnitude (max_kw={max_kw}, penalty={penalty}); expected finite and >= 0, skipped this cycle"
                ))
            }
        }
        _ => Err(format!(
            "grid_import_caps[{idx}] unresolved (window/max_kw/penalty); peak grid-avoidance skipped this cycle"
        )),
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
/// [reserve, max]) and resolves the entity-ref specs + per-direction authority +
/// control. Power limits are RATED — authority does NOT gate planning, so the panel
/// shows the full advisory trajectory (e.g. charging to the peak-ready target) even
/// in Manual/Scheduled mode. Actuation is gated separately, on a zeroed copy of this
/// model (see `gate_storage_for_actuation`) plus `Executor::drive`. None (with a
/// diagnostic) if SoC or capacity is unreadable this cycle — the plan proceeds
/// without this device.
/// One storage device resolved for a cycle. `input` is `None` when the device could
/// not be modelled this cycle (an operational spec read unavailable): it is then
/// EXCLUDED from the LP, but its `control` is still returned so the executor drives it
/// IDLE rather than leaving HA on a prior cycle's active charge/discharge command.
struct StorageBuild {
    input: Option<StorageInput>,
    control: StorageControl,
}

async fn build_storage<A: HaApi>(
    ha: &A,
    sc: &config::StorageConfig,
    diags: &mut Vec<String>,
) -> Option<StorageBuild> {
    // Resolve the control surface FIRST: it does not depend on the LP specs, and we need
    // it to drive the device IDLE on any cycle we cannot model. resolve_direction also
    // reads each direction's authority, reused below for the LP power limits.
    let charge = resolve_direction(ha, sc.charge.as_ref(), diags).await;
    let discharge = resolve_direction(ha, sc.discharge.as_ref(), diags).await;
    let control = StorageControl { id: sc.id.clone(), charge, discharge };

    let mut socs = Vec::new();
    for e in &sc.soc_entities {
        if let Some(v) = f64_of(ha, e, diags).await {
            socs.push(v);
        }
    }
    if socs.is_empty() {
        diags.push(format!("storage '{}': SoC unreadable; unmodelled this cycle", sc.id));
        return Some(StorageBuild { input: None, control });
    }
    let avg_pct = socs.iter().sum::<f64>() / socs.len() as f64;
    // Specs are literals or live entity-refs (e.g. FullChargeCapacity, the
    // backup/export sliders). Capacity is required; the rest fall back + log.
    let capacity_kwh = match resolve(ha, &sc.capacity_kwh, diags).await {
        Some(c) if c.is_finite() && c > 0.0 => c,
        _ => {
            diags.push(format!("storage '{}': capacity unreadable/invalid; unmodelled", sc.id));
            return Some(StorageBuild { input: None, control });
        }
    };
    // No-hardcoding rule: an unreadable operational entity-ref NEVER falls back to an
    // invented number. It fails LOUD (actionable diagnostic → Warning alert) + SAFE
    // (freeze the floor / drop the device / hold grid-charge off) for this cycle.
    let reserve_pct = match resolve(ha, &sc.reserve_soc_pct, diags).await {
        Some(v) => v.clamp(0.0, 100.0),
        None => {
            diags.push(format!(
                "storage '{}': reserve_soc_pct entity unreadable; freezing discharge floor at current SoC ({:.0}%) this cycle (no discharge below an unknown floor)",
                sc.id, avg_pct
            ));
            avg_pct.clamp(0.0, 100.0)
        }
    };
    let efficiency = match resolve(ha, &sc.round_trip_efficiency, diags).await {
        Some(v) => v.clamp(0.05, 1.0),
        None => {
            diags.push(format!(
                "storage '{}': round_trip_efficiency entity unreadable; unmodelled this cycle",
                sc.id
            ));
            return Some(StorageBuild { input: None, control });
        }
    };
    let cycle_cost = match resolve(ha, &sc.cycle_cost_aud_per_kwh, diags).await {
        Some(v) => v.max(0.0),
        None => {
            diags.push(format!(
                "storage '{}': cycle_cost_aud_per_kwh entity unreadable; unmodelled this cycle",
                sc.id
            ));
            return Some(StorageBuild { input: None, control });
        }
    };
    let allow_grid_charge = match resolve_bool(ha, &sc.allow_grid_charge, diags).await {
        Some(b) => b,
        None => {
            diags.push(format!(
                "storage '{}': allow_grid_charge entity unreadable; holding grid-charge OFF this cycle",
                sc.id
            ));
            false
        }
    };
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

    // Power limits at RATED capacity, regardless of per-direction authority. Planning
    // is decoupled from actuation: the panel plans (and shows) the full intended
    // trajectory even when a direction is unauthorised (Manual/Scheduled). The caller
    // derives a separate ZEROED model for the command it commits — see
    // `gate_storage_for_actuation` — so a slot-0 charge can never lean on a future
    // discharge the executor would skip; `Executor::drive` + dry-run gate again.
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
    let max_charge_kw = rated_charge_kw;
    let max_discharge_kw = rated_discharge_kw;

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
    // Load share: the fraction of its BANK's own charge/discharge this cabinet
    // carries (paralleled cabinets load-share both directions) — NOT a fraction of
    // house load. An entity-ref may map it to a live consumption-share sensor;
    // unreadable → None → the LP falls back to an equal split (safe default, not a
    // fabricated rate). An explicit 0.0 parks this cabinet; an all-0.0 bank is idle.
    let load_share = match &sc.load_share {
        Some(vr) => resolve(ha, vr, diags).await.map(|v| v.clamp(0.0, 1.0)),
        None => None,
    };
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
        // Structural grouping (a literal id): devices sharing a bank are
        // co-driven as one unit in the LP. No live value to resolve.
        bank: sc.bank.clone(),
        load_share,
    };
    Some(StorageBuild { input: Some(input), control })
}

/// Derive the ACTUATION storage models from the RATED (advisory) ones by zeroing the
/// power of any direction that is NOT both configured AND authorised. The panel plans at
/// rated power, but the command we actually commit is read from a solve over THESE models:
/// removing a leg the executor will skip means a slot-0 charge can never be justified by a
/// future discharge that won't happen (the Manual/Scheduled "charge now, never discharge"
/// trap), and an UNCONFIGURED (advisory) battery can't tilt the committed load/grid plan
/// with moves it will never make. A device whose every direction is configured AND
/// authorised is returned unchanged, so the gated set equals the rated set and no second
/// solve is needed.
fn gate_storage_for_actuation(
    rated: &[StorageInput],
    controls: &[StorageControl],
) -> Vec<StorageInput> {
    rated
        .iter()
        .map(|s| {
            let ctrl = controls.iter().find(|c| c.id == s.id);
            // A direction is ACTUATABLE only when it is CONFIGURED (has a control surface) AND
            // authorised; otherwise the executor will never drive it, so it must be zeroed for the
            // committed solve. This blocks BOTH an unauthorised configured direction AND an
            // UNCONFIGURED one (e.g. a wizard-added advisory battery with no charge/discharge
            // block) — else its rated power would still shift the committed load/grid plan, making
            // the scheduler start loads around battery moves that can never happen.
            let charge_blocked = !ctrl.and_then(|c| c.charge.as_ref()).is_some_and(|d| d.authority);
            let discharge_blocked =
                !ctrl.and_then(|c| c.discharge.as_ref()).is_some_and(|d| d.authority);
            let mut g = s.clone();
            if charge_blocked {
                g.max_charge_kw = 0.0;
            }
            if discharge_blocked {
                g.max_discharge_kw = 0.0;
            }
            g
        })
        .collect()
}

/// Is the SHOWN (advisory) current-step storage command actually committed this cycle?
/// The panel plots the advisory (rated-power) plan, but the executor writes the gated
/// command — so the pill may show a direction/rate that is NOT what gets actuated. It is
/// "live · executes" only when (a) the shown charge AND discharge equal the committed
/// (gated) values to the watt, and (b) the active direction is one the executor will
/// drive (idle => any authorised direction). Otherwise it is advisory. This keeps an
/// advisory rate from ever being labelled live when a different gated rate is written.
fn storage_action_actuated(
    action: &str,
    shown_charge_kw: f64,
    shown_discharge_kw: f64,
    committed_charge_kw: f64,
    committed_discharge_kw: f64,
    charge_authority: bool,
    discharge_authority: bool,
) -> bool {
    let matches_committed = (shown_charge_kw - committed_charge_kw).abs() < 1e-3
        && (shown_discharge_kw - committed_discharge_kw).abs() < 1e-3;
    let direction_drivable = match action {
        "charging" => charge_authority,
        "discharging" => discharge_authority,
        _ => charge_authority || discharge_authority, // idle: live only if something is driveable
    };
    matches_committed && direction_drivable
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
            // Read the running entity per its HA domain: a `climate.*` entity reports
            // an hvac-mode state ("off"/"cool"/…) so "running" = state != off; anything
            // else (switch/binary_sensor/input_boolean) reads as on/off. Derived from
            // the entity id, not a closed device-type enum — so any device kind works.
            let is_climate = reads_as_climate(&l.state.running_entity);
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
            let mh = self.demand(ha, &l.must_have, observed, false, &mut diags).await;
            let ct = match &l.can_take {
                // can_take prefers the tighter target (target_value/target_percent) over the
                // must-have limit, so an optional precondition is held to the stricter setpoint.
                Some(d) => Some(self.demand(ha, d, observed, true, &mut diags).await),
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
            // A `program` runs as ONE contiguous block: force the min-run to the
            // block length (so the run can't fragment) and cap it at a single start.
            // The whole block then lands under any price cap or not at all (a fresh,
            // unlocked start is gated step-by-step), which is the all-or-nothing
            // program semantics. Overrides the hard-rule values resolved above for
            // this kind only.
            let (min_run_min, max_starts) =
                if matches!(l.must_have, config::DemandCfg::Program { .. }) {
                    let block = match &mh.kind {
                        DemandKind::Runtime { minutes, .. } => f64::from(*minutes),
                        _ => min_run_min,
                    };
                    (min_run_min.max(block), Some(1))
                } else {
                    (min_run_min, max_starts)
                };
            contracts.push(LoadContract {
                id: LoadId(l.id.clone()),
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
        // No power config => no baseload signal => assume none (0), never an invented 0.8.
        let mut baseload = vec![0.0; n];
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
            // Required field. If its entity-ref is unreadable we must NOT invent a
            // baseload — but nor may we treat the gap as FREE PV headroom: with an
            // understated baseload the LP's surplus (pv − baseload) marks steps `sun_pays`
            // and opens price-capped must/can-take ABOVE their ceiling, importing at peak.
            // So flag it and, once pv is known, assume the house self-consumes its PV this
            // cycle (baseload = pv ⇒ zero surplus, balance-neutral) until the entity
            // returns. resolve() on a literal never fails; only an entity-ref can.
            let mut baseline_known = true;
            let baseline_kw = match resolve(ha, &p.baseline_kw, &mut diags).await {
                Some(v) => v.max(0.0),
                None => {
                    baseline_known = false;
                    diags.push(
                        "power.baseline_kw entity unreadable; assuming PV self-consumed (no surplus credit) this cycle".into(),
                    );
                    0.0
                }
            };
            baseload = profiles.baseload_curve(&grid, baseline_kw, cons_now_kw);
            pv = profiles.pv_curve(&grid, |d| if d == today { t_today } else { t_tom }, pv_now_kw);
            if !baseline_known {
                // Unknown baseload ⇒ no free headroom. baseload = pv makes surplus
                // (pv − baseload) zero and the site balance (baseload − pv) neutral, so the
                // LP never opens price-capped loads on phantom solar this cycle.
                baseload.clone_from(&pv);
            }
            if let Some(path) = &self.profile_path {
                if let Err(e) = profiles.save(path) {
                    diags.push(format!("profile save: {e}"));
                }
            }
        }

        // ---- site storage (optional): live SoC reads + per-direction control ----
        // `storage` feeds the LP; `storage_controls` holds EVERY device's control surface
        // (modelled and not). A device we couldn't model (`input: None`) is excluded from
        // the LP but recorded in `idle_storage_ids` so we still command it idle below.
        let mut storage = Vec::new();
        let mut storage_controls = Vec::new();
        let mut idle_storage_ids = Vec::new();
        for sc in &g.storage {
            if let Some(b) = build_storage(ha, sc, &mut diags).await {
                match b.input {
                    Some(s) => storage.push(s),
                    None => idle_storage_ids.push(b.control.id.clone()),
                }
                storage_controls.push(b.control);
            }
        }

        // Plan storage at RATED power for the panel (advisory), but ACTUATE from a model
        // with every unauthorised direction zeroed. The PRIMARY solve (`out`) uses the
        // gated model — it drives the load actuation, the grid lane, the load report AND
        // the storage commands, so a committed slot-0 charge can never lean on a future
        // leg the executor would skip (identical safety to the authority-gated v0.1.9).
        let storage_gated = gate_storage_for_actuation(&storage, &storage_controls);
        let needs_advisory = storage_gated != storage;

        // ---- grid-import caps (the "no grid during peak" control) ----
        // Resolve each windowed cap live. Fail LOUD but SAFE: an unresolved cap is
        // SKIPPED this cycle (with a diagnostic), never invented — it is a cost
        // preference, not a safety gate, so skipping only forgoes peak grid-avoidance;
        // it can never make a device act. (Failing "closed" here would mean guessing a
        // penalty magnitude, which the no-hardcoding rule forbids.)
        let mut grid_import_caps = Vec::new();
        for (i, cap) in g.grid_import_caps.iter().enumerate() {
            match grid_import_cap_from_resolved(
                i,
                resolve_window(ha, &cap.window, &mut diags).await,
                resolve(ha, &cap.max_kw, &mut diags).await,
                resolve(ha, &cap.penalty_aud_per_kwh, &mut diags).await,
            ) {
                Ok(cap) => grid_import_caps.push(cap),
                Err(diag) => diags.push(diag),
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
            storage: storage_gated,
            grid_import_caps,
        };
        let out = self.planner.plan_with_preview(&world, &contracts, preview);
        // Advisory storage trajectory for the panel: a second, DISPLAY-ONLY solve at
        // rated power, run only when gating actually changed a limit (a fully-authorised
        // or storage-free cycle reuses `out`). Its loads/grid are discarded — only the
        // storage cards read `storage_report`; everything actuated still comes from `out`.
        let advisory = if needs_advisory {
            let world_adv = WorldState { storage, ..world.clone() };
            Some(self.planner.plan_with_preview(&world_adv, &contracts, preview))
        } else {
            None
        };
        let storage_report = advisory
            .as_ref()
            .map(|a| a.storage.as_slice())
            .unwrap_or_else(|| out.storage.as_slice());
        let executor = Executor { dry_run: self.dry_run };
        let executed = executor.execute(ha, global_enabled, &contracts, &out.decisions).await;
        // Storage: map each device's slot-0 plan to a current-step command, then
        // actuate the authorised directions (mirrors the load executor; the
        // unauthorised/unconfigured ones are skipped inside execute_storage).
        let mut storage_decisions: Vec<StorageDecision> = out
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
        // Unmodelled devices are absent from `out.storage`; command them IDLE (0 W → rate
        // 0 + idle threshold via the executor) so a prior cycle's active command can't
        // persist. Fail-loud (the diagnostic) + fail-safe (the device actually stops).
        for id in &idle_storage_ids {
            storage_decisions.push(StorageDecision {
                storage_id: id.clone(),
                charge_watts: 0.0,
                discharge_watts: 0.0,
                reason: "unmodelled this cycle (unreadable spec); commanded idle".into(),
            });
        }
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

        // Triaged alerts: the human-facing layer above the raw `diags` bag.
        let alerts = derive_alerts(&out.solver_error, &diags, &out.plans, preview, self.dry_run);

        SolveReport {
            at: now.to_rfc3339(),
            solver_ms: started.elapsed().as_millis() as u64,
            grid_minutes: self.registry.global.planning.grid_minutes,
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
            // Storage cards show the ADVISORY (rated-power) trajectory so the panel
            // reveals the full intended plan — actuation still follows `out` (gated).
            storage: storage_report
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
                    // Per-direction authority: a direction is actuated only when it
                    // is configured AND authorised (`execute_storage` gates each one
                    // independently). The device-level `authority` (any direction) is
                    // kept for the chip; the action pill keys off its own direction so
                    // a charge-only cabinet shows a planned discharge as advisory.
                    let ctrl = storage_controls.iter().find(|c| c.id == b.id);
                    let charge_authority =
                        ctrl.and_then(|c| c.charge.as_ref()).map(|d| d.authority).unwrap_or(false);
                    let discharge_authority = ctrl
                        .and_then(|c| c.discharge.as_ref())
                        .map(|d| d.authority)
                        .unwrap_or(false);
                    let authority = charge_authority || discharge_authority;
                    // Is the SHOWN (advisory) current command actually what gets committed
                    // this cycle? The panel plots the advisory plan, so its slot-0 can differ
                    // from the gated command the executor actually writes — different direction
                    // OR a different rate (e.g. a target needs some charge but the advisory
                    // arbitrage wants more). The pill is "live" only when they match exactly;
                    // see `storage_action_actuated`.
                    let committed = out.storage.iter().find(|g| g.id == b.id);
                    let committed_charge =
                        committed.and_then(|g| g.charge_kw.first().copied()).unwrap_or(0.0);
                    let committed_discharge =
                        committed.and_then(|g| g.discharge_kw.first().copied()).unwrap_or(0.0);
                    let action_actuated = storage_action_actuated(
                        action,
                        charge_now,
                        discharge_now,
                        committed_charge,
                        committed_discharge,
                        charge_authority,
                        discharge_authority,
                    );
                    let reasoning = self
                        .registry
                        .global
                        .storage
                        .iter()
                        .find(|s| s.id == b.id)
                        .map(|cfg| reasoning::for_storage(cfg, b, action, action_actuated, &grid))
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
                        charge_authority,
                        discharge_authority,
                        action_actuated,
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
        // can_take demands prefer the tighter target setpoint over the must-have limit.
        prefer_target: bool,
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
                        exact: false,         // deferrable runtime: run AT LEAST `minutes`
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
                // must-have uses max_percent; can-take prefers the tighter target_percent.
                let (first, second) = if prefer_target {
                    (target_percent, max_percent)
                } else {
                    (max_percent, target_percent)
                };
                let target = match resolve_opt(ha, first, diags).await {
                    Some(v) => Some(v),
                    None => resolve_opt(ha, second, diags).await,
                };
                if target.is_none() {
                    diags.push("humidity target unresolved; demand disabled".into());
                }
                let window = match window {
                    Some(w) => resolve_window(ha, w, diags).await,
                    None => None,
                };
                Demand {
                    kind: DemandKind::Threshold {
                        dir: ThresholdDir::Below,
                        limit: target.unwrap_or(f64::INFINITY),
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
            DemandCfg::Threshold {
                direction,
                value,
                target_value,
                start_hysteresis,
                window,
                max_minutes,
                max_price,
            } => {
                // must-have uses `value`; the (tighter) can-take prefers `target_value`.
                let value_opt = Some(value.clone());
                let (first, second) = if prefer_target {
                    (target_value, &value_opt)
                } else {
                    (&value_opt, target_value)
                };
                let limit = match resolve_opt(ha, first, diags).await {
                    Some(v) => Some(v),
                    None => resolve_opt(ha, second, diags).await,
                };
                if limit.is_none() {
                    diags.push("threshold limit unresolved; demand disabled".into());
                }
                let dir = match direction {
                    config::ThresholdDirCfg::Below => ThresholdDir::Below,
                    config::ThresholdDirCfg::Above => ThresholdDir::Above,
                };
                // A disabled (unresolved) limit must never read as "satisfied": for
                // Below an infinite limit is always-OK, for Above a -infinite one is.
                let disabled =
                    if dir == ThresholdDir::Below { f64::INFINITY } else { f64::NEG_INFINITY };
                let window = match window {
                    Some(w) => resolve_window(ha, w, diags).await,
                    None => None,
                };
                Demand {
                    kind: DemandKind::Threshold {
                        dir,
                        limit: limit.unwrap_or(disabled),
                        observed: if limit.is_some() { observed } else { None },
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
            DemandCfg::Program { length_hours, length_minutes, window, max_price } => {
                // A program plans as a fixed contiguous runtime block (min_run forced
                // to the length in the contract build); resolve its length like a
                // runtime amount.
                let mut mins = match resolve_opt(ha, length_minutes, diags).await {
                    Some(m) => m.ceil().max(0.0) as u32,
                    None => match resolve_opt(ha, length_hours, diags).await {
                        Some(h) => config::hours_to_minutes(h),
                        None => 0,
                    },
                };
                let window = match resolve_window(ha, window, diags).await {
                    Some(w) => w,
                    None => {
                        diags.push("program window unresolved; demand disabled".into());
                        mins = 0;
                        Window { start: chrono::NaiveTime::MIN, end: chrono::NaiveTime::MIN }
                    }
                };
                Demand {
                    // exact: a program is held to EXACTLY this block length (upper-bounded in the LP).
                    kind: DemandKind::Runtime {
                        minutes: mins,
                        window,
                        completed_minutes: 0,
                        exact: true,
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
        DemandKind::Threshold { window, .. } => *window,
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
        DemandKind::Threshold { drop_per_hour, drift_per_hour, .. } => {
            *drop_per_hour = rates.drop_per_hour;
            *drift_per_hour = rates.drift_per_hour;
        }
        DemandKind::Runtime { .. } => {}
    }
    d
}

#[cfg(test)]
mod tests {
    use super::{
        derive_alerts, diag_is_actionable, diag_scope, gate_storage_for_actuation,
        grid_import_cap_from_resolved, reads_as_climate, storage_action_actuated,
    };
    use crate::lp::LoadPlan;
    use crate::model::{LoadId, ServiceCall, StorageControl, StorageDirection, StorageInput};
    use crate::status::Severity;

    fn storage_input() -> StorageInput {
        StorageInput {
            id: "b".into(),
            capacity_kwh: 10.0,
            soc_now_kwh: 5.0,
            min_soc_kwh: 1.0,
            max_soc_kwh: 10.0,
            max_charge_kw: 4.0,
            max_discharge_kw: 4.0,
            round_trip_efficiency: 0.9,
            allow_grid_charge: true,
            available: true,
            cycle_cost_aud_per_kwh: 0.001,
            goals: vec![],
            bank: None,
            load_share: None,
        }
    }
    fn dir(authority: bool) -> StorageDirection {
        StorageDirection {
            authority,
            set_rate: ServiceCall {
                domain: "input_number".into(),
                service: "set_value".into(),
                target_entity: "x".into(),
                data: serde_json::Value::Null,
            },
            set_threshold: None,
        }
    }

    #[test]
    fn grid_import_cap_resolution_is_fail_loud_but_safe() {
        use crate::model::Window;
        use chrono::NaiveTime;
        let w = Window {
            start: NaiveTime::from_hms_opt(15, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
        };
        // Happy path: all parts resolved, non-negative → a usable cap.
        let ok = grid_import_cap_from_resolved(0, Some(w), Some(0.0), Some(10.0)).unwrap();
        assert_eq!((ok.max_kw, ok.penalty_aud_per_kwh), (0.0, 10.0));
        // Invalid magnitude — negative OR non-finite (a miswired entity can report
        // NaN/inf) → skip loud, never clamped or passed through to poison the LP.
        for (mx, pen) in [
            (-1.0, 10.0),
            (0.0, -5.0),
            (f64::NAN, 10.0),
            (0.0, f64::INFINITY),
            (f64::NEG_INFINITY, 1.0),
        ] {
            assert!(
                grid_import_cap_from_resolved(1, Some(w), Some(mx), Some(pen))
                    .unwrap_err()
                    .contains("invalid magnitude"),
                "max_kw={mx} penalty={pen} must be rejected as invalid"
            );
        }
        // Any unresolved part → skip (never invent a window or magnitude).
        for parts in
            [(None, Some(0.0), Some(1.0)), (Some(w), None, Some(1.0)), (Some(w), Some(0.0), None)]
        {
            assert!(grid_import_cap_from_resolved(3, parts.0, parts.1, parts.2)
                .unwrap_err()
                .contains("unresolved"));
        }
    }

    #[test]
    fn gate_storage_zeroes_unconfigured_and_unauthorised_directions() {
        // Codex P2: a direction is actuatable ONLY when configured AND authorised. An advisory
        // battery (no control block) must be zeroed for the committed solve, else its rated power
        // shifts the load/grid plan around moves the executor will never make.
        let g = |ctrls: Vec<StorageControl>| {
            let out = gate_storage_for_actuation(&[storage_input()], &ctrls);
            (out[0].max_charge_kw, out[0].max_discharge_kw)
        };
        // no control entry at all → both directions zeroed
        assert_eq!(g(vec![]), (0.0, 0.0), "an unconfigured (advisory) battery is zeroed");
        // a control with no charge/discharge blocks → zeroed
        assert_eq!(
            g(vec![StorageControl { id: "b".into(), charge: None, discharge: None }]),
            (0.0, 0.0),
            "no charge/discharge block → zeroed"
        );
        // configured + authorised both → rated power preserved
        assert_eq!(
            g(vec![StorageControl {
                id: "b".into(),
                charge: Some(dir(true)),
                discharge: Some(dir(true)),
            }]),
            (4.0, 4.0),
            "fully authorised → unchanged"
        );
        // charge authorised, discharge configured-but-unauthorised → only discharge zeroed
        assert_eq!(
            g(vec![StorageControl {
                id: "b".into(),
                charge: Some(dir(true)),
                discharge: Some(dir(false)),
            }]),
            (4.0, 0.0),
            "unauthorised discharge zeroed; authorised charge kept (prod Scheduled behaviour)"
        );
    }

    fn plan(id: &str, unmet: f64) -> LoadPlan {
        LoadPlan { id: LoadId(id.into()), on: vec![], ct: vec![], unmet }
    }

    #[test]
    fn climate_running_state_is_read_by_domain_not_a_device_type() {
        // Replaces the old `load_type == Aircon` switch: a `climate.*` running entity
        // reads as a thermostat; everything else reads on/off. So any device kind
        // (the wizard's runtime/comfort/threshold/program) maps correctly.
        assert!(reads_as_climate("climate.ac_0"));
        assert!(reads_as_climate("climate.living_room"));
        assert!(!reads_as_climate("binary_sensor.indoor_comfort_hot_water_running"));
        assert!(!reads_as_climate("switch.pool_pump"));
        assert!(!reads_as_climate("input_boolean.aircon"));
        assert!(!reads_as_climate("no_domain"));
    }

    #[test]
    fn storage_action_actuated_is_live_only_when_shown_equals_committed() {
        // Fully authorised, advisory == gated (the no-gating case): live.
        assert!(storage_action_actuated("charging", 4.0, 0.0, 4.0, 0.0, true, true));
        // Charge authorised, but the shown advisory rate (4 kW) differs from the gated
        // command actually written (1 kW for a target) — the divergent-rate case: advisory.
        assert!(!storage_action_actuated("charging", 4.0, 0.0, 1.0, 0.0, true, false));
        // Charge authorised but the gated command is 0 (advisory arbitrage leans on an
        // unauthorised discharge): advisory.
        assert!(!storage_action_actuated("charging", 4.0, 0.0, 0.0, 0.0, true, false));
        // Charging shown but charge is NOT authorised (fully Scheduled): advisory even if
        // the numbers happen to match.
        assert!(!storage_action_actuated("charging", 4.0, 0.0, 4.0, 0.0, false, false));
        // Discharge shown, discharge authorised, shown == committed: live.
        assert!(storage_action_actuated("discharging", 0.0, 3.0, 0.0, 3.0, false, true));
        // Discharge shown but discharge unauthorised: advisory.
        assert!(!storage_action_actuated("discharging", 0.0, 3.0, 0.0, 0.0, true, false));
        // Idle and genuinely idle in both plans, with an authorised direction: live hold.
        assert!(storage_action_actuated("idle", 0.0, 0.0, 0.0, 0.0, true, false));
        // Idle with NO authorised direction (Scheduled): not under live control.
        assert!(!storage_action_actuated("idle", 0.0, 0.0, 0.0, 0.0, false, false));
    }

    #[test]
    fn derive_alerts_triages_each_signal() {
        let diags = vec![
            "load 'hot_water': run window unresolved; holding (observe-only)".to_string(),
            "forecast 4m old".to_string(), // benign -> NOT promoted
        ];
        // Solve failed + a fail-closed diag + an unmet load + preview on.
        let a = derive_alerts(
            &Some("solver error: Infeasible".into()),
            &diags,
            &[plan("hot_water", 90.0), plan("aircon", 0.0)],
            true,
            true,
        );
        assert_eq!(a.iter().filter(|x| x.severity == Severity::Critical).count(), 1, "{a:?}");
        assert!(a.iter().any(|x| x.severity == Severity::Warning
            && x.scope == "hot_water"
            && x.detail.contains("window")));
        assert!(a.iter().any(|x| x.severity == Severity::Warning && x.title == "Demand short"));
        // Preview wins the Info line over dry-run; benign diag is not promoted.
        assert!(a.iter().any(|x| x.severity == Severity::Info && x.detail.contains("Preview")));
        assert!(!a.iter().any(|x| x.detail.contains("Dry-run")));
        assert!(!a.iter().any(|x| x.detail.contains("forecast")));

        // Dry-run (no preview), clean solve, no unmet -> only the Dry-run Info line.
        let b = derive_alerts(&None, &[], &[plan("hot_water", 0.0)], false, true);
        assert_eq!(b.len(), 1);
        assert!(b[0].severity == Severity::Info && b[0].detail.contains("Dry-run"));

        // Fully clear -> no alerts.
        assert!(derive_alerts(&None, &[], &[], false, false).is_empty());
    }

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
