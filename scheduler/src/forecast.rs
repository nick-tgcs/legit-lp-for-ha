//! Provider-neutral price forecast: canonical-schema slots (optionally via a
//! declarative field-map) → `price[]`/`feedin[]` resampled onto the grid.
//!
//! Contract of record: docs/schemas/price-forecast.schema.json. The serde
//! shape here is kept structurally identical to it (CI tests enforce this by
//! validating fixtures against the schema).

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::FieldMap;
use crate::error::SchedulerError;
use crate::time::Grid;

/// One canonical forecast slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub import_per_kwh: f64,
    pub export_per_kwh: Option<f64>,
}

fn field<'a>(
    map: Option<&'a FieldMap>,
    pick: impl Fn(&'a FieldMap) -> Option<&'a String>,
    canonical: &'a str,
) -> &'a str {
    map.and_then(pick).map(String::as_str).unwrap_or(canonical)
}

/// Parse a forecast attribute (array of provider slots) into canonical slots,
/// applying the field-map. Enforces the contract rules: sorted by start,
/// `end > start`, no overlaps. Violations reject the WHOLE forecast (treated
/// as absent by the caller — never guessed).
pub fn parse_slots(attr: &Value, map: Option<&FieldMap>) -> Result<Vec<Slot>, SchedulerError> {
    let f_start = field(map, |m| m.start.as_ref(), "start");
    let f_end = field(map, |m| m.end.as_ref(), "end");
    let f_imp = field(map, |m| m.import_per_kwh.as_ref(), "import_per_kwh");
    let f_exp = field(map, |m| m.export_per_kwh.as_ref(), "export_per_kwh");

    let arr = attr
        .as_array()
        .ok_or_else(|| SchedulerError::Config("forecast attribute is not an array".into()))?;

    let mut slots = Vec::with_capacity(arr.len());
    for (i, s) in arr.iter().enumerate() {
        let ts = |name: &str| -> Result<DateTime<Utc>, SchedulerError> {
            let raw = s[name].as_str().ok_or_else(|| {
                SchedulerError::Config(format!("forecast slot {i}: missing '{name}'"))
            })?;
            DateTime::parse_from_rfc3339(raw)
                .map(|t| t.with_timezone(&Utc))
                .map_err(|e| SchedulerError::Config(format!("forecast slot {i} '{name}': {e}")))
        };
        let start = ts(f_start)?;
        let end = ts(f_end)?;
        let import = s[f_imp].as_f64().ok_or_else(|| {
            SchedulerError::Config(format!("forecast slot {i}: missing numeric '{f_imp}'"))
        })?;
        if end <= start {
            return Err(SchedulerError::Config(format!("forecast slot {i}: end <= start")));
        }
        slots.push(Slot { start, end, import_per_kwh: import, export_per_kwh: s[f_exp].as_f64() });
    }
    for w in slots.windows(2) {
        if w[1].start < w[0].start {
            return Err(SchedulerError::Config("forecast slots not sorted by start".into()));
        }
        if w[1].start < w[0].end {
            return Err(SchedulerError::Config("forecast slots overlap".into()));
        }
    }
    Ok(slots)
}

/// Per-step price series, grid-aligned.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceSeries {
    /// Import price per step; `None` = genuinely unknown (no slot, no current).
    pub import: Vec<Option<f64>>,
    /// Export value per step (slot value, else flat current feed-in, else 0).
    pub feedin: Vec<f64>,
}

/// Resample slots onto the grid. Gap rule: flat-fill from the last known slot
/// (leading gap from `price_now`); the CURRENT step is always overridden by
/// `price_now` when known. Missing forecast entirely → flat `price_now`.
pub fn resample(
    slots: &[Slot],
    grid: &Grid,
    price_now: Option<f64>,
    feedin_now: Option<f64>,
) -> PriceSeries {
    let mut import = Vec::with_capacity(grid.steps.len());
    let mut feedin = Vec::with_capacity(grid.steps.len());
    let mut last_import: Option<f64> = price_now;
    let mut last_feedin: Option<f64> = feedin_now;

    for step in &grid.steps {
        let at = step.with_timezone(&Utc);
        let hit = slots.iter().find(|s| s.start <= at && at < s.end);
        let imp = match hit {
            Some(s) => {
                last_import = Some(s.import_per_kwh);
                if let Some(e) = s.export_per_kwh {
                    last_feedin = Some(e);
                }
                Some(s.import_per_kwh)
            }
            None => last_import, // flat fill (or None if nothing known yet)
        };
        import.push(imp);
        feedin.push(hit.and_then(|s| s.export_per_kwh).or(last_feedin).unwrap_or(0.0));
    }
    if let Some(p) = price_now {
        if let Some(first) = import.first_mut() {
            *first = Some(p);
        }
    }
    PriceSeries { import, feedin }
}

