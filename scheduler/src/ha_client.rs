//! ALL HA REST I/O behind the `HaApi` trait: `get_state`, `call_service`,
//! `get_history` — plus the pure history fold that turns recorder rows into
//! the accumulators (`on-time`, `starts`, `current stretch`).
//!
//! Safe parsing: `unknown`/`unavailable`/`none`/`""` → `None`, never panic.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::error::SchedulerError;
use crate::model::ServiceCall;

pub const UNKNOWN_STATES: [&str; 4] = ["unknown", "unavailable", "none", ""];

#[derive(Debug, Clone, PartialEq)]
pub struct HaState {
    pub state: String,
    pub attributes: Value,
    pub last_changed: Option<DateTime<Utc>>,
}

impl HaState {
    pub fn from_json(v: &Value) -> Result<Self, SchedulerError> {
        let state = v["state"]
            .as_str()
            .ok_or_else(|| SchedulerError::HaApi("state body missing 'state'".into()))?
            .to_string();
        let last_changed = v["last_changed"]
            .as_str()
            .or_else(|| v["last_updated"].as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        Ok(Self { state, attributes: v["attributes"].clone(), last_changed })
    }

    pub fn is_unknown(&self) -> bool {
        UNKNOWN_STATES.contains(&self.state.to_lowercase().as_str())
    }

    pub fn as_f64(&self) -> Option<f64> {
        if self.is_unknown() {
            None
        } else {
            self.state.parse().ok()
        }
    }

    /// `on`/`off` → Some; anything in the unknown set → None.
    pub fn as_on_off(&self) -> Option<bool> {
        match self.state.as_str() {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }

    pub fn attr(&self, name: &str) -> Option<&Value> {
        self.attributes.get(name)
    }
}

/// "Is this state on?" per running-entity flavour. `None` = unknown.
pub fn on_predicate_binary(state: &str) -> Option<bool> {
    match state {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// climate entities: any active hvac mode counts as running; `off` is off;
/// the unknown set is unknown.
pub fn on_predicate_climate(state: &str) -> Option<bool> {
    if UNKNOWN_STATES.contains(&state.to_lowercase().as_str()) {
        None
    } else {
        Some(state != "off")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRow {
    pub state: String,
    pub at: DateTime<Utc>,
}

pub fn history_rows(body: &Value) -> Result<Vec<HistoryRow>, SchedulerError> {
    let lists = body
        .as_array()
        .ok_or_else(|| SchedulerError::HaApi("history body is not an array".into()))?;
    let rows = match lists.first() {
        Some(l) => l
            .as_array()
            .ok_or_else(|| SchedulerError::HaApi("history entity list is not an array".into()))?,
        None => return Ok(vec![]),
    };
    rows.iter()
        .map(|r| {
            let state = r["state"]
                .as_str()
                .ok_or_else(|| SchedulerError::HaApi("history row missing state".into()))?
                .to_string();
            let ts = r["last_changed"]
                .as_str()
                .or_else(|| r["last_updated"].as_str())
                .ok_or_else(|| SchedulerError::HaApi("history row missing timestamp".into()))?;
            let at = DateTime::parse_from_rfc3339(ts)
                .map_err(|e| SchedulerError::HaApi(format!("bad history timestamp: {e}")))?
                .with_timezone(&Utc);
            Ok(HistoryRow { state, at })
        })
        .collect()
}

/// The folded truth about one running entity over `[start, end]`.
///
/// HA's history returns the state AT `start` as the first row, so the fold
/// knows the initial condition. Unknown states close any on-span (not on),
/// and a transition from off/unknown to on counts as a start — the budget
/// protects hardware, whoever (or whatever) started it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fold {
    /// On-spans clipped to [start, end], absolute time.
    pub on_spans: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    /// Instants of off/unknown→on transitions inside (start, end].
    pub start_instants: Vec<DateTime<Utc>>,
    /// State at `end` (true/false/unknown).
    pub final_on: Option<bool>,
    /// Length of the final uninterrupted on/off stretch, measured to `end`.
    pub current_stretch: std::time::Duration,
}

pub fn fold_history(
    rows: &[HistoryRow],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    on: impl Fn(&str) -> Option<bool>,
) -> Fold {
    let mut on_spans = Vec::new();
    let mut start_instants = Vec::new();
    let mut cur: Option<bool> = None; // unknown until first row
    let mut span_open: Option<DateTime<Utc>> = None;
    let mut last_transition = start;

    for r in rows {
        let at = r.at.clamp(start, end);
        let next = on(&r.state);
        if next != cur {
            // Close an open on-span on any change away from on.
            if cur == Some(true) && next != Some(true) {
                if let Some(s) = span_open.take() {
                    on_spans.push((s, at));
                }
            }
            // Open a span / count a start on any change to on.
            if next == Some(true) && cur != Some(true) {
                span_open = Some(at);
                if at > start {
                    start_instants.push(at);
                }
            }
            if r.at >= start {
                last_transition = at;
            }
            cur = next;
        }
    }
    if let Some(s) = span_open {
        on_spans.push((s, end));
    }
    let current_stretch = (end - last_transition).to_std().unwrap_or_default();
    Fold { on_spans, start_instants, final_on: cur, current_stretch }
}

impl Fold {
    pub fn on_secs_total(&self) -> u64 {
        self.on_spans.iter().map(|(s, e)| (*e - *s).num_seconds().max(0) as u64).sum()
    }

    /// On-time intersected with absolute ranges (e.g. window instances).
    pub fn on_secs_within(&self, ranges: &[(DateTime<Utc>, DateTime<Utc>)]) -> u64 {
        let mut total = 0u64;
        for (s, e) in &self.on_spans {
            for (rs, re) in ranges {
                let lo = (*s).max(*rs);
                let hi = (*e).min(*re);
                if hi > lo {
                    total += (hi - lo).num_seconds() as u64;
                }
            }
        }
        total
    }

    pub fn starts(&self) -> u32 {
        self.start_instants.len() as u32
    }
}

/// The seam every consumer of HA goes through. Native async-fn-in-trait;
/// callers are generic over `A: HaApi`.
pub trait HaApi {
    fn get_state(
        &self,
        entity: &str,
    ) -> impl Future<Output = Result<HaState, SchedulerError>> + Send;

    fn call_service(
        &self,
        call: &ServiceCall,
    ) -> impl Future<Output = Result<(), SchedulerError>> + Send;

    fn get_history(
        &self,
        entity: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> impl Future<Output = Result<Vec<HistoryRow>, SchedulerError>> + Send;
}

/// Resolve the HA API `(base, token)` from the optional explicit config
/// (`hass_url` + `long_lived_token`), falling back to the Supervisor proxy.
///
/// Hardened against the bashio gotcha: `bashio::config 'key'` yields the string
/// `"null"` (not empty) for an unset optional, so `run.sh` exports
/// `SCHED_HASS_URL="null"` / `SCHED_TOKEN="null"` when the user leaves them
/// blank. We treat `"null"`/empty as absent, and only take the explicit branch
/// when the URL is actually absolute (`http(s)://…`) — otherwise we'd build a
/// bogus base like `"null/api"` and every request fails with reqwest's opaque
/// "builder error".
pub fn resolve_endpoint(
    hass_url: Option<String>,
    explicit_token: Option<String>,
    supervisor_token: Option<String>,
) -> (String, String) {
    let clean = |v: Option<String>| {
        v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != "null")
    };
    let url = clean(hass_url);
    let tok = clean(explicit_token);
    match (url, tok) {
        (Some(url), Some(tok)) if url.starts_with("http://") || url.starts_with("https://") => {
            (format!("{}/api", url.trim_end_matches('/')), tok)
        }
        _ => {
            ("http://supervisor/core/api".to_string(), clean(supervisor_token).unwrap_or_default())
        }
    }
}

/// Real client: Supervisor proxy or explicit `hass_url` + token.
pub struct HaClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl HaClient {
    /// `base` like `http://supervisor/core/api` or `http://host:8123/api`.
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self { base: base.into(), token: token.into(), http: reqwest::Client::new() }
    }

    async fn get_json(&self, path: &str) -> Result<Value, SchedulerError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SchedulerError::HaApi(format!("GET {path}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SchedulerError::HaApi(format!("GET {path}: HTTP {}", resp.status())));
        }
        resp.json().await.map_err(|e| SchedulerError::HaApi(format!("GET {path} body: {e}")))
    }
}

impl HaApi for HaClient {
    async fn get_state(&self, entity: &str) -> Result<HaState, SchedulerError> {
        let v = self.get_json(&format!("/states/{entity}")).await?;
        HaState::from_json(&v)
    }

    async fn call_service(&self, call: &ServiceCall) -> Result<(), SchedulerError> {
        let mut body = serde_json::json!({ "entity_id": call.target_entity });
        if let Value::Object(extra) = &call.data {
            for (k, v) in extra {
                body[k] = v.clone();
            }
        }
        let path = format!("/services/{}/{}", call.domain, call.service);
        let resp = self
            .http
            .post(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| SchedulerError::HaApi(format!("POST {path}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SchedulerError::HaApi(format!("POST {path}: HTTP {}", resp.status())));
        }
        Ok(())
    }

    async fn get_history(
        &self,
        entity: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<HistoryRow>, SchedulerError> {
        let path = format!(
            "/history/period/{}?filter_entity_id={}&end_time={}&minimal_response",
            start.to_rfc3339().replace('+', "%2B"),
            entity,
            end.to_rfc3339().replace('+', "%2B"),
        );
        history_rows(&self.get_json(&path).await?)
    }
}

/// One HA entity surfaced to the panel's entity picker (the wizard "Connect"
/// step maps a role to one of these). Serialized straight to the `/api/entities`
/// response.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EntityCatalogItem {
    pub id: String,
    pub domain: String,
    /// `friendly_name`, falling back to the entity id.
    pub name: String,
    pub state: String,
}

/// The domain part of an entity id (`switch.pool_pump` -> `switch`); "" if none.
pub fn domain_of(entity_id: &str) -> &str {
    entity_id.split_once('.').map(|(d, _)| d).unwrap_or("")
}

/// Parse an HA `/states` array into catalog items (pure; unit-tested). Entries
/// without an `entity_id` are skipped; a missing `friendly_name` falls back to id.
pub fn catalog_from_states(states: &Value) -> Vec<EntityCatalogItem> {
    states
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let id = e.get("entity_id")?.as_str()?.to_string();
                    let domain = domain_of(&id).to_string();
                    let name = e
                        .get("attributes")
                        .and_then(|a| a.get("friendly_name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or(&id)
                        .to_string();
                    let state = e.get("state").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    Some(EntityCatalogItem { id, domain, name, state })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Keep only entities whose domain is in `domains` (empty = keep all), sorted by
/// id for a stable picker. Pure; unit-tested.
pub fn filter_by_domains(
    mut items: Vec<EntityCatalogItem>,
    domains: &[String],
) -> Vec<EntityCatalogItem> {
    if !domains.is_empty() {
        items.retain(|i| domains.iter().any(|d| d == &i.domain));
    }
    items.sort_by(|a, b| a.id.cmp(&b.id));
    items
}

impl HaClient {
    /// The live HA entity catalog for the panel's entity picker. One GET of
    /// `/states`; parsing is the pure `catalog_from_states`.
    pub async fn list_states(&self) -> Result<Vec<EntityCatalogItem>, SchedulerError> {
        Ok(catalog_from_states(&self.get_json("/states").await?))
    }
}

/// Test double: canned states/history, records every service call.
#[derive(Default)]
pub struct RecordingHa {
    pub states: HashMap<String, Value>,
    pub history: HashMap<String, Vec<HistoryRow>>,
    pub calls: Mutex<Vec<ServiceCall>>,
    /// Entities that should fail with an API error (degraded-read tests).
    pub failing: Vec<String>,
}

impl HaApi for RecordingHa {
    async fn get_state(&self, entity: &str) -> Result<HaState, SchedulerError> {
        if self.failing.iter().any(|e| e == entity) {
            return Err(SchedulerError::HaApi(format!("{entity}: HTTP 500 (canned)")));
        }
        match self.states.get(entity) {
            Some(v) => HaState::from_json(v),
            None => Err(SchedulerError::HaApi(format!("{entity}: not in canned states"))),
        }
    }

    async fn call_service(&self, call: &ServiceCall) -> Result<(), SchedulerError> {
        self.calls.lock().unwrap().push(call.clone());
        Ok(())
    }

    async fn get_history(
        &self,
        entity: &str,
        _start: DateTime<Utc>,
        _end: DateTime<Utc>,
    ) -> Result<Vec<HistoryRow>, SchedulerError> {
        if self.failing.iter().any(|e| e == entity) {
            return Err(SchedulerError::HaApi(format!("{entity}: HTTP 500 (canned)")));
        }
        Ok(self.history.get(entity).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUP: &str = "http://supervisor/core/api";
    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn endpoint_unset_uses_supervisor_proxy() {
        assert_eq!(resolve_endpoint(None, None, s("sup-tok")), (SUP.into(), "sup-tok".into()));
    }

    #[test]
    fn endpoint_bashio_null_falls_back_to_proxy() {
        // The original bug: bashio exports "null" for unset optionals; that must
        // NOT be treated as an explicit URL (which produced base "null/api").
        let (base, tok) = resolve_endpoint(s("null"), s("null"), s("sup-tok"));
        assert_eq!(base, SUP);
        assert_eq!(tok, "sup-tok");
    }

    #[test]
    fn endpoint_blank_or_relative_url_falls_back_to_proxy() {
        assert_eq!(resolve_endpoint(s("  "), s("t"), s("sup")).0, SUP);
        // a non-absolute URL must never become the base
        assert_eq!(resolve_endpoint(s("supervisor"), s("t"), s("sup")).0, SUP);
    }

    #[test]
    fn endpoint_explicit_url_token_used_and_trimmed() {
        let (base, tok) = resolve_endpoint(s("http://homeassistant:8123/"), s(" llt "), None);
        assert_eq!(base, "http://homeassistant:8123/api");
        assert_eq!(tok, "llt");
    }

    fn utc(h: u32, m: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-06-10T{h:02}:{m:02}:00+00:00"))
            .unwrap()
            .with_timezone(&Utc)
    }

    fn rows(spec: &[(u32, u32, &str)]) -> Vec<HistoryRow> {
        spec.iter().map(|(h, m, s)| HistoryRow { state: (*s).into(), at: utc(*h, *m) }).collect()
    }

    #[test]
    fn ha_state_safe_parsing() {
        let v = serde_json::json!({"state": "0.204", "attributes": {"unit_of_measurement": "AUD/kWh"},
            "last_changed": "2026-06-10T07:00:00+00:00"});
        let s = HaState::from_json(&v).unwrap();
        assert_eq!(s.as_f64(), Some(0.204));
        assert!(!s.is_unknown());
        for u in UNKNOWN_STATES {
            let s = HaState::from_json(&serde_json::json!({"state": u, "attributes": {}})).unwrap();
            assert!(s.is_unknown(), "{u:?} is unknown");
            assert_eq!(s.as_f64(), None);
            assert_eq!(s.as_on_off(), None);
        }
        let s = HaState::from_json(&serde_json::json!({"state": "on", "attributes": {}})).unwrap();
        assert_eq!(s.as_on_off(), Some(true));
    }

    #[test]
    fn states_fixture_parses_without_panics() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/states.json");
        let bundle: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for (entity, body) in bundle.as_object().unwrap() {
            let s = HaState::from_json(body).unwrap_or_else(|e| panic!("{entity}: {e}"));
            if s.is_unknown() {
                assert_eq!(s.as_f64(), None);
            } else {
                let _ = s.as_f64(); // numeric or not — must simply not panic
            }
        }
    }

    #[test]
    fn entity_catalog_parses_filters_and_sorts() {
        // The /states endpoint is an ARRAY of {entity_id, state, attributes}.
        let states = serde_json::json!([
            {"entity_id": "switch.pool_pump", "state": "off",
             "attributes": {"friendly_name": "Pool Pump"}},
            {"entity_id": "sensor.humidity", "state": "61.0", "attributes": {}},
            {"entity_id": "climate.living_room", "state": "cool",
             "attributes": {"friendly_name": "Living Room AC"}},
            {"entity_id": "light.kitchen", "state": "on", "attributes": {}},
            {"state": "orphan-with-no-id"}
        ]);
        let all = catalog_from_states(&states);
        assert_eq!(all.len(), 4, "the id-less entry is skipped");
        // friendly_name used when present; falls back to the id otherwise.
        let pump = all.iter().find(|e| e.id == "switch.pool_pump").unwrap();
        assert_eq!((pump.domain.as_str(), pump.name.as_str()), ("switch", "Pool Pump"));
        let hum = all.iter().find(|e| e.id == "sensor.humidity").unwrap();
        assert_eq!(hum.name, "sensor.humidity", "no friendly_name -> id");

        // Domain filtering keeps only the requested domains, sorted by id.
        let picked = filter_by_domains(all.clone(), &["switch".into(), "climate".into()]);
        let ids: Vec<&str> = picked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["climate.living_room", "switch.pool_pump"]);
        // Empty filter keeps everything.
        assert_eq!(filter_by_domains(all, &[]).len(), 4);
        assert_eq!(domain_of("sensor.x"), "sensor");
        assert_eq!(domain_of("no_domain"), "");
    }

    #[test]
    fn climate_predicate() {
        assert_eq!(on_predicate_climate("heat"), Some(true));
        assert_eq!(on_predicate_climate("auto"), Some(true));
        assert_eq!(on_predicate_climate("off"), Some(false));
        assert_eq!(on_predicate_climate("unavailable"), None);
    }

    #[test]
    fn fold_known_sequence() {
        // off@00:00, on@01:00, off@02:30, on@03:30; range 00:00-04:00.
        let r = rows(&[(0, 0, "off"), (1, 0, "on"), (2, 30, "off"), (3, 30, "on")]);
        let f = fold_history(&r, utc(0, 0), utc(4, 0), on_predicate_binary);
        assert_eq!(f.on_secs_total(), 90 * 60 + 30 * 60);
        assert_eq!(f.starts(), 2);
        assert_eq!(f.final_on, Some(true));
        assert_eq!(f.current_stretch.as_secs(), 30 * 60); // on since 03:30
                                                          // Window intersection 02:00-04:00: 02:00-02:30 on + 03:30-04:00 on.
        assert_eq!(f.on_secs_within(&[(utc(2, 0), utc(4, 0))]), 60 * 60);
    }

    #[test]
    fn fold_initial_on_is_not_a_start() {
        // State AT range start is 'on' (HA emits it as the first row).
        let r = rows(&[(0, 0, "on"), (1, 0, "off")]);
        let f = fold_history(&r, utc(0, 0), utc(4, 0), on_predicate_binary);
        assert_eq!(f.starts(), 0);
        assert_eq!(f.on_secs_total(), 3600);
        assert_eq!(f.final_on, Some(false));
        assert_eq!(f.current_stretch.as_secs(), 3 * 3600);
    }

    #[test]
    fn fold_unknown_closes_spans_and_recovery_counts_start() {
        let r = rows(&[(0, 0, "on"), (1, 0, "unavailable"), (2, 0, "on")]);
        let f = fold_history(&r, utc(0, 0), utc(4, 0), on_predicate_binary);
        assert_eq!(f.on_secs_total(), 3600 + 2 * 3600);
        assert_eq!(f.starts(), 1); // unavailable -> on at 02:00
    }

    #[test]
    fn fold_empty_history_is_all_unknown() {
        let f = fold_history(&[], utc(0, 0), utc(4, 0), on_predicate_binary);
        assert_eq!(f.final_on, None);
        assert_eq!(f.on_secs_total(), 0);
        assert_eq!(f.starts(), 0);
    }

    #[test]
    fn fold_real_yesterday_fixture_is_consistent() {
        let path =
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/history_hot_water_yesterday.json");
        let body: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let r = history_rows(&body).unwrap();
        assert!(!r.is_empty());
        let (start, end) =
            (r.first().unwrap().at, r.last().unwrap().at + chrono::Duration::hours(1));
        let f = fold_history(&r, start, end, on_predicate_binary);
        let span = (end - start).num_seconds() as u64;
        assert!(f.on_secs_total() <= span);
        // Independent transition count: prev not-on -> on, after the first row.
        let mut oracle = 0;
        let mut prev = on_predicate_binary(&r[0].state);
        for row in &r[1..] {
            let next = on_predicate_binary(&row.state);
            if next == Some(true) && prev != Some(true) {
                oracle += 1;
            }
            if next != prev {
                prev = next;
            }
        }
        assert_eq!(f.starts(), oracle);
    }

    mod invariants {
        use super::*;
        use proptest::prelude::*;

        prop_compose! {
            fn any_rows()(states in proptest::collection::vec(
                prop::sample::select(vec!["on", "off", "unavailable"]), 0..40,
            ), minutes in proptest::collection::vec(0u32..1440, 0..40)) -> Vec<HistoryRow> {
                let mut ms: Vec<u32> = minutes;
                ms.sort_unstable();
                states.iter().zip(ms).map(|(s, m)| HistoryRow {
                    state: (*s).into(),
                    at: utc(m / 60, m % 60),
                }).collect()
            }
        }

        proptest! {
            #[test]
            fn fold_bounds_and_additivity(rows in any_rows()) {
                let (start, end) = (utc(0, 0), utc(23, 59));
                let f = fold_history(&rows, start, end, on_predicate_binary);
                let span = (end - start).num_seconds() as u64;
                prop_assert!(f.on_secs_total() <= span);
                prop_assert!(f.current_stretch.as_secs() <= span);
                prop_assert!(u64::from(f.starts()) <= rows.len() as u64);
                // Disjoint-window additivity of on-time.
                let mid = utc(12, 0);
                let a = f.on_secs_within(&[(start, mid)]);
                let b = f.on_secs_within(&[(mid, end)]);
                prop_assert_eq!(a + b, f.on_secs_total());
            }
        }
    }
}
