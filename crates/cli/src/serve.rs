//! The foreground `serve` loop.
//!
//! One task does everything in turn: reconcile and notification polls queue
//! PRs, then each queued PR is refreshed. Keeping it sequential means a PR
//! is never refreshed twice at once.

use std::{collections::BTreeSet, time::Duration};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, bail},
};
use sanic_core::{
    config::{Config, default_config_path, default_data_dir},
    pr::PrKey,
    trigger::Trigger,
};
use sanic_github::{ApiError, Client, Token};
use sanic_runner::vcs::VcsResolver;
use sanic_store::Store;
use tokio::time::{Instant, sleep_until};
use tracing::{Instrument, info, info_span, warn};

use crate::{
    ServeArgs, Ui,
    poll::{GithubApi, Poller, Refreshed},
};

pub async fn run(args: ServeArgs) -> Result<()> {
    if args.ui == Ui::Tui {
        bail!("`--ui tui` is not implemented yet; use `--ui logs`");
    }
    let config_path = match args.config {
        Some(path) => path,
        None => default_config_path()?,
    };
    let config = Config::load(&config_path, &VcsResolver)?;
    let data_dir = match args.data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    let store = Store::open(&data_dir.join("state.db"))?;
    let github = Client::new(&config.github.api_url, Token::discover()?)?;
    let me = github
        .viewer_login()
        .await
        .wrap_err("identifying the GitHub user")?;
    info!(user = %me, config = %config_path.display(), "watching GitHub");

    let mut poller = Poller::new(github, store, config, me);
    tokio::select! {
        result = poll_forever(&mut poller) => result,
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            Ok(())
        }
    }
}

async fn poll_forever<G: GithubApi>(poller: &mut Poller<G>) -> Result<()> {
    let reconcile_every = poller.config().poll.reconcile_interval;
    let min_notification = poller.config().poll.min_notification_interval;
    let mut next_reconcile = Instant::now();
    let mut next_notifications = Instant::now();
    // Survives across cycles so a rate limit doesn't drop queued PRs.
    let mut pending: BTreeSet<PrKey> = BTreeSet::new();

    loop {
        sleep_until(next_reconcile.min(next_notifications)).await;
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
                .instrument(info_span!("refresh", pr = %key))
                .await
            {
                Ok(Some(refreshed)) => {
                    triggered += refreshed.triggers.len();
                    log_triggers(&refreshed);
                }
                Ok(None) => {}
                Err(err @ (ApiError::RateLimited { .. } | ApiError::Unauthorized)) => {
                    pending.insert(key);
                    backoff = handle(err)?;
                }
                Err(err) => {
                    warn!(pr = %key, "refresh failed: {err}");
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
            pr = %snap.key,
            profile = %refreshed.profile,
            url = %snap.url,
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
