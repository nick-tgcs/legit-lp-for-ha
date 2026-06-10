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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadType {
    HotWater,
    Dehumidifier,
    Aircon,
}

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

/// One enum covers all three load types — the planner branches on data, not brand.
#[derive(Debug, Clone, PartialEq)]
pub enum DemandKind {
    /// hot_water — accumulate runtime within a window.
    Runtime {
        minutes: u32,
        window: Window,
        completed_minutes: u32,
    },
    /// dehumidifier — keep observed %RH at/below `max`.
    /// `drop_per_hour`/`drift_per_hour` drive the trajectory for `predictive`;
    /// `immediate` uses only `max`/`observed`/`start_hysteresis`.
    HumidityBelow {
        max: f64,
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
    pub load_type: LoadType,
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
