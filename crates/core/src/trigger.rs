//! Deciding what needs attention by comparing a fresh PR snapshot with what
//! was stored on the previous poll.
//!
//! The first time a PR is seen, its existing comments and reviews become the
//! baseline: only a pending review request can trigger then, so starting the
//! tool doesn't flood the dashboard with history.

use std::collections::HashSet;

use serde::Serialize;

use crate::pr::{PrSnapshot, ReviewState};

/// What the store remembers about a PR from the previous poll.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Known {
    pub head_sha: String,
    pub review_requested: bool,
    pub comment_ids: HashSet<String>,
    pub review_ids: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// Someone else's PR newly requests your review.
    ReviewRequested { head_sha: String },
    /// Someone else's PR you're reviewing got new commits.
    Push { from_sha: String, to_sha: String },
    /// Someone replied in a thread you commented in, on someone else's PR.
    Reply {
        thread_id: String,
        comment_ids: Vec<String>,
    },
    /// Someone else commented on or reviewed your PR.
    Feedback {
        comment_ids: Vec<String>,
        review_ids: Vec<String>,
    },
    /// Someone approved your PR. Informational: it starts no run.
    Approved {
        review_ids: Vec<String>,
        reviewers: Vec<String>,
    },
}

impl Trigger {
    /// Stable name used in the event log.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ReviewRequested { .. } => "review_requested",
            Self::Push { .. } => "push",
            Self::Reply { .. } => "reply",
            Self::Feedback { .. } => "feedback",
            Self::Approved { .. } => "approved",
        }
    }
}

/// Returns the triggers `snapshot` raises for the user `me`.
#[must_use]
pub fn detect(me: &str, known: Option<&Known>, snapshot: &PrSnapshot) -> Vec<Trigger> {
    let is_me = |login: &str| login.eq_ignore_ascii_case(me);
    if snapshot.is_authored_by(me) {
        return known
            .map(|known| {
                [
                    feedback(known, snapshot, &is_me),
                    approvals(known, snapshot, &is_me),
                ]
                .into_iter()
                .flatten()
                .collect()
            })
            .unwrap_or_default();
    }

    let mut triggers = Vec::new();
    let newly_requested = snapshot.review_requested && known.is_none_or(|k| !k.review_requested);
    if newly_requested {
        triggers.push(Trigger::ReviewRequested {
            head_sha: snapshot.head_sha.clone(),
        });
    }
    let Some(known) = known else {
        return triggers;
    };

    let reviewing = snapshot.review_requested || snapshot.reviews.iter().any(|r| is_me(&r.author));
    if !newly_requested && reviewing && known.head_sha != snapshot.head_sha {
        triggers.push(Trigger::Push {
            from_sha: known.head_sha.clone(),
            to_sha: snapshot.head_sha.clone(),
        });
    }

    for thread in &snapshot.threads {
        let Some(first_mine) = thread.comments.iter().position(|c| is_me(&c.author)) else {
            continue;
        };
        let comment_ids: Vec<String> = thread.comments[first_mine + 1..]
            .iter()
            .filter(|c| !is_me(&c.author) && !known.comment_ids.contains(&c.id))
            .map(|c| c.id.clone())
            .collect();
        if !comment_ids.is_empty() {
            triggers.push(Trigger::Reply {
                thread_id: thread.id.clone(),
                comment_ids,
            });
        }
    }
    triggers
}

/// Reviews by others that `known` hasn't seen.
fn new_reviews<'a>(
    known: &'a Known,
    snapshot: &'a PrSnapshot,
    is_me: &'a impl Fn(&str) -> bool,
) -> impl Iterator<Item = &'a crate::pr::Review> {
    snapshot
        .reviews
        .iter()
        .filter(move |r| !is_me(&r.author) && !known.review_ids.contains(&r.id))
}

fn feedback(
    known: &Known,
    snapshot: &PrSnapshot,
    is_me: &impl Fn(&str) -> bool,
) -> Option<Trigger> {
    let comment_ids: Vec<String> = snapshot
        .threads
        .iter()
        .flat_map(|t| &t.comments)
        .filter(|c| !is_me(&c.author) && !known.comment_ids.contains(&c.id))
        .map(|c| c.id.clone())
        .collect();
    // A bare approval needs no response (it's reported by `approvals`);
    // anything with words or a change request does.
    let review_ids: Vec<String> = new_reviews(known, snapshot, is_me)
        .filter(|r| !r.body.trim().is_empty() || r.state == ReviewState::ChangesRequested)
        .map(|r| r.id.clone())
        .collect();
    (!comment_ids.is_empty() || !review_ids.is_empty()).then_some(Trigger::Feedback {
        comment_ids,
        review_ids,
    })
}

