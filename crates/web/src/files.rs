//! The files view: the diff a run reviewed, laid out as GitHub's Files
//! changed tab, with each draft and existing thread at its lines.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, Query, State};
use maud::{Markup, html};
use sanic_core::{
    pr::{PrKey, Thread},
    run::Side,
};
use sanic_runner::diff::{Change, DiffFile, DiffIndex, DiffLine, Hunk};
use sanic_store::{DraftRow, ReviewRun};
use serde::Deserialize;

use crate::{
    App, Error, Shared,
    highlight::Highlighter,
    links,
    pr::{self, short},
    pr_href,
    submit::RunPath,
    threads::{self, Existing},
};

/// Diff lines past which a file with nothing of yours on it is only
/// loaded on request, as GitHub does.
const LAZY_LINES: usize = 400;

/// Diff lines the page shows before files with nothing of yours on them
/// are only loaded on request.
const PAGE_LINES: usize = 3000;

/// Diff lines past which a file isn't highlighted.
const HIGHLIGHT_LINES: usize = 2000;

/// Which of the PR page's two views of a run's drafts it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Each draft with the lines around it.
    #[default]
    Drafts,
    /// The whole diff, with the drafts in it.
    Files,
}

impl View {
    /// From the page's `view` parameter; anything else is the drafts.
    pub fn parse(param: Option<&str>) -> Self {
        match param {
            Some("files") => Self::Files,
            _ => Self::Drafts,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drafts => "drafts",
            Self::Files => "files",
        }
    }
}

/// What the files view shows.
pub struct Files<'a> {
    pub app: &'a App,
    pub run: &'a ReviewRun,
    /// The PR's head as last polled, which may have moved on from the
    /// run's.
    pub pr_head: &'a str,
    pub diff: &'a DiffIndex,
    pub drafts: &'a [DraftRow],
    pub existing: Existing<'a>,
}

impl Files<'_> {
    fn key(&self) -> &PrKey {
        self.existing.key
    }

    /// Where the lazy parts of the view load from.
    fn file_href(&self, path: &str) -> String {
        format!(
            "{}/runs/{}/file?{}",
            pr_href(self.key()),
            self.run.id,
            serde_urlencoded::to_string([("path", path)]).unwrap_or_default()
        )
    }
}

/// The view: which head the diff is at, the drafts that aren't on a line
/// of it (the summary first), then each file.
pub fn section(f: &Files<'_>) -> Markup {
    let placed = placed(f.diff, f.drafts);
    let (adds, dels) = f.diff.files().iter().fold((0, 0), |(a, d), file| {
        let (fa, fd) = file.stats();
        (a + fa, d + fd)
    });
    // Files with a draft or thread on them always show; the rest while
    // they're small and the page has room.
    let mut budget = PAGE_LINES;
    let eager: Vec<bool> = f
        .diff
        .files()
        .iter()
        .map(|file| {
            let size = size(file);
            let eager = has_yours(f, file, &placed) || (size <= LAZY_LINES && size <= budget);
            if eager {
                budget = budget.saturating_sub(size);
            }
            eager
        })
        .collect();
    html! {
        section #drafts .files {
            div.fv-head {
                span {
                    "The diff this run reviewed, at " code { (short(&f.run.head_sha)) }
                    @if f.run.head_sha != f.pr_head {
                        " · the PR has moved on to " code { (short(f.pr_head)) }
                    }
                }
                span.stats {
                    (f.diff.files().len()) @if f.diff.files().len() == 1 { " file" } @else { " files" }
                    " " span.a { "+" (adds) } " " span.d { "−" (dels) }
                }
            }
            div.fv-top {
                @for draft in f.drafts.iter().filter(|d| !placed.contains(&d.id)) {
                    (pr::draft_card(f.app, draft, Some(f.diff), f.existing))
                }
            }
            div.fv-files {
                @for (file, eager) in f.diff.files().iter().zip(eager) {
                    (file_box(f, file, &placed, eager))
                }
            }
        }
    }
}

/// How many lines `file`'s diff has.
fn size(file: &DiffFile) -> usize {
    file.hunks.iter().map(|h| h.lines.len()).sum()
}

/// Whether a draft or an existing thread is on `file`'s lines.
fn has_yours(f: &Files<'_>, file: &DiffFile, placed: &HashSet<i64>) -> bool {
    f.drafts
        .iter()
        .any(|d| placed.contains(&d.id) && d.path.as_deref() == Some(file.path.as_str()))
        || f.existing
            .inline()
            .any(|t| t.path.as_deref() == Some(file.path.as_str()) && on_head(f, t).is_some())
}

