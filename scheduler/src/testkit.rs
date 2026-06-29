//! Shared test fixtures (contracts, worlds). Compiled into the lib so
//! integration tests in `tests/` can use the same builders as unit tests.

use std::time::Duration;

use chrono::{DateTime, NaiveTime, TimeZone};
use chrono_tz::Tz;

use crate::model::*;

pub fn sydney(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
    chrono_tz::Australia::Sydney.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}

pub fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

pub fn window(sh: u32, sm: u32, eh: u32, em: u32) -> Window {
    Window { start: t(sh, sm), end: t(eh, em) }
}

pub fn full_day() -> Window {
    window(0, 0, 0, 0)
}

pub fn service(domain: &str, svc: &str, target: &str) -> ServiceCall {
    ServiceCall {
        domain: domain.into(),
        service: svc.into(),
        target_entity: target.into(),
        data: serde_json::Value::Null,
    }
}

pub fn idle_observation() -> Observation {
    Observation {
        running: Some(false),
        starts_today: 0,
        runtime_in_mh_window: Duration::ZERO,
        runtime_in_ct_window: Duration::ZERO,
        // Long since last toggle: no min_off lock in effect.
        current_stretch: Duration::from_secs(24 * 3600),
    }
}

/// A runtime (hot-water-like) load: 90 min before 06:30, capped cheap boost.
pub fn runtime_contract() -> LoadContract {
    LoadContract {
        id: LoadId("hot_water".into()),
        planning: Planning::Runtime,
        power_kw: 3.6,
        authority: true,
        hard: HardRules {
            min_run: Duration::from_secs(20 * 60),
            min_off: Duration::from_secs(15 * 60),
            max_starts_per_day: Some(3),
            windows: vec![],
        },
        must_have: Demand {
            kind: DemandKind::Runtime {
                minutes: 90,
                window: window(0, 0, 6, 30),
                completed_minutes: 0,
            },
            max_price: None,
        },
        can_take: Some(Demand {
            kind: DemandKind::Runtime {
                minutes: 60,
                window: window(10, 0, 16, 0),
                completed_minutes: 0,
            },
            max_price: Some(0.10),
        }),
        prefs: Preferences { start_cost_aud: 0.02 },
        obs: idle_observation(),
        control: Control {
            start: service("input_boolean", "turn_on", "input_boolean.hot_water"),
            stop: service("input_boolean", "turn_off", "input_boolean.hot_water"),
        },
    }
}

/// An immediate (dehumidifier-like) setpoint load.
pub fn immediate_contract(observed: Option<f64>) -> LoadContract {
    LoadContract {
        id: LoadId("dehumidifier".into()),
        planning: Planning::Immediate,
        power_kw: 0.3,
        authority: true,
        hard: HardRules {
            min_run: Duration::from_secs(30 * 60),
            min_off: Duration::from_secs(15 * 60),
            max_starts_per_day: None,
            windows: vec![],
        },
        must_have: Demand {
            kind: DemandKind::Threshold {
                dir: crate::model::ThresholdDir::Below,
                limit: 65.0,
                observed,
                start_hysteresis: 2.0,
                drop_per_hour: 0.0,
                drift_per_hour: 0.0,
                window: None,
                cap_minutes: None,
            },
            max_price: Some(0.15),
        },
        can_take: Some(Demand {
            kind: DemandKind::Threshold {
                dir: crate::model::ThresholdDir::Below,
                limit: 55.0,
                observed,
                start_hysteresis: 0.0,
                drop_per_hour: 0.0,
                drift_per_hour: 0.0,
                window: Some(window(9, 0, 17, 0)),
                cap_minutes: Some(120),
            },
            max_price: Some(0.10),
        }),
        prefs: Preferences { start_cost_aud: 0.01 },
        obs: idle_observation(),
        control: Control {
            start: service("input_boolean", "turn_on", "input_boolean.dehumidifier"),
            stop: service("input_boolean", "turn_off", "input_boolean.dehumidifier"),
        },
    }
}

/// A predictive (aircon-like) temperature-band load.
pub fn predictive_contract(observed: Option<f64>, ambient: Option<f64>) -> LoadContract {
    LoadContract {
        id: LoadId("aircon".into()),
        planning: Planning::Predictive,
        power_kw: 2.5,
        authority: true,
        hard: HardRules {
            min_run: Duration::from_secs(20 * 60),
            min_off: Duration::from_secs(10 * 60),
            max_starts_per_day: None,
            windows: vec![],
        },
        must_have: Demand {
            kind: DemandKind::TemperatureBand {
                min: 19.0,
                max: 25.0,
                observed,
                change_per_hour: 1.5,
                drift_per_hour: 1.0,
                ambient,
                window: window(7, 0, 22, 0),
                cap_minutes: None,
            },
            max_price: Some(0.20),
        },
        can_take: None,
        prefs: Preferences { start_cost_aud: 0.05 },
        obs: idle_observation(),
        control: Control {
            start: service("input_boolean", "turn_on", "input_boolean.aircon"),
            stop: service("input_boolean", "turn_off", "input_boolean.aircon"),
        },
    }
}

/// A world with flat price/feedin and no PV, sized to `steps`.
pub fn flat_world(now: DateTime<Tz>, steps: usize, price: f64) -> WorldState {
    WorldState {
        now,
        global_enabled: true,
        price_now: Some(price),
        import: vec![Some(price); steps],
        feedin: vec![0.05; steps],
        pv: vec![0.0; steps],
        baseload: vec![0.8; steps],
        storage: vec![],
    }
}

/// A 10 kWh home battery starting at 5 kWh: ±5 kW, 90% round-trip, reserve 10%,
/// grid-charging allowed, no goals (self-arbitrages). Used by the storage tests.
pub fn test_storage() -> StorageInput {
    StorageInput {
        id: "battery".into(),
        capacity_kwh: 10.0,
        soc_now_kwh: 5.0,
        min_soc_kwh: 1.0,
        max_soc_kwh: 10.0,
        max_charge_kw: 5.0,
        max_discharge_kw: 5.0,
        round_trip_efficiency: 0.9,
        allow_grid_charge: true,
        available: true,
        // Tiny wear cost: breaks indifference without distorting real arbitrage.
        cycle_cost_aud_per_kwh: 0.001,
        goals: vec![],
    }
}
