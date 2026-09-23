//! The index: reviews you owe and your PRs, as the TUI shows them.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use axum::extract::{Query, State};
use color_eyre::eyre::Result;
use maud::{Markup, html};
use sanic_core::{
    clock::window_start,
    pr::PrKey,
    skip::{PrFacts, Skip},
    start::Why,
};
use sanic_store::{MyPr, OwedReview, RunCounts};
use serde::Deserialize;

use crate::{
    App, Error, Shared,
    page::{self, Kind, countdown, csrf_field, drafts, first_line, github_link, state_cell},
    pr_href,
};

/// What the index shows, read from the store and `serve`'s shared state.
pub struct Overview {
    pub owed: Vec<OwedReview>,
    pub mine: Vec<MyPr>,
    pub counts: RunCounts,
    /// How long until each debounced review is queued.
    pub waiting: HashMap<PrKey, Duration>,
    /// Owed reviews that aren't reviewed automatically, and why.
    pub skipped: HashMap<PrKey, Skip>,
    /// PRs with a review you haven't looked at yet.
    pub unseen: HashSet<PrKey>,
}

impl Overview {
    pub fn load(app: &App) -> Result<Self> {
        let now = Instant::now();
        let waiting = app
            .due
            .borrow()
            .iter()
            .map(|(key, due)| (key.clone(), due.saturating_duration_since(now)))
            .collect();
        let since = since(app);
        let store = app.store();
        let owed = store.owed_reviews(&app.me, since.as_deref())?;
        let skipped = {
            let skips = app.skips.borrow();
            owed.iter()
                .filter_map(|pr| {
                    let facts = PrFacts {
                        profile: &pr.profile,
                        title: &pr.title,
                        is_draft: pr.is_draft,
                        archived: pr.archived,
                        head_sha: &pr.head_sha,
                        head_reviewers: &pr.head_reviewers,
                        me: &app.me,
                    };
                    Some((pr.key.clone(), skips.decide(&facts)?))
                })
                .collect()
        };
        Ok(Self {
            skipped,
            mine: store.my_prs(&app.me, since.as_deref())?,
            counts: store.run_counts()?,
            unseen: store.unseen()?,
            owed,
            waiting,
        })
    }
}

/// The start of the recency window the lists are cut to.
fn since(app: &App) -> Option<String> {
    window_start(app.clock.now(), *app.window.borrow())
}

/// Just the reviews you owe, as [`Overview::load`] reads them.
pub fn owed_reviews(app: &App) -> Result<Vec<OwedReview>> {
    app.store().owed_reviews(&app.me, since(app).as_deref())
}

/// An owed review's status, as the TUI's status column words it, and the
/// CSS class it's shown with.
pub fn owed_status(
    pr: &OwedReview,
    overview: &Overview,
    manual_reviews: bool,
) -> (String, &'static str) {
    let latest = pr.latest_run.as_ref().map(|run| run.status.as_str());
    // A skip says why nothing will happen; a review waiting out the quiet
    // period is newer news than the last run.
    match (overview.waiting.get(&pr.key), latest) {
        _ if pr.archived => ("archived".into(), "dim"),
        _ if let Some(skip) = overview.skipped.get(&pr.key) => (skip.status(), "dim"),
        (Some(left), _) => (format!("waiting {}", countdown(left.as_secs())), "dim"),
        (None, None) => ("waiting".into(), "dim"),
        (None, Some("queued")) if manual_reviews => ("held".into(), "held"),
        (None, Some("queued")) => ("queued".into(), "held"),
        (None, Some("running")) => ("running".into(), "running"),
        (None, Some("succeeded")) => ("drafted".into(), "ok"),
        (None, Some(status @ ("failed" | "crashed"))) => (status.into(), "bad"),
        (None, Some(other)) => (other.into(), "dim"),
    }
}

/// Why a review of `pr` may be started by hand, as the TUI's `r` decides.
pub fn why(pr: &OwedReview, overview: &Overview, manual_reviews: bool) -> Option<Why> {
    let status = pr.latest_run.as_ref().map(|run| run.status.as_str());
    Why::of(overview.skipped.get(&pr.key), status, manual_reviews)
}

#[derive(Debug, Deserialize)]
pub struct IndexQuery {
    /// Show archived PRs too.
    #[serde(default)]
    archived: bool,
}

pub async fn index(
    State(app): State<Shared>,
    Query(query): Query<IndexQuery>,
) -> Result<Markup, Error> {
    let overview = Overview::load(&app)?;
    let content = html! {
        (status_line(&overview.counts, app.manual_reviews))
        (panes(&app, &overview, query.archived))
    };
    Ok(page::layout(&app, Kind::Index, "Dashboard", &content))
}

fn status_line(counts: &RunCounts, manual_reviews: bool) -> Markup {
    html! {
        p.status-line {
            @if manual_reviews { span.held { "manual reviews" } " · " }
            (counts.queued) " queued · " (counts.running) " running · "
            (counts.pending_drafts) " pending drafts"
        }
    }
}

