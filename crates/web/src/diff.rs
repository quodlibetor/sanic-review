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
    let left = draft.side.as_deref() == Some("LEFT");
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
