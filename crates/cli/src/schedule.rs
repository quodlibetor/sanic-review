//! Turning triggers into queued review runs, once a PR has gone quiet.
//!
//! A review trigger starts a per-PR timer. Any later trigger on the PR
//! restarts it, and a later review trigger also replaces the pending
//! request, so a burst of pushes produces one review of the final head.
//! When the timer runs out, the store queues the run, which is idempotent
//! per head SHA and supersedes the PR's run that hasn't started yet.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use color_eyre::eyre::Result;
use sanic_core::{
    pr::PrKey,
    run::{QueuedRun, ReviewRequest, ReviewTrigger},
    trigger::Trigger,
};
use sanic_store::Store;
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tracing::{debug, info, warn};

use crate::poll::Refreshed;

/// A refreshed PR's triggers, as the scheduler sees them.
#[derive(Debug, Clone)]
pub struct Update {
    pub key: PrKey,
    pub profile: String,
    pub head_sha: String,
    pub base_sha: String,
    pub triggers: Vec<Trigger>,
    /// The quiet period configured when the update was sent, so a config
    /// reload applies to the next update without restarting the scheduler.
    pub quiet: Duration,
}

impl Update {
    #[must_use]
    pub fn new(refreshed: &Refreshed, quiet: Duration) -> Self {
        let snap = &refreshed.snapshot;
        Self {
            key: snap.key.clone(),
            profile: refreshed.profile.clone(),
            head_sha: snap.head_sha.clone(),
            base_sha: snap.base_sha.clone(),
            triggers: refreshed.triggers.clone(),
            quiet,
        }
    }

    /// The review this update asks for, if any. `reply` and `respond`
    /// triggers will get runs of their own.
    fn review(&self) -> Option<ReviewRequest> {
        let trigger = self
            .triggers
            .iter()
            .filter_map(|t| match t {
                Trigger::ReviewRequested { .. } => Some(ReviewTrigger::Requested),
                Trigger::Push { from_sha, .. } => Some(ReviewTrigger::Push {
                    from_sha: from_sha.clone(),
                }),
                Trigger::Reply { .. } | Trigger::Feedback { .. } | Trigger::Approved { .. } => None,
            })
            .reduce(ReviewTrigger::merge)?;
        Some(ReviewRequest {
            key: self.key.clone(),
            profile: self.profile.clone(),
            head_sha: self.head_sha.clone(),
            base_sha: self.base_sha.clone(),
            trigger,
        })
    }

    /// Whether this counts as activity on the PR. An approval is only
    /// informational, so it doesn't delay anything.
    fn is_activity(&self) -> bool {
        self.triggers
            .iter()
            .any(|t| !matches!(t, Trigger::Approved { .. }))
    }
}

/// A review request that was already standing when this process first
/// refreshed the PR. Detection treats a standing request as seen, so without
/// this a request that arrived just before a restart, while its review was
/// still being debounced, would never be reviewed. Heads already reviewed
/// are skipped when the run is queued.
#[must_use]
pub fn standing_request(refreshed: &Refreshed, me: &str) -> Option<Trigger> {
    let snap = &refreshed.snapshot;
    let already = refreshed
        .triggers
        .iter()
        .any(|t| matches!(t, Trigger::ReviewRequested { .. }));
    (snap.review_requested && !snap.is_authored_by(me) && !already).then(|| {
        Trigger::ReviewRequested {
            head_sha: snap.head_sha.clone(),
        }
    })
}

/// Pending reviews and when each is due.
#[derive(Debug, Default)]
pub struct Debouncer {
    pending: HashMap<PrKey, (Instant, ReviewRequest)>,
}

