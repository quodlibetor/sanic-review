//! Running queued reviews, a bounded number at a time across all PRs.
//!
//! When a review of a newer head arrives while one of an older head of the
//! same PR is running, the older one is stopped: its drafts would be about
//! code that has already changed.

use std::{
    any::Any,
    collections::{HashMap, HashSet, VecDeque},
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
    manual::ManualReviews,
    pr::PrKey,
    run::{DraftRevision, QueuedRun, Resume, ReviewResult, ReviewTrigger, Revision},
    skip::SkipRules,
};
use sanic_runner::review::{AgentProfile, ReviewRunner, Reviewed, RunSettings};
use sanic_store::Store;
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc},
    task::{self, JoinError, JoinSet},
};
use tracing::{Instrument, Span, debug, error, info, info_span, warn};

pub struct Worker {
    runner: ReviewRunner,
    /// The GitHub login reviews are drafted for.
    me: String,
    store: Arc<Mutex<Store>>,
    settings: RwLock<Settings>,
    limit: Arc<Semaphore>,
    /// The review each PR has running, so a newer head can stop it, and
    /// apart from those, its running regeneration.
    active: Mutex<HashMap<ActiveKey, Active>>,
    /// The worktree paths runs are using, so two never check out at one:
    /// a review that resumes a session shares its path with the run it
    /// resumes, and so with that run's regenerations.
    worktrees: Mutex<HashSet<PathBuf>>,
    /// Each run's resumed form while it tries resuming a session, so a
    /// crash then discards the worktree at that session's path, not only
    /// the one at the run's own.
    resuming: Mutex<HashMap<i64, QueuedRun>>,
    /// Set by [`Worker::cancel_all`]: stopped runs are queued again rather
    /// than superseded, and nothing new starts.
    stopping: AtomicBool,
}

/// A worktree path a run is using, until it's dropped.
struct Claim<'a> {
    worktrees: &'a Mutex<HashSet<PathBuf>>,
    path: PathBuf,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.worktrees
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.path);
    }
}

/// A PR, and whether the run is a regeneration: one can run beside a
/// review of the PR, and isn't stopped by a newer head, since it's meant
/// for its source run's head.
type ActiveKey = (PrKey, bool);

fn active_key(run: &QueuedRun) -> ActiveKey {
    (run.request.key.clone(), run.revision.is_some())
}

struct Active {
    run_id: i64,
    head_sha: String,
    stop: Arc<Stopper>,
}

/// Why a running run was stopped, which decides how it's recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    NewerHead,
    Shutdown,
    Cancelled,
}

/// The signal that stops a running run, and why.
#[derive(Default)]
struct Stopper {
    reason: Mutex<Option<Stop>>,
    notify: Notify,
}

impl Stopper {
    /// The first reason wins: a shutdown during a supersede is a supersede.
    fn stop(&self, reason: Stop) {
        let mut held = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        held.get_or_insert(reason);
        drop(held);
        // `notify_one` keeps the wakeup if the run isn't waiting yet.
        self.notify.notify_one();
    }

    fn reason(&self) -> Option<Stop> {
        *self.reason.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn stopped(&self) {
        if self.reason().is_some() {
            return;
        }
        self.notify.notified().await;
    }
}

/// The parts of the config runs use. Replaced on reload; a run reads them
/// when it starts.
struct Settings {
    /// By profile name.
    profiles: HashMap<String, AgentProfile>,
    runner: RunnerSettings,
    manual_reviews: ManualReviews,
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
            manual_reviews: config.manual_reviews(),
            git_url: config.github.git_url.clone(),
            reference_dirs: config.reference_dirs(),
        }
    }
}

/// Supersedes the queued reviews of PRs `skips` leaves unlisted, which are
/// never reviewed automatically; with `was`, only of those it listed.
/// Failing is logged, not fatal.
pub fn supersede_unlisted(store: &mut Store, skips: &SkipRules, was: Option<&SkipRules>) {
    let newly = |profile: &str, author: &str| {
        skips.unlisted(profile, author) && !was.is_some_and(|was| was.unlisted(profile, author))
    };
    match store.supersede_queued_reviews(newly) {
        Ok(keys) => {
            for key in keys {
                info!(url = %key.url(), "queued review superseded: `authors` excludes its author");
            }
        }
        Err(err) => warn!("superseding queued reviews of skipped authors failed: {err:?}"),
    }
}

