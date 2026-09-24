//! SQLite schema, migrations and queries.

mod dashboard;
mod overview;
mod runs;

use std::{collections::HashSet, path::Path, time::Duration};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use sanic_core::{
    pr::{PrKey, PrSnapshot},
    run::Side,
    state::{PrState, ReviewFact, StateFacts},
    trigger::{Known, Trigger},
};

/// Applied in order; a database's `user_version` is how many have run.
/// Never edit a released migration, only append.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_initial.sql"),
    include_str!("migrations/0002_runs_drafts.sql"),
    include_str!("migrations/0003_pr_body.sql"),
    include_str!("migrations/0004_pr_open.sql"),
    include_str!("migrations/0005_pr_archived.sql"),
    include_str!("migrations/0006_pr_github_updated_at.sql"),
    include_str!("migrations/0007_start_requests.sql"),
    include_str!("migrations/0008_views.sql"),
    include_str!("migrations/0009_closed_prs.sql"),
    include_str!("migrations/0010_review_commit.sql"),
    include_str!("migrations/0011_pr_state.sql"),
    include_str!("migrations/0012_regenerate.sql"),
    include_str!("migrations/0013_draft_based_on.sql"),
    include_str!("migrations/0014_pr_reviewer.sql"),
    include_str!("migrations/0015_thread_placement.sql"),
];

/// How long a write waits for another connection's write to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

pub use dashboard::{DraftRow, DraftStatus, PrPage, ReviewRun};
pub use overview::{Activity, ActivityKind, LatestRun, MyPr, OwedReview};
pub use runs::{Draft, Refusal, Regeneration, RunCounts, RunRecord, SessionRun};

/// A connection to the database. The poller and the runner each open their
/// own, so file databases use WAL and a busy timeout.
pub struct Store {
    conn: Connection,
}

/// A tracked PR's fields that decide whether it's reviewed automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub profile: String,
    pub title: String,
    pub is_draft: bool,
    pub archived: bool,
    pub head_sha: String,
    /// See [`Store::head_reviewers`].
    pub head_reviewers: Vec<String>,
}

/// A logged trigger, as read back from the event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub key: PrKey,
    pub kind: String,
    pub detail: serde_json::Value,
}