impl Debouncer {
    pub fn offer(&mut self, update: &Update, now: Instant) {
        let due = now + update.quiet;
        match (self.pending.remove(&update.key), update.review()) {
            (Some((_, queued)), Some(newer)) => {
                let trigger = queued.trigger.merge(newer.trigger.clone());
                let request = ReviewRequest { trigger, ..newer };
                self.pending.insert(update.key.clone(), (due, request));
            }
            (None, Some(request)) => {
                self.pending.insert(update.key.clone(), (due, request));
            }
            (Some((at, queued)), None) => {
                let at = if update.is_activity() { due } else { at };
                self.pending.insert(update.key.clone(), (at, queued));
            }
            (None, None) => {}
        }
    }

    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.values().map(|(due, _)| *due).min()
    }

    /// Removes and returns the requests due by `now`.
    pub fn take_due(&mut self, now: Instant) -> Vec<ReviewRequest> {
        let keys: Vec<PrKey> = self
            .pending
            .iter()
            .filter(|(_, (due, _))| *due <= now)
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .filter_map(|key| self.pending.remove(&key))
            .map(|(_, request)| request)
            .collect()
    }
}

/// Debounces `updates` and sends each run the store accepts to `runs`.
/// Returns when `updates` closes.
pub async fn schedule(
    mut updates: mpsc::UnboundedReceiver<Update>,
    store: Arc<Mutex<Store>>,
    runs: mpsc::UnboundedSender<QueuedRun>,
) -> Result<()> {
    let mut debouncer = Debouncer::default();
    loop {
        let next = debouncer.next_due();
        tokio::select! {
            update = updates.recv() => match update {
                Some(update) => debouncer.offer(&update, Instant::now()),
                None => return Ok(()),
            },
            () = sleep_until(next.unwrap_or_else(Instant::now)), if next.is_some() => {
                for request in debouncer.take_due(Instant::now()) {
                    let queued = store
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .queue_review(&request);
                    match queued {
                        Ok(Some(run)) => {
                            info!(pr = %request.key, run = run.id, "review queued");
                            if runs.send(run).is_err() {
                                return Ok(());
                            }
                        }
                        Ok(None) => {
                            debug!(pr = %request.key, head = %request.head_sha, "already reviewed");
                        }
                        Err(err) => warn!(pr = %request.key, "queueing review failed: {err:?}"),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use sanic_core::{pr::PrSnapshot, repo::RepoName};

    use super::*;

    const QUIET: Duration = Duration::from_mins(2);

    fn key(number: u32) -> PrKey {
        PrKey {
            repo: RepoName::new("org", "repo"),
            number,
        }
    }

    fn update(number: u32, head: &str, triggers: Vec<Trigger>) -> Update {
        Update {
            key: key(number),
            profile: "default".into(),
            head_sha: head.into(),
            base_sha: "b".into(),
            triggers,
            quiet: QUIET,
        }
    }

    fn requested(head: &str) -> Trigger {
        Trigger::ReviewRequested {
            head_sha: head.into(),
        }
    }

    fn push(from: &str, to: &str) -> Trigger {
        Trigger::Push {
            from_sha: from.into(),
            to_sha: to.into(),
        }
    }

    fn reply() -> Trigger {
        Trigger::Reply {
            thread_id: "t".into(),
            comment_ids: vec!["c".into()],
        }
    }

    fn approved() -> Trigger {
        Trigger::Approved {
            review_ids: vec!["r".into()],
            reviewers: vec!["bob".into()],
        }
    }

    #[test]
    fn a_newer_head_replaces_the_pending_review_but_stays_full() {
        let t0 = Instant::now();
        let mut d = Debouncer::default();
        d.offer(&update(1, "h1", vec![requested("h1")]), t0);
        d.offer(
            &update(1, "h2", vec![push("h1", "h2")]),
            t0 + Duration::from_secs(10),
        );
        assert!(d.take_due(t0 + QUIET).is_empty());
        let due = d.take_due(t0 + QUIET + Duration::from_secs(10));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].head_sha, "h2");
        assert_eq!(due[0].trigger, ReviewTrigger::Requested);
        assert_eq!(d.next_due(), None);
    }

    #[test]
    fn replies_delay_a_pending_review_and_approvals_do_not() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(30);
        let mut d = Debouncer::default();
        d.offer(&update(1, "h1", vec![requested("h1")]), t0);
        d.offer(&update(1, "h1", vec![approved()]), later);
        assert_eq!(d.next_due(), Some(t0 + QUIET));
        d.offer(&update(1, "h1", vec![reply()]), later);
        assert_eq!(d.next_due(), Some(later + QUIET));
    }

    #[test]
    fn a_reloaded_quiet_period_applies_to_the_next_update() {
        let t0 = Instant::now();
        let mut d = Debouncer::default();
        let mut short = update(1, "h1", vec![requested("h1")]);
        short.quiet = Duration::from_secs(5);
        d.offer(&short, t0);
        assert_eq!(d.next_due(), Some(t0 + Duration::from_secs(5)));
    }

    #[test]
    fn non_review_triggers_alone_queue_nothing() {
        let mut d = Debouncer::default();
        d.offer(&update(1, "h1", vec![reply()]), Instant::now());
        assert_eq!(d.next_due(), None);
    }

    #[test]
    fn prs_are_debounced_independently() {
        let t0 = Instant::now();
        let mut d = Debouncer::default();
        d.offer(&update(1, "h1", vec![requested("h1")]), t0);
        d.offer(
            &update(2, "h9", vec![requested("h9")]),
            t0 + Duration::from_secs(60),
        );
        let first = d.take_due(t0 + QUIET);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].key, key(1));
        assert_eq!(d.next_due(), Some(t0 + Duration::from_secs(60) + QUIET));
    }

    fn refreshed(author: &str, requested: bool, triggers: Vec<Trigger>) -> Refreshed {
        Refreshed {
            snapshot: snapshot(author, requested),
            profile: "default".into(),
            triggers,
        }
    }

    #[test]
    fn standing_requests_on_others_prs_are_reviewed() {
        assert_eq!(
            standing_request(&refreshed("alice", true, vec![]), "me"),
            Some(requested("h1"))
        );
        assert_eq!(standing_request(&refreshed("Me", true, vec![]), "me"), None);
        assert_eq!(
            standing_request(&refreshed("alice", false, vec![]), "me"),
            None
        );
        // A fresh request already triggers a review.
        assert_eq!(
            standing_request(&refreshed("alice", true, vec![requested("h1")]), "me"),
            None
        );
    }

    fn snapshot(author: &str, review_requested: bool) -> PrSnapshot {
        PrSnapshot {
            key: key(1),
            title: "t".into(),
            url: "u".into(),
            author: author.into(),
            head_sha: "h1".into(),
            base_sha: "b".into(),
            is_draft: false,
            review_requested,
            requested_teams: vec![],
            reviews: vec![],
            threads: vec![],
            files: None,
        }
    }

    fn store() -> Arc<Mutex<Store>> {
        let mut store = Store::open_in_memory().unwrap();
        store
            .record(&snapshot("alice", true), "default", &[])
            .unwrap();
        Arc::new(Mutex::new(store))
    }

    #[tokio::test(start_paused = true)]
    async fn runs_are_queued_once_the_pr_goes_quiet() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (runs_tx, mut runs) = mpsc::unbounded_channel();
        let task = tokio::spawn(schedule(rx, store(), runs_tx));
        let start = Instant::now();

        tx.send(update(1, "h1", vec![requested("h1")])).unwrap();
        tokio::time::sleep(Duration::from_secs(90)).await;
        tx.send(update(1, "h2", vec![push("h1", "h2")])).unwrap();
        let run = runs.recv().await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(90) + QUIET);
        assert_eq!(run.request.head_sha, "h2");

        // Asking again for the same head doesn't queue a second review.
        tx.send(update(1, "h2", vec![requested("h2")])).unwrap();
        tokio::time::sleep(QUIET * 2).await;
        drop(tx);
        task.await.unwrap().unwrap();
        assert!(runs.try_recv().is_err());
    }
}
