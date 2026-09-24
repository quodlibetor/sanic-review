//! The PR's existing review threads on its page: a summary over the
//! drafts, each thread beside the drafts it overlaps and in their diff,
//! and the rest folded away. A thread a draft was posted as is that
//! draft's posted form, and an existing thread to every other draft.

use std::collections::{HashMap, HashSet};

use maud::{Markup, html};
use sanic_core::{
    pr::{CONVERSATION_THREAD, Comment, PrKey, Thread, is_login},
    run::Side,
};
use sanic_runner::diff::DiffIndex;
use sanic_store::{DraftRow, PostedDraft, Store, ThreadChoice};

use crate::{links::At, markdown, pr_href};

/// How much of a comment's body an excerpt shows.
const EXCERPT: usize = 160;

/// The review threads a run's drafts are shown with, and the head their
/// lines are compared on: the run's.
#[derive(Debug, Clone, Copy)]
pub struct Existing<'a> {
    pub key: &'a PrKey,
    pub threads: &'a [Thread],
    pub head: &'a str,
    /// The run whose drafts are shown.
    pub run: i64,
    /// The PR's head as last polled, for links to GitHub.
    pub pr_head: &'a str,
    /// The run's diff, for the lines a suggestion in a thread replaces.
    pub diff: Option<&'a DiffIndex>,
    /// The login the page is for, whose threads are labelled yours.
    pub me: &'a str,
    pub posted: &'a Posted,
}

/// The first comment of `thread`, if it's an inline thread.
fn first(thread: &Thread) -> Option<&Comment> {
    thread
        .comments
        .first()
        .filter(|_| thread.id != CONVERSATION_THREAD)
}

/// `key`'s threads as last polled, and which of them its drafts were
/// posted as.
pub fn load(store: &Store, key: &PrKey, me: &str) -> color_eyre::Result<(Vec<Thread>, Posted)> {
    let threads = store.threads(key)?;
    let posted = Posted::of(&threads, &store.posted_drafts(key)?, me);
    Ok((threads, posted))
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
        let spot = |d: &PostedDraft| span(d.side.as_deref(), d.start_line, d.line);
        for draft in drafts.iter().filter(|d| d.comment.is_none()) {
            let thread = threads.iter().find(|t| {
                !posted.drafts.contains_key(&t.id)
                    && t.path.as_deref() == Some(draft.path.as_str())
                    && first(t).is_some_and(|c| is_login(&c.author, me) && c.body == draft.body)
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
    pub fn draft_of(&self, thread: &Thread) -> Option<&PostedDraft> {
        self.drafts.get(&thread.id)
    }

    /// The id of the thread draft `id` was posted as, if it was.
    pub fn thread_of(&self, id: i64) -> Option<&str> {
        self.threads.get(&id).map(String::as_str)
    }

    /// Whether a draft of `run`, or a copy in it, was posted as `thread`.
    fn posted_in_run(&self, run: i64, thread: &Thread) -> bool {
        self.runs
            .get(&thread.id)
            .is_some_and(|runs| runs.contains(&run))
    }
}

impl<'a> Existing<'a> {
    /// Where links to the threads' lines point.
    pub fn at(self) -> At<'a> {
        At {
            key: self.key,
            reviewed: self.head,
            current: self.pr_head,
        }
    }

    /// The PR's inline review threads with comments, as the diff shows
    /// them: not the ones the run's drafts were posted as, which are with
    /// those drafts. The conversation isn't on any lines.
    pub fn inline(self) -> impl Iterator<Item = &'a Thread> {
        self.with_comments()
            .filter(move |t| !self.posted.posted_in_run(self.run, t))
    }

    /// The PR's inline review threads with comments, posted from here or
    /// not.
    pub fn with_comments(self) -> impl Iterator<Item = &'a Thread> {
        self.threads
            .iter()
            .filter(|t| t.id != CONVERSATION_THREAD && !t.comments.is_empty())
    }

    /// The thread draft `id` was posted as, if it's on the PR.
    pub fn posted_as(self, id: i64) -> Option<&'a Thread> {
        let thread = self.posted.thread_of(id)?;
        self.with_comments().find(|t| t.id == thread)
    }

    /// Whether `thread` was started by you, and if so, whether from here.
    pub fn mine(self, thread: &Thread) -> Mine {
        if let Some(from) = self.posted.draft_of(thread) {
            return Mine::PostedFrom {
                run: from.run_id,
                draft: from.id,
            };
        }
        let yours = thread
            .comments
            .first()
            .is_some_and(|c| is_login(&c.author, self.me));
        if yours { Mine::Yours } else { Mine::Theirs }
    }

    /// `thread` in full, labelled if it's yours; see [`thread_box_with`].
    pub fn thread_box(self, thread: &Thread) -> Markup {
        thread_box_with(
            self.at(),
            self.diff,
            thread,
            self.mine(thread),
            false,
            &html! {},
        )
    }

    /// The threads `draft` overlaps; see [`Thread::overlaps`]. Those you
    /// posted from here count: only a posted draft has a thread as its
    /// posted form. A draft that's posted or rejected overlaps none: it
    /// won't be posted again.
    pub fn overlapping(self, draft: &DraftRow) -> Vec<&'a Thread> {
        if matches!(draft.status.as_str(), "posted" | "rejected") {
            return Vec::new();
        }
        let Some((path, side, lines)) = lines(draft) else {
            return Vec::new();
        };
        self.with_comments()
            .filter(|t| t.overlaps(path, side, lines, self.head))
            .collect()
    }

    /// Threads on `path` whose last line on the head is `line` of `side`,
    /// for showing in place in a diff.
    pub fn ending_at(self, path: &str, side: Side, line: u32) -> Vec<&'a Thread> {
        self.inline()
            .filter(|t| {
                t.path.as_deref() == Some(path)
                    && t.place
                        .lines_at(t.line, self.head)
                        .is_some_and(|(s, _, last)| s == side && last == line)
            })
            .collect()
    }
}

