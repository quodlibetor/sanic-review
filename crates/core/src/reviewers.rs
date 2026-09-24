//! Who has reviewed a PR, where each stands, and whether their review is
//! on the latest push: the dashboard's "reviewed by".

use crate::pr::is_login;

/// Where a reviewer stands, as GitHub works out a review decision: their
/// latest approval or change request, else that they commented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stance {
    Approved,
    ChangesRequested,
    Commented,
}

impl Stance {
    /// In words, e.g. `changes requested`.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequested => "changes requested",
            Self::Commented => "commented",
        }
    }
}

/// One person's reviews of a PR, summed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer {
    pub login: String,
    pub stance: Stance,
    /// When their latest review was submitted, as GitHub writes times.
    pub submitted_at: String,
    /// Pushes the poller has seen since the commit their latest review is
    /// on; 0 when it's on the head. Several pushes between two polls count
    /// as one.
    pub pushes_since: u32,
}

impl Reviewer {
    #[must_use]
    pub fn on_head(&self) -> bool {
        self.pushes_since == 0
    }
}

/// One submitted review, bots' left out, for [`reviewers`].
#[derive(Debug, Clone, Copy)]
pub struct ReviewRecord<'a> {
    pub author: &'a str,
    /// GitHub's name for its state, e.g. `APPROVED`.
    pub state: &'a str,
    pub submitted_at: &'a str,
    /// The commit it was left on, if GitHub said.
    pub commit: Option<&'a str>,
}

/// A head the poller saw, and when it first saw it.
#[derive(Debug, Clone, Copy)]
pub struct SeenHead<'a> {
    pub sha: &'a str,
    pub seen_at: &'a str,
}