impl Worker {
    pub fn new(data_dir: &Path, store: Arc<Mutex<Store>>, config: &Config, me: &str) -> Self {
        Self {
            runner: ReviewRunner::new(data_dir),
            me: me.to_owned(),
            store,
            settings: RwLock::new(Settings::new(config)),
            limit: Arc::new(Semaphore::new(config.runner.max_concurrent)),
            active: Mutex::default(),
            worktrees: Mutex::default(),
            resuming: Mutex::default(),
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
            run.stop.stop(Stop::Shutdown);
        }
        let mut keys: Vec<PrKey> = active.keys().map(|(key, _)| key.clone()).collect();
        keys.sort();
        keys.dedup();
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

    /// Supersedes the queued reviews of PRs a reload took off the reviews
    /// you owe: unlisted by `skips` but listed by `was`. A review started by
    /// hand on a PR that was already unlisted is left to run.
    pub fn supersede_newly_unlisted(&self, was: &SkipRules, skips: &SkipRules) {
        supersede_unlisted(&mut self.store(), skips, Some(was));
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

    pub fn run_settings(&self, profile: &str) -> Result<RunSettings> {
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

    /// Runs each queued run as it arrives. Returns when `jobs` closes and
    /// every started run has finished. A run whose task panics is recorded
    /// as crashed, and the others carry on.
    pub async fn work(self: Arc<Self>, jobs: mpsc::UnboundedReceiver<Job>) {
        dispatch(
            jobs,
            Arc::clone(&self.limit),
            Dispatch {
                noticed: |job: &Job| {
                    // A held newer head won't run, so stopping the running
                    // review would spend its work for nothing.
                    if job.automatic && self.holds_reviews(&job.run.request.profile) {
                        debug!(run = job.run.id, "held: manual reviews hold its profile");
                    } else {
                        self.stop_older_review(&job.run);
                    }
                },
                claim: || {
                    if self.stopping.load(Ordering::SeqCst) {
                        return None;
                    }
                    match self
                        .store()
                        .claim_next(|profile| self.holds_reviews(profile))
                    {
                        Ok(run) => run,
                        Err(err) => {
                            warn!("taking the next queued run failed: {err:?}");
                            None
                        }
                    }
                },
                register: |run: &QueuedRun| self.register(run),
                execute: |run, stop, permit| Arc::clone(&self).execute(run, stop, permit),
                claim_started: |job: &Job| self.claim_started(job),
                crashed: |run, panic| self.crashed(run, panic),
            },
        )
        .await;
    }

    /// Registers a claimed run before it is spawned, so a cancel always
    /// finds it. `None` if its PR already has this run registered, which a
    /// hand start sent twice does; the claim is undone, since the run is
    /// `running` in the store by now and nothing else would record it.
    fn register(&self, run: &QueuedRun) -> Option<Arc<Stopper>> {
        let stop = self.start(run);
        if stop.is_none() {
            debug!(run = run.id, "already started; putting it back");
            if let Err(err) = self.store().requeue_run(run.id) {
                warn!(run = run.id, "putting the run back failed: {err:?}");
            }
        }
        stop
    }

    /// Claims a job that skips the queue. `false` if it was superseded or
    /// cancelled while it waited for a permit, or if manual reviews started
    /// holding its profile, so it never starts.
    fn claim_started(&self, job: &Job) -> bool {
        if job.automatic && self.holds_reviews(&job.run.request.profile) {
            debug!(
                run = job.run.id,
                "held: manual reviews were turned on before it started"
            );
            return false;
        }
        match self.store().claim_run(job.run.id) {
            Ok(true) => true,
            Ok(false) => {
                debug!(run = job.run.id, "superseded before it started");
                false
            }
            Err(err) => {
                warn!(run = job.run.id, "claiming the run failed: {err:?}");
                false
            }
        }
    }

    /// Whether manual reviews hold automatic runs of `profile`'s PRs, as
    /// the config last loaded.
    fn holds_reviews(&self, profile: &str) -> bool {
        self.settings
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .manual_reviews
            .holds(profile)
    }

    /// Stops the running review of `run`'s PR, if it's of another head.
    fn stop_older_review(&self, run: &QueuedRun) {
        let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(older) = active.get(&(run.request.key.clone(), false))
            && older.head_sha != run.request.head_sha
        {
            older.stop.stop(Stop::NewerHead);
        }
    }

    /// Stops run `id` because you asked: its agent is killed and its
    /// worktree removed. `false` if it isn't running here, so the caller
    /// can say so rather than claim it stopped. The dispatcher registers a
    /// run before it spawns, so a claimed run is always findable here.
    pub fn cancel(&self, id: i64) -> bool {
        let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(run) = active.values().find(|a| a.run_id == id) else {
            return false;
        };
        run.stop.stop(Stop::Cancelled);
        true
    }

    /// Registers `run` as its PR's running review, returning the signal
    /// that stops it. `None` if this run is already registered: a review
    /// started by hand twice before the worker claimed it arrives twice.
    fn start(&self, run: &QueuedRun) -> Option<Arc<Stopper>> {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if active
            .get(&active_key(run))
            .is_some_and(|a| a.run_id == run.id)
        {
            return None;
        }
        let stop = Arc::new(Stopper::default());
        active.insert(
            active_key(run),
            Active {
                run_id: run.id,
                head_sha: run.request.head_sha.clone(),
                stop: Arc::clone(&stop),
            },
        );
        Some(stop)
    }

    fn finish(&self, run: &QueuedRun) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let key = active_key(run);
        if active.get(&key).is_some_and(|a| a.run_id == run.id) {
            active.remove(&key);
        }
    }

    /// Runs `run`, which the dispatcher has already claimed and registered,
    /// holding `_permit` for as long as it takes.
    async fn execute(
        self: Arc<Self>,
        run: QueuedRun,
        stop: Arc<Stopper>,
        _permit: OwnedSemaphorePermit,
    ) {
        // `cancel_all` may have run between the claim and here.
        if self.stopping.load(Ordering::SeqCst) {
            stop.stop(Stop::Shutdown);
        }
        let outcome = self.review(&run, &stop).await;
        self.finish(&run);
        let stored = self.record(&run, &stop, outcome);
        if let Err(err) = stored {
            warn!(url = %run.request.key.url(), "recording the run failed: {err:?}");
        }
        self.log_counts();
    }

    /// Stores how `run` ended, as `outcome` says, and logs it.
    fn record(
        &self,
        run: &QueuedRun,
        stop: &Stopper,
        outcome: Result<Option<Reviewed>>,
    ) -> Result<()> {
        match outcome {
            Ok(None) => match stop.reason() {
                Some(Stop::Cancelled) => {
                    info!("cancelled: you stopped it");
                    self.store().cancel_running_run(run.id)
                }
                // Started by hand, so not started again unasked.
                Some(Stop::Shutdown) if run.revision.is_some() => {
                    info!("regeneration cancelled by shutdown");
                    self.store()
                        .fail_run(run.id, "cancelled when serve stopped; regenerate again")
                }
                Some(Stop::Shutdown) => {
                    info!("cancelled by shutdown; queued again for the next start");
                    self.store().requeue_run(run.id)
                }
                Some(Stop::NewerHead) | None => {
                    info!("stopped: a newer head of the PR was queued");
                    self.store().supersede_run(run.id)
                }
            },
            Ok(Some(Reviewed::Review { result, basis })) => {
                let stored = match (&run.revision, &basis) {
                    (Some(revision), Some(basis)) => self
                        .store()
                        .finish_revision(run.id, &result, revision, basis),
                    _ => self.store().finish_review(run.id, &result),
                };
                if stored.is_ok() {
                    log_result(&result);
                }
                stored
            }
            Ok(Some(Reviewed::Resumed {
                result,
                basis,
                resume,
                moved,
            })) => self
                .store()
                .finish_resumed(run.id, &result, &resume, &basis, &moved)
                .inspect(|()| log_result(&result)),
            Ok(Some(Reviewed::NoUpdate {
                result,
                carried,
                dismissed,
            })) => self
                .store()
                .finish_no_update(run.id, &result, &carried, &dismissed)
                .inspect(|()| log_no_update(&result)),
            Ok(Some(Reviewed::NotResumed { why })) => {
                warn!(url = %run.request.key.url(), "regeneration failed: {why}");
                self.store().fail_run(
                    run.id,
                    &format!("couldn't resume the review's session: {why}; start a fresh review"),
                )
            }
            Ok(Some(Reviewed::Draft {
                revised,
                session_id,
                transcript_path,
            })) => match &run.revision {
                Some(revision) => {
                    let stored = self.store().finish_draft_revision(
                        run.id,
                        revision,
                        &revised,
                        session_id.as_deref(),
                        &transcript_path,
                    );
                    if stored.is_ok() {
                        log_draft_revision(revision, &revised);
                    }
                    stored
                }
                None => Err(eyre!("run {} revised a draft it wasn't given", run.id)),
            },
            Err(err) => {
                warn!(url = %run.request.key.url(), "review failed: {err:?}");
                self.store().fail_run(run.id, &format!("{err:#}"))
            }
        }
    }

    /// Cleans up after a run whose task panicked, as a failed run would
    /// have been.
    async fn crashed(&self, run: QueuedRun, panic: String) {
        let url = run.request.key.url();
        error!(url = %url, "review crashed: {panic}");
        self.finish(&run);
        let resumed = self
            .resuming
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&run.id);
        if let Some(resumed) = resumed {
            self.runner.discard_worktree(&resumed).await;
        }
        self.runner.discard_worktree(&run).await;
        let stored = self.store().crash_run(run.id, &panic);
        if let Err(err) = stored {
            warn!(url = %url, "recording the run failed: {err:?}");
        }
        self.log_counts();
    }

    /// Queued and running runs. Not pending drafts: they're counted as
    /// the lists show them, which takes the recency window and who you are.
    fn log_counts(&self) {
        let counts = self.store().run_counts();
        match counts {
            Ok(counts) => info!(queued = counts.queued, running = counts.running, "runs"),
            Err(err) => warn!("counting runs failed: {err:?}"),
        }
    }

    /// `None` if `stop` fired. A push review resumes the session
    /// of the PR's current run, if it has one and it can, and otherwise
    /// reviews the whole PR afresh.
    async fn review(&self, run: &QueuedRun, stop: &Stopper) -> Result<Option<Reviewed>> {
        let req = &run.request;
        let settings = self.run_settings(&req.profile)?;
        let ctx = self
            .store()
            .pr_context(&req.key, &self.me)?
            .ok_or_else(|| eyre!("{} is not in the database", req.key.url()))?;
        if let Some(revision) = &run.revision {
            if let Some(draft) = revision.draft {
                info!(
                    head = %req.head_sha,
                    source_run = revision.source_run,
                    draft,
                    "revising a draft with your note"
                );
            } else {
                info!(
                    head = %req.head_sha,
                    source_run = revision.source_run,
                    "regenerating with your instruction"
                );
            }
        } else {
            info!(head = %req.head_sha, trigger = req.trigger.as_str(), "reviewing");
            // The brief shows it your pending review's comments.
            let shown = ctx.in_progress.as_ref().map_or(0, |r| r.comments.len());
            self.store().record_in_progress_shown(run.id, shown)?;
        }
        if let Some((resumed, _claim)) = self.resumable(run)? {
            let from = resumed.resume.as_ref().map(|r| r.run);
            info!(
                resumed_from = from,
                "resuming the session of the last review"
            );
            self.resuming
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(run.id, resumed.clone());
            let reviewed = self
                .runner
                .review(&resumed, &ctx, &settings, stop.stopped())
                .await;
            self.resuming
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&run.id);
            match reviewed? {
                Some(Reviewed::NotResumed { why }) => {
                    info!("not resumed, reviewing afresh: {why}");
                }
                reviewed => return Ok(reviewed),
            }
        }
        let _claim = self.claim(run).ok_or_else(|| {
            eyre!(
                "{} is in use by another run",
                self.runner.worktree_path(run).display()
            )
        })?;
        self.runner
            .review(run, &ctx, &settings, stop.stopped())
            .await
    }

