//! Which of a PR's review threads its drafts were posted as, for the
//! dashboard, which shows such a thread as its draft's, and for the brief,
//! which labels its comment as an earlier draft.

use std::collections::{HashMap, HashSet};

use sanic_core::{
    pr::{CONVERSATION_THREAD, Comment, Thread, is_login},
    run::Side,
};

use crate::PostedDraft;

/// The first comment of `thread`, if it's an inline thread.
fn first(thread: &Thread) -> Option<&Comment> {
    thread
        .comments
        .first()
        .filter(|_| thread.id != CONVERSATION_THREAD)
}

/// Whether a comment's `body`, as GitHub has it, is `draft`'s text: the
/// same but for line endings, which GitHub may give as CRLF, and space at
/// either end.
#[must_use]
pub fn same_body(body: &str, draft: &str) -> bool {
    body.replace("\r\n", "\n").trim() == draft.replace("\r\n", "\n").trim()
}

/// A draft's side, as the store has it, and its first and last lines,
/// as a thread's `lines_at` says them.
#[must_use]
pub fn draft_span(side: Option<&str>, start: Option<u32>, line: u32) -> (Side, u32, u32) {
    let side = if side == Some("LEFT") {
        Side::Left
    } else {
        Side::Right
    };
    (side, start.unwrap_or(line).min(line), line)
}

/// The threads drafts of the PR were posted as.
#[derive(Debug, Default)]
pub struct Posted {
    /// The draft each thread was posted as, by thread id.
    drafts: HashMap<String, PostedDraft>,
    /// The thread each draft was posted as, its copies' too, by draft id.
    threads: HashMap<i64, String>,
    /// The runs of the drafts each thread was posted as, its copies' too,
    /// by thread id.
    runs: HashMap<String, HashSet<i64>>,
}

impl Posted {
    /// Which of `threads` each of `drafts` started: the thread whose first
    /// comment is the one recorded for it, or, for a draft posted before
    /// that was recorded, the one whose first comment is `me`'s, word for
    /// word the draft, on its lines of its run's head. A thread goes to one
    /// draft at most; a draft left without one that's word for word one
    /// with a thread, on its lines of its head, is a copy of it (a
    /// revision's, or one marked posted with it) and shares its thread.
    #[must_use]
    pub fn of(threads: &[Thread], drafts: &[PostedDraft], me: &str) -> Self {
        let mut posted = Self::default();
        for draft in drafts {
            let Some(comment) = &draft.comment else {
                continue;
            };
            if let Some(thread) = threads
                .iter()
                .find(|t| first(t).is_some_and(|c| c.id == *comment))
            {
                posted.insert(thread, draft);
            }
        }
        let spot = |d: &PostedDraft| draft_span(d.side.as_deref(), d.start_line, d.line);
        for draft in drafts.iter().filter(|d| d.comment.is_none()) {
            let thread = threads.iter().find(|t| {
                !posted.drafts.contains_key(&t.id)
                    && t.path.as_deref() == Some(draft.path.as_str())
                    && first(t)
                        .is_some_and(|c| is_login(&c.author, me) && same_body(&c.body, &draft.body))
                    && t.place.lines_at(t.line, &draft.head) == Some(spot(draft))
            });
            if let Some(thread) = thread {
                posted.insert(thread, draft);
            }
        }
        for draft in drafts.iter().filter(|d| d.comment.is_none()) {
            if posted.threads.contains_key(&draft.id) {
                continue;
            }
            let copied = threads.iter().find(|t| {
                posted.drafts.get(&t.id).is_some_and(|d| {
                    (&d.head, &d.path, spot(d), &d.body)
                        == (&draft.head, &draft.path, spot(draft), &draft.body)
                })
            });
            if let Some(thread) = copied {
                posted.share(thread, draft);
            }
        }
        posted
    }

