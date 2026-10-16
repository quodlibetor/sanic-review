//! Review now (`r`): one card for any tracked PR, worded as the TUI's
//! confirm is, that starts a review of one you owe, revises the review it
//! has with your instruction, or says why there's nothing to start.

use axum::{
    Form,
    extract::{Path, State},
    response::Redirect,
};
use maud::{Markup, html};
use sanic_core::{
    hold::ClaudeHold,
    pr::is_login,
    skip::Skip,
    start::{ReviewNow, Why},
};
use sanic_store::{OwedReview, PrPage, ReviewRun};
use serde::Deserialize;
use tracing::info;

use crate::{
    App, Error, PrPath, Shared,
    filter::IndexQuery,
    hold,
    index::{Overview, review_now as offered},
    page::{self, Card, Kind, Tone, countdown, csrf_field, first_line, keycap, pr_ref},
    pr::{crumb, short},
    pr_href, regenerate,
};

/// What a review started by hand costs.
const COST: &str = "Spends tokens: a full review of the head as last polled.";

/// What it costs once the PR has moved past its latest review, which it
/// resumes as a push's review does.
const RESUME: &str = "Spends tokens: a review of the head as last polled, resuming the last \
                      review's session if it can, else a full review.";

/// The PR at `path`, if it's tracked, and what `r` offers for it.
fn load(app: &App, path: &PrPath) -> Result<(PrPage, ReviewNow, Overview), Error> {
    let key = path.key()?;
    // This PR alone, whatever the recency window: pressing `r` on it is
    // asking.
    let overview = Overview::load_pr(app, &key).map_err(Error::pr(&key))?;
    let pr = app
        .store()
        .pr_page(&key)
        .map_err(Error::pr(&key))?
        .ok_or_else(|| Error::NotFound(format!("{} isn't tracked", key.url())))?;
    let now = offered(&key, &overview, is_login(&pr.author, &app.me));
    Ok((pr, now, overview))
}

pub async fn confirm(State(app): State<Shared>, Path(path): Path<PrPath>) -> Result<Markup, Error> {
    let (pr, now, overview) = load(&app, &path)?;
    let key = &pr.key;
    // Newest first.
    let runs: Vec<ReviewRun> = app.store().review_runs(key).map_err(Error::pr(key))?;
    let owed = overview.owed_review(key);
    let hold = owed.and_then(|pr| pr.claude_hold.as_ref());
    let href = pr_href(key);
    let meta = html! {
        (pr_ref(key)) " · " (pr.author) " · head " code { (short(&pr.head_sha)) }
        // Held for whatever reason it's offered, a skipped PR's included.
        @if now.starts() && owed.is_some_and(|o| o.held(&overview.manual)) { (held_by(hold)) }
    };
    let (heading, extra, cost, go) = match &now {
        ReviewNow::Start(why) => start(&app, &pr, why, owed),
        ReviewNow::Coming(coming) => {
            let run = runs
                .first()
                .filter(|run| matches!(run.status.as_str(), "queued" | "running"));
            let sha = run.map_or(&pr.head_sha, |run| &run.head_sha);
            let left = overview.waiting.get(key);
            (
                html! { "A review of " code { (short(sha)) } " is already " (coming.as_str()) },
                html! {
                    p.note {
                        "There's nothing to start."
                        @if let Some(run) = run {
                            " " a href={ (href) "?run=" (run.id) } { "See its run" } "."
                        } @else if let Some(left) = left {
                            " It's queued in " (countdown(left.as_secs())) "."
                        }
                    }
                },
                None,
                html! {},
            )
        }
        ReviewNow::Revise => revise(&app, &pr, &runs, owed)?,
        ReviewNow::Yours => (
            html! { "Nothing to start" },
            note(
                "This is your own PR: nothing here reviews it, and drafting replies to \
                 its feedback isn't built yet.",
            ),
            None,
            html! {},
        ),
        ReviewNow::Nothing => (
            html! { "Nothing to start" },
            note(if owed.is_some() {
                "Its latest run was replaced, and no review of it is on its way."
            } else {
                "It isn't a review you owe, so there's no review of it to start."
            }),
            None,
            html! {},
        ),
    };
    // A revision without a session to resume asks nothing either.
    let asks = !go.0.is_empty();
    let content = page::card(&Card {
        kind: "Review now",
        heading,
        title: &pr.title,
        meta,
        // Whatever it starts, the agent would load the PR's own skills and
        // agents.
        extra: html! { @if let Some(hold) = hold { (hold::warning(key, hold)) } (extra) },
        cost,
        go,
        back: (&href, if asks { "Cancel" } else { "Back" }),
        tone: Tone::Ask,
    });
    Ok(page::layout_in(
        &app,
        Kind::Confirm,
        "Review now",
        &[crumb(key), html! { "review now" }],
        &content,
    ))
}

