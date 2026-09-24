//! Read-only summaries of tracked PRs and recent activity, for the terminal
//! UI.

use color_eyre::eyre::Result;
use rusqlite::{OptionalExtension, Row, params, types::Type};
use sanic_core::{
    pr::{PrKey, is_login},
    repo::RepoName,
    reviewers::{ReviewRecord, Reviewer, SeenHead, reviewers},
    run::RunKind,
    state::{Awaiting, Merge, PrState, awaiting},
};

use crate::Store;

/// Someone else's PR that requests your review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwedReview {
    pub key: PrKey,
    pub title: String,
    /// The PR description.
    pub body: String,
    pub author: String,
    /// The profile it matched when last polled.
    pub profile: String,
    pub is_draft: bool,
    /// Set by you; it stops automatic reviews.
    pub archived: bool,
    pub head_sha: String,
    /// See [`Store::head_reviewers`].
    pub head_reviewers: Vec<String>,
    /// The latest run with an agent session to chat with.
    pub chat_run: Option<i64>,
    /// Where it stands; see [`Store::pr_state`].
    pub state: PrState,
    /// The most recently queued review run, if any.
    pub latest_run: Option<LatestRun>,
    pub pending_drafts: u32,
}

impl OwedReview {
    /// The latest run's status, e.g. `failed`, if it has run at all.
    #[must_use]
    pub fn latest_status(&self) -> Option<&str> {
        self.latest_run.as_ref().map(|run| run.status.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRun {
    pub status: String,
    /// Why a failed or crashed run ended.
    pub error: Option<String>,
}

/// An open PR you authored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MyPr {
    pub key: PrKey,
    pub title: String,
    pub is_draft: bool,
    pub archived: bool,
    pub pending_drafts: u32,
    /// The latest run with an agent session to chat with.
    pub chat_run: Option<i64>,
    /// Where it stands; see [`Store::pr_state`].
    pub state: PrState,
}

/// What the dashboard's index shows of a listed PR beyond its list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowFacts {
    pub head_sha: String,
    /// See [`reviewers`].
    pub reviewers: Vec<Reviewer>,
    /// When the poller first saw the current head: the push, give or take
    /// a poll.
    pub head_seen_at: Option<String>,
    /// Your comments waiting on an answer; see [`awaiting`].
    pub awaiting: Vec<Awaiting>,
    pub merge: Merge,
    /// The latest run of any kind, as [`OwedReview::latest_run`] picks it.
    pub latest_run: Option<RunTimes>,
    /// The drafts of the latest review that succeeded, whose drafts the PR
    /// page shows.
    pub decided: Option<Decided>,
}

/// A run's status, and when it was queued, started and finished, as the
/// store writes times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTimes {
    pub status: String,
    pub queued_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// One run's drafts, by status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Decided {
    pub run_id: i64,
    pub pending: u32,
    /// Accepted and not yet posted.
    pub accepted: u32,
    pub rejected: u32,
    pub posted: u32,
    /// When the last of them was marked posted.
    pub posted_at: Option<String>,
    /// You left a submitted review on the commit the run reviewed.
    pub you_reviewed: bool,
    /// A newer review of the PR is queued or running, and will replace
    /// these drafts.
    pub newer: bool,
}

impl Decided {
    /// Every draft rejected, nothing of yours on the commit, and no newer
    /// review on its way: the review is yours to write.
    #[must_use]
    pub fn submit_review(&self) -> bool {
        self.rejected > 0
            && self.pending == 0
            && self.accepted == 0
            && self.posted == 0
            && !self.you_reviewed
            && !self.newer
    }
}

/// Something that happened to a tracked PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// RFC 3339 UTC.
    pub at: String,
    pub key: PrKey,
    pub kind: ActivityKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityKind {
    /// A detected trigger, by its kind (`review_requested`, `push`, ...).
    Trigger(String),
    RunQueued,
    RunStarted,
    /// A run ended: succeeded, failed, crashed or superseded. `error` says
    /// why a failed or crashed one did.
    RunFinished {
        status: String,
        error: Option<String>,
    },
}

