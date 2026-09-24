//! The foreground `serve` loop.
//!
//! One task does all the polling in turn: reconcile and notification polls
//! queue PRs, then each queued PR is refreshed. Keeping it sequential means
//! a PR is never refreshed twice at once. A config file change is picked up
//! between cycles. Refreshes with triggers go to the scheduler, which
//! debounces them into queued runs for the worker.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr},
};
use sanic_core::{
    clock::{RecencyWindow, SystemClock, WindowChoice},
    config::{Config, default_config_path, default_data_dir},
    pr::PrKey,
    run::QueuedRun,
    skip::SkipRules,
    trigger::Trigger,
};
use sanic_github::{ApiError, Client, Token};
use sanic_runner::{mirror::Mirrors, review::RunSettings, vcs::VcsResolver};
use sanic_store::{Refusal, Regeneration, Store};
use sanic_web::Dashboard;
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep_until},
};
use tracing::{Instrument, info, info_span, warn};

use crate::{
    ServeArgs, Ui, config_edit, logging,
    poll::{GithubApi, Poller, Priority, Progress, RefreshQueue, Refreshed},
    schedule::{DueTimes, Update, schedule, standing_request},
    tui::{self, Request, Shared, Sizes, SystemBrowser, Tui},
    watch::ConfigWatcher,
    work::Worker,
};

#[allow(
    clippy::too_many_lines,
    reason = "it starts each of serve's tasks in turn; splitting it only scatters the wiring"
)]
pub async fn run(args: ServeArgs) -> Result<()> {
    // Absolute, because runs hand these paths to `git -C <mirror>` and to
    // `claude` running in a worktree, where relative ones would resolve
    // somewhere else.
    let data_dir = std::path::absolute(match args.data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    })
    .wrap_err("resolving the data directory")?;
    // Before anything logs, so the TUI's log pane and file get it all.
    let logs = match args.ui {
        Ui::Logs => {
            logging::init_stdout();
            None
        }
        Ui::Tui => {
            std::fs::create_dir_all(&data_dir)
                .wrap_err_with(|| format!("creating data directory {}", data_dir.display()))?;
            Some(logging::init_tui(&data_dir.join("serve.log"))?)
        }
    };
    let config_path = std::path::absolute(match args.config {
        Some(path) => path,
        None => default_config_path()?,
    })
    .wrap_err("resolving the config path")?;
    let config = Config::load(&config_path, &VcsResolver)?;
    let db_path = data_dir.join("state.db");
    let store = Store::open(&db_path)?;
    // The scheduler and worker share a second connection, so the poller
    // never waits on them.
    let run_store = Arc::new(Mutex::new(Store::open(&db_path)?));
    let github = Client::new(&config.github.api_url, Token::discover()?)?;
    let me = github
        .viewer_login()
        .await
        .wrap_err("identifying the GitHub user")?;
    info!(user = %me, config = %config_path.display(), "watching GitHub");

    let mut watcher = ConfigWatcher::new(&config_path)?;
    let (updates, updates_rx) = mpsc::unbounded_channel();
    let (runs, runs_rx) = mpsc::unbounded_channel();
    // With `--manual-reviews`, reviews started by hand skip the hold.
    let (started, started_rx) = if args.manual_reviews {
        let (started, started_rx) = mpsc::unbounded_channel();
        (started, Some(started_rx))
    } else {
        (runs.clone(), None)
    };
    resume_queued(&run_store, args.manual_reviews, &runs)?;
    let worker = Arc::new(Worker::new(&data_dir, Arc::clone(&run_store), &config));

    let (requests, requests_rx) = mpsc::unbounded_channel();
    let (refreshes, mut refreshes_rx) = mpsc::unbounded_channel();
    let chatting = Arc::new(AtomicBool::new(false));
    let (due_tx, due) = watch::channel(DueTimes::new());
    let live = Arc::new(Live::new(&config, store.window_choice()?));
    let control = DashboardControl {
        requests: requests.clone(),
        refreshes,
        store: Arc::clone(&run_store),
        started: started.clone(),
        data_dir: data_dir.clone(),
        config_path: config_path.clone(),
        worker: Arc::clone(&worker),
        live: Arc::clone(&live),
    };
    // Bound before the TUI starts, so it knows the address to open.
    let web = live
        .dashboard(&me, args.manual_reviews, &data_dir, &github, control, &due)?
        .bind(args.port)
        .await?;
    let mut ui = match logs {
        Some(logs) => Some(Tui::start(
            &db_path,
            Shared {
                me: me.clone(),
                manual_reviews: args.manual_reviews,
                logs,
                due,
                skips: live.skips.subscribe(),
                window: live.window.subscribe(),
                progress: live.progress.subscribe(),
                clock: Arc::new(SystemClock),
                requests,
                config_path: config_path.clone(),
                data_dir: data_dir.clone(),
                runtime: tokio::runtime::Handle::current(),
                chatting: Arc::clone(&chatting),
                dashboard: web.url(),
                opener: Arc::new(SystemBrowser),
            },
        )?),
        None => None,
    };
    let quit = async {
        match &mut ui {
            Some(ui) => ui.closed().await,
            None => std::future::pending().await,
        }
    };

    let me_for_scheduler = me.clone();
    let mut poller = Poller::new(github, store, config, me);
    // Kept past the `select!`, so running reviews can wind down cleanly
    // rather than be dropped midway.
    let reviews = run_reviews(Arc::clone(&worker), runs_rx, started_rx);
    tokio::pin!(reviews);
    let result = tokio::select! {
        result = quit => {
            info!("shutting down");
            result
        }
        result = poll_forever(
            &mut poller, &mut watcher, &config_path, &updates, &mut refreshes_rx, &worker, &live,
        ) => result,
        result = schedule(
            updates_rx, Arc::clone(&run_store), runs.clone(), due_tx, live.skips.subscribe(),
            args.manual_reviews, me_for_scheduler,
        ) => result,
        () = handle_requests(requests_rx, run_store, Starter {
            manual: args.manual_reviews,
            start: started,
        }) => Ok(()),
        () = &mut reviews => Ok(()),
        result = web.serve() => result,
        () = interrupted(&chatting) => {
            info!("shutting down");
            Ok(())
        }
    };
    // Restores the terminal before anything is printed, however `serve`
    // stopped: the TUI only gives it back by itself when you quit it.
    drop(ui);
    cancel_running(&worker, args.ui == Ui::Tui).await;
    result
}