fn approvals(
    known: &Known,
    snapshot: &PrSnapshot,
    is_me: &impl Fn(&str) -> bool,
) -> Option<Trigger> {
    let approved: Vec<_> = new_reviews(known, snapshot, is_me)
        .filter(|r| r.state == ReviewState::Approved)
        .collect();
    (!approved.is_empty()).then(|| Trigger::Approved {
        review_ids: approved.iter().map(|r| r.id.clone()).collect(),
        reviewers: approved.iter().map(|r| r.author.clone()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pr::{CONVERSATION_THREAD, Comment, PrKey, Review, Thread},
        repo::RepoName,
    };

    const ME: &str = "Me";

    fn snapshot(author: &str) -> PrSnapshot {
        PrSnapshot {
            key: PrKey {
                repo: RepoName::new("org", "repo"),
                number: 1,
            },
            title: "t".into(),
            url: "u".into(),
            author: author.into(),
            head_sha: "h1".into(),
            base_sha: "b".into(),
            is_draft: false,
            review_requested: false,
            reviews: vec![],
            threads: vec![],
            files: None,
        }
    }

    fn comment(id: &str, author: &str) -> Comment {
        Comment {
            id: id.into(),
            author: author.into(),
            body: "b".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn thread(id: &str, comments: Vec<Comment>) -> Thread {
        Thread {
            id: id.into(),
            path: None,
            line: None,
            resolved: false,
            comments,
        }
    }

    fn review(id: &str, author: &str, state: ReviewState, body: &str) -> Review {
        Review {
            id: id.into(),
            author: author.into(),
            state,
            body: body.into(),
            submitted_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    /// What the store would hold after seeing `snapshot`.
    fn known_from(snapshot: &PrSnapshot) -> Known {
        Known {
            head_sha: snapshot.head_sha.clone(),
            review_requested: snapshot.review_requested,
            comment_ids: snapshot
                .threads
                .iter()
                .flat_map(|t| &t.comments)
                .map(|c| c.id.clone())
                .collect(),
            review_ids: snapshot.reviews.iter().map(|r| r.id.clone()).collect(),
        }
    }

    #[test]
    fn first_sight_of_requested_pr_triggers_review() {
        let mut snap = snapshot("alice");
        snap.review_requested = true;
        snap.threads = vec![thread(
            "t",
            vec![comment("c1", "me"), comment("c2", "alice")],
        )];
        assert_eq!(
            detect(ME, None, &snap),
            [Trigger::ReviewRequested {
                head_sha: "h1".into()
            }]
        );
    }

    #[test]
    fn standing_request_does_not_retrigger() {
        let mut snap = snapshot("alice");
        snap.review_requested = true;
        assert_eq!(detect(ME, Some(&known_from(&snap)), &snap), []);
    }

    #[test]
    fn push_triggers_only_when_reviewing() {
        let before = snapshot("alice");
        let mut after = before.clone();
        after.head_sha = "h2".into();
        assert_eq!(detect(ME, Some(&known_from(&before)), &after), []);

        after.reviews = vec![review("r1", "me", ReviewState::Commented, "")];
        assert_eq!(
            detect(ME, Some(&known_from(&before)), &after),
            [Trigger::Push {
                from_sha: "h1".into(),
                to_sha: "h2".into()
            }]
        );
    }

    #[test]
    fn rerequest_after_push_is_one_full_review() {
        let before = snapshot("alice");
        let mut after = before.clone();
        after.head_sha = "h2".into();
        after.review_requested = true;
        assert_eq!(
            detect(ME, Some(&known_from(&before)), &after),
            [Trigger::ReviewRequested {
                head_sha: "h2".into()
            }]
        );
    }

    #[test]
    fn replies_only_count_after_my_comment() {
        let mut before = snapshot("alice");
        before.threads = vec![
            thread("mine", vec![comment("c1", "bob"), comment("c2", "me")]),
            thread("other", vec![comment("c3", "bob")]),
        ];
        let mut after = before.clone();
        after.threads[0].comments.push(comment("c4", "alice"));
        after.threads[0].comments.push(comment("c5", "me"));
        after.threads[1].comments.push(comment("c6", "alice"));
        assert_eq!(
            detect(ME, Some(&known_from(&before)), &after),
            [Trigger::Reply {
                thread_id: "mine".into(),
                comment_ids: vec!["c4".into()]
            }]
        );
    }

    #[test]
    fn replies_trigger_without_a_review_request() {
        let mut before = snapshot("alice");
        before.threads = vec![thread(CONVERSATION_THREAD, vec![comment("c1", "ME")])];
        let mut after = before.clone();
        after.threads[0].comments.push(comment("c2", "alice"));
        assert!(matches!(
            detect(ME, Some(&known_from(&before)), &after).as_slice(),
            [Trigger::Reply { .. }]
        ));
    }

    #[test]
    fn bare_approvals_are_reported_but_need_no_response() {
        let before = snapshot("me");
        let mut after = before.clone();
        after.threads = vec![thread("t", vec![comment("c1", "me"), comment("c2", "bob")])];
        after.reviews = vec![
            review("r1", "bob", ReviewState::Approved, ""),
            review("r2", "carol", ReviewState::ChangesRequested, ""),
            review("r3", "dan", ReviewState::Commented, "looks off"),
        ];
        assert_eq!(
            detect(ME, Some(&known_from(&before)), &after),
            [
                Trigger::Feedback {
                    comment_ids: vec!["c2".into()],
                    review_ids: vec!["r2".into(), "r3".into()],
                },
                Trigger::Approved {
                    review_ids: vec!["r1".into()],
                    reviewers: vec!["bob".into()],
                },
            ]
        );
    }

    #[test]
    fn my_pr_first_sight_is_baseline() {
        let mut snap = snapshot("me");
        snap.threads = vec![thread("t", vec![comment("c1", "bob")])];
        assert_eq!(detect(ME, None, &snap), []);
    }
}
