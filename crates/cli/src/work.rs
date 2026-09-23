//! Running queued reviews, a bounded number at a time across all PRs.
//!
//! When a review of a newer head arrives while one of an older head of the
//! same PR is running, the older one is stopped: its drafts would be about
//! code that has already changed.

use std::{
    any::Any,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
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
    task::{self, JoinError, JoinSet},
};
use tracing::{Instrument, Span, debug, error, info, info_span, warn};

pub struct Worker {
    runner: ReviewRunner,
    store: Arc<Mutex<Store>>,
    settings: RwLock<Settings>,
    limit: Arc<Semaphore>,
    /// The review each PR has running, so a newer head can stop it.
    active: Mutex<HashMap<PrKey, Active>>,
    /// Set by [`Worker::cancel_all`]: stopped runs are queued again rather
    /// than superseded, and nothing new starts.
    stopping: AtomicBool,
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
            stopping: AtomicBool::new(false),
        }
    }

    /// Stops every running review for shutdown, returning their PRs. Each
    /// is queued again, so the next start runs (or holds) it; see
    /// [`Worker::wait_idle`].
    pub fn cancel_all(&self) -> Vec<PrKey> {
        self.stopping.store(true, Ordering::SeqCst);
        let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        for run in active.values() {
            run.cancel.notify_one();
        }
        let mut keys: Vec<PrKey> = active.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Waits up to `limit` for the cancelled reviews to wind down: agents
    /// killed, worktrees removed, runs requeued. `false` if some didn't.
    pub async fn wait_idle(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let idle = self
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty();
            if idle {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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
    /// every started run has finished. A run whose task panics is recorded
    /// as crashed, and the others carry on.
    pub async fn work(self: Arc<Self>, runs: mpsc::UnboundedReceiver<QueuedRun>) {
        supervise(
            runs,
            |run| {
                self.stop_older_review(&run);
                Arc::clone(&self).execute(run)
            },
            |run, panic| self.crashed(run, panic),
        )
        .await;
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
    /// that stops it. `None` if this run is already registered: a review
    /// started by hand twice before the worker claimed it arrives twice.
    fn start(&self, run: &QueuedRun) -> Option<Arc<Notify>> {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if active
            .get(&run.request.key)
            .is_some_and(|a| a.run_id == run.id)
        {
            return None;
        }
        let cancel = Arc::new(Notify::new());
        active.insert(
            run.request.key.clone(),
            Active {
                run_id: run.id,
                head_sha: run.request.head_sha.clone(),
                cancel: Arc::clone(&cancel),
            },
        );
        Some(cancel)
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
        // Shutting down: it stays queued for the next start.
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        // Registered before claiming: a newer head queued after the claim
        // then always finds this run to stop, and one queued before it has
        // already superseded the run in the store, so the claim fails.
        let Some(cancel) = self.start(&run) else {
            debug!("already started");
            return;
        };
        // `cancel_all` may have run between the check above and `start`.
        if self.stopping.load(Ordering::SeqCst) {
            self.finish(&run);
            return;
        }
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
            Ok(None) if self.stopping.load(Ordering::SeqCst) => {
                info!("cancelled by shutdown; queued again for the next start");
                self.store().requeue_run(run.id)
            }
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
        self.log_counts();
    }

    /// Cleans up after a run whose task panicked, as a failed run would
    /// have been.
    async fn crashed(&self, run: QueuedRun, panic: String) {
        let url = run.request.key.url();
        error!(url = %url, "review crashed: {panic}");
        self.finish(&run);
        self.runner.discard_worktree(&run).await;
        let stored = self.store().crash_run(run.id, &panic);
        if let Err(err) = stored {
            warn!(url = %url, "recording the run failed: {err:?}");
        }
        self.log_counts();
    }

    fn log_counts(&self) {
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

/// Spawns `execute` for each run as it arrives, and hands any run whose
/// task panicked to `crashed`, in that run's span.
async fn supervise<E, EF, C, CF>(
    mut runs: mpsc::UnboundedReceiver<QueuedRun>,
    execute: E,
    crashed: C,
) where
    E: Fn(QueuedRun) -> EF,
    EF: Future<Output = ()> + Send + 'static,
    C: Fn(QueuedRun, String) -> CF,
    CF: Future<Output = ()>,
{
    let mut tasks = JoinSet::new();
    let mut started: HashMap<task::Id, QueuedRun> = HashMap::new();
    let finished = async |done: Result<(task::Id, ()), JoinError>,
                          started: &mut HashMap<task::Id, QueuedRun>| {
        let err = match done {
            Ok((id, ())) => {
                started.remove(&id);
                return;
            }
            Err(err) => err,
        };
        let Some(run) = started.remove(&err.id()) else {
            return;
        };
        if err.is_panic() {
            let span = run_span(&run);
            crashed(run, panic_message(err.into_panic()))
                .instrument(span)
                .await;
        }
    };
    loop {
        tokio::select! {
            run = runs.recv() => match run {
                Some(run) => {
                    let task = tasks.spawn(execute(run.clone()).instrument(run_span(&run)));
                    started.insert(task.id(), run);
                }
                None => break,
            },
            Some(done) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                finished(done, &mut started).await;
            }
        }
    }
    while let Some(done) = tasks.join_next_with_id().await {
        finished(done, &mut started).await;
    }
}

fn run_span(run: &QueuedRun) -> Span {
    info_span!("run", id = run.id, url = %run.request.key.url())
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => panic
            .downcast_ref::<&str>()
            .map_or_else(|| "panicked".to_owned(), |s| (*s).to_owned()),
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
        pr::{PrKey, PrSnapshot},
        repo::RepoName,
        run::{ReviewRequest, ReviewTrigger},
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

    fn queued(id: i64) -> QueuedRun {
        QueuedRun {
            id,
            request: ReviewRequest {
                key: PrKey {
                    repo: RepoName::new("org", "repo"),
                    number: 7,
                },
                profile: "p".into(),
                head_sha: "h1".into(),
                base_sha: "b1".into(),
                trigger: ReviewTrigger::Requested,
            },
        }
    }

    #[tokio::test]
    async fn runs_after_a_panicking_run_still_run() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(queued(1)).unwrap();
        // Run 2 is only sent once run 1's crash is handled, and closing the
        // channel then lets `supervise` return.
        let tx = Mutex::new(Some(tx));
        let ran = Arc::new(Mutex::new(Vec::new()));
        let crashes = Mutex::new(Vec::new());
        supervise(
            rx,
            |run| {
                let ran = Arc::clone(&ran);
                async move {
                    assert!(run.id != 1, "boom");
                    ran.lock().unwrap().push(run.id);
                }
            },
            |run, panic| {
                crashes.lock().unwrap().push((run.id, panic));
                if let Some(tx) = tx.lock().unwrap().take() {
                    tx.send(queued(2)).unwrap();
                }
                async {}
            },
        )
        .await;
        assert_eq!(*ran.lock().unwrap(), [2]);
        assert_eq!(*crashes.lock().unwrap(), [(1, "boom".to_owned())]);
    }

    #[tokio::test]
    async fn a_crashed_run_is_recorded_as_crashed() {
        let data = tempfile::TempDir::new().unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let snapshot = PrSnapshot {
            key: queued(0).request.key,
            title: "t".into(),
            body: String::new(),
            url: String::new(),
            author: "alice".into(),
            head_sha: "h1".into(),
            base_sha: "b1".into(),
            is_draft: false,
            review_requested: true,
            requested_teams: vec![],
            reviews: vec![],
            threads: vec![],
            files: None,
            updated_at: None,
        };
        store.record(&snapshot, "p", &[]).unwrap();
        let run = store.queue_review(&queued(0).request).unwrap().unwrap();
        assert!(store.claim_run(run.id).unwrap());
        let store = Arc::new(Mutex::new(store));
        let worker = Worker::new(data.path(), Arc::clone(&store), &config(1, "p"));

        worker.crashed(run.clone(), "boom".into()).await;
        let record = store.lock().unwrap().run(run.id).unwrap().unwrap();
        assert_eq!(record.status, "crashed");
        assert_eq!(record.error.as_deref(), Some("boom"));
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
        newer_head_stops_older(1, 1).await;
    }

    /// A held review started by hand twice reaches the worker twice. With a
    /// free slot for the second copy, it must not take over the first's
    /// registration, or the newer head couldn't stop the first.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_sent_twice_can_still_be_stopped() {
        newer_head_stops_older(2, 2).await;
    }

    /// Sends the review of the first head `sends` times, then queues the
    /// second head once the first is under way, and checks it stopped the
    /// first.
    async fn newer_head_stops_older(sends: usize, max_concurrent: usize) {
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
                 [runner]\nclaude = \"{}\"\nmax_concurrent = {max_concurrent}\n\
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
            updated_at: None,
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
        for _ in 0..sends {
            runs.send(old.clone()).unwrap();
        }
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