/// Resolves on Ctrl-C, except while a TUI chat has the terminal: that
/// Ctrl-C is the agent's.
async fn interrupted(chatting: &AtomicBool) {
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            return std::future::pending().await;
        }
        if !chatting.load(Ordering::SeqCst) {
            return;
        }
    }
}

/// How long running reviews get to stop before `serve` exits anyway.
const CANCEL_LIMIT: Duration = Duration::from_secs(10);

/// Stops running reviews on the way out: their agents are killed, their
/// worktrees removed, and their runs queued again for the next start. A
/// second Ctrl-C exits without waiting; dropping the reviews then still
/// kills the agents.
async fn cancel_running(worker: &Worker, tell_terminal: bool) {
    let cancelled = worker.cancel_all();
    if cancelled.is_empty() {
        return;
    }
    let urls: Vec<String> = cancelled.iter().map(PrKey::url).collect();
    if tell_terminal {
        eprintln!("cancelling running reviews: {}", urls.join(" "));
    }
    info!(
        reviews = %urls.join(" "),
        "cancelling {} running review{}; they run again on the next start",
        urls.len(),
        plural(urls.len(), "", "s")
    );
    tokio::select! {
        stopped = worker.wait_idle(CANCEL_LIMIT) => {
            if !stopped {
                warn!("reviews didn't stop in {CANCEL_LIMIT:?}; exiting anyway");
            }
        }
        _ = tokio::signal::ctrl_c() => warn!("exiting without waiting for reviews to stop"),
    }
}

/// What the scheduler and the TUI follow as it changes: config across
/// reloads, and the poller's progress.
struct Live {
    skips: watch::Sender<SkipRules>,
    /// The recency window in force, in days, for the TUI.
    window: watch::Sender<Option<u32>>,
    /// The same, with where it comes from, for the dashboard.
    recency: watch::Sender<RecencyWindow>,
    /// The refresh batch under way; `None` when the queue is empty.
    progress: watch::Sender<Option<Progress>>,
}

impl Live {
    /// With the window picked on the dashboard, if one is.
    fn new(config: &Config, choice: Option<WindowChoice>) -> Self {
        let recency = RecencyWindow {
            configured: config.poll.updated_within_days,
            choice,
        };
        Self {
            skips: watch::Sender::new(config.skip_rules()),
            window: watch::Sender::new(recency.days()),
            recency: watch::Sender::new(recency),
            progress: watch::Sender::new(None),
        }
    }

    fn publish(&self, config: &Config) {
        self.skips.send_replace(config.skip_rules());
        self.set_recency(|w| w.configured = config.poll.updated_within_days);
    }

    /// Picks a recency window, or with `None` goes back to the config's.
    fn choose_window(&self, choice: Option<WindowChoice>) {
        self.set_recency(|w| w.choice = choice);
    }

    /// Only a change is sent: the poller reconciles on each one.
    fn set_recency(&self, change: impl FnOnce(&mut RecencyWindow)) {
        let modified = self.recency.send_if_modified(|w| {
            let before = *w;
            change(w);
            *w != before
        });
        if modified {
            self.window.send_replace(self.recency.borrow().days());
        }
    }

    /// The dashboard, on its own store connection, seeing what the TUI
    /// sees.
    fn dashboard(
        &self,
        me: &str,
        manual_reviews: bool,
        data_dir: &Path,
        github: &Client,
        control: DashboardControl,
        due: &watch::Receiver<DueTimes>,
    ) -> Result<Dashboard> {
        Dashboard::new(sanic_web::Context {
            me: me.to_owned(),
            manual_reviews,
            data_dir: data_dir.to_owned(),
            config_path: control.config_path.clone(),
            store: Store::open(&data_dir.join("state.db"))?,
            github: github.clone(),
            control: Arc::new(control),
            sources: Arc::new(Mirrors::in_data_dir(data_dir)),
            due: due.clone(),
            skips: self.skips.subscribe(),
            window: self.recency.subscribe(),
            clock: Arc::new(SystemClock),
        })
    }
}

