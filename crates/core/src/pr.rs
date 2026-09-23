//! A point-in-time view of a pull request, as fetched from GitHub.

use std::fmt;

use crate::repo::RepoName;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrKey {
    pub repo: RepoName,
    pub number: u64,
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
    pub url: String,
    pub author: String,
    pub head_sha: String,
    pub base_sha: String,
    pub is_draft: bool,
    /// Review is requested from the current user, directly or via a team.
    pub review_requested: bool,
    pub reviews: Vec<Review>,
    /// Inline review threads, plus the PR conversation as a thread with id
    /// [`CONVERSATION_THREAD`].
    pub threads: Vec<Thread>,
    /// Changed file paths; only fetched when path-scoped config needs them.
    pub files: Option<Vec<String>>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
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
}

impl PrSnapshot {
    #[must_use]
    pub fn is_authored_by(&self, login: &str) -> bool {
        self.author.eq_ignore_ascii_case(login)
    }
}
