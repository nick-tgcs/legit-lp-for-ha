//! The device-agnostic core: the exact load contract the planner consumes.
//!
//! See docs/PLAN.md "The exact load contract". The planner (`lp.rs`) sees only
//! these resolved types — never an entity id, a brand, a `ValueRef`, or a raw
//! HA payload.

use std::time::Duration;

use chrono::{DateTime, NaiveTime};
use chrono_tz::Tz;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoadId(pub String);

/// How the LP models this load (one engine; not a routing choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planning {
    /// Shift required runtime across the horizon (hot water).
    Runtime,
    /// Model setpoint dynamics in the MILP (pre-condition).
    Predictive,
    /// Band constrained at the current step only, no forward model.
    Immediate,
}

/// A generic HA service call: `domain.service` against a target entity.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceCall {
    pub domain: String,
    pub service: String,
    pub target_entity: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Control {
    pub start: ServiceCall,
    pub stop: ServiceCall,
}

/// A daily time window. `end < start` means it crosses midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: NaiveTime,
    pub end: NaiveTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HardRules {
    pub min_run: Duration,
    pub min_off: Duration,
    pub max_starts_per_day: Option<u32>,
    /// Empty = always allowed.
    pub windows: Vec<Window>,
}

/// A demand = what work + the price ceiling it will run below.
#[derive(Debug, Clone, PartialEq)]
pub struct Demand {
    pub kind: DemandKind,
    /// Run only when the effective price is at/below this; `None` = any price.
    pub max_price: Option<f64>,
}

/// Which side of its `limit` a threshold load keeps the observed reading on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdDir {
    /// Keep at/below the limit (dehumidifier, fridge-style cooling).
    Below,
    /// Keep at/above the limit (humidifier).
    Above,
}

/// One enum covers every load kind — the planner branches on data, not brand.
#[derive(Debug, Clone, PartialEq)]
pub enum DemandKind {
    /// Deferrable / fixed-program — accumulate `minutes` of runtime within a
    /// window. A `program` (run-once contiguous block) is this with `min_run`
    /// forced to the block length, a single allowed start, and `exact: true` so
    /// credited runtime is bounded ABOVE too: a deferrable load only has a lower
    /// bound (run AT LEAST `minutes`); a program is held to EXACTLY `minutes`
    /// (± one grid step) so cheap/negative prices can't extend it past its length.
    Runtime { minutes: u32, window: Window, completed_minutes: u32, exact: bool },
    /// Keep an observed reading on one side of `limit`: `Below` (dehumidifier —
    /// at/below) or `Above` (humidifier — at/above). `immediate` uses only
    /// `dir`/`limit`/`observed`/`start_hysteresis`; the rates are kept for parity.
    Threshold {
        dir: ThresholdDir,
        limit: f64,
        observed: Option<f64>,
        start_hysteresis: f64,
        drop_per_hour: f64,
        drift_per_hour: f64,
        window: Option<Window>,
        cap_minutes: Option<u32>,
    },
    /// aircon — keep observed °C within [min, max].
    /// `ambient` (resolved ambient_entity reading) sets drift direction for
    /// `predictive`; `immediate` uses only the band/`observed`.
    TemperatureBand {
        min: f64,
        max: f64,
        observed: Option<f64>,
        change_per_hour: f64,
        drift_per_hour: f64,
        ambient: Option<f64>,
        window: Window,
        cap_minutes: Option<u32>,
    },
}

/// Per-start wear cost in AUD — same units as energy, no abstract weights.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preferences {
    pub start_cost_aud: f64,
}

/// All DERIVED from HA each cycle (recorder history); nothing persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// `None` = unknown/unavailable -> observe-only.
    pub running: Option<bool>,
    /// off->on transitions since local midnight.
    pub starts_today: u32,
    /// On-time inside the CURRENT must-have window instance.
    pub runtime_in_mh_window: Duration,
    /// On-time inside the current can-take window (cap usage).
    pub runtime_in_ct_window: Duration,
    /// Length of the current on/off stretch (min_run / min_off).
    pub current_stretch: Duration,
}

/// The resolved, device-agnostic contract — the solver boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadContract {
    pub id: LoadId,
    pub planning: Planning,
    /// Rated draw (kW): site balance + cost objective.
    pub power_kw: f64,
    /// Resolved from the authority entity; `false` = observe-only.
    pub authority: bool,
    pub hard: HardRules,
    pub must_have: Demand,
    /// Always carries a cap.
    pub can_take: Option<Demand>,
    pub prefs: Preferences,
    pub obs: Observation,
    /// Start/stop service calls (executor only — the planner never acts).
    pub control: Control,
}

/// Pure world snapshot the planner solves against. No I/O.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldState {
    pub now: DateTime<Tz>,
    pub global_enabled: bool,
    /// Import price, currency/kWh, current step.
    pub price_now: Option<f64>,
    /// Import price per step (grid-aligned; `None` = genuinely unknown).
    pub import: Vec<Option<f64>>,
    /// Export value per step (flat current if no forecast).
    pub feedin: Vec<f64>,
    /// kW per step: learned PV shape scaled to forecast day totals.
    pub pv: Vec<f64>,
    /// kW per step: learned consumption profile minus managed loads.
    pub baseload: Vec<f64>,
    /// Site storage devices (home batteries, EVs, …) co-optimised against
    /// price/PV. Empty = none modelled. Each is independent; a device that can
    /// discharge (`max_discharge_kw > 0`) self-arbitrages, while charge-only
    /// devices are driven by their `goals`.
    pub storage: Vec<StorageInput>,
    /// Resolved windowed grid-import caps (the "no grid during peak" control):
    /// inside each window, import above `max_kw` is penalised at `penalty_aud_per_kwh`
    /// per kWh in the LP's cost — soft, so the balance stays feasible. Empty = none.
    pub grid_import_caps: Vec<GridImportCapInput>,
}

