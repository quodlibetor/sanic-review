//! Where a PR stands, for the TUI and the dashboard: mergeable, approved,
//! changes requested, and how many threads wait on your answer. They
//! combine: approved with unanswered comments still needs you.

use std::fmt;

use crate::pr::{Comment, Thread, is_login};

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

/// Why an approved PR can't merge yet, most pressing first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Conflicts,
    CiFailing,
    CiPending,
    /// Behind its base, where the repo requires it to be up to date.
    Behind,
    /// Something else GitHub requires, such as another review.
    Blocked,
}

impl Block {
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Conflicts => "conflicts",
            Self::CiFailing => "ci failing",
            Self::CiPending => "ci pending",
            Self::Behind => "behind",
            Self::Blocked => "blocked",
        }
    }
}

/// What the checks and GitHub's merge state say, approved or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Merge {
    pub ci: Checks,
    /// What would hold the PR up once approved, if anything.
    pub block: Option<Block>,
}

impl Merge {
    /// From GitHub's `mergeStateStatus` and the head's combined checks.
    #[must_use]
    pub fn new(merge_state: Option<&str>, checks: Option<&str>) -> Self {
        let ci = checks_of(checks);
        let block = match (merge_state, ci) {
            (Some("DIRTY"), _) => Some(Block::Conflicts),
            // `UNSTABLE` is any non-passing status, pending ones included.
            (_, Checks::Pending) => Some(Block::CiPending),
            (_, Checks::Failing) | (Some("UNSTABLE"), _) => Some(Block::CiFailing),
            (Some("BEHIND"), _) => Some(Block::Behind),
            (Some("BLOCKED"), _) => Some(Block::Blocked),
            _ => None,
        };
        Self { ci, block }
    }
}

fn checks_of(checks: Option<&str>) -> Checks {
    match checks {
        Some("FAILURE" | "ERROR") => Checks::Failing,
        Some("PENDING" | "EXPECTED") => Checks::Pending,
        _ => Checks::Other,
    }
}

/// Nothing notable, by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrState {
    pub approval: Approval,
    /// Threads with a comment newer than your latest answer in them.
    pub unanswered: u32,
    /// Your own PR, where changes requested are yours to make.
    pub mine: bool,
}

/// How much a state asks of you, for colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Quiet,
    /// Good news: approved or mergeable.
    Good,
    /// You need to act: unanswered comments, or changes requested on your
    /// own PR.
    Act,
}

impl PrState {
    #[must_use]
    pub fn new(facts: &StateFacts<'_>) -> Self {
        Self {
            approval: approval(facts),
            unanswered: unanswered(facts),
            mine: facts.mine,
        }
    }

    #[must_use]
    pub fn urgency(&self) -> Urgency {
        match self.approval {
            _ if self.unanswered > 0 => Urgency::Act,
            // On a review you owe, the author has the changes to make,
            // whoever asked for them.
            Approval::ChangesRequested if self.mine => Urgency::Act,
            Approval::Mergeable | Approval::Approved(_) => Urgency::Good,
            Approval::ChangesRequested | Approval::None => Urgency::Quiet,
        }
    }

    /// Nothing to say, on anyone's PR: shown as `—`.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.approval == Approval::None && self.unanswered == 0
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
    let checks = checks_of(facts.checks);
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
            .find(|seen| is_login(seen.author, review.author))
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
    let is_me = |login: &str| is_login(login, facts.me);
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

/// Threads waiting on someone else's answer to you, for one person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Awaiting {
    pub login: String,
    pub threads: u32,
}

/// Unresolved threads where your latest comment has no answer yet, by who
/// it waits on, most threads first. On someone else's PR that's its
/// `author`; on your own (`author` `None`) it's whoever last spoke in the
/// thread before you, bots aside. They answer with a comment, or with a
/// reaction to one of yours, from your latest comment on.
#[must_use]
pub fn awaiting(threads: &[Thread], me: &str, author: Option<&str>) -> Vec<Awaiting> {
    let mut out: Vec<Awaiting> = Vec::new();
    for thread in threads {
        let Some(whom) = waiting_on(thread, me, author, true) else {
            continue;
        };
        match out.iter_mut().find(|a| is_login(&a.login, whom)) {
            Some(a) => a.threads += 1,
            None => out.push(Awaiting {
                login: whom.to_owned(),
                threads: 1,
            }),
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.threads));
    out
}

