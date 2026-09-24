//! The files view: the diff a run reviewed, laid out as GitHub's Files
//! changed tab, with each draft and existing thread at its lines.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::extract::{Path, Query, State};
use color_eyre::eyre::WrapErr;
use maud::{Markup, html};
use sanic_core::{
    pr::{PrKey, Thread},
    run::Side,
};
use sanic_runner::diff::{CONTEXT, Change, DiffFile, DiffIndex, DiffLine, Hunk};
use sanic_store::{DraftRow, ReviewRun};
use serde::Deserialize;

use crate::{
    App, Error, Shared,
    highlight::{self, Highlighter},
    links,
    page::keycap,
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

/// How the files view lays out a file's lines: in one column, or the old
/// file beside the new.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Unified,
    Split,
}

impl Layout {
    /// From the `layout` parameter; anything else is unified.
    pub fn parse(param: Option<&str>) -> Self {
        match param {
            Some("split") => Self::Split,
            _ => Self::Unified,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unified => "unified",
            Self::Split => "split",
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
    pub layout: Layout,
    /// The page's own link with `layout=` set to each layout.
    pub layout_href: &'a dyn Fn(Layout) -> String,
    /// The mirror has the run's head, so the unchanged lines the diff
    /// leaves out can be shown.
    pub expandable: bool,
    /// Whether the drafts can be revised with the agent.
    pub revise: &'a pr::Revise,
}

impl Files<'_> {
    fn key(&self) -> &PrKey {
        self.existing.key
    }

    /// Where `dir` of `gap`'s lines of `path` load from.
    fn context_href(&self, path: &str, gap: Gap, dir: Dir) -> String {
        let (start, end) = (gap.start.to_string(), gap.end.map(|e| e.to_string()));
        let mut query = vec![
            ("path", path),
            ("start", &start),
            ("dir", dir.as_str()),
            ("layout", self.layout.as_str()),
        ];
        if let Some(end) = &end {
            query.push(("end", end));
        }
        format!(
            "{}/runs/{}/context?{}",
            pr_href(self.key()),
            self.run.id,
            serde_urlencoded::to_string(&query).unwrap_or_default()
        )
    }

    /// Where the lazy parts of the view load from.
    fn file_href(&self, path: &str) -> String {
        format!(
            "{}/runs/{}/file?{}",
            pr_href(self.key()),
            self.run.id,
            serde_urlencoded::to_string([("path", path), ("layout", self.layout.as_str())])
                .unwrap_or_default()
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
    // What the script keeps which files you've viewed under: they're
    // viewed at this head.
    let viewed = format!("{}@{}", f.key(), f.run.head_sha);
    html! {
        section #drafts .files data-layout=(f.layout.as_str()) data-viewed=(viewed) {
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
                span.seg.layouts #layouts {
                    @for layout in [Layout::Unified, Layout::Split] {
                        a class=[(layout == f.layout).then_some("cur")] href=((f.layout_href)(layout))
                            data-layout=(layout.as_str()) {
                            @if layout == Layout::Unified { "Unified" } @else { "Split" }
                        }
                    }
                }
                (keycap("s"))
            }
            div.fv-top {
                @for draft in f.drafts.iter().filter(|d| !placed.contains(&d.id)) {
                    (pr::draft_card(f.app, draft, Some(f.diff), f.existing, f.revise))
                }
            }
            div.fv-body {
                (tree(f, &placed))
                div.fv-files {
                    @for (file, eager) in f.diff.files().iter().zip(eager) {
                        (file_box(f, file, &placed, eager))
                    }
                }
            }
        }
    }
}