impl Store {
    /// Opens (creating if needed) the database at `path` and migrates it.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .wrap_err_with(|| format!("creating data directory {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .wrap_err_with(|| format!("opening database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Self::init(conn).wrap_err_with(|| format!("initializing database {}", path.display()))
    }

    /// Opens an existing database that another connection has migrated,
    /// for reading only.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .wrap_err_with(|| format!("opening database {} read-only", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if usize::try_from(version)? != MIGRATIONS.len() {
            return Err(eyre!(
                "database {} is at schema version {version}, this build expects {}",
                path.display(),
                MIGRATIONS.len()
            ));
        }
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// What was stored for `key` on the previous poll, if it was tracked.
    pub fn known(&self, key: &PrKey) -> Result<Option<Known>> {
        let repo = key.repo.to_string();
        let row = self
            .conn
            .query_row(
                "SELECT head_sha, review_requested, is_draft FROM prs
                 WHERE repo = ?1 AND number = ?2",
                params![repo, key.number],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((head_sha, review_requested, is_draft)) = row else {
            return Ok(None);
        };
        Ok(Some(Known {
            head_sha,
            review_requested,
            is_draft,
            comment_ids: self.ids("comments", &repo, key.number)?,
            review_ids: self.ids("reviews", &repo, key.number)?,
        }))
    }

    fn ids(&self, table: &str, repo: &str, number: u32) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT id FROM {table} WHERE repo = ?1 AND number = ?2"
        ))?;
        let ids = stmt
            .query_map(params![repo, number], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Stores `snapshot` as the new baseline for its PR, as the user `me`
    /// sees it, and logs `triggers`, atomically.
    pub fn record(
        &mut self,
        snapshot: &PrSnapshot,
        me: &str,
        profile: &str,
        triggers: &[Trigger],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_snapshot(&tx, snapshot, me, profile)?;
        let repo = snapshot.key.repo.to_string();
        for trigger in triggers {
            tx.execute(
                &format!(
                    "INSERT INTO events (at, repo, number, kind, detail) VALUES ({NOW}, ?1, ?2, ?3, ?4)"
                ),
                params![
                    repo,
                    snapshot.key.number,
                    trigger.kind(),
                    serde_json::to_string(trigger)?
                ],
            )?;
        }
        tx.commit()
            .wrap_err_with(|| format!("recording {}", snapshot.key.url()))
    }

    /// Logged triggers, oldest first.
    pub fn events(&self) -> Result<Vec<Event>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT repo, number, kind, detail FROM events ORDER BY id")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(repo, number, kind, detail)| {
                Ok(Event {
                    key: PrKey {
                        repo: sanic_core::repo::RepoName::parse(&repo)?,
                        number,
                    },
                    kind,
                    detail: serde_json::from_str(&detail)?,
                })
            })
            .collect()
    }

    /// Remembers that a refresh at `checked_at` (as GitHub writes times)
    /// found `key` closed or not visible, tracked or not.
    pub fn remember_closed(&self, key: &PrKey, checked_at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO closed_prs (repo, number, checked_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (repo, number) DO UPDATE SET checked_at = excluded.checked_at",
            params![key.repo.to_string(), key.number, checked_at],
        )?;
        Ok(())
    }

    /// Forgets that `key` was found closed: a refresh found it open.
    pub fn forget_closed(&self, key: &PrKey) -> Result<()> {
        self.conn.execute(
            "DELETE FROM closed_prs WHERE repo = ?1 AND number = ?2",
            params![key.repo.to_string(), key.number],
        )?;
        Ok(())
    }

    /// When `key` was last found closed, unless it's been seen open since.
    pub fn closed_at(&self, key: &PrKey) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT checked_at FROM closed_prs WHERE repo = ?1 AND number = ?2",
                params![key.repo.to_string(), key.number],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Marks `key` as no longer open, if it's tracked. A later
    /// [`Store::record`] marks it open again.
    pub fn mark_closed(&self, key: &PrKey) -> Result<()> {
        self.conn.execute(
            "UPDATE prs SET open = 0 WHERE repo = ?1 AND number = ?2",
            params![key.repo.to_string(), key.number],
        )?;
        Ok(())
    }

    /// Marks every tracked PR not in `open` as no longer open: `open` is the
    /// complete set a reconcile found.
    pub fn keep_open(&mut self, open: &[PrKey]) -> Result<()> {
        let open: HashSet<(String, u32)> = open
            .iter()
            .map(|k| (k.repo.to_string(), k.number))
            .collect();
        let tx = self.conn.transaction()?;
        let tracked = tx
            .prepare("SELECT repo, number FROM prs WHERE open")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (repo, number) in tracked {
            if !open.contains(&(repo.clone(), number)) {
                tx.execute(
                    "UPDATE prs SET open = 0 WHERE repo = ?1 AND number = ?2",
                    params![repo, number],
                )?;
            }
        }
        tx.commit().wrap_err("marking PRs closed")
    }

    /// What decides whether `key` is reviewed automatically, as last
    /// polled. `None` if it isn't tracked.
    pub fn pr_summary(&self, key: &PrKey) -> Result<Option<PrSummary>> {
        let Some(mut summary) = self
            .conn
            .query_row(
                "SELECT profile, title, is_draft, archived, head_sha FROM prs
                 WHERE repo = ?1 AND number = ?2",
                params![key.repo.to_string(), key.number],
                |row| {
                    Ok(PrSummary {
                        profile: row.get(0)?,
                        title: row.get(1)?,
                        is_draft: row.get(2)?,
                        archived: row.get(3)?,
                        head_sha: row.get(4)?,
                        head_reviewers: Vec::new(),
                    })
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        summary.head_reviewers = self.head_reviewers(key, &summary.head_sha)?;
        Ok(Some(summary))
    }

    /// Who left a submitted review (approving, requesting changes or
    /// commenting) on `head`, oldest first and each once. Bots don't count.
    pub fn head_reviewers(&self, key: &PrKey, head: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT author FROM reviews
             WHERE repo = ?1 AND number = ?2 AND commit_sha = ?3 AND NOT by_bot
                   AND state IN ('APPROVED', 'CHANGES_REQUESTED', 'COMMENTED')
             -- One row per login, as `is_login` compares them.
             GROUP BY lower(author) ORDER BY min(submitted_at), min(rowid)",
        )?;
        let reviewers = stmt
            .query_map(params![key.repo.to_string(), key.number, head], |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(reviewers)
    }

    /// Archives or unarchives `key`. Archiving also supersedes every run of
    /// the PR that's queued but not started, whatever its kind: an archived
    /// PR is meant to be silent. A running one finishes. `false` if the PR
    /// isn't tracked.
    pub fn set_archived(&mut self, key: &PrKey, archived: bool) -> Result<bool> {
        let repo = key.repo.to_string();
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE prs SET archived = ?3 WHERE repo = ?1 AND number = ?2",
            params![repo, key.number, archived],
        )?;
        if archived {
            tx.execute(
                &format!(
                    "UPDATE runs SET status = 'superseded', finished_at = {NOW}
                     WHERE repo = ?1 AND number = ?2 AND status = 'queued'"
                ),
                params![repo, key.number],
            )?;
        }
        tx.commit()
            .wrap_err_with(|| format!("archiving {}", key.url()))?;
        Ok(changed == 1)
    }

    /// Where `key` stands, as last polled; see [`PrState`]. `mine` says
    /// it's your own PR.
    pub fn pr_state(&self, key: &PrKey, me: &str, mine: bool) -> Result<PrState> {
        let (decision, merge, checks): (Option<String>, Option<String>, Option<String>) =
            self.conn.query_row(
                "SELECT review_decision, merge_state, checks FROM prs
                 WHERE repo = ?1 AND number = ?2",
                params![key.repo.to_string(), key.number],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let threads = self.threads(key)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT author, state FROM reviews
             WHERE repo = ?1 AND number = ?2 AND NOT by_bot
             ORDER BY submitted_at, rowid",
        )?;
        let reviews = stmt
            .query_map(params![key.repo.to_string(), key.number], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let reviews: Vec<ReviewFact<'_>> = reviews
            .iter()
            .map(|(author, state)| ReviewFact { author, state })
            .collect();
        Ok(PrState::new(&StateFacts {
            review_decision: decision.as_deref(),
            merge_state: merge.as_deref(),
            checks: checks.as_deref(),
            threads: &threads,
            reviews: &reviews,
            me,
            mine,
        }))
    }

    pub fn tracked_prs(&self) -> Result<u32> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM prs", [], |row| row.get(0))?)
    }

    pub fn poll_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM poll_state WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_poll_state(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO poll_state (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let applied: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let applied = usize::try_from(applied)?;
    if applied > MIGRATIONS.len() {
        return Err(eyre!(
            "database schema version {applied} is newer than this build knows ({})",
            MIGRATIONS.len()
        ))
        .suggestion("run a newer sanic-review, or move the database aside");
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .wrap_err_with(|| format!("applying migration {}", i + 1))?;
        tx.pragma_update(None, "user_version", u32::try_from(i + 1)?)?;
        tx.commit()?;
    }
    Ok(())
}

fn write_snapshot(tx: &Transaction<'_>, snap: &PrSnapshot, me: &str, profile: &str) -> Result<()> {
    let repo = snap.key.repo.to_string();
    let number = snap.key.number;
    tx.execute(
        &format!(
            "INSERT INTO prs (repo, number, title, url, author, head_sha, base_sha, is_draft,
                              review_requested, reviewer, profile, body, open,
                              github_updated_at, review_decision, merge_state, checks,
                              first_seen_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1, ?13, ?14, ?15, ?16,
                     {NOW}, {NOW})
             ON CONFLICT (repo, number) DO UPDATE SET
                 title = excluded.title, body = excluded.body, url = excluded.url,
                 author = excluded.author,
                 head_sha = excluded.head_sha, base_sha = excluded.base_sha,
                 is_draft = excluded.is_draft, review_requested = excluded.review_requested,
                 reviewer = excluded.reviewer, profile = excluded.profile, open = 1,
                 github_updated_at = excluded.github_updated_at,
                 review_decision = excluded.review_decision,
                 merge_state = excluded.merge_state, checks = excluded.checks,
                 updated_at = excluded.updated_at"
        ),
        params![
            repo,
            number,
            snap.title,
            snap.url,
            snap.author,
            snap.head_sha,
            snap.base_sha,
            snap.is_draft,
            snap.review_requested,
            snap.is_reviewer(me),
            profile,
            snap.body,
            snap.updated_at,
            snap.review_decision,
            snap.merge_state,
            snap.checks
        ],
    )?;
    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO revisions (repo, number, head_sha, base_sha, seen_at)
             VALUES (?1, ?2, ?3, ?4, {NOW})"
        ),
        params![repo, number, snap.head_sha, snap.base_sha],
    )?;
    write_threads(tx, snap)?;
    Ok(())
}

