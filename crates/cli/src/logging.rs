//! Where tracing output goes: stdout for `--ui logs` and `setup`; the TUI's
//! log pane plus a log file for `--ui tui`, since writing to the terminal
//! would corrupt the screen.

use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::{self, IsTerminal},
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, WrapErr};
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    EnvFilter,
    fmt::{MakeWriter, format::Writer, time::FormatTime},
    prelude::*,
};

/// Lines kept for the log pane; older ones are only in the log file.
const KEPT_LINES: usize = 1000;

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Honour `NO_COLOR`, and keep escapes out of pipes and log files.
#[must_use]
pub fn color() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Logs to stdout.
pub fn init_stdout() {
    tracing_subscriber::registry()
        .with(filter())
        .with(tracing_subscriber::fmt::layer().with_ansi(color() && io::stdout().is_terminal()))
        .with(ErrorLayer::default())
        .init();
}

/// Logs to the returned buffer, for the TUI's log pane, and appends to
/// `file`.
pub fn init_tui(file: &Path) -> Result<LogLines> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .wrap_err_with(|| format!("opening log file {}", file.display()))?;
    let lines = LogLines::default();
    tracing_subscriber::registry()
        .with(filter())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_timer(ClockTime)
                .with_writer(lines.clone()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Arc::new(log)),
        )
        .with(ErrorLayer::default())
        .init();
    Ok(lines)
}

/// The most recent log lines, oldest first.
#[derive(Debug, Clone, Default)]
pub struct LogLines(Arc<Mutex<Kept>>);

#[derive(Debug, Default)]
struct Kept {
    lines: VecDeque<String>,
    /// Lines dropped from the front so far.
    dropped: u64,
}

impl LogLines {
    /// The kept lines, and how many older ones have been dropped, so a
    /// reader can keep its place as the front moves.
    #[must_use]
    pub fn snapshot(&self) -> (Vec<String>, u64) {
        let kept = self.lock();
        (kept.lines.iter().cloned().collect(), kept.dropped)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<'a> MakeWriter<'a> for LogLines {
    type Writer = EventWriter;

    fn make_writer(&'a self) -> EventWriter {
        EventWriter {
            buf: Vec::new(),
            lines: self.clone(),
        }
    }
}

/// Collects one formatted event and adds its lines when dropped.
pub struct EventWriter {
    buf: Vec<u8>,
    lines: LogLines,
}

impl io::Write for EventWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.buf);
        let mut kept = self.lines.lock();
        kept.lines.extend(text.lines().map(str::to_owned));
        let excess = kept.lines.len().saturating_sub(KEPT_LINES);
        kept.lines.drain(..excess);
        kept.dropped += excess as u64;
    }
}

/// `HH:MM:SS` in UTC: short enough for the log pane, and the same clock as
/// the activity pane's store timestamps.
struct ClockTime;

impl FormatTime for ClockTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            % 86_400;
        write!(
            w,
            "{:02}:{:02}:{:02}",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn each_event_adds_its_lines_and_old_ones_are_dropped() {
        let lines = LogLines::default();
        for i in 0..KEPT_LINES {
            write!(lines.make_writer(), "line {i}").unwrap();
        }
        write!(lines.make_writer(), "error\n  caused by: x\n").unwrap();
        let (kept, dropped) = lines.snapshot();
        assert_eq!(dropped, 2);
        assert_eq!(kept.len(), KEPT_LINES);
        assert_eq!(kept[0], "line 2");
        assert_eq!(kept[KEPT_LINES - 2..], ["error", "  caused by: x"]);
    }
}
