//! A PR's page: the agent's summary and drafts, and what you can do with
//! them and the PR.

use std::collections::HashMap;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use maud::{Markup, html};
use sanic_core::{
    pr::{PrKey, Thread, is_login},
    skip::Skip,
    start::Why,
    state::PrState,
};
use sanic_runner::diff::DiffIndex;
use sanic_store::{DraftRow, DraftStatus, OwedReview, PrPage, ReviewRun, ThreadChoice};
use serde::Deserialize;
use tracing::info;

use crate::{
    App, Error, PrPath, Shared, chat, diff,
    files::{self, Layout, View},
    index::{Overview, archive_form, owed_status, why},
    links,
    page::{self, Card, Kind, Tone, csrf_field, first_line, keycap, pr_ref, state_cell},
    pr_href, submit,
    threads::{self, Existing},
};

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    /// The run whose drafts to show; the latest that succeeded by default.
    run: Option<i64>,
    /// `files` for the files view; see [`View`].
    view: Option<String>,
    /// `split` for the files view's side-by-side layout; see [`Layout`].
    layout: Option<String>,
}

pub async fn page(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
    Query(query): Query<PageQuery>,
) -> Result<Markup, Error> {
    let key = path.key()?;
    let overview = Overview::load(&app).map_err(Error::pr(&key))?;
    let owed = overview.owed.iter().find(|o| o.key == key);
    let (pr, state, runs, shown, drafts, threads) = {
        let store = app.store();
        let load = || -> color_eyre::Result<_> {
            let Some(pr) = store.pr_page(&key)? else {
                return Ok(None);
            };
            // As the lists have it, or else from the store: a PR outside
            // the lists' recency window still has a state. A closed one
            // has none.
            let listed = owed
                .map(|o| o.state)
                .or_else(|| overview.mine.iter().find(|m| m.key == key).map(|m| m.state));
            let state = match listed {
                _ if !pr.open => None,
                Some(state) => Some(state),
                None => {
                    let mine = is_login(&pr.author, &app.me);
                    Some(store.pr_state(&key, &app.me, mine)?)
                }
            };
            let runs = store.review_runs(&key)?;
            let shown = match query.run {
                Some(id) => runs.iter().find(|run| run.id == id),
                None => runs.iter().find(|run| run.status == "succeeded"),
            }
            .cloned();
            let drafts = match &shown {
                Some(run) => store.draft_rows(run.id)?,
                None => Vec::new(),
            };
            let threads = store.threads(&key)?;
            store.record_view(&key)?;
            Ok(Some((pr, state, runs, shown, drafts, threads)))
        };
        load()
            .map_err(Error::pr(&key))?
            .ok_or_else(|| Error::NotFound(format!("{} isn't tracked", key.url())))?
    };
    let diff = match &shown {
        Some(run) => read_diff(&app, run.id),
        None => None,
    };
    let chat = chat::section(&app, &key, shown.as_ref().map(|run| run.id));
    // Revising needs the shown review's agent session.
    let revisable = match &shown {
        Some(run) if run.status == "succeeded" => app
            .store()
            .session_run(run.id)
            .map_err(Error::pr(&key))?
            .is_some(),
        _ => false,
    };
    let revise = match &shown {
        Some(run) => Revise::new(run, &runs, revisable),
        None => Revise::default(),
    };
    let views = Views::new(&app, &key, &query, shown.as_ref(), diff.is_some()).await;
    let header = Header {
        pr: &pr,
        state,
        owed,
        overview: &overview,
        runs: &runs,
        shown: shown.as_ref(),
        chat: chat.is_some(),
        revisable,
    };
    let content = html! {
        (pr_header(&app, &header))
        @if let Some(run) = &shown {
            (drafts_section(&app, &pr, run, &drafts, diff.as_ref(), &threads, &views, &revise))
        } @else if runs.is_empty() {
            p.dim { "No reviews yet." }
        } @else {
            p.dim { "No review has finished yet, so there are no drafts." }
        }
        // After the drafts: it's for once you've read them.
        @if let Some(chat) = &chat { (chat) }
        p.help-foot {
            (keycap("j")) (keycap("k")) " draft · " (keycap("e")) " edit ("
            (keycap("Esc")) " saves) · " (keycap("y")) " accept · " (keycap("n"))
            " reject · " (keycap("u")) " undo · " (keycap("a")) " revise · "
            (keycap("f")) " files · "
            (keycap("p")) " preview · "
            (keycap("r")) (keycap("x")) (keycap("i")) (keycap("c")) " act on the PR · "
            (keycap("q")) " index"
        }
    };
    Ok(page::layout_in(
        &app,
        Kind::Pr,
        &pr.title,
        &[crumb(&pr.key)],
        &content,
    ))
}

/// A run's stored diff, parsed; `None` if it's gone.
pub fn read_diff(app: &App, run: i64) -> Option<DiffIndex> {
    let path = app
        .data_dir
        .join("runs")
        .join(run.to_string())
        .join("pr.diff");
    std::fs::read_to_string(path)
        .ok()
        .map(|text| DiffIndex::parse(&text))
}

