//! Checking the edited config as `serve` would load it, off the UI thread:
//! remote discovery runs jj or git for each local checkout, so what it
//! finds is kept while the editor is open rather than found again on
//! every edit.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, mpsc},
};

use color_eyre::eyre::{Result, eyre};
use sanic_core::{
    config::{CheckoutResolver, Config, Unloadable, Vcs},
    repo::RepoName,
};

/// Whether the text loads, or why not in one line.
pub type Checked = Result<(), String>;

pub struct Checker {
    path: PathBuf,
    resolver: Arc<Remembering>,
    done_tx: mpsc::Sender<(u64, Checked)>,
    done: mpsc::Receiver<(u64, Checked)>,
}

impl Checker {
    /// Checks text as the config at `path`.
    pub fn new(path: PathBuf, resolver: Arc<dyn CheckoutResolver + Send + Sync>) -> Self {
        let (done_tx, done) = mpsc::channel();
        Self {
            path,
            resolver: Arc::new(Remembering {
                resolver,
                found: Mutex::default(),
            }),
            done_tx,
            done,
        }
    }

    /// Starts checking `text`; [`Checker::finished`] hands it back with
    /// `generation`.
    pub fn start(&self, generation: u64, text: String) {
        let path = self.path.clone();
        let resolver = Arc::clone(&self.resolver);
        let done = self.done_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("config check".into())
            .spawn(move || {
                // Only fails once the editor has closed.
                let _ = done.send((generation, check(&text, &path, &*resolver)));
            });
        if let Err(err) = spawned {
            let _ = self
                .done_tx
                .send((generation, Err(format!("couldn't check the config: {err}"))));
        }
    }

    /// The newest check that finished since the last call, if any.
    pub fn finished(&self) -> Option<(u64, Checked)> {
        self.done
            .try_iter()
            .max_by_key(|(generation, _)| *generation)
    }
}

/// Whether `text` loads as the config at `path`. The editor shows which
/// file it is, so the reason leaves it out.
pub fn check(text: &str, path: &Path, resolver: &dyn CheckoutResolver) -> Checked {
    let base = path.parent().unwrap_or(Path::new("."));
    Config::parse(text, base, resolver)
        .map(drop)
        .map_err(|err| Unloadable(err).to_string())
}

/// A resolver that remembers each checkout's answer, failures included.
struct Remembering {
    resolver: Arc<dyn CheckoutResolver + Send + Sync>,
    #[allow(
        clippy::type_complexity,
        reason = "a cache keyed as `resolve` is called"
    )]
    found: Mutex<HashMap<(PathBuf, Option<String>), Result<(Vcs, RepoName), String>>>,
}

impl CheckoutResolver for Remembering {
    fn resolve(&self, path: &Path, remote: Option<&str>) -> Result<(Vcs, RepoName)> {
        let key = (path.to_owned(), remote.map(String::from));
        let known = self
            .found
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        let found = known.unwrap_or_else(|| {
            let found = self
                .resolver
                .resolve(path, remote)
                .map_err(|err| format!("{err:#}"));
            self.found
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key, found.clone());
            found
        });
        found.map_err(|err| eyre!(err))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Counting(AtomicUsize);

    impl CheckoutResolver for Counting {
        fn resolve(&self, path: &Path, _: Option<&str>) -> Result<(Vcs, RepoName)> {
            self.0.fetch_add(1, Ordering::SeqCst);
            match path.file_name().and_then(|n| n.to_str()) {
                Some("gone") => Err(eyre!("no checkout at {}", path.display())),
                _ => Ok((Vcs::Git, RepoName::new("o", "r"))),
            }
        }
    }

    #[test]
    fn checks_run_off_thread_and_remember_checkouts() {
        let counting = Arc::new(Counting(AtomicUsize::new(0)));
        let checker = Checker::new(PathBuf::from("/c/config.toml"), counting.clone());
        let text = "[profile.p]\nrepos = [\"/src/a\"]\n";
        for generation in 1..=2 {
            checker.start(generation, text.into());
            let finished = loop {
                if let Some(found) = checker.finished() {
                    break found;
                }
                std::thread::yield_now();
            };
            assert_eq!(finished, (generation, Ok(())));
        }
        assert_eq!(counting.0.load(Ordering::SeqCst), 1);

        let err = check(
            "[profile.p]\nrepos = [\"/src/gone\"]\n",
            Path::new("/c/config.toml"),
            &*checker.resolver,
        )
        .unwrap_err();
        assert!(err.contains("in profile `p`"), "{err}");
        assert!(err.contains("no checkout at /src/gone"), "{err}");
    }
}
