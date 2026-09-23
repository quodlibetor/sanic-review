//! Queries for agent runs and their drafts.

use std::collections::HashSet;

use color_eyre::eyre::{Result, WrapErr};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use sanic_core::{
    pr::{Comment, PrKey, Thread},
    repo::RepoName,
    run::{
        BaselineDraft, Basis, PrContext, QueuedRun, ReviewRequest, ReviewResult, ReviewTrigger,
        Revision, RunKind,
    },
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
    /// For a `regenerate` run: the review it revises, the original one
    /// when it revises a regeneration.
    pub source_run: Option<i64>,
    /// For a `regenerate` run: what you asked for.
    pub instruction: Option<String>,
}

/// A run whose agent session can be resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRun {
    pub run: QueuedRun,
    pub session_id: String,
    pub status: String,
}

/// What [`Store::queue_regeneration`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Regeneration {
    Queued(QueuedRun),
    Refused(Refusal),
}

/// Why a review can't be regenerated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NoSuchRun,
    /// Its agent session is gone, or it never had one.
    NoSession,
    /// The PR moved on since: regenerating reviews the run's own head.
    HeadMoved {
        run_head: String,
        pr_head: String,
    },
    /// A regeneration of it is already queued or running.
    Underway {
        run: i64,
    },
    /// Its worktree, which the regeneration needs, is in use: most likely
    /// by a chat with its agent.
    WorktreeInUse,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let short = |sha: &str| sha.chars().take(8).collect::<String>();
        match self {
            Self::NoSuchRun => write!(f, "there's no such run"),
            Self::NoSession => write!(f, "the run has no agent session to resume"),
            Self::HeadMoved { run_head, pr_head } => write!(
                f,
                "the PR has moved from {} to {} since that review, and regenerating \
                 reviews the old head; start a fresh review instead",
                short(run_head),
                short(pr_head)
            ),
            Self::Underway { run } => write!(f, "run {run} is already regenerating it"),
            Self::WorktreeInUse => write!(
                f,
                "its worktree is in use, most likely by a chat with its agent; end that first"
            ),
        }
    }
}

/// A stored draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: i64,
    pub kind: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub side: Option<String>,
    pub severity: Option<String>,
    /// Your edit if you made one, else the agent's text.
    pub body: String,
    pub edited: bool,
    pub status: String,
    pub unanchored: bool,
    /// For a regenerated draft: the draft of the run it revised that it's
    /// based on.
    pub based_on: Option<i64>,
}

/// Counts for the terminal's summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunCounts {
    pub queued: u32,
    pub running: u32,
    pub pending_drafts: u32,
}

impl Store {
    /// Whether `req`'s head already has a queued, running or succeeded
    /// review, so [`Store::queue_review`] would skip it.
    pub fn has_review(&self, req: &ReviewRequest) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM runs
                 WHERE repo = ?1 AND number = ?2 AND kind = ?3 AND idem_key = ?4
                       AND status IN ('queued', 'running', 'succeeded')",
                params![
                    req.key.repo.to_string(),
                    req.key.number,
                    REVIEW,
                    req.idempotency_key()
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

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
            .wrap_err_with(|| format!("queueing a review of {}", req.key.url()))?;
        Ok(Some(QueuedRun {
            id,
            request: req.clone(),
            revision: None,
        }))
    }

