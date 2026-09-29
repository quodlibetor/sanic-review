//! The index: reviews you owe and your PRs, as the TUI shows them.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use axum::{
    Form,
    extract::{RawQuery, State},
    http::HeaderName,
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::Result;
use maud::{Markup, html};
use sanic_core::{
    clock::{WindowChoice, window_start},
    manual::ManualReviews,
    pr::PrKey,
    skip::{PrFacts, Skip},
    start::Why,
    state::{Approval, Block, Checks, Merge, PrState, Urgency},
};
use sanic_store::{Decided, List, MyPr, OwedReview, RowFacts};
use serde::Deserialize;
use tracing::info;

use crate::{
    App, Error, Shared,
    cells::{LeadPop, ReviewedBy, plural},
    filter::{self, Facet, IndexQuery, RowValues, Side, count, hidden_line},
    page::{self, Kind, countdown, csrf_field, drafts, keycap, pr_ref},
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
    /// What the index shows of each listed PR beyond its list entry; see
    /// [`Overview::load_facts`].
    pub facts: HashMap<PrKey, RowFacts>,
    /// How many PRs of each list the recency window hides, as the last
    /// reconcile under it counted; also loaded by `load_facts`.
    pub hidden: Hidden,
    /// What manual reviews hold, read once so a reload midway can't split
    /// the page.
    pub manual: ManualReviews,
}

/// Open PRs the recency window leaves out of each list, if counted.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hidden {
    pub owed: Option<u32>,
    pub mine: Option<u32>,
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
            facts: HashMap::new(),
            hidden: Hidden::default(),
            manual: app.manual_reviews(),
        })
    }

    /// Reads [`Overview::facts`], which only the index needs.
    pub fn load_facts(&mut self, app: &App) -> Result<()> {
        let store = app.store();
        let keys = self.owed.iter().map(|pr| &pr.key);
        for key in keys.chain(self.mine.iter().map(|pr| &pr.key)) {
            self.facts
                .insert(key.clone(), store.row_facts(key, &app.me)?);
        }
        let days = app.window.borrow().days();
        self.hidden = Hidden {
            owed: store.hidden(List::Owed, days)?,
            mine: store.hidden(List::Mine, days)?,
        };
        Ok(())
    }

    fn decided(&self, key: &PrKey) -> Option<&Decided> {
        self.facts.get(key)?.decided.as_ref()
    }

    /// Accepted drafts not yet posted.
    fn to_post(&self, key: &PrKey) -> u32 {
        self.decided(key).map_or(0, |d| d.accepted)
    }

    fn merge(&self, key: &PrKey) -> Option<Merge> {
        self.facts.get(key).map(|f| f.merge)
    }

    /// Every draft rejected and no review of yours, with no newer review
    /// on its way, queued, running or waiting out the quiet period.
    fn submit_review(&self, key: &PrKey) -> bool {
        !self.waiting.contains_key(key) && self.decided(key).is_some_and(Decided::submit_review)
    }
}

/// The start of the recency window the lists are cut to.
pub(crate) fn since(app: &App) -> Option<String> {
    window_start(app.clock.now(), app.window.borrow().days())
}

/// Just the reviews you owe, as [`Overview::load`] reads them.
pub fn owed_reviews(app: &App) -> Result<Vec<OwedReview>> {
    app.store().owed_reviews(&app.me, since(app).as_deref())
}

/// An owed review's status, as the TUI's status column words it, and the
/// CSS class it's shown with.
pub fn owed_status(pr: &OwedReview, overview: &Overview) -> (String, &'static str) {
    let latest = pr.latest_status();
    // A skip says why nothing will happen; a review waiting out the quiet
    // period is newer news than the last run.
    match (overview.waiting.get(&pr.key), latest) {
        _ if pr.archived => ("archived".into(), "dim"),
        _ if let Some(skip) = overview.skipped.get(&pr.key) => (skip.status(), "dim"),
        (Some(left), _) => (format!("waiting {}", countdown(left.as_secs())), "dim"),
        (None, None) => ("waiting".into(), "dim"),
        (None, Some("queued")) if pr.held(&overview.manual) => ("held".into(), "held"),
        (None, Some("queued")) => ("queued".into(), "held"),
        (None, Some("running")) => ("running".into(), "running"),
        (None, Some("succeeded")) if pr.latest_run.as_ref().is_some_and(|r| r.no_update) => {
            ("no update".into(), "ok")
        }
        (None, Some("succeeded")) => ("drafted".into(), "ok"),
        (None, Some(status @ ("failed" | "crashed"))) => (status.into(), "bad"),
        (None, Some(other)) => (other.into(), "dim"),
    }
}

