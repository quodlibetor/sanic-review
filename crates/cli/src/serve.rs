//! The foreground `serve` loop.
//!
//! One task does all the polling in turn: reconcile and notification polls
//! queue PRs, then each queued PR is refreshed. Keeping it sequential means
//! a PR is never refreshed twice at once. A config file change is picked up
//! between cycles. Refreshes with triggers go to the scheduler, which
//! debounces them into queued runs for the worker.

use std::{
    collections::HashSet,
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};
use sanic_core::{
    clock::{RecencyWindow, SystemClock, WindowChoice},
    config::{
        Config, DEFAULT_API_URL, Effort, Keys, Unloadable, default_config_path, default_data_dir,
    },
    hold::HeadCheck,
    manual::{ManualReviews, TeamMembers},
    pr::PrKey,
    run::{QueuedRun, ReviewRequest, ReviewTrigger},
    skip::SkipRules,
    trigger::Trigger,
};
use sanic_github::{ApiError, Client, Token};
use sanic_runner::{mirror::Mirrors, review::RunSettings, vcs::VcsResolver};
use sanic_store::{Cancelled, Direction, Moved, Refusal, Regeneration, Store};
use sanic_web::Dashboard;
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep_until},
};
use tracing::{Instrument, debug, info, info_span, warn};

use crate::{
    ServeArgs, Stdout, Ui, config_edit, logging,
    poll::{GithubApi, Poller, Priority, Progress, RefreshQueue, Refreshed},
    schedule::{DueTimes, Update, schedule, standing_request},
    tui::standalone::{self, Edited, Until},
    tui::{self, Request, Shared, Sizes, SystemBrowser, Tui},
    watch::ConfigWatcher,
    work::{self, Job, Worker},
};

/// Why `serve` can't start without a config file, and how to get one.
fn missing_config(path: &Path) -> color_eyre::Report {
    let what = match std::fs::read_link(path) {
        Ok(target) => format!(
            "no config at {}: it links to {}, which doesn't exist",
            path.display(),
            target.display()
        ),
        Err(_) => format!("no config at {}", path.display()),
    };
    eyre!(what)
        .suggestion(
            "run `sanic-review serve` in the foreground of a terminal to write one in the config editor",
        )
        .suggestion("or `sanic-review setup`, which opens the same editor without starting serve")
        .suggestion("or pass `--config PATH` to use another file")
}

/// Whether a first run can open the config editor: it takes keys from
/// stdin and draws on stdout, so both are a terminal `serve` has the
/// foreground of.
fn can_edit(stdin_is_tty: bool, stdout: Stdout) -> bool {
    stdin_is_tty && stdout == Stdout::Foreground
}

