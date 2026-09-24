//! A point-in-time view of a pull request, as fetched from GitHub.

use std::fmt;

use color_eyre::eyre::{Result, eyre};

use crate::{repo::RepoName, run::Side};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrKey {
    pub repo: RepoName,
    /// GitHub PR numbers are GraphQL `Int`s, which are 32-bit.
    pub number: u32,
}

impl PrKey {
    /// The PR's page on github.com. Logs identify PRs by this so the link
    /// is clickable in a terminal.
    #[must_use]
    pub fn url(&self) -> String {
        format!("https://github.com/{}/pull/{}", self.repo, self.number)
    }

    /// Parses a PR page URL as [`PrKey::url`] writes it. Anything after
    /// the number, such as `/files` or a `#fragment`, is ignored.
    pub fn parse_url(url: &str) -> Result<Self> {
        let parsed = url.strip_prefix("https://github.com/").and_then(|rest| {
            let mut parts = rest.split('/');
            let (owner, name) = (parts.next()?, parts.next()?);
            (parts.next()? == "pull").then_some(())?;
            let number = parts.next()?;
            let number = number.split(['#', '?']).next()?.parse().ok()?;
            Some((RepoName::parse(&format!("{owner}/{name}")).ok()?, number))
        });
        let (repo, number) = parsed.ok_or_else(|| {
            eyre!("`{url}` is not a pull request URL like https://github.com/owner/name/pull/123")
        })?;
        Ok(Self { repo, number })
    }
}

impl fmt::Display for PrKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSnapshot {
    pub key: PrKey,
    pub title: String,
    /// The PR description, as written.
    pub body: String,
    pub url: String,
    pub author: String,
    pub head_sha: String,
    pub base_sha: String,
    pub is_draft: bool,
    /// Review is requested from the current user. GitHub fills this for
    /// direct requests; the poller adds team requests that pass the config's
    /// team filter.
    pub review_requested: bool,
    /// Teams whose review is requested, whether or not you're a member.
    pub requested_teams: Vec<TeamRef>,
    pub reviews: Vec<Review>,
    /// Inline review threads, plus the PR conversation as a thread with id
    /// [`CONVERSATION_THREAD`].
    pub threads: Vec<Thread>,
    /// Changed file paths; only fetched when path-scoped config needs them.
    pub files: Option<Vec<String>>,
    /// GitHub's `reviewDecision`: `APPROVED`, `CHANGES_REQUESTED` or
    /// `REVIEW_REQUIRED`; `None` when no review is required.
    pub review_decision: Option<String>,
    /// GitHub's `mergeStateStatus`, e.g. `CLEAN`, `BLOCKED`, `UNSTABLE`.
    pub merge_state: Option<String>,
    /// The head commit's combined checks: `SUCCESS`, `PENDING`, `FAILURE`…
    pub checks: Option<String>,
    /// When GitHub last saw activity on the PR, as it writes timestamps.
    /// `None` if it didn't say.
    pub updated_at: Option<String>,
}

/// A GitHub team, lowercased.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TeamRef {
    pub org: String,
    pub slug: String,
}

impl TeamRef {
    #[must_use]
    pub fn new(org: &str, slug: &str) -> Self {
        Self {
            org: org.to_ascii_lowercase(),
            slug: slug.to_ascii_lowercase(),
        }
    }
}

impl fmt::Display for TeamRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.org, self.slug)
    }
}

/// Thread id for a PR's top-level conversation comments.
pub const CONVERSATION_THREAD: &str = "conversation";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub id: String,
    pub author: String,
    pub state: ReviewState,
    pub body: String,
    pub submitted_at: String,
    /// The commit it was left on, if GitHub said.
    pub commit: Option<String>,
    /// Left by a bot account, which never counts as a reviewer.
    pub by_bot: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

impl ReviewState {
    /// GitHub's name for the state.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "APPROVED",
            Self::ChangesRequested => "CHANGES_REQUESTED",
            Self::Commented => "COMMENTED",
            Self::Dismissed => "DISMISSED",
            Self::Pending => "PENDING",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: String,
    pub path: Option<String>,
    /// Its last line on [`Placement::head`]; `None` once it's outdated.
    pub line: Option<u32>,
    pub resolved: bool,
    /// Where on the diff it sits beyond `line`. The conversation's is empty.
    pub place: Placement,
    /// Oldest first.
    pub comments: Vec<Comment>,
}

