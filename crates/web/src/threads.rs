//! The PR's existing review threads on its page: a summary over the
//! drafts, each thread beside the drafts it overlaps and in their diff,
//! and the rest folded away.

use maud::{Markup, html};
use sanic_core::{
    pr::{CONVERSATION_THREAD, Comment, PrKey, Thread},
    run::Side,
};
use sanic_store::DraftRow;

use crate::links::At;

/// How much of a comment's body an excerpt shows.
const EXCERPT: usize = 160;

/// The review threads a run's drafts are shown with, and the head their
/// lines are compared on: the run's.
#[derive(Debug, Clone, Copy)]
pub struct Existing<'a> {
    pub key: &'a PrKey,
    pub threads: &'a [Thread],
    pub head: &'a str,
    /// The PR's head as last polled, for links to GitHub.
    pub pr_head: &'a str,
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

    /// The PR's inline review threads with comments; the conversation
    /// isn't on any lines.
    pub fn inline(self) -> impl Iterator<Item = &'a Thread> {
        self.threads
            .iter()
            .filter(|t| t.id != CONVERSATION_THREAD && !t.comments.is_empty())
    }

    /// The threads `draft` overlaps; see [`Thread::overlaps`].
    pub fn overlapping(self, draft: &DraftRow) -> Vec<&'a Thread> {
        let Some((path, side, lines)) = lines(draft) else {
            return Vec::new();
        };
        self.inline()
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

/// `draft`'s path, side and first and last lines, if it's on lines.
pub fn lines(draft: &DraftRow) -> Option<(&str, Side, (u32, u32))> {
    let (path, line) = (draft.path.as_deref()?, draft.line?);
    let side = if draft.side.as_deref() == Some("LEFT") {
        Side::Left
    } else {
        Side::Right
    };
    let start = draft.start_line.unwrap_or(line).min(line);
    Some((path, side, (start, line)))
}

/// The summary over the drafts: how many threads the PR has and how many
/// overlap a draft, with the ones that don't folded under it. Nothing
/// when the PR has no review threads.
pub fn summary(existing: Existing<'_>, drafts: &[DraftRow]) -> Markup {
    let all: Vec<&Thread> = existing.inline().collect();
    if all.is_empty() {
        return html! {};
    }
    // A rejected draft folds without its threads, so one only it overlaps
    // stays with the rest.
    let overlapped = |t: &Thread| {
        drafts
            .iter()
            .filter(|d| d.status != "rejected")
            .any(|d| existing.overlapping(d).iter().any(|o| o.id == t.id))
    };
    let (overlapping, rest): (Vec<&Thread>, Vec<&Thread>) =
        all.iter().copied().partition(|t| overlapped(t));
    let resolved = all.iter().filter(|t| t.resolved).count();
    let threads = all.len();
    html! {
        div.threads-bar #existing {
            span.lead { "💬" }
            span {
                b { (threads) } @if threads == 1 { " existing review thread" } @else { " existing review threads" }
                " · "
                @if overlapping.is_empty() { b { "0" } } @else { b.hot { (overlapping.len()) } }
                " overlap" @if overlapping.len() == 1 { "s" } " your drafts"
                @if resolved > 0 { " · " (resolved) " resolved" }
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
                    @for thread in rest { (thread_box(existing.at(), thread)) }
                }
            }
        }
    }
}

/// A thread in full: where it is, its state, a link, and an excerpt of
/// each comment.
pub fn thread_box(at: At<'_>, thread: &Thread) -> Markup {
    thread_box_with(at, thread, false, &html! {})
}

/// [`thread_box`], marked `chosen` if a draft posts in it, with `actions`
/// under its comments.
pub fn thread_box_with(at: At<'_>, thread: &Thread, chosen: bool, actions: &Markup) -> Markup {
    html! {
        div.thread.resolved[thread.resolved].chosen[chosen] {
            (thread_head(at, thread))
            ul.said {
                @for comment in &thread.comments { li { (said(comment)) } }
            }
            (actions)
        }
    }
}

/// A thread's heading: where it is, whether it's resolved or outdated,
/// and its link on GitHub. Its lines on the reviewed head link to them
/// there; see [`At::lines`].
pub fn thread_head(at: At<'_>, thread: &Thread) -> Markup {
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
fn link(thread: &Thread) -> Option<&str> {
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