/// A first run with no config: the config editor writes one, before
/// anything logs so nothing draws over it, and `serve` carries on with
/// what it wrote. Without a terminal, or when nothing's written, it fails
/// saying how to get one.
async fn first_run(path: &Path) -> Result<String> {
    if !can_edit(std::io::stdin().is_terminal(), Stdout::detect()) {
        return Err(missing_config(path));
    }
    // There's no config for an `api_url` yet. Without a token it still
    // edits; `serve` then fails on the token as it would anyway.
    let github = match Token::discover() {
        Ok(token) => Some(Arc::new(Client::new(DEFAULT_API_URL, token)?)),
        Err(_) => None,
    };
    let edited = {
        let path = path.to_owned();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            standalone::edit(
                &path,
                Arc::new(VcsResolver),
                github,
                runtime,
                Until::Written,
            )
        })
        .await
        .wrap_err("running the config editor")??
    };
    match (edited, Config::read_text(path)?) {
        (Edited::Written, Some(text)) => Ok(text),
        _ => Err(eyre!("no config written, so `serve` can't start"))
            .suggestion(
                "run `sanic-review serve` again, and save with Ctrl-S; it starts once saved",
            )
            .suggestion("or pass `--config PATH` to use another file"),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "it starts each of serve's tasks in turn; splitting it only scatters the wiring"
)]
pub async fn run(args: ServeArgs) -> Result<()> {
    let ui_kind = Ui::resolve(args.ui, Stdout::detect())?;
    // Absolute, because runs hand these paths to `git -C <mirror>` and to
    // `claude` running in a worktree, where relative ones would resolve
    // somewhere else.
    let data_dir = std::path::absolute(match args.data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    })
    .wrap_err("resolving the data directory")?;
    let config_path = std::path::absolute(match args.config {
        Some(path) => path,
        None => default_config_path()?,
    })
    .wrap_err("resolving the config path")?;
    let config_text = match Config::read_text(&config_path)? {
        Some(text) => text,
        None => first_run(&config_path).await?,
    };
    // Before anything logs, so the TUI's log pane and file get it all.
    let logs = match ui_kind {
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
    let mut config = Config::parse_file(&config_text, &config_path, &VcsResolver)?;
    // The flag only switches the setting on; from here the config decides.
    // Only a config that turns it off is written, so one serve can't write,
    // say in the Nix store, still starts with the flag.
    if args.manual_reviews && !config.runner.manual_reviews {
        config_edit::set_manual_reviews_in_file(&config_path, true, &VcsResolver)?
            .map_err(|Unloadable(err)| err)?;
        config.runner.manual_reviews = true;
        info!(config = %config_path.display(), "--manual-reviews: turned `runner.manual_reviews` on");
    }
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
    // Queued reviews whose heads are left to check for `.claude` changes.
    let (checks, checks_rx) = mpsc::unbounded_channel();
    let (runs, runs_rx) = mpsc::unbounded_channel();
    // Reviews started by hand, which skip any manual reviews hold.
    let (started, started_rx) = mpsc::unbounded_channel();
    resume_queued(
        &run_store,
        &config.manual_reviews(),
        &config.skip_rules(),
        &me,
        &runs,
    )?;
    let worker = Arc::new(Worker::new(&data_dir, Arc::clone(&run_store), &config, &me));

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
        .dashboard(&me, &data_dir, &github, control, &due)?
        .bind(args.port)
        .await?;
    let mut ui = match logs {
        Some(logs) => Some(Tui::start(
            &db_path,
            Shared {
                me: me.clone(),
                manual_reviews: live.manual_reviews.subscribe(),
                keys: live.keys.subscribe(),
                guessed_keys: crate::tui::guessed_keys(),
                logs,
                due,
                skips: live.skips.subscribe(),
                window: live.window.subscribe(),
                progress: live.progress.subscribe(),
                paused: live.paused.subscribe(),
                github: Arc::new(github.clone()),
                clock: Arc::new(SystemClock),
                requests,
                config_path: config_path.clone(),
                resolver: Arc::new(VcsResolver),
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
    let reviews = run_reviews(
        Arc::clone(&worker),
        Inputs {
            runs: runs_rx,
            started: started_rx,
            manual: live.manual_reviews.subscribe(),
            store: Arc::clone(&run_store),
            checks: checks.clone(),
        },
    );
    tokio::pin!(reviews);
    let result = tokio::select! {
        result = quit => {
            info!("shutting down");
            result
        }
        result = poll_forever(
            &mut poller, &mut watcher, &config_path, &updates, &mut refreshes_rx, &worker, &live,
        ) => result,
        () = check_heads(checks_rx, Arc::clone(&worker), Arc::clone(&run_store), runs.clone()) => {
            Ok(())
        }
        result = schedule(
            updates_rx, Arc::clone(&run_store), runs.clone(), due_tx, live.skips.subscribe(),
            live.manual_reviews.subscribe(), me_for_scheduler,
        ) => result,
        () = handle_requests(requests_rx, run_store, Arc::clone(&worker), Starter {
            manual: live.manual_reviews.subscribe(),
            start: started,
            checks,
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
    cancel_running(&worker, ui_kind == Ui::Tui).await;
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
    /// `runner.manual_reviews` and the profiles that override it: queued
    /// reviews they hold wait until you start one. Sent only when it
    /// changes.
    manual_reviews: watch::Sender<ManualReviews>,
    /// `tui.keys`, for the TUI's text fields.
    keys: watch::Sender<Option<Keys>>,
    /// The recency window in force, in days, for the TUI.
    window: watch::Sender<Option<u32>>,
    /// The same, with where it comes from, for the dashboard.
    recency: watch::Sender<RecencyWindow>,
    /// The refresh batch under way; `None` when the queue is empty.
    progress: watch::Sender<Option<Progress>>,
    /// Until when a rate limit pauses polling, so the config editor's
    /// counts, sharing the token, wait too.
    paused: watch::Sender<Option<Instant>>,
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
            manual_reviews: watch::Sender::new(config.manual_reviews()),
            keys: watch::Sender::new(config.tui.keys),
            window: watch::Sender::new(recency.days()),
            recency: watch::Sender::new(recency),
            progress: watch::Sender::new(None),
            paused: watch::Sender::new(None),
        }
    }

    fn publish(&self, config: &Config) {
        self.skips.send_replace(config.skip_rules());
        let members = Arc::clone(&self.manual_reviews.borrow().members);
        let manual = config.manual_reviews().with_members(members);
        self.manual_reviews
            .send_if_modified(|was| std::mem::replace(was, manual.clone()) != manual);
        self.keys.send_replace(config.tui.keys);
        self.set_recency(|w| w.configured = config.poll.updated_within_days);
    }

    /// Takes the teams' members as last fetched.
    fn set_members(&self, members: &Arc<TeamMembers>) {
        self.manual_reviews.send_if_modified(|manual| {
            let changed = manual.members != *members;
            manual.members = Arc::clone(members);
            changed
        });
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
        data_dir: &Path,
        github: &Client,
        control: DashboardControl,
        due: &watch::Receiver<DueTimes>,
    ) -> Result<Dashboard> {
        Dashboard::new(sanic_web::Context {
            me: me.to_owned(),
            manual_reviews: self.manual_reviews.subscribe(),
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
/// `skip_titles` patterns the way its ignore editor does, switches manual
/// reviews the way its `m` does, and reads the run settings its chat
/// commands are built from.
struct DashboardControl {
    requests: mpsc::UnboundedSender<Request>,
    /// PRs to refresh from GitHub now, for the poller.
    refreshes: mpsc::UnboundedSender<PrKey>,
    /// For regenerations, which start at once and return their run.
    store: Arc<Mutex<Store>>,
    /// Straight to the worker, past any manual reviews hold.
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

    fn set_manual_reviews(&self, on: bool) -> Result<Result<bool, Unloadable>> {
        config_edit::set_manual_reviews_in_file(&self.config_path, on, &VcsResolver)
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

    fn cancel_run(&self, run: i64) -> Result<Cancelled> {
        cancel_run(&self.store, &self.worker, run)
    }
}

/// Cancels run `run`, stopping its agent if it's running.
fn cancel_run(store: &Mutex<Store>, worker: &Worker, run: i64) -> Result<Cancelled> {
    // Recorded first, so a run that finishes in between reads as finished
    // rather than being lost.
    let outcome = store
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .cancel_run(run)?;
    match &outcome {
        Cancelled::Queued { key } => info!(url = %key.url(), run, "a queued run was cancelled"),
        Cancelled::Running { key } => {
            info!(url = %key.url(), run, "cancelling a running review");
            if !worker.cancel(run) {
                warn!(url = %key.url(), run, "it was already finishing");
            }
        }
        Cancelled::Finished { status } => debug!(run, "not cancelled: it's {status}"),
        Cancelled::NoSuchRun => debug!(run, "not cancelled: no such run"),
    }
    Ok(outcome)
}

/// Queues a regeneration of run `source`, or of just `draft` of it, and
/// starts it, past any manual reviews hold, or says why not.
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

/// Puts the runs a previous process left running back in the queue,
/// superseding those of PRs `skips` leaves unlisted or that are stale for
/// `me`, and sends them to `runs`, which holds those `manual` holds. The
/// worker takes them from the store in the queue's order; the send is what
/// tells it they're there, and what logs each held one.
fn resume_queued(
    store: &Mutex<Store>,
    manual: &ManualReviews,
    skips: &SkipRules,
    me: &str,
    runs: &mpsc::UnboundedSender<QueuedRun>,
) -> Result<()> {
    let recovered = {
        let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
        store.recover_runs()?;
        work::supersede_unlisted(&mut store, skips, None);
        work::supersede_stale(&mut store, me, None);
        store.queued_reviews()?
    };
    let held = {
        let store = store.lock().unwrap_or_else(PoisonError::into_inner);
        recovered
            .iter()
            .filter(|run| held_why(manual, &store, run).is_some())
            .count()
    };
    if held > 0 {
        info!(
            runs = held,
            "queued runs held: manual reviews, or a `.claude` hold"
        );
    }
    if recovered.len() > held {
        info!(runs = recovered.len() - held, "resuming queued runs");
    }
    for run in recovered {
        let _ = runs.send(run);
    }
    Ok(())
}

/// How often `serve` looks for `sanic-review review` requests.
const START_POLL: Duration = Duration::from_secs(2);

/// Where reviews started by hand go: straight to the worker, past any
/// manual reviews hold.
struct Starter {
    /// Manual reviews as the config last loaded: while they hold a PR's
    /// profile, its held review is the one to start.
    manual: watch::Receiver<ManualReviews>,
    start: mpsc::UnboundedSender<QueuedRun>,
    /// Where a review whose head isn't checked for `.claude` yet goes to
    /// be.
    checks: mpsc::UnboundedSender<QueuedRun>,
}

/// Carries out what the TUI and `sanic-review review` ask for. Never
/// returns.
async fn handle_requests(
    mut requests: mpsc::UnboundedReceiver<Request>,
    store: Arc<Mutex<Store>>,
    worker: Arc<Worker>,
    starter: Starter,
) {
    // Open while the TUI or the dashboard can still send; `review`
    // requests come either way.
    let mut open = true;
    let mut poll = tokio::time::interval(START_POLL);
    loop {
        tokio::select! {
            request = requests.recv(), if open => match request {
                Some(Request::Rerun(key)) => review_now(&key, None, &store, &starter),
                Some(Request::Archive { key, archived }) => archive(&key, archived, &store),
                Some(Request::MoveRun { run, up }) => move_run(run, up, &store),
                Some(Request::Cancel(run)) => {
                    if let Err(err) = cancel_run(&store, &worker, run) {
                        warn!(run, "cancelling failed: {err:?}");
                    }
                }
                Some(Request::ClearStale) => {
                    let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
                    work::supersede_stale(&mut store, worker.me(), None);
                }
                Some(Request::SaveLayout(sizes)) => save_layout(sizes, &store),
                None => open = false,
            },
            _ = poll.tick() => {
                let taken = store
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take_start_requests();
                match taken {
                    Ok(requests) => {
                        for (key, effort) in requests {
                            review_now(&key, effort, &store, &starter);
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

/// Moves a queued run one place, as the dashboard's buttons do.
fn move_run(run: i64, up: bool, store: &Mutex<Store>) {
    let dir = if up { Direction::Up } else { Direction::Down };
    let moved = store
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .move_run(run, dir);
    match moved {
        Ok(Moved::Moved { key, from, to }) => {
            info!(url = %key.url(), run, from, to, "a queued run was moved");
        }
        Ok(Moved::AlreadyThere { .. }) => {}
        Ok(Moved::NotQueued { status }) => debug!(run, "not moved: it's {status}"),
        Ok(Moved::NoSuchRun) => debug!(run, "not moved: no such run"),
        Err(err) => warn!(run, "moving the run failed: {err:?}"),
    }
}

fn save_layout(sizes: Sizes, store: &Mutex<Store>) {
    let store = store.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err(err) = tui::save_layout(&store, sizes) {
        warn!("{err:?}");
    }
}

/// Starts a review of `key` now: its queued review if manual reviews hold
/// the profile that was queued under, or its PR has a `.claude` hold;
/// otherwise a review of its current head is queued, as the scheduler
/// would, so a head that already has a queued, running or succeeded
/// review is left alone. See [`by_hand`] for which kind. Given an
/// `effort`, the review runs at that rather than its profile's, as does a
/// queued one that isn't held.
fn review_now(key: &PrKey, effort: Option<Effort>, store: &Mutex<Store>, starter: &Starter) {
    let _span = info_span!("review_now", url = %key.url()).entered();
    let mut queued_at = None;
    let run = {
        let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
        let manual = starter.manual.borrow().clone();
        let queued = store
            .queued_review(key)
            .and_then(|run| match (run, effort) {
                (Some(mut run), Some(effort)) => {
                    if store.set_effort(run.id, effort)? {
                        run.effort = Some(effort);
                        queued_at = Some(effort);
                    }
                    Ok(Some(run))
                }
                (run, _) => Ok(run),
            });
        let held = queued.map(|run| run.filter(|run| held_why(&manual, &store, run).is_some()));
        // Marked, so a stale sweep doesn't drop it while it waits for a slot.
        let held = held.and_then(|held| match held {
            Some(run) => Ok(store.mark_by_hand(run.id)?.then_some(run)),
            None => Ok(None),
        });
        held.and_then(|held| match held {
            Some(run) => Ok(Some(run)),
            None => store.review_request(key).and_then(|req| match req {
                Some(req) => {
                    let req = by_hand(&store, req)?;
                    store.queue_review_by_hand(&req, effort)
                }
                None => Ok(None),
            }),
        })
    };
    let pending = |run: &QueuedRun| {
        let store = store.lock().unwrap_or_else(PoisonError::into_inner);
        store
            .head_check(key, &run.request.base_sha, &run.request.head_sha)
            .is_ok_and(|check| check == HeadCheck::Pending)
    };
    match run {
        // You can't have seen what it holds for yet. A failed check can
        // be started past, under its warning.
        Ok(Some(run)) if pending(&run) => {
            info!(
                head = %run.request.head_sha,
                "not started yet: its head is still being checked for `.claude` changes; \
                 start it again once that's done"
            );
            let _ = starter.checks.send(run);
        }
        Ok(Some(run)) => {
            info!(
                head = %run.request.head_sha,
                effort = run.effort.map_or("the profile's", Effort::name),
                "review started by hand"
            );
            let _ = starter.start.send(run);
        }
        Ok(None) => match (effort, queued_at) {
            (_, Some(effort)) => info!(
                effort = effort.name(),
                "not started now: its queued review waits for a slot, and will run at this effort"
            ),
            (Some(effort), None) => warn!(
                url = %key.url(),
                effort = effort.name(),
                "not started, so the effort wasn't applied: the PR isn't tracked, or its head \
                 already has a running or finished review"
            ),
            (None, None) => info!(
                "not started: the PR isn't tracked, or its head already has a queued, \
                 running or finished review"
            ),
        },
        Err(err) => warn!(url = %key.url(), "starting the review failed: {err:?}"),
    }
}

/// `req`, a full review of the PR's head, as a review started by hand
/// queues it: when the PR's latest run succeeded on an older head, as a
/// push from that head instead, so it resumes that review's session if it
/// can, as a push's review does.
fn by_hand(store: &Store, req: ReviewRequest) -> Result<ReviewRequest> {
    let latest = store.review_runs(&req.key)?.into_iter().next();
    Ok(match latest {
        Some(run) if run.status == "succeeded" && run.head_sha != req.head_sha => ReviewRequest {
            trigger: ReviewTrigger::Push {
                from_sha: run.head_sha,
            },
            ..req
        },
        _ => req,
    })
}

/// What [`run_reviews`] takes runs from.
struct Inputs {
    /// Queued by the scheduler, or left queued before `serve` started.
    runs: mpsc::UnboundedReceiver<QueuedRun>,
    /// Started by hand, past any hold.
    started: mpsc::UnboundedReceiver<QueuedRun>,
    /// Manual reviews as the config last loaded.
    manual: watch::Receiver<ManualReviews>,
    /// Where the held reviews are, when manual reviews are turned off.
    store: Arc<Mutex<Store>>,
    /// Where an automatic review whose head isn't checked for `.claude`
    /// yet goes to be.
    checks: mpsc::UnboundedSender<QueuedRun>,
}

/// Runs queued reviews. Those from `runs` that manual reviews hold are
/// only logged and stay queued in the store, and of those just the ones
/// started by hand run.
async fn run_reviews(worker: Arc<Worker>, inputs: Inputs) {
    let (jobs, jobs_rx) = mpsc::unbounded_channel();
    tokio::join!(hold(inputs, jobs), worker.work(jobs_rx));
}

const START_ONE: &str = "start one with `r` in the TUI, Review now on the dashboard or \
                         `sanic-review review <PR url>`";

/// Says what manual reviews hold, if anything.
fn log_holding(manual: &ManualReviews) {
    if manual.runner {
        info!("manual reviews are on: reviews are queued and held; {START_ONE}");
        return;
    }
    let profiles: Vec<&str> = manual.holding_profiles().collect();
    if !profiles.is_empty() {
        info!(
            "manual reviews are on for profiles {}: their reviews are queued and held; \
             {START_ONE}",
            profiles.join(", ")
        );
    }
}

/// Hands runs to the worker as `jobs`, holding automatic ones of profiles
/// manual reviews hold. A switch that stops holding a profile starts its
/// held reviews, which the worker runs `runner.max_concurrent` at a time;
/// one that starts holding it holds what's queued from then on, and the
/// worker holds those that were waiting their turn. Returns once its
/// inputs close.
async fn hold(inputs: Inputs, jobs: mpsc::UnboundedSender<Job>) {
    let Inputs {
        mut runs,
        mut started,
        mut manual,
        store,
        checks,
    } = inputs;
    let mut holding = manual.borrow_and_update().clone();
    log_holding(&holding);
    // Only fails once the worker has stopped, which ends `serve` anyway.
    let send = |run, automatic| {
        let _ = jobs.send(Job { run, automatic });
    };
    loop {
        // A switch first, so runs after it are handled as it says.
        tokio::select! {
            biased;
            Ok(()) = manual.changed() => {
                let now = manual.borrow_and_update().clone();
                if holding.releases_any(&now) {
                    let released = |run: &QueuedRun| {
                        holding.releases(&now, &run.request.profile, &run.request.author)
                    };
                    for run in held_reviews(&store, released) {
                        send(run, true);
                    }
                }
                if now.badge() != holding.badge() {
                    log_holding(&now);
                }
                holding = now;
            }
            Some(run) = runs.recv() => {
                let (why, unchecked) = {
                    let store = store.lock().unwrap_or_else(PoisonError::into_inner);
                    let req = &run.request;
                    let check = store.head_check(&req.key, &req.base_sha, &req.head_sha);
                    (
                        held_why(&holding, &store, &run),
                        !check.is_ok_and(|c| c == HeadCheck::Done),
                    )
                };
                // Checked only once a review of it is queued, so PRs that
                // only involve you cost nothing.
                if unchecked {
                    let _ = checks.send(run.clone());
                }
                if let Some(why) = why {
                    info!(
                        url = %run.request.key.url(),
                        run = run.id,
                        head = %run.request.head_sha,
                        profile = %run.request.profile,
                        "review held: {why}"
                    );
                } else {
                    if holding.holds_profile(&run.request.profile) {
                        info!(
                            url = %run.request.key.url(),
                            run = run.id,
                            author = %run.request.author,
                            "review not held: `auto_review_authors` lets its author through"
                        );
                    }
                    send(run, true);
                }
            }
            Some(run) = started.recv() => send(run, false),
            else => return,
        }
    }
}

/// Why an automatic `run` is held, if it is: `manual` holds its profile
/// and author, or a `.claude` hold does. Held if the store can't say.
pub(crate) fn held_why(
    manual: &ManualReviews,
    store: &Store,
    run: &QueuedRun,
) -> Option<&'static str> {
    if manual.holds(&run.request.profile, &run.request.author) {
        return Some("manual reviews are on");
    }
    match store.claude_holds_run(
        &run.request.key,
        &run.request.base_sha,
        &run.request.head_sha,
    ) {
        Ok(why) => why,
        Err(err) => {
            warn!(
                url = %run.request.key.url(),
                "checking for a `.claude` hold failed, so holding the review: {err:?}"
            );
            Some("checking for a `.claude` hold failed")
        }
    }
}

/// The reviews manual reviews held that `released` picks, now that they
/// no longer hold them, less those a `.claude` hold still keeps.
fn held_reviews(store: &Mutex<Store>, released: impl Fn(&QueuedRun) -> bool) -> Vec<QueuedRun> {
    let store = store.lock().unwrap_or_else(PoisonError::into_inner);
    match store.queued_reviews() {
        Ok(mut held) => {
            // Those a `.claude` hold keeps would only nudge the worker.
            held.retain(|run| {
                released(run)
                    && store
                        .claude_holds_run(
                            &run.request.key,
                            &run.request.base_sha,
                            &run.request.head_sha,
                        )
                        .is_ok_and(|why| why.is_none())
            });
            if !held.is_empty() {
                info!(
                    reviews = held.len(),
                    "manual reviews no longer hold them: starting the held reviews"
                );
            }
            held
        }
        Err(err) => {
            warn!("reading the held reviews failed; they start when serve next does: {err:?}");
            Vec::new()
        }
    }
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
                    let members = poller.members();
                    if live.manual_reviews.borrow().members != members {
                        // The worker first, so it never holds a run `hold()`
                        // has just released.
                        worker.set_members(Arc::clone(&members));
                        live.set_members(&members);
                    }
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
            live.paused.send_replace(Some(resume));
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
                supersede_stale(poller, &key);
            }
            Ok(None) => supersede_stale(poller, &key),
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

/// Supersedes `key`'s queued review if the refresh just recorded made it
/// stale: closed, approved, or no longer yours to review.
fn supersede_stale<G: GithubApi>(poller: &mut Poller<G>, key: &PrKey) {
    let me = poller.me().to_owned();
    work::supersede_stale(poller.store_mut(), &me, Some(key));
}

/// Checks the head of each queued review sent, on its base, for
/// `.claude` changes, one at a time, since each may fetch into a mirror,
/// then, if it got done, hands the PR's queued review, if any, to `runs`,
/// which holds it if something else still does. A failed check isn't
/// handed back, which would only ask for it again; it's tried again when
/// a review of that head is next queued or `serve` next starts. Never
/// returns while `checks` is open.
async fn check_heads(
    mut checks: mpsc::UnboundedReceiver<QueuedRun>,
    worker: Arc<Worker>,
    store: Arc<Mutex<Store>>,
    runs: mpsc::UnboundedSender<QueuedRun>,
) {
    while let Some(run) = checks.recv().await {
        let req = &run.request;
        let check = |store: &Mutex<Store>| {
            store
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .head_check(&req.key, &req.base_sha, &req.head_sha)
        };
        // An earlier request for the same head finished it.
        if check(&store).is_ok_and(|c| c == HeadCheck::Done) {
            continue;
        }
        worker
            .check_head(&req.key, &req.base_sha, &req.head_sha)
            .instrument(info_span!("check_head", url = %req.key.url()))
            .await;
        if !check(&store).is_ok_and(|c| c == HeadCheck::Done) {
            continue;
        }
        let queued = store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queued_review(&req.key);
        if let Ok(Some(run)) = queued {
            let _ = runs.send(run);
        }
    }
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
            let was = live.skips.borrow().clone();
            // The worker first: a review manual reviews held, and
            // turning them off releases, asks it whether they still hold.
            worker.configure(&config);
            live.publish(&config);
            // After publishing, so a review queued under the old rules is caught.
            worker.supersede_newly_unlisted(&was, &config.skip_rules());
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
    use sanic_core::{pr::PrSnapshot, repo::RepoName, run::ReviewTrigger};

    use sanic_core::hold::Finding;

    use super::*;

    /// Enough config for a [`Worker`] that never runs anything here.
    fn test_config() -> Config {
        Config::parse(
            "[profile.default]\nrepos = [{ github = \"org\" }]\n",
            Path::new("/"),
            &VcsResolver,
        )
        .unwrap()
    }

    /// A store with `key` tracked, its head checked for `.claude` with
    /// nothing found.
    fn store_with_pr(key: &PrKey) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        store.record(&pr_snapshot(key), "me", "p", &[]).unwrap();
        store.record_check(key, "b1", "h1", Ok(&[])).unwrap();
        store
    }

    fn pr_snapshot(key: &PrKey) -> PrSnapshot {
        PrSnapshot {
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
        }
    }

    fn key() -> PrKey {
        PrKey {
            repo: RepoName::new("org", "repo"),
            number: 7,
        }
    }

    /// Runs `handle_requests` until it has handled everything sent, and
    /// returns the runs it started.
    async fn handle(
        store: &Arc<Mutex<Store>>,
        manual: ManualReviews,
        sent: Vec<Request>,
    ) -> Vec<QueuedRun> {
        let (requests, requests_rx) = mpsc::unbounded_channel();
        for request in sent {
            requests.send(request).unwrap();
        }
        let (start, mut started) = mpsc::unbounded_channel();
        let data = tempfile::TempDir::new().unwrap();
        let worker = Arc::new(Worker::new(
            data.path(),
            Arc::clone(store),
            &test_config(),
            "me",
        ));
        let handled = tokio::time::timeout(
            Duration::from_secs(1),
            handle_requests(
                requests_rx,
                Arc::clone(store),
                worker,
                Starter {
                    manual: watch::channel(manual).1,
                    start,
                    checks: mpsc::unbounded_channel().0,
                },
            ),
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
            handle(&store, ManualReviews::default(), sent).await,
            std::slice::from_ref(&failed)
        );
        // Archiving then superseded the requeued run.
        let store = store.lock().unwrap();
        assert_eq!(store.run(failed.id).unwrap().unwrap().status, "superseded");
        assert!(store.pr_summary(&key).unwrap().unwrap().archived);
    }

    #[tokio::test(start_paused = true)]
    async fn a_review_the_pr_moved_past_is_started_again_as_a_push() {
        let key = key();
        let mut store = store_with_pr(&key);
        let reviewed = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        store.claim_run(reviewed.id).unwrap();
        store
            .finish_review(
                reviewed.id,
                &sanic_core::run::ReviewResult {
                    summary: "Fine.".into(),
                    summary_note: None,
                    verdict: sanic_core::run::Verdict::Comment,
                    comments: vec![],
                    session_id: Some("sess".into()),
                    transcript_path: "t".into(),
                    resumed_from: None,
                },
            )
            .unwrap();
        let pushed = PrSnapshot {
            head_sha: "h2".into(),
            ..pr_snapshot(&key)
        };
        store.record(&pushed, "me", "p", &[]).unwrap();
        // Checked, and it doesn't change `.claude`.
        store
            .record_check(&key, &pushed.base_sha, "h2", Ok(&[]))
            .unwrap();
        let store = Arc::new(Mutex::new(store));

        let started = handle(&store, ManualReviews::default(), vec![Request::Rerun(key)]).await;
        let [run] = &started[..] else {
            panic!("{started:?}")
        };
        assert_eq!(run.request.head_sha, "h2");
        assert_eq!(
            run.request.trigger,
            ReviewTrigger::Push {
                from_sha: "h1".into()
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn requests_save_the_tui_layout() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        handle(
            &store,
            ManualReviews::default(),
            vec![Request::SaveLayout(Sizes::default())],
        )
        .await;
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
        // What `sanic-review review --effort high <url>` writes.
        store.request_start(&key, Some(Effort::High)).unwrap();
        let store = Arc::new(Mutex::new(store));

        // The CLI's request starts the held run, at its effort; `r` then
        // finds nothing left to start, since that head's review is under way.
        let started = handle(&store, ManualReviews::runner(true), vec![]).await;
        let held = QueuedRun {
            effort: Some(Effort::High),
            ..held
        };
        assert_eq!(started, std::slice::from_ref(&held));
        assert_eq!(
            store.lock().unwrap().queued_review(&key).unwrap(),
            Some(held.clone())
        );
        store.lock().unwrap().claim_run(held.id).unwrap();
        assert!(
            handle(
                &store,
                ManualReviews::runner(true),
                vec![Request::Rerun(key)]
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_requested_effort_reaches_a_new_run_and_an_unheld_queued_one() {
        let key = key();
        let store = store_with_pr(&key);
        store.request_start(&key, Some(Effort::Max)).unwrap();
        let store = Arc::new(Mutex::new(store));
        let started = handle(&store, ManualReviews::runner(false), vec![]).await;
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].effort, Some(Effort::Max));

        // Queued and waiting for a slot, not held: it isn't started again,
        // but runs at the effort asked for.
        let other = PrKey { number: 8, ..key };
        let queued = {
            let mut store = store.lock().unwrap();
            store.record(&pr_snapshot(&other), "me", "p", &[]).unwrap();
            let req = store.review_request(&other).unwrap().unwrap();
            let queued = store.queue_review(&req).unwrap().unwrap();
            store.request_start(&other, Some(Effort::Low)).unwrap();
            queued
        };
        assert!(
            handle(&store, ManualReviews::runner(false), vec![])
                .await
                .is_empty()
        );
        let claimed = store
            .lock()
            .unwrap()
            .queued_review(&other)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, queued.id);
        assert_eq!(claimed.effort, Some(Effort::Low));
    }

    #[test]
    fn resuming_supersedes_reviews_of_skipped_authors() {
        let kept = key();
        let skipped = PrKey { number: 8, ..key() };
        let mut store = store_with_pr(&kept);
        let mut snap = pr_snapshot(&skipped);
        snap.author = "dependabot".into();
        store.record(&snap, "me", "p", &[]).unwrap();
        let mut queue = |key: &PrKey| {
            store
                .queue_review(&store.review_request(key).unwrap().unwrap())
                .unwrap()
                .unwrap()
        };
        let resumed = queue(&kept);
        let interrupted = queue(&skipped);
        store.claim_run(interrupted.id).unwrap();
        let config = Config::parse(
            "[review_requests]\nauthors = [\"*\", \"!dependabot\"]\n\
             [profile.p]\nrepos = [{ github = \"org\" }]\n",
            Path::new("/"),
            &VcsResolver,
        )
        .unwrap();
        let store = Mutex::new(store);
        let (runs, mut sent) = mpsc::unbounded_channel();

        resume_queued(
            &store,
            &ManualReviews::default(),
            &config.skip_rules(),
            "me",
            &runs,
        )
        .unwrap();
        let sent: Vec<QueuedRun> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
        assert_eq!(sent, [resumed]);
        let store = store.lock().unwrap();
        assert_eq!(
            store.run(interrupted.id).unwrap().unwrap().status,
            "superseded"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn r_starts_the_held_run_only_where_its_profile_is_held() {
        let key = key();
        let mut store = store_with_pr(&key);
        let queued = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        // Its profile's own holds it, with the runner's off.
        let held = ManualReviews {
            runner: false,
            overrides: vec![("p".into(), true)],
            ..ManualReviews::default()
        };
        let started = handle(&store, held, vec![Request::Rerun(key.clone())]).await;
        assert_eq!(started, std::slice::from_ref(&queued));
        // Its profile's own lets it run by itself, with the runner's on:
        // it's waiting for a slot, and its head's review is already queued.
        let runs = ManualReviews {
            runner: true,
            overrides: vec![("p".into(), false)],
            ..ManualReviews::default()
        };
        assert!(
            handle(&store, runs, vec![Request::Rerun(key)])
                .await
                .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn r_goes_by_the_profile_the_run_was_queued_under_not_the_prs() {
        let key = key();
        let mut store = store_with_pr(&key);
        // Queued under `p`, then the PR's repo moves to `q`.
        let queued = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        store.record(&pr_snapshot(&key), "me", "q", &[]).unwrap();
        let store = Arc::new(Mutex::new(store));
        let holding = |profile: &str| ManualReviews {
            runner: false,
            overrides: vec![(profile.into(), true)],
            ..ManualReviews::default()
        };
        // Only `q` holds: the run isn't held, so there's nothing to start.
        assert!(
            handle(&store, holding("q"), vec![Request::Rerun(key.clone())])
                .await
                .is_empty()
        );
        // Only `p` holds: it's held, and `r` starts it.
        let started = handle(&store, holding("p"), vec![Request::Rerun(key)]).await;
        assert_eq!(started, std::slice::from_ref(&queued));
    }

    #[tokio::test]
    async fn manual_reviews_hold_until_turned_off_then_start_what_they_held() {
        let key = key();
        let mut store = store_with_pr(&key);
        let held = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let (runs, runs_rx) = mpsc::unbounded_channel();
        let (started, started_rx) = mpsc::unbounded_channel();
        let (manual, manual_rx) = watch::channel(ManualReviews::runner(true));
        let (jobs, mut jobs_rx) = mpsc::unbounded_channel();
        let gate = tokio::spawn(hold(
            Inputs {
                runs: runs_rx,
                started: started_rx,
                manual: manual_rx,
                store: Arc::new(Mutex::new(store)),
                checks: mpsc::unbounded_channel().0,
            },
            jobs,
        ));
        let job = |run: &QueuedRun, automatic| Job {
            run: run.clone(),
            automatic,
        };
        let other = |id| QueuedRun { id, ..held.clone() };

        // The scheduler's run is only logged; one started by hand, taken
        // after it, goes through.
        runs.send(held.clone()).unwrap();
        started.send(other(100)).unwrap();
        assert_eq!(jobs_rx.recv().await, Some(job(&other(100), false)));

        // Turning them off starts the held review, from the store, and
        // what's queued after it.
        manual.send_replace(ManualReviews::runner(false));
        runs.send(other(101)).unwrap();
        assert_eq!(jobs_rx.recv().await, Some(job(&held, true)));
        assert_eq!(jobs_rx.recv().await, Some(job(&other(101), true)));

        // Turned on again, what's queued is held once more.
        manual.send_replace(ManualReviews::runner(true));
        runs.send(other(102)).unwrap();
        drop((runs, started, manual));
        gate.await.unwrap();
        assert_eq!(jobs_rx.recv().await, None);
    }

    #[tokio::test]
    async fn a_teams_members_arriving_start_their_held_reviews() {
        use sanic_core::{manual::AutoAuthors, pr::TeamRef};

        let key = key();
        let mut store = store_with_pr(&key);
        let held = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let team = TeamRef::new("org", "backend");
        let before = ManualReviews {
            auto: AutoAuthors {
                teams: vec![team.clone()],
                ..AutoAuthors::default()
            },
            ..ManualReviews::runner(true)
        };
        let (runs, runs_rx) = mpsc::unbounded_channel();
        let (_started, started_rx) = mpsc::unbounded_channel();
        let (manual, manual_rx) = watch::channel(before.clone());
        let (jobs, mut jobs_rx) = mpsc::unbounded_channel();
        let gate = tokio::spawn(hold(
            Inputs {
                runs: runs_rx,
                started: started_rx,
                manual: manual_rx,
                store: Arc::new(Mutex::new(store)),
                checks: mpsc::unbounded_channel().0,
            },
            jobs,
        ));
        // Its members aren't known yet, so alice's review is held.
        runs.send(held.clone()).unwrap();
        let alice = std::collections::HashSet::from(["alice".to_owned()]);
        let members = std::collections::HashMap::from([(team, alice)]);
        manual.send_replace(before.with_members(Arc::new(members)));
        let job = |run: &QueuedRun| Job {
            run: run.clone(),
            automatic: true,
        };
        assert_eq!(jobs_rx.recv().await, Some(job(&held)));
        // From then on hers go straight through.
        let next = QueuedRun { id: 100, ..held };
        runs.send(next.clone()).unwrap();
        assert_eq!(jobs_rx.recv().await, Some(job(&next)));
        drop((runs, manual));
        gate.abort();
    }

    #[tokio::test]
    async fn a_profiles_own_manual_reviews_hold_its_runs_whatever_the_runners_say() {
        let key = key();
        let mut store = store_with_pr(&key);
        // Of profile `p`.
        let held = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let manual_with = |runner, p: Option<bool>| ManualReviews {
            runner,
            overrides: p.map(|on| ("p".to_owned(), on)).into_iter().collect(),
            ..ManualReviews::default()
        };
        let (runs, runs_rx) = mpsc::unbounded_channel();
        let (started, started_rx) = mpsc::unbounded_channel::<QueuedRun>();
        let (manual, manual_rx) = watch::channel(manual_with(false, Some(true)));
        let (jobs, mut jobs_rx) = mpsc::unbounded_channel();
        let gate = tokio::spawn(hold(
            Inputs {
                runs: runs_rx,
                started: started_rx,
                manual: manual_rx,
                store: Arc::new(Mutex::new(store)),
                checks: mpsc::unbounded_channel().0,
            },
            jobs,
        ));
        let job = |run: &QueuedRun| Job {
            run: run.clone(),
            automatic: true,
        };
        let mut other = QueuedRun {
            id: 100,
            ..held.clone()
        };
        other.request.profile = "q".into();

        // `p`'s own holds its run; `q` follows the runner's, which is off.
        runs.send(held.clone()).unwrap();
        runs.send(other.clone()).unwrap();
        assert_eq!(jobs_rx.recv().await, Some(job(&other)));

        // Switching the runner's either way leaves `p`'s run held. Each
        // switch is yielded to, so `hold` sees it rather than just the
        // last: a run it wrongly released would be sent a second time.
        for runner in [true, false] {
            manual.send_replace(manual_with(runner, Some(true)));
            tokio::task::yield_now().await;
        }
        // Its own turned off, it starts, from the store.
        manual.send_replace(manual_with(false, Some(false)));
        assert_eq!(jobs_rx.recv().await, Some(job(&held)));
        drop((runs, started, manual));
        gate.await.unwrap();
        assert_eq!(jobs_rx.recv().await, None);
    }

    /// A store with `key` tracked, and a `.claude` hold on it.
    fn store_with_claude_hold(key: &PrKey) -> Store {
        let mut store = store_with_pr(key);
        let found = [Finding::Path(".claude/skills/x/SKILL.md".into())];
        store.record_check(key, "b1", "h1", Ok(&found)).unwrap();
        store
    }

    #[tokio::test(start_paused = true)]
    async fn r_doesnt_start_a_head_still_being_checked_but_does_one_that_failed() {
        let key = key();
        let mut store = Store::open_in_memory().unwrap();
        store.record(&pr_snapshot(&key), "me", "p", &[]).unwrap();
        let queued = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let rerun = || vec![Request::Rerun(key.clone())];
        assert!(
            handle(&store, ManualReviews::default(), rerun())
                .await
                .is_empty()
        );
        store
            .lock()
            .unwrap()
            .record_check(&key, "b1", "h1", Err("boom"))
            .unwrap();
        assert_eq!(
            handle(&store, ManualReviews::default(), rerun()).await,
            [queued]
        );
    }

    #[tokio::test]
    async fn a_claude_hold_keeps_every_automatic_review_whatever_manual_reviews_say() {
        let key = key();
        let mut store = store_with_claude_hold(&key);
        let requested = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let mut push = store.review_request(&key).unwrap().unwrap();
        push.head_sha = "h2".into();
        push.trigger = ReviewTrigger::Push {
            from_sha: "h1".into(),
        };
        let pushed = store.queue_review(&push).unwrap().unwrap();
        let store = Arc::new(Mutex::new(store));
        let (runs, runs_rx) = mpsc::unbounded_channel();
        let (started, started_rx) = mpsc::unbounded_channel();
        let (manual, manual_rx) = watch::channel(ManualReviews::runner(true));
        let (jobs, mut jobs_rx) = mpsc::unbounded_channel();
        let (checks, mut checks_rx) = mpsc::unbounded_channel();
        let gate = tokio::spawn(hold(
            Inputs {
                runs: runs_rx,
                started: started_rx,
                manual: manual_rx,
                store: Arc::clone(&store),
                checks,
            },
            jobs,
        ));

        // Turning manual reviews off releases neither, and neither a
        // request's review nor a push's goes through.
        manual.send_replace(ManualReviews::runner(false));
        tokio::task::yield_now().await;
        runs.send(requested.clone()).unwrap();
        runs.send(pushed.clone()).unwrap();
        // Started by hand, it goes through.
        started.send(pushed.clone()).unwrap();
        assert_eq!(
            jobs_rx.recv().await,
            Some(Job {
                run: pushed.clone(),
                automatic: false
            })
        );
        drop((runs, started, manual));
        gate.await.unwrap();
        assert_eq!(jobs_rx.recv().await, None);
        // Only the push's head, which isn't checked yet, is left to check.
        assert_eq!(checks_rx.recv().await, Some(pushed));
        assert_eq!(checks_rx.recv().await, None);
        // Nor does the worker take it from the queue.
        assert!(
            store
                .lock()
                .unwrap()
                .claim_next(|_, _| false)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn r_starts_a_claude_held_run_with_manual_reviews_off() {
        let key = key();
        let mut store = store_with_claude_hold(&key);
        let queued = store
            .queue_review(&store.review_request(&key).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let started = handle(&store, ManualReviews::default(), vec![Request::Rerun(key)]).await;
        assert_eq!(started, [queued]);
    }

    #[test]
    fn a_first_run_edits_only_with_a_terminal_in_the_foreground() {
        assert!(can_edit(true, Stdout::Foreground));
        assert!(!can_edit(false, Stdout::Foreground));
        assert!(!can_edit(true, Stdout::Background));
        assert!(!can_edit(true, Stdout::NotTerminal));
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_config_behind_a_link_names_the_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let link = dir.path().join("config.toml");
        let target = dir.path().join("dotfiles/sanic.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(Config::read_text(&link).unwrap(), None);
        let err = missing_config(&link).to_string();
        assert!(
            err.contains(&format!("links to {}", target.display())),
            "{err}"
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
            let worker = Worker::new(dir.path(), store, poller.config(), "me");
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

    /// A refresh that finds a PR merged or its head approved drops its
    /// queued review.
    #[tokio::test]
    async fn refreshes_supersede_reviews_gone_stale() {
        use sanic_core::pr::{Review, ReviewState};

        use crate::poll::tests::{FakeGithub, key, poller, snapshot};

        let prs = [key("org/a", 1), key("org/b", 2), key("org/c", 3)];
        let github = FakeGithub::default();
        for pr in &prs {
            let mut snap = snapshot(pr, "alice", &[]);
            snap.review_requested = true;
            github.prs.borrow_mut().insert(pr.clone(), snap);
        }
        let mut poller = poller(github);
        let (updates, _updates) = mpsc::unbounded_channel();
        let live = Live::new(poller.config(), None);
        let refresh = async |poller: &mut Poller<_>| {
            let mut pending = RefreshQueue::default();
            pending.extend(prs.clone(), Priority::Involved);
            let mut asked = mpsc::unbounded_channel().1;
            refresh_queued(
                poller,
                &mut pending,
                &mut asked,
                &mut HashSet::new(),
                &updates,
                &live,
            )
            .await
            .unwrap();
        };
        refresh(&mut poller).await;
        let runs: Vec<QueuedRun> = prs
            .iter()
            .map(|pr| {
                let store = poller.store_mut();
                let req = store.review_request(pr).unwrap().unwrap();
                store.queue_review(&req).unwrap().unwrap()
            })
            .collect();

        poller.github().prs.borrow_mut().remove(&prs[0]);
        poller
            .github()
            .prs
            .borrow_mut()
            .get_mut(&prs[1])
            .unwrap()
            .reviews
            .push(Review {
                id: "r1".into(),
                author: "bob".into(),
                state: ReviewState::Approved,
                body: String::new(),
                submitted_at: "2026-01-02T00:00:00Z".into(),
                commit: Some("h1".into()),
                by_bot: false,
            });
        refresh(&mut poller).await;
        let status = |run: &QueuedRun| poller.store().run(run.id).unwrap().unwrap().status;
        assert_eq!(status(&runs[0]), "superseded");
        assert_eq!(status(&runs[1]), "superseded");
        assert_eq!(status(&runs[2]), "queued");
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
            worker: Arc::new(Worker::new(dir.path(), Arc::clone(&store), &config, "me")),
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

        // Manual reviews are switched in `[runner]`, for the reload to
        // pick up.
        assert!(control.set_manual_reviews(false).unwrap().unwrap());
        assert!(!control.set_manual_reviews(false).unwrap().unwrap());
        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            text.ends_with("\n[runner]\nmanual_reviews = false\n"),
            "{text}"
        );
        // A config that doesn't load, as the reload would find it, is left
        // alone, with why.
        for (from, to, why) in [
            (
                "manual_reviews = false",
                "manual_reviews = \"no\"",
                "manual_reviews",
            ),
            (
                "[runner]",
                "[poll]\nreconcile_secs = 0\n[runner]",
                "reconcile_secs",
            ),
        ] {
            let broken = text.replace(from, to);
            std::fs::write(&config_path, &broken).unwrap();
            let refused = control.set_manual_reviews(true).unwrap().unwrap_err();
            assert!(refused.to_string().contains(why), "{refused}");
            assert_eq!(std::fs::read_to_string(&config_path).unwrap(), broken);
        }
        std::fs::write(&config_path, &text).unwrap();
        let reloaded = Config::parse(&text, dir.path(), &VcsResolver).unwrap();
        let mut manual = control.live.manual_reviews.subscribe();
        control.live.publish(&reloaded);
        assert!(manual.has_changed().unwrap());
        assert!(!manual.borrow_and_update().runner);
        // A reload that leaves it as it was doesn't say it changed.
        control.live.publish(&reloaded);
        assert!(!manual.has_changed().unwrap());

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
            resumed_from: None,
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
            resumed_from: None,
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
