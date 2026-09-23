//! The foreground `serve` loop.
//!
//! One task does all the polling in turn: reconcile and notification polls
//! queue PRs, then each queued PR is refreshed. Keeping it sequential means
//! a PR is never refreshed twice at once. A config file change is picked up
//! between cycles. Refreshes with triggers go to the scheduler, which
//! debounces them into queued runs for the worker.

use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, bail},
};
use sanic_core::{
    config::{Config, default_config_path, default_data_dir},
    pr::PrKey,
    run::QueuedRun,
    trigger::Trigger,
};
use sanic_github::{ApiError, Client, Token};
use sanic_runner::vcs::VcsResolver;
use sanic_store::Store;
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tracing::{Instrument, info, info_span, warn};

use crate::{
    ServeArgs, Ui,
    poll::{GithubApi, Poller, Refreshed},
    schedule::{Update, schedule, standing_request},
    watch::ConfigWatcher,
    work::Worker,
};

pub async fn run(args: ServeArgs) -> Result<()> {
    if args.ui == Ui::Tui {
        bail!("`--ui tui` is not implemented yet; use `--ui logs`");
    }
    // Absolute, because runs hand these paths to `git -C <mirror>` and to
    // `claude` running in a worktree, where relative ones would resolve
    // somewhere else.
    let config_path = std::path::absolute(match args.config {
        Some(path) => path,
        None => default_config_path()?,
    })
    .wrap_err("resolving the config path")?;
    let config = Config::load(&config_path, &VcsResolver)?;
    let data_dir = std::path::absolute(match args.data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    })
    .wrap_err("resolving the data directory")?;
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
    let recovered = run_store
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .recover_runs()?;
    if !recovered.is_empty() {
        if args.no_reviews {
            info!(runs = recovered.len(), "queued runs held by --no-reviews");
        } else {
            info!(runs = recovered.len(), "resuming queued runs");
        }
    }
    for run in recovered {
        let _ = runs.send(run);
    }
    let worker = Arc::new(Worker::new(&data_dir, Arc::clone(&run_store), &config));

    let mut poller = Poller::new(github, store, config, me);
    tokio::select! {
        result = poll_forever(&mut poller, &mut watcher, &config_path, &updates, &worker) => result,
        result = schedule(updates_rx, run_store, runs) => result,
        () = run_or_hold(args.no_reviews, Arc::clone(&worker), runs_rx) => Ok(()),
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            Ok(())
        }
    }
}

/// Runs queued reviews, or with `--no-reviews` only logs them; they stay
/// queued in the store either way until a worker runs them.
async fn run_or_hold(
    hold: bool,
    worker: Arc<Worker>,
    mut runs: mpsc::UnboundedReceiver<QueuedRun>,
) {
    if !hold {
        return worker.work(runs).await;
    }
    info!("--no-reviews: reviews are queued but not run");
    while let Some(run) = runs.recv().await {
        info!(
            url = %run.request.key.url(),
            head = %run.request.head_sha,
            "review queued, not run (--no-reviews)"
        );
    }
}

async fn poll_forever<G: GithubApi>(
    poller: &mut Poller<G>,
    watcher: &mut ConfigWatcher,
    config_path: &Path,
    updates: &mpsc::UnboundedSender<Update>,
    worker: &Worker,
) -> Result<()> {
    let mut next_reconcile = Instant::now();
    let mut next_notifications = Instant::now();
    // Survives across cycles so a rate limit doesn't drop queued PRs.
    let mut pending: BTreeSet<PrKey> = BTreeSet::new();
    // PRs refreshed since startup; the first refresh of each checks for a
    // standing review request.
    let mut seen: HashSet<PrKey> = HashSet::new();

    loop {
        tokio::select! {
            () = sleep_until(next_reconcile.min(next_notifications)) => {}
            () = watcher.changed() => {
                if reload(poller, config_path, worker) {
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
                Ok(keys) => pending.extend(keys),
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
                    pending.extend(keys);
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
            && let Some(key) = pending.pop_first()
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
                    pending.insert(key);
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
fn reload<G: GithubApi>(poller: &mut Poller<G>, path: &Path, worker: &Worker) -> bool {
    match Config::load(path, &VcsResolver) {
        Ok(config) => {
            if config.github.api_url != poller.config().github.api_url {
                warn!("`github.api_url` changed; restart to use it");
            }
            worker.configure(&config);
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
    use super::*;

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