/// What the PR page's header shows.
struct Header<'a> {
    pr: &'a PrPage,
    state: Option<PrState>,
    owed: Option<&'a OwedReview>,
    overview: &'a Overview,
    runs: &'a [ReviewRun],
    shown: Option<&'a ReviewRun>,
    chat: bool,
    /// The shown run has an agent session to revise it with.
    revisable: bool,
}

/// The title; the PR's ref, author, run status and state, with its
/// actions; then its description and runs, folded.
fn pr_header(app: &App, h: &Header<'_>) -> Markup {
    let pr = h.pr;
    // `—` fills a column; in a sentence it says nothing.
    let state = h.state.filter(|state| !state.is_blank());
    let status = h
        .owed
        .map(|o| owed_status(o, h.overview, app.manual_reviews));
    let why = h.owed.and_then(|o| why(o, h.overview, app.manual_reviews));
    let href = pr_href(&pr.key);
    html! {
        div.prh {
            h1 { (pr.title) }
            div.meta {
                (pr_ref(&pr.key)) span { " · " (pr.author) }
                @if pr.is_draft { " · " span.dim { "draft" } }
                @if !pr.open { " · " span.dim { "closed" } }
                @if pr.archived { " · " span.dim { "archived" } }
                @if let Some((label, class)) = &status {
                    " · " span.chip.(class) { (label) }
                }
                @if let Some(state) = state { " · " (state_cell(state)) }
                span.sp #pr-actions
                    data-review-now=[why.as_ref().map(|_| format!("{href}/review-now"))]
                    data-ignore=[h.owed.map(|_| format!("{href}/ignore"))]
                    data-chat=[h.chat.then_some("#chat")] {
                    @if h.revisable {
                        @if let Some(run) = h.shown {
                            a.btn href={ (href) "/runs/" (run.id) "/regenerate" } data-dialog
                                title="Revise this review with the agent, from your instruction" {
                                "Agent…"
                            }
                        }
                    }
                    @if why.is_some() {
                        a.btn href={ (href) "/review-now" } data-dialog { "Review now" (keycap("r")) }
                    }
                    @if h.owed.is_some() {
                        a.btn href={ (href) "/ignore" } { "Ignore by title" (keycap("i")) }
                    }
                    @if h.chat { a.btn href="#chat" { "Chat" (keycap("c")) } }
                    (archive_form(app, &pr.key, pr.archived, "pr"))
                }
            }
            div.subnav {
                @if !pr.body.trim().is_empty() {
                    details.description {
                        summary { "Description" }
                        pre { (pr.body) }
                    }
                }
                @if !h.runs.is_empty() { (run_list(&pr.key, h.runs, h.shown)) }
            }
        }
    }
}

/// When a run happened, for the browser to show in its own time; the
/// timestamp itself, which is UTC, without the script.
fn when(at: &str) -> Markup {
    let shown = at.get(..16).unwrap_or(at).replace('T', " ");
    html! { time datetime=(at) { (shown) " UTC" } }
}

fn run_list(key: &PrKey, runs: &[ReviewRun], shown: Option<&ReviewRun>) -> Markup {
    let href = pr_href(key);
    // Runs come newest first; "run 3 of 3" counts from the oldest.
    let count = runs.len();
    let number = |id: i64| {
        runs.iter()
            .position(|run| run.id == id)
            .map_or(0, |i| count - i)
    };
    html! {
        details.runs {
            summary {
                @if let Some(run) = shown {
                    "Run " (number(run.id)) " of " (count) " · "
                    (when(run.finished_at.as_deref().unwrap_or(&run.queued_at)))
                    " · reviewed " code { (short(&run.head_sha)) }
                } @else {
                    "Runs (" (count) ")"
                }
            }
            ol reversed {
                @for run in runs {
                    li.current[shown.is_some_and(|s| s.id == run.id)] {
                        a href={ (href) "?run=" (run.id) } { "run " (number(run.id)) }
                        " at " code { (short(&run.head_sha)) } " · "
                        (when(run.finished_at.as_deref().unwrap_or(&run.queued_at))) " · "
                        span.(run_class(&run.status)) { (run.status) }
                        @if let (Some(draft), Some(of)) = (run.draft_id, run.draft_run) {
                            " · revises "
                            a href={ (href) "?run=" (of) "#draft-" (draft) } { "draft #" (draft) }
                            " of run " (number(of))
                        } @else if let Some(source) = run.source_run {
                            " · revises "
                            a href={ (href) "?run=" (source) } { "run " (number(source)) }
                        }
                        @if let Some(instruction) = &run.instruction {
                            " · " span.instruction title=(instruction) {
                                "“" (first_line(instruction))
                                @if instruction.trim_end().contains('\n') { "…" }
                                "”"
                            }
                        }
                        @if let Some(error) = &run.error {
                            " — " span.error { (first_line(error)) }
                        }
                    }
                }
            }
        }
    }
}