/// The dashboard starts reviews the way the TUI's `r` does, adds
/// `skip_titles` patterns the way its ignore editor does, and reads the
/// run settings its chat commands are built from.
struct DashboardControl {
    requests: mpsc::UnboundedSender<Request>,
    /// PRs to refresh from GitHub now, for the poller.
    refreshes: mpsc::UnboundedSender<PrKey>,
    /// For regenerations, which start at once and return their run.
    store: Arc<Mutex<Store>>,
    /// Straight to the worker, past any `--manual-reviews` hold.
    started: mpsc::UnboundedSender<QueuedRun>,
    data_dir: PathBuf,
    /// Edited in place; the watcher reloads it.
    config_path: PathBuf,
    /// Holds the run settings as the config last loaded.
    worker: Arc<Worker>,
    /// Where a recency window picked on the dashboard is published.
    live: Arc<Live>,
}

impl sanic_web::Control for DashboardControl {
    fn review_now(&self, key: PrKey) {
        // Only fails once `serve` is stopping.
        let _ = self.requests.send(Request::Rerun(key));
    }

    fn refresh(&self, key: PrKey) {
        info!(url = %key.url(), "refreshing after a post");
        // Only fails once `serve` is stopping.
        let _ = self.refreshes.send(key);
    }

    fn add_skip_title(&self, pattern: &str, profile: Option<&str>) -> Result<bool> {
        config_edit::add_skip_title_to_file(&self.config_path, profile, pattern)
    }

    fn run_settings(&self, profile: &str) -> Result<RunSettings> {
        self.worker.run_settings(profile)
    }

    fn set_window(&self, choice: Option<WindowChoice>) -> Result<()> {
        // Stored first: the reconcile it prompts reads it from there.
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_window_choice(choice)?;
        self.live.choose_window(choice);
        Ok(())
    }

    fn regenerate(
        &self,
        run_id: i64,
        draft: Option<i64>,
        instruction: &str,
    ) -> Result<Result<i64, Refusal>> {
        regenerate(
            &self.store,
            &self.data_dir,
            &self.started,
            (run_id, draft),
            instruction,
        )
    }
}

/// Queues a regeneration of run `source`, or of just `draft` of it, and
/// starts it, past any `--manual-reviews` hold, or says why not.
fn regenerate(
    store: &Mutex<Store>,
    data_dir: &Path,
    started: &mpsc::UnboundedSender<QueuedRun>,
    (source, draft): (i64, Option<i64>),
    instruction: &str,
) -> Result<Result<i64, Refusal>> {
    let in_use = |review| sanic_runner::chat::worktree_path(data_dir, review).exists();
    let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
    let queued = match draft {
        Some(draft) => store.queue_draft_regeneration(source, draft, instruction, in_use)?,
        None => store.queue_regeneration(source, instruction, in_use)?,
    };
    drop(store);
    match queued {
        Regeneration::Queued(run) => {
            let id = run.id;
            info!(url = %run.request.key.url(), run = id, source_run = source, "regenerating a review");
            // Only fails once `serve` is stopping.
            let _ = started.send(*run);
            Ok(Ok(id))
        }
        Regeneration::Refused(why) => {
            info!(source_run = source, "not regenerated: {why}");
            Ok(Err(why))
        }
    }
}

/// Counts a refresh batch and says when to log progress: only for big
/// batches, every [`ProgressLog::EVERY`] PRs or [`ProgressLog::INTERVAL`].
struct ProgressLog {
    done: usize,
    logged_at: Instant,
}

struct Step {
    progress: Progress,
    log: bool,
}

impl ProgressLog {
    /// Smaller batches finish too soon to need progress lines.
    const BIG: usize = 20;
    const EVERY: usize = 25;
    const INTERVAL: Duration = Duration::from_secs(10);

    fn new(now: Instant) -> Self {
        Self {
            done: 0,
            logged_at: now,
        }
    }

    /// One more PR is about to be refreshed, with `remaining` after it.
    fn step(&mut self, remaining: usize, now: Instant) -> Step {
        let progress = Progress {
            done: self.done,
            total: self.done + remaining + 1,
        };
        self.done += 1;
        let due = self.done.is_multiple_of(Self::EVERY) || now >= self.logged_at + Self::INTERVAL;
        let log = progress.total >= Self::BIG && progress.done > 0 && due;
        if log {
            self.logged_at = now;
        }
        Step { progress, log }
    }
}

/// Sends the runs a previous process left queued or running to `runs`.
fn resume_queued(
    store: &Mutex<Store>,
    manual: bool,
    runs: &mpsc::UnboundedSender<QueuedRun>,
) -> Result<()> {
    let recovered = store
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .recover_runs()?;
    if !recovered.is_empty() {
        if manual {
            info!(
                runs = recovered.len(),
                "queued runs held by --manual-reviews"
            );
        } else {
            info!(runs = recovered.len(), "resuming queued runs");
        }
    }
    for run in recovered {
        let _ = runs.send(run);
    }
    Ok(())
}

/// How often `serve` looks for `sanic-review review` requests.
const START_POLL: Duration = Duration::from_secs(2);