/// What holds a review, on its card: a `.claude` hold, a check of its
/// head for one, or else manual reviews.
fn held_by(claude: Option<&ClaudeHold>) -> Markup {
    html! {
        @if claude.is_some_and(|h| !h.findings.is_empty()) {
            " · held for its " code { ".claude" } " changes"
        } @else if claude.is_some_and(ClaudeHold::warns) {
            " · held: checking it for " code { ".claude" } " changes failed"
        } @else if claude.is_some() {
            " · waiting until its head is checked for " code { ".claude" }
            " changes, which it must be before it can start"
        } @else {
            " · held by " a href="/settings" { "manual reviews" }
        }
    }
}

/// A card's parts: heading, what more it says, its cost, and its go.
type Parts = (Markup, Markup, Option<Markup>, Markup);

fn note(text: &str) -> Markup {
    html! { p.note { (text) } }
}

/// Starting a review of `pr`, which you owe (`owed`), for `why`.
fn start(app: &App, pr: &PrPage, why: &Why, owed: Option<&OwedReview>) -> Parts {
    let latest = owed.and_then(|o| o.latest_run.as_ref());
    // As `serve` queues it: see its `review_now`.
    let cost = if owed.is_some_and(OwedReview::behind) {
        RESUME
    } else {
        COST
    };
    let go = |label: &str| {
        html! {
            form #confirm method="post" action={ (pr_href(&pr.key)) "/review-now" } data-in-place {
                (csrf_field(app))
                // A dialog over the index sets it to `index`.
                input type="hidden" name="next" value="pr";
                button.btn.go type="submit" { (label) (keycap("y")) }
            }
        }
    };
    let skipped = html! { (cost) " It stays skipped for automatic reviews." };
    match why {
        Why::Skipped(Skip::Reviewed { by, .. }) => (
            html! { "Already reviewed by " (by) ". Review anyway?" },
            html! {},
            Some(skipped),
            go("Review anyway"),
        ),
        Why::Skipped(skip) => (
            html! { "Review this " (skip.label()) "-skipped PR anyway?" },
            html! {},
            Some(skipped),
            go("Review anyway"),
        ),
        Why::Failed => (
            html! { "Rerun this review?" },
            html! {
                @if let Some(run) = latest {
                    @if let Some(error) = &run.error {
                        div.error { (run.status) ": " (error) }
                    }
                }
            },
            Some(html! { (cost) }),
            go("Rerun the review"),
        ),
        Why::Cancelled => (
            html! { "Review this again? You cancelled its last run." },
            html! {},
            Some(html! { (cost) }),
            go("Review it again"),
        ),
        Why::Held => (
            html! { "Start the held review now?" },
            html! {},
            Some(html! { (cost) }),
            go("Start it"),
        ),
        Why::MovedOn => (
            html! { "Review the new head?" },
            html! {
                @if let Some(run) = latest {
                    p.note {
                        "Its latest review is of " code { (short(&run.head_sha)) }
                        ", and nothing is reviewing " code { (short(&pr.head_sha)) } "."
                    }
                }
            },
            Some(html! { (cost) }),
            go("Review the new head"),
        ),
        Why::Unreviewed => (
            html! { "Review this PR now?" },
            note("Nothing has reviewed it yet, and no review of it is on its way."),
            Some(html! { (cost) }),
            go("Review it"),
        ),
    }
}

