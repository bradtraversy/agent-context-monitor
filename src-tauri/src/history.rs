use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::model::{LimitWindow, ProviderUsage, UsageStatus, same_reset, unix_secs};

const KEEP_SECS: i64 = 120 * DAY;
const DAY: i64 = 24 * 60 * 60;
/// A smaller drop in weekly usage is rounding, not a reset.
const RESET_DROP_PERCENT: f64 = 5.0;

/// One fresh usage reading, stored as a line of JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Sample {
    t: i64,
    p: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    s: Option<f64>,
    w: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    r: Option<i64>,
}

impl Sample {
    fn same_reading(&self, other: &Sample) -> bool {
        self.s == other.s && self.w == other.w && same_reset(self.r, other.r)
    }
}

/// Appends a sample only when a provider's fresh reading changes, so the file stays small.
pub struct Recorder {
    path: PathBuf,
    last: HashMap<String, Sample>,
}

impl Recorder {
    pub fn open(path: PathBuf, now: SystemTime) -> Self {
        let mut samples = read_samples(&path);
        let cutoff = unix_secs(now) - KEEP_SECS;
        let before = samples.len();
        samples.retain(|s| s.t >= cutoff);
        if samples.len() < before {
            let text: String = samples.iter().map(line).collect();
            let _ = crate::files::replace(&path, text.as_bytes());
        }
        let mut last = HashMap::new();
        for s in samples {
            last.insert(s.p.clone(), s);
        }
        Recorder { path, last }
    }

    pub fn record(&mut self, usage: &[ProviderUsage], now: SystemTime) {
        for u in usage {
            let Some(sample) = sample_from(u, now) else {
                continue;
            };
            if let Some(prev) = self.last.get(&sample.p)
                && (sample.t <= prev.t || sample.same_reading(prev))
            {
                continue;
            }
            if let Some(dir) = self.path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let written = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .and_then(|mut f| f.write_all(line(&sample).as_bytes()));
            if written.is_ok() {
                self.last.insert(sample.p.clone(), sample);
            }
        }
    }
}

fn sample_from(u: &ProviderUsage, now: SystemTime) -> Option<Sample> {
    if u.stale || u.status != UsageStatus::Available {
        return None;
    }
    let Some(&LimitWindow::Reported {
        used_percent: w,
        resets_at: r,
    }) = main_limit(u)
    else {
        return None;
    };
    let s = match u.limit("session") {
        Some(LimitWindow::Reported { used_percent, .. }) => Some(*used_percent),
        _ => None,
    };
    let t = u
        .observed_at
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp())
        .unwrap_or_else(|| unix_secs(now));
    Some(Sample {
        t,
        p: u.provider.to_string(),
        s,
        w,
        r,
    })
}

/// The limit the History tab charts: the weekly one, or the monthly one for plans billed
/// by the month.
fn main_limit(u: &ProviderUsage) -> Option<&LimitWindow> {
    u.limit("weekly").or_else(|| u.limit("monthly"))
}

/// How long a provider's charted limit lasts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Week,
    Month,
}

impl Period {
    pub fn of(u: &ProviderUsage) -> Period {
        if u.limit("weekly").is_none() && u.limit("monthly").is_some() {
            Period::Month
        } else {
            Period::Week
        }
    }
}

fn line(sample: &Sample) -> String {
    serde_json::to_string(sample).unwrap_or_default() + "\n"
}

