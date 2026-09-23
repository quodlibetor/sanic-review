//! Agent runs and the drafts they produce.
//!
//! Only `review` runs exist so far. `reply` and `respond` runs will add
//! [`RunKind`] variants, their own triggers and their own output types.

use serde::{Deserialize, Serialize};

use crate::pr::{PrKey, Thread};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunKind {
    /// Review someone else's PR.
    Review,
}

impl RunKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
        }
    }
}

/// Why a review run was queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTrigger {
    /// Your review was requested: a full review.
    Requested,
    /// New commits on a PR you've reviewed; `from_sha` is the head you last
    /// saw. Reviews are still full until incremental reviews exist.
    Push { from_sha: String },
}

impl ReviewTrigger {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Requested => "review_requested",
            Self::Push { .. } => "push",
        }
    }

    /// Combines a queued trigger with a newer one for the same PR. A pending
    /// full review stays full, and a chain of pushes keeps the oldest base.
    #[must_use]
    pub fn merge(self, newer: Self) -> Self {
        match (self, newer) {
            (Self::Requested, _) | (_, Self::Requested) => Self::Requested,
            (older @ Self::Push { .. }, Self::Push { .. }) => older,
        }
    }
}

/// A review to run on one PR revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRequest {
    pub key: PrKey,
    pub profile: String,
    pub head_sha: String,
    pub base_sha: String,
    pub trigger: ReviewTrigger,
}

impl ReviewRequest {
    /// Reviews are idempotent per `(pr, head_sha)`.
    #[must_use]
    pub fn idempotency_key(&self) -> &str {
        &self.head_sha
    }
}

/// What the store knows about a PR, for the agent's brief. Everything here
/// is written by PR participants, so it's untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrContext {
    pub title: String,
    /// The PR description.
    pub body: String,
    pub url: String,
    pub author: String,
    pub threads: Vec<Thread>,
}

/// A run the store has accepted and the runner should pick up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedRun {
    pub id: i64,
    pub request: ReviewRequest,
}

/// What the agent suggests you do with the review. There is deliberately no
/// `approve`: approving is only ever your own choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Comment,
    RequestChanges,
    None,
}

impl Verdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::RequestChanges => "request_changes",
            Self::None => "none",
        }
    }
}

/// Which side of the diff an inline comment is on, as GitHub names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Left,
    Right,
}

impl Side {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
    Nit,
}

impl Severity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::Major => "major",
            Self::Minor => "minor",
            Self::Nit => "nit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// An inline comment the agent drafted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    /// First line of a multi-line comment.
    #[serde(default)]
    pub start_line: Option<u32>,
    pub side: Side,
    pub body: String,
    pub severity: Severity,
    pub confidence: Confidence,
}

/// The structured output of a `review` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOutput {
    pub summary: String,
    pub suggested_verdict: Verdict,
    #[serde(default)]
    pub comments: Vec<InlineComment>,
}

/// An inline comment plus whether its anchor is outside the PR's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftComment {
    pub comment: InlineComment,
    /// GitHub would reject this anchor; it can be re-anchored or posted as a
    /// top-level comment instead.
    pub unanchored: bool,
}

/// A finished review, ready to store as drafts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewResult {
    pub summary: String,
    pub verdict: Verdict,
    pub comments: Vec<DraftComment>,
    /// Lets "regenerate with instruction" resume the conversation.
    pub session_id: Option<String>,
    pub transcript_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_review_wins_when_merging_triggers() {
        let push = |from: &str| ReviewTrigger::Push {
            from_sha: from.into(),
        };
        assert_eq!(
            push("a").merge(ReviewTrigger::Requested),
            ReviewTrigger::Requested
        );
        assert_eq!(
            ReviewTrigger::Requested.merge(push("a")),
            ReviewTrigger::Requested
        );
        assert_eq!(push("a").merge(push("b")), push("a"));
    }

    #[test]
    fn approve_is_not_a_verdict() {
        let parsed: Result<Verdict, _> = serde_json::from_str(r#""approve""#);
        assert!(parsed.is_err());
    }
}