/// Everyone with a submitted review, in the order they first reviewed.
/// `reviews` run oldest first; `heads` are every head the poller saw. A
/// dismissal takes a reviewer's approval or change request back, and
/// someone whose only reviews were dismissed isn't listed; pending reviews
/// don't count.
#[must_use]
pub fn reviewers(
    reviews: &[ReviewRecord<'_>],
    heads: &[SeenHead<'_>],
    head: &str,
) -> Vec<Reviewer> {
    struct Seen<'a> {
        login: &'a str,
        standing: Option<Stance>,
        commented: bool,
        latest: Option<ReviewRecord<'a>>,
    }
    let mut seen: Vec<Seen<'_>> = Vec::new();
    for &review in reviews {
        if review.state == "PENDING" {
            continue;
        }
        if !seen.iter().any(|s| is_login(s.login, review.author)) {
            seen.push(Seen {
                login: review.author,
                standing: None,
                commented: false,
                latest: None,
            });
        }
        let Some(person) = seen.iter_mut().find(|s| is_login(s.login, review.author)) else {
            continue;
        };
        match review.state {
            "APPROVED" => person.standing = Some(Stance::Approved),
            "CHANGES_REQUESTED" => person.standing = Some(Stance::ChangesRequested),
            "DISMISSED" => {
                person.standing = None;
                continue;
            }
            _ => person.commented = true,
        }
        person.latest = Some(review);
    }
    seen.into_iter()
        .filter_map(|person| {
            let latest = person.latest?;
            let stance = person
                .standing
                .or(person.commented.then_some(Stance::Commented))?;
            Some(Reviewer {
                login: latest.author.to_owned(),
                stance,
                submitted_at: latest.submitted_at.to_owned(),
                pushes_since: pushes_since(&latest, heads, head),
            })
        })
        .collect()
}

/// Heads seen after the one `review` is on, and at least one if that
/// isn't the head. A commit the poller never saw as a head counts from
/// when the review was submitted.
fn pushes_since(review: &ReviewRecord<'_>, heads: &[SeenHead<'_>], head: &str) -> u32 {
    let Some(commit) = review.commit else {
        return 0;
    };
    if commit == head {
        return 0;
    }
    let from = heads
        .iter()
        .find(|h| h.sha == commit)
        .map_or(review.submitted_at, |h| h.seen_at);
    let later = heads.iter().filter(|h| h.seen_at > from).count();
    u32::try_from(later).unwrap_or(u32::MAX).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review<'a>(
        author: &'a str,
        state: &'a str,
        at: &'a str,
        commit: &'a str,
    ) -> ReviewRecord<'a> {
        ReviewRecord {
            author,
            state,
            submitted_at: at,
            commit: Some(commit),
        }
    }

    const HEADS: &[SeenHead<'static>] = &[
        SeenHead {
            sha: "h1",
            seen_at: "2026-01-01T00:00:00Z",
        },
        SeenHead {
            sha: "h2",
            seen_at: "2026-01-02T00:00:00Z",
        },
        SeenHead {
            sha: "h3",
            seen_at: "2026-01-03T00:00:00Z",
        },
    ];

    #[test]
    fn each_reviewer_once_with_where_they_stand() {
        let reviews = [
            review("bob", "APPROVED", "2026-01-01T01:00:00Z", "h1"),
            review("carol", "COMMENTED", "2026-01-01T02:00:00Z", "h1"),
            // A later comment keeps Bob's approval but is his latest review.
            review("Bob", "COMMENTED", "2026-01-03T01:00:00Z", "h3"),
            review("carol", "CHANGES_REQUESTED", "2026-01-02T01:00:00Z", "h2"),
            review("dave", "PENDING", "2026-01-03T02:00:00Z", "h3"),
        ];
        let got = reviewers(&reviews, HEADS, "h3");
        assert_eq!(
            got,
            [
                Reviewer {
                    login: "Bob".into(),
                    stance: Stance::Approved,
                    submitted_at: "2026-01-03T01:00:00Z".into(),
                    pushes_since: 0,
                },
                Reviewer {
                    login: "carol".into(),
                    stance: Stance::ChangesRequested,
                    submitted_at: "2026-01-02T01:00:00Z".into(),
                    pushes_since: 1,
                },
            ]
        );
        assert!(got[0].on_head());
        assert!(!got[1].on_head());
    }

    #[test]
    fn dismissals_take_a_stance_back() {
        let reviews = [
            review("bob", "APPROVED", "2026-01-01T01:00:00Z", "h1"),
            review("bob", "DISMISSED", "2026-01-02T01:00:00Z", "h1"),
            review("carol", "COMMENTED", "2026-01-01T02:00:00Z", "h1"),
            review("carol", "CHANGES_REQUESTED", "2026-01-01T03:00:00Z", "h1"),
            review("carol", "DISMISSED", "2026-01-01T04:00:00Z", "h1"),
        ];
        let got = reviewers(&reviews, HEADS, "h3");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].login, "carol");
        assert_eq!(got[0].stance, Stance::Commented);
        assert_eq!(got[0].pushes_since, 2);
    }

    #[test]
    fn a_commit_never_seen_as_a_head_counts_from_the_review() {
        let unseen = [review("bob", "APPROVED", "2026-01-02T12:00:00Z", "gone")];
        assert_eq!(reviewers(&unseen, HEADS, "h3")[0].pushes_since, 1);
        let early = [review("bob", "APPROVED", "2025-12-31T00:00:00Z", "gone")];
        assert_eq!(reviewers(&early, HEADS, "h3")[0].pushes_since, 3);
        // Two heads seen in one poll are still a push.
        let same = [
            SeenHead {
                sha: "h1",
                seen_at: "2026-01-01T00:00:00Z",
            },
            SeenHead {
                sha: "h2",
                seen_at: "2026-01-01T00:00:00Z",
            },
        ];
        let on_h1 = [review("bob", "APPROVED", "2026-01-01T00:00:00Z", "h1")];
        assert_eq!(reviewers(&on_h1, &same, "h2")[0].pushes_since, 1);
        // Not when GitHub didn't say which commit.
        let unknown = [ReviewRecord {
            commit: None,
            ..review("bob", "APPROVED", "2026-01-03T12:00:00Z", "")
        }];
        assert!(reviewers(&unknown, HEADS, "h3")[0].on_head());
    }
}
