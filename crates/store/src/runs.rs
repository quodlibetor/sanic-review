//! Queries for agent runs and their drafts.

use color_eyre::eyre::{Result, WrapErr};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};
use sanic_core::{
    pr::{Comment, PrKey, Thread},
    repo::RepoName,
    run::{PrContext, QueuedRun, ReviewRequest, ReviewResult, ReviewTrigger, RunKind},
};

use crate::{NOW, Store};

const REVIEW: &str = RunKind::Review.as_str();

/// A run's current state, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub status: String,
    pub error: Option<String>,
    pub suggested_verdict: Option<String>,
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
}

/// A stored draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub kind: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub side: Option<String>,
    pub severity: Option<String>,
    pub body: String,
    pub status: String,
    pub unanchored: bool,
}

/// Counts for the terminal's summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunCounts {
    pub queued: u32,
    pub running: u32,
    pub pending_drafts: u32,
}

impl Store {
    /// Queues a review of `req`'s head, superseding any other review of the
    /// PR that hasn't started. `None` if this head is already queued,
    /// running or reviewed; a failed or superseded run of it is requeued.
    pub fn queue_review(&mut self, req: &ReviewRequest) -> Result<Option<QueuedRun>> {
        let repo = req.key.repo.to_string();
        let number = req.key.number;
        let key = req.idempotency_key();
        let from_sha = match &req.trigger {
            ReviewTrigger::Requested => None,
            ReviewTrigger::Push { from_sha } => Some(from_sha.as_str()),
        };
        // Immediate: this reads before it writes, and a deferred transaction
        // can't upgrade to a write after the poller's connection commits; it
        // fails at once instead of waiting out the busy timeout.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, status FROM runs
                 WHERE repo = ?1 AND number = ?2 AND kind = ?3 AND idem_key = ?4",
                params![repo, number, REVIEW, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((_, status)) = &existing
            && matches!(status.as_str(), "queued" | "running" | "succeeded")
        {
            return Ok(None);
        }
        tx.execute(
            &format!(
                "UPDATE runs SET status = 'superseded', finished_at = {NOW}
                 WHERE repo = ?1 AND number = ?2 AND kind = ?3 AND status = 'queued'"
            ),
            params![repo, number, REVIEW],
        )?;
        let id = if let Some((id, _)) = existing {
            tx.execute(
                &format!(
                    "UPDATE runs SET trigger = ?2, profile = ?3, base_sha = ?4, from_sha = ?5,
                         status = 'queued', error = NULL, suggested_verdict = NULL,
                         session_id = NULL, transcript_path = NULL,
                         queued_at = {NOW}, started_at = NULL, finished_at = NULL
                     WHERE id = ?1"
                ),
                params![
                    id,
                    req.trigger.as_str(),
                    req.profile,
                    req.base_sha,
                    from_sha
                ],
            )?;
            id
        } else {
            tx.execute(
                &format!(
                    "INSERT INTO runs (repo, number, kind, trigger, idem_key, profile, head_sha,
                                       base_sha, from_sha, status, queued_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', {NOW})"
                ),
                params![
                    repo,
                    number,
                    REVIEW,
                    req.trigger.as_str(),
                    key,
                    req.profile,
                    req.head_sha,
                    req.base_sha,
                    from_sha
                ],
            )?;
            tx.last_insert_rowid()
        };
        tx.commit()
            .wrap_err_with(|| format!("queueing a review of {}", req.key))?;
        Ok(Some(QueuedRun {
            id,
            request: req.clone(),
        }))
    }

    /// Marks a queued run as running. `false` if it was superseded while it
    /// waited.
    pub fn claim_run(&self, id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE runs SET status = 'running', started_at = {NOW}
                 WHERE id = ?1 AND status = 'queued'"
            ),
            [id],
        )?;
        Ok(changed == 1)
    }

    /// Stores a finished review's drafts and marks the run succeeded.
    pub fn finish_review(&mut self, id: i64, result: &ReviewResult) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            &format!(
                "UPDATE runs SET status = 'succeeded', suggested_verdict = ?2, session_id = ?3,
                     transcript_path = ?4, finished_at = {NOW}
                 WHERE id = ?1"
            ),
            params![
                id,
                result.verdict.as_str(),
                result.session_id,
                result.transcript_path
            ],
        )?;
        tx.execute(
            &format!(
                "INSERT INTO drafts (run_id, kind, original_body, status, unanchored,
                                     created_at, updated_at)
                 VALUES (?1, 'summary', ?2, 'pending', 0, {NOW}, {NOW})"
            ),
            params![id, result.summary],
        )?;
        for draft in &result.comments {
            let c = &draft.comment;
            tx.execute(
                &format!(
                    "INSERT INTO drafts (run_id, kind, path, line, start_line, side, severity,
                                         confidence, original_body, status, unanchored,
                                         created_at, updated_at)
                     VALUES (?1, 'comment', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9,
                             {NOW}, {NOW})"
                ),
                params![
                    id,
                    c.path,
                    c.line,
                    c.start_line,
                    c.side.as_str(),
                    c.severity.as_str(),
                    c.confidence.as_str(),
                    c.body,
                    draft.unanchored
                ],
            )?;
        }
        tx.commit()
            .wrap_err_with(|| format!("storing drafts for run {id}"))
    }

    pub fn fail_run(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            &format!(
                "UPDATE runs SET status = 'failed', error = ?2, finished_at = {NOW} WHERE id = ?1"
            ),
            params![id, error],
        )?;
        Ok(())
    }

    /// Requeues runs a previous process left running, and returns every
    /// queued run, oldest first.
    pub fn recover_runs(&self) -> Result<Vec<QueuedRun>> {
        self.conn.execute(
            "UPDATE runs SET status = 'queued', started_at = NULL WHERE status = 'running'",
            [],
        )?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, repo, number, profile, head_sha, base_sha, from_sha
             FROM runs WHERE status = 'queued' AND kind = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([REVIEW], queued_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(
                |(id, repo, number, profile, head_sha, base_sha, from_sha)| {
                    Ok(QueuedRun {
                        id,
                        request: ReviewRequest {
                            key: PrKey {
                                repo: RepoName::parse(&repo)?,
                                number,
                            },
                            profile,
                            head_sha,
                            base_sha,
                            trigger: from_sha.map_or(ReviewTrigger::Requested, |from_sha| {
                                ReviewTrigger::Push { from_sha }
                            }),
                        },
                    })
                },
            )
            .collect()
    }

    pub fn run(&self, id: i64) -> Result<Option<RunRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT status, error, suggested_verdict, session_id, transcript_path
                 FROM runs WHERE id = ?1",
                [id],
                |row| {
                    Ok(RunRecord {
                        status: row.get(0)?,
                        error: row.get(1)?,
                        suggested_verdict: row.get(2)?,
                        session_id: row.get(3)?,
                        transcript_path: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// A run's drafts, summary first.
    pub fn drafts(&self, run_id: i64) -> Result<Vec<Draft>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT kind, path, line, start_line, side, severity,
                    coalesce(edited_body, original_body), status, unanchored
             FROM drafts WHERE run_id = ?1 ORDER BY kind != 'summary', id",
        )?;
        let drafts = stmt
            .query_map([run_id], |row| {
                Ok(Draft {
                    kind: row.get(0)?,
                    path: row.get(1)?,
                    line: row.get(2)?,
                    start_line: row.get(3)?,
                    side: row.get(4)?,
                    severity: row.get(5)?,
                    body: row.get(6)?,
                    status: row.get(7)?,
                    unanchored: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(drafts)
    }

    pub fn run_counts(&self) -> Result<RunCounts> {
        Ok(self.conn.query_row(
            "SELECT
                 (SELECT count(*) FROM runs WHERE status = 'queued'),
                 (SELECT count(*) FROM runs WHERE status = 'running'),
                 (SELECT count(*) FROM drafts WHERE status = 'pending')",
            [],
            |row| {
                Ok(RunCounts {
                    queued: row.get(0)?,
                    running: row.get(1)?,
                    pending_drafts: row.get(2)?,
                })
            },
        )?)
    }

    /// The PR's title, description, author and threads as last polled, for a
    /// brief.
    pub fn pr_context(&self, key: &PrKey) -> Result<Option<PrContext>> {
        let repo = key.repo.to_string();
        let Some((title, body, url, author)) = self
            .conn
            .query_row(
                "SELECT title, body, url, author FROM prs WHERE repo = ?1 AND number = ?2",
                params![repo, key.number],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut threads_stmt = self.conn.prepare_cached(
            "SELECT thread_id, path, line, resolved FROM threads
             WHERE repo = ?1 AND number = ?2 ORDER BY rowid",
        )?;
        let mut comments_stmt = self.conn.prepare_cached(
            "SELECT id, author, body, created_at FROM comments
             WHERE repo = ?1 AND number = ?2 AND thread_id = ?3 ORDER BY created_at, rowid",
        )?;
        let mut threads: Vec<Thread> = threads_stmt
            .query_map(params![repo, key.number], |row| {
                Ok(Thread {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    line: row.get(2)?,
                    resolved: row.get(3)?,
                    comments: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        for thread in &mut threads {
            thread.comments = comments_stmt
                .query_map(params![repo, key.number, thread.id], |row| {
                    Ok(Comment {
                        id: row.get(0)?,
                        author: row.get(1)?,
                        body: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
        }
        Ok(Some(PrContext {
            title,
            body,
            url,
            author,
            threads,
        }))
    }
}

type QueuedRow = (i64, String, u32, String, String, String, Option<String>);

fn queued_row(row: &Row<'_>) -> rusqlite::Result<QueuedRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

#[cfg(test)]
mod tests {
    use sanic_core::{
        pr::PrSnapshot,
        run::{Confidence, DraftComment, InlineComment, Severity, Side, Verdict},
    };

    use super::*;

    fn snapshot() -> PrSnapshot {
        PrSnapshot {
            key: PrKey {
                repo: RepoName::new("org", "repo"),
                number: 7,
            },
            title: "Add thing".into(),
            body: "Adds the thing.\n\nFixes #3.".into(),
            url: "https://github.com/org/repo/pull/7".into(),
            author: "alice".into(),
            head_sha: "h1".into(),
            base_sha: "b1".into(),
            is_draft: false,
            review_requested: true,
            requested_teams: vec![],
            reviews: vec![],
            threads: vec![Thread {
                id: "t1".into(),
                path: Some("src/lib.rs".into()),
                line: Some(3),
                resolved: false,
                comments: vec![
                    Comment {
                        id: "c2".into(),
                        author: "alice".into(),
                        body: "because".into(),
                        created_at: "2026-01-02T00:00:00Z".into(),
                    },
                    Comment {
                        id: "c1".into(),
                        author: "bob".into(),
                        body: "why?".into(),
                        created_at: "2026-01-01T00:00:00Z".into(),
                    },
                ],
            }],
            files: None,
        }
    }

    fn store() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        store.record(&snapshot(), "default", &[]).unwrap();
        store
    }

    fn request(head: &str) -> ReviewRequest {
        ReviewRequest {
            key: snapshot().key,
            profile: "default".into(),
            head_sha: head.into(),
            base_sha: "b1".into(),
            trigger: ReviewTrigger::Requested,
        }
    }

    fn status(store: &Store, id: i64) -> String {
        store.run(id).unwrap().unwrap().status
    }

    fn result() -> ReviewResult {
        ReviewResult {
            summary: "Looks reasonable.".into(),
            verdict: Verdict::Comment,
            comments: vec![DraftComment {
                comment: InlineComment {
                    path: "src/lib.rs".into(),
                    line: 4,
                    start_line: Some(2),
                    side: Side::Right,
                    body: "off by one?".into(),
                    severity: Severity::Major,
                    confidence: Confidence::Medium,
                },
                unanchored: true,
            }],
            session_id: Some("sess".into()),
            transcript_path: "/data/runs/1/transcript.jsonl".into(),
        }
    }

    #[test]
    fn a_head_is_reviewed_once() {
        let mut store = store();
        let run = store.queue_review(&request("h1")).unwrap().unwrap();
        assert_eq!(store.queue_review(&request("h1")).unwrap(), None);
        assert!(store.claim_run(run.id).unwrap());
        assert_eq!(store.queue_review(&request("h1")).unwrap(), None);
        store.finish_review(run.id, &result()).unwrap();
        assert_eq!(store.queue_review(&request("h1")).unwrap(), None);
    }

    /// The poller writes on its own connection while the scheduler queues.
    #[test]
    fn queueing_waits_for_another_connections_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        store.record(&snapshot(), "default", &[]).unwrap();
        let other = rusqlite::Connection::open(&path).unwrap();
        other
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO poll_state VALUES ('k', 'v');")
            .unwrap();
        let commit = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            other.execute_batch("COMMIT").unwrap();
        });
        assert!(store.queue_review(&request("h1")).unwrap().is_some());
        commit.join().unwrap();
    }

    #[test]
    fn a_newer_head_supersedes_a_queued_run() {
        let mut store = store();
        let old = store.queue_review(&request("h1")).unwrap().unwrap();
        let new = store.queue_review(&request("h2")).unwrap().unwrap();
        assert_eq!(status(&store, old.id), "superseded");
        assert!(!store.claim_run(old.id).unwrap());
        assert!(store.claim_run(new.id).unwrap());

        // Returning to a superseded head queues it again under the same id.
        let again = store.queue_review(&request("h1")).unwrap().unwrap();
        assert_eq!(again.id, old.id);
        assert_eq!(status(&store, again.id), "queued");
        // A running run isn't superseded.
        assert_eq!(status(&store, new.id), "running");
    }

    #[test]
    fn failed_runs_are_retried() {
        let mut store = store();
        let run = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(run.id).unwrap();
        store.fail_run(run.id, "boom").unwrap();
        assert_eq!(
            store.run(run.id).unwrap().unwrap().error.as_deref(),
            Some("boom")
        );
        let retry = store.queue_review(&request("h1")).unwrap().unwrap();
        assert_eq!(retry.id, run.id);
        assert_eq!(store.run(run.id).unwrap().unwrap().error, None);
    }

    #[test]
    fn finished_reviews_store_summary_then_comments() {
        let mut store = store();
        let run = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(run.id).unwrap();
        store.finish_review(run.id, &result()).unwrap();

        let record = store.run(run.id).unwrap().unwrap();
        assert_eq!(record.status, "succeeded");
        assert_eq!(record.suggested_verdict.as_deref(), Some("comment"));
        assert_eq!(record.session_id.as_deref(), Some("sess"));

        let drafts = store.drafts(run.id).unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].kind, "summary");
        assert_eq!(drafts[0].body, "Looks reasonable.");
        assert_eq!(drafts[1].kind, "comment");
        assert_eq!(drafts[1].start_line, Some(2));
        assert_eq!(drafts[1].side.as_deref(), Some("RIGHT"));
        assert!(drafts[1].unanchored);
        assert_eq!(
            store.run_counts().unwrap(),
            RunCounts {
                queued: 0,
                running: 0,
                pending_drafts: 2
            }
        );
    }

    #[test]
    fn interrupted_runs_are_requeued() {
        let mut store = store();
        let running = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(running.id).unwrap();
        let mut push = request("h2");
        push.trigger = ReviewTrigger::Push {
            from_sha: "h1".into(),
        };
        let queued = store.queue_review(&push).unwrap().unwrap();
        assert_eq!(store.recover_runs().unwrap(), [running, queued]);
        assert_eq!(store.run_counts().unwrap().queued, 2);
    }

    #[test]
    fn context_has_threads_oldest_comment_first() {
        let store = store();
        let ctx = store.pr_context(&snapshot().key).unwrap().unwrap();
        assert_eq!(ctx.title, "Add thing");
        assert_eq!(ctx.body, "Adds the thing.\n\nFixes #3.");
        assert_eq!(ctx.threads.len(), 1);
        let ids: Vec<_> = ctx.threads[0].comments.iter().map(|c| &c.id).collect();
        assert_eq!(ids, ["c1", "c2"]);
    }
}