/// Both lists. The page rereads them every few seconds, as the TUI
/// rereads the store.
fn panes(app: &App, overview: &Overview, show_archived: bool) -> Markup {
    let refresh = format!("/?archived={show_archived}");
    let owed: Vec<_> = overview
        .owed
        .iter()
        .filter(|pr| show_archived || !pr.archived)
        .collect();
    let mine: Vec<_> = overview
        .mine
        .iter()
        .filter(|pr| show_archived || !pr.archived)
        .collect();
    let hidden = |archived: usize| {
        if show_archived || archived == 0 {
            String::new()
        } else {
            format!(" · {archived} archived")
        }
    };
    let owed_archived = overview.owed.iter().filter(|pr| pr.archived).count();
    let mine_archived = overview.mine.iter().filter(|pr| pr.archived).count();
    html! {
        div #panes hx-get=(refresh) hx-trigger="every 5s" hx-select="#panes" hx-swap="outerHTML"
            data-show-archived=(show_archived) {
            section.pane #owed {
                h2 { "Reviews you owe (" (owed.len()) (hidden(owed_archived)) ")" }
                @if owed.is_empty() {
                    p.dim { "No reviews requested." }
                } @else {
                    ol.rows {
                        @for pr in owed { (owed_row(app, pr, overview)) }
                    }
                }
            }
            section.pane #mine {
                h2 { "Your PRs (" (mine.len()) (hidden(mine_archived)) ")" }
                @if mine.is_empty() {
                    p.dim { "No open PRs of yours." }
                } @else {
                    ol.rows {
                        @for pr in mine { (my_row(app, pr, overview)) }
                    }
                }
            }
            p.toggle {
                @if show_archived {
                    a #toggle-archived href="/?archived=false" { "Hide archived PRs" }
                } @else {
                    a #toggle-archived href="/?archived=true" { "Show archived PRs" }
                }
            }
        }
    }
}

fn owed_row(app: &App, pr: &OwedReview, overview: &Overview) -> Markup {
    let (label, class) = owed_status(pr, overview, app.manual_reviews);
    let why = why(pr, overview, app.manual_reviews);
    let href = pr_href(&pr.key);
    // Only the latest run's error: an older failure a later run replaced
    // doesn't need attention.
    let error = pr.latest_run.as_ref().and_then(|run| run.error.as_deref());
    html! {
        li.row.archived[pr.archived] data-href=(href)
            data-review-now=[why.as_ref().map(|_| format!("{href}/review-now"))]
            data-ignore={ (href) "/ignore" }
            data-chat=[pr.chat_run.map(|_| format!("{href}#chat"))] {
            (state_cell(pr.state))
            span.status.(class) { (label) }
            span.drafts { (drafts(pr.pending_drafts)) }
            (unseen(overview, &pr.key))
            (github_link(&pr.key))
            span.title-cell {
                a.title href=(href) { (pr.title) }
                " " span.author.dim { "(" (pr.author) ")" }
            }
            span.actions {
                @if why.is_some() {
                    a.button href={ (href) "/review-now" } { "Review now" }
                }
                a.button href={ (href) "/ignore" } { "Ignore by title" }
                @if pr.chat_run.is_some() { a.button href={ (href) "#chat" } { "Chat" } }
                (archive_form(app, &pr.key, pr.archived, "index"))
            }
            @if let Some(error) = error {
                div.error { (first_line(error)) }
            }
        }
    }
}

fn my_row(app: &App, pr: &MyPr, overview: &Overview) -> Markup {
    let href = pr_href(&pr.key);
    html! {
        li.row.archived[pr.archived] data-href=(href)
            data-chat=[pr.chat_run.map(|_| format!("{href}#chat"))] {
            // Its state is its status, as in the TUI.
            @if pr.archived {
                span.state.quiet { "archived" }
            } @else {
                (state_cell(pr.state))
            }
            span.drafts { (drafts(pr.pending_drafts)) }
            (unseen(overview, &pr.key))
            (github_link(&pr.key))
            span.title-cell {
                @if pr.is_draft { span.dim { "[draft] " } }
                a.title href=(href) { (pr.title) }
            }
            span.actions {
                @if pr.chat_run.is_some() { a.button href={ (href) "#chat" } { "Chat" } }
                (archive_form(app, &pr.key, pr.archived, "index"))
            }
        }
    }
}

/// The `new` mark, in a cell of its own either way so the columns line up.
fn unseen(overview: &Overview, key: &PrKey) -> Markup {
    html! {
        span.new-cell {
            @if overview.unseen.contains(key) {
                span.unseen title="a review you haven't looked at yet" { "new" }
            }
        }
    }
}

/// A button that archives the PR, or unarchives it, then goes back to
/// `next`: `index` or `pr`.
pub fn archive_form(app: &App, key: &PrKey, archived: bool, next: &str) -> Markup {
    html! {
        form.archive method="post" action={ (pr_href(key)) "/archive" } {
            (csrf_field(app))
            input type="hidden" name="archived" value=(!archived);
            input type="hidden" name="next" value=(next);
            button type="submit" { @if archived { "Unarchive" } @else { "Archive" } }
        }
    }
}
