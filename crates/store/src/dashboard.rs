//! Queries for the web dashboard: a PR's page, editing and deciding on
//! drafts, and which PRs you haven't looked at since their last review.

use std::collections::HashSet;

use color_eyre::eyre::{Result, WrapErr};
use rusqlite::{OptionalExtension, Row, params};
use sanic_core::{pr::PrKey, run::RunKind};

use crate::{NOW, Store, overview::key_columns};

const REVIEW: &str = RunKind::Review.as_str();
const REGENERATE: &str = RunKind::Regenerate.as_str();

/// A tracked PR, as last polled, for its dashboard page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrPage {
    pub key: PrKey,
    pub title: String,
    /// The PR description.
    pub body: String,
    /// The PR's page on GitHub, as GitHub gave it.
    pub url: String,
    pub author: String,
    pub head_sha: String,
    pub is_draft: bool,
    pub archived: bool,
    pub open: bool,
}

/// One review run of a PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRun {
    pub id: i64,
    pub status: String,
    pub error: Option<String>,
    pub suggested_verdict: Option<String>,
    /// The revision it reviewed; its comments are anchored to this commit.
    pub head_sha: String,
    /// When it was last queued, as the store writes timestamps.
    pub queued_at: String,
    pub finished_at: Option<String>,
    /// For a regeneration: the review it revises, and what you asked for.
    pub source_run: Option<i64>,
    pub instruction: Option<String>,
}

/// A stored draft with everything the dashboard shows and edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRow {
    pub id: i64,
    pub run_id: i64,
    /// The PR the draft's run belongs to.
    pub key: PrKey,
    pub kind: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub side: Option<String>,
    pub severity: Option<String>,
    pub confidence: Option<String>,
    pub original_body: String,
    /// `None` until you edit it, and again if an edit restores the
    /// original.
    pub edited_body: Option<String>,
    pub status: String,
    pub unanchored: bool,
    /// For a regenerated draft: the draft of the revised run it's based on,
    /// kept as it was if unchanged ("revised from #N" otherwise).
    pub based_on: Option<i64>,
}

impl DraftRow {
    /// The body as it stands: your edit, or the agent's original.
    #[must_use]
    pub fn body(&self) -> &str {
        self.edited_body.as_deref().unwrap_or(&self.original_body)
    }
}

/// What you can decide about a draft. `stale` and `posted` are set by
/// `serve`, never directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftStatus {
    Pending,
    Accepted,
    Rejected,
}

impl DraftStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

/// Statuses you can still edit and decide on.
const DECIDABLE: &str = "('pending', 'accepted', 'rejected')";

const DRAFT_COLUMNS: &str = "r.repo, r.number, d.id, d.run_id, d.kind, d.path, d.line,
    d.start_line, d.side, d.severity, d.confidence, d.original_body, d.edited_body,
    d.status, d.unanchored, d.based_on";

fn draft_row(row: &Row<'_>) -> rusqlite::Result<DraftRow> {
    Ok(DraftRow {
        key: key_columns(row)?,
        id: row.get(2)?,
        run_id: row.get(3)?,
        kind: row.get(4)?,
        path: row.get(5)?,
        line: row.get(6)?,
        start_line: row.get(7)?,
        side: row.get(8)?,
        severity: row.get(9)?,
        confidence: row.get(10)?,
        original_body: row.get(11)?,
        edited_body: row.get(12)?,
        status: row.get(13)?,
        unanchored: row.get(14)?,
        based_on: row.get(15)?,
    })
}

