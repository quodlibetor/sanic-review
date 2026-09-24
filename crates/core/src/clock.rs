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

/// Parses a time as [`rfc3339`] writes it, or as the store does, with
/// fractional seconds. `None` for anything else.
#[must_use]
pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<u64>);
    let (y, m, d) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let time = time.split('.').next()?;
    let mut time = time.splitn(3, ':').map(str::parse::<u64>);
    let (h, min, sec) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    let secs = days_from_civil(y, m, d)? * 86_400 + h * 3600 + min * 60 + sec;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// How long before `now` `then` was, in its largest unit, e.g. `3h` or
/// `2d`; `now` for under a minute or in the future.
#[must_use]
pub fn ago(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        86_400..1_209_600 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

/// A recency window picked on the dashboard, in place of
/// `poll.updated_within_days` until it's reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowChoice {
    Days(u32),
    /// No limit.
    All,
}

impl WindowChoice {
    /// The window it sets: `None` for no limit.
    #[must_use]
    pub fn days(self) -> Option<u32> {
        match self {
            Self::Days(days) => Some(days),
            Self::All => None,
        }
    }

    /// As it's stored and sent in forms: a day count or `all`.
    #[must_use]
    pub fn as_str(self) -> String {
        match self {
            Self::Days(days) => days.to_string(),
            Self::All => "all".into(),
        }
    }

    /// The inverse of [`WindowChoice::as_str`]. A window of no days is no
    /// window, as `updated_within_days = 0` is.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "all" | "0" => Some(Self::All),
            days => days.parse().ok().map(Self::Days),
        }
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date, after Howard
/// Hinnant's `days_from_civil`; `None` before 1970.
fn days_from_civil(y: u64, m: u64, d: u64) -> Option<u64> {
    let y = if m <= 2 { y.checked_sub(1)? } else { y };
    let era = y / 400;
    let yoe = y % 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe).checked_sub(719_468)
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
    fn parses_what_it_formats_and_the_stores_times() {
        for secs in [0, 1_709_164_800, 1_790_181_231] {
            assert_eq!(parse_rfc3339(&rfc3339(at(secs))), Some(at(secs)));
        }
        assert_eq!(
            parse_rfc3339("2026-09-23T16:33:51.123Z"),
            Some(at(1_790_181_231))
        );
        for bad in [
            "",
            "2026-09-23",
            "2026-13-01T00:00:00Z",
            "1969-12-31T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn ago_is_the_largest_unit() {
        let now = at(10_000_000);
        let before = |secs: u64| ago(now, at(10_000_000 - secs));
        assert_eq!(before(59), "now");
        assert_eq!(before(61), "1m");
        assert_eq!(before(3 * 3600 + 5), "3h");
        assert_eq!(before(2 * 86_400), "2d");
        assert_eq!(before(15 * 86_400), "2w");
        assert_eq!(ago(at(0), at(5)), "now");
    }

    #[test]
    fn window_choices_round_trip() {
        for choice in [WindowChoice::Days(30), WindowChoice::All] {
            assert_eq!(WindowChoice::parse(&choice.as_str()), Some(choice));
        }
        assert_eq!(WindowChoice::parse("0"), Some(WindowChoice::All));
        assert_eq!(WindowChoice::parse("soon"), None);
        assert_eq!(WindowChoice::Days(30).days(), Some(30));
        assert_eq!(WindowChoice::All.days(), None);
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