fn run_class(status: &str) -> &'static str {
    match status {
        "succeeded" => "ok",
        "failed" | "crashed" => "bad",
        "running" => "running",
        "queued" => "held",
        _ => "dim",
    }
}

/// The PR, as the top bar names it, linking to its page.
pub fn crumb(key: &PrKey) -> Markup {
    html! { a href=(pr_href(key)) { (key.repo) "#" (key.number) } }
}

pub fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// What a review of `reviewed` comes with once the PR is at `head`.
pub fn moved_on(reviewed: &str, head: &str) -> Markup {
    html! {
        "Reviewed at " code { (short(reviewed)) } "; the PR has moved on to "
        code { (short(head)) } ". These comments post against the older commit."
    }
}

/// Which view of the drafts the page shows, and the run it was asked
/// for, which the links to the other view keep.
struct Views {
    shown: View,
    run: Option<i64>,
    layout: Layout,
    /// The files view can show the lines its diff leaves out.
    expandable: bool,
}

impl Views {
    /// As `query` asks, for the `shown` run, which may have a diff.
    async fn new(
        app: &Shared,
        key: &PrKey,
        query: &PageQuery,
        shown: Option<&ReviewRun>,
        diff: bool,
    ) -> Self {
        let view = View::parse(query.view.as_deref());
        let expandable = match shown {
            Some(run) if view == View::Files && diff => {
                files::expandable(app, key, &run.head_sha).await
            }
            _ => false,
        };
        Self {
            shown: view,
            run: query.run,
            layout: Layout::parse(query.layout.as_deref()),
            expandable,
        }
    }

    fn href(&self, key: &PrKey, view: View) -> String {
        self.href_with(key, view, self.layout)
    }

    /// The page for `view`, laid out as `layout` when that's the files
    /// view's split.
    fn href_with(&self, key: &PrKey, view: View, layout: Layout) -> String {
        let run = self.run.map(|r| format!("run={r}&")).unwrap_or_default();
        let layout = match (view, layout) {
            (View::Files, Layout::Split) => "&layout=split",
            (View::Files, Layout::Unified) if self.layout == Layout::Split => "&layout=unified",
            _ => "",
        };
        format!("{}?{run}view={}{layout}", pr_href(key), view.as_str())
    }

    /// The two views, as tabs, with how many files the diff has.
    fn tabs(&self, key: &PrKey, files: usize) -> Markup {
        html! {
            nav.views #views {
                @for (view, label) in [(View::Drafts, "Drafts"), (View::Files, "Files changed")] {
                    a class=[(view == self.shown).then_some("cur")] href=(self.href(key, view))
                        data-view=(view.as_str()) {
                        (label)
                        @if view == View::Files { " " span.cnt { (files) } }
                    }
                }
                (keycap("f"))
            }
        }
    }
}

/// Whether a run's drafts can each be revised with the agent, and how the
/// revisions of them asked for so far went.
#[derive(Debug, Default)]
pub struct Revise {
    /// The run succeeded and has an agent session to resume.
    open: bool,
    /// The latest regeneration of each draft on its own, by draft.
    runs: HashMap<i64, ReviewRun>,
}

impl Revise {
    /// For `run`'s drafts, from its PR's `runs`; `revisable` if it has a
    /// session to resume.
    pub fn new(run: &ReviewRun, runs: &[ReviewRun], revisable: bool) -> Self {
        let mut latest = HashMap::new();
        // Newest first, so the first of each draft's is its latest.
        for other in runs {
            if let Some(draft) = other.draft_id {
                latest.entry(draft).or_insert_with(|| other.clone());
            }
        }
        Self {
            open: revisable && run.status == "succeeded",
            runs: latest,
        }
    }

    /// As [`Revise::new`], reading what it needs from the store.
    pub fn load(
        store: &sanic_store::Store,
        key: &PrKey,
        run: &ReviewRun,
    ) -> color_eyre::Result<Self> {
        let revisable = store.session_run(run.id)?.is_some();
        Ok(Self::new(run, &store.review_runs(key)?, revisable))
    }
}