impl Store {
    /// `None` if `key` isn't tracked.
    pub fn pr_page(&self, key: &PrKey) -> Result<Option<PrPage>> {
        Ok(self
            .conn
            .query_row(
                "SELECT title, body, url, author, head_sha, is_draft, archived, open
                 FROM prs WHERE repo = ?1 AND number = ?2",
                params![key.repo.to_string(), key.number],
                |row| {
                    Ok(PrPage {
                        key: key.clone(),
                        title: row.get(0)?,
                        body: row.get(1)?,
                        url: row.get(2)?,
                        author: row.get(3)?,
                        head_sha: row.get(4)?,
                        is_draft: row.get(5)?,
                        archived: row.get(6)?,
                        open: row.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    /// `key`'s review runs, regenerations included, most recently queued
    /// first, as [`Store::owed_reviews`] orders them to pick the latest.
    pub fn review_runs(&self, key: &PrKey) -> Result<Vec<ReviewRun>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, status, error, suggested_verdict, head_sha, queued_at, finished_at,
                    source_run, instruction
             FROM runs
             WHERE repo = ?1 AND number = ?2 AND kind IN (?3, ?4)
             ORDER BY queued_at DESC, id DESC",
        )?;
        let runs = stmt
            .query_map(
                params![key.repo.to_string(), key.number, REVIEW, REGENERATE],
                |row| {
                    Ok(ReviewRun {
                        id: row.get(0)?,
                        status: row.get(1)?,
                        error: row.get(2)?,
                        suggested_verdict: row.get(3)?,
                        head_sha: row.get(4)?,
                        queued_at: row.get(5)?,
                        finished_at: row.get(6)?,
                        source_run: row.get(7)?,
                        instruction: row.get(8)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(runs)
    }

    /// A run's drafts, summary first, then as the agent wrote them.
    pub fn draft_rows(&self, run_id: i64) -> Result<Vec<DraftRow>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {DRAFT_COLUMNS} FROM drafts d JOIN runs r ON r.id = d.run_id
             WHERE d.run_id = ?1 ORDER BY d.kind != 'summary', d.id"
        ))?;
        let drafts = stmt
            .query_map([run_id], draft_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(drafts)
    }

    pub fn draft_row(&self, id: i64) -> Result<Option<DraftRow>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {DRAFT_COLUMNS} FROM drafts d JOIN runs r ON r.id = d.run_id
                     WHERE d.id = ?1"
                ),
                [id],
                draft_row,
            )
            .optional()?)
    }

    /// Replaces a draft's body with your edit. An edit back to the original
    /// clears it. `false` if the draft doesn't exist or can no longer be
    /// edited, e.g. because it was posted.
    pub fn edit_draft(&self, id: i64, body: &str) -> Result<bool> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE drafts SET edited_body = nullif(?2, original_body), updated_at = {NOW}
                 WHERE id = ?1 AND status IN {DECIDABLE}"
            ),
            params![id, body],
        )?;
        Ok(changed == 1)
    }

    /// Accepts or rejects a draft, or puts it back to pending. `false` if
    /// the draft doesn't exist or is stale or posted.
    pub fn set_draft_status(&self, id: i64, status: DraftStatus) -> Result<bool> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE drafts SET status = ?2, updated_at = {NOW}
                 WHERE id = ?1 AND status IN {DECIDABLE}"
            ),
            params![id, status.as_str()],
        )?;
        Ok(changed == 1)
    }

    /// Marks accepted drafts posted, once GitHub has them.
    pub fn mark_posted(&mut self, ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for id in ids {
            tx.execute(
                &format!(
                    "UPDATE drafts SET status = 'posted', updated_at = {NOW}
                     WHERE id = ?1 AND status = 'accepted'"
                ),
                [id],
            )?;
        }
        tx.commit().wrap_err("marking drafts posted")
    }

    /// Records that you opened `key`'s page just now. Does nothing if the
    /// PR isn't tracked.
    pub fn record_view(&self, key: &PrKey) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO views (repo, number, viewed_at)
                 SELECT repo, number, {NOW} FROM prs WHERE repo = ?1 AND number = ?2
                 ON CONFLICT (repo, number) DO UPDATE SET viewed_at = excluded.viewed_at"
            ),
            params![key.repo.to_string(), key.number],
        )?;
        Ok(())
    }

    /// PRs with a review that finished since you last opened their page,
    /// or that you've never opened.
    pub fn unseen(&self) -> Result<HashSet<PrKey>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT r.repo, r.number FROM runs r
             LEFT JOIN views v ON v.repo = r.repo AND v.number = r.number
             WHERE r.status = 'succeeded'
             GROUP BY r.repo, r.number
             HAVING max(r.finished_at) > coalesce(max(v.viewed_at), '')",
        )?;
        let unseen = stmt
            .query_map([], key_columns)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(unseen)
    }
}

#[cfg(test)]
mod tests {
    use sanic_core::{
        pr::PrSnapshot,
        repo::RepoName,
        run::{
            Confidence, DraftComment, InlineComment, ReviewRequest, ReviewResult, ReviewTrigger,
            Severity, Side, Verdict,
        },
    };

    use super::*;

    fn key() -> PrKey {
        PrKey {
            repo: RepoName::new("org", "repo"),
            number: 7,
        }
    }

    fn snapshot() -> PrSnapshot {
        PrSnapshot {
            key: key(),
            title: "Add thing".into(),
            body: "Adds the thing.".into(),
            url: "https://github.com/org/repo/pull/7".into(),
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
        }
    }

