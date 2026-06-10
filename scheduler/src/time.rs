//! Injected-now time helpers: windows (incl. overnight), wall-clock-anchored
//! grid, local midnight, conservative rounding.
//!
//! `now` is always passed in — core modules never read the wall clock. That is
//! what makes the whole pipeline testable with fixed clocks.

use std::time::Duration;

use chrono::{DateTime, NaiveTime, TimeZone, Timelike};
use chrono_tz::Tz;

use crate::error::SchedulerError;
use crate::model::Window;

/// Is local time `t` inside window `w`?
/// `end < start` crosses midnight; `start == end` means the full day.
pub fn in_window(t: NaiveTime, w: &Window) -> bool {
    if w.start == w.end {
        true
    } else if w.start < w.end {
        t >= w.start && t < w.end
    } else {
        t >= w.start || t < w.end
    }
}

/// The instant of local midnight on `now`'s local date.
/// On days where 00:00 is skipped/ambiguous (DST corner), takes the earliest
/// valid instant.
pub fn local_midnight(now: DateTime<Tz>) -> DateTime<Tz> {
    let tz = now.timezone();
    let date = now.date_naive();
    // earliest() handles ambiguity; walk forward through a (theoretical) gap.
    for minutes in 0..=120 {
        let naive = date.and_time(NaiveTime::MIN) + chrono::Duration::minutes(minutes);
        if let Some(dt) = tz.from_local_datetime(&naive).earliest() {
            return dt;
        }
    }
    unreachable!("no valid local time within 2h of midnight");
}

/// The planning grid: wall-clock-anchored steps of `step_minutes`, starting at
/// the step containing `now`, covering `horizon_hours`.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub step_minutes: u32,
    /// Step start instants, strictly increasing, `steps[0]` contains `now`.
    pub steps: Vec<DateTime<Tz>>,
}

impl Grid {
    /// Build the grid. `step_minutes` must divide 60 so steps stay anchored to
    /// wall-clock quarter/half hours across DST shifts (which are whole hours).
    pub fn build(
        now: DateTime<Tz>,
        step_minutes: u32,
        horizon_hours: u32,
    ) -> Result<Self, SchedulerError> {
        if step_minutes == 0 || 60 % step_minutes != 0 {
            return Err(SchedulerError::Config(format!(
                "grid_minutes must divide 60, got {step_minutes}"
            )));
        }
        // Anchor: floor `now` to a step boundary in local wall time. Because
        // step_minutes divides 60, flooring minutes is enough.
        let floored_min = now.minute() - now.minute() % step_minutes;
        let anchor = now
            .with_minute(floored_min)
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .expect("flooring minutes/seconds is always valid");

        // Walk in ABSOLUTE time: monotone by construction, DST-safe. Local
        // labels stay on step boundaries because DST shifts are whole hours.
        let count = (u64::from(horizon_hours) * 60 / u64::from(step_minutes)) as usize;
        let step = chrono::Duration::minutes(i64::from(step_minutes));
        let steps = (0..count).map(|k| anchor + step * k as i32).collect();
        Ok(Self { step_minutes, steps })
    }

    pub fn step_duration(&self) -> Duration {
        Duration::from_secs(u64::from(self.step_minutes) * 60)
    }
}

/// Conservative rounding: how many whole grid steps cover `d` (round UP).
/// Used for min_run/min_off (never under-enforce) and required runtime
/// (never under-deliver).
pub fn round_up_to_steps(d: Duration, step_minutes: u32) -> u32 {
    let step_secs = u64::from(step_minutes) * 60;
    d.as_secs().div_ceil(step_secs) as u32
}

/// A window instance: a run of consecutive grid steps inside the window,
/// tagged with the local date its first step falls on (daily windows recur
/// across a >24h or midnight-crossing horizon).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInstance {
    /// Local date of the instance's first in-window step.
    pub date: chrono::NaiveDate,
    /// Step index range [start, end) into `Grid::steps`.
    pub steps: std::ops::Range<usize>,
}