/// Where an inline review thread sits, as GitHub placed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    /// The first line of a multi-line thread, on `head`.
    pub start_line: Option<u32>,
    pub side: Option<Side>,
    /// The PR head the thread's `line` and `start_line` are lines of: the
    /// head when it was fetched.
    pub head: Option<String>,
    /// Newer commits changed its lines, so it has no `line` on `head`.
    pub outdated: bool,
    /// Its lines in `original_commit`, where it was left.
    pub original_start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub original_commit: Option<String>,
}

impl Thread {
    /// Whether this thread already speaks to `lines` of `path` on `side`,
    /// all lines of `head`: it's on at least one of them, and still open.
    /// Resolved threads don't count, nor do ones whose lines on `head`
    /// aren't known; see [`Placement::lines_at`].
    #[must_use]
    pub fn overlaps(&self, path: &str, side: Side, (start, end): (u32, u32), head: &str) -> bool {
        if self.resolved || self.comments.is_empty() || self.path.as_deref() != Some(path) {
            return false;
        }
        self.place
            .lines_at(self.line, head)
            .is_some_and(|(s, first, last)| s == side && first <= end && start <= last)
    }
}

impl Placement {
    /// Its side and first and last lines on `head`: GitHub's `line` (the
    /// thread's) and `start_line` when `head` is the PR head they were
    /// fetched for and the thread isn't outdated, else its original lines
    /// when `head` is the commit it was left on. `None` otherwise, since
    /// lines of another commit may not be the same lines, and for a thread
    /// on a whole file.
    #[must_use]
    pub fn lines_at(&self, line: Option<u32>, head: &str) -> Option<(Side, u32, u32)> {
        let (start, end) = match (line, self.original_line) {
            (Some(line), _) if !self.outdated && self.head.as_deref() == Some(head) => {
                (self.start_line, line)
            }
            (_, Some(line)) if self.original_commit.as_deref() == Some(head) => {
                (self.original_start_line, line)
            }
            _ => return None,
        };
        let side = self.side.unwrap_or(Side::Right);
        Some((side, start.unwrap_or(end).min(end), end))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
    /// Its page on GitHub, when GitHub gave one.
    pub url: Option<String>,
    /// Left by a bot account, which never needs an answer.
    pub by_bot: bool,
    /// When you reacted to it with an emoji, which counts as answering it.
    /// The comment's own time stands in if GitHub didn't give the
    /// reaction's.
    pub reacted_at: Option<String>,
    /// Everyone's latest reaction to it, one per login, among the newest
    /// GitHub returned. The PR author's reaction to your comment answers
    /// it, as yours answers theirs.
    pub reactions: Vec<Reaction>,
}

/// Someone's emoji reaction to a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub login: String,
    /// The comment's own time stands in if GitHub didn't give one.
    pub at: String,
}

impl PrSnapshot {
    #[must_use]
    pub fn is_authored_by(&self, login: &str) -> bool {
        is_login(&self.author, login)
    }

    /// Whether `login` reviews this PR: their review is requested, or
    /// they've left one in any state. Submitting a review clears the
    /// request, and a push can dismiss the review, so neither alone would
    /// do. Triggers and the reviews you owe share this rule.
    #[must_use]
    pub fn is_reviewer(&self, login: &str) -> bool {
        self.review_requested || self.reviews.iter().any(|r| is_login(&r.author, login))
    }
}