/// Where reviews started by hand go: straight to the worker, even with
/// `--manual-reviews`. Without it, that's the queue every run goes to.
struct Starter {
    manual: bool,
    start: mpsc::UnboundedSender<QueuedRun>,
}

/// Carries out what the TUI and `sanic-review review` ask for. Never
/// returns.
async fn handle_requests(
    mut requests: mpsc::UnboundedReceiver<Request>,
    store: Arc<Mutex<Store>>,
    starter: Starter,
) {
    // Open while the TUI or the dashboard can still send; `review`
    // requests come either way.
    let mut open = true;
    let mut poll = tokio::time::interval(START_POLL);
    loop {
        tokio::select! {
            request = requests.recv(), if open => match request {
                Some(Request::Rerun(key)) => review_now(&key, &store, &starter),
                Some(Request::Archive { key, archived }) => archive(&key, archived, &store),
                Some(Request::SaveLayout(sizes)) => save_layout(sizes, &store),
                None => open = false,
            },
            _ = poll.tick() => {
                let taken = store
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take_start_requests();
                match taken {
                    Ok(keys) => {
                        for key in keys {
                            review_now(&key, &store, &starter);
                        }
                    }
                    Err(err) => warn!("reading `sanic-review review` requests failed: {err:?}"),
                }
            }
        }
    }
}

fn archive(key: &PrKey, archived: bool, store: &Mutex<Store>) {
    let set = store
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .set_archived(key, archived);
    let url = key.url();
    match set {
        Ok(true) if archived => info!(url = %url, "archived"),
        Ok(true) => info!(url = %url, "unarchived"),
        Ok(false) => warn!(url = %url, "not archived: the PR isn't tracked"),
        Err(err) => warn!(url = %url, "archiving failed: {err:?}"),
    }
}

fn save_layout(sizes: Sizes, store: &Mutex<Store>) {
    let store = store.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err(err) = tui::save_layout(&store, sizes) {
        warn!("{err:?}");
    }
}

/// Starts a review of `key` now. With `--manual-reviews` that's its held
/// review if it has one; otherwise a full review of its current head is
/// queued, as the scheduler would, so a head that already has a queued,
/// running or succeeded review is left alone.
fn review_now(key: &PrKey, store: &Mutex<Store>, starter: &Starter) {
    let _span = info_span!("review_now", url = %key.url()).entered();
    let run = {
        let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
        let held = if starter.manual {
            store.queued_review(key)
        } else {
            Ok(None)
        };
        held.and_then(|held| match held {
            Some(run) => Ok(Some(run)),
            None => store.review_request(key).and_then(|req| match req {
                Some(req) => store.queue_review(&req),
                None => Ok(None),
            }),
        })
    };
    match run {
        Ok(Some(run)) => {
            info!(head = %run.request.head_sha, "review started by hand");
            let _ = starter.start.send(run);
        }
        Ok(None) => info!(
            "not started: the PR isn't tracked, or its head already has a queued, \
             running or finished review"
        ),
        Err(err) => warn!(url = %key.url(), "starting the review failed: {err:?}"),
    }
}

/// Runs queued reviews. With `--manual-reviews` they're only logged, and
/// stay queued in the store, and just the ones started by hand run.
async fn run_reviews(
    worker: Arc<Worker>,
    mut runs: mpsc::UnboundedReceiver<QueuedRun>,
    started: Option<mpsc::UnboundedReceiver<QueuedRun>>,
) {
    let Some(started) = started else {
        return worker.work(runs).await;
    };
    info!(
        "--manual-reviews: reviews are queued and held; start one with `r` in the TUI \
         or `sanic-review review <PR url>`"
    );
    let hold = async {
        while let Some(run) = runs.recv().await {
            info!(
                url = %run.request.key.url(),
                run = run.id,
                head = %run.request.head_sha,
                "review held (--manual-reviews)"
            );
        }
    };
    tokio::join!(hold, worker.work(started));
}