/// The files, as a list to jump from: each one's name and stats, and
/// whether a draft or thread is on it. The script ticks the ones you've
/// viewed.
fn tree(f: &Files<'_>, placed: &HashSet<i64>) -> Markup {
    let files = f.diff.files();
    html! {
        details.tree open {
            summary { "Files " span.dim { (files.len()) } }
            ul {
                @for file in files {
                    @let (adds, dels) = file.stats();
                    @let drafts = f.drafts.iter().filter(|d| placed.contains(&d.id) && d.path.as_deref() == Some(file.path.as_str())).count();
                    @let threads = f.existing.inline().filter(|t| t.path.as_deref() == Some(file.path.as_str()) && on_head(f, t)).count();
                    @let (dir, name) = file.path.rsplit_once('/').map_or(("", file.path.as_str()), |(d, n)| (d, n));
                    li {
                        a href={ "#" (links::file_anchor(&file.path)) } title=(file.path) {
                            span.nm {
                                @if !dir.is_empty() { span.dim { (dir) "/" } }
                                (name)
                            }
                            span.marks {
                                @if drafts > 0 {
                                    span.cnt title="Drafts on this file" { (drafts) }
                                }
                                @if threads > 0 {
                                    span.th title="Existing threads on this file" { "💬" (threads) }
                                }
                                span.stats { span.a { "+" (adds) } " " span.d { "−" (dels) } }
                            }
                        }
                    }
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
            .any(|t| t.path.as_deref() == Some(file.path.as_str()) && on_head(f, t))
}

/// Whether a thread is on lines of the run's head, so the diff shows it.
fn on_head(f: &Files<'_>, thread: &Thread) -> bool {
    thread
        .place
        .lines_at(thread.line, f.existing.head)
        .is_some()
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
                // The script's: folding this file, and ticking it viewed,
                // which folds it too.
                button.fold type="button" title="Fold or unfold this file" aria-expanded="true" { "▾" }
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
                label.viewed title="Viewed: fold it, here and next time" {
                    input type="checkbox"; " Viewed"
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

/// `file`'s hunks, laid out as `f.layout` says, with the drafts and
/// threads that end on a line under its row.
fn table(f: &Files<'_>, file: &DiffFile, placed: &HashSet<i64>) -> Markup {
    let path = file.path.as_str();
    let rows = Rows::new(f, path, placed);
    let highlight = size(file) <= highlight::MAX_LINES;
    let split = f.layout == Layout::Split;
    let gaps = gaps(file);
    html! {
        table.d.split[split] {
            // A fixed layout takes its widths from here, not the first row.
            @if split { colgroup { col.n; col; col.n; col; } }
            @for (hunk, gap) in file.hunks.iter().zip(&gaps) {
                @let code = highlighted(path, hunk, highlight);
                (gap_row(f, path, *gap, Some(&hunk_header(hunk))))
                @if split {
                    @for (left, right) in pairs(&hunk.lines) {
                        @let side = |i: Option<usize>| i.map(|i| (&hunk.lines[i], &code[i]));
                        (rows.split(side(left), side(right)))
                    }
                } @else {
                    @for (line, code) in hunk.lines.iter().zip(&code) {
                        (rows.unified(line, code))
                    }
                }
            }
            // The lines after the last hunk, to the end of the file.
            @if let Some(gap) = gaps.get(file.hunks.len()).copied().flatten().filter(|_| f.expandable) {
                (gap_row(f, path, Some(gap), None))
            }
        }
    }
}

/// Lines to show per click on an expander.
const EXPAND: u32 = 20;

/// The most unchanged lines one request shows.
const MAX_EXPAND: u32 = 10 * EXPAND;

/// Unchanged lines the diff leaves out, numbered in the new file: before
/// a hunk, between two, or after the last to the end of the file
/// (`end: None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Gap {
    start: u32,
    end: Option<u32>,
    /// What to add to a line's number in the new file for its number in
    /// the old: they're the same lines, shifted by the hunks before.
    offset: i64,
}

/// Which of a gap's lines an expander shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Dir {
    /// Up from the hunk below.
    Up,
    /// Down from the hunk above.
    Down,
    /// All of it, as far as one request shows.
    All,
}

impl Dir {
    fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::All => "all",
        }
    }
}

/// A hunk's lines on one side as `first..end`, where an empty side, which
/// git numbers by the line before it, starts after that line.
fn span(range: &std::ops::Range<u32>) -> (u32, u32) {
    let first = if range.is_empty() {
        range.start + 1
    } else {
        range.start
    };
    (first, first + (range.end - range.start))
}

/// The gap before each of `file`'s hunks, then the one after the last,
/// each `None` where there's nothing left out: a new file's hunk is all of
/// it, and a deleted file has nothing left to show.
fn gaps(file: &DiffFile) -> Vec<Option<Gap>> {
    if file.change == Change::Deleted {
        return vec![None; file.hunks.len() + 1];
    }
    let mut gaps = Vec::with_capacity(file.hunks.len() + 1);
    let mut next = 1;
    for hunk in &file.hunks {
        let ((first, end), (old_first, _)) = (span(&hunk.new), span(&hunk.old));
        gaps.push((next < first).then_some(Gap {
            start: next,
            end: Some(first - 1),
            offset: i64::from(old_first) - i64::from(first),
        }));
        next = end;
    }
    // git gives each hunk `CONTEXT` unchanged lines after its last
    // change, or fewer where the file ends; so a last hunk with fewer is
    // at the end, and only one with all of them may have lines after.
    let after = file
        .hunks
        .last()
        .filter(|last| {
            let trailing = last
                .lines
                .iter()
                .rev()
                .take_while(|l| l.old.is_some() && l.new.is_some())
                .count();
            file.change != Change::Added && trailing >= CONTEXT
        })
        .map(|last| {
            let ((_, end), (_, old_end)) = (span(&last.new), span(&last.old));
            Gap {
                start: end,
                end: None,
                offset: i64::from(old_end) - i64::from(end),
            }
        });
    gaps.push(after);
    gaps
}

/// A hunk's header, `header`, with the expanders for the `gap` before it
/// when the mirror has the run's head; or, without a header, the expander
/// for the lines after the last hunk.
fn gap_row(f: &Files<'_>, path: &str, gap: Option<Gap>, header: Option<&str>) -> Markup {
    let split = f.layout == Layout::Split;
    let gap = gap.filter(|_| f.expandable);
    let button = |dir: Dir, label: &str, title: &str| {
        html! {
            @if let Some(gap) = gap {
                button.x type="button" hx-get=(f.context_href(path, gap, dir))
                    hx-target="closest tr" hx-swap="outerHTML" title=(title) { (label) }
            }
        }
    };
    let size = gap.and_then(|g| Some(g.end? + 1 - g.start));
    html! {
        tr.hh {
            td.n.xs colspan=[(!split).then_some(2)] {
                @match (gap, size) {
                    (None, _) => {}
                    (Some(_), Some(n)) if n <= EXPAND => {
                        (button(Dir::All, "↕", &format!("Show the {n} lines left out here")))
                    }
                    (Some(g), _) => {
                        // Down from the hunk above, and up from the one below.
                        @if g.start > 1 { (button(Dir::Down, "↓", "Show more lines below the hunk above")) }
                        @if g.end.is_some() { (button(Dir::Up, "↑", "Show more lines above this hunk")) }
                    }
                }
            }
            td.c colspan=[split.then_some(3)] {
                @if let Some(header) = header { (header) }
            }
        }
    }
}

/// What a file's rows are drawn with: the drafts by the line they end on,
/// and the lines they're on.
struct Rows<'a> {
    f: &'a Files<'a>,
    path: &'a str,
    ending: HashMap<(Side, u32), Vec<&'a DraftRow>>,
    marked: Vec<(Side, u32, u32)>,
}

impl<'a> Rows<'a> {
    fn new(f: &'a Files<'a>, path: &'a str, placed: &HashSet<i64>) -> Self {
        let mut ending: HashMap<(Side, u32), Vec<&DraftRow>> = HashMap::new();
        let mut marked = Vec::new();
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
        Self {
            f,
            path,
            ending,
            marked,
        }
    }

    fn marked(&self, side: Side, n: Option<u32>) -> bool {
        n.is_some_and(|n| {
            self.marked
                .iter()
                .any(|&(s, first, last)| s == side && (first..=last).contains(&n))
        })
    }

    /// One line, its old and new numbers beside it.
    fn unified(&self, line: &DiffLine, code: &Markup) -> Markup {
        let class = class(line);
        let mark = self.marked(Side::Left, line.old) || self.marked(Side::Right, line.new);
        html! {
            tr.(class).mark[mark] {
                td.n { (num(line.old)) }
                td.n { (num(line.new)) }
                td.c { span.sign { (sign(class)) } (code) }
            }
            (self.under(&[(Side::Left, line.old), (Side::Right, line.new)], 3))
        }
    }

    /// An old line beside the new one, either of which may be missing.
    fn split(
        &self,
        left: Option<(&DiffLine, &Markup)>,
        right: Option<(&DiffLine, &Markup)>,
    ) -> Markup {
        let old = left.and_then(|(l, _)| l.old);
        let new = right.and_then(|(l, _)| l.new);
        let cell = |line: Option<(&DiffLine, &Markup)>, side: Side, n: Option<u32>| {
            html! {
                @if let Some((line, code)) = line {
                    @let class = class(line);
                    td.n.(class) { (num(n)) }
                    td.c.(class).mark[self.marked(side, n)] { span.sign { (sign(class)) } (code) }
                } @else {
                    td.n.none {}
                    td.c.none {}
                }
            }
        };
        html! {
            tr {
                (cell(left, Side::Left, old))
                (cell(right, Side::Right, new))
            }
            (self.under(&[(Side::Left, old), (Side::Right, new)], 4))
        }
    }

    /// The threads and drafts that end on these lines, in a row of
    /// `columns` under them.
    fn under(&self, here: &[(Side, Option<u32>)], columns: u32) -> Markup {
        let f = self.f;
        let threads: Vec<&Thread> = here
            .iter()
            .filter_map(|&(side, n)| Some(f.existing.ending_at(self.path, side, n?)))
            .flatten()
            .collect();
        let drafts: Vec<&DraftRow> = here
            .iter()
            .filter_map(|&(side, n)| self.ending.get(&(side, n?)))
            .flatten()
            .copied()
            .collect();
        html! {
            @if !threads.is_empty() || !drafts.is_empty() {
                tr.inl {
                    td colspan=(columns) {
                        @for thread in threads {
                            (threads::thread_box(f.existing.at(), Some(f.diff), thread))
                        }
                        @for draft in drafts {
                            (pr::draft_card(f.app, draft, Some(f.diff), f.existing, f.revise))
                        }
                    }
                }
            }
        }
    }
}

fn class(line: &DiffLine) -> &'static str {
    match (line.old, line.new) {
        (None, _) => "add",
        (_, None) => "del",
        _ => "ctx",
    }
}

