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
    state::{PrState, Urgency},
};
use sanic_store::{MyPr, OwedReview};
use serde::Deserialize;

use crate::{
    App, Error, Shared,
    page::{self, Kind, countdown, csrf_field, drafts, first_line, keycap, pr_ref},
    pr_href,
};

/// What the index shows, read from the store and `serve`'s shared state.
pub struct Overview {
    pub owed: Vec<OwedReview>,
    pub mine: Vec<MyPr>,
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
    let latest = pr.latest_status();
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
    Why::of(
        overview.skipped.get(&pr.key),
        pr.latest_status(),
        manual_reviews,
    )
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
    let content = lists(&app, &overview, query.archived);
    Ok(page::layout(&app, Kind::Index, "Dashboard", &content))
}

/// Where an owed review sits in the index, by what it asks of you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwedGroup {
    /// Drafts to decide on, a review you haven't looked at, comments to
    /// answer, or a run that failed or is held.
    NeedsYou,
    /// A review is on its way: waiting out the quiet period, queued or
    /// running.
    InFlight,
    /// Nothing to do now: skipped, archived, or reviewed with nothing left.
    Quiet,
}

impl OwedGroup {
    pub fn of(pr: &OwedReview, overview: &Overview, manual_reviews: bool) -> Self {
        let status = pr.latest_status();
        let (label, class) = owed_status(pr, overview, manual_reviews);
        if pr.archived {
            Self::Quiet
        } else if pr.pending_drafts > 0
            || overview.unseen.contains(&pr.key)
            || pr.state.urgency() == Urgency::Act
            || run_needs_you(&label, class)
        {
            Self::NeedsYou
        } else if overview.skipped.contains_key(&pr.key) {
            Self::Quiet
        } else if matches!(status, None | Some("queued" | "running"))
            || overview.waiting.contains_key(&pr.key)
        {
            Self::InFlight
        } else {
            Self::Quiet
        }
    }
}

/// Where one of your PRs sits in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MyGroup {
    /// Comments to answer, changes requested, or drafts.
    NeedsYou,
    /// Approved or mergeable.
    Ready,
    WaitingOnReviewers,
}

impl MyGroup {
    pub fn of(pr: &MyPr) -> Self {
        match pr.state.urgency() {
            Urgency::Act => Self::NeedsYou,
            _ if pr.pending_drafts > 0 => Self::NeedsYou,
            Urgency::Good => Self::Ready,
            Urgency::Quiet => Self::WaitingOnReviewers,
        }
    }
}

/// Whether the status [`owed_status`] shows is a run to act on: one that
/// failed or crashed, or one `--manual-reviews` holds. A skip or a wait
/// out the quiet period, shown in the run's place, says there's nothing
/// to act on yet.
fn run_needs_you(label: &str, class: &str) -> bool {
    class == "bad" || (class == "held" && label == "held")
}

/// A row's lead: the one thing to look at next, and how it's shown.
struct Lead {
    text: String,
    class: &'static str,
}

/// The pressing part of a state, e.g. `2 unanswered` of `approved · 2
/// unanswered`: the last of its words, as they run least pressing first.
fn pressing(state: PrState) -> String {
    let status = state.status();
    status.rsplit(" · ").next().unwrap_or(&status).to_owned()
}

fn owed_lead(pr: &OwedReview, label: &str, class: &'static str, run_first: bool) -> Lead {
    if run_first {
        Lead {
            text: label.to_owned(),
            class,
        }
    } else if pr.pending_drafts > 0 {
        Lead {
            text: drafts(pr.pending_drafts),
            class: "cnt",
        }
    } else if pr.state.urgency() == Urgency::Act {
        Lead {
            text: pressing(pr.state),
            class: "chip u-act",
        }
    } else {
        Lead {
            text: label.to_owned(),
            class,
        }
    }
}

fn my_lead(pr: &MyPr) -> Lead {
    if pr.archived {
        return Lead {
            text: "archived".into(),
            class: "chip dim",
        };
    }
    match pr.state.urgency() {
        Urgency::Act => Lead {
            text: pressing(pr.state),
            class: "chip u-act",
        },
        _ if pr.pending_drafts > 0 => Lead {
            text: drafts(pr.pending_drafts),
            class: "cnt",
        },
        Urgency::Good => {
            let status = pr.state.status();
            Lead {
                text: status.split(" · ").next().unwrap_or(&status).to_owned(),
                class: "chip u-good",
            }
        }
        Urgency::Quiet if pr.is_draft => Lead {
            text: "draft PR".into(),
            class: "chip dim",
        },
        Urgency::Quiet => Lead {
            text: "waiting".into(),
            class: "chip dim",
        },
    }
}

