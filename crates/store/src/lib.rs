//! SQLite schema, migrations and queries.

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
    trigger::{Known, Trigger},
};

/// Applied in order; a database's `user_version` is how many have run.
/// Never edit a released migration, only append.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_initial.sql"),
    include_str!("migrations/0002_runs_drafts.sql"),
    include_str!("migrations/0003_pr_body.sql"),
];

/// How long a write waits for another connection's write to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

pub use overview::{Activity, ActivityKind, MyPr, OwedReview, ReviewState};
pub use runs::{Draft, RunCounts, RunRecord};

/// A connection to the database. The poller and the runner each open their
/// own, so file databases use WAL and a busy timeout.
pub struct Store {
    conn: Connection,
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
                "SELECT head_sha, review_requested FROM prs WHERE repo = ?1 AND number = ?2",
                params![repo, key.number],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()?;
        let Some((head_sha, review_requested)) = row else {
            return Ok(None);
        };
        Ok(Some(Known {
            head_sha,
            review_requested,
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

    /// Stores `snapshot` as the new baseline for its PR and logs `triggers`,
    /// atomically.
    pub fn record(
        &mut self,
        snapshot: &PrSnapshot,
        profile: &str,
        triggers: &[Trigger],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_snapshot(&tx, snapshot, profile)?;
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

fn write_snapshot(tx: &Transaction<'_>, snap: &PrSnapshot, profile: &str) -> Result<()> {
    let repo = snap.key.repo.to_string();
    let number = snap.key.number;
    tx.execute(
        &format!(
            "INSERT INTO prs (repo, number, title, url, author, head_sha, base_sha, is_draft,
                              review_requested, profile, body, first_seen_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, {NOW}, {NOW})
             ON CONFLICT (repo, number) DO UPDATE SET
                 title = excluded.title, body = excluded.body, url = excluded.url,
                 author = excluded.author,
                 head_sha = excluded.head_sha, base_sha = excluded.base_sha,
                 is_draft = excluded.is_draft, review_requested = excluded.review_requested,
                 profile = excluded.profile, updated_at = excluded.updated_at"
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
            profile,
            snap.body
        ],
    )?;
    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO revisions (repo, number, head_sha, base_sha, seen_at)
             VALUES (?1, ?2, ?3, ?4, {NOW})"
        ),
        params![repo, number, snap.head_sha, snap.base_sha],
    )?;
    for thread in &snap.threads {
        tx.execute(
            "INSERT INTO threads (repo, number, thread_id, path, line, resolved)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (repo, number, thread_id) DO UPDATE SET
                 path = excluded.path, line = excluded.line, resolved = excluded.resolved",
            params![
                repo,
                number,
                thread.id,
                thread.path,
                thread.line,
                thread.resolved
            ],
        )?;
        for c in &thread.comments {
            tx.execute(
                "INSERT INTO comments (id, repo, number, thread_id, author, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
                params![
                    c.id,
                    repo,
                    number,
                    thread.id,
                    c.author,
                    c.body,
                    c.created_at
                ],
            )?;
        }
    }
    for r in &snap.reviews {
        tx.execute(
            "INSERT INTO reviews (id, repo, number, author, state, body, submitted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (id) DO UPDATE SET state = excluded.state, body = excluded.body",
            params![
                r.id,
                repo,
                number,
                r.author,
                r.state.as_str(),
                r.body,
                r.submitted_at
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sanic_core::{
        pr::{Comment, Review, ReviewState, Thread},
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
            }],
            threads: vec![Thread {
                id: "t1".into(),
                path: Some("src/lib.rs".into()),
                line: Some(3),
                resolved: false,
                comments: vec![Comment {
                    id: "c1".into(),
                    author: "bob".into(),
                    body: "why?".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                }],
            }],
            files: None,
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
        store.record(&snap, "default", &[]).unwrap();
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
        store.record(&snap, "default", &[]).unwrap();
        snap.head_sha = "h2".into();
        snap.review_requested = false;
        snap.threads[0].comments[0].body = "edited".into();
        store.record(&snap, "default", &[]).unwrap();

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
        store.record(&snap, "default", &[trigger]).unwrap();
        let events = store.events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].key, snap.key);
        assert_eq!(events[0].kind, "review_requested");
        assert_eq!(events[0].detail["head_sha"], "h1");
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