    /// For a push review, `run` resuming the session of the PR's current
    /// run, and its claim on that session's worktree path:
    /// `None` if there's no such session, or its path is in use, by
    /// another run, one queued to start there, or a chat.
    fn resumable(&self, run: &QueuedRun) -> Result<Option<(QueuedRun, Claim<'_>)>> {
        if run.revision.is_some() || !matches!(run.request.trigger, ReviewTrigger::Push { .. }) {
            return Ok(None);
        }
        // The run whose drafts the PR's page shows, so the drafts it's told
        // of, and carries with no update, are those you've been deciding on.
        let Some(session) = self.store().current_session_run(&run.request.key)? else {
            return Ok(None);
        };
        if session.run.request.head_sha == run.request.head_sha {
            return Ok(None);
        }
        let worktree = session.run.worktree_run();
        // A regeneration queued there would find it in use, or take it
        // over.
        if self.store().worktree_busy(worktree, run.id)? {
            debug!("not resumed: another run is queued for the session's worktree");
            return Ok(None);
        }
        let (drafts, lineage) = self.store().resumable(session.run.id)?;
        let resumed = QueuedRun {
            resume: Some(Resume {
                run: session.run.id,
                session_id: session.session_id,
                head_sha: session.run.request.head_sha.clone(),
                base_sha: session.run.request.base_sha.clone(),
                drafts,
            }),
            worktree: Some(worktree),
            lineage: std::iter::once(session.run.id).chain(lineage).collect(),
            ..run.clone()
        };
        if self.runner.worktree_path(&resumed).exists() {
            debug!("not resumed: the session's worktree is in use");
            return Ok(None);
        }
        Ok(self.claim(&resumed).map(|claim| (resumed, claim)))
    }

    /// Claims the worktree path `run` checks out at, unless another run
    /// has.
    fn claim(&self, run: &QueuedRun) -> Option<Claim<'_>> {
        let path = self.runner.worktree_path(run);
        let mut claimed = self
            .worktrees
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        claimed.insert(path.clone()).then(|| Claim {
            worktrees: &self.worktrees,
            path,
        })
    }
}