#[allow(clippy::too_many_arguments)]
fn drafts_section(
    app: &App,
    pr: &PrPage,
    run: &ReviewRun,
    drafts: &[DraftRow],
    diff: Option<&DiffIndex>,
    threads: &[Thread],
    views: &Views,
    revise: &Revise,
) -> Markup {
    let existing = Existing {
        key: &pr.key,
        threads,
        head: &run.head_sha,
        pr_head: &pr.head_sha,
    };
    let suggested = run.suggested_verdict.as_deref().unwrap_or("none");
    let preselect = if suggested == "request_changes" {
        "REQUEST_CHANGES"
    } else {
        "COMMENT"
    };
    let preview = format!("{}/runs/{}/preview", pr_href(&pr.key), run.id);
    let count = |status: &str| drafts.iter().filter(|d| d.status == status).count();
    html! {
        // The verdict and Preview stay in view over the drafts.
        // A post, so picking Approve here is something only this page
        // can do: see `submit::pick`.
        form.rbar #review-bar method="post" action=(preview) {
            (csrf_field(app))
            span.tally {
                b data-count="pending" { (count("pending")) } " pending · "
                b.ok data-count="accepted" { (count("accepted")) } " accepted · "
                b data-count="rejected" { (count("rejected")) } " rejected"
            }
            span.sp {
                span.seg {
                    @for (value, label) in [
                        ("COMMENT", "Comment"),
                        ("REQUEST_CHANGES", "Request changes"),
                        ("APPROVE", "Approve"),
                    ] {
                        label {
                            input type="radio" name="event" value=(value)
                                checked[value == preselect];
                            (label)
                            @if value == preselect && suggested != "none" {
                                span.sugg title="The agent's suggestion. It never suggests approving." {
                                    "suggested"
                                }
                            }
                        }
                    }
                }
                button.btn.go #preview type="submit" { "Preview the review" (keycap("p")) }
            }
        }
        @if run.head_sha != pr.head_sha {
            div.banner { (moved_on(&run.head_sha, &pr.head_sha)) }
        }
        @if let Some(n) = run.in_progress_comments.filter(|&n| n > 0) {
            p.seen {
                "The agent saw " (n) " of your pending review "
                @if n == 1 { "comment" } @else { "comments" }
                " on GitHub, and was asked to check them, not repeat them. That review is "
                "yours to submit there."
            }
        }
        // Where Revise lands: the new run, before it has drafts or a diff.
        @if matches!(run.status.as_str(), "queued" | "running") {
            div.banner {
                "This run is " (run.status) ": its drafts show here once it finishes. "
                "Reload to see them."
            }
        } @else if diff.is_none() {
            div.banner { "This run's diff is gone, so drafts are shown without context." }
        }
        (threads::summary(existing, drafts))
        // The files view needs the diff.
        @if let Some(diff) = diff { (views.tabs(&pr.key, diff.files().len())) }
        @match (views.shown, diff) {
            (View::Files, Some(diff)) => {
                (files::section(&files::Files {
                    app,
                    run,
                    pr_head: &pr.head_sha,
                    diff,
                    drafts,
                    existing,
                    layout: views.layout,
                    layout_href: &|layout| views.href_with(&pr.key, View::Files, layout),
                    expandable: views.expandable,
                    revise,
                }))
            }
            _ => {
                section #drafts {
                    @for draft in drafts { (draft_card(app, draft, diff, existing, revise)) }
                }
            }
        }
    }
}

/// One draft: its anchor and decision, the diff around it, the existing
/// threads it overlaps, and its body, which a click or `e` turns into a
/// box that saves as you leave it; then its private note, and a way to
/// revise it with the agent.
pub fn draft_card(
    app: &App,
    draft: &DraftRow,
    diff: Option<&DiffIndex>,
    existing: Existing<'_>,
    revise: &Revise,
) -> Markup {
    let editable = matches!(draft.status.as_str(), "pending" | "accepted" | "rejected");
    let edit = format!("/drafts/{}/edit", draft.id);
    let status = format!("/drafts/{}/status", draft.id);
    // A rejected draft folds to one line.
    let open = draft.status != "rejected";
    let context = diff
        .filter(|_| open)
        .and_then(|diff| diff::context(diff, draft, existing));
    let overlapping = if open {
        existing.overlapping(draft)
    } else {
        Vec::new()
    };
    let accepted = accepted_label(draft, chosen(draft, existing), !overlapping.is_empty());
    let dropped = draft
        .drop_reason
        .as_deref()
        .filter(|_| draft.status == "rejected");
    html! {
        article.draft.dc.(draft.status).summary[draft.kind == "summary"] #{ "draft-" (draft.id) } {
            div.h {
                @if draft.kind == "summary" {
                    span.anc { "Summary" } span.tag-sum { "the review body" }
                } @else {
                    @if let Some(url) = line_link(draft, existing.at()) {
                        a.anc href=(url) title="On GitHub, at the reviewed commit" { (anchor(draft)) }
                    } @else {
                        span.anc { (anchor(draft)) }
                    }
                    @if let Some(severity) = &draft.severity { span.sev.(severity) { (severity) } }
                    @if let Some(confidence) = &draft.confidence {
                        span.dim.conf { (confidence) " conf." }
                    }
                    @if !overlapping.is_empty() {
                        a.tag-overlap href={ "#draft-" (draft.id) "-threads" }
                            title="An existing review thread is on these lines." {
                            "overlaps an existing thread"
                        }
                    }
                    @if draft.unanchored {
                        span.tag-body title="Not on a line of the diff, so GitHub won't take it inline. If you accept it, it's posted in the review body." {
                            "not in the diff → goes in the body"
                        }
                    }
                }
                @if let Some(from) = draft.based_on {
                    a.edited href={ "#draft-" (from) } { "revised from #" (from) }
                }
                @if draft.edited_body.is_some() { span.edited { "edited" } }
                span.sp {
                    @if editable {
                        form.decide method="post" action=(status) hx-post=(status)
                            hx-target="closest article" hx-swap="outerHTML"
                            hx-sync="closest article:queue all" {
                            (csrf_field(app))
                            @match draft.status.as_str() {
                                "pending" => {
                                    button.btn name="status" value="accepted"
                                        title=[(!overlapping.is_empty()).then_some("As a comment of its own, not in the existing thread.")] {
                                        @if overlapping.is_empty() { "Accept" } @else { "Post separately" }
                                        (keycap("y"))
                                    }
                                    button.btn name="status" value="rejected" { "Reject" (keycap("n")) }
                                }
                                other => {
                                    span.st.(other) {
                                        @if other == "accepted" { (accepted) }
                                        @else if dropped.is_some() { "✕ dropped by the agent" }
                                        @else { "✕ rejected" }
                                    }
                                    button.btn name="status" value="pending"
                                        title=[dropped.map(|_| "Put it back, pending")] {
                                        @if dropped.is_some() { "Restore" } @else { "Undo" }
                                        (keycap("u"))
                                    }
                                }
                            }
                        }
                    } @else {
                        span.st.(draft.status) { (draft.status) }
                    }
                }
            }
            @if let Some(context) = context { (context) }
            @if open { (in_threads(app, draft, existing, &overlapping, editable)) }
            div.body data-edit[editable] title=[editable.then_some("click or e to edit")] {
                (draft.body())
            }
            @if editable {
                // Queued behind each other, so an edit saved on blur lands
                // before the Accept click that caused the blur.
                form.edit method="post" action=(edit) hx-post=(edit) hx-trigger="change"
                    hx-target="closest article" hx-swap="outerHTML"
                    hx-sync="closest article:queue all" {
                    (csrf_field(app))
                    textarea name="body" rows="4" { (draft.body()) }
                    // Without the script there's no click-to-edit: the box
                    // shows, with this.
                    button.btn.save type="submit" { "Save" }
                }
            }
            (card_foot(app, draft, editable, dropped, revise))
        }
    }
}

