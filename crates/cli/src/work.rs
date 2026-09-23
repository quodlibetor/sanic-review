//! Running queued reviews, a bounded number at a time across all PRs.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock},
};

use color_eyre::eyre::{Result, eyre};
use sanic_core::{
    config::{Config, RunnerSettings},
    run::{QueuedRun, ReviewResult},
};
use sanic_runner::review::{AgentProfile, ReviewRunner, RunSettings};
use sanic_store::Store;
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinSet,
};
use tracing::{Instrument, debug, info, info_span, warn};

pub struct Worker {
    runner: ReviewRunner,
    store: Arc<Mutex<Store>>,
    settings: RwLock<Settings>,
    limit: Arc<Semaphore>,
}

/// The parts of the config runs use. Replaced on reload; a run reads them
/// when it starts.
struct Settings {
    /// By profile name.
    profiles: HashMap<String, AgentProfile>,
    runner: RunnerSettings,
    git_url: String,
    reference_dirs: Vec<PathBuf>,
}

impl Settings {
    fn new(config: &Config) -> Self {
        Self {
            profiles: config
                .profiles
                .iter()
                .map(|p| (p.name.clone(), AgentProfile::from(p)))
                .collect(),
            runner: config.runner.clone(),
            git_url: config.github.git_url.clone(),
            reference_dirs: config.reference_dirs(),
        }
    }
}

impl Worker {
    pub fn new(data_dir: &Path, store: Arc<Mutex<Store>>, config: &Config) -> Self {
        Self {
            runner: ReviewRunner::new(data_dir),
            store,
            settings: RwLock::new(Settings::new(config)),
            limit: Arc::new(Semaphore::new(config.runner.max_concurrent)),
        }
    }

    /// Applies a reloaded config to runs that start from now on. Lowering
    /// `max_concurrent` takes effect as running runs finish.
    pub fn configure(&self, config: &Config) {
        let new = Settings::new(config);
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let (old_max, new_max) = (settings.runner.max_concurrent, new.runner.max_concurrent);
        *settings = new;
        if new_max > old_max {
            self.limit.add_permits(new_max - old_max);
        } else if new_max < old_max {
            let limit = Arc::clone(&self.limit);
            let excess = u32::try_from(old_max - new_max).unwrap_or(u32::MAX);
            tokio::spawn(async move {
                if let Ok(permits) = limit.acquire_many_owned(excess).await {
                    permits.forget();
                }
            });
        }
    }

    fn run_settings(&self, profile: &str) -> Result<RunSettings> {
        let settings = self.settings.read().unwrap_or_else(PoisonError::into_inner);
        let agent = settings
            .profiles
            .get(profile)
            .ok_or_else(|| eyre!("profile `{profile}` is no longer configured"))?;
        Ok(RunSettings::new(
            agent.clone(),
            &settings.runner,
            &settings.git_url,
            settings.reference_dirs.clone(),
        ))
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs each queued run as it arrives. Returns when `runs` closes and
    /// every started run has finished.
    pub async fn work(self: Arc<Self>, mut runs: mpsc::UnboundedReceiver<QueuedRun>) {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                run = runs.recv() => match run {
                    Some(run) => {
                        let span = info_span!("run", id = run.id, url = %run.request.key.url());
                        tasks.spawn(Arc::clone(&self).execute(run).instrument(span));
                    }
                    None => break,
                },
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        while tasks.join_next().await.is_some() {}
    }

    async fn execute(self: Arc<Self>, run: QueuedRun) {
        let Ok(_permit) = self.limit.acquire().await else {
            return;
        };
        // Bound first so the store guard is dropped before the review awaits.
        let claimed = self.store().claim_run(run.id);
        let outcome = match claimed {
            Ok(true) => self.review(&run).await,
            Ok(false) => {
                debug!("superseded before it started");
                return;
            }
            Err(err) => Err(err),
        };
        let stored = match outcome {
            Ok(result) => {
                let stored = self.store().finish_review(run.id, &result);
                if stored.is_ok() {
                    log_result(&result);
                }
                stored
            }
            Err(err) => {
                warn!(url = %run.request.key.url(), "review failed: {err:?}");
                self.store().fail_run(run.id, &format!("{err:#}"))
            }
        };
        if let Err(err) = stored {
            warn!(url = %run.request.key.url(), "recording the run failed: {err:?}");
        }
        let counts = self.store().run_counts();
        match counts {
            Ok(counts) => info!(
                queued = counts.queued,
                running = counts.running,
                pending_drafts = counts.pending_drafts,
                "runs"
            ),
            Err(err) => warn!("counting runs failed: {err:?}"),
        }
    }

    async fn review(&self, run: &QueuedRun) -> Result<ReviewResult> {
        let req = &run.request;
        let settings = self.run_settings(&req.profile)?;
        let ctx = self
            .store()
            .pr_context(&req.key)?
            .ok_or_else(|| eyre!("{} is not in the database", req.key.url()))?;
        info!(head = %req.head_sha, trigger = req.trigger.as_str(), "reviewing");
        self.runner.review(run, &ctx, &settings).await
    }
}

fn log_result(result: &ReviewResult) {
    let unanchored = result.comments.iter().filter(|c| c.unanchored).count();
    let headline = result.summary.lines().next().unwrap_or_default();
    info!(
        verdict = result.verdict.as_str(),
        comments = result.comments.len(),
        unanchored,
        "review drafted: {headline}"
    );
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::bail;
    use sanic_core::{
        config::{CheckoutResolver, Vcs},
        repo::RepoName,
    };

    use super::*;

    struct NoCheckouts;

    impl CheckoutResolver for NoCheckouts {
        fn resolve(&self, path: &Path, _: Option<&str>) -> Result<(Vcs, RepoName)> {
            bail!("unexpected checkout {}", path.display())
        }
    }

    fn config(max_concurrent: usize, profile: &str) -> Config {
        let text = format!(
            "[runner]\nmax_concurrent = {max_concurrent}\n\
             [profile.{profile}]\nrepos = [{{ github = \"org\" }}]\n"
        );
        Config::parse(&text, Path::new("/"), &NoCheckouts).unwrap()
    }

    #[tokio::test]
    async fn reloads_apply_to_later_runs() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let worker = Worker::new(Path::new("/data"), store, &config(2, "old"));
        assert!(worker.run_settings("old").is_ok());

        worker.configure(&config(3, "new"));
        assert_eq!(worker.limit.available_permits(), 3);
        assert!(worker.run_settings("old").is_err());
        assert!(worker.run_settings("new").is_ok());

        worker.configure(&config(1, "new"));
        tokio::task::yield_now().await;
        assert_eq!(worker.limit.available_permits(), 1);

        let with_refs = Config::parse(
            "[runner]\nread_paths = [\"/refs\"]\n[profile.new]\nrepos = [{ github = \"org\" }]\n",
            Path::new("/"),
            &NoCheckouts,
        )
        .unwrap();
        worker.configure(&with_refs);
        assert_eq!(
            worker.run_settings("new").unwrap().reference_dirs,
            [PathBuf::from("/refs")]
        );
    }
}