fn sign(class: &str) -> &'static str {
    match class {
        "add" => "+",
        "del" => "-",
        _ => " ",
    }
}

fn num(n: Option<u32>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

/// A hunk's lines as the split layout's rows, by index: context beside
/// itself, and each run of removed lines beside the added ones that
/// follow it, as far as both go.
fn pairs(lines: &[DiffLine]) -> Vec<(Option<usize>, Option<usize>)> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].old.is_some() && lines[i].new.is_some() {
            rows.push((Some(i), Some(i)));
            i += 1;
            continue;
        }
        let dels = i;
        while i < lines.len() && lines[i].new.is_none() {
            i += 1;
        }
        let adds = i;
        while i < lines.len() && lines[i].old.is_none() {
            i += 1;
        }
        let (dels, adds) = (dels..adds, adds..i);
        for k in 0..dels.len().max(adds.len()) {
            let at = |r: &std::ops::Range<usize>| (k < r.len()).then_some(r.start + k);
            rows.push((at(&dels), at(&adds)));
        }
    }
    rows
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
    layout: Option<String>,
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
        diff: Some(&loaded.diff),
    };
    let Some(file) = loaded.diff.file(&query.path) else {
        return Err(Error::NotFound(format!(
            "run {} of {} doesn't change `{}`",
            path.run,
            key.url(),
            query.path
        )));
    };
    let expandable = expandable(&app, &key, &loaded.run.head_sha).await;
    // Only the page has layout links.
    let no_links = |_: Layout| String::new();
    let f = Files {
        app: &app,
        run: &loaded.run,
        pr_head: &loaded.pr_head,
        diff: &loaded.diff,
        drafts: &loaded.drafts,
        existing,
        layout: Layout::parse(query.layout.as_deref()),
        layout_href: &no_links,
        expandable,
        revise: &loaded.revise,
    };
    Ok(table(&f, file, &placed(&loaded.diff, &loaded.drafts)))
}