/// A thread's side and last line on the run's head.
fn on_head(f: &Files<'_>, thread: &Thread) -> Option<(Side, u32)> {
    let (side, _, last) = thread.place.lines_at(thread.line, f.existing.head)?;
    Some((side, last))
}

/// The drafts shown at a line of the diff, rather than over it: comments
/// whose last line is one of the diff's.
fn placed(diff: &DiffIndex, drafts: &[DraftRow]) -> HashSet<i64> {
    drafts
        .iter()
        .filter(|d| d.kind == "comment" && !d.unanchored)
        .filter(|d| {
            threads::lines(d).is_some_and(|(path, side, (_, last))| {
                diff.hunks(path)
                    .iter()
                    .flat_map(|h| &h.lines)
                    .any(|l| number(l, side) == Some(last))
            })
        })
        .map(|d| d.id)
        .collect()
}

fn number(line: &DiffLine, side: Side) -> Option<u32> {
    match side {
        Side::Left => line.old,
        Side::Right => line.new,
    }
}

/// One file: its header, then its diff, or a button that loads it.
fn file_box(f: &Files<'_>, file: &DiffFile, placed: &HashSet<i64>, eager: bool) -> Markup {
    let (adds, dels) = file.stats();
    let anchor = links::file_anchor(&file.path);
    let github = f
        .existing
        .at()
        .file(&file.path, file.change == Change::Deleted);
    html! {
        div.file #(anchor) data-path=(file.path) {
            div.fh {
                @match file.change {
                    Change::Added => { span.chip.fadd { "added" } }
                    Change::Deleted => { span.chip.fdel { "deleted" } }
                    Change::Modified if file.renamed_from.is_some() => { span.chip { "renamed" } }
                    Change::Modified => {}
                }
                span.fp {
                    @if let Some(from) = &file.renamed_from { span.dim { (from) " → " } }
                    (file.path)
                }
                span.stats { span.a { "+" (adds) } " " span.d { "−" (dels) } }
                @if let Some(github) = github {
                    a.gh href=(github) title="This file on GitHub" { "on GitHub ↗" }
                }
            }
            div.fb {
                @if file.binary {
                    p.note { "Binary file, not shown." }
                } @else if file.hunks.is_empty() {
                    p.note {
                        @if file.renamed_from.is_some() { "Renamed without changes." } @else { "No lines changed." }
                    }
                } @else if eager {
                    (table(f, file, placed))
                } @else {
                    div.lazy {
                        span.dim { (size(file)) " lines of diff. " }
                        button.btn type="button" hx-get=(f.file_href(&file.path))
                            hx-target="closest .fb" hx-swap="innerHTML" { "Load diff" }
                    }
                }
            }
        }
    }
}

