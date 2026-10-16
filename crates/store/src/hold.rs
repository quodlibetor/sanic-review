//! The `.claude` hold: what each PR's changes reach under `.claude`, and
//! how far each check of a head on a base has got. See
//! [`sanic_core::hold`].

use color_eyre::eyre::Result;
use rusqlite::{OptionalExtension, Transaction, params};
use sanic_core::{
    hold::{ClaudeFinding, ClaudeHold, Finding, HeadCheck},
    pr::{PrKey, PrSnapshot, is_login},
};

use crate::{NOW, Store, overview::key_columns};

const DONE: &str = "done";
const FAILED: &str = "failed";

/// SQL that's true when an automatic review of the PR in `repo` and
/// `number`, at `head` on `base`, SQL for their columns, is held for
/// `.claude`: the PR's changes reach it, or that check isn't done.
pub(crate) fn claude_held(repo: &str, number: &str, base: &str, head: &str) -> String {
    format!(
        "(EXISTS (SELECT 1 FROM claude_holds h WHERE h.repo = {repo} AND h.number = {number})
          OR NOT EXISTS (SELECT 1 FROM claude_checks c
                         WHERE c.repo = {repo} AND c.number = {number}
                               AND c.base_sha = {base} AND c.head_sha = {head}
                               AND c.state = '{DONE}'))"
    )
}

/// Marks `snap`'s head on its base checked when it's `me`'s own PR, which
/// the hold leaves alone: it's only for reviews of other people's.
pub(crate) fn skip_own(tx: &Transaction<'_>, snap: &PrSnapshot, me: &str) -> Result<()> {
    if is_login(&snap.author, me) {
        set_check(tx, &snap.key, &snap.base_sha, &snap.head_sha, DONE, None)?;
    }
    Ok(())
}

impl Store {
    /// The check of `key`'s `head` on `base`; [`HeadCheck::Pending`] for
    /// one not made yet.
    pub fn head_check(&self, key: &PrKey, base: &str, head: &str) -> Result<HeadCheck> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT state, error FROM claude_checks
                 WHERE repo = ?1 AND number = ?2 AND base_sha = ?3 AND head_sha = ?4",
                params![key.repo.to_string(), key.number, base, head],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((state, _)) if state == DONE => HeadCheck::Done,
            Some((_, error)) => HeadCheck::Failed(error.unwrap_or_default()),
            None => HeadCheck::Pending,
        })
    }

    /// Records the check of `key`'s `head` on `base`: what of its changes
    /// reaches `.claude`, which finishes it, or why it failed.
    pub fn record_check(
        &mut self,
        key: &PrKey,
        base: &str,
        head: &str,
        check: Result<&[Finding], &str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        match check {
            Ok(findings) => {
                insert_findings(&tx, key, head, findings)?;
                set_check(&tx, key, base, head, DONE, None)?;
            }
            Err(why) => set_check(&tx, key, base, head, FAILED, Some(why))?,
        }
        tx.commit()?;
        Ok(())
    }

    /// Why an automatic review of `key` at `head` on `base` is held for
    /// `.claude`, if it is.
    pub fn claude_holds_run(
        &self,
        key: &PrKey,
        base: &str,
        head: &str,
    ) -> Result<Option<&'static str>> {
        if self.has_claude_findings(key)? {
            return Ok(Some("its changes reach `.claude`"));
        }
        Ok(match self.head_check(key, base, head)? {
            HeadCheck::Done => None,
            HeadCheck::Pending => Some("its head isn't checked for `.claude` changes yet"),
            HeadCheck::Failed(_) => Some("checking its head for `.claude` changes failed"),
        })
    }

    /// Whether `key`'s changes reach `.claude`, which holds its reviews
    /// whatever their head.
    pub fn has_claude_findings(&self, key: &PrKey) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM claude_holds WHERE repo = ?1 AND number = ?2 LIMIT 1",
                params![key.repo.to_string(), key.number],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Why `key`'s reviews are held for `.claude`, if they are: what its
    /// changes reach, and the check of its queued review's head, if it
    /// has one queued. `None` with nothing found and no check to wait on.
    pub fn claude_hold(&self, key: &PrKey) -> Result<Option<ClaudeHold>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT source_repo, source_number, reason, path, link, target, head_sha
             FROM claude_holds WHERE repo = ?1 AND number = ?2
             ORDER BY seen_at, rowid",
        )?;
        let findings = stmt
            .query_map(params![key.repo.to_string(), key.number], |row| {
                let reason: String = row.get(2)?;
                let path: String = row.get(3)?;
                let finding = if reason == "link" {
                    Finding::Link {
                        path,
                        link: row.get(4)?,
                        target: row.get(5)?,
                    }
                } else {
                    Finding::Path(path)
                };
                Ok(ClaudeFinding {
                    finding,
                    source: key_columns(row)?,
                    head_sha: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let head = match self.queued_review(key)? {
            Some(run) => self.head_check(key, &run.request.base_sha, &run.request.head_sha)?,
            None => HeadCheck::Done,
        };
        let hold = ClaudeHold { findings, head };
        Ok((!hold.findings.is_empty() || hold.head != HeadCheck::Done).then_some(hold))
    }
}

/// Adds `findings`, `key`'s own, first shown by `head`; ones seen before
/// keep the head that first showed them.
fn insert_findings(
    tx: &Transaction<'_>,
    key: &PrKey,
    head: &str,
    findings: &[Finding],
) -> Result<()> {
    let repo = key.repo.to_string();
    for finding in findings {
        let (reason, path, link, target) = match finding {
            Finding::Path(path) => ("path", path.as_str(), "", ""),
            Finding::Link { path, link, target } => {
                ("link", path.as_str(), link.as_str(), target.as_str())
            }
        };
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO claude_holds
                     (repo, number, source_repo, source_number, reason, path, link, target,
                      head_sha, seen_at)
                 VALUES (?1, ?2, ?1, ?2, ?3, ?4, ?5, ?6, ?7, {NOW})"
            ),
            params![repo, key.number, reason, path, link, target, head],
        )?;
    }
    Ok(())
}