/// Why a review of `pr` may be started by hand, as the TUI's `r` decides.
pub fn why(pr: &OwedReview, overview: &Overview) -> Option<Why> {
    Why::of(
        overview.skipped.get(&pr.key),
        pr.latest_status(),
        pr.held(&overview.manual),
    )
}

/// Tells htmx the address bar's URL once the sidebar's request swaps in.
const HX_REPLACE_URL: HeaderName = HeaderName::from_static("hx-replace-url");

pub async fn index(
    State(app): State<Shared>,
    RawQuery(raw): RawQuery,
) -> Result<impl IntoResponse, Error> {
    let query = IndexQuery::parse(raw.as_deref()).map_err(Error::BadRequest)?;
    let mut overview = Overview::load(&app)?;
    overview.load_facts(&app)?;
    let listed = Listed::of(&overview, query.archived);
    let values: Vec<RowValues> = listed
        .owed
        .iter()
        .map(|l| l.values.clone())
        .chain(listed.mine.iter().map(|l| l.values.clone()))
        .collect();
    // The top bar counts the pending drafts of the rows the filter leaves.
    let pending = query.filter.is_active().then(|| {
        values
            .iter()
            .filter(|v| query.filter.matches(v))
            .map(|v| v.pending)
            .sum()
    });
    let content = html! {
        (filter::sidebar(&query, &values, &app.me))
        (lists(&app, &overview, &listed, &query))
    };
    let page = page::layout(
        &app,
        Kind::Index {
            archived: query.archived,
            pending,
        },
        "Dashboard",
        &content,
    );
    Ok(([(HX_REPLACE_URL, query.href())], page))
}

/// The rows the lists show before the filter, in the store's order, each
/// with its group and what the filter sees of it.
struct Listed<'a> {
    owed: Vec<Listing<'a, OwedReview, OwedGroup>>,
    /// `None` is archived.
    mine: Vec<Listing<'a, MyPr, Option<MyGroup>>>,
}

struct Listing<'a, P, G> {
    pr: &'a P,
    group: G,
    values: RowValues<'a>,
}

impl<'a> Listed<'a> {
    fn of(overview: &'a Overview, show_archived: bool) -> Self {
        let reviewers = |key: &PrKey| -> Vec<&'a str> {
            overview.facts.get(key).map_or_else(Vec::new, |f| {
                f.reviewers.iter().map(|r| r.login.as_str()).collect()
            })
        };
        let states = |key: &PrKey, state: PrState, is_draft: bool| {
            row_states(
                state,
                overview.merge(key),
                is_draft,
                overview.unseen.contains(key),
            )
        };
        let owed = overview
            .owed
            .iter()
            .filter(|pr| show_archived || !pr.archived)
            .map(|pr| {
                let group = OwedGroup::of(pr, overview);
                let values = RowValues {
                    list: Side::Owed,
                    status: group.status(),
                    author: Some(&pr.author),
                    reviewers: reviewers(&pr.key),
                    repo: pr.key.repo.to_string(),
                    states: states(&pr.key, pr.state, pr.is_draft),
                    title: &pr.title,
                    reference: pr.key.to_string(),
                    pending: pr.pending_drafts,
                };
                Listing { pr, group, values }
            })
            .collect();
        let mine = overview
            .mine
            .iter()
            .filter(|pr| show_archived || !pr.archived)
            .map(|pr| {
                let group = (!pr.archived).then(|| MyGroup::of(pr, overview));
                let values = RowValues {
                    list: Side::Mine,
                    status: group.map_or("archived", MyGroup::status),
                    author: None,
                    reviewers: reviewers(&pr.key),
                    repo: pr.key.repo.to_string(),
                    states: states(&pr.key, pr.state, pr.is_draft),
                    title: &pr.title,
                    reference: pr.key.to_string(),
                    pending: pr.pending_drafts,
                };
                Listing { pr, group, values }
            })
            .collect();
        Self { owed, mine }
    }
}

