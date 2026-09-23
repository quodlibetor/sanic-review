//! Running queued reviews, a bounded number at a time across all PRs.
//!
//! When a review of a newer head arrives while one of an older head of the
//! same PR is running, the older one is stopped: its drafts would be about
//! code that has already changed.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock},
};

use color_eyre::eyre::{Result, eyre};
use sanic_core::{
    config::{Config, RunnerSettings},
    pr::PrKey,
    run::{QueuedRun, ReviewResult},
};
use sanic_runner::review::{AgentProfile, ReviewRunner, RunSettings};
use sanic_store::Store;
use tokio::{
    sync::{Notify, Semaphore, mpsc},
    task::JoinSet,
};
use tracing::{Instrument, debug, info, info_span, warn};

pub struct Worker {
    runner: ReviewRunner,
    store: Arc<Mutex<Store>>,
    settings: RwLock<Settings>,
    limit: Arc<Semaphore>,
    /// The review each PR has running, so a newer head can stop it.
    active: Mutex<HashMap<PrKey, Active>>,
}

struct Active {
    run_id: i64,
    head_sha: String,
    cancel: Arc<Notify>,
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
            active: Mutex::default(),
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
                        self.stop_older_review(&run);
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

    /// Stops the running review of `run`'s PR, if it's of another head.
    fn stop_older_review(&self, run: &QueuedRun) {
        let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(older) = active.get(&run.request.key)
            && older.head_sha != run.request.head_sha
        {
            // `notify_one` keeps the wakeup if the run isn't waiting yet.
            older.cancel.notify_one();
        }
    }