#[cfg(test)]
impl Store {
    /// Marks every head recorded so far checked on its base with nothing
    /// found, for tests of what the hold doesn't concern.
    pub(crate) fn check_all_heads(&self) {
        self.conn
            .execute(
                &format!(
                    "INSERT OR REPLACE INTO claude_checks (repo, number, base_sha, head_sha, state)
                     SELECT repo, number, base_sha, head_sha, '{DONE}' FROM revisions"
                ),
                [],
            )
            .unwrap();
    }
}

fn set_check(
    tx: &Transaction<'_>,
    key: &PrKey,
    base: &str,
    head: &str,
    state: &str,
    error: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO claude_checks (repo, number, base_sha, head_sha, state, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key.repo.to_string(), key.number, base, head, state, error],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sanic_core::{
        repo::RepoName,
        run::{ReviewRequest, ReviewTrigger},
    };

    use super::*;

    fn snapshot(head: &str, author: &str) -> PrSnapshot {
        PrSnapshot {
            key: PrKey {
                repo: RepoName::new("org", "repo"),
                number: 7,
            },
            title: "t".into(),
            body: String::new(),
            url: "https://github.com/org/repo/pull/7".into(),
            author: author.into(),
            head_sha: head.into(),
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
            in_progress: None,
        }
    }

    fn path(p: &str) -> Finding {
        Finding::Path(p.into())
    }

    /// Records `head` and checks it on `b1`, finding `found`.
    fn checked(store: &mut Store, head: &str, found: &[Finding]) {
        let key = snapshot(head, "alice").key;
        store
            .record(&snapshot(head, "alice"), "me", "p", &[])
            .unwrap();
        store.record_check(&key, "b1", head, Ok(found)).unwrap();
    }

    #[test]
    fn a_hold_sticks_through_a_push_that_reverts_it() {
        let mut store = Store::open_in_memory().unwrap();
        let key = snapshot("h1", "alice").key;
        checked(&mut store, "h1", &[]);
        assert_eq!(store.claude_hold(&key).unwrap(), None);
        assert_eq!(store.claude_holds_run(&key, "b1", "h1").unwrap(), None);

        checked(&mut store, "h2", &[path(".claude/skills/x/SKILL.md")]);
        checked(&mut store, "h3", &[]);
        let hold = store.claude_hold(&key).unwrap().expect("held");
        assert_eq!(
            hold.findings,
            [ClaudeFinding {
                finding: path(".claude/skills/x/SKILL.md"),
                source: key.clone(),
                head_sha: "h2".into(),
            }]
        );
        assert_eq!(hold.head, HeadCheck::Done);
        assert!(store.claude_holds_run(&key, "b1", "h3").unwrap().is_some());

        // Seen again later, a path keeps the head that first showed it.
        checked(&mut store, "h4", &[path(".claude/skills/x/SKILL.md")]);
        assert_eq!(
            store.claude_hold(&key).unwrap().unwrap().findings[0].head_sha,
            "h2"
        );
    }

    #[test]
    fn a_check_is_of_a_head_on_a_base_and_a_failure_is_its_own() {
        let mut store = Store::open_in_memory().unwrap();
        let key = snapshot("h1", "alice").key;
        store
            .record(&snapshot("h1", "alice"), "me", "p", &[])
            .unwrap();
        assert!(store.claude_holds_run(&key, "b1", "h1").unwrap().is_some());
        store
            .record_check(&key, "b1", "h1", Err("no mirror"))
            .unwrap();
        assert_eq!(
            store.head_check(&key, "b1", "h1").unwrap(),
            HeadCheck::Failed("no mirror".into())
        );
        store.record_check(&key, "b1", "h1", Ok(&[])).unwrap();
        assert_eq!(store.head_check(&key, "b1", "h1").unwrap(), HeadCheck::Done);
        // A retarget, or a moved base, is checked again.
        assert_eq!(
            store.head_check(&key, "b2", "h1").unwrap(),
            HeadCheck::Pending
        );
        assert!(store.claude_holds_run(&key, "b2", "h1").unwrap().is_some());
    }

    #[test]
    fn only_a_queued_review_shows_its_check() {
        let mut store = Store::open_in_memory().unwrap();
        let key = snapshot("h1", "alice").key;
        store
            .record(&snapshot("h1", "alice"), "me", "p", &[])
            .unwrap();
        // Nothing queued: nothing to wait on, and nothing found.
        assert_eq!(store.claude_hold(&key).unwrap(), None);
        store
            .queue_review(&ReviewRequest {
                key: key.clone(),
                author: "alice".into(),
                profile: "p".into(),
                head_sha: "h1".into(),
                base_sha: "b1".into(),
                trigger: ReviewTrigger::Requested,
            })
            .unwrap()
            .unwrap();
        let waiting = store.claude_hold(&key).unwrap().expect("waits");
        assert_eq!(waiting.head, HeadCheck::Pending);
        assert!(!waiting.warns());
        store.record_check(&key, "b1", "h1", Err("boom")).unwrap();
        assert!(store.claude_hold(&key).unwrap().unwrap().warns());
    }

    #[test]
    fn your_own_prs_are_never_held() {
        let mut store = Store::open_in_memory().unwrap();
        let key = snapshot("h1", "Me").key;
        store.record(&snapshot("h1", "Me"), "me", "p", &[]).unwrap();
        assert_eq!(store.claude_holds_run(&key, "b1", "h1").unwrap(), None);
    }
}
