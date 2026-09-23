//! A point-in-time view of a pull request, as fetched from GitHub.

use std::fmt;

use color_eyre::eyre::{Result, eyre};

use crate::repo::RepoName;

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
    pub line: Option<u32>,
    pub resolved: bool,
    /// Oldest first.
    pub comments: Vec<Comment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
    /// Left by a bot account, which never needs an answer.
    pub by_bot: bool,
    /// When you reacted to it with an emoji, which counts as answering it.
    /// The comment's own time stands in if GitHub didn't give the
    /// reaction's.
    pub reacted_at: Option<String>,
}

impl PrSnapshot {
    #[must_use]
    pub fn is_authored_by(&self, login: &str) -> bool {
        self.author.eq_ignore_ascii_case(login)
    }
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

    #[test]
    fn url_is_the_github_pull_page() {
        let key = PrKey {
            repo: RepoName::new("Org", "Repo"),
            number: 7,
        };
        assert_eq!(key.url(), "https://github.com/org/repo/pull/7");
    }
}