/// The state's words the lead didn't already say, for the second line.
fn other_words(state: PrState, lead: &str) -> Vec<String> {
    if state.is_blank() {
        return Vec::new();
    }
    state
        .status()
        .split(" · ")
        .filter(|word| *word != lead)
        .map(str::to_owned)
        .collect()
}

/// Both lists, grouped by what they ask of you. The page rereads them, and
/// the counts, every few seconds, as the TUI rereads the store.
fn lists(app: &App, overview: &Overview, show_archived: bool) -> Markup {
    let refresh = format!("/?archived={show_archived}");
    html! {
        div #panes hx-get=(refresh) hx-trigger="every 5s" hx-select="#panes"
            hx-select-oob="#counts" hx-swap="outerHTML" data-show-archived=(show_archived) {
            (owed_list(app, overview, show_archived))
            (my_list(app, overview, show_archived))
            p.help-foot {
                (keycap("j")) (keycap("k")) " move (a folded group is skipped) · "
                (keycap("Tab")) " other list · " (keycap("Enter")) " open · "
                (keycap("r")) (keycap("a")) (keycap("i")) (keycap("c"))
                " act on the selected row · " (keycap("A")) " archived · "
                (keycap("?")) " all keys"
            }
        }
    }
}

/// The list heading's link that shows or hides archived PRs.
fn archived_toggle(show_archived: bool, hidden: usize) -> Markup {
    html! {
        span.sp {
            @if show_archived {
                a.toggle-archived href="/?archived=false" { "hide archived" }
                " " (keycap("A"))
            } @else if hidden > 0 {
                a.toggle-archived href="/?archived=true" { "show " (hidden) " archived" }
                " " (keycap("A"))
            }
        }
    }
}

fn owed_list(app: &App, overview: &Overview, show_archived: bool) -> Markup {
    let shown: Vec<(OwedGroup, &OwedReview)> = overview
        .owed
        .iter()
        .filter(|pr| show_archived || !pr.archived)
        .map(|pr| (OwedGroup::of(pr, overview, app.manual_reviews), pr))
        .collect();
    let group = |group| -> Vec<&OwedReview> {
        let mut prs: Vec<_> = shown
            .iter()
            .filter(|(g, _)| *g == group)
            .map(|(_, pr)| *pr)
            .collect();
        // New reviews first; otherwise the store's order.
        prs.sort_by_key(|pr| !overview.unseen.contains(&pr.key));
        prs
    };
    let (need, flight, quiet) = (
        group(OwedGroup::NeedsYou),
        group(OwedGroup::InFlight),
        group(OwedGroup::Quiet),
    );
    let archived = overview.owed.iter().filter(|pr| pr.archived).count();
    html! {
        section.list #owed {
            h2 {
                "Reviews you owe " span.dim { (shown.len()) }
                (archived_toggle(show_archived, archived))
            }
            @if shown.is_empty() { p.dim { "No reviews requested." } }
            @if !need.is_empty() {
                div.group-h.act { "NEEDS YOU · " (need.len()) }
                @for pr in &need { (owed_row(app, pr, overview)) }
            }
            @if !flight.is_empty() {
                div.group-h {
                    "IN FLIGHT · " (flight.len())
                    span.dim { " — nothing to do; it shows up above when drafted" }
                }
                @for pr in &flight { (owed_row(app, pr, overview)) }
            }
            @if !quiet.is_empty() {
                details.grp.quiet #owed-quiet {
                    summary {
                        div.group-h {
                            "NOTHING TO DO NOW · " (quiet.len())
                            span.dim { " — skipped, archived, or done; r reviews one anyway" }
                        }
                    }
                    @for pr in &quiet { (owed_row(app, pr, overview)) }
                }
            }
        }
    }
}

fn my_list(app: &App, overview: &Overview, show_archived: bool) -> Markup {
    let shown: Vec<&MyPr> = overview
        .mine
        .iter()
        .filter(|pr| show_archived || !pr.archived)
        .collect();
    let group = |group: Option<MyGroup>| -> Vec<&MyPr> {
        shown
            .iter()
            .filter(|pr| {
                if pr.archived {
                    group.is_none()
                } else {
                    group == Some(MyGroup::of(pr))
                }
            })
            .copied()
            .collect()
    };
    let archived = overview.mine.iter().filter(|pr| pr.archived).count();
    html! {
        section.list #mine {
            h2 {
                "Your PRs " span.dim { (shown.len()) }
                (archived_toggle(show_archived, archived))
            }
            @if shown.is_empty() { p.dim { "No open PRs of yours." } }
            @for (which, heading, act) in [
                (Some(MyGroup::NeedsYou), "NEEDS YOU", true),
                (Some(MyGroup::Ready), "READY", false),
                (Some(MyGroup::WaitingOnReviewers), "WAITING ON REVIEWERS", false),
                (None, "ARCHIVED", false),
            ] {
                @let prs = group(which);
                @if !prs.is_empty() {
                    div.group-h.act[act] { (heading) " · " (prs.len()) }
                    @for pr in &prs { (my_row(app, pr, overview)) }
                }
            }
        }
    }
}

