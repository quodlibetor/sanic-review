//! Drafts shown in the diff they comment on.

use maud::{Markup, html};
use sanic_core::run::Side;
use sanic_runner::diff::{DiffIndex, DiffLine};
use sanic_store::DraftRow;

use crate::threads::{self, Existing};

/// Lines shown on either side of a comment's lines.
const AROUND: usize = 3;

/// The lines around `draft`'s anchor in `diff`, its own lines marked, with
/// each existing thread under the line it ends on. `None` if the draft
/// isn't anchored in the diff.
pub fn context(diff: &DiffIndex, draft: &DraftRow, existing: Existing<'_>) -> Option<Markup> {
    let (path, line) = (draft.path.as_deref()?, draft.line?);
    let start = draft.start_line.unwrap_or(line);
    let side = if draft.side.as_deref() == Some("LEFT") {
        Side::Left
    } else {
        Side::Right
    };
    at(diff, path, side, (start, line), existing)
}

/// The lines around `start..=line` of `path` in `diff`, on `side`, marked,
/// with each existing thread under the line it ends on. `None` when the
/// diff doesn't have those lines.
pub fn at(
    diff: &DiffIndex,
    path: &str,
    side: Side,
    (start, line): (u32, u32),
    existing: Existing<'_>,
) -> Option<Markup> {
    let left = side == Side::Left;
    let number = |l: &DiffLine| if left { l.old } else { l.new };
    let lines = diff.hunks(path).iter().find_map(|hunk| {
        let first = hunk.lines.iter().position(|l| number(l) == Some(start))?;
        let last = hunk.lines.iter().rposition(|l| number(l) == Some(line))?;
        (first <= last).then(|| {
            let from = first.saturating_sub(AROUND);
            let to = (last + AROUND + 1).min(hunk.lines.len());
            (&hunk.lines[from..to], first - from..=last - from)
        })
    })?;
    let (lines, marked) = lines;
    Some(html! {
        table.diff {
            @for (i, l) in lines.iter().enumerate() {
                @let class = match (l.old, l.new) {
                    (None, _) => "add",
                    (_, None) => "del",
                    _ => "ctx",
                };
                @let sign = match class { "add" => "+", "del" => "-", _ => " " };
                tr.(class).anchor[marked.contains(&i)] {
                    td.num { (l.old.map(|n| n.to_string()).unwrap_or_default()) }
                    td.num { (l.new.map(|n| n.to_string()).unwrap_or_default()) }
                    td.code { span.sign { (sign) } (l.text) }
                }
                @let ending = [(Side::Left, l.old), (Side::Right, l.new)]
                    .into_iter()
                    .filter_map(|(side, n)| Some(existing.ending_at(path, side, n?)))
                    .flatten();
                @for thread in ending {
                    tr.thread-row.resolved[thread.resolved] {
                        td colspan="3" {
                            "💬 "
                            @if let Some(first) = thread.comments.first() { (threads::said(first)) }
                            @if thread.comments.len() > 1 {
                                span.dim { " +" (thread.comments.len() - 1) " more" }
                            }
                            @if thread.resolved { " " span.chip.dim { "resolved" } }
                        }
                    }
                }
            }
        }
    })
}

/// The starting old and new line of a `@@ -a,b +c,d @@` header, or `None`
/// if it isn't one. The counts are ignored: the body says how many.
fn hunk_start(header: &str) -> Option<(u32, u32)> {
    let inner = header.strip_prefix("@@ ")?.split(" @@").next()?;
    let (old, new) = inner.split_once(' ')?;
    let first = |part: &str, sign: char| -> Option<u32> {
        part.strip_prefix(sign)?
            .split(',')
            .next()?
            .parse::<u32>()
            .ok()
    };
    Some((first(old, '-')?, first(new, '+')?))
}

/// GitHub's own `diffHunk` for a review thread, rendered as the drafts'
/// context is, with the thread's line marked. `None` if the hunk isn't
/// one, so a thread shows no code rather than the wrong code.
pub fn from_hunk(hunk: &str, line: Option<u32>) -> Option<Markup> {
    let mut lines = hunk.lines();
    let (mut old, mut new) = hunk_start(lines.next()?)?;
    let mut rows = Vec::new();
    for text in lines {
        // GitHub sends "\ No newline at end of file" like git does.
        if text.starts_with('\\') {
            continue;
        }
        let (class, body) = match text.chars().next() {
            Some('+') => ("add", &text[1..]),
            Some('-') => ("del", &text[1..]),
            Some(' ') => ("ctx", &text[1..]),
            // An empty line is a context line GitHub trimmed.
            None => ("ctx", text),
            Some(_) => return None,
        };
        let (at_old, at_new) = match class {
            "add" => (None, Some(new)),
            "del" => (Some(old), None),
            _ => (Some(old), Some(new)),
        };
        if at_old.is_some() {
            old += 1;
        }
        if at_new.is_some() {
            new += 1;
        }
        rows.push((class, body.to_owned(), at_old, at_new));
    }
    if rows.is_empty() {
        return None;
    }
    Some(html! {
        table.diff {
            @for (class, text, at_old, at_new) in &rows {
                @let sign = match *class { "add" => "+", "del" => "-", _ => " " };
                @let marked = line.is_some() && *at_new == line;
                tr.(*class).anchor[marked] {
                    td.num { (at_old.map(|n| n.to_string()).unwrap_or_default()) }
                    td.num { (at_new.map(|n| n.to_string()).unwrap_or_default()) }
                    td.code { span.sign { (sign) } (text) }
                }
            }
        }
    })
}