/// Under a draft: its private note, unless it's folded, why the agent
/// dropped it, and revising it with the agent while it's `decidable`.
fn card_foot(
    app: &App,
    draft: &DraftRow,
    decidable: bool,
    dropped: Option<&str>,
    revise: &Revise,
) -> Markup {
    let open = draft.status != "rejected";
    let revision = revise.runs.get(&draft.id);
    let underway = revision.is_some_and(|run| matches!(run.status.as_str(), "queued" | "running"));
    html! {
        @if open { @if let Some(note) = &draft.note { (private_note(note)) } }
        @if let Some(reason) = dropped {
            div.dropped {
                b { "The agent dropped this draft" } span.dim { " (it's kept, rejected)" } ": "
                span.why { (reason) }
            }
        }
        @if let Some(run) = revision { (revision_state(&draft.key, run)) }
        @if revise.open && decidable && !underway { (revise_form(app, draft)) }
    }
}

/// How the latest revision of a draft on its own went, linking the run
/// with the result.
fn revision_state(key: &PrKey, run: &ReviewRun) -> Markup {
    let href = format!("{}?run={}", pr_href(key), run.id);
    html! {
        div.revising.(run_class(&run.status)) {
            @match run.status.as_str() {
                "queued" | "running" => {
                    "The agent is revising this draft: " a href=(href) { "its run" } " is "
                    (run.status) ". The result is a new run; reload to see it."
                }
                "succeeded" => { "Revised with the agent in " a href=(href) { "a later run" } "." }
                status => {
                    "Revising this draft " (status) " in " a href=(href) { "its run" }
                    @if let Some(error) = &run.error { ": " span.error { (first_line(error)) } }
                    "."
                }
            }
        }
    }
}

/// "Revise…": a box for your note to the agent about this draft, e.g. why
/// it's wrong or what to focus on, which starts a regeneration of just
/// this draft.
fn revise_form(app: &App, draft: &DraftRow) -> Markup {
    let action = format!("/drafts/{}/revise", draft.id);
    html! {
        details.revise {
            summary.btn title="Revise just this draft with the agent, from your note" {
                "Revise…" (keycap("a"))
            }
            form method="post" action=(action) {
                (csrf_field(app))
                textarea name="instruction" rows="2" required
                    placeholder="e.g. not true because …; focus on the fix; reword" {}
                div.go {
                    button.btn.go type="submit" { "Revise with the agent" }
                    span.dim {
                        "Spends tokens. The result is a new run: this draft revised or dropped, "
                        "the others as they stand."
                    }
                }
            }
        }
    }
}

/// The agent's note on a draft, for you: set apart from the draft, and
/// labelled, since nothing in it is posted.
fn private_note(note: &str) -> Markup {
    html! {
        aside.pnote {
            div.lbl { "Reviewer note " span { "(not posted)" } }
            div.text { (note) }
        }
    }
}

