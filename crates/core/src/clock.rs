//! Wall-clock time, behind a trait so tests can fake it, and the UTC
//! formats GitHub uses.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ`, as GitHub writes timestamps. Values in this
/// format compare correctly as strings.
#[must_use]
pub fn rfc3339(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (y, m, d) = civil_from_days(secs / 86_400);
    let s = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// The start of the window of `days` before `now`, in [`rfc3339`] form.
/// `None` for no window.
#[must_use]
pub fn window_start(now: SystemTime, days: Option<u32>) -> Option<String> {
    let days = days?;
    let start = now
        .checked_sub(Duration::from_secs(u64::from(days) * 86_400))
        .unwrap_or(UNIX_EPOCH);
    Some(rfc3339(start))
}

/// The proleptic Gregorian date of a day count since 1970-01-01, after
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn formats_utc_like_github() {
        assert_eq!(rfc3339(at(0)), "1970-01-01T00:00:00Z");
        // 2024-02-29, a leap day, and 2026-09-23 16:33:51.
        assert_eq!(rfc3339(at(1_709_164_800)), "2024-02-29T00:00:00Z");
        assert_eq!(rfc3339(at(1_790_181_231)), "2026-09-23T16:33:51Z");
    }

    #[test]
    fn windows_count_back_whole_days() {
        let now = at(1_790_181_231);
        assert_eq!(
            window_start(now, Some(14)).as_deref(),
            Some("2026-09-09T16:33:51Z")
        );
        assert_eq!(window_start(now, None), None);
    }
}