    /// Registers `run` as its PR's running review, returning the signal
    /// that stops it.
    fn start(&self, run: &QueuedRun) -> Arc<Notify> {
        let cancel = Arc::new(Notify::new());
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                run.request.key.clone(),
                Active {
                    run_id: run.id,
                    head_sha: run.request.head_sha.clone(),
                    cancel: Arc::clone(&cancel),
                },
            );
        cancel
    }

    fn finish(&self, run: &QueuedRun) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if active
            .get(&run.request.key)
            .is_some_and(|a| a.run_id == run.id)
        {
            active.remove(&run.request.key);
        }
    }

    async fn execute(self: Arc<Self>, run: QueuedRun) {
        let Ok(_permit) = self.limit.acquire().await else {
            return;
        };
        // Registered before claiming: a newer head queued after the claim
        // then always finds this run to stop, and one queued before it has
        // already superseded the run in the store, so the claim fails.
        let cancel = self.start(&run);
        // Bound first so the store guard is dropped before the review awaits.
        let claimed = self.store().claim_run(run.id);
        let outcome = match claimed {
            Ok(true) => self.review(&run, cancel.notified()).await,
            Ok(false) => {
                self.finish(&run);
                debug!("superseded before it started");
                return;
            }
            Err(err) => Err(err),
        };
        self.finish(&run);
        let stored = match outcome {
            Ok(None) => {
                info!("stopped: a newer head of the PR was queued");
                self.store().supersede_run(run.id)
            }
            Ok(Some(result)) => {
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

    /// `None` if `cancel` stopped it.
    async fn review(
        &self,
        run: &QueuedRun,
        cancel: impl Future<Output = ()>,
    ) -> Result<Option<ReviewResult>> {
        let req = &run.request;
        let settings = self.run_settings(&req.profile)?;
        let ctx = self
            .store()
            .pr_context(&req.key)?
            .ok_or_else(|| eyre!("{} is not in the database", req.key.url()))?;
        info!(head = %req.head_sha, trigger = req.trigger.as_str(), "reviewing");
        self.runner.review(run, &ctx, &settings, cancel).await
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

    /// Runs git isolated from the user's config, returning trimmed stdout.
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// `<root>/org/repo.git` whose PR 7 was pushed twice: returns
    /// `(base, first head, second head)`.
    fn pushed_twice(root: &Path) -> (String, String, String) {
        let github_repo = root.join("org/repo.git");
        std::fs::create_dir_all(&github_repo).unwrap();
        git(&github_repo, &["init", "-q", "--bare"]);
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        let mut heads = Vec::new();
        for contents in ["one\n", "two\n", "three\n"] {
            std::fs::write(work.join("lib.rs"), contents).unwrap();
            git(&work, &["add", "."]);
            git(&work, &["commit", "-q", "-m", contents.trim()]);
            heads.push(git(&work, &["rev-parse", "HEAD"]));
        }
        let target = github_repo.to_string_lossy();
        git(&work, &["push", "-q", &target, "HEAD:refs/pull/7/head"]);
        let [base, first, second] = heads.try_into().unwrap();
        (base, first, second)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_newer_head_stops_the_running_review_of_an_older_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let (base, first, second) = pushed_twice(&dir.path().join("github"));
        // Hangs until killed when briefed on the first head; answers on any
        // other.
        let fake = dir.path().join("fake");
        std::fs::create_dir(&fake).unwrap();
        let answer = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "structured_output": {
                "summary": "fine", "suggested_verdict": "none", "comments": []
            }
        });
        std::fs::write(fake.join("answer.jsonl"), format!("{answer}\n")).unwrap();
        let script = fake.join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nd='{}'\ncat > \"$d/stdin.$$\"\n\
                 if grep -q {first} \"$d/stdin.$$\"; then touch \"$d/started\"; exec sleep 30; fi\n\
                 cat \"$d/answer.jsonl\"\n",
                fake.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let config = Config::parse(
            &format!(
                "[github]\ngit_url = \"{}\"\n\
                 [runner]\nclaude = \"{}\"\nmax_concurrent = 1\n\
                 [profile.p]\nrepos = [{{ github = \"org\" }}]\n",
                dir.path().join("github").display(),
                script.display()
            ),
            Path::new("/"),
            &NoCheckouts,
        )
        .unwrap();

        let key = PrKey {
            repo: RepoName::new("org", "repo"),
            number: 7,
        };
        let mut store = Store::open_in_memory().unwrap();
        let snapshot = sanic_core::pr::PrSnapshot {
            key: key.clone(),
            title: "t".into(),
            body: String::new(),
            url: key.url(),
            author: "alice".into(),
            head_sha: first.clone(),
            base_sha: base.clone(),
            is_draft: false,
            review_requested: true,
            requested_teams: vec![],
            reviews: vec![],
            threads: vec![],
            files: None,
        };
        store.record(&snapshot, "p", &[]).unwrap();
        let store = Arc::new(Mutex::new(store));
        let request = |head: &str| sanic_core::run::ReviewRequest {
            key: key.clone(),
            profile: "p".into(),
            head_sha: head.into(),
            base_sha: base.clone(),
            trigger: sanic_core::run::ReviewTrigger::Requested,
        };
        let queue = |head: &str| {
            store
                .lock()
                .unwrap()
                .queue_review(&request(head))
                .unwrap()
                .unwrap()
        };

        let data = dir.path().join("data");
        let worker = Arc::new(Worker::new(&data, Arc::clone(&store), &config));
        let (runs, runs_rx) = mpsc::unbounded_channel();
        let working = tokio::spawn(Arc::clone(&worker).work(runs_rx));
        let old = queue(&first);
        runs.send(old.clone()).unwrap();
        while !fake.join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let new = queue(&second);
        runs.send(new.clone()).unwrap();
        drop(runs);
        tokio::time::timeout(std::time::Duration::from_secs(20), working)
            .await
            .expect("the old review was not stopped")
            .unwrap();

        let store = store.lock().unwrap();
        assert_eq!(store.run(old.id).unwrap().unwrap().status, "superseded");
        assert!(store.drafts(old.id).unwrap().is_empty());
        assert!(!data.join(format!("worktrees/{}", old.id)).exists());
        assert_eq!(store.run(new.id).unwrap().unwrap().status, "succeeded");
        assert_eq!(store.drafts(new.id).unwrap()[0].body, "fine");
    }
}