/// What the State facet says of a row: its approval, failing CI,
/// conflicts, whether it's a draft, and a review you haven't looked at.
fn row_states(
    state: PrState,
    merge: Option<Merge>,
    is_draft: bool,
    unseen: bool,
) -> Vec<&'static str> {
    let approval = match state.approval {
        Approval::Approved(_) => Some("approved"),
        Approval::ChangesRequested => Some("changes-requested"),
        Approval::Mergeable => Some("mergeable"),
        Approval::None => None,
    };
    let ci = merge.is_some_and(|m| m.ci == Checks::Failing);
    let conflicts = merge.is_some_and(|m| m.block == Some(Block::Conflicts));
    approval
        .into_iter()
        .chain(ci.then_some("ci-failing"))
        .chain(conflicts.then_some("conflicts"))
        .chain(is_draft.then_some("draft-pr"))
        .chain(unseen.then_some("unseen"))
        .collect()
}

/// Where an owed review sits in the index, by what it asks of you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwedGroup {
    /// Drafts to decide on or post, a review you haven't looked at or
    /// have yet to write, comments to answer, or a run that failed or is
    /// held.
    NeedsYou,
    /// A review is on its way: waiting out the quiet period, queued or
    /// running.
    InFlight,
    /// Nothing to do now: skipped, archived, or reviewed with nothing left.
    Quiet,
}

impl OwedGroup {
    /// Its [`filter::STATUSES`] value.
    fn status(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs-you",
            Self::InFlight => "in-flight",
            Self::Quiet => "nothing-to-do",
        }
    }

    pub fn of(pr: &OwedReview, overview: &Overview) -> Self {
        let status = pr.latest_status();
        let (label, class) = owed_status(pr, overview);
        if pr.archived {
            Self::Quiet
        } else if pr.pending_drafts > 0
            || overview.unseen.contains(&pr.key)
            || pr.state.urgency() == Urgency::Act
            || run_needs_you(&label, class)
            || overview.to_post(&pr.key) > 0
            || overview.submit_review(&pr.key)
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
    /// Comments to answer, changes requested, drafts to decide on or
    /// post, or an approval held up by failing CI or conflicts.
    NeedsYou,
    /// Mergeable, or approved and waiting only on CI or its base.
    Ready,
    /// Not approved, or approved but blocked by something else GitHub
    /// requires, such as another review.
    WaitingOnReviewers,
}

impl MyGroup {
    /// Its [`filter::STATUSES`] value.
    fn status(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs-you",
            Self::Ready => "ready",
            Self::WaitingOnReviewers => "waiting",
        }
    }

    pub fn of(pr: &MyPr, overview: &Overview) -> Self {
        match pr.state.urgency() {
            Urgency::Act => Self::NeedsYou,
            _ if pr.pending_drafts > 0 || overview.to_post(&pr.key) > 0 => Self::NeedsYou,
            Urgency::Good => match approved_block(pr.state, overview.merge(&pr.key)) {
                Some(Block::Conflicts | Block::CiFailing) => Self::NeedsYou,
                Some(Block::Blocked) => Self::WaitingOnReviewers,
                Some(Block::CiPending | Block::Behind) | None => Self::Ready,
            },
            Urgency::Quiet => Self::WaitingOnReviewers,
        }
    }
}

/// Why an approved PR can't merge yet; `None` if it isn't approved, or
/// can merge.
fn approved_block(state: PrState, merge: Option<Merge>) -> Option<Block> {
    match state.approval {
        Approval::Approved(_) => merge?.block,
        _ => None,
    }
}

/// Whether the status [`owed_status`] shows is a run to act on: one that
/// failed or crashed, or one manual reviews hold. A skip or a wait
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

/// What your own PR's state asks of you, as a lead: comments to answer,
/// else the changes requested.
fn pressing(state: PrState) -> String {
    if state.unanswered > 0 {
        format!("{} to answer", state.unanswered)
    } else {
        "changes requested".into()
    }
}

fn owed_lead(pr: &OwedReview, overview: &Overview, label: &str, class: &'static str) -> Lead {
    let lead = |text: String, class| Lead { text, class };
    let decided = overview.decided(&pr.key);
    // A newer review on its way says more than the last one's drafts.
    let done = pr.latest_status() == Some("succeeded") && !overview.waiting.contains_key(&pr.key);
    if run_needs_you(label, class) {
        lead(label.to_owned(), chip(class))
    } else if pr.pending_drafts > 0 {
        lead(drafts(pr.pending_drafts), "cnt")
    } else if pr.state.urgency() == Urgency::Act {
        lead(pressing(pr.state), "chip u-act")
    } else if let Some(n) = decided.map(|d| d.accepted).filter(|n| *n > 0) {
        lead(format!("{n} to post"), "sig post")
    } else if overview.submit_review(&pr.key) {
        lead("submit review".into(), "sig post")
    } else if done && decided.is_some_and(|d| d.posted > 0) {
        lead("posted".into(), "chip ok")
    } else {
        lead(label.to_owned(), chip(class))
    }
}

