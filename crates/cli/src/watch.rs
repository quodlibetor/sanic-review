//! Noticing when the config file changes.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr, eyre};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// Editors write a file in several steps (truncate, write, rename); wait for
/// the burst to finish before reloading.
const SETTLE: Duration = Duration::from_millis(250);

pub struct ConfigWatcher {
    // Dropping the watcher stops the events.
    _watcher: RecommendedWatcher,
    events: mpsc::UnboundedReceiver<()>,
    /// A change was received but `changed` was dropped while it settled;
    /// the next call reports it instead of waiting for another.
    unreported: bool,
}

impl ConfigWatcher {
    /// Watches the file's directory rather than the file, so replacing the
    /// file by rename (as most editors do) is still seen.
    pub fn new(path: &Path) -> Result<Self> {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."));
        let name: OsString = path
            .file_name()
            .ok_or_else(|| eyre!("config path {} has no file name", path.display()))?
            .to_owned();
        let (tx, events) = mpsc::unbounded_channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                let touches_config = event.paths.iter().any(|p| p.file_name() == Some(&name));
                if touches_config && !matches!(event.kind, EventKind::Access(_)) {
                    // A closed receiver means serve is shutting down.
                    let _ = tx.send(());
                }
            })
            .wrap_err("starting the config file watcher")?;
        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .wrap_err_with(|| format!("watching {}", dir.display()))?;
        Ok(Self {
            _watcher: watcher,
            events,
            unreported: false,
        })
    }

    /// Resolves after the config file changes and the writes have settled.
    /// Cancel-safe: `serve` races it against its poll timer.
    pub async fn changed(&mut self) {
        if !self.unreported {
            if self.events.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
            self.unreported = true;
        }
        tokio::time::sleep(SETTLE).await;
        while self.events.try_recv().is_ok() {}
        self.unreported = false;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn sees_writes_and_renames_but_not_siblings() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a").unwrap();
        let mut watcher = ConfigWatcher::new(&path).unwrap();
        let wait = Duration::from_secs(5);

        std::fs::write(dir.path().join("other.toml"), "x").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), watcher.changed())
                .await
                .is_err(),
            "a sibling file is not the config"
        );

        std::fs::write(&path, "b").unwrap();
        tokio::time::timeout(wait, watcher.changed()).await.unwrap();

        let tmp = dir.path().join(".config.toml.swp");
        std::fs::write(&tmp, "c").unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        tokio::time::timeout(wait, watcher.changed()).await.unwrap();
    }

    #[tokio::test]
    async fn a_change_is_reported_after_a_cancelled_wait() {
        use std::io::Write as _;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a").unwrap();
        let mut watcher = ConfigWatcher::new(&path).unwrap();

        // An append is a single event.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"b")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // `serve`'s poll timer can win the race while the change settles.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), watcher.changed())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), watcher.changed())
            .await
            .unwrap();
    }
}