/// Reviewing `pr` again at its head, which its newest succeeded review
/// of `runs` (newest first) is of: only worth it for what you ask, so
/// it's a revision of that review with your instruction, which the Agent
/// card's post starts. When the latest run is a revision of it that
/// failed or was cancelled (`owed` says), it says so, and a whole
/// review's revision starts the box from its instruction; a draft's is
/// about that draft alone.
fn revise(
    app: &App,
    pr: &PrPage,
    runs: &[ReviewRun],
    owed: Option<&OwedReview>,
) -> Result<Parts, Error> {
    let reviewed = runs
        .iter()
        .find(|run| run.status == "succeeded" && run.head_sha == pr.head_sha);
    let sha = reviewed.map_or(&pr.head_sha, |run| &run.head_sha);
    let heading = html! {
        "Review " (pr.key) " again? It hasn't changed since the review at " code { (short(sha)) } "."
    };
    let session = match reviewed {
        Some(run) => app
            .store()
            .session_run(run.id)
            .map_err(Error::pr(&pr.key))?
            .map(|_| run),
        None => None,
    };
    let Some(run) = session else {
        return Ok((
            heading,
            note("That review has no agent session to resume, so there's nothing to start."),
            None,
            html! {},
        ));
    };
    // The latest run, which the query `owed` comes from orders as `runs`.
    let failed = runs
        .first()
        .filter(|_| owed.is_some_and(OwedReview::revision_failed));
    let instruction = failed
        .filter(|last| last.draft_id.is_none())
        .and_then(|last| last.instruction.as_deref());
    let extra = html! {
        @if let Some(last) = failed {
            p.note {
                "Its last revision"
                @if last.draft_id.is_some() { ", of one draft," }
                " "
                @if last.status == "cancelled" { "was cancelled" } @else { (last.status) }
                @if let Some(error) = &last.error { ": " span.error { (first_line(error)) } }
                "."
                @if instruction.is_some() { " Its instruction is below, to start from." }
            }
        }
        p.note {
            "Nothing has changed since, so another run is only worth it for what you "
            "want looked at differently: say what, and the review's agent works again "
            "with your instruction. The result is a new run with fresh drafts for "
            code { (short(sha)) } "; this one and its drafts stay as they are."
        }
        label.instruction for="instruction" { "Your instruction (required)" }
        textarea #instruction form="confirm" name="instruction" rows="4" required autofocus
            placeholder="e.g. Look harder at the error handling, and drop the nits." {
            @if let Some(instruction) = instruction { (instruction) }
        }
    };
    Ok((
        heading,
        extra,
        Some(html! { (regenerate::COST) }),
        regenerate::go(app, &pr.key, run.id),
    ))
}

#[derive(Debug, Deserialize)]
pub struct ReviewNowForm {
    /// `index` to go back to the index afterwards, as a dialog opened
    /// there asks; else the PR's page.
    #[serde(default)]
    next: String,
    /// The index's URL, to go back to as it was.
    back: Option<String>,
}

/// Starts the review the card offered, decided again: the card may be
/// stale, or sent twice. Only [`ReviewNow::Start`] starts one here; a
/// revision goes through the Agent card's post.
pub async fn start_now(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
    Form(form): Form<ReviewNowForm>,
) -> Result<Redirect, Error> {
    let (pr, now, _) = load(&app, &path)?;
    let url = pr.key.url();
    let refused = match now {
        ReviewNow::Start(_) => None,
        ReviewNow::Coming(coming) => {
            Some(format!("a review of {url} is already {}", coming.as_str()))
        }
        ReviewNow::Revise => Some(format!(
            "{url} hasn't changed since its review: revise that review with an instruction \
             instead"
        )),
        ReviewNow::Yours => Some(format!(
            "{url} is your own PR: drafting replies to its feedback isn't built yet"
        )),
        ReviewNow::Nothing => Some(format!("there's nothing to start for {url}")),
    };
    if let Some(why) = refused {
        return Err(Error::Refused(why));
    }
    info!(url = %url, "review requested from the dashboard");
    let href = pr_href(&pr.key);
    app.control.review_now(pr.key);
    Ok(Redirect::to(&if form.next == "index" {
        IndexQuery::from_href(form.back.as_deref()).href()
    } else {
        href
    }))
}