async fn poll_forever<G: GithubApi>(
    poller: &mut Poller<G>,
    watcher: &mut ConfigWatcher,
    config_path: &Path,
    updates: &mpsc::UnboundedSender<Update>,
    refreshes: &mut mpsc::UnboundedReceiver<PrKey>,
    worker: &Worker,
    live: &Live,
) -> Result<()> {
    let mut next_reconcile = Instant::now();
    let mut next_notifications = Instant::now();
    // Survives across cycles so a rate limit doesn't drop queued PRs.
    let mut pending = RefreshQueue::default();
    // PRs refreshed since startup; the first refresh of each checks for a
    // standing review request.
    let mut seen: HashSet<PrKey> = HashSet::new();
    let mut window = live.recency.subscribe();
    // Until when a rate limit pauses polling.
    let mut paused_until = Instant::now();

    loop {
        tokio::select! {
            () = sleep_until(next_reconcile.min(next_notifications)) => {}
            Ok(()) = window.changed() => {
                // A wider window takes in PRs nothing has fetched yet,
                // once any pause is over.
                next_reconcile = Instant::now().max(paused_until);
                continue;
            }
            () = watcher.changed() => {
                if reload(poller, config_path, worker, live) {
                    // New entries may cover PRs nothing has fetched yet.
                    next_reconcile = Instant::now().max(paused_until);
                }
                continue;
            }
            Some(key) = refreshes.recv() => {
                // First in the queue, and refreshed now rather than after
                // the next poll, unless a rate limit pauses polling: then
                // it waits in the queue for the pause to end.
                pending.push(key, Priority::Asked);
                if Instant::now() < paused_until {
                    continue;
                }
            }
        }
        let reconcile_every = poller.config().poll.reconcile_interval;
        let min_notification = poller.config().poll.min_notification_interval;
        let now = Instant::now();
        let mut backoff = None;

        if now >= next_reconcile {
            next_reconcile = now + reconcile_every;
            match poller.reconcile().instrument(info_span!("reconcile")).await {
                Ok(found) => {
                    let (requested, involved) = (found.requested.len(), found.involved.len());
                    pending.extend(found.requested, Priority::Requested);
                    pending.extend(found.involved, Priority::Involved);
                    info!(
                        "reconcile: {requested} review requests, {involved} involving you, \
                         {} queued",
                        pending.len()
                    );
                }
                Err(err) => backoff = handle(err)?,
            }
        }
        if backoff.is_none() && now >= next_notifications {
            match poller
                .poll_notifications()
                .instrument(info_span!("notifications"))
                .await
            {
                Ok((keys, interval)) => {
                    if !keys.is_empty() {
                        let count = keys.len();
                        pending.extend(keys, Priority::Notified);
                        info!("notifications: {count} PRs, {} queued", pending.len());
                    }
                    next_notifications = now + interval.unwrap_or_default().max(min_notification);
                }
                Err(err) => {
                    next_notifications = now + min_notification;
                    backoff = handle(err)?;
                }
            }
        }

        let triggered = if backoff.is_none() {
            let (triggered, wait) =
                refresh_queued(poller, &mut pending, refreshes, &mut seen, updates, live).await?;
            backoff = wait;
            triggered
        } else {
            0
        };
        if let Some(wait) = backoff {
            let resume = Instant::now() + wait;
            paused_until = resume;
            next_reconcile = next_reconcile.max(resume);
            next_notifications = next_notifications.max(resume);
        }
        if triggered > 0 {
            info!(
                tracked = poller.store().tracked_prs()?,
                triggers = triggered,
                queued = pending.len(),
                "summary"
            );
        }
    }
}

/// Refreshes queued PRs, most urgent first, until the queue is empty or a
/// rate limit says to wait. Returns the triggers found and that wait.
/// Refreshes the dashboard asks for during a long batch join the queue
/// before each pop, so they go next rather than after the batch.
async fn refresh_queued<G: GithubApi>(
    poller: &mut Poller<G>,
    pending: &mut RefreshQueue,
    asked: &mut mpsc::UnboundedReceiver<PrKey>,
    seen: &mut HashSet<PrKey>,
    updates: &mpsc::UnboundedSender<Update>,
    live: &Live,
) -> Result<(usize, Option<Duration>)> {
    let mut triggered = 0;
    let mut backoff = None;
    let mut progress = ProgressLog::new(Instant::now());
    while backoff.is_none() {
        take_asked(asked, pending);
        let Some((key, priority)) = pending.pop() else {
            break;
        };
        let step = progress.step(pending.len(), Instant::now());
        live.progress.send_replace(Some(step.progress));
        if step.log {
            info!("refreshed {}/{}", step.progress.done, step.progress.total);
        }
        match poller
            .refresh(&key)
            .instrument(info_span!("refresh", url = %key.url()))
            .await
        {
            Ok(Some(refreshed)) => {
                triggered += refreshed.triggers.len();
                log_triggers(&refreshed);
                let quiet = poller.config().poll.quiet_period;
                let mut update = Update::new(&refreshed, quiet);
                if seen.insert(key.clone())
                    && let Some(standing) = standing_request(&refreshed, poller.me())
                {
                    update.triggers.push(standing);
                }
                if !update.triggers.is_empty() {
                    // Only fails once the scheduler has stopped, which
                    // ends `serve` anyway.
                    let _ = updates.send(update);
                }
            }
            Ok(None) => {}
            Err(err @ (ApiError::RateLimited { .. } | ApiError::Unauthorized)) => {
                pending.push(key, priority);
                backoff = handle(err)?;
            }
            Err(err) => {
                warn!(url = %key.url(), "refresh failed: {err}");
            }
        }
    }
    live.progress.send_replace(None);
    Ok((triggered, backoff))
}

/// Queues every refresh the dashboard has asked for so far, first.
fn take_asked(asked: &mut mpsc::UnboundedReceiver<PrKey>, pending: &mut RefreshQueue) {
    while let Ok(key) = asked.try_recv() {
        pending.push(key, Priority::Asked);
    }
}

/// Swaps in the config at `path`, keeping the current one if the new one
/// doesn't load. Returns whether it changed.
fn reload<G: GithubApi>(poller: &mut Poller<G>, path: &Path, worker: &Worker, live: &Live) -> bool {
    match Config::load(path, &VcsResolver) {
        Ok(config) => {
            if config.github.api_url != poller.config().github.api_url {
                warn!("`github.api_url` changed; restart to use it");
            }
            worker.configure(&config);
            live.publish(&config);
            poller.set_config(config);
            info!(config = %path.display(), "config reloaded");
            true
        }
        Err(err) => {
            warn!("config change not applied, keeping the previous config: {err:?}");
            false
        }
    }
}