fn read_samples(path: &PathBuf) -> Vec<Sample> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut samples: Vec<Sample> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    samples.sort_by_key(|s| s.t);
    samples
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Bar {
    /// Unix seconds when the day or week began.
    pub start: i64,
    /// Percent of the weekly limit; `None` before history began.
    pub used: Option<f64>,
    /// The day or week is still in progress.
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderHistory {
    pub provider: String,
    pub name: String,
    pub period: Period,
    pub daily: Vec<Bar>,
    pub weekly: Vec<Bar>,
}

pub fn load(
    path: &PathBuf,
    providers: &[(String, String, Period)],
    now: SystemTime,
    offset_at: &dyn Fn(i64) -> i64,
    days: usize,
    weeks: usize,
) -> Vec<ProviderHistory> {
    // Nothing older than the retention window is kept, so asking for more shows nothing new.
    let kept_days = (KEEP_SECS / DAY) as usize;
    let days = days.clamp(1, kept_days);
    let weeks = weeks.clamp(1, kept_days.div_ceil(7) + 1);
    let samples = read_samples(path);
    providers
        .iter()
        .map(|(provider, name, period)| {
            let mine: Vec<&Sample> = samples.iter().filter(|s| &s.p == provider).collect();
            let weekly = match period {
                Period::Week => weekly(&mine, unix_secs(now), weeks),
                Period::Month => week_sums(&daily(&mine, unix_secs(now), offset_at, weeks * 7)),
            };
            ProviderHistory {
                provider: provider.clone(),
                name: name.clone(),
                period: *period,
                daily: daily(&mine, unix_secs(now), offset_at, days),
                weekly,
            }
        })
        .collect()
}

/// Share of the weekly limit used on each local day, from the rise between readings.
/// `offset_at` gives the local UTC offset at a time, which daylight saving changes.
fn daily(samples: &[&Sample], now: i64, offset_at: &dyn Fn(i64) -> i64, days: usize) -> Vec<Bar> {
    let day_of = |t: i64| (t + offset_at(t)).div_euclid(DAY);
    let today = day_of(now);
    let first_day = samples.first().map(|s| day_of(s.t));
    let mut used: HashMap<i64, f64> = HashMap::new();
    for pair in samples.windows(2) {
        let (prev, cur) = (pair[0], pair[1]);
        let reset = !same_reset(prev.r, cur.r) || cur.w < prev.w - RESET_DROP_PERCENT;
        let rise = if reset {
            cur.w
        } else {
            (cur.w - prev.w).max(0.0)
        };
        *used.entry(day_of(cur.t)).or_default() += rise;
    }
    (today - days as i64 + 1..=today)
        .map(|day| Bar {
            start: day * DAY - offset_at(day * DAY),
            used: first_day
                .filter(|first| day >= *first)
                .map(|_| used.get(&day).copied().unwrap_or(0.0)),
            current: day == today,
        })
        .collect()
}

/// A monthly limit has no weekly windows, so each week shows the share of the monthly limit
/// used over seven days, ending today.
fn week_sums(days: &[Bar]) -> Vec<Bar> {
    let mut weeks: Vec<Bar> = days
        .rchunks(7)
        .map(|week| {
            let known: Vec<f64> = week.iter().filter_map(|b| b.used).collect();
            Bar {
                start: week[0].start,
                used: (!known.is_empty()).then(|| known.iter().sum()),
                current: week.iter().any(|b| b.current),
            }
        })
        .collect();
    weeks.reverse();
    weeks
}

/// Highest weekly usage reached in each weekly window, keyed by its reset time.
fn weekly(samples: &[&Sample], now: i64, shown: usize) -> Vec<Bar> {
    let mut weeks: Vec<(i64, f64)> = Vec::new();
    for s in samples {
        let Some(reset) = s.r else { continue };
        match weeks.last_mut() {
            Some((r, peak)) if same_reset(Some(*r), Some(reset)) => *peak = peak.max(s.w),
            _ => weeks.push((reset, s.w)),
        }
    }
    let skip = weeks.len().saturating_sub(shown);
    weeks
        .into_iter()
        .skip(skip)
        .map(|(reset, peak)| Bar {
            start: reset - 7 * DAY,
            used: Some(peak),
            current: reset > now,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    const NOW: i64 = 1_792_800_000; // 2026-10-24T00:00:00Z, a Saturday

    fn sample(t: i64, w: f64, r: i64) -> Sample {
        Sample {
            t,
            p: "claude".into(),
            s: None,
            w,
            r: Some(r),
        }
    }

    fn usage(observed: i64, weekly: f64) -> ProviderUsage {
        let mut u = ProviderUsage::unknown("claude", "Claude", "");
        u.status = UsageStatus::Available;
        u.observed_at = chrono::DateTime::from_timestamp(observed, 0).map(|t| t.to_rfc3339());
        u.limits = vec![crate::model::Limit::weekly(LimitWindow::Reported {
            used_percent: weekly,
            resets_at: Some(NOW + DAY),
        })];
        u
    }

    #[test]
    fn records_only_fresh_changed_readings_and_survives_restart() {
        let dir = std::env::temp_dir().join(format!("acm-history-{}", std::process::id()));
        let path = dir.join("usage-history.jsonl");
        let _ = fs::remove_file(&path);
        let now = UNIX_EPOCH + Duration::from_secs(NOW as u64);

        let mut recorder = Recorder::open(path.clone(), now);
        recorder.record(&[usage(NOW - 600, 10.0)], now);
        recorder.record(&[usage(NOW - 300, 10.0)], now);
        let mut stale = usage(NOW - 200, 50.0);
        stale.stale = true;
        recorder.record(&[stale], now);
        recorder.record(&[usage(NOW - 100, 12.0)], now);

        let mut reopened = Recorder::open(path.clone(), now);
        reopened.record(&[usage(NOW - 100, 12.0)], now);
        let weekly: Vec<f64> = read_samples(&path).iter().map(|s| s.w).collect();
        assert_eq!(weekly, vec![10.0, 12.0]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn daily_bars_add_up_rises_and_count_usage_after_a_reset() {
        let reset = NOW + 2 * DAY;
        let next_reset = reset + 7 * DAY;
        let samples = [
            sample(NOW - 2 * DAY + 3600, 20.0, reset),
            sample(NOW - 2 * DAY + 7200, 25.0, reset),
            sample(NOW - DAY + 3600, 24.5, reset + 30),
            sample(NOW - DAY + 7200, 31.0, reset),
            sample(NOW + 30, 4.0, next_reset),
        ];
        let refs: Vec<&Sample> = samples.iter().collect();
        let bars = daily(&refs, NOW + 60, &|_| 0, 14);
        assert_eq!(bars.len(), 14);
        let used: Vec<Option<f64>> = bars.iter().rev().take(4).map(|b| b.used).collect();
        assert_eq!(used, vec![Some(4.0), Some(6.5), Some(5.0), None]);
        assert!(bars.last().unwrap().current);
        assert_eq!(bars.last().unwrap().start, NOW);
    }

    #[test]
    fn local_offset_moves_late_evening_usage_to_that_day() {
        let reset = NOW + 3 * DAY;
        let samples = [
            sample(NOW - 7200, 10.0, reset),
            sample(NOW - 3600, 15.0, reset),
        ];
        let refs: Vec<&Sample> = samples.iter().collect();
        let utc = daily(&refs, NOW + 3600, &|_| 0, 14);
        let new_york = daily(&refs, NOW + 3600, &|_| -4 * 3600, 14);
        assert_eq!(utc[12].used, Some(5.0));
        assert_eq!(new_york[13].used, Some(5.0));
    }

    #[test]
    fn days_before_a_clock_change_keep_their_own_offset() {
        // New York moves from -4h to -5h a day before NOW.
        let switch = NOW - DAY;
        let offset_at = |t: i64| if t < switch { -4 * 3600 } else { -5 * 3600 };
        let reset = NOW + 3 * DAY;
        // 00:30 local, two days before NOW, while still on -4h.
        let early = NOW - 2 * DAY + 4 * 3600 + 1800;
        let samples = [
            sample(early - 7200, 10.0, reset),
            sample(early, 15.0, reset),
        ];
        let refs: Vec<&Sample> = samples.iter().collect();
        let bars = daily(&refs, NOW, &offset_at, 14);
        let day = bars.iter().find(|b| b.used == Some(5.0)).unwrap();
        assert_eq!(day.start, NOW - 2 * DAY + 4 * 3600);
    }

    #[test]
    fn weekly_bars_keep_each_windows_peak() {
        let first = NOW - 3 * DAY;
        let second = first + 7 * DAY;
        let samples = [
            sample(first - DAY, 70.0, first),
            sample(first - 3600, 82.0, first + 20),
            sample(first + 3600, 3.0, second),
            sample(NOW - 60, 40.0, second),
        ];
        let refs: Vec<&Sample> = samples.iter().collect();
        let bars = weekly(&refs, NOW, 8);
        let peaks: Vec<(Option<f64>, bool)> = bars.iter().map(|b| (b.used, b.current)).collect();
        assert_eq!(peaks, vec![(Some(82.0), false), (Some(40.0), true)]);
    }

    #[test]
    fn a_short_daily_range_still_gets_every_week_asked_for() {
        let dir = std::env::temp_dir().join(format!("acm-history-weeks-{}", std::process::id()));
        let path = dir.join("usage-history.jsonl");
        fs::create_dir_all(&dir).unwrap();
        let text: String = (0..10)
            .map(|week| sample(NOW - week * 7 * DAY, 50.0, NOW - week * 7 * DAY + DAY))
            .map(|s| line(&s))
            .collect();
        fs::write(&path, text).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(NOW as u64);
        let providers = [("claude".to_string(), "Claude".to_string(), Period::Week)];
        let history = load(&path, &providers, now, &|_| 0, 14, 8);
        assert_eq!(history[0].daily.len(), 14);
        assert_eq!(history[0].weekly.len(), 8);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn monthly_limits_add_up_each_seven_days() {
        let day = |i: i64, used| Bar {
            start: i * DAY,
            used,
            current: i == 13,
        };
        let days: Vec<Bar> = (0..14)
            .map(|i| day(i, if i < 3 { None } else { Some(1.0) }))
            .collect();
        let weeks = week_sums(&days);
        assert_eq!(weeks.len(), 2);
        assert_eq!(weeks[0].start, 0);
        assert_eq!(weeks[0].used, Some(4.0));
        assert!(!weeks[0].current);
        assert_eq!(weeks[1].start, 7 * DAY);
        assert_eq!(weeks[1].used, Some(7.0));
        assert!(weeks[1].current);
    }
}
