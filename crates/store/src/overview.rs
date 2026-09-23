//! Read-only summaries of tracked PRs and recent activity, for the terminal
//! UI.

use color_eyre::eyre::Result;
use rusqlite::{Row, params, types::Type};
use sanic_core::{pr::PrKey, repo::RepoName, state::PrState};

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
    /// Open PRs by others that request your review, by repo and number.
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
             WHERE p.open AND p.review_requested AND lower(p.author) != lower(?1)
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
        pr::{PrSnapshot, Review, ReviewState as GithubState},
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
        store.record(&requested, "default", &[]).unwrap();
        store.record(&mine, "default", &[]).unwrap();
        store.record(&snapshot(1, "bob"), "default", &[]).unwrap();
        let mut merged = snapshot(4, "alice");
        merged.review_requested = true;
        store.record(&merged, "default", &[]).unwrap();
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
            store.record(snap, "default", &[]).unwrap();
        }
        store.record(&snapshot(5, "alice"), "default", &[]).unwrap();
        let merged = snapshot(6, "me");
        store.record(&merged, "default", &[]).unwrap();
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
            store.record(snap, "default", &[]).unwrap();
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
            state::{Approval, PrState},
        };
        let comment = |author: &str, at: &str| Comment {
            id: format!("{author}{at}"),
            author: author.into(),
            body: String::new(),
            created_at: at.into(),
            by_bot: false,
            reacted_at: None,
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
            comments: vec![comment("bob", "2026-01-02T00:00:00Z")],
        }];
        let mut owed = snapshot(2, "alice");
        owed.review_requested = true;
        // Bob's question in a thread you haven't joined doesn't count here.
        owed.threads = mine.threads.clone();
        store.record(&mine, "default", &[]).unwrap();
        store.record(&owed, "default", &[]).unwrap();

        assert_eq!(
            store.my_prs("me", None).unwrap()[0].state,
            PrState {
                approval: Approval::Mergeable,
                unanswered: 1,
            }
        );
        assert_eq!(
            store.owed_reviews("me", None).unwrap()[0].state,
            PrState::default()
        );
    }

    #[test]
    fn activity_is_newest_first_and_limited() {
        let mut store = Store::open_in_memory().unwrap();
        let mut snap = snapshot(7, "alice");
        snap.review_requested = true;
        let trigger = Trigger::ReviewRequested {
            head_sha: "h1".into(),
        };
        store.record(&snap, "default", &[trigger]).unwrap();
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