/// Enumerate the window's instances over the grid: maximal runs of consecutive
/// steps whose local start time lies inside `w`.
pub fn window_instances(w: &Window, grid: &Grid) -> Vec<WindowInstance> {
    let mut out: Vec<WindowInstance> = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, t) in grid.steps.iter().enumerate() {
        if in_window(t.time(), w) {
            run_start.get_or_insert(i);
        } else if let Some(s) = run_start.take() {
            out.push(WindowInstance {
                date: grid.steps[s].date_naive(),
                steps: s..i,
            });
        }
    }
    if let Some(s) = run_start {
        out.push(WindowInstance {
            date: grid.steps[s].date_naive(),
            steps: s..grid.steps.len(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Australia::Sydney;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn w(sh: u32, sm: u32, eh: u32, em: u32) -> Window {
        Window { start: t(sh, sm), end: t(eh, em) }
    }

    fn sydney(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        Sydney.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn plain_window_membership() {
        let win = w(8, 0, 17, 0);
        assert!(in_window(t(9, 0), &win));
        assert!(in_window(t(8, 0), &win)); // start inclusive
        assert!(!in_window(t(17, 0), &win)); // end exclusive
        assert!(!in_window(t(7, 59), &win));
    }

    #[test]
    fn overnight_window_membership() {
        let win = w(22, 0, 6, 30);
        assert!(in_window(t(23, 0), &win));
        assert!(in_window(t(3, 0), &win));
        assert!(in_window(t(22, 0), &win)); // start inclusive
        assert!(!in_window(t(6, 30), &win)); // end exclusive
        assert!(!in_window(t(12, 0), &win));
    }

    #[test]
    fn equal_bounds_means_full_day() {
        let win = w(0, 0, 0, 0);
        assert!(in_window(t(0, 0), &win));
        assert!(in_window(t(12, 0), &win));
        assert!(in_window(t(23, 59), &win));
    }

    #[test]
    fn local_midnight_is_start_of_local_date() {
        let mid = local_midnight(sydney(2026, 6, 10, 15, 42));
        assert_eq!(mid, sydney(2026, 6, 10, 0, 0));
    }

    #[test]
    fn grid_anchors_to_wall_clock_and_covers_horizon() {
        let g = Grid::build(sydney(2026, 6, 10, 10, 7), 15, 24).unwrap();
        assert_eq!(g.steps.len(), 96);
        assert_eq!(g.steps[0], sydney(2026, 6, 10, 10, 0)); // floored, contains now
        assert!(g.steps.iter().all(|s| s.minute() % 15 == 0 && s.second() == 0));
    }

    #[test]
    fn grid_rejects_step_not_dividing_hour() {
        assert!(Grid::build(sydney(2026, 6, 10, 10, 0), 7, 24).is_err());
        assert!(Grid::build(sydney(2026, 6, 10, 10, 0), 0, 24).is_err());
    }

    #[test]
    fn grid_monotone_across_dst_start() {
        // Sydney DST starts 2026-10-04: 02:00 -> 03:00 (02:xx does not exist).
        let g = Grid::build(sydney(2026, 10, 4, 0, 30), 15, 24).unwrap();
        assert_eq!(g.steps.len(), 96);
        assert!(g.steps.windows(2).all(|p| p[0] < p[1]));
        // Local labels stay on quarter-hours even across the shift.
        assert!(g.steps.iter().all(|s| s.minute() % 15 == 0));
    }

    #[test]
    fn grid_monotone_across_dst_end() {
        // Sydney DST ends 2026-04-05: 03:00 -> 02:00 (02:xx happens twice).
        let g = Grid::build(sydney(2026, 4, 4, 23, 0), 15, 24).unwrap();
        assert!(g.steps.windows(2).all(|p| p[0] < p[1]));
    }

    #[test]
    fn round_up_is_conservative() {
        assert_eq!(round_up_to_steps(Duration::from_secs(20 * 60), 15), 2);
        assert_eq!(round_up_to_steps(Duration::from_secs(30 * 60), 15), 2);
        assert_eq!(round_up_to_steps(Duration::from_secs(60), 15), 1);
        assert_eq!(round_up_to_steps(Duration::ZERO, 15), 0);
    }

    #[test]
    fn window_instances_recur_across_midnight() {
        // Horizon from 22:00 for 24h; window 07:00-22:00 -> one instance
        // tomorrow (07:00..22:00 truncated at horizon end 22:00).
        let g = Grid::build(sydney(2026, 6, 10, 22, 0), 15, 24).unwrap();
        let inst = window_instances(&w(7, 0, 22, 0), &g);
        assert_eq!(inst.len(), 1);
        assert_eq!(inst[0].date, chrono::NaiveDate::from_ymd_opt(2026, 6, 11).unwrap());
        assert_eq!(g.steps[inst[0].steps.start], sydney(2026, 6, 11, 7, 0));

        // Window 00:00-06:30 from 22:00 -> exactly tomorrow's instance.
        let inst = window_instances(&w(0, 0, 6, 30), &g);
        assert_eq!(inst.len(), 1);
        assert_eq!(g.steps[inst[0].steps.start], sydney(2026, 6, 11, 0, 0));
        // 6.5h at 15min = 26 steps.
        assert_eq!(inst[0].steps.len(), 26);
    }

    #[test]
    fn window_instances_split_today_and_tomorrow() {
        // From 10:00, window 07:00-22:00 -> today's tail + tomorrow's head.
        let g = Grid::build(sydney(2026, 6, 10, 10, 0), 15, 24).unwrap();
        let inst = window_instances(&w(7, 0, 22, 0), &g);
        assert_eq!(inst.len(), 2);
        assert_eq!(inst[0].date, chrono::NaiveDate::from_ymd_opt(2026, 6, 10).unwrap());
        assert_eq!(inst[0].steps.start, 0); // already inside the window now
        assert_eq!(g.steps[inst[0].steps.end], sydney(2026, 6, 10, 22, 0));
        assert_eq!(inst[1].date, chrono::NaiveDate::from_ymd_opt(2026, 6, 11).unwrap());
        assert_eq!(g.steps[inst[1].steps.start], sydney(2026, 6, 11, 7, 0));
    }

    #[test]
    fn overnight_window_is_one_instance_across_midnight() {
        // 22:00-06:30 from 20:00: the run 22:00 -> 06:30 must be ONE instance
        // even though it crosses midnight (it's one continuous demand window).
        let g = Grid::build(sydney(2026, 6, 10, 20, 0), 15, 24).unwrap();
        let inst = window_instances(&w(22, 0, 6, 30), &g);
        assert_eq!(inst.len(), 1);
        assert_eq!(g.steps[inst[0].steps.start], sydney(2026, 6, 10, 22, 0));
        assert_eq!(inst[0].steps.len(), 34); // 8.5h = 34 steps
    }
}