/// Whether the mirror has `head`, so the view can show lines the diff
/// leaves out.
pub async fn expandable(app: &Shared, key: &PrKey, head: &str) -> bool {
    let (sources, repo, head) = (Arc::clone(&app.sources), key.repo.clone(), head.to_owned());
    tokio::task::spawn_blocking(move || sources.has_commit(&repo, &head))
        .await
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
pub struct ContextQuery {
    path: String,
    /// The lines still left out, as the expander that asked has them.
    start: u32,
    end: Option<u32>,
    dir: Dir,
    layout: Option<String>,
}

/// Unchanged lines a gap in a file's diff leaves out, read from the
/// mirror at the run's head, as rows to put in place of the expander that
/// asked, with one for what's still left out.
pub async fn context(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Query(query): Query<ContextQuery>,
) -> Result<Markup, Error> {
    let key = path.pr().key()?;
    let loaded = Loaded::load(&app, &key, path.run)?;
    let Some(file) = loaded.diff.file(&query.path) else {
        return Err(Error::NotFound(format!(
            "run {} of {} doesn't change `{}`",
            path.run,
            key.url(),
            query.path
        )));
    };
    // The gap the lines are in: they must be ones the diff leaves out.
    let gaps = gaps(file);
    let within = |gap: &Gap| {
        gap.start <= query.start
            && match (gap.end, query.end) {
                (None, _) => true,
                (Some(gap), Some(end)) => end <= gap,
                (Some(_), None) => false,
            }
    };
    let Some((i, gap)) = gaps
        .iter()
        .enumerate()
        .find_map(|(i, gap)| gap.filter(within).map(|gap| (i, gap)))
    else {
        return Err(Error::Refused(format!(
            "those lines of `{}` are in its diff already",
            query.path
        )));
    };
    let header = file.hunks.get(i).map(hunk_header);

    let (sources, repo, head, file_path) = (
        Arc::clone(&app.sources),
        key.repo.clone(),
        loaded.run.head_sha.clone(),
        query.path.clone(),
    );
    let read = tokio::task::spawn_blocking(move || sources.file_at(&repo, &head, &file_path))
        .await
        .wrap_err("reading from the mirror")
        .and_then(std::convert::identity)
        .map_err(Error::pr(&key))?;
    let layout = Layout::parse(query.layout.as_deref());
    let Some(text) = read else {
        // In place of the expander's row, which is the hunk's header.
        let split = layout == Layout::Split;
        return Ok(html! {
            tr.hh {
                td.n.xs colspan=[(!split).then_some(2)] {}
                td.c colspan=[split.then_some(3)] {
                    @if let Some(header) = &header { (header) " " }
                    span.dim { "The local mirror no longer has this file at the reviewed commit." }
                }
            }
        });
    };
    let text = String::from_utf8_lossy(&text);
    let lines: Vec<&str> = text.lines().collect();
    let last = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let asked = Gap {
        start: query.start.max(1),
        end: query.end,
        ..gap
    };
    let Some(Reveal {
        lines: (from, to),
        rest,
    }) = reveal(query.dir, asked, last)
    else {
        return Ok(html! {});
    };

    let existing = Existing {
        key: &key,
        threads: &loaded.threads,
        head: &loaded.run.head_sha,
        pr_head: &loaded.pr_head,
        diff: Some(&loaded.diff),
    };
    let no_links = |_: Layout| String::new();
    let f = Files {
        app: &app,
        run: &loaded.run,
        pr_head: &loaded.pr_head,
        diff: &loaded.diff,
        drafts: &loaded.drafts,
        existing,
        layout,
        layout_href: &no_links,
        expandable: true,
        revise: &loaded.revise,
    };
    let shown = shown(&f, &query.path, &lines, (from, to), gap.offset);
    let rest = rest.map_or_else(
        || html! {},
        |rest| gap_row(&f, &query.path, Some(rest), header.as_deref()),
    );
    Ok(html! {
        @if query.dir == Dir::Up { (rest) (shown) } @else { (shown) (rest) }
    })
}

/// What a click on an expander shows.
struct Reveal {
    /// The first and last line, in the new file.
    lines: (u32, u32),
    /// What's still left out.
    rest: Option<Gap>,
}

/// Which of `asked`'s lines a click `dir` shows, in a file of `last`
/// lines; `None` if there's nothing there.
fn reveal(dir: Dir, asked: Gap, last: u32) -> Option<Reveal> {
    let (start, end) = (asked.start, asked.end.unwrap_or(last).min(last));
    if start > end {
        return None;
    }
    let (from, to) = match dir {
        Dir::Up => ((end + 1).saturating_sub(EXPAND).max(start), end),
        Dir::Down => (start, (start + EXPAND - 1).min(end)),
        Dir::All => (start, (start + MAX_EXPAND - 1).min(end)),
    };
    let rest = if dir == Dir::Up {
        (start < from).then_some(Gap {
            end: Some(from - 1),
            ..asked
        })
    } else {
        (to < end).then_some(Gap {
            start: to + 1,
            ..asked
        })
    };
    Some(Reveal {
        lines: (from, to),
        rest,
    })
}

/// Lines `from` to `to` of `lines`, the file at the run's head, as rows of
/// unchanged lines: each is the old file's line `offset` away.
fn shown(f: &Files<'_>, path: &str, lines: &[&str], (from, to): (u32, u32), offset: i64) -> Markup {
    let rows = Rows::new(f, path, &HashSet::new());
    let mut highlighter = Highlighter::for_path(path);
    html! {
        @for n in from..=to {
            @let text = lines.get(n as usize - 1).copied().unwrap_or_default();
            @let line = DiffLine {
                old: u32::try_from(i64::from(n) + offset).ok(),
                new: Some(n),
                text: text.to_owned(),
            };
            @let code = if let Some(h) = &mut highlighter { h.line(text) } else { html! { (text) } };
            @if f.layout == Layout::Split {
                (rows.split(Some((&line, &code)), Some((&line, &code))))
            } @else {
                (rows.unified(&line, &code))
            }
        }
    }
}

/// A run of a PR with what its files view needs.
struct Loaded {
    run: ReviewRun,
    pr_head: String,
    drafts: Vec<DraftRow>,
    threads: Vec<Thread>,
    diff: DiffIndex,
    revise: pr::Revise,
}

impl Loaded {
    fn load(app: &App, key: &PrKey, run_id: i64) -> Result<Self, Error> {
        let missing = || Error::NotFound(format!("{} has no run {run_id}", key.url()));
        let (run, pr_head, drafts, threads, revise) = {
            let store = app.store();
            let load = || -> color_eyre::Result<_> {
                let Some(pr) = store.pr_page(key)? else {
                    return Ok(None);
                };
                let Some(run) = store.review_runs(key)?.into_iter().find(|r| r.id == run_id) else {
                    return Ok(None);
                };
                let drafts = store.draft_rows(run.id)?;
                let revise = pr::Revise::load(&store, key, &run)?;
                Ok(Some((
                    run,
                    pr.head_sha,
                    drafts,
                    store.threads(key)?,
                    revise,
                )))
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
            revise,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(old: Option<u32>, new: Option<u32>) -> DiffLine {
        DiffLine {
            old,
            new,
            text: String::new(),
        }
    }

    #[test]
    fn split_rows_put_each_removed_line_beside_an_added_one() {
        let lines = [
            line(Some(1), Some(1)),
            line(Some(2), None),
            line(Some(3), None),
            line(None, Some(2)),
            line(Some(4), Some(3)),
            line(None, Some(4)),
            line(Some(5), None),
        ];
        assert_eq!(
            pairs(&lines),
            [
                (Some(0), Some(0)),
                (Some(1), Some(3)),
                (Some(2), None),
                (Some(4), Some(4)),
                (None, Some(5)),
                (Some(6), None),
            ]
        );
    }
}
