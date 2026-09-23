//! Where a PR stands, for the TUI and the dashboard: mergeable, approved,
//! changes requested, and how many threads wait on your answer. They
//! combine: approved with unanswered comments still needs you.

use std::fmt;

use crate::pr::Thread;

/// What a PR's state is worked out from, as last polled.
#[derive(Debug, Clone, Copy)]
pub struct StateFacts<'a> {
    /// GitHub's `reviewDecision`.
    pub review_decision: Option<&'a str>,
    /// GitHub's `mergeStateStatus`.
    pub merge_state: Option<&'a str>,
    /// The head commit's combined check state.
    pub checks: Option<&'a str>,
    pub threads: &'a [Thread],
    /// Submitted reviews, oldest first, bots' left out. Approval is worked
    /// out from them when GitHub gives no `reviewDecision`, as for a repo
    /// that doesn't require reviews.
    pub reviews: &'a [ReviewFact<'a>],
    /// The current user.
    pub me: &'a str,
    /// Your own PR: every thread can need your answer. On someone else's,
    /// only threads you've commented in can.
    pub mine: bool,
}

/// One submitted review, for [`StateFacts::reviews`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewFact<'a> {
    pub author: &'a str,
    /// GitHub's name for its state, e.g. `APPROVED`.
    pub state: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Approval {
    /// Approved, and GitHub would merge it now.
    Mergeable,
    /// Approved, but not mergeable yet, because of `Checks` if known.
    Approved(Checks),
    ChangesRequested,
    #[default]
    None,
}

/// Why an approved PR isn't mergeable, as far as its checks go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checks {
    Failing,
    Pending,
    /// Passing, or not what's holding it up.
    Other,
}

/// Nothing notable, by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrState {
    pub approval: Approval,
    /// Threads with a comment newer than your latest answer in them.
    pub unanswered: u32,
}

/// How much a state asks of you, for colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Quiet,
    /// Good news: approved or mergeable.
    Good,
    /// You need to act: unanswered comments or changes requested.
    Act,
}

impl PrState {
    #[must_use]
    pub fn new(facts: &StateFacts<'_>) -> Self {
        Self {
            approval: approval(facts),
            unanswered: unanswered(facts),
        }
    }

    #[must_use]
    pub fn urgency(&self) -> Urgency {
        match self.approval {
            _ if self.unanswered > 0 => Urgency::Act,
            Approval::ChangesRequested => Urgency::Act,
            Approval::Mergeable | Approval::Approved(_) => Urgency::Good,
            Approval::None => Urgency::Quiet,
        }
    }

    /// In full, e.g. `approved · ci failing · 2 unanswered`, or `—` when
    /// there's nothing to say. This is what the dashboard shows.
    #[must_use]
    pub fn status(&self) -> String {
        self.to_string()
    }

    /// At most `width` characters: the full status if it fits, else the
    /// same in short words, else the most pressing word alone.
    #[must_use]
    pub fn fitted(&self, width: usize) -> String {
        let full = self.status();
        if full.chars().count() <= width {
            return full;
        }
        let words = self.short_words();
        let short = words.join(" · ");
        if short.chars().count() <= width {
            return short;
        }
        words.into_iter().last().unwrap_or_default()
    }

    /// Short words, least pressing first.
    fn short_words(self) -> Vec<String> {
        let mut words = Vec::new();
        match self.approval {
            Approval::Mergeable => words.push("mergeable".into()),
            Approval::Approved(_) => words.push("approved".into()),
            Approval::ChangesRequested => words.push("changes".into()),
            Approval::None => {}
        }
        if self.unanswered > 0 {
            words.push(format!("{} new", self.unanswered));
        }
        if words.is_empty() {
            words.push("—".into());
        }
        words
    }
}

impl fmt::Display for PrState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        match self.approval {
            Approval::Mergeable => parts.push("mergeable".into()),
            Approval::Approved(checks) => {
                parts.push("approved".into());
                match checks {
                    Checks::Failing => parts.push("ci failing".into()),
                    Checks::Pending => parts.push("ci pending".into()),
                    Checks::Other => {}
                }
            }
            Approval::ChangesRequested => parts.push("changes requested".into()),
            Approval::None => {}
        }
        if self.unanswered > 0 {
            parts.push(format!("{} unanswered", self.unanswered));
        }
        if parts.is_empty() {
            return write!(f, "—");
        }
        write!(f, "{}", parts.join(" · "))
    }
}

fn approval(facts: &StateFacts<'_>) -> Approval {
    let checks = match facts.checks {
        Some("FAILURE" | "ERROR") => Checks::Failing,
        Some("PENDING" | "EXPECTED") => Checks::Pending,
        _ => Checks::Other,
    };
    match facts
        .review_decision
        .or_else(|| decision_from(facts.reviews))
    {
        Some("CHANGES_REQUESTED") => Approval::ChangesRequested,
        Some("APPROVED") => {
            // GitHub's own word, when it gave one; else passing checks.
            // `UNKNOWN` is GitHub not having worked it out yet.
            let mergeable = match facts.merge_state {
                Some("UNKNOWN") | None => facts.checks == Some("SUCCESS"),
                Some(state) => matches!(state, "CLEAN" | "HAS_HOOKS"),
            };
            if mergeable {
                Approval::Mergeable
            } else {
                Approval::Approved(checks)
            }
        }
        _ => Approval::None,
    }
}

