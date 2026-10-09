//! The PR's existing review threads on its page: a summary over the
//! drafts, each thread beside the drafts it overlaps and in their diff,
//! and the rest folded away. A thread a draft was posted as is that
//! draft's posted form, and an existing thread to every other draft.

use std::collections::HashSet;

use maud::{Markup, html};
use sanic_core::{
    pr::{CONVERSATION_THREAD, Comment, PrKey, Thread, is_login},
    run::Side,
    state::is_unanswered,
};
use sanic_runner::diff::DiffIndex;
use sanic_store::{DraftRow, Posted, Store, ThreadChoice, draft_span};

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
    /// The run whose drafts are shown; `None` when the PR has no review to
    /// show its threads beside, so nothing was posted from here.
    pub run: Option<i64>,
    /// The PR's head as last polled, for links to GitHub.
    pub pr_head: &'a str,
    /// The run's diff, for the lines a suggestion in a thread replaces.
    pub diff: Option<&'a DiffIndex>,
    /// The login the page is for, whose threads are labelled yours.
    pub me: &'a str,
    /// Your own PR, where any thread can wait on you; see
    /// [`sanic_core::state::is_unanswered`].
    pub own_pr: bool,
    pub posted: &'a Posted,
}

/// `key`'s threads as last polled, and which of them its drafts were
/// posted as.
pub fn load(store: &Store, key: &PrKey, me: &str) -> color_eyre::Result<(Vec<Thread>, Posted)> {
    let threads = store.threads(key)?;
    let posted = Posted::of(&threads, &store.posted_drafts(key)?, me);
    Ok((threads, posted))
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
        self.with_comments().filter(move |t| {
            self.run
                .is_none_or(|run| !self.posted.posted_in_run(run, t))
        })
    }

    /// The PR's inline review threads with comments, posted from here or
    /// not.
    pub fn with_comments(self) -> impl Iterator<Item = &'a Thread> {
        self.threads
            .iter()
            .filter(|t| t.id != CONVERSATION_THREAD && !t.comments.is_empty())
    }

    /// [`Self::with_comments`] and the conversation, which is on no lines
    /// but is a thread the status line counts.
    pub fn every_thread(self) -> impl Iterator<Item = &'a Thread> {
        self.threads.iter().filter(|t| !t.comments.is_empty())
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

    /// Whether `thread` waits on your answer, by the same rule as the
    /// status line's count.
    pub fn waiting(self, thread: &Thread) -> bool {
        is_unanswered(thread, self.me, self.own_pr)
    }

    /// `thread` in full, labelled if it's yours; see [`thread_box_with`].
    pub fn thread_box(self, thread: &Thread) -> Markup {
        thread_box_with(
            self.at(),
            self.diff,
            thread,
            self.mine(thread),
            Marks {
                chosen: false,
                waiting: self.waiting(thread),
            },
            &html! {},
            &html! {},
        )
    }

    /// The threads `draft` overlaps; see [`Thread::overlaps`]. Those you
    /// posted from here count: only a posted draft has a thread as its
    /// posted form. A draft that's posted, rejected or dismissed overlaps
    /// none: it won't be posted again, or as it is.
    pub fn overlapping(self, draft: &DraftRow) -> Vec<&'a Thread> {
        if matches!(draft.status.as_str(), "posted" | "rejected" | "dismissed") {
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

/// The commit `thread`'s lines are lines of, with its path, side and
/// first and last lines there: the head it was fetched at, or, once
/// outdated, the commit it was left on. `None` for a thread on no lines.
pub fn placed(thread: &Thread) -> Option<(&str, &str, Side, (u32, u32))> {
    let path = thread.path.as_deref()?;
    [
        thread.place.head.as_deref(),
        thread.place.original_commit.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(|commit| {
        let (side, first, last) = thread.place.lines_at(thread.line, commit)?;
        Some((commit, path, side, (first, last)))
    })
}

/// `draft`'s path, side and first and last lines, if it's on lines.
pub fn lines(draft: &DraftRow) -> Option<(&str, Side, (u32, u32))> {
    let (path, line) = (draft.path.as_deref()?, draft.line?);
    let (side, start, line) = draft_span(draft.side.as_deref(), draft.start_line, line);
    Some((path, side, (start, line)))
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
                a.threads-rest href="#threads" {
                    (rest.len()) @if rest.len() == 1 { " thread doesn't" } @else { " threads don't" }
                    " overlap a draft"
                }
            }
        }
    }
}

/// How a thread box is marked out from the others.
#[derive(Debug, Clone, Copy, Default)]
pub struct Marks {
    /// A draft posts in it.
    pub chosen: bool,
    /// It waits on your answer.
    pub waiting: bool,
}