/// What an accepted draft says it posts: where, if it's in `chosen`, an
/// existing thread; else whether that's separately from threads it
/// `overlaps`.
fn accepted_label(draft: &DraftRow, chosen: Option<&Thread>, overlaps: bool) -> String {
    match &draft.choice {
        Some(ThreadChoice::React { comment, .. }) => {
            let by = chosen
                .and_then(|t| t.comments.iter().find(|c| c.id == *comment))
                .map_or("their", |c| c.author.as_str());
            format!("✓ 👍 on {by}'s comment instead")
        }
        Some(ThreadChoice::Reply { .. }) => {
            let by = chosen
                .and_then(|t| t.comments.first())
                .map_or("their", |c| c.author.as_str());
            format!("✓ reply in {by}'s thread")
        }
        None if overlaps => "✓ posts separately".into(),
        None => "✓ accepted".into(),
    }
}

/// The thread `draft` is accepted to post in, if it's still on the PR.
fn chosen<'a>(draft: &DraftRow, existing: Existing<'a>) -> Option<&'a Thread> {
    let choice = draft.choice.as_ref()?;
    existing.inline().find(|t| t.id == choice.thread())
}

/// The existing threads under `draft`: those it's `overlapping`, and the
/// one it's accepted to post in, overlapping or not, marked. While it's
/// `editable`, a comment gets the choices of what to post in each.
fn in_threads(
    app: &App,
    draft: &DraftRow,
    existing: Existing<'_>,
    overlapping: &[&Thread],
    editable: bool,
) -> Markup {
    let chosen = chosen(draft, existing);
    let mut shown = overlapping.to_vec();
    if let Some(thread) = chosen.filter(|t| !shown.iter().any(|s| s.id == t.id)) {
        shown.push(thread);
    }
    let choosable = editable && draft.kind == "comment";
    html! {
        @if !shown.is_empty() {
            div.overlaps #{ "draft-" (draft.id) "-threads" } {
                div.dim {
                    @if overlapping.is_empty() { "Posts in this thread:" } @else { "Already said on these lines:" }
                }
                @for thread in shown {
                    @let actions = if choosable { choose_form(app, draft, thread) } else { html! {} };
                    (threads::thread_box_with(
                        existing.at(),
                        thread,
                        chosen.is_some_and(|c| c.id == thread.id),
                        &actions,
                    ))
                }
            }
        }
    }
}

