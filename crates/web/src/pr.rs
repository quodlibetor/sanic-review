//! A PR's page: the agent's summary and drafts, and what you can do with
//! them and the PR.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use maud::{Markup, html};
use sanic_core::pr::PrKey;
use sanic_runner::diff::DiffIndex;
use sanic_store::{DraftRow, DraftStatus, PrPage, ReviewRun};
use serde::Deserialize;
use tracing::info;

use crate::{
    App, Error, PrPath, Shared, chat, diff,
    index::{Overview, Why, archive_form, owed_status},
    page::{self, Kind, csrf_field, first_line, github_link},
    pr_href,
};

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    /// The run whose drafts to show; the latest that succeeded by default.
    run: Option<i64>,
}

pub async fn page(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
    Query(query): Query<PageQuery>,
) -> Result<Markup, Error> {
    let key = path.key()?;
    let overview = Overview::load(&app).map_err(Error::pr(&key))?;
    let (pr, runs, shown, drafts) = {
        let store = app.store();
        let load = || -> color_eyre::Result<_> {
            let Some(pr) = store.pr_page(&key)? else {
                return Ok(None);
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
            store.record_view(&key)?;
            Ok(Some((pr, runs, shown, drafts)))
        };
        load()
            .map_err(Error::pr(&key))?
            .ok_or_else(|| Error::NotFound(format!("{} isn't tracked", key.url())))?
    };
    let diff = match &shown {
        Some(run) => read_diff(&app, run.id),
        None => None,
    };
    let chat = chat::section(&app, &key);
    let content = html! {
        (pr_header(&app, &pr, &overview, chat.is_some()))
        @if !pr.body.trim().is_empty() {
            details.description {
                summary { "Description" }
                pre { (pr.body) }
            }
        }
        @if let Some(chat) = &chat { (chat) }
        (run_list(&pr.key, &runs, shown.as_ref()))
        @if let Some(run) = &shown {
            (drafts_section(&app, &pr, run, &drafts, diff.as_ref()))
        }
    };
    Ok(page::layout(&app, Kind::Pr, &pr.title, &content))
}

/// A run's stored diff, parsed; `None` if it's gone.
fn read_diff(app: &App, run: i64) -> Option<DiffIndex> {
    let path = app
        .data_dir
        .join("runs")
        .join(run.to_string())
        .join("pr.diff");
    std::fs::read_to_string(path)
        .ok()
        .map(|text| DiffIndex::parse(&text))
}

fn pr_header(app: &App, pr: &PrPage, overview: &Overview, chat: bool) -> Markup {
    let owed = overview.owed.iter().find(|o| o.key == pr.key);
    let status = owed.map(|o| owed_status(o, overview, app.manual_reviews));
    let why = owed.and_then(|o| Why::of(o, overview.skipped.get(&o.key), app.manual_reviews));
    let href = pr_href(&pr.key);
    html! {
        h1 { (pr.title) }
        p.meta {
            (github_link(&pr.key))
            " by " (pr.author)
            @if pr.is_draft { " · " span.dim { "draft" } }
            @if !pr.open { " · " span.dim { "closed" } }
            @if pr.archived { " · " span.dim { "archived" } }
            @if let Some((label, class)) = &status { " · " span.status.(class) { (label) } }
        }
        div.actions #pr-actions data-review-now=[why.map(|_| format!("{href}/review-now"))]
            data-ignore=[owed.map(|_| format!("{href}/ignore"))]
            data-chat=[chat.then_some("#chat")] {
            @if why.is_some() {
                a.button href={ (href) "/review-now" } { "Review now" }
            }
            @if owed.is_some() {
                a.button href={ (href) "/ignore" } { "Ignore by title" }
            }
            (archive_form(app, &pr.key, pr.archived, "pr"))
        }
    }
}

fn run_list(key: &PrKey, runs: &[ReviewRun], shown: Option<&ReviewRun>) -> Markup {
    let href = pr_href(key);
    html! {
        @if runs.is_empty() {
            p.dim { "No reviews yet." }
        } @else {
            details.runs open[runs.len() > 1 && shown.is_none()] {
                summary { "Reviews (" (runs.len()) ")" }
                ol {
                    @for run in runs {
                        li.current[shown.is_some_and(|s| s.id == run.id)] {
                            a href={ (href) "?run=" (run.id) } {
                                "run " (run.id) " at " code { (short(&run.head_sha)) }
                            }
                            " " span.status { (run.status) }
                            @if let Some(error) = &run.error { " " span.error { (first_line(error)) } }
                        }
                    }
                }
            }
            @if shown.is_none() {
                p.dim { "No review has finished yet, so there are no drafts." }
            }
        }
    }
}

fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

fn drafts_section(
    app: &App,
    pr: &PrPage,
    run: &ReviewRun,
    drafts: &[DraftRow],
    diff: Option<&DiffIndex>,
) -> Markup {
    let suggested = run.suggested_verdict.as_deref().unwrap_or("none");
    let preselect = if suggested == "request_changes" {
        "REQUEST_CHANGES"
    } else {
        "COMMENT"
    };
    let preview = format!("{}/runs/{}/preview", pr_href(&pr.key), run.id);
    html! {
        @if run.head_sha != pr.head_sha {
            p.warn {
                "This review is of " code { (short(&run.head_sha)) } "; the PR is now at "
                code { (short(&pr.head_sha)) } ". Its comments post against the older commit."
            }
        }
        @if diff.is_none() {
            p.warn { "This run's diff is gone, so drafts are shown without context." }
        }
        section #drafts {
            @for draft in drafts { (draft_card(app, draft, diff)) }
        }
        form.submit method="get" action=(preview) {
            fieldset {
                legend { "Verdict" }
                @for (value, label) in [
                    ("COMMENT", "Comment"),
                    ("REQUEST_CHANGES", "Request changes"),
                    ("APPROVE", "Approve"),
                ] {
                    label {
                        input type="radio" name="event" value=(value) checked[value == preselect];
                        " " (label)
                    }
                }
                p.dim {
                    "The agent suggests " (suggested.replace('_', " "))
                    ". It never suggests approving; that's only ever your pick."
                }
            }
            button type="submit" { "Preview the review" }
            " " span.dim { "Nothing is posted until you confirm on the next page." }
        }
    }
}