/// Every review thread the PR has, whatever the drafts do: the ones
/// waiting on your answer first, then the rest, with the resolved ones
/// folded away. `ask` gives a thread its "Agent…" link, where there's a
/// review to revise.
pub fn section(
    existing: Existing<'_>,
    drafts: &[DraftRow],
    code: &Code<'_>,
    ask: impl Fn(&Thread) -> Markup,
) -> Markup {
    // Shown in full beside their drafts already, so here they're a line
    // each: the same thread twice on one page reads as two.
    let above: HashSet<&str> = drafts
        .iter()
        .flat_map(|d| existing.overlapping(d))
        .map(|t| t.id.as_str())
        .collect();
    let (mut waiting, mut open, mut resolved) = (Vec::new(), Vec::new(), Vec::new());
    for thread in existing.every_thread() {
        if thread.resolved {
            resolved.push(thread);
        } else if existing.waiting(thread) {
            waiting.push(thread);
        } else {
            open.push(thread);
        }
    }
    if waiting.is_empty() && open.is_empty() && resolved.is_empty() {
        return html! {};
    }
    let box_of = |thread: &Thread| {
        let waiting = existing.waiting(thread);
        if above.contains(thread.id.as_str()) {
            return html! {
                div.thread.waiting[waiting] {
                    (thread_head_with(existing.at(), thread, existing.mine(thread), waiting))
                    p.dim { "Shown with your drafts above." }
                }
            };
        }
        thread_box_with(
            existing.at(),
            existing.diff,
            thread,
            existing.mine(thread),
            Marks {
                chosen: false,
                waiting,
            },
            &code.of(thread),
            &ask(thread),
        )
    };
    html! {
        section #threads {
            h2 {
                "Review threads"
                @if !waiting.is_empty() {
                    " · " span.hot { (waiting.len()) " waiting on you" }
                }
            }
            @for thread in waiting.iter().chain(&open) { (box_of(thread)) }
            @if !resolved.is_empty() {
                details.threads-resolved {
                    summary {
                        (resolved.len())
                        @if resolved.len() == 1 { " resolved thread" } @else { " resolved threads" }
                    }
                    @for thread in &resolved { (box_of(thread)) }
                }
            }
        }
    }
}

/// A thread in full: where it is, its state, a link, and each comment,
/// its suggestions against the lines `diff` has.
pub fn thread_box(at: At<'_>, diff: Option<&DiffIndex>, thread: &Thread) -> Markup {
    thread_box_with(
        at,
        diff,
        thread,
        Mine::Theirs,
        Marks::default(),
        &html! {},
        &html! {},
    )
}

/// [`thread_box`], labelled with whose it is, marked as `marks` says,
/// with `above` between its heading and its comments, where the code it
/// sits on goes, and `actions` under them.
pub fn thread_box_with(
    at: At<'_>,
    diff: Option<&DiffIndex>,
    thread: &Thread,
    mine: Mine,
    marks: Marks,
    above: &Markup,
    actions: &Markup,
) -> Markup {
    let cx = markdown::Context::thread(diff, thread, at.reviewed);
    html! {
        div.thread.resolved[thread.resolved].chosen[marks.chosen].waiting[marks.waiting] {
            (thread_head_with(at, thread, mine, marks.waiting))
            (above)
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
    thread_head_with(at, thread, mine, false)
}

/// [`thread_head`], flagged when the thread waits on your answer.
pub fn thread_head_with(at: At<'_>, thread: &Thread, mine: Mine, waiting: bool) -> Markup {
    let head = at.reviewed;
    let conversation = thread.id == CONVERSATION_THREAD;
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
            @if conversation {
                span.anc { "the conversation" }
            } @else if let Some(url) = on_head
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
            @if waiting {
                span.chip.hot title="Someone's comment here is newer than your last answer." {
                    "waiting on you"
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

/// An instruction asking the agent to answer `thread`, for the Agent
/// card's box to start with. It's a starting point, not a form: you edit
/// it before the agent sees it.
pub fn answer_prompt(thread: &Thread) -> String {
    use std::fmt::Write;
    let mut out = String::from("Draft an answer to this review thread");
    if let Some(path) = &thread.path {
        out.push_str(" on ");
        out.push_str(path);
        if let Some(line) = thread.line {
            let _ = write!(out, ":{line}");
        }
    }
    out.push_str(".\n\n");
    for comment in &thread.comments {
        let _ = writeln!(
            out,
            "{}: {}",
            comment.author,
            excerpt_of(&comment.body, 400)
        );
    }
    out
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

/// The code each thread is on, for [`section`]: the reviewed diff where
/// it has those lines, else the hunk GitHub itself shows the thread
/// against, which needs no local checkout.
#[derive(Debug, Clone, Copy)]
pub struct Code<'a> {
    existing: Existing<'a>,
}

impl<'a> Code<'a> {
    pub fn new(existing: Existing<'a>) -> Self {
        Self { existing }
    }

    fn of(&self, thread: &Thread) -> Markup {
        if let Some((commit, path, side, lines)) = placed(thread)
            && commit == self.existing.head
            && let Some(diff) = self.existing.diff
            && let Some(shown) = crate::diff::at(diff, path, side, lines, self.existing)
        {
            return shown;
        }
        let Some(hunk) = &thread.diff_hunk else {
            return html! {};
        };
        crate::diff::from_hunk(hunk, thread.line).unwrap_or_else(|| html! {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_are_one_line_and_cut_short() {
        assert_eq!(excerpt("why\n\n  this?"), "why this?");
        let long = "word ".repeat(100);
        let cut = excerpt(&long);
        assert!(cut.ends_with("word…"), "{cut}");
        assert!(cut.chars().count() <= EXCERPT + 1);
    }
}
