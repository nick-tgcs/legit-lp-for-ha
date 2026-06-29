//! Registry YAML — raw (serde) types + validation.
//!
//! Parsing/validation is pure; resolving `ValueRef`s and observations into
//! `LoadContract`s happens each cycle in the orchestrator (it needs `HaApi`).

use chrono::NaiveTime;
use serde::{Deserialize, Serialize};

use crate::error::SchedulerError;

/// A number that is either a literal or read live from an HA entity each cycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged, deny_unknown_fields)]
pub enum ValueRef {
    /// A bare number, e.g. `capacity_kwh: 18.1`.
    Plain(f64),
    /// `{ value: 18.1 }` — the explicit literal form.
    Literal { value: f64 },
    /// `{ entity: sensor.x }` — read live each cycle.
    Entity { entity: String },
}

impl ValueRef {
    /// The constant value if this is a literal (`Plain`/`Literal`); `None` for an
    /// entity-ref, which is only known after a live read.
    pub fn as_literal(&self) -> Option<f64> {
        match self {
            ValueRef::Plain(v) | ValueRef::Literal { value: v } => Some(*v),
            ValueRef::Entity { .. } => None,
        }
    }

    /// The HA entity backing this ref, if any (`None` for a literal). Used by the
    /// reasoning panel to show the user which slider/sensor drove each value.
    pub fn source(&self) -> Option<&str> {
        match self {
            ValueRef::Entity { entity } => Some(entity),
            _ => None,
        }
    }
}

/// A boolean that is either a literal (`true`/`false`) or read live from an HA
/// entity each cycle (e.g. an `input_boolean` toggle).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum BoolRef {
    Plain(bool),
    Entity { entity: String },
}

/// A clock time that is either a literal "HH:MM" or read live from an HA entity
/// (e.g. an `input_datetime` the user edits) each cycle. This lets a load's run
/// window track a UI control instead of being baked into the registry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TimeRef {
    Literal(String),
    Entity { entity: String },
}

impl TimeRef {
    /// The HA entity backing this time ref, if any (`None` for a literal).
    pub fn source(&self) -> Option<&str> {
        match self {
            TimeRef::Entity { entity } => Some(entity),
            TimeRef::Literal(_) => None,
        }
    }
}