    /// Queues a `regenerate` run revising run `source` with `instruction`,
    /// or says why not. It's for `source`'s head, which must still be the
    /// PR's, and it isn't held by `--manual-reviews`: it's started by hand.
    ///
    /// Revising a regeneration resumes its session, but the new run's
    /// source is the original review: every revision's session lives in
    /// that review's worktree path. `worktree_in_use` says whether that
    /// review's worktree exists.
    pub fn queue_regeneration(
        &mut self,
        source: i64,
        instruction: &str,
        worktree_in_use: impl FnOnce(i64) -> bool,
    ) -> Result<Regeneration> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                "SELECT r.repo, r.number, r.profile, r.head_sha, r.base_sha, r.session_id,
                        p.head_sha, coalesce(r.source_run, r.id)
                 FROM runs r JOIN prs p ON p.repo = r.repo AND p.number = r.number
                 WHERE r.id = ?1",
                [source],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((repo, number, profile, head_sha, base_sha, session_id, pr_head, original)) = row
        else {
            return Ok(Regeneration::Refused(Refusal::NoSuchRun));
        };
        // It checks out where the review ran, which a chat may be using.
        if worktree_in_use(original) {
            return Ok(Regeneration::Refused(Refusal::WorktreeInUse));
        }
        let Some(session_id) = session_id else {
            return Ok(Regeneration::Refused(Refusal::NoSession));
        };
        if pr_head != head_sha {
            return Ok(Regeneration::Refused(Refusal::HeadMoved {
                run_head: head_sha,
                pr_head,
            }));
        }
        let underway: Option<i64> = tx
            .query_row(
                "SELECT id FROM runs WHERE source_run = ?1 AND status IN ('queued', 'running')",
                [original],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(run) = underway {
            return Ok(Regeneration::Refused(Refusal::Underway { run }));
        }
        let earlier: u32 = tx.query_row(
            "SELECT count(*) FROM runs WHERE source_run = ?1",
            [original],
            |row| row.get(0),
        )?;
        tx.execute(
            &format!(
                "INSERT INTO runs (repo, number, kind, trigger, idem_key, profile, head_sha,
                                   base_sha, status, queued_at, source_run, instruction)
                 VALUES (?1, ?2, ?3, 'regenerate', ?4, ?5, ?6, ?7, 'queued', {NOW}, ?8, ?9)"
            ),
            params![
                repo,
                number,
                RunKind::Regenerate.as_str(),
                format!("{original}/{}", earlier + 1),
                profile,
                head_sha,
                base_sha,
                original,
                instruction
            ],
        )?;
        let id = tx.last_insert_rowid();
        let baseline = baseline(&tx, source)?;
        tx.commit()
            .wrap_err_with(|| format!("queueing a regeneration of run {source}"))?;
        Ok(Regeneration::Queued(QueuedRun {
            id,
            request: ReviewRequest {
                key: PrKey {
                    repo: RepoName::parse(&repo)?,
                    number,
                },
                profile,
                head_sha,
                base_sha,
                trigger: ReviewTrigger::Requested,
            },
            revision: Some(Revision {
                source_run: original,
                revises: source,
                session_id,
                instruction: instruction.to_owned(),
                baseline,
            }),
        }))
    }

    /// A full review of `key` at its head as last polled, for rerunning a
    /// review by hand. `None` if the PR isn't tracked.
    pub fn review_request(&self, key: &PrKey) -> Result<Option<ReviewRequest>> {
        Ok(self
            .conn
            .query_row(
                "SELECT profile, head_sha, base_sha FROM prs WHERE repo = ?1 AND number = ?2",
                params![key.repo.to_string(), key.number],
                |row| {
                    Ok(ReviewRequest {
                        key: key.clone(),
                        profile: row.get(0)?,
                        head_sha: row.get(1)?,
                        base_sha: row.get(2)?,
                        trigger: ReviewTrigger::Requested,
                    })
                },
            )
            .optional()?)
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
        self.finish(id, result, None)
    }

    /// Stores a finished regeneration's drafts, which start from those of
    /// run `revises` as `basis` says, and marks the run succeeded.
    pub fn finish_revision(
        &mut self,
        id: i64,
        result: &ReviewResult,
        revises: i64,
        basis: &Basis,
    ) -> Result<()> {
        self.finish(id, result, Some((revises, basis)))
    }

    fn finish(
        &mut self,
        id: i64,
        result: &ReviewResult,
        revision: Option<(i64, &Basis)>,
    ) -> Result<()> {
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
        let base = |based_on: Option<i64>| -> Result<Option<Base>> {
            match revision {
                Some((revises, _)) => Base::find(&tx, revises, based_on),
                None => Ok(None),
            }
        };
        // Base drafts already carried over whole, so a repeat is pending.
        let mut kept = HashSet::new();
        let none = (None, None, None, None);
        let summary_based_on = revision.and_then(|(_, basis)| basis.summary);
        let summary = Stored::new(
            "summary",
            &none,
            &result.summary,
            base(summary_based_on)?,
            &mut kept,
        );
        tx.execute(
            &format!(
                "INSERT INTO drafts (run_id, kind, original_body, edited_body, status, unanchored,
                                     based_on, created_at, updated_at)
                 VALUES (?1, 'summary', ?2, ?3, ?4, 0, ?5, {NOW}, {NOW})"
            ),
            params![
                id,
                summary.original_body,
                summary.edited_body,
                summary.status,
                summary.based_on
            ],
        )?;
        for (i, draft) in result.comments.iter().enumerate() {
            let c = &draft.comment;
            let based_on = revision.and_then(|(_, basis)| basis.comments.get(i).copied().flatten());
            let anchor = (
                Some(c.path.clone()),
                Some(c.line),
                c.start_line,
                Some(c.side.as_str().to_owned()),
            );
            let stored = Stored::new("comment", &anchor, &c.body, base(based_on)?, &mut kept);
            tx.execute(
                &format!(
                    "INSERT INTO drafts (run_id, kind, path, line, start_line, side, severity,
                                         confidence, original_body, edited_body, status,
                                         unanchored, based_on, created_at, updated_at)
                     VALUES (?1, 'comment', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
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
                    stored.original_body,
                    stored.edited_body,
                    stored.status,
                    draft.unanchored,
                    stored.based_on
                ],
            )?;
        }
        tx.commit()
            .wrap_err_with(|| format!("storing drafts for run {id}"))
    }

    /// Marks a run that was stopped for a newer head as superseded.
    pub fn supersede_run(&self, id: i64) -> Result<()> {
        self.conn.execute(
            &format!("UPDATE runs SET status = 'superseded', finished_at = {NOW} WHERE id = ?1"),
            [id],
        )?;
        Ok(())
    }

    /// Run `id`, if it has a session to resume.
    pub fn session_run(&self, id: i64) -> Result<Option<SessionRun>> {
        self.session_run_where("id = ?1", params![id])
    }

    /// `key`'s most recent run that has a session to resume.
    pub fn latest_session_run(&self, key: &PrKey) -> Result<Option<SessionRun>> {
        self.session_run_where(
            "repo = ?1 AND number = ?2",
            params![key.repo.to_string(), key.number],
        )
    }

    fn session_run_where(
        &self,
        filter: &str,
        values: impl rusqlite::Params,
    ) -> Result<Option<SessionRun>> {
        let row = self
            .conn
            .query_row(
                &format!(
                    "SELECT id, repo, number, profile, head_sha, base_sha, from_sha, session_id,
                            status, source_run, instruction
                     FROM runs WHERE {filter} AND session_id IS NOT NULL
                     ORDER BY coalesce(finished_at, queued_at) DESC, id DESC LIMIT 1"
                ),
                values,
                |row| {
                    Ok((
                        queued_row(row)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(queued, session_id, status, source_run, instruction)| {
            let mut run = queued_run(queued)?;
            // A regeneration's session lives where its source review ran.
            run.revision = source_run.map(|source_run| Revision {
                source_run,
                revises: run.id,
                session_id: session_id.clone(),
                instruction: instruction.unwrap_or_default(),
                baseline: Vec::new(),
            });
            Ok(SessionRun {
                run,
                session_id,
                status,
            })
        })
        .transpose()
    }

    /// PRs with a review or regeneration running, by repo and number.
    pub fn running_reviews(&self) -> Result<Vec<PrKey>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT repo, number FROM runs
             WHERE status = 'running' AND kind IN (?1, ?2) ORDER BY repo, number",
        )?;
        let rows = stmt
            .query_map([REVIEW, RunKind::Regenerate.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(repo, number)| {
                Ok(PrKey {
                    repo: RepoName::parse(&repo)?,
                    number,
                })
            })
            .collect()
    }

    /// Puts a run that was interrupted back in the queue, for the next
    /// start to run (or hold).
    pub fn requeue_run(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET status = 'queued', started_at = NULL, finished_at = NULL
             WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn fail_run(&self, id: i64, error: &str) -> Result<()> {
        self.end_run(id, "failed", error)
    }

    /// Records a run whose task panicked. Like a failed run, it's queued
    /// again the next time its key comes up.
    pub fn crash_run(&self, id: i64, panic: &str) -> Result<()> {
        self.end_run(id, "crashed", panic)
    }

    fn end_run(&self, id: i64, status: &str, error: &str) -> Result<()> {
        self.conn.execute(
            &format!("UPDATE runs SET status = ?2, error = ?3, finished_at = {NOW} WHERE id = ?1"),
            params![id, status, error],
        )?;
        Ok(())
    }

    /// Requeues runs a previous process left running, and returns every
    /// queued run, oldest first. Where a PR has both, only the most recently
    /// queued survives: a queued run can only sit beside a running one if it
    /// came later, so it's for the newer head, and the rest are superseded.
    pub fn recover_runs(&mut self) -> Result<Vec<QueuedRun>> {
        let tx = self.conn.transaction()?;
        tx.execute(
            &format!(
                "UPDATE runs SET status = 'superseded', finished_at = {NOW}
                 WHERE status IN ('queued', 'running') AND EXISTS (
                     SELECT 1 FROM runs AS newer
                     WHERE newer.repo = runs.repo AND newer.number = runs.number
                       AND newer.kind = runs.kind
                       AND newer.status IN ('queued', 'running')
                       AND (newer.queued_at, newer.id) > (runs.queued_at, runs.id))"
            ),
            [],
        )?;
        // A regeneration is started by hand, so one a previous process
        // didn't finish isn't started again unasked.
        tx.execute(
            &format!(
                "UPDATE runs SET status = 'failed', finished_at = {NOW},
                     error = 'interrupted when serve stopped; regenerate again'
                 WHERE kind = ?1 AND status IN ('queued', 'running')"
            ),
            [RunKind::Regenerate.as_str()],
        )?;
        tx.execute(
            "UPDATE runs SET status = 'queued', started_at = NULL WHERE status = 'running'",
            [],
        )?;
        tx.commit().wrap_err("recovering interrupted runs")?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, repo, number, profile, head_sha, base_sha, from_sha
             FROM runs WHERE status = 'queued' AND kind = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([REVIEW], queued_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(queued_run).collect()
    }

    /// `key`'s review that's queued but not started, if any.
    pub fn queued_review(&self, key: &PrKey) -> Result<Option<QueuedRun>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, repo, number, profile, head_sha, base_sha, from_sha
                 FROM runs WHERE status = 'queued' AND kind = ?1 AND repo = ?2 AND number = ?3
                 ORDER BY id DESC LIMIT 1",
                params![REVIEW, key.repo.to_string(), key.number],
                queued_row,
            )
            .optional()?;
        row.map(queued_run).transpose()
    }

    /// Asks the running `serve` to review `key` now; see
    /// [`Store::take_start_requests`].
    pub fn request_start(&self, key: &PrKey) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO start_requests (repo, number, requested_at) VALUES (?1, ?2, {NOW})"
            ),
            params![key.repo.to_string(), key.number],
        )?;
        Ok(())
    }

    /// Removes and returns the PRs `sanic-review review` asked to review
    /// now, oldest first.
    pub fn take_start_requests(&mut self) -> Result<Vec<PrKey>> {
        let tx = self.conn.transaction()?;
        let rows = tx
            .prepare("SELECT repo, number FROM start_requests ORDER BY id")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tx.execute("DELETE FROM start_requests", [])?;
        tx.commit().wrap_err("taking start requests")?;
        rows.into_iter()
            .map(|(repo, number)| {
                Ok(PrKey {
                    repo: RepoName::parse(&repo)?,
                    number,
                })
            })
            .collect()
    }

    pub fn run(&self, id: i64) -> Result<Option<RunRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT status, error, suggested_verdict, session_id, transcript_path,
                        source_run, instruction
                 FROM runs WHERE id = ?1",
                [id],
                |row| {
                    Ok(RunRecord {
                        status: row.get(0)?,
                        error: row.get(1)?,
                        suggested_verdict: row.get(2)?,
                        session_id: row.get(3)?,
                        transcript_path: row.get(4)?,
                        source_run: row.get(5)?,
                        instruction: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    /// A run's drafts, summary first.
    pub fn drafts(&self, run_id: i64) -> Result<Vec<Draft>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT kind, path, line, start_line, side, severity,
                    coalesce(edited_body, original_body), status, unanchored, id,
                    edited_body IS NOT NULL, based_on
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
                    id: row.get(9)?,
                    edited: row.get(10)?,
                    based_on: row.get(11)?,
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
        Ok(Some(PrContext {
            title,
            body,
            url,
            author,
            threads: self.threads(key)?,
        }))
    }

    /// `key`'s threads as last polled, each's comments oldest first.
    pub fn threads(&self, key: &PrKey) -> Result<Vec<Thread>> {
        let repo = key.repo.to_string();
        let mut threads_stmt = self.conn.prepare_cached(
            "SELECT thread_id, path, line, resolved FROM threads
             WHERE repo = ?1 AND number = ?2 ORDER BY rowid",
        )?;
        let mut comments_stmt = self.conn.prepare_cached(
            "SELECT id, author, body, created_at, by_bot, reacted_at FROM comments
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
                        by_bot: row.get(4)?,
                        reacted_at: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
        }
        Ok(threads)
    }
}

/// Run `run`'s drafts as a revision starts from them.
fn baseline(tx: &Transaction<'_>, run: i64) -> Result<Vec<BaselineDraft>> {
    let mut stmt = tx.prepare(
        "SELECT id, kind, path, line, start_line, side, coalesce(edited_body, original_body),
                status, edited_body IS NOT NULL
         FROM drafts WHERE run_id = ?1 ORDER BY kind != 'summary', id",
    )?;
    let drafts = stmt
        .query_map([run], |row| {
            Ok(BaselineDraft {
                id: row.get(0)?,
                kind: row.get(1)?,
                path: row.get(2)?,
                line: row.get(3)?,
                start_line: row.get(4)?,
                side: row.get(5)?,
                text: row.get(6)?,
                status: row.get(7)?,
                edited: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(drafts)
}

/// A baseline draft a revised one names, as stored.
struct Base {
    id: i64,
    kind: String,
    anchor: (Option<String>, Option<u32>, Option<u32>, Option<String>),
    original_body: String,
    edited_body: Option<String>,
    status: String,
}

impl Base {
    /// Run `revises`' draft `id`, if it is one.
    fn find(tx: &Transaction<'_>, revises: i64, id: Option<i64>) -> Result<Option<Self>> {
        let Some(id) = id else { return Ok(None) };
        Ok(tx
            .query_row(
                "SELECT id, kind, path, line, start_line, side, original_body, edited_body, status
                 FROM drafts WHERE id = ?1 AND run_id = ?2",
                params![id, revises],
                |row| {
                    Ok(Self {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        anchor: (row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?),
                        original_body: row.get(6)?,
                        edited_body: row.get(7)?,
                        status: row.get(8)?,
                    })
                },
            )
            .optional()?)
    }

    fn text(&self) -> &str {
        self.edited_body.as_deref().unwrap_or(&self.original_body)
    }
}

/// How a draft is stored: its text, your edit, its status and what it's
/// based on. A revised draft that's word for word its base, at the same
/// anchor, keeps the base's edit and status, unless an earlier draft
/// already kept them (`kept`); any other is pending.
struct Stored {
    original_body: String,
    edited_body: Option<String>,
    status: String,
    based_on: Option<i64>,
}

impl Stored {
    fn new(
        kind: &str,
        anchor: &(Option<String>, Option<u32>, Option<u32>, Option<String>),
        body: &str,
        base: Option<Base>,
        kept: &mut HashSet<i64>,
    ) -> Self {
        match base {
            Some(base)
                if base.kind == kind
                    && base.anchor == *anchor
                    && base.text() == body
                    && kept.insert(base.id) =>
            {
                Self {
                    original_body: base.original_body,
                    edited_body: base.edited_body,
                    status: base.status,
                    based_on: Some(base.id),
                }
            }
            // A summary isn't revised from a comment, or the other way.
            base => Self {
                original_body: body.to_owned(),
                edited_body: None,
                status: "pending".into(),
                based_on: base.filter(|b| b.kind == kind).map(|b| b.id),
            },
        }
    }
}

type QueuedRow = (i64, String, u32, String, String, String, Option<String>);

fn queued_run(
    (id, repo, number, profile, head_sha, base_sha, from_sha): QueuedRow,
) -> Result<QueuedRun> {
    Ok(QueuedRun {
        id,
        revision: None,
        request: ReviewRequest {
            key: PrKey {
                repo: RepoName::parse(&repo)?,
                number,
            },
            profile,
            head_sha,
            base_sha,
            trigger: from_sha.map_or(ReviewTrigger::Requested, |from_sha| ReviewTrigger::Push {
                from_sha,
            }),
        },
    })
}

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
                        by_bot: false,
                        reacted_at: None,
                    },
                    Comment {
                        id: "c1".into(),
                        author: "bob".into(),
                        body: "why?".into(),
                        created_at: "2026-01-01T00:00:00Z".into(),
                        by_bot: false,
                        reacted_at: None,
                    },
                ],
            }],
            files: None,
            updated_at: None,
            review_decision: None,
            merge_state: None,
            checks: None,
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
    fn a_head_has_a_review_while_queued_running_or_done() {
        let mut store = store();
        assert!(!store.has_review(&request("h1")).unwrap());
        let run = store.queue_review(&request("h1")).unwrap().unwrap();
        assert!(store.has_review(&request("h1")).unwrap());
        assert!(!store.has_review(&request("h2")).unwrap());
        store.claim_run(run.id).unwrap();
        store.fail_run(run.id, "boom").unwrap();
        assert!(!store.has_review(&request("h1")).unwrap());
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

        store.claim_run(run.id).unwrap();
        store.crash_run(run.id, "index out of bounds").unwrap();
        let record = store.run(run.id).unwrap().unwrap();
        assert_eq!(record.status, "crashed");
        assert_eq!(record.error.as_deref(), Some("index out of bounds"));
        let retry = store.queue_review(&request("h1")).unwrap().unwrap();
        assert_eq!(retry.id, run.id);
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
    fn a_rerun_is_a_full_review_of_the_polled_head() {
        let store = store();
        assert_eq!(
            store.review_request(&snapshot().key).unwrap(),
            Some(request("h1"))
        );
        let mut other = snapshot().key;
        other.number = 8;
        assert_eq!(store.review_request(&other).unwrap(), None);
    }

    #[test]
    fn a_prs_queued_review_can_be_found() {
        let mut store = store();
        assert_eq!(store.queued_review(&snapshot().key).unwrap(), None);
        let run = store.queue_review(&request("h1")).unwrap().unwrap();
        assert_eq!(
            store.queued_review(&snapshot().key).unwrap(),
            Some(run.clone())
        );
        store.claim_run(run.id).unwrap();
        assert_eq!(store.queued_review(&snapshot().key).unwrap(), None);
    }

    #[test]
    fn start_requests_are_taken_once() {
        let mut store = store();
        let key = snapshot().key;
        store.request_start(&key).unwrap();
        store.request_start(&key).unwrap();
        assert_eq!(store.take_start_requests().unwrap(), [key.clone(), key]);
        assert!(store.take_start_requests().unwrap().is_empty());
    }

    #[test]
    fn sessions_are_found_by_run_or_by_prs_latest() {
        let mut store = store();
        let key = snapshot().key;
        assert_eq!(store.latest_session_run(&key).unwrap(), None);
        let first = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(first.id).unwrap();
        store.finish_review(first.id, &result()).unwrap();
        // A newer run with no session yet doesn't hide the older one.
        let second = store.queue_review(&request("h2")).unwrap().unwrap();
        let found = store.latest_session_run(&key).unwrap().unwrap();
        assert_eq!(found.run, first);
        assert_eq!(found.session_id, "sess");
        assert_eq!(found.status, "succeeded");
        assert_eq!(store.session_run(first.id).unwrap().unwrap().run, first);
        assert_eq!(store.session_run(second.id).unwrap(), None);
    }

    #[test]
    fn a_review_is_regenerated_as_a_new_run_of_the_same_head() {
        let mut store = store();
        let source = store.queue_review(&request("h1")).unwrap().unwrap();
        // No session until it has run.
        assert_eq!(
            store.queue_regeneration(source.id, "x", |_| false).unwrap(),
            Regeneration::Refused(Refusal::NoSession)
        );
        store.claim_run(source.id).unwrap();
        store.finish_review(source.id, &result()).unwrap();

        let Regeneration::Queued(run) = store
            .queue_regeneration(source.id, "be terser", |_| false)
            .unwrap()
        else {
            panic!("refused");
        };
        assert_ne!(run.id, source.id);
        assert_eq!(run.request.head_sha, "h1");
        let revision = run.revision.clone().unwrap();
        assert_eq!(revision.source_run, source.id);
        assert_eq!(revision.revises, source.id);
        assert_eq!(revision.session_id, "sess");
        assert_eq!(revision.instruction, "be terser");
        // The baseline is the source's drafts as they stand.
        let kinds: Vec<&str> = revision.baseline.iter().map(|d| d.kind.as_str()).collect();
        assert_eq!(kinds, ["summary", "comment"]);
        assert_eq!(revision.baseline[1].text, "off by one?");
        assert_eq!(revision.baseline[1].status, "pending");
        let record = store.run(run.id).unwrap().unwrap();
        assert_eq!(record.source_run, Some(source.id));
        assert_eq!(record.instruction.as_deref(), Some("be terser"));
        // One at a time.
        assert_eq!(
            store
                .queue_regeneration(source.id, "again", |_| false)
                .unwrap(),
            Regeneration::Refused(Refusal::Underway { run: run.id })
        );
        store.claim_run(run.id).unwrap();
        store.fail_run(run.id, "x").unwrap();
        assert!(matches!(
            store
                .queue_regeneration(source.id, "again", |_| false)
                .unwrap(),
            Regeneration::Queued(_)
        ));

        // The PR moved on: regenerate reviews the old head, so it's refused.
        let mut moved = snapshot();
        moved.head_sha = "h2".into();
        store.record(&moved, "default", &[]).unwrap();
        let refused = store.queue_regeneration(source.id, "x", |_| false).unwrap();
        assert_eq!(
            refused,
            Regeneration::Refused(Refusal::HeadMoved {
                run_head: "h1".into(),
                pr_head: "h2".into(),
            })
        );
        let Regeneration::Refused(why) = refused else {
            unreachable!()
        };
        assert!(
            why.to_string().contains("start a fresh review instead"),
            "{why}"
        );
        assert_eq!(
            store.queue_regeneration(999, "x", |_| false).unwrap(),
            Regeneration::Refused(Refusal::NoSuchRun)
        );
    }

    #[test]
    fn a_regeneration_is_revised_in_its_reviews_worktree() {
        let mut store = store();
        let review = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(review.id).unwrap();
        store.finish_review(review.id, &result()).unwrap();
        let Regeneration::Queued(first) =
            store.queue_regeneration(review.id, "x", |_| false).unwrap()
        else {
            panic!("refused");
        };
        store.claim_run(first.id).unwrap();
        let revised = ReviewResult {
            session_id: Some("sess-2".into()),
            ..result()
        };
        store.finish_review(first.id, &revised).unwrap();
        // A chat with it resumes where the review ran.
        let session = store.session_run(first.id).unwrap().unwrap();
        assert_eq!(session.run.revision.unwrap().source_run, review.id);

        let mut checked = None;
        let Regeneration::Queued(second) = store
            .queue_regeneration(first.id, "y", |id| {
                checked = Some(id);
                false
            })
            .unwrap()
        else {
            panic!("refused");
        };
        assert_eq!(checked, Some(review.id));
        let revision = second.revision.unwrap();
        assert_eq!(revision.source_run, review.id);
        assert_eq!(revision.session_id, "sess-2");
        assert_eq!(
            store.queue_regeneration(review.id, "z", |_| true).unwrap(),
            Regeneration::Refused(Refusal::WorktreeInUse)
        );
    }

    /// An anchored comment on `src/lib.rs`.
    fn comment(body: &str, line: u32) -> DraftComment {
        DraftComment {
            comment: InlineComment {
                path: "src/lib.rs".into(),
                line,
                start_line: None,
                side: Side::Right,
                body: body.into(),
                severity: Severity::Minor,
                confidence: Confidence::High,
            },
            unanchored: false,
        }
    }

    #[test]
    fn a_revision_keeps_your_choices_on_drafts_it_leaves_alone() {
        use crate::DraftStatus;

        let mut store = store();
        let source = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(source.id).unwrap();
        let original = ReviewResult {
            summary: "S".into(),
            comments: vec![
                comment("A", 1),
                comment("B", 2),
                comment("C", 3),
                comment("D", 4),
            ],
            ..result()
        };
        store.finish_review(source.id, &original).unwrap();
        let ids: Vec<i64> = store
            .drafts(source.id)
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        let [summary, a, b, c, d] = ids[..] else {
            panic!("{ids:?}")
        };
        store.set_draft_status(a, DraftStatus::Accepted).unwrap();
        store.edit_draft(b, "B, as you put it").unwrap();
        store.set_draft_status(b, DraftStatus::Accepted).unwrap();
        store.set_draft_status(c, DraftStatus::Rejected).unwrap();

        let Regeneration::Queued(run) =
            store.queue_regeneration(source.id, "x", |_| false).unwrap()
        else {
            panic!("refused");
        };
        let revised = ReviewResult {
            summary: "S".into(),
            comments: vec![
                comment("A", 1),                // unchanged: still accepted
                comment("B, as you put it", 2), // your edit, unchanged
                comment("C", 3),                // rejected; shouldn't be back
                comment("D, reworded", 4),      // changed: pending, from d
                comment("A", 9),                // moved: pending, from a
                comment("E", 5),                // new
                comment("F", 6),                // names a draft it wasn't shown
                comment("A", 1),                // a repeat of a: pending
                comment("G", 7),                // names the summary: new
            ],
            ..result()
        };
        let basis = Basis {
            summary: Some(summary),
            comments: vec![
                Some(a),
                Some(b),
                Some(c),
                Some(d),
                Some(a),
                None,
                Some(9999),
                Some(a),
                Some(summary),
            ],
        };
        store
            .finish_revision(run.id, &revised, source.id, &basis)
            .unwrap();

        let got: Vec<(String, bool, String, Option<i64>)> = store
            .drafts(run.id)
            .unwrap()
            .into_iter()
            .map(|d| (d.body, d.edited, d.status, d.based_on))
            .collect();
        let row = |body: &str, edited: bool, status: &str, based_on: Option<i64>| {
            (body.to_owned(), edited, status.to_owned(), based_on)
        };
        assert_eq!(
            got,
            [
                row("S", false, "pending", Some(summary)),
                row("A", false, "accepted", Some(a)),
                row("B, as you put it", true, "accepted", Some(b)),
                row("C", false, "rejected", Some(c)),
                row("D, reworded", false, "pending", Some(d)),
                row("A", false, "pending", Some(a)),
                row("E", false, "pending", None),
                row("F", false, "pending", None),
                row("A", false, "pending", Some(a)),
                row("G", false, "pending", None),
            ]
        );
        // The source run's drafts are as you left them.
        let before: Vec<String> = store
            .drafts(source.id)
            .unwrap()
            .into_iter()
            .map(|d| d.status)
            .collect();
        assert_eq!(
            before,
            ["pending", "accepted", "accepted", "rejected", "pending"]
        );
    }

    #[test]
    fn unfinished_regenerations_fail_on_restart() {
        let mut store = store();
        let source = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(source.id).unwrap();
        store.finish_review(source.id, &result()).unwrap();
        let Regeneration::Queued(run) =
            store.queue_regeneration(source.id, "x", |_| false).unwrap()
        else {
            panic!("refused");
        };
        store.claim_run(run.id).unwrap();
        assert!(store.recover_runs().unwrap().is_empty());
        assert_eq!(status(&store, run.id), "failed");
    }

    #[test]
    fn a_cancelled_run_is_queued_again() {
        let mut store = store();
        let run = store.queue_review(&request("h1")).unwrap().unwrap();
        assert!(store.running_reviews().unwrap().is_empty());
        store.claim_run(run.id).unwrap();
        assert_eq!(store.running_reviews().unwrap(), [snapshot().key]);
        store.requeue_run(run.id).unwrap();
        assert_eq!(status(&store, run.id), "queued");
        assert_eq!(store.recover_runs().unwrap(), [run]);
    }

    #[test]
    fn interrupted_runs_are_requeued() {
        let mut store = store();
        let running = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(running.id).unwrap();
        assert_eq!(store.recover_runs().unwrap(), [running]);
        assert_eq!(store.run_counts().unwrap().queued, 1);
    }

    #[test]
    fn an_interrupted_run_on_an_older_head_is_superseded() {
        let mut store = store();
        let running = store.queue_review(&request("h1")).unwrap().unwrap();
        store.claim_run(running.id).unwrap();
        let mut push = request("h2");
        push.trigger = ReviewTrigger::Push {
            from_sha: "h1".into(),
        };
        let queued = store.queue_review(&push).unwrap().unwrap();
        assert_eq!(store.recover_runs().unwrap(), [queued]);
        assert_eq!(status(&store, running.id), "superseded");
        assert_eq!(store.run_counts().unwrap().queued, 1);
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