/// The `new` mark.
fn unseen(overview: &Overview, key: &PrKey) -> Markup {
    html! {
        span.n {
            @if overview.unseen.contains(key) {
                span.newdot title="a review you haven't looked at yet" {}
            }
        }
    }
}

fn owed_row(app: &App, pr: &OwedReview, overview: &Overview) -> Markup {
    let (label, class) = owed_status(pr, overview, app.manual_reviews);
    let run_first = run_needs_you(&label, class);
    let class = match class {
        "ok" => "chip ok",
        "bad" => "chip bad",
        "held" => "chip held",
        "running" => "chip running",
        _ => "chip dim",
    };
    let lead = owed_lead(pr, &label, class, run_first);
    let why = why(pr, overview, app.manual_reviews);
    let href = pr_href(&pr.key);
    // Only the latest run's error: an older failure a later run replaced
    // doesn't need attention.
    let error = pr.latest_run.as_ref().and_then(|run| run.error.as_deref());
    let mut rest: Vec<Markup> = Vec::new();
    if pr.pending_drafts > 0 && lead.class != "cnt" {
        rest.push(html! { span.cnt { (drafts(pr.pending_drafts)) } });
    }
    if label != lead.text {
        rest.push(html! { (label) });
    }
    for word in other_words(pr.state, &lead.text) {
        rest.push(html! { span.(state_class(pr.state)) { (word) } });
    }
    let meta = html! {
        (pr_ref(&pr.key)) " · " (pr.author)
        @for part in &rest { " · " (part) }
        @if let Some(error) = error {
            " · " span.error title=(error) { (first_line(error)) }
        }
    };
    let actions = html! {
        @if why.is_some() {
            a.linkbtn href={ (href) "/review-now" } { "review now " (keycap("r")) }
        }
        a.linkbtn href={ (href) "/ignore" } { "ignore by title " (keycap("i")) }
        @if pr.chat_run.is_some() {
            a.linkbtn href={ (href) "#chat" } { "chat " (keycap("c")) }
        }
        (archive_form(app, &pr.key, pr.archived, "index"))
    };
    html! {
        // Selection follows the PR across refreshes, not the row's place.
        div.ib.archived[pr.archived] data-row data-key=(pr.key) data-href=(href)
            data-review-now=[why.as_ref().map(|_| format!("{href}/review-now"))]
            data-ignore={ (href) "/ignore" }
            data-chat=[pr.chat_run.map(|_| format!("{href}#chat"))] {
            (unseen(overview, &pr.key))
            span.x { span.(lead.class) { (lead.text) } }
            span.t { a href=(href) title=(pr.title) { (pr.title) } }
            span.m { (meta) }
            span.a { (actions) }
        }
    }
}

fn my_row(app: &App, pr: &MyPr, overview: &Overview) -> Markup {
    let href = pr_href(&pr.key);
    let lead = my_lead(pr);
    let mut rest: Vec<Markup> = Vec::new();
    if pr.pending_drafts > 0 && lead.class != "cnt" {
        rest.push(html! { span.cnt { (drafts(pr.pending_drafts)) } });
    }
    if pr.is_draft && lead.text != "draft PR" {
        rest.push(html! { "draft PR" });
    }
    for word in other_words(pr.state, &lead.text) {
        rest.push(html! { span.(state_class(pr.state)) { (word) } });
    }
    html! {
        div.ib.archived[pr.archived] data-row data-key=(pr.key) data-href=(href)
            data-chat=[pr.chat_run.map(|_| format!("{href}#chat"))] {
            (unseen(overview, &pr.key))
            span.x { span.(lead.class) { (lead.text) } }
            span.t { a href=(href) title=(pr.title) { (pr.title) } }
            span.m {
                (pr_ref(&pr.key))
                @for part in &rest { " · " (part) }
            }
            span.a {
                @if pr.chat_run.is_some() {
                    a.linkbtn href={ (href) "#chat" } { "chat " (keycap("c")) }
                }
                (archive_form(app, &pr.key, pr.archived, "index"))
            }
        }
    }
}

/// The CSS class a state's words take, by its urgency.
fn state_class(state: PrState) -> &'static str {
    match state.urgency() {
        Urgency::Act => "u-act",
        Urgency::Good => "u-good",
        Urgency::Quiet => "u-quiet",
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
            @if next == "index" {
                button.linkbtn type="submit" {
                    @if archived { "unarchive " } @else { "archive " } (keycap("a"))
                }
            } @else {
                button.btn type="submit" {
                    @if archived { "Unarchive" } @else { "Archive" } (keycap("a"))
                }
            }
        }
    }
}