/// Who started a thread, as its heading says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mine {
    Theirs,
    /// You, on GitHub.
    Yours,
    /// You, posting draft `draft` of run `run` from here.
    PostedFrom {
        run: i64,
        draft: i64,
    },
}

/// `draft`'s path, side and first and last lines, if it's on lines.
pub fn lines(draft: &DraftRow) -> Option<(&str, Side, (u32, u32))> {
    let (path, line) = (draft.path.as_deref()?, draft.line?);
    let (side, start, line) = span(draft.side.as_deref(), draft.start_line, line);
    Some((path, side, (start, line)))
}

/// A draft's side, as the store has it, and its first and last lines.
fn span(side: Option<&str>, start: Option<u32>, line: u32) -> (Side, u32, u32) {
    let side = if side == Some("LEFT") {
        Side::Left
    } else {
        Side::Right
    };
    (side, start.unwrap_or(line).min(line), line)
}

/// The summary over the drafts: how many existing threads the PR has and
/// how many overlap a draft, with the ones that don't folded under it,
/// and how many of the rest the drafts shown were posted as or posted
/// in, which are with their drafts: one a draft overlaps counts as
/// overlapping. Nothing when the PR has no review threads.
pub fn summary(existing: Existing<'_>, drafts: &[DraftRow]) -> Markup {
    let overlapped: HashSet<&str> = drafts
        .iter()
        .flat_map(|d| existing.overlapping(d))
        .map(|t| t.id.as_str())
        .collect();
    // Threads the drafts shown were posted as, or a posted reply or 👍
    // went in, which their cards link to or show.
    let posted_here: HashSet<&str> = drafts
        .iter()
        .filter(|d| d.status == "posted")
        .filter_map(|d| {
            existing
                .posted
                .thread_of(d.id)
                .or_else(|| d.choice.as_ref().map(ThreadChoice::thread))
        })
        .collect();
    let (mut overlapping, mut rest, mut posted) = (Vec::new(), Vec::new(), Vec::new());
    for thread in existing.with_comments() {
        if overlapped.contains(thread.id.as_str()) {
            overlapping.push(thread);
        } else if posted_here.contains(thread.id.as_str()) {
            posted.push(thread);
        } else {
            rest.push(thread);
        }
    }
    let threads = overlapping.len() + rest.len();
    if threads == 0 && posted.is_empty() {
        return html! {};
    }
    let resolved = overlapping
        .iter()
        .chain(&rest)
        .filter(|t| t.resolved)
        .count();
    html! {
        div.threads-bar #existing {
            span.lead { "💬" }
            span {
                @if threads > 0 {
                    b { (threads) } @if threads == 1 { " existing review thread" } @else { " existing review threads" }
                    " · "
                    @if overlapping.is_empty() { b { "0" } } @else { b.hot { (overlapping.len()) } }
                    " overlap" @if overlapping.len() == 1 { "s" } " your drafts"
                    @if resolved > 0 { " · " (resolved) " resolved" }
                    @if !posted.is_empty() { " · " }
                }
                @if !posted.is_empty() {
                    b { (posted.len()) } " posted from here"
                }
            }
            @if !overlapping.is_empty() {
                span.dim { "Each is shown beside the drafts it overlaps." }
            }
            @if !rest.is_empty() {
                details.threads-rest {
                    summary {
                        (rest.len()) @if rest.len() == 1 { " thread doesn't" } @else { " threads don't" }
                        " overlap a draft"
                    }
                    @for thread in rest { (existing.thread_box(thread)) }
                }
            }
        }
    }
}