impl Store {
    /// Open PRs by others that you review, by repo and number: see
    /// `PrSnapshot::is_reviewer`.
    /// With `since`, only those GitHub saw activity on from then on, or
    /// that have no GitHub timestamp stored.
    pub fn owed_reviews(&self, me: &str, since: Option<&str>) -> Result<Vec<OwedReview>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.repo, p.number, p.title, p.author, p.profile, p.is_draft, p.archived,
                    latest.status, latest.error,
                    (SELECT count(*) FROM drafts d JOIN runs r ON r.id = d.run_id
                     WHERE r.repo = p.repo AND r.number = p.number AND d.status = 'pending'),
                    p.body, p.head_sha, (
                        SELECT id FROM runs s
                        WHERE s.repo = p.repo AND s.number = p.number AND s.session_id IS NOT NULL
                        ORDER BY coalesce(s.finished_at, s.queued_at) DESC, s.id DESC LIMIT 1)
             FROM prs p
             LEFT JOIN runs latest ON latest.id = (
                 SELECT id FROM runs r
                 WHERE r.repo = p.repo AND r.number = p.number
                 ORDER BY r.queued_at DESC, r.id DESC LIMIT 1)
             -- lower() on both sides is `is_login`'s rule.
             WHERE p.open AND p.reviewer AND lower(p.author) != lower(?1)
                   AND (?2 IS NULL OR p.github_updated_at IS NULL OR p.github_updated_at >= ?2)
             ORDER BY p.repo, p.number",
        )?;
        let mut owed = stmt
            .query_map(params![me, since], |row| {
                let status: Option<String> = row.get(7)?;
                let error: Option<String> = row.get(8)?;
                Ok(OwedReview {
                    key: key_columns(row)?,
                    title: row.get(2)?,
                    author: row.get(3)?,
                    profile: row.get(4)?,
                    is_draft: row.get(5)?,
                    archived: row.get(6)?,
                    latest_run: status.map(|status| LatestRun { status, error }),
                    pending_drafts: row.get(9)?,
                    body: row.get(10)?,
                    head_sha: row.get(11)?,
                    head_reviewers: Vec::new(),
                    chat_run: row.get(12)?,
                    state: PrState::default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for pr in &mut owed {
            pr.head_reviewers = self.head_reviewers(&pr.key, &pr.head_sha)?;
            pr.state = self.pr_state(&pr.key, me, false)?;
        }
        Ok(owed)
    }

    /// Open PRs you authored, by repo and number, limited by `since` as
    /// [`Store::owed_reviews`] is.
    pub fn my_prs(&self, me: &str, since: Option<&str>) -> Result<Vec<MyPr>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.repo, p.number, p.title, p.is_draft, p.archived,
                    (SELECT count(*) FROM drafts d JOIN runs r ON r.id = d.run_id
                     WHERE r.repo = p.repo AND r.number = p.number AND d.status = 'pending')
             FROM prs p
             -- lower() on both sides is `is_login`'s rule.
             WHERE p.open AND lower(p.author) = lower(?1)
                   AND (?2 IS NULL OR p.github_updated_at IS NULL OR p.github_updated_at >= ?2)
             ORDER BY p.repo, p.number",
        )?;
        let rows = stmt
            .query_map(params![me, since], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(
                |(repo, number, title, is_draft, archived, pending_drafts)| {
                    let key = key(&repo, number)?;
                    Ok(MyPr {
                        chat_run: self.latest_session_run(&key)?.map(|s| s.run.id),
                        state: self.pr_state(&key, me, true)?,
                        key,
                        title,
                        is_draft,
                        archived,
                        pending_drafts,
                    })
                },
            )
            .collect()
    }

    /// What the index shows of `key` beyond its list entry, for `me`.
    pub fn row_facts(&self, key: &PrKey, me: &str) -> Result<RowFacts> {
        let repo = key.repo.to_string();
        let (author, head, merge_state, checks): (String, String, Option<String>, Option<String>) =
            self.conn.query_row(
                "SELECT author, head_sha, merge_state, checks FROM prs
                 WHERE repo = ?1 AND number = ?2",
                params![repo, key.number],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let heads = self
            .conn
            .prepare_cached(
                "SELECT head_sha, seen_at FROM revisions WHERE repo = ?1 AND number = ?2
                 ORDER BY seen_at",
            )?
            .query_map(params![repo, key.number], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let reviews = self
            .conn
            .prepare_cached(
                "SELECT author, state, submitted_at, commit_sha FROM reviews
                 WHERE repo = ?1 AND number = ?2 AND NOT by_bot
                 ORDER BY submitted_at, rowid",
            )?
            .query_map(params![repo, key.number], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let seen: Vec<SeenHead<'_>> = heads
            .iter()
            .map(|(sha, seen_at)| SeenHead { sha, seen_at })
            .collect();
        let records: Vec<ReviewRecord<'_>> = reviews
            .iter()
            .map(|(author, state, submitted_at, commit)| ReviewRecord {
                author,
                state,
                submitted_at,
                commit: commit.as_deref(),
            })
            .collect();
        let mine = is_login(&author, me);
        let latest_run = self
            .conn
            .prepare_cached(
                "SELECT queued_at, started_at, finished_at, status FROM runs
                 WHERE repo = ?1 AND number = ?2
                 ORDER BY queued_at DESC, id DESC LIMIT 1",
            )?
            .query_row(params![repo, key.number], |row| {
                Ok(RunTimes {
                    queued_at: row.get(0)?,
                    started_at: row.get(1)?,
                    finished_at: row.get(2)?,
                    status: row.get(3)?,
                })
            })
            .optional()?;
        Ok(RowFacts {
            reviewers: reviewers(&records, &seen, &head),
            head_seen_at: heads
                .iter()
                .find(|(sha, _)| *sha == head)
                .map(|(_, at)| at.clone()),
            awaiting: awaiting(&self.threads(key)?, me, (!mine).then_some(author.as_str())),
            merge: Merge::new(merge_state.as_deref(), checks.as_deref()),
            latest_run,
            decided: self.decided(key, me)?,
            head_sha: head,
        })
    }

    /// The drafts of `key`'s latest review that succeeded, by status.
    fn decided(&self, key: &PrKey, me: &str) -> Result<Option<Decided>> {
        let repo = key.repo.to_string();
        Ok(self
            .conn
            .prepare_cached(
                "SELECT r.id,
                        count(*) FILTER (WHERE d.status = 'pending'),
                        count(*) FILTER (WHERE d.status = 'accepted'),
                        count(*) FILTER (WHERE d.status = 'rejected'),
                        count(*) FILTER (WHERE d.status = 'posted'),
                        max(d.updated_at) FILTER (WHERE d.status = 'posted'),
                        EXISTS (SELECT 1 FROM reviews v
                                WHERE v.repo = r.repo AND v.number = r.number
                                      AND v.commit_sha = r.head_sha AND NOT v.by_bot
                                      AND lower(v.author) = lower(?3)
                                      AND v.state IN ('APPROVED', 'CHANGES_REQUESTED',
                                                      'COMMENTED')),
                        EXISTS (SELECT 1 FROM runs n
                                WHERE n.repo = r.repo AND n.number = r.number
                                      AND n.kind IN (?4, ?5)
                                      AND n.status IN ('queued', 'running')
                                      AND (n.queued_at, n.id) > (r.queued_at, r.id))
                 FROM runs r LEFT JOIN drafts d ON d.run_id = r.id
                 WHERE r.id = (SELECT id FROM runs
                               WHERE repo = ?1 AND number = ?2 AND status = 'succeeded'
                                     AND kind IN (?4, ?5)
                               ORDER BY queued_at DESC, id DESC LIMIT 1)
                 GROUP BY r.id",
            )?
            .query_row(
                params![
                    repo,
                    key.number,
                    me,
                    RunKind::Review.as_str(),
                    RunKind::Regenerate.as_str()
                ],
                |row| {
                    Ok(Decided {
                        run_id: row.get(0)?,
                        pending: row.get(1)?,
                        accepted: row.get(2)?,
                        rejected: row.get(3)?,
                        posted: row.get(4)?,
                        posted_at: row.get(5)?,
                        you_reviewed: row.get(6)?,
                        newer: row.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    /// The `limit` most recent triggers and run transitions, newest first.
    /// A requeued run keeps only its latest queue, start and finish times.
    pub fn recent_activity(&self, limit: u32) -> Result<Vec<Activity>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT at, repo, number, 'trigger', kind, NULL FROM events
             UNION ALL
             SELECT queued_at, repo, number, 'queued', NULL, NULL FROM runs
             UNION ALL
             SELECT started_at, repo, number, 'started', NULL, NULL FROM runs
             WHERE started_at IS NOT NULL
             UNION ALL
             SELECT finished_at, repo, number, 'finished', status, error FROM runs
             WHERE finished_at IS NOT NULL
             ORDER BY 1 DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(at, repo, number, what, detail, error)| {
                let detail = detail.unwrap_or_default();
                let kind = match what.as_str() {
                    "trigger" => ActivityKind::Trigger(detail),
                    "queued" => ActivityKind::RunQueued,
                    "started" => ActivityKind::RunStarted,
                    _ => ActivityKind::RunFinished {
                        status: detail,
                        error,
                    },
                };
                Ok(Activity {
                    at,
                    key: key(&repo, number)?,
                    kind,
                })
            })
            .collect()
    }
}

/// The PR key in a row's first two columns.
pub(crate) fn key_columns(row: &Row<'_>) -> rusqlite::Result<PrKey> {
    let repo: String = row.get(0)?;
    Ok(PrKey {
        repo: RepoName::parse(&repo)
            .map_err(|err| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, err.into()))?,
        number: row.get(1)?,
    })
}

fn key(repo: &str, number: u32) -> Result<PrKey> {
    Ok(PrKey {
        repo: RepoName::parse(repo)?,
        number,
    })
}

#[cfg(test)]
mod tests {
    use sanic_core::{
        pr::{Placement, PrSnapshot, Review, ReviewState as GithubState},
        run::{ReviewRequest, ReviewResult, ReviewTrigger, Verdict},
        trigger::Trigger,
    };

    use super::*;

    fn snapshot(number: u32, author: &str) -> PrSnapshot {
        PrSnapshot {
            key: PrKey {
                repo: RepoName::new("org", "repo"),
                number,
            },
            title: format!("PR {number}"),
            body: String::new(),
            url: format!("https://github.com/org/repo/pull/{number}"),
            author: author.into(),
            head_sha: "h1".into(),
            base_sha: "b1".into(),
            is_draft: false,
            review_requested: false,
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

    fn review(id: &str, author: &str, state: GithubState, at: &str) -> Review {
        Review {
            id: id.into(),
            author: author.into(),
            state,
            body: String::new(),
            submitted_at: at.into(),
            commit: None,
            by_bot: false,
        }
    }

    fn request(snap: &PrSnapshot) -> ReviewRequest {
        ReviewRequest {
            key: snap.key.clone(),
            profile: "default".into(),
            head_sha: snap.head_sha.clone(),
            base_sha: snap.base_sha.clone(),
            trigger: ReviewTrigger::Requested,
        }
    }

    fn result() -> ReviewResult {
        ReviewResult {
            summary: "Fine.".into(),
            summary_note: None,
            verdict: Verdict::Comment,
            comments: vec![],
            session_id: None,
            transcript_path: "t".into(),
        }
    }

    #[test]
    fn owed_reviews_are_requested_prs_by_others() {
        let mut store = Store::open_in_memory().unwrap();
        let mut requested = snapshot(2, "alice");
        requested.review_requested = true;
        let mut mine = snapshot(3, "Me");
        mine.review_requested = true;
        store.record(&requested, "me", "default", &[]).unwrap();
        store.record(&mine, "me", "default", &[]).unwrap();
        store
            .record(&snapshot(1, "bob"), "me", "default", &[])
            .unwrap();
        let mut merged = snapshot(4, "alice");
        merged.review_requested = true;
        store.record(&merged, "me", "default", &[]).unwrap();
        store.mark_closed(&merged.key).unwrap();

        let owed = store.owed_reviews("me", None).unwrap();
        assert_eq!(
            owed,
            [OwedReview {
                key: requested.key.clone(),
                title: "PR 2".into(),
                body: String::new(),
                author: "alice".into(),
                profile: "default".into(),
                is_draft: false,
                archived: false,
                head_sha: "h1".into(),
                head_reviewers: vec![],
                chat_run: None,
                state: PrState::default(),
                latest_run: None,
                pending_drafts: 0,
            }]
        );

        // Archived PRs are still listed, flagged, for the UI to hide.
        store.set_archived(&requested.key, true).unwrap();
        assert!(store.owed_reviews("me", None).unwrap()[0].archived);
        store.set_archived(&requested.key, false).unwrap();

        let run = store.queue_review(&request(&requested)).unwrap().unwrap();
        let latest = |store: &Store| {
            store.owed_reviews("me", None).unwrap()[0]
                .latest_run
                .clone()
        };
        assert_eq!(latest(&store).unwrap().status, "queued");
        store.claim_run(run.id).unwrap();
        store.finish_review(run.id, &result()).unwrap();
        let owed = &store.owed_reviews("me", None).unwrap()[0];
        assert_eq!(owed.latest_run.as_ref().unwrap().status, "succeeded");
        assert_eq!(owed.pending_drafts, 1);

        let mut pushed = request(&requested);
        pushed.head_sha = "h2".into();
        let run = store.queue_review(&pushed).unwrap().unwrap();
        store.claim_run(run.id).unwrap();
        store.crash_run(run.id, "index out of bounds").unwrap();
        assert_eq!(
            latest(&store),
            Some(LatestRun {
                status: "crashed".into(),
                error: Some("index out of bounds".into()),
            })
        );
    }

    #[test]
    fn my_prs_carry_where_reviewers_stand() {
        let mut store = Store::open_in_memory().unwrap();
        let mut waiting = snapshot(1, "me");
        waiting.is_draft = true;
        waiting.reviews = vec![
            review("r0", "bob", GithubState::Commented, "2026-01-01T00:00:00Z"),
            review("r1", "me", GithubState::Commented, "2026-01-01T00:00:00Z"),
        ];
        let mut approved = snapshot(2, "ME");
        approved.reviews = vec![
            review(
                "r2",
                "bob",
                GithubState::ChangesRequested,
                "2026-01-01T00:00:00Z",
            ),
            review("r3", "Bob", GithubState::Approved, "2026-01-02T00:00:00Z"),
            review(
                "r4",
                "carol",
                GithubState::Commented,
                "2026-01-03T00:00:00Z",
            ),
        ];
        let mut blocked = snapshot(3, "me");
        blocked.reviews = vec![
            review("r5", "bob", GithubState::Approved, "2026-01-01T00:00:00Z"),
            review(
                "r6",
                "carol",
                GithubState::ChangesRequested,
                "2026-01-02T00:00:00Z",
            ),
        ];
        let mut dismissed = snapshot(4, "me");
        dismissed.reviews = vec![
            review(
                "r7",
                "bob",
                GithubState::ChangesRequested,
                "2026-01-01T00:00:00Z",
            ),
            review("r8", "bob", GithubState::Dismissed, "2026-01-02T00:00:00Z"),
        ];
        for snap in [&waiting, &approved, &blocked, &dismissed] {
            store.record(snap, "me", "default", &[]).unwrap();
        }
        store
            .record(&snapshot(5, "alice"), "me", "default", &[])
            .unwrap();
        let merged = snapshot(6, "me");
        store.record(&merged, "me", "default", &[]).unwrap();
        store.mark_closed(&merged.key).unwrap();

        let states: Vec<_> = store
            .my_prs("me", None)
            .unwrap()
            .into_iter()
            .map(|pr| (pr.key.number, pr.is_draft, pr.state.status()))
            .collect();
        // GitHub gave no review decision, so each reviewer's latest word
        // decides.
        assert_eq!(
            states,
            [
                (1, true, "—".to_owned()),
                (2, false, "approved".to_owned()),
                (3, false, "changes requested".to_owned()),
                (4, false, "—".to_owned()),
            ]
        );
    }

    #[test]
    fn lists_can_leave_out_prs_quiet_since_a_time() {
        let mut store = Store::open_in_memory().unwrap();
        let mut old = snapshot(1, "alice");
        old.review_requested = true;
        old.updated_at = Some("2026-01-01T00:00:00Z".into());
        let mut recent = snapshot(2, "alice");
        recent.review_requested = true;
        recent.updated_at = Some("2026-09-20T00:00:00Z".into());
        let mut unknown = snapshot(3, "alice");
        unknown.review_requested = true;
        let mut mine = snapshot(4, "me");
        mine.updated_at = Some("2026-01-01T00:00:00Z".into());
        for snap in [&old, &recent, &unknown, &mine] {
            store.record(snap, "me", "default", &[]).unwrap();
        }
        let since = Some("2026-09-09T00:00:00Z");
        let owed: Vec<u32> = store
            .owed_reviews("me", since)
            .unwrap()
            .iter()
            .map(|pr| pr.key.number)
            .collect();
        assert_eq!(owed, [2, 3]);
        assert_eq!(store.owed_reviews("me", None).unwrap().len(), 3);
        assert!(store.my_prs("me", since).unwrap().is_empty());
        assert_eq!(store.my_prs("me", None).unwrap().len(), 1);
    }

    #[test]
    fn prs_carry_their_state() {
        use sanic_core::{
            pr::{CONVERSATION_THREAD, Comment, Thread},
            state::{Approval, PrState, Urgency},
        };
        let comment = |author: &str, at: &str| Comment {
            id: format!("{author}{at}"),
            author: author.into(),
            body: String::new(),
            created_at: at.into(),
            url: None,
            by_bot: false,
            reacted_at: None,
            reactions: vec![],
        };
        let mut store = Store::open_in_memory().unwrap();
        let mut mine = snapshot(1, "me");
        mine.review_decision = Some("APPROVED".into());
        mine.merge_state = Some("CLEAN".into());
        mine.threads = vec![Thread {
            id: CONVERSATION_THREAD.into(),
            path: None,
            line: None,
            resolved: false,
            place: Placement::default(),
            comments: vec![comment("bob", "2026-01-02T00:00:00Z")],
        }];
        let mut owed = snapshot(2, "alice");
        owed.review_requested = true;
        // Bob's question in a thread you haven't joined doesn't count here.
        owed.threads = mine.threads.clone();
        store.record(&mine, "me", "default", &[]).unwrap();
        store.record(&owed, "me", "default", &[]).unwrap();

        assert_eq!(
            store.my_prs("me", None).unwrap()[0].state,
            PrState {
                approval: Approval::Mergeable,
                unanswered: 1,
                mine: true,
            }
        );
        assert_eq!(
            store.owed_reviews("me", None).unwrap()[0].state,
            PrState::default()
        );

        // Your own change request on a review you owe: it says so, but the
        // changes are the author's to make.
        let mut asked = snapshot(3, "alice");
        asked.review_requested = true;
        asked.reviews = vec![review(
            "r9",
            "me",
            GithubState::ChangesRequested,
            "2026-01-03T00:00:00Z",
        )];
        store.record(&asked, "me", "default", &[]).unwrap();
        let owed = store.owed_reviews("me", None).unwrap();
        let state = owed.iter().find(|pr| pr.key.number == 3).unwrap().state;
        assert_eq!(state.status(), "changes requested");
        assert_eq!(state.urgency(), Urgency::Quiet);
    }

    #[test]
    fn prs_you_have_reviewed_stay_owed_after_the_request_clears() {
        let mut store = Store::open_in_memory().unwrap();
        let mut snap = snapshot(2, "alice");
        snap.review_requested = true;
        store.record(&snap, "me", "default", &[]).unwrap();
        let listed = |store: &Store| -> Vec<u32> {
            let owed = store.owed_reviews("me", None).unwrap();
            owed.iter().map(|pr| pr.key.number).collect()
        };
        assert_eq!(listed(&store), [2]);

        // Submitting your review clears GitHub's request.
        snap.review_requested = false;
        let mut mine = review("r1", "Me", GithubState::Commented, "2026-01-02T00:00:00Z");
        mine.commit = Some("h1".into());
        snap.reviews = vec![mine];
        store.record(&snap, "me", "default", &[]).unwrap();
        assert_eq!(listed(&store), [2]);
        let owed = &store.owed_reviews("me", None).unwrap()[0];
        assert_eq!(owed.head_reviewers, ["Me"]);

        // A push, which may dismiss your review, gets its own run and
        // drafts on the same row.
        let trigger = Trigger::Push {
            from_sha: "h1".into(),
            to_sha: "h2".into(),
        };
        snap.head_sha = "h2".into();
        snap.reviews[0].state = GithubState::Dismissed;
        store.record(&snap, "me", "default", &[trigger]).unwrap();
        let mut pushed = request(&snap);
        pushed.trigger = ReviewTrigger::Push {
            from_sha: "h1".into(),
        };
        let run = store.queue_review(&pushed).unwrap().unwrap();
        store.claim_run(run.id).unwrap();
        store.finish_review(run.id, &result()).unwrap();
        let owed = store.owed_reviews("me", None).unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].head_reviewers, Vec::<String>::new());
        assert_eq!(owed[0].latest_run.as_ref().unwrap().status, "succeeded");
        assert_eq!(owed[0].pending_drafts, 1);
    }

    #[test]
    fn row_facts_say_who_reviewed_and_what_waits() {
        use sanic_core::{
            pr::{Comment, Placement, Reaction, Thread},
            reviewers::Stance,
            state::Block,
        };
        let comment = |id: &str, author: &str, at: &str| Comment {
            id: id.into(),
            author: author.into(),
            body: String::new(),
            created_at: at.into(),
            url: None,
            by_bot: false,
            reacted_at: None,
            reactions: vec![],
        };
        let mut store = Store::open_in_memory().unwrap();
        let mut snap = snapshot(2, "alice");
        snap.review_requested = true;
        snap.merge_state = Some("BLOCKED".into());
        snap.checks = Some("FAILURE".into());
        store.record(&snap, "me", "default", &[]).unwrap();
        let facts = store.row_facts(&snap.key, "me").unwrap();
        assert!(facts.reviewers.is_empty());
        assert_eq!(facts.merge.block, Some(Block::CiFailing));
        assert_eq!(facts.latest_run, None);
        assert_eq!(facts.decided, None);
        assert!(facts.head_seen_at.is_some());

        // You review h1 and ask Alice something; she reacts to it, then a
        // push, then Bob approves h2 and you ask again.
        let mut yours = review("r1", "me", GithubState::Commented, "2026-01-01T00:00:00Z");
        yours.commit = Some("h1".into());
        snap.reviews = vec![yours];
        let mut asked = comment("c1", "me", "2026-01-01T00:00:00Z");
        asked.reactions = vec![Reaction {
            login: "alice".into(),
            at: "2026-01-01T01:00:00Z".into(),
        }];
        snap.threads = vec![Thread {
            id: "t1".into(),
            path: None,
            line: None,
            resolved: false,
            place: Placement::default(),
            comments: vec![asked],
        }];
        store.record(&snap, "me", "default", &[]).unwrap();
        assert!(
            store
                .row_facts(&snap.key, "me")
                .unwrap()
                .awaiting
                .is_empty()
        );
        store
            .conn
            .execute(
                "UPDATE revisions SET seen_at = '2000-01-01T00:00:00.000Z'",
                [],
            )
            .unwrap();
        snap.head_sha = "h2".into();
        let mut bobs = review("r2", "bob", GithubState::Approved, "2026-01-02T00:00:00Z");
        bobs.commit = Some("h2".into());
        snap.reviews.push(bobs);
        snap.threads[0]
            .comments
            .push(comment("c2", "me", "2026-01-03T00:00:00Z"));
        store.record(&snap, "me", "default", &[]).unwrap();
        let facts = store.row_facts(&snap.key, "me").unwrap();
        let who: Vec<_> = facts
            .reviewers
            .iter()
            .map(|r| (r.login.as_str(), r.stance, r.pushes_since))
            .collect();
        assert_eq!(
            who,
            [("me", Stance::Commented, 1), ("bob", Stance::Approved, 0)]
        );
        assert_eq!(facts.awaiting.len(), 1);
        assert_eq!(facts.awaiting[0].login, "alice");
        // Reactions are stored as last polled.
        let threads = store.threads(&snap.key).unwrap();
        assert_eq!(threads[0].comments[0].reactions.len(), 1);
    }

    #[test]
    fn row_facts_count_the_latest_reviews_drafts() {
        use crate::DraftStatus;
        let mut store = Store::open_in_memory().unwrap();
        let mut snap = snapshot(2, "alice");
        snap.review_requested = true;
        store.record(&snap, "me", "default", &[]).unwrap();
        // A review whose drafts you all rejected, with nothing of yours on
        // its commit, is yours to write.
        let run = store.queue_review(&request(&snap)).unwrap().unwrap();
        store.claim_run(run.id).unwrap();
        store.finish_review(run.id, &result()).unwrap();
        let facts = store.row_facts(&snap.key, "me").unwrap();
        assert!(facts.latest_run.unwrap().finished_at.is_some());
        let decided = facts.decided.unwrap();
        assert_eq!((decided.run_id, decided.pending), (run.id, 1));
        assert!(!decided.submit_review());
        let summary = store.draft_rows(run.id).unwrap()[0].id;
        store
            .set_draft_status(summary, DraftStatus::Rejected)
            .unwrap();
        let decided = store.row_facts(&snap.key, "me").unwrap().decided.unwrap();
        assert_eq!((decided.pending, decided.rejected), (0, 1));
        assert!(decided.submit_review());
        // Not while a newer review is on its way.
        let mut pushed = request(&snap);
        pushed.head_sha = "h2".into();
        let newer = store.queue_review(&pushed).unwrap().unwrap();
        let decided = store.row_facts(&snap.key, "me").unwrap().decided.unwrap();
        assert!(decided.newer);
        assert!(!decided.submit_review());
        store.supersede_run(newer.id).unwrap();
        let decided = store.row_facts(&snap.key, "me").unwrap().decided.unwrap();
        assert!(decided.submit_review());
        store
            .set_draft_status(summary, DraftStatus::Accepted)
            .unwrap();
        store.mark_posted(&[summary]).unwrap();
        let decided = store.row_facts(&snap.key, "me").unwrap().decided.unwrap();
        assert_eq!(decided.posted, 1);
        assert!(decided.posted_at.is_some());
    }

    #[test]
    fn activity_is_newest_first_and_limited() {
        let mut store = Store::open_in_memory().unwrap();
        let mut snap = snapshot(7, "alice");
        snap.review_requested = true;
        let trigger = Trigger::ReviewRequested {
            head_sha: "h1".into(),
        };
        store.record(&snap, "me", "default", &[trigger]).unwrap();
        let run = store.queue_review(&request(&snap)).unwrap().unwrap();
        store.claim_run(run.id).unwrap();
        store.fail_run(run.id, "boom").unwrap();
        // Everything above can land in the same millisecond; spread it out
        // so the order is the one this test means.
        store
            .conn
            .execute_batch(
                "UPDATE events SET at = '2026-01-01T00:00:00.000Z';
                 UPDATE runs SET queued_at = '2026-01-01T00:00:01.000Z',
                                 started_at = '2026-01-01T00:00:02.000Z',
                                 finished_at = '2026-01-01T00:00:03.000Z';",
            )
            .unwrap();

        let kinds: Vec<_> = store
            .recent_activity(10)
            .unwrap()
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                ActivityKind::RunFinished {
                    status: "failed".into(),
                    error: Some("boom".into()),
                },
                ActivityKind::RunStarted,
                ActivityKind::RunQueued,
                ActivityKind::Trigger("review_requested".into()),
            ]
        );
        let latest = store.recent_activity(1).unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].at, "2026-01-01T00:00:03.000Z");
        assert_eq!(latest[0].key, snap.key);
    }
}