/// A resolved windowed grid-import cap. Inside `window`, grid import above `max_kw`
/// (kW) is charged `penalty_aud_per_kwh` per kWh in the LP's cost. SOFT — an
/// overage slack absorbs whatever draw is physically unavoidable (PV+battery
/// short), so the site balance can never go infeasible.
#[derive(Debug, Clone, PartialEq)]
pub struct GridImportCapInput {
    pub window: Window,
    pub max_kw: f64,
    pub penalty_aud_per_kwh: f64,
}

/// One resolved storage device the planner co-optimises, built from config + a
/// live SoC read each cycle. Energy in kWh, power in kW. The planner never
/// *commands* storage (no service calls); it only plans and reports trajectories.
#[derive(Debug, Clone, PartialEq)]
pub struct StorageInput {
    pub id: String,
    /// Total usable capacity (kWh).
    pub capacity_kwh: f64,
    /// Live state of charge at `now` (kWh), already clamped into [min,max].
    pub soc_now_kwh: f64,
    /// Reserve floor and ceiling the plan must stay within (kWh).
    pub min_soc_kwh: f64,
    pub max_soc_kwh: f64,
    /// Power limits (kW). `max_discharge_kw == 0` => charge-only (e.g. an EV).
    pub max_charge_kw: f64,
    pub max_discharge_kw: f64,
    /// Round-trip efficiency in (0,1]; applied as sqrt on each of charge/discharge.
    pub round_trip_efficiency: f64,
    /// If false, may only charge from instantaneous PV (never the grid).
    pub allow_grid_charge: bool,
    /// Usable this cycle (e.g. an EV that is plugged in). When false the device
    /// neither charges nor discharges.
    pub available: bool,
    /// Throughput wear cost (AUD/kWh of charge+discharge) — breaks indifference
    /// against pointless cycling; keep well below a typical arbitrage spread.
    pub cycle_cost_aud_per_kwh: f64,
    /// Composable charging goals (in addition to inherent self-consumption /
    /// arbitrage for dischargeable devices). Empty = price-only behaviour.
    pub goals: Vec<StorageGoal>,
    /// Coordination bank id. Devices sharing a bank are driven as ONE unit — the
    /// LP forces a single charge/discharge direction across the whole bank each
    /// step (paralleled cabinets run together, as the real controller does),
    /// instead of letting a linear objective split two identical cabinets onto an
    /// arbitrary vertex (one working, one idle). `None` = independent (its own
    /// singleton bank); single-device behaviour is unchanged.
    pub bank: Option<String>,
    /// Fraction [0,1] of its bank's charge/discharge this cabinet carries —
    /// paralleled cabinets load-share both directions in hardware. `None` = an
    /// equal split. Non-zero shares are normalised within the bank (they sum to 1)
    /// and force a proportional split of whatever the bank does, so the plan
    /// can't park one cabinet idle while the other carries the house. `0.0` parks
    /// this member (its peers do the bank's work); if EVERY member is `0.0` the
    /// whole bank stays parked (zero throughput, any targets left unmet).
    pub load_share: Option<f64>,
}

/// A resolved storage charging goal. Multiple goals compose (a device may have a
/// deadline target AND opportunistic price-charging at once).
#[derive(Debug, Clone, PartialEq)]
pub enum StorageGoal {
    /// Reach `soc_kwh` by the next occurrence of `ready_by`, as cheaply as
    /// possible (soft — shortfall is reported, never forced).
    Target { soc_kwh: f64, ready_by: NaiveTime },
    /// Opportunistically charge while the import price is below `below`
    /// ($/kWh), up to `up_to_kwh` of stored energy.
    Price { below: f64, up_to_kwh: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    NoChange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub load_id: LoadId,
    pub action: Action,
    pub reason: String,
}

/// A price threshold the LP nudges so an existing price-gated executor automation
/// fires when intended: `active` while the LP drives the direction, `idle` while not.
#[derive(Debug, Clone, PartialEq)]
pub struct StorageThreshold {
    /// `domain.service` + target; data (`{value: …}`) is filled per cycle.
    pub call: ServiceCall,
    pub active: f64,
    pub idle: f64,
}

/// Resolved control for ONE storage direction (charge or discharge) — the executor
/// boundary. The planner never sees this (it stays in `StorageInput`); the executor
/// fills `set_rate`'s value with the planned watts each cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct StorageDirection {
    /// Resolved from the direction's authority entity; `false` = LP must not actuate.
    pub authority: bool,
    /// Per-cabinet rate setter (e.g. `input_number.set_value`); value = watts.
    pub set_rate: ServiceCall,
    pub set_threshold: Option<StorageThreshold>,
}

/// The resolved control surface for one storage device (executor only). A direction
/// is `None` when not configured (advisory: planned + reported, never actuated).
#[derive(Debug, Clone, PartialEq)]
pub struct StorageControl {
    pub id: String,
    pub charge: Option<StorageDirection>,
    pub discharge: Option<StorageDirection>,
}

/// Current-step storage command derived from the plan's slot 0 (receding horizon).
/// Charge/discharge are mutually exclusive in the plan, so at most one is non-zero.
#[derive(Debug, Clone, PartialEq)]
pub struct StorageDecision {
    pub storage_id: String,
    pub charge_watts: f64,
    pub discharge_watts: f64,
    pub reason: String,
}