/// A thread in full: where it is, its state, a link, and each comment,
/// its suggestions against the lines `diff` has.
pub fn thread_box(at: At<'_>, diff: Option<&DiffIndex>, thread: &Thread) -> Markup {
    thread_box_with(at, diff, thread, Mine::Theirs, false, &html! {})
}

/// [`thread_box`], labelled with whose it is, marked `chosen` if a draft
/// posts in it, with `actions` under its comments.
pub fn thread_box_with(
    at: At<'_>,
    diff: Option<&DiffIndex>,
    thread: &Thread,
    mine: Mine,
    chosen: bool,
    actions: &Markup,
) -> Markup {
    let cx = markdown::Context::thread(diff, thread, at.reviewed);
    html! {
        div.thread.resolved[thread.resolved].chosen[chosen] {
            (thread_head(at, thread, mine))
            ul.said {
                @for comment in &thread.comments {
                    li { b { (comment.author) } (markdown::render(&comment.body, &cx)) }
                }
            }
            (actions)
        }
    }
}

/// A thread's heading: where it is, whether it's resolved or outdated,
/// whether it's yours, and its link on GitHub. Its lines on the reviewed
/// head link to them there; see [`At::lines`].
pub fn thread_head(at: At<'_>, thread: &Thread, mine: Mine) -> Markup {
    let head = at.reviewed;
    let path = thread.path.as_deref().unwrap_or("?");
    let place = &thread.place;
    let range = |side: Option<Side>, first: Option<u32>, last: u32| {
        let old = if side == Some(Side::Left) {
            " (old)"
        } else {
            ""
        };
        match first {
            Some(first) if first != last => format!(":{first}-{last}{old}"),
            _ => format!(":{last}{old}"),
        }
    };
    // GitHub's lines when they aren't the reviewed head's, flagged.
    let on_head = place.lines_at(thread.line, head);
    let (lines, elsewhere) = match on_head {
        Some((side, first, last)) => (Some(range(Some(side), Some(first), last)), false),
        None => match (thread.line, place.original_line) {
            (Some(line), _) if !place.outdated => {
                (Some(range(place.side, place.start_line, line)), true)
            }
            (_, Some(line)) => (
                Some(range(place.side, place.original_start_line, line)),
                true,
            ),
            _ => (None, false),
        },
    };
    let count = thread.comments.len();
    html! {
        div.th {
            @if let Some(url) = on_head
                .filter(|_| thread.path.is_some())
                .and_then(|(side, first, last)| at.lines(path, side, (first, last)))
            {
                a.anc href=(url)
                    title="On GitHub, at the reviewed commit" {
                    (path) @if let Some(lines) = &lines { (lines) }
                }
            } @else {
                span.anc { (path) @if let Some(lines) = &lines { (lines) } }
            }
            @if elsewhere {
                span.dim title="Lines of another commit than the one reviewed, so they aren't compared with the drafts'." {
                    "on another commit"
                }
            }
            @match mine {
                Mine::Theirs => {}
                Mine::Yours => {
                    span.chip.dim title="You started it on GitHub." { "yours" }
                }
                Mine::PostedFrom { run, draft } => {
                    a.chip.dim href={ (pr_href(at.key)) "?run=" (run) "#draft-" (draft) }
                        title={ "You posted it from here, as draft " (draft) "." } {
                        "you posted this from run " (run)
                    }
                }
            }
            @if thread.resolved { span.chip.dim { "resolved" } }
            @if thread.place.outdated { span.chip.dim { "outdated" } }
            span.dim {
                (count) @if count == 1 { " comment" } @else { " comments" }
            }
            @if let Some(url) = link(thread) {
                a.gh href=(url) { "on GitHub ↗" }
            }
        }
    }
}

/// Who said what, shortened, with all of it on hover.
pub fn said(comment: &Comment) -> Markup {
    html! {
        b { (comment.author) } ": "
        span.excerpt title=(comment.body) { (excerpt(&comment.body)) }
    }
}

/// A thread's page on GitHub: its first comment's, if GitHub gave one.
pub fn link(thread: &Thread) -> Option<&str> {
    thread
        .comments
        .first()
        .and_then(|c| c.url.as_deref())
        .filter(|url| url.starts_with("https://"))
}

/// `body` on one line, cut at [`EXCERPT`] characters.
pub fn excerpt(body: &str) -> String {
    excerpt_of(body, EXCERPT)
}

/// `body` on one line, cut at `max` characters.
pub fn excerpt_of(body: &str, max: usize) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use sanic_core::pr::Placement;

    use super::*;

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

    #[test]
    fn excerpts_are_one_line_and_cut_short() {
        assert_eq!(excerpt("why\n\n  this?"), "why this?");
        let long = "word ".repeat(100);
        let cut = excerpt(&long);
        assert!(cut.ends_with("word…"), "{cut}");
        assert!(cut.chars().count() <= EXCERPT + 1);
    }
}