/// What `draft` can post in `thread` instead of a comment of its own: a
/// thumbs-up on one of its comments, the first unless you pick another,
/// or itself as a reply there.
fn choose_form(app: &App, draft: &DraftRow, thread: &Thread) -> Markup {
    let action = format!("/drafts/{}/thread", draft.id);
    let picked = match &draft.choice {
        Some(ThreadChoice::React { thread: t, comment }) if *t == thread.id => Some(comment),
        _ => None,
    };
    html! {
        form.choose method="post" action=(action) hx-post=(action)
            hx-target="closest article" hx-swap="outerHTML"
            hx-sync="closest article:queue all" {
            (csrf_field(app))
            input type="hidden" name="thread" value=(thread.id);
            @if let [only] = &thread.comments[..] {
                input type="hidden" name="comment" value=(only.id);
            } @else {
                select name="comment" title="The comment the 👍 goes on" {
                    @for (i, c) in thread.comments.iter().enumerate() {
                        option value=(c.id)
                            selected[picked.map_or(i == 0, |p| *p == c.id)] {
                            (c.author) ": " (threads::excerpt_of(&c.body, 40))
                        }
                    }
                }
            }
            button.btn name="choice" value="react"
                title="Post a 👍 on that comment, and not the draft's text" {
                "👍 instead"
            }
            button.btn name="choice" value="reply" title="Post the draft as a reply in this thread" {
                "Reply here instead"
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChoiceForm {
    choice: String,
    thread: String,
    #[serde(default)]
    comment: String,
}

/// Accepts a draft to post in an existing thread instead of on its own.
pub async fn choose_thread(
    State(app): State<Shared>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<ChoiceForm>,
) -> Result<Response, Error> {
    let ChoiceForm {
        choice,
        thread,
        comment,
    } = form;
    let choice = match choice.as_str() {
        "react" if !comment.is_empty() => ThreadChoice::React { thread, comment },
        "reply" => ThreadChoice::Reply { thread },
        other => {
            return Err(Error::Refused(format!(
                "`{other}` isn't a way to post in a thread"
            )));
        }
    };
    let _posting = app.posting.lock().await;
    decided(&app, id, &headers, |store| store.choose_thread(id, &choice))
}

/// Where `draft`'s lines are on GitHub: see [`links::At::lines`]; for
/// lines the diff doesn't have, in the file at the reviewed head.
pub fn line_link(draft: &DraftRow, at: links::At<'_>) -> Option<String> {
    if draft.unanchored {
        return submit::blob_link(draft, at.reviewed);
    }
    let (path, side, lines) = threads::lines(draft)?;
    at.lines(path, side, lines)
}

/// `path:line`, or `path:start-line`, with the side when it's the old one.
pub fn anchor(draft: &DraftRow) -> String {
    let path = draft.path.as_deref().unwrap_or("?");
    let line = draft.line.map(|l| l.to_string()).unwrap_or_default();
    let lines = match draft.start_line {
        Some(start) if Some(start) != draft.line => format!("{start}-{line}"),
        _ => line,
    };
    let side = if draft.side.as_deref() == Some("LEFT") {
        " (old)"
    } else {
        ""
    };
    format!("{path}:{lines}{side}")
}

#[derive(Debug, Deserialize)]
pub struct EditForm {
    body: String,
}

pub async fn edit_draft(
    State(app): State<Shared>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<EditForm>,
) -> Result<Response, Error> {
    // Browsers send textarea line breaks as CRLF.
    let body = form.body.replace("\r\n", "\n");
    // Not while a review is being posted: its drafts are about to be
    // marked posted, as GitHub has them.
    let _posting = app.posting.lock().await;
    decided(&app, id, &headers, |store| store.edit_draft(id, &body))
}

#[derive(Debug, Deserialize)]
pub struct ReviseForm {
    instruction: String,
}

/// Revises draft `id` alone with the agent, from your note: `serve`
/// starts a regeneration of it through
/// [`Control::regenerate`](crate::Control::regenerate). Back on the
/// draft, which says it's being revised; or why it wasn't.
pub async fn revise_draft(
    State(app): State<Shared>,
    Path(id): Path<i64>,
    Form(form): Form<ReviseForm>,
) -> Result<Response, Error> {
    let instruction = form.instruction.replace("\r\n", "\n");
    if instruction.trim().is_empty() {
        return Err(Error::Refused(
            "say what to change: the note is empty".into(),
        ));
    }
    let draft = app
        .store()
        .draft_row(id)?
        .ok_or_else(|| Error::NotFound(format!("there's no draft {id}")))?;
    let key = &draft.key;
    let started = app
        .control
        .regenerate(draft.run_id, Some(id), &instruction)
        .map_err(Error::pr(key))?;
    match started {
        Ok(run) => {
            info!(url = %key.url(), source_run = draft.run_id, draft = id, run, "draft revision requested from the dashboard");
            let back = format!("{}?run={}#draft-{id}", pr_href(key), draft.run_id);
            Ok(Redirect::to(&back).into_response())
        }
        Err(why) => Err(Error::Refused(format!("Draft {id} wasn't revised: {why}."))),
    }
}

#[derive(Debug, Deserialize)]
pub struct StatusForm {
    status: String,
}

pub async fn set_draft_status(
    State(app): State<Shared>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<StatusForm>,
) -> Result<Response, Error> {
    let status = match form.status.as_str() {
        "pending" => DraftStatus::Pending,
        "accepted" => DraftStatus::Accepted,
        "rejected" => DraftStatus::Rejected,
        other => return Err(Error::Refused(format!("`{other}` isn't a draft status"))),
    };
    let _posting = app.posting.lock().await;
    decided(&app, id, &headers, |store| {
        store.set_draft_status(id, status)
    })
}

/// Applies `change` to draft `id`, then answers htmx with the redrawn card
/// and a plain form post with a redirect back to the PR page.
fn decided(
    app: &App,
    id: i64,
    headers: &HeaderMap,
    change: impl FnOnce(&sanic_store::Store) -> color_eyre::Result<bool>,
) -> Result<Response, Error> {
    let (draft, threads, head, pr_head, revise) = {
        let store = app.store();
        let Some(draft) = store.draft_row(id)? else {
            return Err(Error::NotFound(format!("there's no draft {id}")));
        };
        let key = draft.key.clone();
        if !change(&store).map_err(Error::pr(&key))? {
            return Err(Error::Refused(format!(
                "draft {id} is {} and can't be changed",
                draft.status
            )));
        }
        let load = || -> color_eyre::Result<_> {
            let draft = store.draft_row(id)?.unwrap_or(draft);
            let runs = store.review_runs(&key)?;
            let run = runs.iter().find(|run| run.id == draft.run_id).cloned();
            let revise = match &run {
                Some(run) => Revise::new(run, &runs, store.session_run(run.id)?.is_some()),
                None => Revise::default(),
            };
            let head = run.map(|run| run.head_sha).unwrap_or_default();
            let pr_head = store
                .pr_page(&key)?
                .map(|pr| pr.head_sha)
                .unwrap_or_default();
            Ok((draft, store.threads(&key)?, head, pr_head, revise))
        };
        load().map_err(Error::pr(&key))?
    };
    if headers.contains_key("hx-request") {
        let diff = read_diff(app, draft.run_id);
        let existing = Existing {
            key: &draft.key,
            threads: &threads,
            head: &head,
            pr_head: &pr_head,
        };
        return Ok(draft_card(app, &draft, diff.as_ref(), existing, &revise).into_response());
    }
    let back = format!("{}?run={}#draft-{id}", pr_href(&draft.key), draft.run_id);
    Ok(Redirect::to(&back).into_response())
}

/// What a review started by hand costs.
const COST: &str = "Spends tokens: a full review of the head as last polled.";

pub async fn confirm_review_now(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
) -> Result<Markup, Error> {
    let key = path.key()?;
    let overview = Overview::load(&app).map_err(Error::pr(&key))?;
    let owed = overview.owed.iter().find(|pr| pr.key == key);
    let why = owed.and_then(|pr| why(pr, &overview, app.manual_reviews));
    let pr = app
        .store()
        .pr_page(&key)
        .map_err(Error::pr(&key))?
        .ok_or_else(|| Error::NotFound(format!("{} isn't tracked", key.url())))?;
    let href = pr_href(&key);
    let meta = html! {
        (pr_ref(&key)) " · " (pr.author) " · head " code { (short(&pr.head_sha)) }
        @if why == Some(Why::Held) { " · held by " code { "--manual-reviews" } }
    };
    let error = owed
        .and_then(|pr| pr.latest_run.as_ref())
        .and_then(|run| Some((run.status.as_str(), run.error.as_deref()?)));
    let go = |label: &str| {
        html! {
            form #confirm method="post" action={ (href) "/review-now" } {
                (csrf_field(&app))
                // A dialog over the index sets it to `index`.
                input type="hidden" name="next" value="pr";
                button.btn.go type="submit" { (label) (keycap("y")) }
            }
        }
    };
    // Worded as the TUI's confirm is.
    let (heading, extra, cost, go) = match &why {
        None => (
            html! { "Nothing to start" },
            html! {
                p.note {
                    "Only a review you owe whose latest run failed or crashed, that's "
                    "skipped or archived, or that " code { "--manual-reviews" }
                    " is holding can be started by hand."
                }
            },
            None,
            html! {},
        ),
        Some(Why::Skipped(Skip::Reviewed { by, .. })) => (
            html! { "Already reviewed by " (by) ". Review anyway?" },
            html! {},
            Some(html! { (COST) " It stays skipped for automatic reviews." }),
            go("Review anyway"),
        ),
        Some(Why::Skipped(skip)) => (
            html! { "Review this " (skip.label()) "-skipped PR anyway?" },
            html! {},
            Some(html! { (COST) " It stays skipped for automatic reviews." }),
            go("Review anyway"),
        ),
        Some(Why::Failed) => (
            html! { "Rerun this review?" },
            html! {
                @if let Some((status, error)) = error {
                    div.error { (status) ": " (error) }
                }
            },
            Some(html! { (COST) }),
            go("Rerun the review"),
        ),
        Some(Why::Held) => (
            html! { "Start the held review now?" },
            html! {},
            Some(html! { (COST) }),
            go("Start it"),
        ),
    };
    let content = page::card(&Card {
        kind: "Review now",
        heading,
        title: &pr.title,
        meta,
        extra,
        cost,
        go,
        back: (&href, if why.is_some() { "Cancel" } else { "Back" }),
        tone: Tone::Ask,
    });
    Ok(page::layout_in(
        &app,
        Kind::Confirm,
        "Review now",
        &[crumb(&key), html! { "review now" }],
        &content,
    ))
}

#[derive(Debug, Deserialize)]
pub struct ReviewNowForm {
    /// `index` to go back to the index afterwards, as a dialog opened
    /// there asks; else the PR's page.
    #[serde(default)]
    next: String,
}

pub async fn review_now(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
    Form(form): Form<ReviewNowForm>,
) -> Result<Redirect, Error> {
    let key = path.key()?;
    // As the confirm page decided, again: it may be stale, or sent twice.
    let overview = Overview::load(&app).map_err(Error::pr(&key))?;
    let Some(owed) = overview.owed.iter().find(|pr| pr.key == key) else {
        return Err(Error::NotFound(format!(
            "{} isn't a tracked review you owe",
            key.url()
        )));
    };
    if why(owed, &overview, app.manual_reviews).is_none() {
        return Err(Error::Refused(format!(
            "there's nothing to start for {}: its review isn't failed, held or skipped",
            key.url()
        )));
    }
    info!(url = %key.url(), "review requested from the dashboard");
    let href = pr_href(&key);
    app.control.review_now(key);
    Ok(Redirect::to(if form.next == "index" { "/" } else { &href }))
}

#[derive(Debug, Deserialize)]
pub struct ArchiveForm {
    archived: bool,
    /// Where to go afterwards: `index`, or else the PR's page.
    next: String,
}

/// Archives or unarchives the PR straight in the store, as
/// `sanic-review archive` does, so the page you land on already agrees.
pub async fn archive(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
    Form(form): Form<ArchiveForm>,
) -> Result<Redirect, Error> {
    let key = path.key()?;
    let url = key.url();
    let changed = app
        .store()
        .set_archived(&key, form.archived)
        .map_err(Error::pr(&key))?;
    if !changed {
        return Err(Error::NotFound(format!("{url} isn't tracked")));
    }
    if form.archived {
        info!(url = %url, "archived from the dashboard");
    } else {
        info!(url = %url, "unarchived from the dashboard");
    }
    Ok(if form.next == "index" {
        Redirect::to("/")
    } else {
        Redirect::to(&pr_href(&key))
    })
}