/// A queued run for [`Worker::work`]. An `automatic` one, queued by the
/// scheduler or left from before, is held after all if manual reviews
/// hold its profile by the time it would start; one started by hand never
/// is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub run: QueuedRun,
    pub automatic: bool,
}

/// What [`dispatch`] calls as jobs arrive, are taken and finish.
struct Dispatch<N, C, R, E, S, CR> {
    /// Every job the scheduler sends, for the newer-head check.
    noticed: N,
    /// The next queued run, already claimed; `None` when there's none.
    claim: C,
    /// Registers a claimed run; `None` if it mustn't start after all.
    register: R,
    execute: E,
    /// Claims a job that skips the queue; `false` if it can't start.
    claim_started: S,
    crashed: CR,
}

/// Runs what `claim` hands back, `limit` at a time, preferring jobs sent
/// on `jobs` since those were asked for by hand. A run whose task panicked
/// goes to `crashed`, in that run's span. Returns once `jobs` closes and
/// the last task ends.
async fn dispatch<N, C, R, E, EF, S, CR, CF>(
    mut jobs: mpsc::UnboundedReceiver<Job>,
    limit: Arc<Semaphore>,
    handlers: Dispatch<N, C, R, E, S, CR>,
) where
    N: Fn(&Job),
    C: FnMut() -> Option<QueuedRun>,
    R: Fn(&QueuedRun) -> Option<Arc<Stopper>>,
    E: Fn(QueuedRun, Arc<Stopper>, OwnedSemaphorePermit) -> EF,
    EF: Future<Output = ()> + Send + 'static,
    S: Fn(&Job) -> bool,
    CR: Fn(QueuedRun, String) -> CF,
    CF: Future<Output = ()>,
{
    let Dispatch {
        noticed,
        mut claim,
        register,
        execute,
        claim_started,
        crashed,
    } = handlers;
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
    let mut jump: VecDeque<Job> = VecDeque::new();
    // Claimed, waiting only for a permit: taken from the store when `jobs`
    // has closed and the queue is being drained.
    let mut ready: VecDeque<QueuedRun> = VecDeque::new();
    // Cleared when there's nothing to take, so the permit arm doesn't spin
    // on an empty queue; set again by a job or a finish.
    let mut work = true;
    let mut open = true;
    loop {
        // Nothing more will arrive: finish what's queued, then stop.
        if !open && jump.is_empty() && ready.is_empty() && tasks.is_empty() {
            match claim() {
                Some(run) => {
                    ready.push_back(run);
                    work = true;
                }
                None => break,
            }
        }
        tokio::select! {
            biased;
            Some(done) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                finished(done, &mut started).await;
                work = true;
            }
            job = jobs.recv(), if open => match job {
                Some(job) => {
                    noticed(&job);
                    // An automatic job is only a nudge: the store holds the
                    // order, and `claim` takes it from there.
                    if !job.automatic {
                        jump.push_back(job);
                    }
                    work = true;
                }
                None => open = false,
            },
            Ok(permit) = Arc::clone(&limit).acquire_owned(), if work => {
                // A hand start that can't run, or one the registration
                // refused, leaves the store's queue still worth a look.
                let mut tried = false;
                let taken = match jump.pop_front() {
                    Some(job) => {
                        tried = true;
                        claim_started(&job).then_some(job.run)
                    }
                    None => ready.pop_front().or_else(&mut claim),
                };
                tried |= taken.is_some();
                if let Some((run, stop)) =
                    taken.and_then(|run| register(&run).map(|stop| (run, stop)))
                {
                    let task =
                        tasks.spawn(execute(run.clone(), stop, permit).instrument(run_span(&run)));
                    started.insert(task.id(), run);
                } else {
                    drop(permit);
                    work = tried;
                }
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

fn log_draft_revision(revision: &Revision, revised: &DraftRevision) {
    let draft = revision.draft.unwrap_or_default();
    match revised {
        DraftRevision::Dropped { reason } => {
            let headline = reason.lines().next().unwrap_or_default();
            info!(draft, "draft dropped: {headline}");
        }
        DraftRevision::Summary { .. } | DraftRevision::Comment(_) => {
            info!(draft, "draft revised");
        }
    }
}

fn log_no_update(result: &ReviewResult) {
    let headline = result.summary.lines().next().unwrap_or_default();
    info!("no update since the review it resumed: {headline}");
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
            revision: None,
            resume: None,
            worktree: None,
            lineage: vec![],
        }
    }

    #[tokio::test]
    async fn runs_after_a_panicking_run_still_run() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(by_hand(&queued(1))).unwrap();
        // Run 2 is only sent once run 1's crash is handled, and closing the
        // channel then lets `dispatch` return.
        let tx = Mutex::new(Some(tx));
        let ran = Arc::new(Mutex::new(Vec::new()));
        let crashes = Mutex::new(Vec::new());
        dispatch(
            rx,
            Arc::new(Semaphore::new(1)),
            Dispatch {
                noticed: |_: &Job| {},
                claim: || None,
                register: |_: &QueuedRun| Some(Arc::new(Stopper::default())),
                execute: |run: QueuedRun, _stop, permit| {
                    let ran = Arc::clone(&ran);
                    async move {
                        let _permit = permit;
                        assert!(run.id != 1, "boom");
                        ran.lock().unwrap().push(run.id);
                    }
                },
                claim_started: |_: &Job| true,
                crashed: |run: QueuedRun, panic| {
                    crashes.lock().unwrap().push((run.id, panic));
                    if let Some(tx) = tx.lock().unwrap().take() {
                        tx.send(by_hand(&queued(2))).unwrap();
                    }
                    async {}
                },
            },
        )
        .await;
        assert_eq!(*ran.lock().unwrap(), [2]);
        assert_eq!(*crashes.lock().unwrap(), [(1, "boom".to_owned())]);
    }

    /// `run`, started by hand.
    /// Runs `dispatch` over `jobs`, taking `pending` as the store's queue,
    /// and returns the run ids it started in order. `startable` says
    /// whether a hand-started job may be claimed.
    async fn dispatched(jobs: Vec<Job>, pending: Vec<QueuedRun>, startable: bool) -> Vec<i64> {
        let (tx, rx) = mpsc::unbounded_channel();
        for job in jobs {
            tx.send(job).unwrap();
        }
        drop(tx);
        let queue = Mutex::new(VecDeque::from(pending));
        let ran = Arc::new(Mutex::new(Vec::new()));
        dispatch(
            rx,
            Arc::new(Semaphore::new(1)),
            Dispatch {
                noticed: |_: &Job| {},
                claim: || queue.lock().unwrap().pop_front(),
                register: |_: &QueuedRun| Some(Arc::new(Stopper::default())),
                execute: |run: QueuedRun, _stop, permit| {
                    let ran = Arc::clone(&ran);
                    async move {
                        let _permit = permit;
                        ran.lock().unwrap().push(run.id);
                    }
                },
                claim_started: |_: &Job| startable,
                crashed: |_: QueuedRun, _| async {},
            },
        )
        .await;
        let ran = ran.lock().unwrap();
        ran.clone()
    }

    #[tokio::test]
    async fn the_queue_is_taken_in_order() {
        let ran = dispatched(vec![], vec![queued(1), queued(2), queued(3)], true).await;
        assert_eq!(ran, [1, 2, 3]);
    }

    #[tokio::test]
    async fn a_hand_started_run_goes_before_the_queue() {
        let ran = dispatched(vec![by_hand(&queued(9))], vec![queued(1)], true).await;
        assert_eq!(ran, [9, 1]);
    }

    /// A hand start that can't be claimed must not strand the store's
    /// queue: there is a free slot and a run waiting for it.
    #[tokio::test]
    async fn a_skipped_hand_start_still_leaves_the_queue_running() {
        let ran = dispatched(vec![by_hand(&queued(9))], vec![queued(1)], false).await;
        assert_eq!(ran, [1]);
    }

    fn by_hand(run: &QueuedRun) -> Job {
        Job {
            run: run.clone(),
            automatic: false,
        }
    }

    /// A store tracking [`queued`]'s PR, with a review of it queued.
    fn store_with_queued_review() -> (Store, QueuedRun) {
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
            review_decision: None,
            merge_state: None,
            checks: None,
            in_progress: None,
        };
        store.record(&snapshot, "me", "p", &[]).unwrap();
        let run = store.queue_review(&queued(0).request).unwrap().unwrap();
        (store, run)
    }

    #[tokio::test]
    async fn a_crashed_run_is_recorded_as_crashed() {
        let data = tempfile::TempDir::new().unwrap();
        let (store, run) = store_with_queued_review();
        assert!(store.claim_run(run.id).unwrap());
        let store = Arc::new(Mutex::new(store));
        let worker = Worker::new(data.path(), Arc::clone(&store), &config(1, "p"), "me");

        worker.crashed(run.clone(), "boom".into()).await;
        let record = store.lock().unwrap().run(run.id).unwrap().unwrap();
        assert_eq!(record.status, "crashed");
        assert_eq!(record.error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn automatic_runs_stay_queued_while_manual_reviews_are_on() {
        let data = tempfile::TempDir::new().unwrap();
        let (store, run) = store_with_queued_review();
        let store = Arc::new(Mutex::new(store));
        // Unset in the config, so on.
        let worker = Arc::new(Worker::new(
            data.path(),
            Arc::clone(&store),
            &config(1, "p"),
            "me",
        ));
        let (jobs, jobs_rx) = mpsc::unbounded_channel();
        jobs.send(Job {
            run: run.clone(),
            automatic: true,
        })
        .unwrap();
        drop(jobs);
        worker.work(jobs_rx).await;
        let record = store.lock().unwrap().run(run.id).unwrap().unwrap();
        assert_eq!(record.status, "queued");
    }

    #[tokio::test]
    async fn a_profiles_own_manual_reviews_hold_its_runs_with_the_runners_off() {
        let data = tempfile::TempDir::new().unwrap();
        let (store, run) = store_with_queued_review();
        let store = Arc::new(Mutex::new(store));
        let text = format!(
            "[runner]\nmanual_reviews = false\n\
             [profile.{}]\nmanual_reviews = true\nrepos = [{{ github = \"org\" }}]\n",
            run.request.profile
        );
        let config = Config::parse(&text, Path::new("/"), &NoCheckouts).unwrap();
        let worker = Arc::new(Worker::new(data.path(), Arc::clone(&store), &config, "me"));
        let (jobs, jobs_rx) = mpsc::unbounded_channel();
        jobs.send(Job {
            run: run.clone(),
            automatic: true,
        })
        .unwrap();
        drop(jobs);
        worker.work(jobs_rx).await;
        let record = store.lock().unwrap().run(run.id).unwrap().unwrap();
        assert_eq!(record.status, "queued");
    }

    #[tokio::test]
    async fn reloads_apply_to_later_runs() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let worker = Worker::new(Path::new("/data"), store, &config(2, "old"), "me");
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

    #[tokio::test]
    async fn a_reload_supersedes_queued_reviews_of_skipped_authors() {
        let (store, run) = store_with_queued_review();
        let store = Arc::new(Mutex::new(store));
        let worker = Worker::new(
            Path::new("/data"),
            Arc::clone(&store),
            &config(1, "p"),
            "me",
        );
        let record = |store: &Mutex<Store>| store.lock().unwrap().run(run.id).unwrap().unwrap();

        let text = "[review_requests]\nauthors = [\"*\", \"!Alice\"]\n\
                    [profile.p]\nrepos = [{ github = \"org\" }]\n";
        let skipping = Config::parse(text, Path::new("/"), &NoCheckouts)
            .unwrap()
            .skip_rules();
        let listing = config(1, "p").skip_rules();
        worker.supersede_newly_unlisted(&listing, &listing);
        assert_eq!(record(&store).status, "queued");
        // Already unlisted, as a review started by hand from its page is.
        worker.supersede_newly_unlisted(&skipping, &skipping);
        assert_eq!(record(&store).status, "queued");
        worker.supersede_newly_unlisted(&listing, &skipping);
        assert_eq!(record(&store).status, "superseded");
        assert!(store.lock().unwrap().queued_reviews().unwrap().is_empty());
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

    /// Stops a running review either by cancelling it or by shutting down,
    /// and returns its status and whether its worktree survived.
    async fn stop_running_review(cancel: bool) -> (String, bool) {
        let dir = tempfile::TempDir::new().unwrap();
        let (base, first, _) = pushed_twice(&dir.path().join("github"));
        let fake = dir.path().join("fake");
        let script = hangs_on(&fake, &first);
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
        let mut snapshot = sanic_core::pr::PrSnapshot {
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
            review_decision: None,
            merge_state: None,
            checks: None,
            in_progress: None,
        };
        snapshot.head_sha = first.clone();
        store.record(&snapshot, "me", "p", &[]).unwrap();
        let store = Arc::new(Mutex::new(store));
        let run = store
            .lock()
            .unwrap()
            .queue_review(&sanic_core::run::ReviewRequest {
                key,
                profile: "p".into(),
                head_sha: first,
                base_sha: base,
                trigger: sanic_core::run::ReviewTrigger::Requested,
            })
            .unwrap()
            .unwrap();

        let data = dir.path().join("data");
        let worker = Arc::new(Worker::new(&data, Arc::clone(&store), &config, "me"));
        let (runs, runs_rx) = mpsc::unbounded_channel();
        let working = tokio::spawn(Arc::clone(&worker).work(runs_rx));
        runs.send(by_hand(&run)).unwrap();
        while !fake.join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if cancel {
            store.lock().unwrap().cancel_run(run.id).unwrap();
            assert!(worker.cancel(run.id), "the run wasn't registered");
        } else {
            worker.cancel_all();
        }
        drop(runs);
        tokio::time::timeout(std::time::Duration::from_secs(20), working)
            .await
            .expect("the review was not stopped")
            .unwrap();

        let status = store.lock().unwrap().run(run.id).unwrap().unwrap().status;
        assert!(store.lock().unwrap().drafts(run.id).unwrap().is_empty());
        (status, data.join(format!("worktrees/{}", run.id)).exists())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_running_review_records_it_cancelled() {
        let (status, worktree) = stop_running_review(true).await;
        assert_eq!(status, "cancelled");
        assert!(!worktree, "the worktree was left behind");
    }

    /// The same stop by shutdown instead, so the two can't be confused: a
    /// shutdown queues the run again, a cancel ends it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_review_stopped_by_shutdown_is_queued_again() {
        let (status, _) = stop_running_review(false).await;
        assert_eq!(status, "queued");
    }

    /// A held review started by hand twice reaches the worker twice. With a
    /// free slot for the second copy, it must not take over the first's
    /// registration, or the newer head couldn't stop the first.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_sent_twice_can_still_be_stopped() {
        newer_head_stops_older(2, 2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_regeneration_gets_drafts_of_its_own() {
        let dir = tempfile::TempDir::new().unwrap();
        let (base, first, _) = pushed_twice(&dir.path().join("github"));
        let fake = dir.path().join("fake");
        // Never hangs: nothing it's sent mentions this.
        let script = hangs_on(&fake, "no-such-head");
        let config = Config::parse(
            &format!(
                "[github]\ngit_url = \"{}\"\n[runner]\nclaude = \"{}\"\n\
                 [profile.p]\nrepos = [{{ github = \"org\" }}]\n",
                dir.path().join("github").display(),
                script.display()
            ),
            Path::new("/"),
            &NoCheckouts,
        )
        .unwrap();
        let mut source = queued(0);
        source.request.head_sha.clone_from(&first);
        source.request.base_sha.clone_from(&base);
        let mut store = Store::open_in_memory().unwrap();
        let snapshot = sanic_core::pr::PrSnapshot {
            key: source.request.key.clone(),
            title: "t".into(),
            body: String::new(),
            url: source.request.key.url(),
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
            review_decision: None,
            merge_state: None,
            checks: None,
            in_progress: None,
        };
        store.record(&snapshot, "me", "p", &[]).unwrap();
        let source = store.queue_review(&source.request).unwrap().unwrap();
        store.claim_run(source.id).unwrap();
        let original = ReviewResult {
            summary: "Original.".into(),
            summary_note: None,
            verdict: sanic_core::run::Verdict::Comment,
            comments: vec![],
            session_id: Some("sess-0".into()),
            transcript_path: "t".into(),
            resumed_from: None,
        };
        store.finish_review(source.id, &original).unwrap();
        let sanic_store::Regeneration::Queued(run) = store
            .queue_regeneration(source.id, "Be terser.", |_| false)
            .unwrap()
        else {
            panic!("refused");
        };
        let store = Arc::new(Mutex::new(store));

        let worker = Arc::new(Worker::new(
            &dir.path().join("data"),
            Arc::clone(&store),
            &config,
            "me",
        ));
        let (runs, runs_rx) = mpsc::unbounded_channel();
        runs.send(by_hand(&run)).unwrap();
        drop(runs);
        tokio::time::timeout(std::time::Duration::from_secs(20), worker.work(runs_rx))
            .await
            .unwrap();

        let store = store.lock().unwrap();
        assert_eq!(store.run(run.id).unwrap().unwrap().status, "succeeded");
        assert_eq!(store.drafts(run.id).unwrap()[0].body, "fine");
        // The source run and its drafts are as they were.
        let before = store.run(source.id).unwrap().unwrap();
        assert_eq!(before.session_id.as_deref(), Some("sess-0"));
        assert_eq!(store.drafts(source.id).unwrap()[0].body, "Original.");
        let sent = briefs(&fake);
        assert!(sent.contains("Be terser."), "{sent}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_review_resumes_the_last_reviews_session() {
        let pushed = review_a_push(Some(&nothing_new())).await;
        let store = pushed.store.lock().unwrap();
        let record = store.run(pushed.run).unwrap().unwrap();
        assert_eq!(record.status, "succeeded");
        // Nothing new: the drafts of the review it resumed, carried to the
        // new head. The line the comment was on changed.
        let drafts = store.drafts(pushed.run).unwrap();
        let bodies: Vec<&str> = drafts.iter().map(|d| d.body.as_str()).collect();
        assert_eq!(bodies, ["Original.", "On the old line."]);
        assert!(drafts[1].unanchored);
        assert_eq!(drafts[1].status, "accepted");
        assert_eq!(drafts[1].based_on, Some(pushed.source_comment));
        let runs = store.review_runs(&queued(0).request.key).unwrap();
        assert_eq!(runs[0].no_update.as_deref(), Some("fine"));
        let args = std::fs::read_to_string(pushed.fake.join("args")).unwrap();
        assert!(args.contains("--resume\nsess-0\n--fork-session"), "{args}");
        // Where the session lives.
        let cwd = std::fs::read_to_string(pushed.fake.join("cwd")).unwrap();
        assert!(
            cwd.trim()
                .ends_with(&format!("worktrees/{}", pushed.source)),
            "{cwd}"
        );
        let sent = briefs(&pushed.fake);
        assert!(sent.contains("# New commits on"), "{sent}");
        assert!(sent.contains("## What changed since"), "{sent}");
        assert!(sent.contains("## Your last review's drafts"), "{sent}");
        assert!(sent.contains("On the old line."), "{sent}");
        // A chat with it resumes its session there too.
        let chat = store
            .latest_session_run(&queued(0).request.key)
            .unwrap()
            .unwrap();
        assert_eq!(chat.run.id, pushed.run);
        assert_eq!(chat.run.worktree_run(), pushed.source);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_review_whose_session_is_gone_reviews_afresh() {
        let pushed = review_a_push(None).await;
        let store = pushed.store.lock().unwrap();
        assert_eq!(store.run(pushed.run).unwrap().unwrap().status, "succeeded");
        // A fresh review's `none` is a review, with its summary to post.
        assert_eq!(store.drafts(pushed.run).unwrap()[0].body, "fine");
        let runs = store.review_runs(&queued(0).request.key).unwrap();
        assert_eq!(runs[0].no_update, None);
        let sent = briefs(&pushed.fake);
        assert!(sent.contains("# New commits on"), "{sent}");
        assert!(
            sent.contains("Review the whole PR as it now stands"),
            "{sent}"
        );
        let cwd = std::fs::read_to_string(pushed.fake.join("cwd")).unwrap();
        assert!(
            cwd.trim().ends_with(&format!("worktrees/{}", pushed.run)),
            "{cwd}"
        );
        let chat = store
            .latest_session_run(&queued(0).request.key)
            .unwrap()
            .unwrap();
        assert_eq!(chat.run.worktree_run(), pushed.run);
    }

    struct Pushed {
        _dir: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        fake: PathBuf,
        /// The review whose session the push review may resume.
        source: i64,
        /// The push review.
        run: i64,
        /// `source`'s comment draft.
        source_comment: i64,
    }

    /// Reviews the first head, with session `sess-0`, then a push of the
    /// second, answered `none` with no comments, by a fake `claude` that
    /// says the session is gone when `gone`.
    async fn review_a_push(resumed: Option<&serde_json::Value>) -> Pushed {
        let dir = tempfile::TempDir::new().unwrap();
        let (base, first, second) = pushed_twice(&dir.path().join("github"));
        let fake = dir.path().join("fake");
        let script = resuming_fake(&fake, resumed);
        let config = Config::parse(
            &format!(
                "[github]\ngit_url = \"{}\"\n[runner]\nclaude = \"{}\"\n\
                 [profile.p]\nrepos = [{{ github = \"org\" }}]\n",
                dir.path().join("github").display(),
                script.display()
            ),
            Path::new("/"),
            &NoCheckouts,
        )
        .unwrap();
        let mut request = queued(0).request;
        request.head_sha.clone_from(&second);
        request.base_sha.clone_from(&base);
        let mut store = Store::open_in_memory().unwrap();
        let snapshot = sanic_core::pr::PrSnapshot {
            key: request.key.clone(),
            title: "t".into(),
            body: String::new(),
            url: request.key.url(),
            author: "alice".into(),
            head_sha: second.clone(),
            base_sha: base.clone(),
            is_draft: false,
            review_requested: true,
            requested_teams: vec![],
            reviews: vec![],
            threads: vec![],
            files: None,
            updated_at: None,
            review_decision: None,
            merge_state: None,
            checks: None,
            in_progress: None,
        };
        store.record(&snapshot, "me", "p", &[]).unwrap();
        let mut earlier = request.clone();
        earlier.head_sha.clone_from(&first);
        let source = store.queue_review(&earlier).unwrap().unwrap();
        store.claim_run(source.id).unwrap();
        let original = ReviewResult {
            summary: "Original.".into(),
            summary_note: None,
            verdict: sanic_core::run::Verdict::Comment,
            comments: vec![sanic_core::run::DraftComment {
                comment: sanic_core::run::InlineComment {
                    path: "lib.rs".into(),
                    line: 1,
                    start_line: None,
                    side: sanic_core::run::Side::Right,
                    body: "On the old line.".into(),
                    severity: sanic_core::run::Severity::Nit,
                    confidence: sanic_core::run::Confidence::High,
                    note: None,
                },
                unanchored: false,
            }],
            session_id: Some("sess-0".into()),
            transcript_path: "t".into(),
            resumed_from: None,
        };
        store.finish_review(source.id, &original).unwrap();
        let source_comment = store.drafts(source.id).unwrap()[1].id;
        store
            .set_draft_status(source_comment, sanic_store::DraftStatus::Accepted)
            .unwrap();
        request.trigger = ReviewTrigger::Push { from_sha: first };
        let run = store.queue_review(&request).unwrap().unwrap();
        let store = Arc::new(Mutex::new(store));

        let data = dir.path().join("data");
        let worker = Arc::new(Worker::new(&data, Arc::clone(&store), &config, "me"));
        let (runs, runs_rx) = mpsc::unbounded_channel();
        runs.send(by_hand(&run)).unwrap();
        drop(runs);
        tokio::time::timeout(std::time::Duration::from_secs(20), worker.work(runs_rx))
            .await
            .unwrap();
        Pushed {
            _dir: dir,
            store,
            fake,
            source: source.id,
            run: run.id,
            source_comment,
        }
    }

    /// A fake `claude` in `fake` that answers `none` with no comments,
    /// and says the session is gone when resuming one, if `gone`.
    fn resuming_fake(fake: &Path, resumed: Option<&serde_json::Value>) -> PathBuf {
        std::fs::create_dir(fake).unwrap();
        let answering = |output: &serde_json::Value| {
            serde_json::json!({
                "type": "result", "subtype": "success", "is_error": false, "num_turns": 2,
                "session_id": "sess-1", "structured_output": output
            })
        };
        let answer = answering(&nothing_new());
        let missing = serde_json::json!({
            "type": "result", "subtype": "error_during_execution", "is_error": true,
            "num_turns": 0, "errors": ["No conversation found with session ID: sess-0"]
        });
        let resumed = resumed.map_or(missing, answering);
        std::fs::write(fake.join("answer.jsonl"), format!("{answer}\n")).unwrap();
        std::fs::write(fake.join("resumed.jsonl"), format!("{resumed}\n")).unwrap();
        let script = fake.join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nd='{}'\ncat > \"$d/stdin.$$\"\npwd > \"$d/cwd\"\n\
                 case \" $* \" in *\" --resume \"*) printf '%s\\n' \"$@\" > \"$d/args\"; \
                 cat \"$d/resumed.jsonl\";; *) cat \"$d/answer.jsonl\";; esac\n",
                fake.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_review_with_no_update_can_dismiss_a_stale_summary() {
        let answer = serde_json::json!({
            "summary": "Only a rebase.", "suggested_verdict": "none", "comments": [],
            "dismiss": [{ "id": 1, "reason": "The concern it describes was resolved." }]
        });
        let pushed = review_a_push(Some(&answer)).await;
        let store = pushed.store.lock().unwrap();
        let drafts = store.draft_rows(pushed.run).unwrap();
        let states: Vec<(&str, &str, Option<&str>)> = drafts
            .iter()
            .map(|d| (d.body(), d.status.as_str(), d.obsolete.as_deref()))
            .collect();
        assert_eq!(
            states,
            [
                (
                    "Original.",
                    "dismissed",
                    Some("The concern it describes was resolved.")
                ),
                ("On the old line.", "accepted", None),
            ]
        );
        let runs = store.review_runs(&queued(0).request.key).unwrap();
        assert_eq!(runs[0].no_update.as_deref(), Some("Only a rebase."));
        let sent = briefs(&pushed.fake);
        assert!(sent.contains("list it in `dismiss`"), "{sent}");
    }

    /// A resumed review's answer when nothing changed.
    fn nothing_new() -> serde_json::Value {
        serde_json::json!({ "summary": "fine", "suggested_verdict": "none", "comments": [] })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_review_keeps_an_accepted_draft_it_reissues_accepted() {
        let reissued = |based_on: i64| {
            serde_json::json!({
                "summary": "Still one thing.", "suggested_verdict": "comment",
                "comments": [
                    { "path": "lib.rs", "line": 1, "side": "RIGHT", "body": "On the old line.",
                      "severity": "nit", "confidence": "high", "based_on": based_on },
                    { "path": "lib.rs", "line": 1, "side": "RIGHT", "body": "Something new.",
                      "severity": "nit", "confidence": "high" }
                ]
            })
        };
        // The source's drafts are 1 and 2, the comment accepted.
        let pushed = review_a_push(Some(&reissued(2))).await;
        let store = pushed.store.lock().unwrap();
        assert_eq!(pushed.source_comment, 2);
        let drafts = store.drafts(pushed.run).unwrap();
        let kept: Vec<(&str, &str, Option<i64>)> = drafts
            .iter()
            .map(|d| (d.body.as_str(), d.status.as_str(), d.based_on))
            .collect();
        assert_eq!(
            kept,
            [
                ("Still one thing.", "pending", None),
                ("On the old line.", "accepted", Some(2)),
                ("Something new.", "pending", None),
            ]
        );
        let runs = store.review_runs(&queued(0).request.key).unwrap();
        assert_eq!(runs[0].no_update, None);
    }

    /// Every brief the fake `claude` in `fake` was sent.
    fn briefs(fake: &Path) -> String {
        std::fs::read_dir(fake)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("stdin."))
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect()
    }

    /// A fake `claude` in `fake` that hangs until killed when briefed on
    /// `head`, and answers on any other.
    fn hangs_on(fake: &Path, head: &str) -> PathBuf {
        std::fs::create_dir(fake).unwrap();
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
                 if grep -q {head} \"$d/stdin.$$\"; then touch \"$d/started\"; exec sleep 30; fi\n\
                 cat \"$d/answer.jsonl\"\n",
                fake.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }

    /// Sends the review of the first head `sends` times, then queues the
    /// second head once the first is under way, and checks it stopped the
    /// first.
    async fn newer_head_stops_older(sends: usize, max_concurrent: usize) {
        let dir = tempfile::TempDir::new().unwrap();
        let (base, first, second) = pushed_twice(&dir.path().join("github"));
        let fake = dir.path().join("fake");
        let script = hangs_on(&fake, &first);
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
            review_decision: None,
            merge_state: None,
            checks: None,
            in_progress: Some(sanic_core::pr::InProgressReview {
                id: "PRR_1".into(),
                comments: vec![sanic_core::pr::InProgressComment {
                    id: "PRRC_1".into(),
                    path: "lib.rs".into(),
                    line: Some(1),
                    start_line: None,
                    outdated: false,
                    body: "My own pending comment.".into(),
                }],
            }),
        };
        store.record(&snapshot, "me", "p", &[]).unwrap();
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
        let worker = Arc::new(Worker::new(&data, Arc::clone(&store), &config, "me"));
        let (runs, runs_rx) = mpsc::unbounded_channel();
        let working = tokio::spawn(Arc::clone(&worker).work(runs_rx));
        let old = queue(&first);
        for _ in 0..sends {
            runs.send(by_hand(&old)).unwrap();
        }
        while !fake.join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let new = queue(&second);
        runs.send(by_hand(&new)).unwrap();
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
        // Its brief had your pending review, and the run says so.
        let runs = store.review_runs(&key).unwrap();
        assert_eq!(runs[0].id, new.id);
        assert_eq!(runs[0].in_progress_comments, Some(1));
        assert!(briefs(&fake).contains("My own pending comment."));
    }
}
