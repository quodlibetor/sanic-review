//! When a review of a PR you owe may be started by hand, which the TUI's
//! `r` and the dashboard's Review now both ask first.

use crate::skip::Skip;

/// Why a review may be started by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// The latest run failed or crashed.
    Failed,
    /// `--manual-reviews` is holding its queued review.
    Held,
    /// It isn't reviewed automatically, for this reason: archived, a draft,
    /// already reviewed by someone, or a skipped title.
    Skipped(Skip),
}

impl Why {
    /// From why the PR isn't reviewed automatically, if it isn't
    /// ([`SkipRules::decide`](crate::skip::SkipRules::decide)'s answer),
    /// and its latest run's status. `None` if there's nothing to start:
    /// its review is under way, done, or will come by itself.
    #[must_use]
    pub fn of(skip: Option<&Skip>, latest_run: Option<&str>, manual_reviews: bool) -> Option<Self> {
        match (skip, latest_run) {
            (Some(skip), _) => Some(Self::Skipped(skip.clone())),
            (None, Some("failed" | "crashed")) => Some(Self::Failed),
            (None, Some("queued")) if manual_reviews => Some(Self::Held),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_come_first_then_failures_then_holds() {
        let draft = Skip::Draft;
        assert_eq!(
            Why::of(Some(&draft), Some("failed"), true),
            Some(Why::Skipped(Skip::Draft))
        );
        for status in ["failed", "crashed"] {
            assert_eq!(Why::of(None, Some(status), false), Some(Why::Failed));
        }
        assert_eq!(Why::of(None, Some("queued"), true), Some(Why::Held));
        // Without --manual-reviews a queued review runs by itself.
        assert_eq!(Why::of(None, Some("queued"), false), None);
        for status in [None, Some("running"), Some("succeeded"), Some("superseded")] {
            assert_eq!(Why::of(None, status, true), None, "{status:?}");
        }
    }
}