/// The chip a status's CSS class is shown as.
fn chip(class: &str) -> &'static str {
    match class {
        "ok" => "chip ok",
        "bad" => "chip bad",
        "held" => "chip held",
        "running" => "chip running",
        _ => "chip dim",
    }
}

fn my_lead(pr: &MyPr, overview: &Overview) -> Lead {
    let lead = |text: String, class| Lead { text, class };
    if pr.archived {
        return lead("archived".into(), "chip dim");
    }
    let to_post = overview.to_post(&pr.key);
    match pr.state.urgency() {
        Urgency::Act => lead(pressing(pr.state), "chip u-act"),
        _ if pr.pending_drafts > 0 => lead(drafts(pr.pending_drafts), "cnt"),
        _ if to_post > 0 => lead(format!("{to_post} to post"), "sig post"),
        Urgency::Good => {
            let block = approved_block(pr.state, overview.merge(&pr.key));
            if let Some(block @ (Block::Conflicts | Block::CiFailing)) = block {
                return lead(block.word().into(), "chip bad");
            }
            let status = pr.state.status();
            let first = status.split(" · ").next().unwrap_or(&status);
            lead(first.to_owned(), "chip u-good")
        }
        Urgency::Quiet if pr.is_draft => lead("draft PR".into(), "chip dim"),
        Urgency::Quiet => lead("waiting".into(), "chip dim"),
    }
}

/// [`owed_status`] as the index words it: a PR skipped because someone
/// reviewed its head is just `skipped`, since the row says who.
fn index_status(pr: &OwedReview, overview: &Overview) -> (String, &'static str) {
    match overview.skipped.get(&pr.key) {
        Some(Skip::Reviewed { .. }) if !pr.archived => ("skipped".into(), "dim"),
        _ => owed_status(pr, overview),
    }
}

/// Both lists, grouped by what they ask of you. The page rereads them, and
/// the counts, every few seconds, as the TUI rereads the store.
fn lists(app: &App, overview: &Overview, listed: &Listed, query: &IndexQuery) -> Markup {
    // X shows or hides archived PRs, keeping the filter.
    let toggled = query.with_archived(!query.archived).href();
    html! {
        div #panes hx-get=(query.href()) hx-trigger="every 5s" hx-select="#panes"
            hx-select-oob=(filter::REFRESHED) hx-swap="outerHTML" data-archived-href=(toggled) {
            (owed_list(app, overview, &listed.owed, query))
            (my_list(app, overview, &listed.mine, query))
            p.help-foot {
                (keycap("j")) (keycap("k")) " move (a folded group is skipped) · "
                (keycap("Tab")) " other list · " (keycap("Enter")) " open · "
                (keycap("r")) (keycap("x")) (keycap("i")) (keycap("c"))
                " act on the selected row · " (keycap("v")) " reviewers · "
                (keycap("d")) " details · " (keycap("X")) " archived · "
                (keycap("?")) " all keys"
            }
        }
    }
}

/// The list heading's link that shows or hides archived PRs.
fn archived_toggle(query: &IndexQuery, hidden: usize) -> Markup {
    let toggled = query.with_archived(!query.archived).href();
    html! {
        span.sp {
            @if query.archived {
                a.toggle-archived href=(toggled) { "hide archived" }
                " " (keycap("X"))
            } @else if hidden > 0 {
                a.toggle-archived href=(toggled) { "show " (hidden) " archived" }
                " " (keycap("X"))
            }
        }
    }
}