/// `snap`'s threads, their comments and its reviews.
fn write_threads(tx: &Transaction<'_>, snap: &PrSnapshot) -> Result<()> {
    let repo = snap.key.repo.to_string();
    let number = snap.key.number;
    for thread in &snap.threads {
        let place = &thread.place;
        tx.execute(
            "INSERT INTO threads (repo, number, thread_id, path, line, resolved, start_line,
                                  side, head_sha, outdated, original_start_line,
                                  original_line, original_commit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT (repo, number, thread_id) DO UPDATE SET
                 path = excluded.path, line = excluded.line, resolved = excluded.resolved,
                 start_line = excluded.start_line, side = excluded.side,
                 head_sha = excluded.head_sha, outdated = excluded.outdated,
                 original_start_line = excluded.original_start_line,
                 original_line = excluded.original_line,
                 original_commit = excluded.original_commit",
            params![
                repo,
                number,
                thread.id,
                thread.path,
                thread.line,
                thread.resolved,
                place.start_line,
                place.side.map(Side::as_str),
                place.head,
                place.outdated,
                place.original_start_line,
                place.original_line,
                place.original_commit
            ],
        )?;
        for c in &thread.comments {
            tx.execute(
                "INSERT INTO comments (id, repo, number, thread_id, author, body, created_at,
                                       by_bot, reacted_at, url)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body,
                     by_bot = excluded.by_bot, reacted_at = excluded.reacted_at,
                     url = excluded.url",
                params![
                    c.id,
                    repo,
                    number,
                    thread.id,
                    c.author,
                    c.body,
                    c.created_at,
                    c.by_bot,
                    c.reacted_at,
                    c.url
                ],
            )?;
        }
    }
    for r in &snap.reviews {
        tx.execute(
            "INSERT INTO reviews (id, repo, number, author, state, body, submitted_at,
                                  commit_sha, by_bot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (id) DO UPDATE SET state = excluded.state, body = excluded.body,
                 commit_sha = excluded.commit_sha, by_bot = excluded.by_bot",
            params![
                r.id,
                repo,
                number,
                r.author,
                r.state.as_str(),
                r.body,
                r.submitted_at,
                r.commit,
                r.by_bot
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sanic_core::{
        pr::{Comment, Placement, Review, ReviewState, Thread},
        repo::RepoName,
    };

    use super::*;

    fn snapshot() -> PrSnapshot {
        PrSnapshot {
            key: PrKey {
                repo: RepoName::new("org", "repo"),
                number: 7,
            },
            title: "Add thing".into(),
            body: "Adds the thing.".into(),
            url: "https://github.com/org/repo/pull/7".into(),
            author: "alice".into(),
            head_sha: "h1".into(),
            base_sha: "b1".into(),
            is_draft: false,
            review_requested: true,
            requested_teams: vec![],
            reviews: vec![Review {
                id: "r1".into(),
                author: "bob".into(),
                state: ReviewState::Commented,
                body: "hm".into(),
                submitted_at: "2026-01-01T00:00:00Z".into(),
                commit: Some("h1".into()),
                by_bot: false,
            }],
            threads: vec![Thread {
                id: "t1".into(),
                path: Some("src/lib.rs".into()),
                line: Some(3),
                resolved: false,
                place: Placement::default(),
                comments: vec![Comment {
                    id: "c1".into(),
                    author: "bob".into(),
                    body: "why?".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    url: None,
                    by_bot: false,
                    reacted_at: None,
                }],
            }],
            files: None,
            updated_at: None,
            review_decision: None,
            merge_state: None,
            checks: None,
        }
    }

    #[test]
    fn unknown_pr_has_no_baseline() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.known(&snapshot().key).unwrap(), None);
    }

    #[test]
    fn recorded_snapshot_becomes_the_baseline() {
        let mut store = Store::open_in_memory().unwrap();
        let snap = snapshot();
        store.record(&snap, "me", "default", &[]).unwrap();
        let known = store.known(&snap.key).unwrap().unwrap();
        assert_eq!(known.head_sha, "h1");
        assert!(known.review_requested);
        assert_eq!(known.comment_ids, HashSet::from(["c1".into()]));
        assert_eq!(known.review_ids, HashSet::from(["r1".into()]));
    }

    #[test]
    fn recording_again_updates_in_place() {
        let mut store = Store::open_in_memory().unwrap();
        let mut snap = snapshot();
        store.record(&snap, "me", "default", &[]).unwrap();
        snap.head_sha = "h2".into();
        snap.review_requested = false;
        snap.threads[0].comments[0].body = "edited".into();
        store.record(&snap, "me", "default", &[]).unwrap();

        let known = store.known(&snap.key).unwrap().unwrap();
        assert_eq!(known.head_sha, "h2");
        assert!(!known.review_requested);
        assert_eq!(store.tracked_prs().unwrap(), 1);
    }

    #[test]
    fn triggers_are_logged_with_their_detail() {
        let mut store = Store::open_in_memory().unwrap();
        let snap = snapshot();
        let trigger = Trigger::ReviewRequested {
            head_sha: "h1".into(),
        };
        store.record(&snap, "me", "default", &[trigger]).unwrap();
        let events = store.events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].key, snap.key);
        assert_eq!(events[0].kind, "review_requested");
        assert_eq!(events[0].detail["head_sha"], "h1");
    }

    #[test]
    fn prs_close_and_reopen() {
        let mut store = Store::open_in_memory().unwrap();
        let snap = snapshot();
        let mut other = snapshot();
        other.key.number = 8;
        store.record(&snap, "me", "default", &[]).unwrap();
        store.record(&other, "me", "default", &[]).unwrap();
        let open = |store: &Store| -> Vec<u32> {
            store
                .conn
                .prepare("SELECT number FROM prs WHERE open ORDER BY number")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(open(&store), [7, 8]);

        store.keep_open(std::slice::from_ref(&other.key)).unwrap();
        assert_eq!(open(&store), [8]);
        store.mark_closed(&other.key).unwrap();
        assert_eq!(open(&store), Vec::<u32>::new());
        // Seen open again.
        store.record(&snap, "me", "default", &[]).unwrap();
        assert_eq!(open(&store), [7]);
    }

    #[test]
    fn summaries_are_as_last_polled() {
        let mut store = Store::open_in_memory().unwrap();
        let mut snap = snapshot();
        assert_eq!(store.pr_summary(&snap.key).unwrap(), None);
        store.record(&snap, "me", "default", &[]).unwrap();
        snap.title = "build(deps): bump".into();
        store.record(&snap, "me", "other", &[]).unwrap();
        assert_eq!(
            store.pr_summary(&snap.key).unwrap(),
            Some(PrSummary {
                profile: "other".into(),
                title: "build(deps): bump".into(),
                is_draft: false,
                archived: false,
                head_sha: "h1".into(),
                // The fixture's review is on the head.
                head_reviewers: vec!["bob".into()],
            })
        );
    }

    #[test]
    fn archiving_sticks_through_polls_and_supersedes_queued_reviews() {
        let mut store = Store::open_in_memory().unwrap();
        let snap = snapshot();
        let mut other = snapshot();
        other.key.number = 8;
        assert!(!store.set_archived(&snap.key, true).unwrap());
        store.record(&snap, "me", "default", &[]).unwrap();
        let run = store
            .queue_review(&store.review_request(&snap.key).unwrap().unwrap())
            .unwrap()
            .unwrap();

        assert!(store.set_archived(&snap.key, true).unwrap());
        assert_eq!(store.run(run.id).unwrap().unwrap().status, "superseded");
        store.record(&snap, "me", "default", &[]).unwrap();
        assert!(store.pr_summary(&snap.key).unwrap().unwrap().archived);
        assert!(store.set_archived(&snap.key, false).unwrap());
        assert!(!store.pr_summary(&snap.key).unwrap().unwrap().archived);
    }

    #[test]
    fn closed_prs_are_remembered_until_forgotten() {
        let store = Store::open_in_memory().unwrap();
        let snap = snapshot();
        assert_eq!(store.closed_at(&snap.key).unwrap(), None);
        store
            .remember_closed(&snap.key, "2026-09-01T00:00:00Z")
            .unwrap();
        store
            .remember_closed(&snap.key, "2026-09-02T00:00:00Z")
            .unwrap();
        assert_eq!(
            store.closed_at(&snap.key).unwrap().as_deref(),
            Some("2026-09-02T00:00:00Z")
        );
        store.forget_closed(&snap.key).unwrap();
        assert_eq!(store.closed_at(&snap.key).unwrap(), None);
    }

    #[test]
    fn head_reviewers_are_people_with_a_submitted_review_on_the_head() {
        let mut store = Store::open_in_memory().unwrap();
        let mut snap = snapshot();
        let review =
            |id: &str, author: &str, state: ReviewState, commit: &str, by_bot: bool| Review {
                id: id.into(),
                author: author.into(),
                state,
                body: String::new(),
                submitted_at: format!("2026-01-01T00:00:0{}Z", id.len()),
                commit: Some(commit.into()),
                by_bot,
            };
        snap.reviews = vec![
            review("r1", "carol", ReviewState::Approved, "h1", false),
            review("r22", "Bob", ReviewState::Commented, "h1", false),
            review("r333", "bob", ReviewState::ChangesRequested, "h1", false),
            review("r4", "dave", ReviewState::Approved, "old", false),
            review("r5", "erin", ReviewState::Pending, "h1", false),
            review("r6", "fred", ReviewState::Dismissed, "h1", false),
            review("r7", "renovate[bot]", ReviewState::Approved, "h1", true),
        ];
        store.record(&snap, "me", "default", &[]).unwrap();
        assert_eq!(
            store.head_reviewers(&snap.key, "h1").unwrap(),
            ["carol", "Bob"]
        );
        assert!(store.head_reviewers(&snap.key, "h2").unwrap().is_empty());
    }

    #[test]
    fn poll_state_round_trips() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.poll_state("k").unwrap(), None);
        store.set_poll_state("k", "a").unwrap();
        store.set_poll_state("k", "b").unwrap();
        assert_eq!(store.poll_state("k").unwrap().as_deref(), Some("b"));
    }
}
