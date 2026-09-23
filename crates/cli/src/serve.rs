//! The foreground `serve` loop.
//!
//! One task does all the polling in turn: reconcile and notification polls
//! queue PRs, then each queued PR is refreshed. Keeping it sequential means
//! a PR is never refreshed twice at once. A config file change is picked up
//! between cycles. Refreshes with triggers go to the scheduler, which
//! debounces them into queued runs for the worker.

use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr},
};
use sanic_core::{
    clock::SystemClock,
    config::{Config, default_config_path, default_data_dir},
    pr::PrKey,
    run::QueuedRun,
    skip::SkipRules,
    trigger::Trigger,
};
use sanic_github::{ApiError, Client, Token};
use sanic_runner::vcs::VcsResolver;
use sanic_store::Store;
use sanic_web::Dashboard;
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep_until},
};
use tracing::{Instrument, info, info_span, warn};

use crate::{
    ServeArgs, Ui, logging,
    poll::{GithubApi, Poller, Priority, RefreshQueue, Refreshed},
    schedule::{DueTimes, Update, schedule, standing_request},
    tui::{Request, Shared, Tui},
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
    let (due_tx, due) = watch::channel(DueTimes::new());
    let live = Live::new(&config);
    let web = live.dashboard(
        &me,
        args.manual_reviews,
        &data_dir,
        &github,
        &requests,
        &due,
    )?;
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
                clock: Arc::new(SystemClock),
                requests,
                config_path: config_path.clone(),
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

    let mut poller = Poller::new(github, store, config, me);
    // Dropping `ui` afterwards restores the terminal before any error is
    // printed.
    tokio::select! {
        result = quit => {
            info!("shutting down");
            result
        }
        result = poll_forever(
            &mut poller, &mut watcher, &config_path, &updates, &worker, &live,
        ) => result,
        result = schedule(
            updates_rx, Arc::clone(&run_store), runs.clone(), due_tx, live.skips.subscribe(),
        ) => result,
        () = handle_requests(requests_rx, run_store, Starter {
            manual: args.manual_reviews,
            start: started,
        }) => Ok(()),
        () = run_reviews(Arc::clone(&worker), runs_rx, started_rx) => Ok(()),
        result = web.serve(args.port) => result,
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            Ok(())
        }
    }
}

/// Config the scheduler and the TUI follow across reloads.
struct Live {
    skips: watch::Sender<SkipRules>,
    /// `poll.updated_within_days`.
    window: watch::Sender<Option<u32>>,
}

impl Live {
    fn new(config: &Config) -> Self {
        Self {
            skips: watch::Sender::new(config.skip_rules()),
            window: watch::Sender::new(config.poll.updated_within_days),
        }
    }

    fn publish(&self, config: &Config) {
        self.skips.send_replace(config.skip_rules());
        self.window.send_replace(config.poll.updated_within_days);
    }

    /// The dashboard, on its own store connection, seeing what the TUI
    /// sees.
    fn dashboard(
        &self,
        me: &str,
        manual_reviews: bool,
        data_dir: &Path,
        github: &Client,
        requests: &mpsc::UnboundedSender<Request>,
        due: &watch::Receiver<DueTimes>,
    ) -> Result<Dashboard> {
        Dashboard::new(sanic_web::Context {
            me: me.to_owned(),
            manual_reviews,
            data_dir: data_dir.to_owned(),
            store: Store::open(&data_dir.join("state.db"))?,
            github: github.clone(),
            control: Arc::new(DashboardControl(requests.clone())),
            due: due.clone(),
            skips: self.skips.subscribe(),
            window: self.window.subscribe(),
            clock: Arc::new(SystemClock),
        })
    }
}

/// The dashboard starts reviews the way the TUI's `r` does.
struct DashboardControl(mpsc::UnboundedSender<Request>);

impl sanic_web::Control for DashboardControl {
    fn review_now(&self, key: PrKey) {
        // Only fails once `serve` is stopping.
        let _ = self.0.send(Request::Rerun(key));
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

    loop {
        tokio::select! {
            () = sleep_until(next_reconcile.min(next_notifications)) => {}
            () = watcher.changed() => {
                if reload(poller, config_path, worker, live) {
                    // New entries may cover PRs nothing has fetched yet.
                    next_reconcile = Instant::now();
                }
                continue;
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
                    pending.extend(found.requested, Priority::Requested);
                    pending.extend(found.involved, Priority::Involved);
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
                    pending.extend(keys, Priority::Notified);
                    next_notifications = now + interval.unwrap_or_default().max(min_notification);
                }
                Err(err) => {
                    next_notifications = now + min_notification;
                    backoff = handle(err)?;
                }
            }
        }

        let mut triggered = 0;
        while backoff.is_none()
            && let Some((key, priority)) = pending.pop()
        {
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

        if let Some(wait) = backoff {
            let resume = Instant::now() + wait;
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
        };
        store.record(&snapshot, "p", &[]).unwrap();
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