fn owed_list(
    app: &App,
    overview: &Overview,
    listed: &[Listing<OwedReview, OwedGroup>],
    query: &IndexQuery,
) -> Markup {
    let shown: Vec<&Listing<OwedReview, OwedGroup>> = listed
        .iter()
        .filter(|l| query.filter.matches(&l.values))
        .collect();
    // The group's rows the filter leaves, and how many it has without it.
    let group = |group| -> (Vec<&OwedReview>, usize) {
        let mut prs: Vec<_> = shown
            .iter()
            .filter(|l| l.group == group)
            .map(|l| l.pr)
            .collect();
        // New reviews first; otherwise the store's order.
        prs.sort_by_key(|pr| !overview.unseen.contains(&pr.key));
        (prs, listed.iter().filter(|l| l.group == group).count())
    };
    let (need, flight, quiet) = (
        group(OwedGroup::NeedsYou),
        group(OwedGroup::InFlight),
        group(OwedGroup::Quiet),
    );
    let archived = overview.owed.iter().filter(|pr| pr.archived).count();
    let back = query.href();
    html! {
        section.list #owed {
            h2 {
                "Reviews you owe " span.dim { (count(shown.len(), listed.len())) }
                (archived_toggle(query, archived))
            }
            (hidden_line(query, listed.len() - shown.len(), None))
            @if listed.is_empty() { p.dim { "No reviews requested." } }
            @else if !shown.is_empty() { (columns()) }
            @if !need.0.is_empty() {
                div.group-h.act { "NEEDS YOU · " (count(need.0.len(), need.1)) }
                @for pr in &need.0 { (owed_row(app, pr, overview, &back)) }
            }
            @if !flight.0.is_empty() {
                div.group-h {
                    "IN FLIGHT · " (count(flight.0.len(), flight.1))
                    span.dim { " — nothing to do; it shows up above when drafted" }
                }
                @for pr in &flight.0 { (owed_row(app, pr, overview, &back)) }
            }
            @if !quiet.0.is_empty() {
                details.grp.quiet #owed-quiet {
                    summary {
                        div.group-h {
                            "NOTHING TO DO NOW · " (count(quiet.0.len(), quiet.1))
                            span.dim { " — skipped, archived, or done; r reviews one anyway" }
                        }
                    }
                    @for pr in &quiet.0 { (owed_row(app, pr, overview, &back)) }
                }
            }
            (hidden_foot(app, overview.hidden.owed, query))
        }
    }
}

fn my_list(
    app: &App,
    overview: &Overview,
    listed: &[Listing<MyPr, Option<MyGroup>>],
    query: &IndexQuery,
) -> Markup {
    let shown: Vec<&Listing<MyPr, Option<MyGroup>>> = listed
        .iter()
        .filter(|l| query.filter.matches(&l.values))
        .collect();
    let group = |group: Option<MyGroup>| -> (Vec<&MyPr>, usize) {
        let prs = shown
            .iter()
            .filter(|l| l.group == group)
            .map(|l| l.pr)
            .collect();
        (prs, listed.iter().filter(|l| l.group == group).count())
    };
    let archived = overview.mine.iter().filter(|pr| pr.archived).count();
    // Your PRs are all yours, so an author picked leaves them all.
    let note = query
        .filter
        .picked(Facet::Author)
        .next()
        .is_some()
        .then_some("author isn't applied here: these are all yours");
    let back = query.href();
    html! {
        section.list #mine {
            h2 {
                "Your PRs " span.dim { (count(shown.len(), listed.len())) }
                (archived_toggle(query, archived))
            }
            (hidden_line(query, listed.len() - shown.len(), note))
            @if listed.is_empty() { p.dim { "No open PRs of yours." } }
            @else if !shown.is_empty() { (columns()) }
            @for (which, heading, act) in [
                (Some(MyGroup::NeedsYou), "NEEDS YOU", true),
                (Some(MyGroup::Ready), "READY", false),
                (Some(MyGroup::WaitingOnReviewers), "WAITING ON REVIEWERS", false),
                (None, "ARCHIVED", false),
            ] {
                @let (prs, all) = group(which);
                @if !prs.is_empty() {
                    div.group-h.act[act] { (heading) " · " (count(prs.len(), all)) }
                    @for pr in &prs { (my_row(app, pr, overview, &back)) }
                }
            }
            (hidden_foot(app, overview.hidden.mine, query))
        }
    }
}

/// The lists' column heads, over the status and PR cells.
fn columns() -> Markup {
    html! {
        div.colh aria-hidden="true" {
            span {} span {} span { "REVIEWED BY · WAITING ON" } span { "PR" }
        }
    }
}

