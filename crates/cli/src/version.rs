//! The `--version` string: the crate version plus the commit `build.rs`
//! found, e.g. `0.1.0 (580b3cb1, 2026-09-24 18:31 UTC)`.

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sanic_core::clock::rfc3339;

pub static VERSION: LazyLock<String> = LazyLock::new(|| {
    format(
        env!("CARGO_PKG_VERSION"),
        option_env!("SANIC_GIT_SHA"),
        option_env!("SANIC_GIT_TIME")
            .and_then(|t| t.parse().ok())
            .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs))),
        option_env!("SANIC_GIT_DIRTY") == Some("true"),
    )
});

fn format(pkg: &str, sha: Option<&str>, time: Option<SystemTime>, dirty: bool) -> String {
    let Some(sha) = sha else {
        return format!("{pkg} (unknown commit)");
    };
    let dirty = if dirty { "-dirty" } else { "" };
    match time {
        Some(time) => format!("{pkg} ({sha}{dirty}, {})", utc(time)),
        None => format!("{pkg} ({sha}{dirty})"),
    }
}

/// `YYYY-MM-DD HH:MM UTC`, as the dashboard shows run times.
fn utc(time: SystemTime) -> String {
    let at = rfc3339(time);
    format!("{} UTC", at.get(..16).unwrap_or(&at).replace('T', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn utc_formats_epoch_and_known_times() {
        assert_eq!(utc(at(0)), "1970-01-01 00:00 UTC");
        assert_eq!(utc(at(951_782_400)), "2000-02-29 00:00 UTC");
        assert_eq!(utc(at(1_790_274_660)), "2026-09-24 18:31 UTC");
    }

    #[test]
    fn format_covers_clean_dirty_and_unknown() {
        let t = Some(at(1_790_274_660));
        assert_eq!(
            format("0.1.0", Some("580b3cb1"), t, false),
            "0.1.0 (580b3cb1, 2026-09-24 18:31 UTC)"
        );
        assert_eq!(
            format("0.1.0", Some("580b3cb1"), t, true),
            "0.1.0 (580b3cb1-dirty, 2026-09-24 18:31 UTC)"
        );
        assert_eq!(
            format("0.1.0", Some("580b3cb1"), None, false),
            "0.1.0 (580b3cb1)"
        );
        assert_eq!(format("0.1.0", None, t, true), "0.1.0 (unknown commit)");
    }
}