/// One draft, with its diff context and what you can do with it.
pub fn draft_card(app: &App, draft: &DraftRow, diff: Option<&DiffIndex>) -> Markup {
    let editable = matches!(draft.status.as_str(), "pending" | "accepted" | "rejected");
    let edit = format!("/drafts/{}/edit", draft.id);
    let status = format!("/drafts/{}/status", draft.id);
    let context = diff.and_then(|diff| diff::context(diff, draft));
    html! {
        article.draft.(draft.status) #{ "draft-" (draft.id) } {
            header {
                @if draft.kind == "summary" {
                    strong { "Summary" }
                } @else {
                    code { (anchor(draft)) }
                }
                @if let Some(severity) = &draft.severity { " " span.severity { (severity) } }
                @if let Some(confidence) = &draft.confidence {
                    " " span.dim { (confidence) " confidence" }
                }
                " " span.badge { (draft.status) }
                @if draft.edited_body.is_some() { " " span.dim { "edited" } }
            }
            @if draft.unanchored {
                p.warn {
                    "Not on a line of the diff, so GitHub won't take it inline. "
                    "If you accept it, it's posted in the review body."
                }
            }
            @if let Some(context) = context { (context) }
            // Queued behind each other, so an edit saved on blur lands before
            // the Accept click that caused the blur.
            form.edit method="post" action=(edit) hx-post=(edit) hx-trigger="change"
                hx-target="closest article" hx-swap="outerHTML" hx-sync="closest article:queue all" {
                (csrf_field(app))
                textarea name="body" rows="4" readonly[!editable] { (draft.body()) }
                @if editable { button.save type="submit" { "Save" } }
            }
            @if editable {
                form.decide method="post" action=(status) hx-post=(status)
                    hx-target="closest article" hx-swap="outerHTML"
                    hx-sync="closest article:queue all" {
                    (csrf_field(app))
                    @if draft.status != "accepted" {
                        button name="status" value="accepted" { "Accept" }
                    }
                    @if draft.status != "rejected" {
                        button name="status" value="rejected" { "Reject" }
                    }
                    @if draft.status != "pending" {
                        button name="status" value="pending" { "Undo" }
                    }
                    button type="button" disabled
                        title="Coming later: the runner can't resume an agent session yet." {
                        "Regenerate with instruction"
                    }
                }
            }
        }
    }
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
    let draft = {
        let store = app.store();
        let Some(draft) = store.draft_row(id)? else {
            return Err(Error::NotFound(format!("there's no draft {id}")));
        };
        if !change(&store).map_err(Error::pr(&draft.key))? {
            return Err(Error::Refused(format!(
                "draft {id} is {} and can't be changed",
                draft.status
            )));
        }
        store
            .draft_row(id)
            .map_err(Error::pr(&draft.key))?
            .unwrap_or(draft)
    };
    if headers.contains_key("hx-request") {
        let diff = read_diff(app, draft.run_id);
        return Ok(draft_card(app, &draft, diff.as_ref()).into_response());
    }
    let back = format!("{}?run={}#draft-{id}", pr_href(&draft.key), draft.run_id);
    Ok(Redirect::to(&back).into_response())
}

pub async fn confirm_review_now(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
) -> Result<Markup, Error> {
    let key = path.key()?;
    let overview = Overview::load(&app).map_err(Error::pr(&key))?;
    let why = overview
        .owed
        .iter()
        .find(|pr| pr.key == key)
        .and_then(|pr| Why::of(pr, overview.skipped.get(&key), app.manual_reviews));
    let href = pr_href(&key);
    let content = html! {
        h1 { "Review now" }
        @match why {
            None => {
                p { "There's nothing to start for " (github_link(&key)) ": only a review you owe "
                    "whose latest run failed or crashed, that's skipped or archived, or that "
                    "--manual-reviews is holding can be started by hand." }
                p { a #cancel href=(href) { "Back" } }
            }
            Some(why) => {
                p {
                    @match why {
                        Why::Skipped(reason) => { "Review this " (reason) "-skipped PR anyway, " }
                        Why::Failed => { "Rerun the review of " }
                        Why::Held => { "Start the held review of " }
                    }
                    (github_link(&key))
                    @if why == Why::Held { " now?" } @else { " at its current head?" }
                    " This spends tokens."
                }
                form #confirm method="post" action={ (href) "/review-now" } {
                    (csrf_field(&app))
                    button type="submit" { kbd { "y" } " Review now" }
                    " "
                    a #cancel href=(href) { kbd { "Esc" } " Cancel" }
                }
            }
        }
    };
    Ok(page::layout(&app, Kind::Confirm, "Review now", &content))
}

pub async fn review_now(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
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
    if Why::of(owed, overview.skipped.get(&key), app.manual_reviews).is_none() {
        return Err(Error::Refused(format!(
            "there's nothing to start for {}: its review isn't failed, held or skipped",
            key.url()
        )));
    }
    info!(url = %key.url(), "review requested from the dashboard");
    let href = pr_href(&key);
    app.control.review_now(key);
    Ok(Redirect::to(&href))
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