/// Your comments whose reactions could answer them: yours in the threads
/// [`awaiting`] would count if nobody had reacted. Only their reactions
/// need fetching to tell.
#[must_use]
pub fn reactions_wanted<'a>(threads: &'a [Thread], me: &str, author: Option<&str>) -> Vec<&'a str> {
    threads
        .iter()
        .filter(|thread| waiting_on(thread, me, author, false).is_some())
        .flat_map(|thread| &thread.comments)
        .filter(|c| is_login(&c.author, me))
        .map(|c| c.id.as_str())
        .collect()
}

/// Who `thread` waits on for an answer to you, as [`awaiting`] decides;
/// with `reactions` off, reactions don't count as answers.
fn waiting_on<'a>(
    thread: &'a Thread,
    me: &str,
    author: Option<&'a str>,
    reactions: bool,
) -> Option<&'a str> {
    if thread.resolved {
        return None;
    }
    let mine = |c: &&Comment| is_login(&c.author, me);
    let said = thread
        .comments
        .iter()
        .filter(mine)
        .map(|c| c.created_at.as_str())
        .max()?;
    let whom = match author {
        Some(author) => author,
        None => thread
            .comments
            .iter()
            .rev()
            .find(|c| !is_login(&c.author, me) && !c.by_bot)?
            .author
            .as_str(),
    };
    let replied = thread
        .comments
        .iter()
        .filter(|c| is_login(&c.author, whom))
        .map(|c| c.created_at.as_str());
    let reacted = thread
        .comments
        .iter()
        .filter(mine)
        .flat_map(|c| &c.reactions)
        .filter(|r| reactions && is_login(&r.login, whom))
        .map(|r| r.at.as_str());
    let mut answers = replied.chain(reacted);
    (!answers.any(|at| at >= said)).then_some(whom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::{CONVERSATION_THREAD, Comment, Placement, Reaction};

    fn comment(author: &str, at: &str) -> Comment {
        Comment {
            id: format!("{author}-{at}"),
            author: author.into(),
            body: String::new(),
            created_at: format!("2026-01-01T00:00:{at}Z"),
            url: None,
            by_bot: false,
            reacted_at: None,
            reactions: vec![],
        }
    }

    fn thread(comments: Vec<Comment>) -> Thread {
        Thread {
            id: "t".into(),
            path: None,
            line: None,
            resolved: false,
            place: Placement::default(),
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
            mine: false,
        };
        assert_eq!(both.status(), "approved · ci failing · 2 unanswered");
        assert_eq!(both.urgency(), Urgency::Act);
        assert_eq!(both.fitted(40), "approved · ci failing · 2 unanswered");
        assert_eq!(both.fitted(16), "approved · 2 new");
        assert_eq!(both.fitted(10), "2 new");
        let changes = PrState {
            approval: Approval::ChangesRequested,
            unanswered: 1,
            mine: false,
        };
        assert_eq!(changes.status(), "changes requested · 1 unanswered");
        assert_eq!(changes.fitted(15), "changes · 1 new");
        let quiet = PrState {
            approval: Approval::None,
            unanswered: 0,
            mine: false,
        };
        assert_eq!(quiet.fitted(10), "—");
        assert!(quiet.is_blank());
        assert!(
            PrState {
                mine: true,
                ..quiet
            }
            .is_blank()
        );
        assert!(!changes.is_blank());
        assert_eq!(
            PrState {
                approval: Approval::Mergeable,
                unanswered: 0,
                mine: false,
            }
            .urgency(),
            Urgency::Good
        );
    }

    #[test]
    fn merge_says_why_an_approval_would_wait() {
        let block = |merge, checks| Merge::new(merge, checks).block;
        assert_eq!(block(Some("CLEAN"), Some("SUCCESS")), None);
        assert_eq!(
            block(Some("DIRTY"), Some("FAILURE")),
            Some(Block::Conflicts)
        );
        assert_eq!(
            block(Some("BLOCKED"), Some("FAILURE")),
            Some(Block::CiFailing)
        );
        assert_eq!(
            block(Some("UNSTABLE"), Some("SUCCESS")),
            Some(Block::CiFailing)
        );
        assert_eq!(
            block(Some("BLOCKED"), Some("PENDING")),
            Some(Block::CiPending)
        );
        assert_eq!(
            block(Some("UNSTABLE"), Some("PENDING")),
            Some(Block::CiPending)
        );
        assert_eq!(block(Some("BEHIND"), Some("SUCCESS")), Some(Block::Behind));
        assert_eq!(
            block(Some("BLOCKED"), Some("SUCCESS")),
            Some(Block::Blocked)
        );
        // CI is said whether or not anyone approved.
        assert_eq!(Merge::new(None, Some("ERROR")).ci, Checks::Failing);
    }

    fn reaction(login: &str, at: &str) -> Reaction {
        Reaction {
            login: login.into(),
            at: format!("2026-01-01T00:00:{at}Z"),
        }
    }

    #[test]
    fn your_comments_wait_on_the_author_until_they_answer() {
        let waits = |threads: &[Thread]| awaiting(threads, "Me", Some("alice"));
        // Your comment, then nothing from Alice; Bob's reply isn't hers.
        let asked = thread(vec![comment("me", "01"), comment("bob", "02")]);
        assert_eq!(
            waits(std::slice::from_ref(&asked)),
            [Awaiting {
                login: "alice".into(),
                threads: 1,
            }]
        );
        // She replied, or reacted to your comment.
        let replied = thread(vec![comment("me", "01"), comment("Alice", "02")]);
        let mut reacted = asked.clone();
        reacted.comments[0].reactions = vec![reaction("alice", "03")];
        assert!(waits(&[replied, reacted.clone()]).is_empty());
        // A reaction to Bob's comment, or from before your latest, doesn't.
        let mut elsewhere = asked.clone();
        elsewhere.comments[1].reactions = vec![reaction("alice", "03")];
        reacted.comments.push(comment("me", "04"));
        assert_eq!(waits(&[elsewhere, reacted])[0].threads, 2);
        // Resolved threads, and threads you're not in, wait on nobody.
        let mut resolved = asked.clone();
        resolved.resolved = true;
        let theirs = thread(vec![comment("bob", "01")]);
        assert!(waits(&[resolved, theirs]).is_empty());
    }

    #[test]
    fn only_your_comments_awaiting_an_answer_want_their_reactions() {
        let mut asked = thread(vec![comment("me", "01"), comment("me", "02")]);
        asked.id = "asked".into();
        let replied = thread(vec![comment("me", "01"), comment("alice", "02")]);
        let theirs = thread(vec![comment("bob", "01")]);
        // A reaction already fetched doesn't stop them being wanted again.
        let mut reacted = asked.clone();
        reacted.comments[1].reactions = vec![reaction("alice", "03")];
        let threads = [asked, replied, theirs, reacted];
        assert_eq!(
            reactions_wanted(&threads, "Me", Some("alice")),
            ["me-01", "me-02", "me-01", "me-02"]
        );
    }

    #[test]
    fn on_your_pr_your_replies_wait_on_whoever_you_answered() {
        let mut bot = comment("ci[bot]", "03");
        bot.by_bot = true;
        let threads = [
            thread(vec![comment("erin", "01"), comment("me", "02"), bot]),
            thread(vec![
                comment("frank", "01"),
                comment("erin", "02"),
                comment("me", "03"),
            ]),
            thread(vec![comment("frank", "01"), comment("me", "02")]),
            // Erin spoke last: that's yours to answer, not waiting.
            thread(vec![comment("me", "01"), comment("erin", "02")]),
            // Nobody to wait on.
            thread(vec![comment("me", "01")]),
        ];
        assert_eq!(
            awaiting(&threads, "me", None),
            [
                Awaiting {
                    login: "erin".into(),
                    threads: 2,
                },
                Awaiting {
                    login: "frank".into(),
                    threads: 1,
                },
            ]
        );
    }

    #[test]
    fn changes_requested_ask_you_to_act_only_on_your_own_prs() {
        let changes = |mine, unanswered| PrState {
            approval: Approval::ChangesRequested,
            unanswered,
            mine,
        };
        assert_eq!(changes(true, 0).urgency(), Urgency::Act);
        assert_eq!(changes(false, 0).urgency(), Urgency::Quiet);
        // Unanswered comments ask you to act on anyone's.
        assert_eq!(changes(false, 1).urgency(), Urgency::Act);
        // As GitHub says it, or as the reviews do.
        for decision in [Some("CHANGES_REQUESTED"), None] {
            let reviews = [ReviewFact {
                author: "me",
                state: "CHANGES_REQUESTED",
            }];
            let owed = PrState::new(&StateFacts {
                review_decision: decision,
                merge_state: None,
                checks: None,
                threads: &[],
                reviews: &reviews,
                me: "me",
                mine: false,
            });
            assert_eq!(owed.status(), "changes requested");
            assert_eq!(owed.urgency(), Urgency::Quiet, "{decision:?}");
        }
    }
}
