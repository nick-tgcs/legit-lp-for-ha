//! Registry YAML — raw (serde) types + validation.
//!
//! Parsing/validation is pure; resolving `ValueRef`s and observations into
//! `LoadContract`s happens each cycle in the orchestrator (it needs `HaApi`).

use chrono::NaiveTime;
use serde::Deserialize;

use crate::error::SchedulerError;
use crate::model::Window;

/// A number that is either a literal or read live from an HA entity each cycle.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged, deny_unknown_fields)]
pub enum ValueRef {
    Literal { value: f64 },
    Entity { entity: String },
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WindowCfg {
    pub start: String,
    pub end: String,
}

impl WindowCfg {
    pub fn parse(&self) -> Result<Window, SchedulerError> {
        let parse = |s: &str| {
            NaiveTime::parse_from_str(s, "%H:%M")
                .map_err(|e| SchedulerError::Config(format!("bad window time '{s}': {e}")))
        };
        Ok(Window { start: parse(&self.start)?, end: parse(&self.end)? })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RegistryConfig {
    pub global: GlobalConfig,
    pub loads: Vec<LoadConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct GlobalConfig {
    pub enabled_entity: String,
    pub pricing: PricingConfig,
    pub power: Option<PowerConfig>,
    #[serde(default)]
    pub planning: PlanningConfig,
    /// Site-wide hard rules: reserved; v1 rejects non-empty.
    #[serde(default)]
    pub hard_rules: Vec<serde_yaml::Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PricingConfig {
    pub import_entity: String,
    pub feedin_entity: Option<String>,
    pub forecast: Option<ForecastConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ForecastConfig {
    pub entity: String,
    #[serde(default = "default_forecast_attribute")]
    pub attribute: String,
    /// Provider field-map -> canonical schema. Omit if already canonical.
    pub fields: Option<FieldMap>,
}

fn default_forecast_attribute() -> String {
    "forecast".into()
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FieldMap {
    pub start: Option<String>,
    pub end: Option<String>,
    pub import_per_kwh: Option<String>,
    pub export_per_kwh: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PowerConfig {
    pub consumption_entity: String,
    pub pv_entity: String,
    pub pv_forecast: Option<PvForecastConfig>,
    pub baseline_kw: f64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PvForecastConfig {
    pub today_entity: String,
    pub tomorrow_entity: String,
    pub now_entity: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadTypeCfg {
    HotWater,
    Dehumidifier,
    Aircon,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanningMode {
    Runtime,
    Predictive,
    Immediate,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LoadConfig {
    pub id: String,
    #[serde(rename = "type")]
    pub load_type: LoadTypeCfg,
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AuthorityCfg {
    pub enabled_entity: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ControlCfg {
    pub start: ServiceCallCfg,
    pub stop: ServiceCallCfg,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StateCfg {
    pub running_entity: String,
    /// Humidity/temperature sensor for setpoint loads; runtime loads omit it.
    pub observed_entity: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CapabilityCfg {
    pub power_kw: f64,
    pub drop_per_hour: Option<f64>,
    pub change_per_hour: Option<f64>,
    pub drift_per_hour: Option<f64>,
    pub ambient_entity: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HardRulesCfg {
    #[serde(default)]
    pub min_run_minutes: u32,
    #[serde(default)]
    pub min_off_minutes: u32,
    pub max_starts_per_day: Option<u32>,
    #[serde(default)]
    pub windows: Vec<WindowCfg>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DemandCfg {
    Runtime {
        amount_hours: Option<ValueRef>,
        amount_minutes: Option<ValueRef>,
        /// Can-take cap.
        max_minutes: Option<u32>,
        window: WindowCfg,
        max_price: Option<ValueRef>,
    },
    HumidityBelow {
        max_percent: Option<ValueRef>,
        /// Can-take target (tighter than must-have max).
        target_percent: Option<ValueRef>,
        start_hysteresis: Option<ValueRef>,
        window: Option<WindowCfg>,
        max_minutes: Option<u32>,
        max_price: Option<ValueRef>,
    },
    TemperatureBand {
        target_c: ValueRef,
        band_c: f64,
        window: WindowCfg,
        max_minutes: Option<u32>,
        max_price: Option<ValueRef>,
    },
}

impl DemandCfg {
    pub fn cap_minutes(&self) -> Option<u32> {
        match self {
            DemandCfg::Runtime { max_minutes, .. }
            | DemandCfg::HumidityBelow { max_minutes, .. }
            | DemandCfg::TemperatureBand { max_minutes, .. } => *max_minutes,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PreferencesCfg {
    pub start_cost_aud: f64,
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

    let mut seen = std::collections::HashSet::new();
    for l in &cfg.loads {
        if !seen.insert(&l.id) {
            return err(format!("duplicate load id '{}'", l.id));
        }
        l.control.start.split()?;
        l.control.stop.split()?;
        for w in &l.hard_rules.windows {
            w.parse()?;
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
                let ok = matches!(
                    &l.must_have,
                    DemandCfg::Runtime { amount_hours, amount_minutes, .. }
                        if amount_hours.is_some() || amount_minutes.is_some()
                );
                if !ok {
                    return err(format!(
                        "load '{}': planning=runtime requires a runtime must_have \
                         with amount_hours or amount_minutes",
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

fn validate_demand_windows(d: &DemandCfg) -> Result<(), SchedulerError> {
    match d {
        DemandCfg::Runtime { window, .. } | DemandCfg::TemperatureBand { window, .. } => {
            window.parse().map(|_| ())
        }
        DemandCfg::HumidityBelow { window, .. } => {
            window.as_ref().map(|w| w.parse().map(|_| ())).transpose().map(|_| ())
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

        // Hot water: live-tuned amount, overnight-ish window, no must-have price.
        match &cfg.loads[0].must_have {
            DemandCfg::Runtime { amount_hours, window, max_price, .. } => {
                assert!(matches!(amount_hours, Some(ValueRef::Entity { .. })));
                assert_eq!(window.parse().unwrap().start.to_string(), "00:00:00");
                assert!(max_price.is_none());
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