/// The `reviewDecision` GitHub would give if it gave one, from each
/// reviewer's latest word: a change request by anyone, else an approval by
/// anyone. Comments don't change where a reviewer stands, and a dismissal
/// takes their word back.
fn decision_from<'a>(reviews: &[ReviewFact<'a>]) -> Option<&'static str> {
    let mut latest: Vec<ReviewFact<'a>> = Vec::new();
    for &review in reviews {
        if !matches!(review.state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            continue;
        }
        match latest
            .iter_mut()
            .find(|seen| seen.author.eq_ignore_ascii_case(review.author))
        {
            Some(seen) => seen.state = review.state,
            None => latest.push(review),
        }
    }
    if latest.iter().any(|r| r.state == "CHANGES_REQUESTED") {
        Some("CHANGES_REQUESTED")
    } else if latest.iter().any(|r| r.state == "APPROVED") {
        Some("APPROVED")
    } else {
        None
    }
}

/// Threads that aren't resolved and have someone else's comment (not a
/// bot's) newer than your latest answer: your own comment, or your
/// reaction to any comment in the thread.
fn unanswered(facts: &StateFacts<'_>) -> u32 {
    let is_me = |login: &str| login.eq_ignore_ascii_case(facts.me);
    let count = facts
        .threads
        .iter()
        .filter(|thread| !thread.resolved)
        .filter(|thread| {
            let commented = thread.comments.iter().any(|c| is_me(&c.author));
            // On someone else's PR, as for reply triggers: threads you've
            // commented in, the conversation included.
            if !facts.mine && !commented {
                return false;
            }
            let answered = thread
                .comments
                .iter()
                .filter(|c| is_me(&c.author))
                .map(|c| c.created_at.as_str())
                .chain(
                    thread
                        .comments
                        .iter()
                        .filter_map(|c| c.reacted_at.as_deref()),
                )
                .max();
            thread.comments.iter().any(|c| {
                !is_me(&c.author)
                    && !c.by_bot
                    && answered.is_none_or(|at| c.created_at.as_str() > at)
            })
        })
        .count();
    // A poll fetches far fewer threads than that.
    u32::try_from(count).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::{CONVERSATION_THREAD, Comment};

    fn comment(author: &str, at: &str) -> Comment {
        Comment {
            id: format!("{author}-{at}"),
            author: author.into(),
            body: String::new(),
            created_at: format!("2026-01-01T00:00:{at}Z"),
            by_bot: false,
            reacted_at: None,
        }
    }

    fn thread(comments: Vec<Comment>) -> Thread {
        Thread {
            id: "t".into(),
            path: None,
            line: None,
            resolved: false,
            comments,
        }
    }

    fn state(decision: Option<&str>, merge: Option<&str>, checks: Option<&str>) -> PrState {
        PrState::new(&StateFacts {
            review_decision: decision,
            merge_state: merge,
            checks,
            threads: &[],
            reviews: &[],
            me: "me",
            mine: true,
        })
    }

    fn count(threads: &[Thread], mine: bool) -> u32 {
        PrState::new(&StateFacts {
            review_decision: None,
            merge_state: None,
            checks: None,
            threads,
            reviews: &[],
            me: "Me",
            mine,
        })
        .unanswered
    }

    fn from_reviews(
        reviews: &[(&str, &str)],
        decision: Option<&str>,
        checks: Option<&str>,
    ) -> String {
        let reviews: Vec<ReviewFact<'_>> = reviews
            .iter()
            .map(|&(author, state)| ReviewFact { author, state })
            .collect();
        PrState::new(&StateFacts {
            review_decision: decision,
            merge_state: None,
            checks,
            threads: &[],
            reviews: &reviews,
            me: "me",
            mine: true,
        })
        .status()
    }

    #[test]
    fn without_githubs_decision_each_reviewers_latest_word_counts() {
        // Changes requested by anyone outweigh approvals.
        let blocked = [("bob", "APPROVED"), ("carol", "CHANGES_REQUESTED")];
        assert_eq!(from_reviews(&blocked, None, None), "changes requested");
        // A reviewer's later approval replaces their change request, in any case.
        let turned = [("bob", "CHANGES_REQUESTED"), ("Bob", "APPROVED")];
        assert_eq!(from_reviews(&turned, None, None), "approved");
        assert_eq!(from_reviews(&turned, None, Some("SUCCESS")), "mergeable");
        assert_eq!(
            from_reviews(&turned, None, Some("FAILURE")),
            "approved · ci failing"
        );
        // Comments don't change a standing; a dismissal takes it back.
        let commented = [("bob", "APPROVED"), ("bob", "COMMENTED")];
        assert_eq!(from_reviews(&commented, None, None), "approved");
        let dismissed = [("bob", "CHANGES_REQUESTED"), ("bob", "DISMISSED")];
        assert_eq!(from_reviews(&dismissed, None, None), "—");
        assert_eq!(from_reviews(&[], None, None), "—");
        // GitHub's own word wins when it gives one.
        assert_eq!(from_reviews(&blocked, Some("REVIEW_REQUIRED"), None), "—");
        assert_eq!(from_reviews(&blocked, Some("APPROVED"), None), "approved");
    }

    #[test]
    fn approval_follows_github() {
        assert_eq!(
            state(Some("APPROVED"), Some("CLEAN"), None).status(),
            "mergeable"
        );
        assert_eq!(
            state(Some("APPROVED"), Some("UNSTABLE"), Some("FAILURE")).status(),
            "approved · ci failing"
        );
        assert_eq!(
            state(Some("APPROVED"), Some("BLOCKED"), Some("PENDING")).status(),
            "approved · ci pending"
        );
        // Without a merge state, or before GitHub has worked it out,
        // passing checks make it mergeable.
        assert_eq!(
            state(Some("APPROVED"), None, Some("SUCCESS")).status(),
            "mergeable"
        );
        assert_eq!(
            state(Some("APPROVED"), Some("UNKNOWN"), Some("SUCCESS")).status(),
            "mergeable"
        );
        assert_eq!(
            state(Some("CHANGES_REQUESTED"), Some("BLOCKED"), None).status(),
            "changes requested"
        );
        assert_eq!(state(Some("REVIEW_REQUIRED"), None, None).status(), "—");
        assert_eq!(state(None, None, None).urgency(), Urgency::Quiet);
    }

    #[test]
    fn a_thread_waits_on_you_until_you_comment_or_react() {
        // Nobody else spoke, or you answered last.
        assert_eq!(count(&[thread(vec![comment("me", "01")])], true), 0);
        assert_eq!(
            count(
                &[thread(vec![comment("bob", "01"), comment("me", "02")])],
                true
            ),
            0
        );
        // Bob spoke after you.
        let open = thread(vec![comment("me", "01"), comment("bob", "02")]);
        assert_eq!(count(std::slice::from_ref(&open), true), 1);

        // Your reaction answers, by the reaction's time...
        let mut reacted = open.clone();
        reacted.comments[1].reacted_at = Some("2026-01-01T00:00:03Z".into());
        assert_eq!(count(&[reacted.clone()], true), 0);
        // ...or when GitHub didn't say, the comment's.
        reacted.comments[1].reacted_at = Some(reacted.comments[1].created_at.clone());
        assert_eq!(count(&[reacted.clone()], true), 0);
        // A reaction to an older comment doesn't answer a newer one.
        let mut older = thread(vec![comment("bob", "01"), comment("carol", "05")]);
        older.comments[0].reacted_at = Some("2026-01-01T00:00:03Z".into());
        assert_eq!(count(&[older], true), 1);
    }

    #[test]
    fn resolved_threads_and_bots_need_no_answer() {
        let mut resolved = thread(vec![comment("bob", "01")]);
        resolved.resolved = true;
        let mut bot = thread(vec![comment("codecov[bot]", "01")]);
        bot.comments[0].by_bot = true;
        assert_eq!(count(&[resolved, bot], true), 0);
    }

    #[test]
    fn on_others_prs_only_threads_you_joined_count() {
        let joined = thread(vec![comment("me", "01"), comment("bob", "02")]);
        let not_joined = thread(vec![comment("bob", "01")]);
        let mut conversation = thread(vec![comment("carol", "03")]);
        conversation.id = CONVERSATION_THREAD.into();
        assert_eq!(
            count(
                &[joined.clone(), not_joined.clone(), conversation.clone()],
                false
            ),
            1
        );
        assert_eq!(count(&[joined, not_joined, conversation], true), 3);
    }

    #[test]
    fn states_combine_and_shorten_to_fit() {
        let both = PrState {
            approval: Approval::Approved(Checks::Failing),
            unanswered: 2,
        };
        assert_eq!(both.status(), "approved · ci failing · 2 unanswered");
        assert_eq!(both.urgency(), Urgency::Act);
        assert_eq!(both.fitted(40), "approved · ci failing · 2 unanswered");
        assert_eq!(both.fitted(16), "approved · 2 new");
        assert_eq!(both.fitted(10), "2 new");
        let changes = PrState {
            approval: Approval::ChangesRequested,
            unanswered: 1,
        };
        assert_eq!(changes.status(), "changes requested · 1 unanswered");
        assert_eq!(changes.fitted(15), "changes · 1 new");
        let quiet = PrState {
            approval: Approval::None,
            unanswered: 0,
        };
        assert_eq!(quiet.fitted(10), "—");
        assert_eq!(
            PrState {
                approval: Approval::Mergeable,
                unanswered: 0
            }
            .urgency(),
            Urgency::Good
        );
    }
}