/// Parse a clock string, accepting both "HH:MM" (registry literals) and "HH:MM:SS"
/// (the shape `input_datetime` entities report their state in).
pub fn parse_clock(s: &str) -> Result<NaiveTime, SchedulerError> {
    let s = s.trim();
    NaiveTime::parse_from_str(s, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .map_err(|e| SchedulerError::Config(format!("bad window time '{s}': {e}")))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowCfg {
    pub start: TimeRef,
    pub end: TimeRef,
}

impl WindowCfg {
    /// Validate any literal bounds now (a malformed literal is a config error).
    /// Entity bounds are resolved — and checked — live each cycle, so they cannot
    /// be validated at parse time.
    pub fn validate(&self) -> Result<(), SchedulerError> {
        for b in [&self.start, &self.end] {
            if let TimeRef::Literal(s) = b {
                parse_clock(s)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegistryConfig {
    pub global: GlobalConfig,
    pub loads: Vec<LoadConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobalConfig {
    pub enabled_entity: String,
    /// Optional HA boolean: when ON, the scheduler ALSO solves observe-only loads
    /// (authority off) and shows the plan on the panel — a preview / dry sample —
    /// WITHOUT ever controlling them. Per-load authority still governs real
    /// control; the executor never acts on an unauthorised load. This is the
    /// persistent, automatable path; the in-panel checkbox (POST /api/preview) is
    /// the transient one, OR-combined with this. Omit to use only the checkbox.
    pub preview_entity: Option<String>,
    pub pricing: PricingConfig,
    pub power: Option<PowerConfig>,
    /// Site storage devices (home batteries, EVs, …). Omit/empty if none.
    #[serde(default)]
    pub storage: Vec<StorageConfig>,
    #[serde(default)]
    pub planning: PlanningConfig,
    /// Site-wide hard rules: reserved; v1 rejects non-empty.
    #[serde(default)]
    pub hard_rules: Vec<serde_yaml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PricingConfig {
    pub import_entity: String,
    pub feedin_entity: Option<String>,
    pub forecast: Option<ForecastConfig>,
    /// Optional SEPARATE feed-in (export) price forecast. Some providers (Amber)
    /// publish feed-in on its own sensor rather than as a field inside the import
    /// forecast; point this at that sensor so the panel's feed-in line and the
    /// export valuation vary per step instead of flat-lining the current value.
    /// The slot value field (mapped via `import_per_kwh`) IS the export price.
    pub feedin_forecast: Option<ForecastConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForecastConfig {
    pub entity: String,
    /// Attribute on `entity` holding the forecast list. Required — the engine never
    /// assumes a provider's attribute name (no-hardcoding rule).
    pub attribute: String,
    /// Provider field-map -> canonical schema. Omit if already canonical.
    pub fields: Option<FieldMap>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldMap {
    pub start: Option<String>,
    pub end: Option<String>,
    pub import_per_kwh: Option<String>,
    pub export_per_kwh: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PowerConfig {
    pub consumption_entity: String,
    pub pv_entity: String,
    pub pv_forecast: Option<PvForecastConfig>,
    /// Assumed always-on house baseload (kW) the plan subtracts before placing
    /// managed loads — literal or entity-ref (e.g. a measured baseload sensor).
    pub baseline_kw: ValueRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PvForecastConfig {
    pub today_entity: String,
    pub tomorrow_entity: String,
    pub now_entity: String,
}

/// One site storage device (a home battery cabinet, an EV, …). All energy in kWh,
/// power in kW. The scheduler plans + reports each device's optimal trajectory and,
/// for any direction given a `charge:`/`discharge:` control block AND live
/// authority (Optimiser mode), drives it by writing the planned rate; a direction
/// without a control block stays advisory (planned + reported, never actuated).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageConfig {
    pub id: String,
    /// State-of-charge sensors (%), one per pack unit; averaged to a device SoC.
    pub soc_entities: Vec<String>,
    /// Total usable capacity (kWh) — literal, or (better) an entity-ref to the
    /// live FullChargeCapacity so it tracks degradation.
    pub capacity_kwh: ValueRef,
    /// Per-cabinet charge power limit (kW) — literal or entity-ref.
    /// `max_discharge_kw == 0` = charge-only.
    pub max_charge_kw: ValueRef,
    /// Per-cabinet discharge power limit (kW). `0` = charge-only. Required — the
    /// engine never assumes a discharge rating (no-hardcoding rule).
    pub max_discharge_kw: ValueRef,
    /// Round-trip efficiency in (0,1] — literal or entity-ref. Required.
    pub round_trip_efficiency: ValueRef,
    /// Reserve floor (% of capacity) — the hard discharge floor. Literal or an
    /// entity-ref (e.g. an export-limit slider). Required.
    pub reserve_soc_pct: ValueRef,
    /// Usable ceiling, as a percentage of `capacity_kwh` — literal or entity-ref. Required.
    pub max_soc_pct: ValueRef,
    /// If false, may only charge from instantaneous PV (never the grid). Literal
    /// (`true`/`false`) or an entity-ref to a toggle. Required.
    pub allow_grid_charge: BoolRef,
    /// Optional binary sensor; when off the device is idle (e.g. EV unplugged).
    pub available_entity: Option<String>,
    /// Throughput wear cost (AUD/kWh) — literal or entity-ref. Required.
    pub cycle_cost_aud_per_kwh: ValueRef,
    /// Composable charging goals (deadline targets, opportunistic price). A
    /// dischargeable device self-arbitrages even with no goals.
    #[serde(default)]
    pub goals: Vec<StorageGoalCfg>,
    /// Per-direction CONTROL. When a direction is present AND authorised
    /// (Optimiser mode), the LP drives it by writing the planned per-cabinet rate
    /// (+ optional price threshold) each cycle; absent = that direction is
    /// advisory (planned + reported, never actuated).
    #[serde(default)]
    pub charge: Option<StorageDirectionCfg>,
    #[serde(default)]
    pub discharge: Option<StorageDirectionCfg>,
}

/// Control surface for ONE storage direction (charge or discharge). While the LP
/// holds authority for this direction it writes the planned per-cabinet rate to
/// `set_rate` (the service's value = watts) each cycle and, if `set_threshold` is
/// given, sets it to `active` while acting / `idle` while not — so an existing
/// price-gated executor automation fires exactly when the LP intends.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageDirectionCfg {
    pub authority: AuthorityCfg,
    /// The LP fills this call's value with the planned rate in watts.
    pub set_rate: ServiceCallCfg,
    #[serde(default)]
    pub set_threshold: Option<StorageThresholdCfg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageThresholdCfg {
    /// `domain.service`, e.g. `input_number.set_value`.
    pub service: String,
    pub target: String,
    /// Value set while the LP is actively driving this direction (permissive).
    pub active: ValueRef,
    /// Value set while the LP is idle on this direction (blocking).
    pub idle: ValueRef,
}

impl StorageThresholdCfg {
    pub fn split(&self) -> Result<(String, String), SchedulerError> {
        match self.service.split_once('.') {
            Some((d, s)) if !d.is_empty() && !s.is_empty() => Ok((d.into(), s.into())),
            _ => Err(SchedulerError::Config(format!(
                "service must be 'domain.service', got '{}'",
                self.service
            ))),
        }
    }
}

/// A storage charging goal. Multiple goals compose on one device — e.g. an EV
/// with both a morning target and opportunistic cheap top-ups.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StorageGoalCfg {
    /// Charge to `soc_pct` by the next occurrence of `ready_by`, as cheaply as
    /// possible. `ready_by` is a literal "HH:MM" or an entity-ref (e.g. the peak
    /// start `input_datetime`). Soft — shortfall is reported, never forced.
    Target { soc_pct: ValueRef, ready_by: TimeRef },
    /// Charge while import price is below `below` ($/kWh), up to `up_to_soc_pct`
    /// (default 100). The "just charge when it's cheap" policy.
    Price { below: ValueRef, up_to_soc_pct: Option<ValueRef> },
}

// No `default_*` value fns: the engine never assumes an operational magnitude.
// Every operational field is required (a missing key is a hard parse error); only
// the structural grid/horizon below keep defaults (solve mechanics, not config).

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanningConfig {
    #[serde(default = "default_grid_minutes")]
    pub grid_minutes: u32,
    #[serde(default = "default_horizon_hours")]
    pub horizon_hours: u32,
}

impl Default for PlanningConfig {
    fn default() -> Self {
        Self { grid_minutes: default_grid_minutes(), horizon_hours: default_horizon_hours() }
    }
}

fn default_grid_minutes() -> u32 {
    15
}
fn default_horizon_hours() -> u32 {
    24
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanningMode {
    Runtime,
    Predictive,
    Immediate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoadConfig {
    pub id: String,
    pub planning: PlanningMode,
    pub authority: AuthorityCfg,
    pub control: ControlCfg,
    pub state: StateCfg,
    pub capability: CapabilityCfg,
    pub hard_rules: HardRulesCfg,
    pub must_have: DemandCfg,
    pub can_take: Option<DemandCfg>,
    pub preferences: PreferencesCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthorityCfg {
    pub enabled_entity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlCfg {
    pub start: ServiceCallCfg,
    pub stop: ServiceCallCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceCallCfg {
    /// `domain.service`, e.g. `input_boolean.turn_on`.
    pub service: String,
    pub target: String,
    pub data: Option<serde_json::Value>,
}

impl ServiceCallCfg {
    pub fn split(&self) -> Result<(String, String), SchedulerError> {
        match self.service.split_once('.') {
            Some((d, s)) if !d.is_empty() && !s.is_empty() => Ok((d.into(), s.into())),
            _ => Err(SchedulerError::Config(format!(
                "service must be 'domain.service', got '{}'",
                self.service
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateCfg {
    pub running_entity: String,
    /// Humidity/temperature sensor for setpoint loads; runtime loads omit it.
    pub observed_entity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilityCfg {
    /// Rated electrical draw (kW) — literal or entity-ref (e.g. a live power meter).
    pub power_kw: ValueRef,
    /// Setpoint dynamics (°C or %RH per hour) — literal or entity-ref. Absent = the
    /// load has no modelled dynamics (0); required for `predictive` aircon/dehum.
    pub drop_per_hour: Option<ValueRef>,
    pub change_per_hour: Option<ValueRef>,
    pub drift_per_hour: Option<ValueRef>,
    pub ambient_entity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HardRulesCfg {
    /// Minimum continuous run once started (minutes) — literal or entity-ref.
    /// Required (use a literal `0` for "no minimum"); the engine never defaults it.
    pub min_run_minutes: ValueRef,
    /// Minimum off-time between runs (minutes) — literal or entity-ref.
    /// Required (use a literal `0` for "no minimum"); the engine never defaults it.
    pub min_off_minutes: ValueRef,
    /// Daily on-transition ceiling — literal or entity-ref; absent = unbounded.
    pub max_starts_per_day: Option<ValueRef>,
    #[serde(default)]
    pub windows: Vec<WindowCfg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DemandCfg {
    Runtime {
        amount_hours: Option<ValueRef>,
        amount_minutes: Option<ValueRef>,
        /// Can-take cap (minutes) — literal or entity-ref.
        max_minutes: Option<ValueRef>,
        window: WindowCfg,
        max_price: Option<ValueRef>,
    },
    HumidityBelow {
        max_percent: Option<ValueRef>,
        /// Can-take target (tighter than must-have max).
        target_percent: Option<ValueRef>,
        start_hysteresis: Option<ValueRef>,
        window: Option<WindowCfg>,
        max_minutes: Option<ValueRef>,
        max_price: Option<ValueRef>,
    },
    /// Keep an observed reading on one side of a limit (the wizard's "keep under a
    /// limit" kind). `direction: below` is the dehumidifier/cooling case (the
    /// generalisation of `humidity_below`); `above` is the humidifier case.
    Threshold {
        direction: ThresholdDirCfg,
        /// Must-have limit (%RH, ppm, …) — literal or entity-ref.
        value: ValueRef,
        /// Can-take target (tighter than the must-have limit).
        target_value: Option<ValueRef>,
        start_hysteresis: Option<ValueRef>,
        window: Option<WindowCfg>,
        max_minutes: Option<ValueRef>,
        max_price: Option<ValueRef>,
    },
    TemperatureBand {
        target_c: ValueRef,
        /// Half-band around target (°C) — literal or entity-ref (e.g. a hysteresis slider).
        band_c: ValueRef,
        window: WindowCfg,
        max_minutes: Option<ValueRef>,
        max_price: Option<ValueRef>,
    },
    /// A fixed program that runs ONCE as a contiguous block, started at the cheapest
    /// feasible moment inside `window` (the wizard's "fixed program" kind:
    /// washing machine, dryer). Lowered to a `runtime` demand with `min_run` forced
    /// to the block length and a single allowed start, so the whole run lands under
    /// any price cap or not at all (all-or-nothing).
    Program {
        length_hours: Option<ValueRef>,
        length_minutes: Option<ValueRef>,
        window: WindowCfg,
        max_price: Option<ValueRef>,
    },
}

impl DemandCfg {
    /// The can-take cap ref, if set. Presence (not the value) is what validation
    /// checks; the magnitude is resolved live each cycle.
    pub fn cap_minutes(&self) -> Option<&ValueRef> {
        match self {
            DemandCfg::Runtime { max_minutes, .. }
            | DemandCfg::HumidityBelow { max_minutes, .. }
            | DemandCfg::Threshold { max_minutes, .. }
            | DemandCfg::TemperatureBand { max_minutes, .. } => max_minutes.as_ref(),
            // A program is run-once (never a can-take); it carries no cap.
            DemandCfg::Program { .. } => None,
        }
    }
}

/// Which side of its limit a `threshold` load keeps the observed reading on.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdDirCfg {
    Below,
    Above,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreferencesCfg {
    /// Per-start wear cost (AUD) — literal or entity-ref.
    pub start_cost_aud: ValueRef,
}

/// Required runtime: live-tuned hours -> whole minutes, rounding up
/// (never under-deliver must-have).
pub fn hours_to_minutes(hours: f64) -> u32 {
    (hours * 60.0).ceil().max(0.0) as u32
}

/// The bundled example registry, seeded on first boot: the add-on config dir
/// starts empty, and crashing on a missing file would make install require a
/// manual file drop before the panel even comes up. The example is this site's
/// real contract surface and boots observe-only (authorities/global gate it).
const SEED_REGISTRY: &str = include_str!("../../addon/example.yaml");

/// Read the registry, writing the bundled example first if none exists yet.
/// Never overwrites an existing file.
pub fn load_or_seed(path: &std::path::Path) -> Result<String, SchedulerError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(path, SEED_REGISTRY).map_err(|e| {
                SchedulerError::Config(format!("seed registry {}: {e}", path.display()))
            })?;
            tracing::warn!(
                "registry {} was missing; seeded the bundled example — edit it for this site",
                path.display()
            );
            Ok(SEED_REGISTRY.to_string())
        }
        Err(e) => Err(SchedulerError::Config(format!("read registry {}: {e}", path.display()))),
    }
}

pub fn parse(yaml: &str) -> Result<RegistryConfig, SchedulerError> {
    let cfg: RegistryConfig =
        serde_yaml::from_str(yaml).map_err(|e| SchedulerError::Config(e.to_string()))?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Serialize a registry back to YAML, **validating it first**. The UI owns the
/// whole registry file (D1), so every save round-trips through the same
/// validation the loader applies — a save can never persist a config the engine
/// would reject on its next boot. Comments are not preserved (the file is
/// machine-managed); the full parsed struct is re-emitted, so fields the wizard
/// never surfaces are still round-tripped intact rather than dropped.
pub fn serialize_registry(cfg: &RegistryConfig) -> Result<String, SchedulerError> {
    validate(cfg)?;
    let mut v = serde_yaml::to_value(cfg).map_err(|e| SchedulerError::Config(e.to_string()))?;
    prune_empty(&mut v);
    serde_yaml::to_string(&v).map_err(|e| SchedulerError::Config(e.to_string()))
}

/// Drop `null` map entries and empty sequences/maps so the machine-managed file
/// stays clean and diff-friendly — every absent optional reads as an absence,
/// not a `null` line. Round-trips identically: a dropped Option re-parses to
/// `None`, a dropped `#[serde(default)]` Vec to empty (the round-trip test guards this).
fn prune_empty(v: &mut serde_yaml::Value) {
    use serde_yaml::Value;
    match v {
        Value::Mapping(m) => {
            let keys: Vec<Value> = m.keys().cloned().collect();
            for k in keys {
                if let Some(val) = m.get_mut(&k) {
                    prune_empty(val);
                }
                let drop = match m.get(&k) {
                    Some(Value::Null) => true,
                    Some(Value::Sequence(s)) => s.is_empty(),
                    Some(Value::Mapping(mm)) => mm.is_empty(),
                    _ => false,
                };
                if drop {
                    m.remove(&k);
                }
            }
        }
        Value::Sequence(s) => s.iter_mut().for_each(prune_empty),
        _ => {}
    }
}

/// Atomically write the (validated) registry to `path`: write a sibling temp
/// file in the same directory, then rename over the target — so an interrupted
/// write can never truncate or corrupt the live registry the engine reads.
pub fn save_registry(path: &std::path::Path, cfg: &RegistryConfig) -> Result<(), SchedulerError> {
    let yaml = serialize_registry(cfg)?;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("registry.yaml");
    let tmp = path.with_file_name(format!("{name}.tmp"));
    std::fs::write(&tmp, &yaml)
        .map_err(|e| SchedulerError::Config(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| SchedulerError::Config(format!("rename {}: {e}", path.display())))?;
    Ok(())
}

fn validate(cfg: &RegistryConfig) -> Result<(), SchedulerError> {
    let err = |m: String| Err(SchedulerError::Config(m));

    if !cfg.global.hard_rules.is_empty() {
        return err("site-wide hard_rules are not supported in v1 (must be empty)".into());
    }
    let g = cfg.global.planning.grid_minutes;
    if g == 0 || 60 % g != 0 {
        return err(format!("planning.grid_minutes must divide 60, got {g}"));
    }
    let h = cfg.global.planning.horizon_hours;
    if !(1..=48).contains(&h) {
        return err(format!("planning.horizon_hours must be 1..=48, got {h}"));
    }
    let mut storage_ids = std::collections::HashSet::new();
    for s in &cfg.global.storage {
        if !storage_ids.insert(&s.id) {
            return err(format!("duplicate storage id '{}'", s.id));
        }
        validate_storage(s)?;
    }

    let mut seen = std::collections::HashSet::new();
    for l in &cfg.loads {
        if !seen.insert(&l.id) {
            return err(format!("duplicate load id '{}'", l.id));
        }
        l.control.start.split()?;
        l.control.stop.split()?;
        for w in &l.hard_rules.windows {
            w.validate()?;
        }
        validate_demand_windows(&l.must_have)?;
        if let Some(ct) = &l.can_take {
            validate_demand_windows(ct)?;
            if ct.cap_minutes().is_none() {
                return err(format!(
                    "load '{}': can_take must always be capped (max_minutes)",
                    l.id
                ));
            }
        }
        match l.planning {
            PlanningMode::Runtime => {
                let runtime_ok = matches!(
                    &l.must_have,
                    DemandCfg::Runtime { amount_hours, amount_minutes, .. }
                        if amount_hours.is_some() || amount_minutes.is_some()
                );
                // A `program` is a run-once contiguous block — it plans as a runtime
                // demand, so it must declare a block length (hours or minutes).
                let program_ok = matches!(
                    &l.must_have,
                    DemandCfg::Program { length_hours, length_minutes, .. }
                        if length_hours.is_some() || length_minutes.is_some()
                );
                if !runtime_ok && !program_ok {
                    return err(format!(
                        "load '{}': planning=runtime requires a runtime must_have with \
                         amount_hours/amount_minutes, or a program must_have with \
                         length_hours/length_minutes",
                        l.id
                    ));
                }
            }
            PlanningMode::Predictive => {
                let rate =
                    l.capability.drop_per_hour.is_some() || l.capability.change_per_hour.is_some();
                if !rate || l.capability.drift_per_hour.is_none() {
                    return err(format!(
                        "load '{}': planning=predictive requires capability \
                         drop_per_hour/change_per_hour and drift_per_hour",
                        l.id
                    ));
                }
                if l.state.observed_entity.is_none() {
                    return err(format!(
                        "load '{}': planning=predictive requires state.observed_entity",
                        l.id
                    ));
                }
            }
            PlanningMode::Immediate => {
                if l.state.observed_entity.is_none() {
                    return err(format!(
                        "load '{}': planning=immediate requires state.observed_entity",
                        l.id
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_storage(s: &StorageConfig) -> Result<(), SchedulerError> {
    let err = |m: String| Err(SchedulerError::Config(m));
    if s.id.is_empty() {
        return err("storage device needs a non-empty id".into());
    }
    if s.soc_entities.is_empty() {
        return err(format!("storage '{}': soc_entities must list at least one sensor", s.id));
    }
    // Literal specs are checked now; entity-refs are validated live each cycle.
    if let Some(c) = s.capacity_kwh.as_literal() {
        if !c.is_finite() || c <= 0.0 {
            return err(format!("storage '{}': capacity_kwh must be > 0", s.id));
        }
    }
    if let Some(c) = s.max_charge_kw.as_literal() {
        if c <= 0.0 {
            return err(format!("storage '{}': max_charge_kw must be > 0", s.id));
        }
    }
    if let Some(d) = s.max_discharge_kw.as_literal() {
        if d < 0.0 {
            return err(format!(
                "storage '{}': max_discharge_kw must be >= 0 (0 = charge-only)",
                s.id
            ));
        }
    }
    if let Some(e) = s.round_trip_efficiency.as_literal() {
        if e <= 0.0 || e > 1.0 {
            return err(format!("storage '{}': round_trip_efficiency must be in (0,1]", s.id));
        }
    }
    if let Some(m) = s.max_soc_pct.as_literal() {
        if !(0.0..=100.0).contains(&m) {
            return err(format!("storage '{}': max_soc_pct must be within 0..=100", s.id));
        }
    }
    if let (Some(r), Some(m)) = (s.reserve_soc_pct.as_literal(), s.max_soc_pct.as_literal()) {
        if !(0.0..=100.0).contains(&r) || r >= m {
            return err(format!(
                "storage '{}': reserve_soc_pct ({r}) must be < max_soc_pct ({m}), within 0..=100",
                s.id
            ));
        }
    } else if let Some(r) = s.reserve_soc_pct.as_literal() {
        if !(0.0..=100.0).contains(&r) {
            return err(format!("storage '{}': reserve_soc_pct must be within 0..=100", s.id));
        }
    }
    for dir in [&s.charge, &s.discharge].into_iter().flatten() {
        dir.set_rate.split()?;
        if let Some(t) = &dir.set_threshold {
            t.split()?;
        }
    }
    for g in &s.goals {
        if let StorageGoalCfg::Target { ready_by: TimeRef::Literal(rb), .. } = g {
            parse_clock(rb).map_err(|e| {
                SchedulerError::Config(format!("storage '{}': bad ready_by '{rb}': {e}", s.id))
            })?;
        }
    }
    Ok(())
}

fn validate_demand_windows(d: &DemandCfg) -> Result<(), SchedulerError> {
    match d {
        DemandCfg::Runtime { window, .. }
        | DemandCfg::TemperatureBand { window, .. }
        | DemandCfg::Program { window, .. } => window.validate(),
        DemandCfg::HumidityBelow { window, .. } | DemandCfg::Threshold { window, .. } => {
            window.as_ref().map(|w| w.validate()).transpose().map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_yaml() -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../addon/example.yaml");
        std::fs::read_to_string(path).expect("read addon/example.yaml")
    }

    #[test]
    fn load_or_seed_missing_file_writes_the_bundled_example() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legit_lp.yaml");
        let yaml = load_or_seed(&path).expect("seeds on first boot");
        assert!(path.exists(), "registry file was created");
        parse(&yaml).expect("seeded registry parses + validates");
        assert_eq!(yaml, std::fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn load_or_seed_existing_file_is_returned_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legit_lp.yaml");
        std::fs::write(&path, "user: edited").unwrap();
        let yaml = load_or_seed(&path).expect("reads existing");
        assert_eq!(yaml, "user: edited", "never overwrites a user registry");
    }

    #[test]
    fn load_or_seed_unwritable_dir_errors_without_panic() {
        let path = std::path::Path::new("/nonexistent-dir/legit_lp.yaml");
        assert!(load_or_seed(path).is_err());
    }

    #[test]
    fn example_registry_round_trips_and_validates() {
        let cfg = parse(&example_yaml()).expect("example.yaml parses + validates");
        assert_eq!(cfg.loads.len(), 3);
        let ids: Vec<&str> = cfg.loads.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, ["hot_water", "dehumidifier", "aircon"]);
        assert_eq!(cfg.loads[0].planning, PlanningMode::Runtime);
        assert_eq!(cfg.loads[1].planning, PlanningMode::Immediate);
        assert_eq!(cfg.loads[2].planning, PlanningMode::Predictive);

        // Provider-neutral pricing block with the Amber field-map.
        let f = cfg.global.pricing.forecast.as_ref().unwrap();
        assert_eq!(f.attribute, "forecasts");
        let map = f.fields.as_ref().unwrap();
        assert_eq!(map.start.as_deref(), Some("start_time"));
        assert_eq!(map.import_per_kwh.as_deref(), Some("per_kwh"));

        // Separate feed-in (export) forecast wired (Amber publishes it on its own
        // sensor, distinct from the general/import forecast above).
        let ff = cfg.global.pricing.feedin_forecast.as_ref().unwrap();
        assert_eq!(ff.entity, "sensor.beckton_feed_in_forecast");
        assert_eq!(ff.attribute, "forecasts");
        assert_eq!(ff.fields.as_ref().unwrap().import_per_kwh.as_deref(), Some("per_kwh"));

        // Hot water: live-tuned amount, overnight-ish window, no must-have price.
        match &cfg.loads[0].must_have {
            DemandCfg::Runtime { amount_hours, window, max_price, .. } => {
                assert!(matches!(amount_hours, Some(ValueRef::Entity { .. })));
                assert!(matches!(&window.start, TimeRef::Literal(s) if s == "00:00"));
                assert!(max_price.is_none());
                window.validate().unwrap();
            }
            other => panic!("hot_water must_have wrong kind: {other:?}"),
        }
        // Every can_take is capped.
        for l in &cfg.loads {
            assert!(l.can_take.as_ref().unwrap().cap_minutes().is_some());
        }
        // Aircon predictive capability complete.
        let cap = &cfg.loads[2].capability;
        assert!(cap.change_per_hour.is_some() && cap.drift_per_hour.is_some());
        assert_eq!(cap.ambient_entity.as_deref(), Some("sensor.temp_outside"));

        // Preview (shadow-solve) toggle wired to an HA boolean.
        assert_eq!(
            cfg.global.preview_entity.as_deref(),
            Some("input_boolean.lp_scheduler_preview")
        );

        // Two site storage cabinets parsed, each entity-ref'd with per-direction control.
        assert_eq!(cfg.global.storage.len(), 2);
        let b = &cfg.global.storage[0];
        assert_eq!(b.id, "sonnen01");
        assert_eq!(b.soc_entities, ["sensor.usoc_sonnen01"]);
        assert!(matches!(b.capacity_kwh, ValueRef::Entity { .. }));
        assert_eq!(b.max_charge_kw.as_literal(), Some(4.0));
        assert_eq!(b.max_discharge_kw.as_literal(), Some(4.0));
        assert!(matches!(b.round_trip_efficiency, ValueRef::Entity { .. }));
        assert!(matches!(b.reserve_soc_pct, ValueRef::Entity { .. }));
        assert!(matches!(b.cycle_cost_aud_per_kwh, ValueRef::Entity { .. }));
        assert!(matches!(b.allow_grid_charge, BoolRef::Entity { .. }));
        let charge = b.charge.as_ref().expect("charge control");
        assert_eq!(charge.authority.enabled_entity, "binary_sensor.battery_charge_automated");
        assert_eq!(charge.set_rate.target, "input_number.input_number_sonnen01_grid_charge_rate");
        let t = charge.set_threshold.as_ref().expect("charge threshold");
        assert_eq!(t.active.as_literal(), Some(5.0));
        let disc = b.discharge.as_ref().expect("discharge control");
        assert_eq!(disc.authority.enabled_entity, "binary_sensor.battery_export_automated");
        assert_eq!(cfg.global.storage[1].id, "sonnen02");
    }

    /// A complete storage spec: every operational field is REQUIRED now (no engine
    /// defaults), so each device must spell them out (literal or entity-ref).
    fn storage_required_fields() -> &'static str {
        "      round_trip_efficiency: 0.9
      reserve_soc_pct: 0
      max_soc_pct: 100
      allow_grid_charge: true
      cycle_cost_aud_per_kwh: 0.001\n"
    }

    #[test]
    fn storage_list_parses_devices_and_goals() {
        let req = storage_required_fields();
        let yaml = format!(
            "
global:
  enabled_entity: input_boolean.x
  pricing: {{ import_entity: sensor.p }}
  storage:
    - id: home
      soc_entities: [sensor.home_soc]
      capacity_kwh: 10
      max_charge_kw: 5
      max_discharge_kw: 5
{req}    - id: ev
      soc_entities: [sensor.ev_soc]
      capacity_kwh: 60
      max_charge_kw: 7
      max_discharge_kw: 0
{req}      available_entity: binary_sensor.ev_plugged_in
      goals:
        - kind: target
          soc_pct: {{ value: 80 }}
          ready_by: \"07:00\"
        - kind: price
          below: {{ value: 0.10 }}
loads: []
"
        );
        let cfg = parse(&yaml).expect("storage list parses");
        assert_eq!(cfg.global.storage.len(), 2);
        let home = &cfg.global.storage[0];
        // The fields are now explicit, not defaulted — assert the supplied values.
        assert_eq!(home.round_trip_efficiency.as_literal(), Some(0.9));
        assert_eq!(home.max_soc_pct.as_literal(), Some(100.0));
        assert_eq!(home.reserve_soc_pct.as_literal(), Some(0.0));
        assert!(home.allow_grid_charge == BoolRef::Plain(true) && home.available_entity.is_none());
        let ev = &cfg.global.storage[1];
        assert_eq!(ev.max_discharge_kw.as_literal(), Some(0.0), "charge-only (explicit 0)");
        assert_eq!(ev.available_entity.as_deref(), Some("binary_sensor.ev_plugged_in"));
        assert_eq!(ev.goals.len(), 2);
        assert!(matches!(&ev.goals[0], StorageGoalCfg::Target { ready_by, .. }
                if matches!(ready_by, TimeRef::Literal(s) if s == "07:00")));
        assert!(matches!(&ev.goals[1], StorageGoalCfg::Price { .. }));
    }

    #[test]
    fn rejects_storage_reserve_at_or_above_max() {
        // Literal reserve >= ceiling is rejected at parse (entity-refs check live).
        let y = "
global:
  enabled_entity: input_boolean.x
  pricing: { import_entity: sensor.p }
  storage:
    - { id: a, soc_entities: [sensor.s], capacity_kwh: 10, max_charge_kw: 5, max_discharge_kw: 5, round_trip_efficiency: 0.9, reserve_soc_pct: 100, max_soc_pct: 100, allow_grid_charge: true, cycle_cost_aud_per_kwh: 0.001 }
loads: []
";
        assert!(
            matches!(parse(y), Err(SchedulerError::Config(m)) if m.contains("reserve_soc_pct"))
        );
    }

    #[test]
    fn rejects_storage_with_nonpositive_capacity() {
        let y = "
global:
  enabled_entity: input_boolean.x
  pricing: { import_entity: sensor.p }
  storage:
    - { id: a, soc_entities: [sensor.s], capacity_kwh: 0, max_charge_kw: 5, max_discharge_kw: 0, round_trip_efficiency: 0.9, reserve_soc_pct: 0, max_soc_pct: 100, allow_grid_charge: true, cycle_cost_aud_per_kwh: 0.001 }
loads: []
";
        assert!(matches!(parse(y), Err(SchedulerError::Config(m)) if m.contains("capacity_kwh")));
    }

    #[test]
    fn rejects_bad_ready_by_and_duplicate_storage_ids() {
        let bad_time = "
global:
  enabled_entity: input_boolean.x
  pricing: { import_entity: sensor.p }
  storage:
    - id: ev
      soc_entities: [sensor.s]
      capacity_kwh: 10
      max_charge_kw: 5
      max_discharge_kw: 0
      round_trip_efficiency: 0.9
      reserve_soc_pct: 0
      max_soc_pct: 100
      allow_grid_charge: true
      cycle_cost_aud_per_kwh: 0.001
      goals: [{ kind: target, soc_pct: { value: 80 }, ready_by: \"99:99\" }]
loads: []
";
        assert!(
            matches!(parse(bad_time), Err(SchedulerError::Config(m)) if m.contains("ready_by"))
        );
        let dup = "
global:
  enabled_entity: input_boolean.x
  pricing: { import_entity: sensor.p }
  storage:
    - { id: a, soc_entities: [sensor.s1], capacity_kwh: 10, max_charge_kw: 5, max_discharge_kw: 0, round_trip_efficiency: 0.9, reserve_soc_pct: 0, max_soc_pct: 100, allow_grid_charge: true, cycle_cost_aud_per_kwh: 0.001 }
    - { id: a, soc_entities: [sensor.s2], capacity_kwh: 10, max_charge_kw: 5, max_discharge_kw: 0, round_trip_efficiency: 0.9, reserve_soc_pct: 0, max_soc_pct: 100, allow_grid_charge: true, cycle_cost_aud_per_kwh: 0.001 }
loads: []
";
        assert!(
            matches!(parse(dup), Err(SchedulerError::Config(m)) if m.contains("duplicate storage"))
        );
    }

    // ---- D1: the UI owns the registry — serialize round-trips losslessly ----

    #[test]
    fn serialize_round_trips_the_example_registry() {
        // Parse -> serialize -> parse must reproduce the EXACT same struct: the UI
        // re-emits the whole file on every save, so any field it doesn't surface
        // (start/stop services, can_take, preferences, per-direction battery control)
        // must survive untouched. Plain vs Literal vs Entity value forms are distinct
        // variants and must each be preserved.
        let cfg = parse(&example_yaml()).expect("example parses");
        let yaml = serialize_registry(&cfg).expect("serializes");
        let round = parse(&yaml).expect("re-parses");
        assert_eq!(cfg, round, "full struct survived a serialize/parse round-trip");
        // Spot-check the three value forms came back as the SAME variant, not coerced.
        assert!(matches!(round.loads[0].capability.power_kw, ValueRef::Plain(_)), "Plain kept");
        match &round.loads[0].must_have {
            DemandCfg::Runtime { amount_hours, max_price, .. } => {
                assert!(matches!(amount_hours, Some(ValueRef::Entity { .. })), "Entity kept");
                assert!(max_price.is_none(), "a None optional stays None across round-trip");
            }
            other => panic!("hot_water must_have wrong kind: {other:?}"),
        }
        let charge = round.global.storage[0].charge.as_ref().unwrap();
        assert!(
            matches!(charge.set_threshold.as_ref().unwrap().active, ValueRef::Literal { .. }),
            "Literal {{ value }} form kept distinct from a bare Plain"
        );
    }

    #[test]
    fn serialize_validates_before_emitting() {
        // A save must never persist a config the loader would reject. Mutate a parsed
        // registry to something invalid and confirm serialize refuses it.
        let mut cfg = parse(&example_yaml()).expect("example parses");
        cfg.global.planning.grid_minutes = 7; // does not divide 60
        assert!(
            matches!(serialize_registry(&cfg), Err(SchedulerError::Config(m)) if m.contains("divide 60")),
            "serialize rejects an invalid registry instead of writing it"
        );
    }

    #[test]
    fn save_registry_writes_atomically_and_reloads_equal() {
        let cfg = parse(&example_yaml()).expect("example parses");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legit_lp.yaml");
        save_registry(&path, &cfg).expect("saves");
        // The live file parses back to the same struct, and no temp file is left behind.
        let reloaded = parse(&std::fs::read_to_string(&path).unwrap()).expect("reloads");
        assert_eq!(cfg, reloaded);
        assert!(!path.with_file_name("legit_lp.yaml.tmp").exists(), "temp file cleaned up");
    }

    // ---- D3: new device kinds parse, validate, and round-trip ----

    fn new_kinds_yaml() -> &'static str {
        "
global:
  enabled_entity: input_boolean.x
  pricing: { import_entity: sensor.p }
loads:
  - id: humidifier
    planning: immediate
    authority: { enabled_entity: binary_sensor.hum_auth }
    control:
      start: { service: switch.turn_on, target: switch.humidifier }
      stop: { service: switch.turn_off, target: switch.humidifier }
    state: { running_entity: switch.humidifier, observed_entity: sensor.humidity }
    capability: { power_kw: 0.2 }
    hard_rules: { min_run_minutes: 10, min_off_minutes: 10 }
    must_have:
      kind: threshold
      direction: above
      value: 40
      start_hysteresis: 2
    preferences: { start_cost_aud: 0.01 }
  - id: washer
    planning: runtime
    authority: { enabled_entity: binary_sensor.washer_auth }
    control:
      start: { service: switch.turn_on, target: switch.washer }
      stop: { service: switch.turn_off, target: switch.washer }
    state: { running_entity: switch.washer }
    capability: { power_kw: 0.5 }
    hard_rules: { min_run_minutes: 0, min_off_minutes: 0 }
    must_have:
      kind: program
      length_minutes: 90
      window: { start: \"09:00\", end: \"17:00\" }
      max_price: { value: 0.30 }
    preferences: { start_cost_aud: 0.02 }
"
    }

    #[test]
    fn threshold_above_and_program_parse_validate_and_round_trip() {
        let cfg = parse(new_kinds_yaml()).expect("new kinds parse + validate");
        match &cfg.loads[0].must_have {
            DemandCfg::Threshold { direction, value, .. } => {
                assert_eq!(*direction, ThresholdDirCfg::Above);
                assert_eq!(value.as_literal(), Some(40.0));
            }
            other => panic!("humidifier wrong kind: {other:?}"),
        }
        match &cfg.loads[1].must_have {
            DemandCfg::Program { length_minutes, .. } => {
                assert_eq!(length_minutes.as_ref().and_then(|v| v.as_literal()), Some(90.0));
            }
            other => panic!("washer wrong kind: {other:?}"),
        }
        // The UI owns the registry: these kinds survive a serialize/parse round-trip.
        let round = parse(&serialize_registry(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, round);
    }

    #[test]
    fn rejects_program_without_a_length() {
        // planning=runtime + a program must_have with no length is a hard error.
        let y = new_kinds_yaml().replace("length_minutes: 90", "length_minutes_typo: 90");
        assert!(
            matches!(parse(&y), Err(SchedulerError::Config(m)) if m.contains("planning=runtime"))
        );
    }

    #[test]
    fn value_ref_parses_both_forms() {
        let v: ValueRef = serde_yaml::from_str("{ value: 0.15 }").unwrap();
        assert_eq!(v, ValueRef::Literal { value: 0.15 });
        let v: ValueRef = serde_yaml::from_str("{ entity: sensor.x }").unwrap();
        assert_eq!(v, ValueRef::Entity { entity: "sensor.x".into() });
    }

    #[test]
    fn hours_to_minutes_rounds_up() {
        assert_eq!(hours_to_minutes(1.5), 90);
        assert_eq!(hours_to_minutes(0.011), 1);
        assert_eq!(hours_to_minutes(0.0), 0);
        assert_eq!(hours_to_minutes(-1.0), 0);
    }

    fn mutate_example(from: &str, to: &str) -> Result<RegistryConfig, SchedulerError> {
        let y = example_yaml();
        assert!(y.contains(from), "fixture must contain '{from}'");
        parse(&y.replacen(from, to, 1))
    }

    #[test]
    fn rejects_predictive_without_rates() {
        let r = mutate_example("change_per_hour: 1.5", "unused_field_xx: 1.5");
        assert!(matches!(r, Err(SchedulerError::Config(m)) if m.contains("predictive")));
    }

    // ---- Guard A: the no-hardcoding rule, enforced in the engine itself ----
    // (See docs/lp-no-hardcoding.md.) The engine must NEVER assume an operational
    // magnitude: every operational field is REQUIRED, and there are no `default_*`
    // value fns. These two tests fail the moment that regresses.

    #[test]
    fn guard_omitting_a_required_operational_field_is_a_parse_error() {
        // Each operational field, when omitted, must be a hard parse error — not a
        // silent engine default. Re-adding a serde default flips a case to Ok and fails.
        for field in [
            "round_trip_efficiency",
            "reserve_soc_pct",
            "max_soc_pct",
            "allow_grid_charge",
            "cycle_cost_aud_per_kwh",
            "max_discharge_kw",
        ] {
            let r = mutate_example(&format!("{field}:"), &format!("{field}_omitted_xx:"));
            assert!(
                matches!(&r, Err(SchedulerError::Config(m)) if m.contains(field)),
                "omitting an operational field must error (no engine default for {field}); got {r:?}"
            );
        }
    }

    #[test]
    fn guard_no_operational_default_fns_remain() {
        // Source-scan: no `default_*` value fn for an operational field may exist. The
        // needles are assembled at runtime so this test's own source can't trip it.
        let src = include_str!("config.rs");
        for suffix in
            ["zero_ref", "round_trip_ref", "cycle_cost_ref", "true_ref", "max_soc_pct_ref"]
        {
            let banned = format!("fn default_{suffix}");
            assert!(
                !src.contains(banned.as_str()),
                "operational default fn reintroduced: {banned}"
            );
        }
        // Only the two STRUCTURAL defaults (grid/horizon mechanics) are permitted.
        assert!(src.contains(&format!("fn default_{}", "grid_minutes")), "structural default kept");
    }

    #[test]
    fn rejects_runtime_planning_without_runtime_demand() {
        // Hot water planning=runtime but strip its amount.
        let r = mutate_example(
            "amount_hours: { entity: input_number.input_number_hot_water_runtime }",
            "amount_hours_disabled: 1",
        );
        assert!(matches!(r, Err(SchedulerError::Config(m)) if m.contains("planning=runtime")));
    }

    #[test]
    fn rejects_uncapped_can_take() {
        let r = mutate_example("max_minutes: 60", "max_minutes_disabled: 60");
        assert!(matches!(r, Err(SchedulerError::Config(m)) if m.contains("capped")));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let r = mutate_example("id: dehumidifier", "id: hot_water");
        assert!(matches!(r, Err(SchedulerError::Config(m)) if m.contains("duplicate")));
    }

    #[test]
    fn rejects_bad_service_and_bad_window() {
        let r = mutate_example("service: input_boolean.turn_on", "service: turnon");
        assert!(matches!(r, Err(SchedulerError::Config(m)) if m.contains("domain.service")));
        let r = mutate_example("start: \"00:00\"", "start: \"25:99\"");
        assert!(matches!(r, Err(SchedulerError::Config(m)) if m.contains("bad window time")));
    }

    #[test]
    fn rejects_bad_grid_and_site_hard_rules() {
        let r = mutate_example("grid_minutes: 15", "grid_minutes: 7");
        assert!(matches!(r, Err(SchedulerError::Config(m)) if m.contains("divide 60")));
        let r = mutate_example(
            "hard_rules:\n      min_run_minutes: 20",
            "hard_rules:\n      min_run_minutes: 20",
        );
        assert!(r.is_ok()); // sanity: untouched parse still fine
    }
}
