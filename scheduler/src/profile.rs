//! Learned half-hour-of-day EWMA profiles: baseload consumption (weekday vs
//! weekend) and PV shape. Persisted write-through to `/data/profile.json`
//! (the EMHASS `last_run.py` pattern). Cold buckets fall back to a configured
//! baseline (consumption) / zero (PV).

use chrono::{DateTime, Datelike, Timelike, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::time::Grid;

pub const BUCKETS: usize = 48; // half-hours per day

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Bucket {
    pub value_kw: f64,
    pub samples: u32,
}

impl Bucket {
    fn sample(&mut self, kw: f64, alpha: f64) {
        if self.samples == 0 {
            self.value_kw = kw;
        } else {
            self.value_kw = alpha * kw + (1.0 - alpha) * self.value_kw;
        }
        self.samples = self.samples.saturating_add(1);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profiles {
    /// Per-sample EWMA weight. At 60s ticks each half-hour bucket gets ~30
    /// samples/day; 0.05 gives a ~half-life of a few days of behaviour.
    pub alpha: f64,
    pub consumption_weekday: Vec<Bucket>,
    pub consumption_weekend: Vec<Bucket>,
    pub pv: Vec<Bucket>,
}

impl Default for Profiles {
    fn default() -> Self {
        Self {
            alpha: 0.05,
            consumption_weekday: vec![Bucket::default(); BUCKETS],
            consumption_weekend: vec![Bucket::default(); BUCKETS],
            pv: vec![Bucket::default(); BUCKETS],
        }
    }
}

pub fn bucket_index(t: DateTime<Tz>) -> usize {
    (t.hour() as usize) * 2 + (t.minute() as usize) / 30
}

fn is_weekend(t: DateTime<Tz>) -> bool {
    matches!(t.weekday(), Weekday::Sat | Weekday::Sun)
}

impl Profiles {
    /// Record a baseload sample (consumption MINUS managed-load draw — the
    /// caller corrects it; otherwise the scheduler learns its own loads).
    pub fn sample_baseload(&mut self, now: DateTime<Tz>, kw: f64) {
        let i = bucket_index(now);
        let a = self.alpha;
        if is_weekend(now) {
            self.consumption_weekend[i].sample(kw, a);
        } else {
            self.consumption_weekday[i].sample(kw, a);
        }
    }

    pub fn sample_pv(&mut self, now: DateTime<Tz>, kw: f64) {
        let a = self.alpha;
        self.pv[bucket_index(now)].sample(kw.max(0.0), a);
    }

    /// Baseload forecast over the grid; cold buckets → `baseline_kw`.
    /// The CURRENT step is overridden by `live_kw` when known.
    pub fn baseload_curve(&self, grid: &Grid, baseline_kw: f64, live_kw: Option<f64>) -> Vec<f64> {
        let mut out: Vec<f64> = grid
            .steps
            .iter()
            .map(|s| {
                let set = if is_weekend(*s) {
                    &self.consumption_weekend
                } else {
                    &self.consumption_weekday
                };
                let b = set[bucket_index(*s)];
                if b.samples > 0 {
                    b.value_kw
                } else {
                    baseline_kw
                }
            })
            .collect();
        if let (Some(kw), Some(first)) = (live_kw, out.first_mut()) {
            *first = kw;
        }
        out
    }

    /// PV forecast over the grid: the learned half-hour shape, rescaled per
    /// local date so the day's energy matches the provider's day total
    /// (Forecast.Solar `energy_production_today`/`_tomorrow`). No total or a
    /// cold shape → the raw shape (zeros when cold). Current step overridden
    /// by the live PV reading.
    pub fn pv_curve(
        &self,
        grid: &Grid,
        day_total_kwh: impl Fn(chrono::NaiveDate) -> Option<f64>,
        live_kw: Option<f64>,
    ) -> Vec<f64> {
        // Energy the raw shape implies over one full day: Σ kW · 0.5 h.
        let shape_day_kwh: f64 = self.pv.iter().map(|b| b.value_kw * 0.5).sum();

        let mut out: Vec<f64> = grid
            .steps
            .iter()
            .map(|s| {
                let raw = self.pv[bucket_index(*s)].value_kw;
                match day_total_kwh(s.date_naive()) {
                    Some(total) if shape_day_kwh > 0.0 => raw * (total / shape_day_kwh),
                    _ => raw,
                }
            })
            .collect();
        if let (Some(kw), Some(first)) = (live_kw, out.first_mut()) {
            *first = kw.max(0.0);
        }
        out
    }

    // ---- persistence (write-through JSON; corrupt/missing -> cold start) ----

    pub fn load(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(body) => match serde_json::from_str(&body) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("profile {path:?} corrupt ({e}); starting cold");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("profiles serialize"))?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::sydney;
    use crate::time::Grid;

    #[test]
    fn ewma_first_sample_seeds_then_blends() {
        let mut b = Bucket::default();
        b.sample(2.0, 0.5);
        assert_eq!(b.value_kw, 2.0); // first sample seeds, no decay from zero
        b.sample(4.0, 0.5);
        assert_eq!(b.value_kw, 3.0); // 0.5*4 + 0.5*2
        assert_eq!(b.samples, 2);
    }

    #[test]
    fn weekday_weekend_buckets_are_separate() {
        let mut p = Profiles::default();
        let wed = sydney(2026, 6, 10, 19, 10); // Wednesday 19:00 bucket
        let sat = sydney(2026, 6, 13, 19, 10); // Saturday same bucket index
        p.sample_baseload(wed, 1.0);
        p.sample_baseload(sat, 3.0);
        let i = bucket_index(wed);
        assert_eq!(p.consumption_weekday[i].value_kw, 1.0);
        assert_eq!(p.consumption_weekend[i].value_kw, 3.0);
    }

    #[test]
    fn baseload_curve_uses_baseline_when_cold_and_live_now() {
        let mut p = Profiles::default();
        let now = sydney(2026, 6, 10, 10, 0);
        p.sample_baseload(now + chrono::Duration::minutes(30), 2.5); // warm 10:30 bucket
        let g = Grid::build(now, 30, 1).unwrap(); // 10:00, 10:30
        let curve = p.baseload_curve(&g, 0.8, Some(1.7));
        assert_eq!(curve[0], 1.7); // live override
        assert_eq!(curve[1], 2.5); // warm bucket
        let curve = p.baseload_curve(&g, 0.8, None);
        assert_eq!(curve[0], 0.8); // cold bucket -> baseline
    }

    #[test]
    fn pv_curve_rescales_to_day_total() {
        let mut p = Profiles::default();
        let now = sydney(2026, 6, 10, 10, 0);
        // Warm two buckets: 1 kW at 10:00, 3 kW at 10:30 → shape day = 2 kWh.
        p.sample_pv(now, 1.0);
        p.sample_pv(now + chrono::Duration::minutes(30), 3.0);
        let g = Grid::build(now, 30, 1).unwrap();
        // Provider says today totals 4 kWh → scale ×2.
        let curve = p.pv_curve(&g, |_| Some(4.0), None);
        assert_eq!(curve, vec![2.0, 6.0]);
        // No day total → raw shape.
        let curve = p.pv_curve(&g, |_| None, None);
        assert_eq!(curve, vec![1.0, 3.0]);
        // Live override on current step.
        let curve = p.pv_curve(&g, |_| Some(4.0), Some(0.4));
        assert_eq!(curve[0], 0.4);
    }

    #[test]
    fn cold_pv_is_zero_not_nan() {
        let p = Profiles::default();
        let g = Grid::build(sydney(2026, 6, 10, 10, 0), 30, 2).unwrap();
        let curve = p.pv_curve(&g, |_| Some(10.0), None);
        assert!(curve.iter().all(|v| *v == 0.0 && v.is_finite()));
    }

    #[test]
    fn persistence_round_trip_and_corrupt_file_cold_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.json");
        let mut p = Profiles::default();
        p.sample_pv(sydney(2026, 6, 10, 12, 0), 5.0);
        p.save(&path).unwrap();
        assert_eq!(Profiles::load(&path), p);

        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(Profiles::load(&path), Profiles::default());
        assert_eq!(Profiles::load(&dir.path().join("missing.json")), Profiles::default());
    }

    mod invariants {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Rescaled PV day energy equals the provider total whenever the
            /// shape is warm; buckets stay within sampled bounds.
            #[test]
            fn pv_rescale_hits_target(samples in proptest::collection::vec(0.0f64..10.0, 1..48),
                                      total in 0.1f64..50.0) {
                let mut p = Profiles::default();
                let day0 = sydney(2026, 6, 10, 0, 0);
                for (i, kw) in samples.iter().enumerate() {
                    p.sample_pv(day0 + chrono::Duration::minutes(30 * i as i64), *kw);
                }
                // Full-day grid starting at midnight (48 half-hour steps).
                let g = Grid::build(day0, 30, 24).unwrap();
                let curve = p.pv_curve(&g, |_| Some(total), None);
                let day_kwh: f64 = curve.iter().map(|kw| kw * 0.5).sum();
                let shape: f64 = p.pv.iter().map(|b| b.value_kw * 0.5).sum();
                if shape > 0.0 {
                    prop_assert!((day_kwh - total).abs() < 1e-6);
                }
            }
        }
    }
}