    fn insert(&mut self, thread: &Thread, draft: &PostedDraft) {
        self.drafts.insert(thread.id.clone(), draft.clone());
        self.share(thread, draft);
    }

    /// Gives `draft` `thread`, which it was posted as or is a copy of.
    fn share(&mut self, thread: &Thread, draft: &PostedDraft) {
        self.threads.insert(draft.id, thread.id.clone());
        self.runs
            .entry(thread.id.clone())
            .or_default()
            .insert(draft.run_id);
    }

    /// The draft `thread` was posted as, if one was.
    #[must_use]
    pub fn draft_of(&self, thread: &Thread) -> Option<&PostedDraft> {
        self.drafts.get(&thread.id)
    }

    /// The id of the thread draft `id` was posted as, if it was.
    #[must_use]
    pub fn thread_of(&self, id: i64) -> Option<&str> {
        self.threads.get(&id).map(String::as_str)
    }

    /// Whether a draft of `run`, or a copy in it, was posted as `thread`.
    #[must_use]
    pub fn posted_in_run(&self, run: i64, thread: &Thread) -> bool {
        self.runs
            .get(&thread.id)
            .is_some_and(|runs| runs.contains(&run))
    }
}

#[cfg(test)]
mod tests {
    use sanic_core::pr::Placement;

    use super::*;

    #[test]
    fn bodies_match_whatever_their_line_endings_and_outer_space() {
        assert!(same_body("Why?\r\nReally.\r\n", "Why?\nReally."));
        assert!(same_body(" Why? ", "Why?"));
        assert!(!same_body("Why?\n\nReally.", "Why?\nReally."));
    }

    fn thread(id: &str, start: Option<u32>, line: u32, author: &str, body: &str) -> Thread {
        Thread {
            id: id.into(),
            path: Some("src/a.rs".into()),
            line: Some(line),
            resolved: false,
            place: Placement {
                start_line: start,
                side: Some(Side::Right),
                head: Some("h2".into()),
                ..Placement::default()
            },
            comments: vec![Comment {
                id: format!("{id}-c1"),
                author: author.into(),
                body: body.into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                url: None,
                by_bot: false,
                reacted_at: None,
                reactions: vec![],
            }],
        }
    }

    fn posted(id: i64, start: Option<u32>, line: u32, comment: Option<&str>) -> PostedDraft {
        PostedDraft {
            id,
            run_id: 1,
            head: "h2".into(),
            path: "src/a.rs".into(),
            line,
            start_line: start,
            side: Some("RIGHT".into()),
            body: "Why?".into(),
            comment: comment.map(Into::into),
        }
    }

    #[test]
    fn a_thread_is_the_draft_that_recorded_it_or_else_yours_word_for_word() {
        let threads = [
            thread("a", None, 5, "Me", "Why?"),
            thread("b", Some(3), 5, "me", "Why?"),
            thread("c", None, 5, "bob", "Why?"),
            thread("d", None, 5, "me", "Why not?"),
        ];
        let drafts = [
            // A copy with nothing recorded comes first, and finds `a`
            // already taken by the draft that recorded it: it shares it.
            posted(1, None, 5, None),
            posted(2, None, 5, Some("a-c1")),
            posted(3, Some(3), 5, None),
        ];
        let found = Posted::of(&threads, &drafts, "me");
        let of = |i: usize| found.draft_of(&threads[i]).map(|d| d.id);
        assert_eq!([of(0), of(1), of(2), of(3)], [Some(2), Some(3), None, None]);
        assert_eq!(
            [found.thread_of(1), found.thread_of(2), found.thread_of(3)],
            [Some("a"), Some("a"), Some("b")]
        );
        // Its lines on another head aren't the draft's.
        let moved = [PostedDraft {
            head: "h1".into(),
            ..posted(4, None, 5, None)
        }];
        assert!(
            Posted::of(&threads, &moved, "me")
                .draft_of(&threads[0])
                .is_none()
        );
    }
}