/// `file`'s hunks, a row per line, with the drafts and threads that end on
/// a line under it.
fn table(f: &Files<'_>, file: &DiffFile, placed: &HashSet<i64>) -> Markup {
    let path = file.path.as_str();
    let mut ending: HashMap<(Side, u32), Vec<&DraftRow>> = HashMap::new();
    let mut marked: Vec<(Side, u32, u32)> = Vec::new();
    for draft in f.drafts.iter().filter(|d| placed.contains(&d.id)) {
        if let Some((p, side, (first, last))) = threads::lines(draft)
            && p == path
        {
            ending.entry((side, last)).or_default().push(draft);
            if draft.status != "rejected" {
                marked.push((side, first, last));
            }
        }
    }
    let is_marked = |side: Side, n: Option<u32>| {
        n.is_some_and(|n| {
            marked
                .iter()
                .any(|&(s, first, last)| s == side && (first..=last).contains(&n))
        })
    };
    let highlight = size(file) <= HIGHLIGHT_LINES;
    html! {
        table.d {
            @for hunk in &file.hunks {
                tr.hh { td.n colspan="2" {} td.c { (hunk_header(hunk)) } }
                @for (line, code) in hunk.lines.iter().zip(highlighted(path, hunk, highlight)) {
                    @let class = match (line.old, line.new) {
                        (None, _) => "add",
                        (_, None) => "del",
                        _ => "ctx",
                    };
                    @let sign = match class { "add" => "+", "del" => "-", _ => " " };
                    @let mark = is_marked(Side::Left, line.old) || is_marked(Side::Right, line.new);
                    tr.(class).mark[mark] {
                        td.n { (line.old.map(|n| n.to_string()).unwrap_or_default()) }
                        td.n { (line.new.map(|n| n.to_string()).unwrap_or_default()) }
                        td.c { span.sign { (sign) } (code) }
                    }
                    @let here = [(Side::Left, line.old), (Side::Right, line.new)];
                    @let threads: Vec<&Thread> = here
                        .iter()
                        .filter_map(|&(side, n)| Some(f.existing.ending_at(path, side, n?)))
                        .flatten()
                        .collect();
                    @let drafts: Vec<&DraftRow> = here
                        .iter()
                        .filter_map(|&(side, n)| ending.get(&(side, n?)))
                        .flatten()
                        .copied()
                        .collect();
                    @if !threads.is_empty() || !drafts.is_empty() {
                        tr.inl {
                            td colspan="3" {
                                @for thread in threads {
                                    (threads::thread_box(f.existing.at(), thread))
                                }
                                @for draft in drafts {
                                    (pr::draft_card(f.app, draft, Some(f.diff), f.existing))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `@@ -a,b +c,d @@`, as git wrote it.
fn hunk_header(hunk: &Hunk) -> String {
    let range = |r: &std::ops::Range<u32>| match r.end - r.start {
        1 => r.start.to_string(),
        len => format!("{},{len}", r.start),
    };
    format!("@@ -{} +{} @@", range(&hunk.old), range(&hunk.new))
}

/// Each of `hunk`'s lines as it's shown: highlighted when `highlight` and
/// syntect knows the file, each side on its own, so an old line and the
/// new one that replaces it are each read in order.
fn highlighted(path: &str, hunk: &Hunk, highlight: bool) -> Vec<Markup> {
    let fresh = || highlight.then(|| Highlighter::for_path(path)).flatten();
    let (mut old, mut new) = (fresh(), fresh());
    hunk.lines
        .iter()
        .map(|line| {
            let text = line.text.as_str();
            match (line.old, line.new, &mut old, &mut new) {
                (Some(_), None, Some(old), _) => old.line(text),
                (_, Some(_), _, Some(new)) => {
                    if line.old.is_some()
                        && let Some(old) = &mut old
                    {
                        old.line(text);
                    }
                    new.line(text)
                }
                _ => html! { (text) },
            }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    path: String,
}

/// One file's diff, for a file the view loads on request.
pub async fn file(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Query(query): Query<FileQuery>,
) -> Result<Markup, Error> {
    let key = path.pr().key()?;
    let loaded = Loaded::load(&app, &key, path.run)?;
    let existing = Existing {
        key: &key,
        threads: &loaded.threads,
        head: &loaded.run.head_sha,
        pr_head: &loaded.pr_head,
    };
    let f = Files {
        app: &app,
        run: &loaded.run,
        pr_head: &loaded.pr_head,
        diff: &loaded.diff,
        drafts: &loaded.drafts,
        existing,
    };
    let Some(file) = loaded.diff.file(&query.path) else {
        return Err(Error::NotFound(format!(
            "run {} of {} doesn't change `{}`",
            path.run,
            key.url(),
            query.path
        )));
    };
    Ok(table(&f, file, &placed(&loaded.diff, &loaded.drafts)))
}

/// A run of a PR with what its files view needs.
struct Loaded {
    run: ReviewRun,
    pr_head: String,
    drafts: Vec<DraftRow>,
    threads: Vec<Thread>,
    diff: DiffIndex,
}

impl Loaded {
    fn load(app: &App, key: &PrKey, run_id: i64) -> Result<Self, Error> {
        let missing = || Error::NotFound(format!("{} has no run {run_id}", key.url()));
        let (run, pr_head, drafts, threads) = {
            let store = app.store();
            let load = || -> color_eyre::Result<_> {
                let Some(pr) = store.pr_page(key)? else {
                    return Ok(None);
                };
                let Some(run) = store.review_runs(key)?.into_iter().find(|r| r.id == run_id) else {
                    return Ok(None);
                };
                let drafts = store.draft_rows(run.id)?;
                Ok(Some((run, pr.head_sha, drafts, store.threads(key)?)))
            };
            load().map_err(Error::pr(key))?.ok_or_else(missing)?
        };
        let diff = pr::read_diff(app, run.id)
            .ok_or_else(|| Error::NotFound(format!("run {run_id}'s diff is gone")))?;
        Ok(Self {
            run,
            pr_head,
            drafts,
            threads,
            diff,
        })
    }
}