/// Windows the hidden-PRs line offers, by their words.
const WINDOWS: &[(WindowChoice, &str)] = &[
    (WindowChoice::Days(30), "1 month"),
    (WindowChoice::Days(180), "6 months"),
    (WindowChoice::All, "all"),
];

/// Under a list, dim: how many older PRs the recency window hides, and
/// wider windows to pick, or the way back to the configured one. Nothing
/// while the configured window is in force and hides nothing.
fn hidden_foot(app: &App, hidden: Option<u32>, query: &IndexQuery) -> Markup {
    let window = *app.window.borrow();
    let wider = |choice: WindowChoice| match (choice.days(), window.days()) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(days), Some(now)) => days > now,
    };
    let offered: Vec<&(WindowChoice, &str)> = WINDOWS
        .iter()
        .filter(|(choice, _)| wider(*choice))
        .collect();
    let hidden = hidden.filter(|n| *n > 0);
    let pick = |choice: &str, words: &str| {
        html! {
            form.window method="post" action="/window" {
                (csrf_field(app))
                // Back to the index as it was.
                input type="hidden" name="back" value=(query.href());
                input type="hidden" name="window" value=(choice);
                button.linkbtn type="submit" { (words) }
            }
        }
    };
    html! {
        @if hidden.is_some() || window.choice.is_some() {
            // A div: the forms in it would end a paragraph.
            div.hidden-foot {
                @if let Some(n) = hidden {
                    (plural(n, "older PR", "older PRs")) " hidden"
                } @else if window.days().is_none() {
                    "showing PRs of any age"
                } @else {
                    "showing the last " (window.days().unwrap_or_default()) " days"
                }
                @if !offered.is_empty() {
                    " · show last "
                    @for (i, (choice, words)) in offered.iter().enumerate() {
                        @if i > 0 { " · " }
                        (pick(&choice.as_str(), words))
                    }
                }
                @if window.choice.is_some() {
                    " · " (pick("default", "back to default"))
                    @if let Some(days) = window.configured { " (" (days) " days)" }
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

/// `back` is the index's URL, which archiving goes back to.
fn owed_row(app: &App, pr: &OwedReview, overview: &Overview, back: &str) -> Markup {
    let (label, class) = index_status(pr, overview);
    let lead = owed_lead(pr, overview, &label, class);
    let why = why(pr, overview);
    let href = pr_href(&pr.key);
    let facts = overview.facts.get(&pr.key);
    let waiting = overview
        .waiting
        .get(&pr.key)
        .map(|left| format!("queued in {}", countdown(left.as_secs())));
    let lead = LeadPop {
        key: &pr.key,
        text: &lead.text,
        class: lead.class,
        facts,
        status: pr.latest_status(),
        // Only the latest run's error: an older failure a later run
        // replaced doesn't need attention.
        error: pr.latest_run.as_ref().and_then(|run| run.error.as_deref()),
        no_update: pr.latest_run.as_ref().is_some_and(|run| run.no_update),
        skip: overview.skipped.get(&pr.key),
        waiting: waiting.as_deref(),
        manual_reviews: pr.held(&overview.manual),
        now: app.clock.now(),
    };
    let row = Row {
        key: &pr.key,
        lead: &lead,
        head: &pr.head_sha,
        state: pr.state,
        pending: pr.pending_drafts,
        facts,
        submit_review: overview.submit_review(&pr.key),
        author: Some(&pr.author),
        is_draft: pr.is_draft,
    };
    let actions = html! {
        @if why.is_some() {
            a.linkbtn href={ (href) "/review-now" } data-dialog { "review now " (keycap("r")) }
        }
        a.linkbtn href={ (href) "/ignore" } { "ignore by title " (keycap("i")) }
        @if pr.chat_run.is_some() {
            a.linkbtn href={ (href) "#chat" } { "chat " (keycap("c")) }
        }
        (archive_form(app, &pr.key, pr.archived, Some(back)))
    };
    html! {
        // Selection follows the PR across refreshes, not the row's place.
        div.ib.archived[pr.archived] data-row data-key=(pr.key) data-href=(href)
            data-review-now=[why.as_ref().map(|_| format!("{href}/review-now"))]
            data-ignore={ (href) "/ignore" }
            data-chat=[pr.chat_run.map(|_| format!("{href}#chat"))] {
            (unseen(overview, &pr.key))
            (row.cells(app, &pr.title))
            span.a { (actions) }
        }
    }
}

fn my_row(app: &App, pr: &MyPr, overview: &Overview, back: &str) -> Markup {
    let href = pr_href(&pr.key);
    let lead = my_lead(pr, overview);
    let facts = overview.facts.get(&pr.key);
    let run = facts.and_then(|f| f.latest_run.as_ref());
    let lead = LeadPop {
        key: &pr.key,
        text: &lead.text,
        class: lead.class,
        facts,
        status: run.map(|run| run.status.as_str()),
        error: None,
        no_update: false,
        skip: None,
        waiting: None,
        manual_reviews: run.is_some_and(|run| overview.manual.holds(&run.profile)),
        now: app.clock.now(),
    };
    let row = Row {
        key: &pr.key,
        lead: &lead,
        head: facts.map_or("", |f| f.head_sha.as_str()),
        state: pr.state,
        pending: pr.pending_drafts,
        facts,
        submit_review: overview.submit_review(&pr.key),
        author: None,
        is_draft: pr.is_draft,
    };
    html! {
        div.ib.archived[pr.archived] data-row data-key=(pr.key) data-href=(href)
            data-chat=[pr.chat_run.map(|_| format!("{href}#chat"))] {
            (unseen(overview, &pr.key))
            (row.cells(app, &pr.title))
            span.a {
                @if pr.chat_run.is_some() {
                    a.linkbtn href={ (href) "#chat" } { "chat " (keycap("c")) }
                }
                (archive_form(app, &pr.key, pr.archived, Some(back)))
            }
        }
    }
}

/// A row's cells: the lead, the status (reviewed by, and what the PR asks
/// of you or waits on), and the PR (its title, and under it its ref,
/// author and state words). Each fact is in one of them.
struct Row<'a> {
    key: &'a PrKey,
    lead: &'a LeadPop<'a>,
    head: &'a str,
    state: PrState,
    pending: u32,
    facts: Option<&'a RowFacts>,
    /// [`Overview::submit_review`].
    submit_review: bool,
    /// Someone else's PR's author; `None` for yours.
    author: Option<&'a str>,
    is_draft: bool,
}

impl Row<'_> {
    fn cells(&self, app: &App, title: &str) -> Markup {
        let href = pr_href(self.key);
        let reviewed_by = ReviewedBy {
            key: self.key,
            me: &app.me,
            head: self.head,
            facts: self.facts,
            now: app.clock.now(),
        };
        html! {
            span.x { (self.lead.render()) }
            span.y {
                (reviewed_by.render())
                span.needs { @for signal in self.needs() { (signal) } }
            }
            span.t { a href=(href) title=(title) { (title) } }
            span.m {
                (pr_ref(self.key))
                @if let Some(author) = self.author { " · " (author) }
                @for word in self.state_words() { " · " (word) }
            }
        }
    }

    fn merge(&self) -> Option<Merge> {
        self.facts.map(|f| f.merge)
    }

    /// What the PR asks of you, or waits on, that the lead doesn't say.
    fn needs(&self) -> Vec<Markup> {
        let lead = self.lead.text;
        let decided = self.facts.and_then(|f| f.decided.as_ref());
        let mut out = Vec::new();
        if self.pending > 0 && lead != drafts(self.pending) {
            out.push(html! { span.cnt { (drafts(self.pending)) } });
        }
        let unanswered = self.state.unanswered;
        if unanswered > 0 && lead != pressing(self.state) {
            out.push(html! {
                span.sig.you title="comments by others you haven't answered" {
                    (unanswered) " to answer"
                }
            });
        }
        let to_post = decided.map_or(0, |d| d.accepted);
        if to_post > 0 && lead != format!("{to_post} to post") {
            out.push(html! { span.sig.post { (to_post) " to post" } });
        }
        if self.submit_review && lead != "submit review" {
            out.push(html! { span.sig.post { "submit review" } });
        }
        let tip = if self.author.is_some() {
            "your comments the author hasn't answered"
        } else {
            "your replies the reviewer hasn't answered"
        };
        for waits in self.facts.map_or(&[][..], |f| &f.awaiting[..]) {
            out.push(html! {
                span.sig.them title=(tip) { (waits.threads) " awaiting " b { "@" (waits.login) } }
            });
        }
        // On your own PR, failing CI and what holds up its approval are
        // yours to see to.
        if self.author.is_none()
            && let Some(merge) = self.merge()
        {
            let block = approved_block(self.state, Some(merge));
            let ci = (merge.ci == Checks::Failing && block != Some(Block::CiFailing))
                .then_some(Block::CiFailing);
            for block in ci.into_iter().chain(block) {
                if block.word() != lead {
                    out.push(html! { span.sig.(block_class(block)) { (block.word()) } });
                }
            }
        }
        out
    }

    /// Where the PR stands for the PR cell: its approval, and on someone
    /// else's PR, CI and what holds up the approval; then whether it's a
    /// draft.
    fn state_words(&self) -> Vec<Markup> {
        let lead = self.lead.text;
        let mut out = Vec::new();
        let approval = match self.state.approval {
            Approval::Mergeable => Some("mergeable"),
            Approval::Approved(_) => Some("approved"),
            Approval::ChangesRequested => Some("changes requested"),
            Approval::None => None,
        };
        if let Some(word) = approval.filter(|word| *word != lead) {
            out.push(html! { span.(approval_class(self.state)) { (word) } });
        }
        let merge = self.merge();
        let block = approved_block(self.state, merge);
        let ci = merge.map_or(Checks::Other, |m| m.ci);
        if self.author.is_some() {
            if let Some(block) = block {
                out.push(html! { span.sig.(block_class(block)) { (block.word()) } });
            }
            for (checks, word) in [
                (Checks::Failing, "ci failing"),
                (Checks::Pending, "ci pending"),
            ] {
                if ci == checks && block.map(Block::word) != Some(word) {
                    out.push(html! { span.sig.(block_class_of(word)) { (word) } });
                }
            }
        } else if ci == Checks::Pending && block.is_none() {
            out.push(html! { span.sig.cipend { "ci pending" } });
        }
        if self.is_draft && lead != "draft PR" {
            out.push(html! { span.tagpr { "draft PR" } });
        }
        out
    }
}

/// How a block's word is shown: what needs fixing stands out.
fn block_class(block: Block) -> &'static str {
    block_class_of(block.word())
}

fn block_class_of(word: &str) -> &'static str {
    match word {
        "ci failing" | "conflicts" => "cifail",
        _ => "cipend",
    }
}