    fn result() -> ReviewResult {
        ReviewResult {
            summary: "Looks fine.".into(),
            verdict: Verdict::RequestChanges,
            comments: vec![DraftComment {
                comment: InlineComment {
                    path: "src/lib.rs".into(),
                    line: 4,
                    start_line: None,
                    side: Side::Right,
                    body: "off by one?".into(),
                    severity: Severity::Major,
                    confidence: Confidence::High,
                },
                unanchored: false,
            }],
            session_id: None,
            transcript_path: "t".into(),
        }
    }

    /// A store with one tracked PR and one finished review of it.
    fn reviewed() -> (Store, i64) {
        let mut store = Store::open_in_memory().unwrap();
        store.record(&snapshot(), "me", "default", &[]).unwrap();
        let run = store
            .queue_review(&ReviewRequest {
                key: key(),
                profile: "default".into(),
                head_sha: "h1".into(),
                base_sha: "b1".into(),
                trigger: ReviewTrigger::Requested,
            })
            .unwrap()
            .unwrap();
        store.claim_run(run.id).unwrap();
        store.finish_review(run.id, &result()).unwrap();
        (store, run.id)
    }

    #[test]
    fn a_pr_page_has_its_runs_and_drafts() {
        let (store, run) = reviewed();
        let page = store.pr_page(&key()).unwrap().unwrap();
        assert_eq!(page.title, "Add thing");
        assert_eq!(page.head_sha, "h1");
        let mut other = key();
        other.number = 8;
        assert_eq!(store.pr_page(&other).unwrap(), None);

        let runs = store.review_runs(&key()).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, run);
        assert_eq!(
            runs[0].suggested_verdict.as_deref(),
            Some("request_changes")
        );

        let drafts = store.draft_rows(run).unwrap();
        let kinds: Vec<_> = drafts.iter().map(|d| d.kind.as_str()).collect();
        assert_eq!(kinds, ["summary", "comment"]);
        assert_eq!(drafts[1].key, key());
        assert_eq!(drafts[1].confidence.as_deref(), Some("high"));
        assert_eq!(
            store.draft_row(drafts[1].id).unwrap().as_ref(),
            Some(&drafts[1])
        );
    }

    #[test]
    fn edits_keep_the_original_and_can_restore_it() {
        let (store, run) = reviewed();
        let id = store.draft_rows(run).unwrap()[1].id;
        assert!(store.edit_draft(id, "off by one").unwrap());
        let draft = store.draft_row(id).unwrap().unwrap();
        assert_eq!(draft.body(), "off by one");
        assert_eq!(draft.original_body, "off by one?");

        assert!(store.edit_draft(id, "off by one?").unwrap());
        assert_eq!(store.draft_row(id).unwrap().unwrap().edited_body, None);
        assert!(!store.edit_draft(9999, "x").unwrap());
    }

    #[test]
    fn posted_drafts_are_final() {
        let (mut store, run) = reviewed();
        let [summary, comment] = [0, 1].map(|i| store.draft_rows(run).unwrap()[i].id);
        assert!(
            store
                .set_draft_status(comment, DraftStatus::Accepted)
                .unwrap()
        );
        assert!(
            store
                .set_draft_status(summary, DraftStatus::Rejected)
                .unwrap()
        );
        store.mark_posted(&[summary, comment]).unwrap();

        let status = |id| store.draft_row(id).unwrap().unwrap().status;
        // Only accepted drafts are marked posted.
        assert_eq!(status(summary), "rejected");
        assert_eq!(status(comment), "posted");
        assert!(
            !store
                .set_draft_status(comment, DraftStatus::Pending)
                .unwrap()
        );
        assert!(!store.edit_draft(comment, "changed").unwrap());
        assert!(
            store
                .set_draft_status(summary, DraftStatus::Pending)
                .unwrap()
        );
    }

    #[test]
    fn a_pr_is_unseen_until_viewed_after_its_latest_review() {
        let (store, run) = reviewed();
        assert_eq!(store.unseen().unwrap(), HashSet::from([key()]));
        store.record_view(&key()).unwrap();
        assert!(store.unseen().unwrap().is_empty());

        // A later review makes it unseen again.
        store
            .conn
            .execute(
                "UPDATE runs SET finished_at = '9999-01-01T00:00:00.000Z' WHERE id = ?1",
                [run],
            )
            .unwrap();
        assert_eq!(store.unseen().unwrap(), HashSet::from([key()]));

        // Viewing an untracked PR records nothing.
        let mut other = key();
        other.number = 8;
        store.record_view(&other).unwrap();
    }
}