/// Rate limits pause polling; a rejected token stops it; anything else is
/// logged and retried on the next cycle.
fn handle(err: ApiError) -> Result<Option<Duration>> {
    match err {
        ApiError::RateLimited { retry_after } => {
            warn!("rate limited; pausing for {}s", retry_after.as_secs());
            Ok(Some(retry_after))
        }
        ApiError::Unauthorized => Err(err)
            .wrap_err("GitHub rejected the token")
            .suggestion("run `gh auth login`, or update GITHUB_TOKEN"),
        ApiError::Other(report) => {
            warn!("poll failed: {report:?}");
            Ok(None)
        }
    }
}

fn log_triggers(refreshed: &Refreshed) {
    let snap = &refreshed.snapshot;
    for trigger in &refreshed.triggers {
        info!(
            url = %snap.key.url(),
            profile = %refreshed.profile,
            "{}",
            describe(trigger)
        );
    }
}

fn describe(trigger: &Trigger) -> String {
    let short = |sha: &str| sha.chars().take(8).collect::<String>();
    match trigger {
        Trigger::ReviewRequested { head_sha } => {
            format!("review requested at {}", short(head_sha))
        }
        Trigger::ReadyForReview { head_sha } => {
            format!("ready for review at {}", short(head_sha))
        }
        Trigger::Push { from_sha, to_sha } => {
            format!("new commits {}..{}", short(from_sha), short(to_sha))
        }
        Trigger::Reply { comment_ids, .. } => {
            format!(
                "{} new repl{} to you",
                comment_ids.len(),
                plural(comment_ids.len(), "y", "ies")
            )
        }
        Trigger::Feedback {
            comment_ids,
            review_ids,
        } => format!(
            "feedback on your PR: {} comment{}, {} review{}",
            comment_ids.len(),
            plural(comment_ids.len(), "", "s"),
            review_ids.len(),
            plural(review_ids.len(), "", "s"),
        ),
        Trigger::Approved { reviewers, .. } => format!("approved by {}", reviewers.join(", ")),
    }
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use sanic_core::{pr::PrSnapshot, repo::RepoName};

    use super::*;

    fn store_with_pr(key: &PrKey) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let snapshot = PrSnapshot {
            key: key.clone(),
            title: "t".into(),
            body: String::new(),
            url: key.url(),
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
        store
    }

    fn key() -> PrKey {
        PrKey {
            repo: RepoName::new("org", "repo"),
            number: 7,
        }
    }

    /// Runs `handle_requests` until it has handled everything sent, and
    /// returns the runs it started.
    async fn handle(store: &Arc<Mutex<Store>>, manual: bool, sent: Vec<Request>) -> Vec<QueuedRun> {
        let (requests, requests_rx) = mpsc::unbounded_channel();
        for request in sent {
            requests.send(request).unwrap();
        }
        let (start, mut started) = mpsc::unbounded_channel();
        let handled = tokio::time::timeout(
            Duration::from_secs(1),
            handle_requests(requests_rx, Arc::clone(store), Starter { manual, start }),
        );
        assert!(handled.await.is_err(), "handle_requests never returns");
        std::iter::from_fn(|| started.try_recv().ok()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn requests_rerun_once_and_archive() {
        let key = key();
        let mut store = store_with_pr(&key);
        let failed = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        store.claim_run(failed.id).unwrap();
        store.fail_run(failed.id, "boom").unwrap();
        let store = Arc::new(Mutex::new(store));

        // The second request finds the first one's run queued.
        let archive = Request::Archive {
            key: key.clone(),
            archived: true,
        };
        let sent = vec![
            Request::Rerun(key.clone()),
            Request::Rerun(key.clone()),
            archive,
        ];
        assert_eq!(
            handle(&store, false, sent).await,
            std::slice::from_ref(&failed)
        );
        // Archiving then superseded the requeued run.
        let store = store.lock().unwrap();
        assert_eq!(store.run(failed.id).unwrap().unwrap().status, "superseded");
        assert!(store.pr_summary(&key).unwrap().unwrap().archived);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_save_the_tui_layout() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        handle(&store, false, vec![Request::SaveLayout(Sizes::default())]).await;
        let store = store.lock().unwrap();
        assert!(store.poll_state("tui.layout").unwrap().is_some());
        assert_eq!(tui::load_layout(&store), Sizes::default());
    }

    #[tokio::test(start_paused = true)]
    async fn manual_reviews_start_the_held_run_including_from_the_cli() {
        let key = key();
        let mut store = store_with_pr(&key);
        let held = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        // What `sanic-review review <url>` writes.
        store.request_start(&key).unwrap();
        let store = Arc::new(Mutex::new(store));

        // The CLI's request starts the held run; `r` then finds nothing
        // left to start, since that head's review is under way.
        let started = handle(&store, true, vec![]).await;
        assert_eq!(started, std::slice::from_ref(&held));
        store.lock().unwrap().claim_run(held.id).unwrap();
        assert!(
            handle(&store, true, vec![Request::Rerun(key)])
                .await
                .is_empty()
        );
    }

    #[test]
    fn big_batches_log_progress_now_and_then() {
        let start = Instant::now();
        let mut log = ProgressLog::new(start);
        let logged: Vec<usize> = (0..60)
            .filter_map(|i| {
                let step = log.step(59 - i, start);
                assert_eq!(step.progress.total, 60);
                step.log.then_some(step.progress.done)
            })
            .collect();
        assert_eq!(logged, [24, 49]);

        // A slow batch logs on time as well.
        let mut log = ProgressLog::new(start);
        let _ = log.step(29, start);
        assert!(log.step(28, start + ProgressLog::INTERVAL).log);

        // A small one never does.
        let mut log = ProgressLog::new(start);
        let later = start + ProgressLog::INTERVAL * 2;
        assert!((0..5).all(|i| !log.step(4 - i, later).log));
    }

    /// A refresh the dashboard asks for goes ahead of the next poll, but
    /// not through a rate limit's pause.
    #[tokio::test(start_paused = true)]
    async fn asked_refreshes_go_now_unless_a_rate_limit_pauses_polling() {
        use crate::poll::tests::{FakeGithub, key, poller, snapshot};

        let pr = key("org/repo", 7);
        // Without a pause it's fetched when asked; the first reconcile's
        // rate limit holds it until the pause ends.
        for (limit, fetched_after) in [(None, 5), (Some(60), 60)] {
            let github = FakeGithub::default();
            github.rate_limit.set(limit.map(Duration::from_secs));
            github
                .prs
                .borrow_mut()
                .insert(pr.clone(), snapshot(&pr, "alice", &[]));
            let mut poller = poller(github);
            let dir = tempfile::TempDir::new().unwrap();
            let config_path = dir.path().join("config.toml");
            std::fs::write(&config_path, "").unwrap();
            let mut watcher = ConfigWatcher::new(&config_path).unwrap();
            let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
            let worker = Worker::new(dir.path(), store, poller.config());
            let live = Live::new(poller.config(), None);
            let (updates, _updates) = mpsc::unbounded_channel();
            let (refreshes, mut asked) = mpsc::unbounded_channel();
            let start = Instant::now();
            let polling = tokio::time::timeout(
                Duration::from_secs(90),
                poll_forever(
                    &mut poller,
                    &mut watcher,
                    &config_path,
                    &updates,
                    &mut asked,
                    &worker,
                    &live,
                ),
            );
            let ask = async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                refreshes.send(pr.clone()).unwrap();
            };
            let (stopped, ()) = tokio::join!(polling, ask);
            assert!(stopped.is_err(), "poll_forever never returns");
            // Nothing else fetches it: it's in no search or notification.
            let github = poller.github();
            assert_eq!(*github.fetched.borrow(), std::slice::from_ref(&pr));
            let at = github.fetched_at.borrow()[0] - start;
            assert_eq!(at.as_secs(), fetched_after, "{limit:?}");
        }
    }

    /// A refresh asked for while a batch is queued goes next, not after it.
    #[tokio::test]
    async fn asked_refreshes_jump_a_queued_batch() {
        use crate::poll::tests::{FakeGithub, key, poller, snapshot};

        let (batch, asked_for) = ([key("org/a", 1), key("org/b", 2)], key("org/c", 3));
        let github = FakeGithub::default();
        for pr in batch.iter().chain([&asked_for]) {
            github
                .prs
                .borrow_mut()
                .insert(pr.clone(), snapshot(pr, "alice", &[]));
        }
        let mut poller = poller(github);
        let mut pending = RefreshQueue::default();
        pending.extend(batch.clone(), Priority::Involved);
        let (refreshes, mut asked) = mpsc::unbounded_channel();
        refreshes.send(asked_for.clone()).unwrap();
        let (updates, _updates) = mpsc::unbounded_channel();
        let live = Live::new(poller.config(), None);
        refresh_queued(
            &mut poller,
            &mut pending,
            &mut asked,
            &mut HashSet::new(),
            &updates,
            &live,
        )
        .await
        .unwrap();
        let fetched = poller.github().fetched.borrow();
        assert_eq!(fetched.first(), Some(&asked_for), "{fetched:?}");
        assert_eq!(fetched.len(), 3);
    }

    #[test]
    fn the_dashboard_adds_skip_titles_and_picks_windows() {
        use sanic_web::Control as _;

        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        let text = "# Mine.\n[profile.default] # the org\nrepos = [{ github = \"org\" }]\n";
        std::fs::write(&config_path, text).unwrap();
        let config = Config::parse(text, dir.path(), &VcsResolver).unwrap();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let control = DashboardControl {
            requests: mpsc::unbounded_channel().0,
            refreshes: mpsc::unbounded_channel().0,
            store: Arc::clone(&store),
            started: mpsc::unbounded_channel().0,
            data_dir: dir.path().to_owned(),
            config_path: config_path.clone(),
            worker: Arc::new(Worker::new(dir.path(), Arc::clone(&store), &config)),
            live: Arc::new(Live::new(&config, None)),
        };
        assert!(control.add_skip_title("build(deps)*", None).unwrap());
        assert!(!control.add_skip_title("build(deps)*", None).unwrap());
        assert!(control.add_skip_title("wip*", Some("default")).unwrap());
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "# Mine.\n[profile.default] # the org\nrepos = [{ github = \"org\" }]\n\
             skip_titles = [\"wip*\"]\n\n[review_requests]\nskip_titles = [\"build(deps)*\"]\n"
        );
        assert!(control.add_skip_title("x*", Some("nope")).is_err());

        // A recency window picked there is stored, and followed by the
        // lists and the poller, until it's reset; the config is untouched.
        let before = std::fs::read_to_string(&config_path).unwrap();
        let mut window = control.live.recency.subscribe();
        control.set_window(Some(WindowChoice::Days(180))).unwrap();
        assert!(window.has_changed().unwrap());
        assert_eq!(window.borrow_and_update().days(), Some(180));
        assert_eq!(*control.live.window.borrow(), Some(180));
        assert_eq!(
            store.lock().unwrap().window_choice().unwrap(),
            Some(WindowChoice::Days(180))
        );
        control.set_window(None).unwrap();
        assert_eq!(*control.live.window.borrow(), Some(14));
        assert_eq!(store.lock().unwrap().window_choice().unwrap(), None);
        // Only a change prompts a reconcile: not the same pick again, nor
        // a config reload that leaves the window as it was.
        window.mark_unchanged();
        control.set_window(None).unwrap();
        control.live.publish(&config);
        assert!(!window.has_changed().unwrap());
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);
    }

    #[test]
    fn regenerating_starts_the_new_run_past_any_hold() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = key();
        let mut store = store_with_pr(&key);
        let source = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        store.claim_run(source.id).unwrap();
        let result = sanic_core::run::ReviewResult {
            summary: "s".into(),
            summary_note: None,
            verdict: sanic_core::run::Verdict::None,
            comments: vec![],
            session_id: Some("sess".into()),
            transcript_path: "t".into(),
        };
        store.finish_review(source.id, &result).unwrap();
        let store = Mutex::new(store);
        let (started, mut started_rx) = mpsc::unbounded_channel();

        let id = regenerate(&store, dir.path(), &started, (source.id, None), "be terser")
            .unwrap()
            .unwrap();
        let run = started_rx.try_recv().unwrap();
        assert_eq!(run.id, id);
        assert_eq!(run.revision.unwrap().instruction, "be terser");
        // Refusals come back as such, with nothing started.
        assert_eq!(
            regenerate(&store, dir.path(), &started, (source.id, None), "again").unwrap(),
            Err(Refusal::Underway { run: id })
        );
        std::fs::create_dir_all(sanic_runner::chat::worktree_path(dir.path(), source.id)).unwrap();
        assert_eq!(
            regenerate(&store, dir.path(), &started, (source.id, None), "again").unwrap(),
            Err(Refusal::WorktreeInUse)
        );
        assert!(started_rx.try_recv().is_err());
    }

    #[test]
    fn revising_one_draft_starts_a_regeneration_of_just_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = key();
        let mut store = store_with_pr(&key);
        let source = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        store.claim_run(source.id).unwrap();
        let result = sanic_core::run::ReviewResult {
            summary: "s".into(),
            summary_note: None,
            verdict: sanic_core::run::Verdict::None,
            comments: vec![],
            session_id: Some("sess".into()),
            transcript_path: "t".into(),
        };
        store.finish_review(source.id, &result).unwrap();
        let summary = store.drafts(source.id).unwrap()[0].id;
        let store = Mutex::new(store);
        let (started, mut started_rx) = mpsc::unbounded_channel();

        let id = regenerate(
            &store,
            dir.path(),
            &started,
            (source.id, Some(summary)),
            "reword",
        )
        .unwrap()
        .unwrap();
        let run = started_rx.try_recv().unwrap();
        assert_eq!(run.id, id);
        let revision = run.revision.unwrap();
        assert_eq!(revision.draft, Some(summary));
        assert_eq!(revision.instruction, "reword");
        assert_eq!(
            regenerate(&store, dir.path(), &started, (source.id, Some(9999)), "x").unwrap(),
            Err(Refusal::NoSuchDraft)
        );
    }

    #[test]
    fn describes_triggers_for_humans() {
        assert_eq!(
            describe(&Trigger::Push {
                from_sha: "0123456789".into(),
                to_sha: "abcdef0123".into()
            }),
            "new commits 01234567..abcdef01"
        );
        assert_eq!(
            describe(&Trigger::Reply {
                thread_id: "t".into(),
                comment_ids: vec!["a".into(), "b".into()]
            }),
            "2 new replies to you"
        );
        assert_eq!(
            describe(&Trigger::Feedback {
                comment_ids: vec!["a".into()],
                review_ids: vec![]
            }),
            "feedback on your PR: 1 comment, 0 reviews"
        );
        assert_eq!(
            describe(&Trigger::Approved {
                review_ids: vec!["r".into(), "s".into()],
                reviewers: vec!["bob".into(), "carol".into()]
            }),
            "approved by bob, carol"
        );
    }
}