/// The CSS class an approval's word takes, by the state's urgency without
/// its unanswered comments, which the status cell says.
fn approval_class(state: PrState) -> &'static str {
    let approval = PrState {
        unanswered: 0,
        ..state
    };
    match approval.urgency() {
        Urgency::Act => "u-act",
        Urgency::Good => "u-good",
        Urgency::Quiet => "u-quiet",
    }
}

#[derive(Debug, Deserialize)]
pub struct WindowForm {
    /// A [`WindowChoice`], or `default` for `poll.updated_within_days`.
    window: String,
    /// The index's URL, to go back to as it was.
    back: Option<String>,
}

/// Picks the recency window from the hidden-PRs line, then goes back to
/// the index. `serve` stores it and reconciles with it.
pub async fn set_window(
    State(app): State<Shared>,
    Form(form): Form<WindowForm>,
) -> Result<Redirect, Error> {
    let choice = match form.window.as_str() {
        "default" => None,
        text => Some(
            WindowChoice::parse(text)
                .ok_or_else(|| Error::Refused(format!("`{text}` isn't a recency window")))?,
        ),
    };
    app.control.set_window(choice)?;
    if let Some(choice) = choice {
        info!(window = %choice.as_str(), "recency window picked from the dashboard");
    } else {
        info!("recency window back to `poll.updated_within_days`");
    }
    Ok(Redirect::to(
        &IndexQuery::from_href(form.back.as_deref()).href(),
    ))
}

/// A button that archives the PR, or unarchives it, then goes back to the
/// index at `back`, or else to the PR's page.
pub fn archive_form(app: &App, key: &PrKey, archived: bool, back: Option<&str>) -> Markup {
    html! {
        form.archive method="post" action={ (pr_href(key)) "/archive" } {
            (csrf_field(app))
            input type="hidden" name="archived" value=(!archived);
            input type="hidden" name="next" value=(if back.is_some() { "index" } else { "pr" });
            @if let Some(back) = back { input type="hidden" name="back" value=(back); }
            @if back.is_some() {
                button.linkbtn type="submit" {
                    @if archived { "unarchive " } @else { "archive " } (keycap("x"))
                }
            } @else {
                button.btn type="submit" {
                    @if archived { "Unarchive" } @else { "Archive" } (keycap("x"))
                }
            }
        }
    }
}