/// Resample a DEDICATED feed-in (export) forecast onto the grid. Mirrors
/// [`resample`]'s gap rule for the feed-in series: flat-fill from the last known
/// slot, then override the current step with `feedin_now` when known. Each slot's
/// value IS the export price: for a provider whose feed-in forecast is a SEPARATE
/// sensor from the import forecast (e.g. Amber), the field-map points the slot's
/// `import_per_kwh` at that sensor's value field, and it is read here as feed-in.
/// Without this, the panel's feed-in line could only carry the single current
/// value forward, drawing a flat line.
pub fn resample_feedin(slots: &[Slot], grid: &Grid, feedin_now: Option<f64>) -> Vec<f64> {
    let mut out = Vec::with_capacity(grid.steps.len());
    let mut last = feedin_now;
    for step in &grid.steps {
        let at = step.with_timezone(&Utc);
        if let Some(s) = slots.iter().find(|s| s.start <= at && at < s.end) {
            last = Some(s.import_per_kwh);
        }
        out.push(last.unwrap_or(0.0));
    }
    if let Some(f) = feedin_now {
        if let Some(first) = out.first_mut() {
            *first = f;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FieldMap;
    use crate::testkit::sydney;

    fn amber_map() -> FieldMap {
        FieldMap {
            start: Some("start_time".into()),
            end: Some("end_time".into()),
            import_per_kwh: Some("per_kwh".into()),
            export_per_kwh: None,
        }
    }

    fn read_fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn schema() -> Value {
        let path =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/schemas/price-forecast.schema.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn to_canonical_json(slots: &[Slot]) -> Value {
        Value::Array(
            slots
                .iter()
                .map(|s| {
                    let mut o = serde_json::json!({
                        "start": s.start.to_rfc3339(),
                        "end": s.end.to_rfc3339(),
                        "import_per_kwh": s.import_per_kwh,
                    });
                    if let Some(e) = s.export_per_kwh {
                        o["export_per_kwh"] = e.into();
                    }
                    o
                })
                .collect(),
        )
    }

    #[test]
    fn real_amber_fixture_maps_to_canonical_and_validates_against_schema() {
        let body = read_fixture("forecast_amber.json");
        let slots = parse_slots(&body["attributes"]["forecasts"], Some(&amber_map())).unwrap();
        assert!(slots.len() > 40, "real forecast has many slots");
        assert!(slots.windows(2).all(|w| w[0].start <= w[1].start));
        // Variable slot durations are REAL (Amber: 5-min current, 30-min rest).
        let durations: std::collections::HashSet<i64> =
            slots.iter().map(|s| (s.end - s.start).num_minutes()).collect();
        assert!(durations.len() > 1, "fixture exercises variable durations: {durations:?}");
        // The mapped output IS the canonical contract.
        let validator = jsonschema::validator_for(&schema()).unwrap();
        assert!(validator.is_valid(&to_canonical_json(&slots)));
    }

    #[test]
    fn canonical_fixture_parses_without_field_map_and_validates() {
        let body = read_fixture("forecast_canonical.json");
        let validator = jsonschema::validator_for(&schema()).unwrap();
        assert!(validator.is_valid(&body), "canonical fixture matches the schema");
        let slots = parse_slots(&body, None).unwrap();
        assert_eq!(slots.len(), 3);
        assert_eq!(slots[0].export_per_kwh, Some(0.08));
        assert_eq!(slots[1].export_per_kwh, None);
    }

    #[test]
    fn malformed_slots_reject_whole_forecast() {
        let bad = serde_json::json!([
            {"start": "2026-06-10T10:00:00+00:00", "end": "2026-06-10T10:00:00+00:00", "import_per_kwh": 0.1}
        ]);
        assert!(
            matches!(parse_slots(&bad, None), Err(SchedulerError::Config(m)) if m.contains("end <= start"))
        );

        let overlap = serde_json::json!([
            {"start": "2026-06-10T10:00:00+00:00", "end": "2026-06-10T11:00:00+00:00", "import_per_kwh": 0.1},
            {"start": "2026-06-10T10:30:00+00:00", "end": "2026-06-10T12:00:00+00:00", "import_per_kwh": 0.2}
        ]);
        assert!(
            matches!(parse_slots(&overlap, None), Err(SchedulerError::Config(m)) if m.contains("overlap"))
        );

        let unsorted = serde_json::json!([
            {"start": "2026-06-10T11:00:00+00:00", "end": "2026-06-10T12:00:00+00:00", "import_per_kwh": 0.1},
            {"start": "2026-06-10T10:00:00+00:00", "end": "2026-06-10T10:30:00+00:00", "import_per_kwh": 0.2}
        ]);
        assert!(
            matches!(parse_slots(&unsorted, None), Err(SchedulerError::Config(m)) if m.contains("sorted"))
        );
    }

    #[test]
    fn resample_fills_gaps_and_overrides_current_step() {
        // Canonical fixture: 18:00-19:00 covered, GAP 19:00-20:00, 20:00-21:00.
        let body = read_fixture("forecast_canonical.json");
        let slots = parse_slots(&body, None).unwrap();
        let g = Grid::build(sydney(2026, 6, 10, 18, 7), 30, 3).unwrap(); // 18:00..21:00
        let s = resample(&slots, &g, Some(0.999), Some(0.06));
        // Step 0 (18:00) overridden by price_now.
        assert_eq!(s.import[0], Some(0.999));
        assert_eq!(s.import[1], Some(0.28)); // 18:30 slot
                                             // 19:00 + 19:30 gap -> flat fill from last slot (0.28).
        assert_eq!(s.import[2], Some(0.28));
        assert_eq!(s.import[3], Some(0.28));
        assert_eq!(s.import[4], Some(0.18)); // 20:00 slot
                                             // feedin: slot export at 18:00 = 0.08; absent in 18:30 slot -> carries
                                             // last known (0.08); 20:00 slot -> 0.04.
        assert_eq!(s.feedin[0], 0.08);
        assert_eq!(s.feedin[1], 0.08);
        assert_eq!(s.feedin[4], 0.04);
    }

    #[test]
    fn missing_forecast_is_flat_current_and_unknown_without_current() {
        let g = Grid::build(sydney(2026, 6, 10, 18, 0), 30, 2).unwrap();
        let s = resample(&[], &g, Some(0.25), None);
        assert!(s.import.iter().all(|p| *p == Some(0.25)));
        assert!(s.feedin.iter().all(|f| *f == 0.0));
        let s = resample(&[], &g, None, None);
        assert!(s.import.iter().all(|p| p.is_none()));
    }

    fn u(h: u32, m: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-06-10T{h:02}:{m:02}:00+00:00"))
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn feedin_forecast_resamples_to_a_varying_series() {
        // A dedicated feed-in (export) forecast lives on its OWN provider sensor
        // (Amber publishes feed-in separately from the general/import forecast),
        // so each slot's value IS the export price. Before this, feed-in could
        // only carry the single CURRENT value forward — a flat line on the panel.
        let slots = vec![
            Slot { start: u(0, 0), end: u(0, 30), import_per_kwh: 0.06, export_per_kwh: None },
            Slot { start: u(0, 30), end: u(1, 0), import_per_kwh: 0.09, export_per_kwh: None },
            Slot { start: u(1, 0), end: u(1, 30), import_per_kwh: 0.03, export_per_kwh: None },
        ];
        // 10:00 Sydney == 00:00 UTC; 15-min grid, 2 h -> 8 steps.
        let g = Grid::build(sydney(2026, 6, 10, 10, 0), 15, 2).unwrap();
        let f = resample_feedin(&slots, &g, Some(0.05));
        assert_eq!(f.len(), g.steps.len());
        assert_eq!(f[0], 0.05, "current step overridden by feedin_now");
        assert_eq!(f[1], 0.06, "00:15 still inside the first slot");
        assert_eq!(f[2], 0.09, "00:30 second slot");
        assert_eq!(f[4], 0.03, "01:00 third slot");
        assert_eq!(f[7], 0.03, "beyond the last slot flat-fills the last known");
        assert!(f.windows(2).any(|w| w[0] != w[1]), "feed-in is NOT a flat line: {f:?}");
    }

    #[test]
    fn feedin_forecast_without_current_or_slots_is_flat_zero() {
        let g = Grid::build(sydney(2026, 6, 10, 10, 0), 30, 2).unwrap();
        let f = resample_feedin(&[], &g, None);
        assert!(f.iter().all(|v| *v == 0.0), "no slots, no current -> 0.0");
    }
}
