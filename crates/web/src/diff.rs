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

/// The lines around `start..=line` of a file read whole from the mirror,
/// its own lines marked. For a thread on a PR no review here has fetched
/// a diff for, where there's no hunk to show.
pub fn from_file(text: &str, (start, line): (u32, u32)) -> Option<Markup> {
    let all: Vec<&str> = text.lines().collect();
    let (first, last) = (usize::try_from(start).ok()?, usize::try_from(line).ok()?);
    // The lines are 1-based, and a thread can name a line the file no
    // longer has.
    let (first, last) = (first.checked_sub(1)?, last.checked_sub(1)?);
    if first > last || last >= all.len() {
        return None;
    }
    let from = first.saturating_sub(AROUND);
    let to = (last + AROUND + 1).min(all.len());
    Some(html! {
        table.diff.whole {
            @for (i, text) in all[from..to].iter().enumerate() {
                @let n = from + i;
                tr.ctx.anchor[(first..=last).contains(&n)] {
                    td.num { (n + 1) }
                    td.code { span.sign { " " } (text) }
                }
            }
        }
    })
}