/// Whether `login` and `other` name the same GitHub account: logins
/// ignore case, ASCII only. The store's queries compare logins in SQL with
/// `lower()`, which is the same rule; keep them in step.
#[must_use]
pub fn is_login(login: &str, other: &str) -> bool {
    login.eq_ignore_ascii_case(other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_back_to_keys() {
        let key = PrKey {
            repo: RepoName::new("org", "repo"),
            number: 7,
        };
        assert_eq!(PrKey::parse_url(&key.url()).unwrap(), key);
        assert_eq!(
            PrKey::parse_url("https://github.com/Org/Repo/pull/7/files#diff").unwrap(),
            key
        );
        for bad in [
            "https://github.com/org/repo/issues/7",
            "https://github.com/org/repo/pull/x",
            "https://example.com/org/repo/pull/7",
            "org/repo#7",
        ] {
            assert!(PrKey::parse_url(bad).is_err(), "{bad}");
        }
    }

    fn thread(path: &str, line: Option<u32>, place: Placement) -> Thread {
        Thread {
            id: "t".into(),
            path: Some(path.into()),
            line,
            resolved: false,
            place,
            comments: vec![Comment {
                id: "c".into(),
                author: "bob".into(),
                body: "hm".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                url: None,
                by_bot: false,
                reacted_at: None,
                reactions: vec![],
            }],
        }
    }

    /// On the head it was fetched at, `start..=line` on the new side.
    fn current(start: Option<u32>) -> Placement {
        Placement {
            start_line: start,
            side: Some(Side::Right),
            head: Some("h2".into()),
            ..Placement::default()
        }
    }

    #[test]
    fn a_thread_overlaps_lines_it_shares_on_the_same_file_and_side() {
        let on = |t: &Thread, lines| t.overlaps("src/a.rs", Side::Right, lines, "h2");
        let single = thread("src/a.rs", Some(5), current(None));
        assert!(on(&single, (5, 5)));
        assert!(on(&single, (3, 5)));
        assert!(on(&single, (5, 9)));
        assert!(!on(&single, (6, 9)));
        assert!(!on(&single, (1, 4)));

        let range = thread("src/a.rs", Some(8), current(Some(4)));
        assert!(on(&range, (1, 4)));
        assert!(on(&range, (8, 12)));
        assert!(on(&range, (6, 6)));
        assert!(!on(&range, (9, 12)));
        assert!(!on(&range, (1, 3)));

        // Another file, or the other side of the same file.
        assert!(!range.overlaps("src/b.rs", Side::Right, (4, 8), "h2"));
        assert!(!range.overlaps("src/a.rs", Side::Left, (4, 8), "h2"));
        // No side from GitHub is the new file's.
        let sideless = thread(
            "src/a.rs",
            Some(5),
            Placement {
                side: None,
                ..current(None)
            },
        );
        assert!(on(&sideless, (5, 5)));
    }

    #[test]
    fn resolved_threads_and_empty_ones_dont_overlap() {
        let mut resolved = thread("src/a.rs", Some(5), current(None));
        resolved.resolved = true;
        assert!(!resolved.overlaps("src/a.rs", Side::Right, (5, 5), "h2"));
        let mut empty = thread("src/a.rs", Some(5), current(None));
        empty.comments.clear();
        assert!(!empty.overlaps("src/a.rs", Side::Right, (5, 5), "h2"));
    }

    #[test]
    fn lines_count_only_on_the_commit_they_are_lines_of() {
        let on = |t: &Thread, head| t.overlaps("src/a.rs", Side::Right, (5, 5), head);
        // Fetched for another head: its line there may not be line 5 here.
        let fresh = thread("src/a.rs", Some(5), current(None));
        assert!(!on(&fresh, "h1"));
        // Never fetched with a head, as rows from before it was recorded.
        assert!(!on(
            &thread("src/a.rs", Some(5), Placement::default()),
            "h2"
        ));

        // Outdated: its original lines hold only on the commit it was left
        // on, and its stale `line` never counts.
        let outdated = thread(
            "src/a.rs",
            None,
            Placement {
                side: Some(Side::Right),
                head: Some("h2".into()),
                outdated: true,
                original_start_line: Some(4),
                original_line: Some(6),
                original_commit: Some("h1".into()),
                ..Placement::default()
            },
        );
        assert!(on(&outdated, "h1"));
        assert!(!on(&outdated, "h2"));
        let mut stale_line = outdated.clone();
        stale_line.line = Some(5);
        assert!(!on(&stale_line, "h2"));
        // A thread on the whole file has no lines.
        assert!(!on(&thread("src/a.rs", None, current(None)), "h2"));
    }

    #[test]
    fn url_is_the_github_pull_page() {
        let key = PrKey {
            repo: RepoName::new("Org", "Repo"),
            number: 7,
        };
        assert_eq!(key.url(), "https://github.com/org/repo/pull/7");
    }
}
