//! The status strip over a PR page's drafts: which head the shown review
//! is of, what's become of the PR's head since, and the PR's existing
//! review threads, on one line. While a review of the PR's head is on its
//! way, the head part rereads itself until it's done, and the drafts shown
//! are dimmed: they post against the older commit.

use std::time::{Duration, SystemTime};

use axum::extract::{Path, State};
use maud::{Markup, html};
use sanic_core::{
    clock::{ago, parse_rfc3339},
    hold::ClaudeHold,
    manual::ManualReviews,
    pr::PrKey,
};
use sanic_store::ReviewRun;

use crate::{
    App, Error, Shared,
    index::{Overview, owed_now},
    page::countdown,
    pr::short,
    pr_href,
    submit::RunPath,
};

/// What's become of the PR's head since the shown review's.
#[derive(Debug, Clone, Copy)]
pub enum Next<'a> {
    /// A review of it is waiting out the quiet period, queued in this long.
    Waiting(Duration),
    Queued(&'a ReviewRun),
    /// Queued, and held until its head is checked for `.claude` changes,
    /// after which it starts by itself if the check finds none and manual
    /// reviews don't hold it.
    Checking,
    /// Queued, and held until you start it.
    Held(HeldBy),
    Running(&'a ReviewRun),
    /// Reviewed, with drafts to look at.
    Ready(&'a ReviewRun),
    /// Its review failed or crashed.
    Failed(&'a ReviewRun),
    /// Nothing has reviewed it, or is on its way to.
    Nothing,
}

/// What holds a queued review until you start it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldBy {
    /// Manual reviews, by the profile it was queued under.
    Manual,
    /// The PR's `.claude` hold, for what its changes reach.
    Claude,
    /// The PR's `.claude` hold, since checking its head failed.
    CheckFailed,
}

impl<'a> Next<'a> {
    /// From the PR's `runs`, newest first: the newest of `head`'s, unless a
    /// review `waiting` out the quiet period is newer news than a finished
    /// one. A queued one is held by the PR's `.claude` hold (`claude`), or
    /// else if `manual` holds the profile it was queued under and the PR's
    /// `author`; one that only waits for its head's check is on its way
    /// unless `manual` holds it.
    pub fn of(
        runs: &'a [ReviewRun],
        head: &str,
        waiting: Option<Duration>,
        manual: &ManualReviews,
        author: &str,
        claude: Option<&ClaudeHold>,
    ) -> Self {
        let run = runs.iter().find(|run| run.head_sha == head);
        match run.map(|run| (run, run.status.as_str())) {
            Some((run, "running")) => Self::Running(run),
            Some((_, "queued")) if let Some(hold) = claude.filter(|h| h.warns()) => {
                Self::Held(if hold.findings.is_empty() {
                    HeldBy::CheckFailed
                } else {
                    HeldBy::Claude
                })
            }
            Some((run, "queued")) if manual.holds(&run.profile, author) => {
                Self::Held(HeldBy::Manual)
            }
            Some((_, "queued")) if claude.is_some() => Self::Checking,
            Some((run, "queued")) => Self::Queued(run),
            _ if let Some(left) = waiting => Self::Waiting(left),
            Some((run, "succeeded")) => Self::Ready(run),
            Some((run, "failed" | "crashed")) => Self::Failed(run),
            _ => Self::Nothing,
        }
    }

    /// Where a review of the head is, after "A review of X is", while
    /// it's on its way by itself, as the page polls for and the preview
    /// asks about; `None` otherwise.
    pub fn coming(self) -> Option<&'static str> {
        match self {
            Self::Waiting(_) => Some("waiting for the PR to go quiet"),
            Self::Queued(_) => Some("queued"),
            Self::Running(_) => Some("running"),
            Self::Checking => Some("waiting until its head is checked for .claude changes"),
            Self::Held(_) | Self::Ready(_) | Self::Failed(_) | Self::Nothing => None,
        }
    }
}

/// The strip's head part: the shown run's head, and what's become of the
/// PR's since.
#[derive(Debug, Clone, Copy)]
pub struct Drift<'a> {
    pub key: &'a PrKey,
    pub shown: &'a ReviewRun,
    pub pr_head: &'a str,
    pub next: Next<'a>,
    now: SystemTime,
    /// A review of the PR may be started by hand; see [`owed_now`].
    startable: bool,
}

impl<'a> Drift<'a> {
    /// For `shown`, one of the PR's `runs`, once the PR's head is
    /// `pr_head`; `None` while that's still the head it reviewed.
    pub fn new(
        app: &App,
        overview: &Overview,
        key: &'a PrKey,
        runs: &'a [ReviewRun],
        shown: &'a ReviewRun,
        pr_head: &'a str,
    ) -> Option<Self> {
        if shown.head_sha == pr_head {
            return None;
        }
        let owed = overview.owed_review(key);
        let waiting = overview.waiting.get(key).copied();
        // From the store, not the overview: a PR that isn't a review you owe
        // has one too.
        let claude = app.store().claude_hold(key).unwrap_or_else(|err| {
            tracing::warn!(url = %key.url(), "reading the PR's `.claude` hold failed: {err:?}");
            None
        });
        // Unknown, it's held, as the worker would hold it.
        let author = match app.store().pr_summary(key) {
            Ok(pr) => pr.map(|pr| pr.author).unwrap_or_default(),
            Err(err) => {
                tracing::warn!(url = %key.url(), "reading the PR's author failed: {err:?}");
                String::new()
            }
        };
        Some(Self {
            key,
            shown,
            pr_head,
            next: Next::of(
                runs,
                pr_head,
                waiting,
                &overview.manual,
                &author,
                claude.as_ref(),
            ),
            now: app.clock.now(),
            startable: owed.is_some_and(|o| owed_now(o, overview).starts()),
        })
    }

    /// How long ago `at` was, as the strip says it.
    fn since(&self, at: Option<&str>) -> String {
        match at.and_then(parse_rfc3339).map(|then| ago(self.now, then)) {
            Some(ago) if ago == "now" => " <1m".into(),
            Some(ago) => format!(" {ago}"),
            None => String::new(),
        }
    }

    /// The head part on its own, which polls for itself while a review is
    /// [`Next::coming`] and stops once one answer says it isn't.
    pub fn segment(&self) -> Markup {
        let coming = self.next.coming().is_some();
        let href = pr_href(self.key);
        let poll = format!("{href}/runs/{}/strip", self.shown.id);
        let view = |run: &ReviewRun| format!("{href}?run={}", run.id);
        let new = html! { code { (short(self.pr_head)) } };
        let again = html! {
            @if self.startable {
                " · " a.linkbtn data-dialog href={ (href) "/review-now" } { "Review again" }
            }
        };
        html! {
            span.head #run-state .coming[coming]
                hx-get=[coming.then_some(&poll)] hx-trigger=[coming.then_some("every 5s")]
                hx-swap=[coming.then_some("outerHTML")] {
                span title="The review shown: its comments post against this commit" {
                    "Showing " code { (short(&self.shown.head_sha)) }
                }
                " · "
                @match self.next {
                    Next::Waiting(left) => {
                        span.dim {
                            "⟳ " (new) " waiting for the PR to go quiet ("
                            (countdown(left.as_secs())) ")"
                        }
                    }
                    Next::Queued(run) => {
                        span.held { "⟳ " (new) " queued" (self.since(Some(&run.queued_at))) }
                    }
                    Next::Running(run) => {
                        span.running { "⟳ " (new) " running" (self.since(run.started_at.as_deref())) }
                    }
                    Next::Checking => {
                        span.held {
                            "⟳ " (new) " waiting until its head is checked for " code { ".claude" }
                            " changes"
                        }
                    }
                    Next::Held(by) => {
                        span.held {
                            (new)
                            @match by {
                                HeldBy::Manual => " held by manual reviews",
                                HeldBy::Claude => { " held for its " code { ".claude" } " changes" }
                                HeldBy::CheckFailed => {
                                    " held: checking it for " code { ".claude" } " changes failed"
                                }
                            }
                        }
                        @if self.startable {
                            " · " a.linkbtn data-dialog href={ (href) "/review-now" } { "Start it" }
                        }
                    }
                    Next::Ready(run) => {
                        span.ok { "✓ " (new) " ready" } " → " a href=(view(run)) { "view it" }
                    }
                    Next::Failed(run) => {
                        span.bad { "✗ " (new) " " (run.status) } " → "
                        a href=(view(run)) { "see why" }
                        (again)
                    }
                    Next::Nothing => {
                        span.dim { (new) " not reviewed" }
                        (again)
                    }
                }
            }
        }
    }
}

/// The strip: the `drift` part while the PR has moved past the shown
/// review, and the `threads` summary when it has review threads; nothing
/// without either.
pub fn strip(drift: Option<&Drift<'_>>, threads: Option<Markup>) -> Markup {
    html! {
        @if drift.is_some() || threads.is_some() {
            div.strip #strip {
                @if let Some(drift) = drift { (drift.segment()) }
                @if let Some(threads) = threads { (threads) }
            }
        }
    }
}

/// The head part again, for the page's poll while a review is on its way.
/// Empty once the shown run is of the PR's head, or the PR or the run is
/// gone: an error htmx wouldn't swap in, so the poll would never stop.
pub async fn poll(State(app): State<Shared>, Path(path): Path<RunPath>) -> Result<Markup, Error> {
    let key = path.pr().key()?;
    let overview = Overview::load_pr(&app, &key).map_err(Error::pr(&key))?;
    let (pr, runs) = {
        let store = app.store();
        let pr = store.pr_page(&key).map_err(Error::pr(&key))?;
        (pr, store.review_runs(&key).map_err(Error::pr(&key))?)
    };
    let shown = runs.iter().find(|run| run.id == path.run);
    Ok(pr
        .as_ref()
        .zip(shown)
        .and_then(|(pr, shown)| Drift::new(&app, &overview, &key, &runs, shown, &pr.head_sha))
        .map(|drift| drift.segment())
        .unwrap_or_default())
}
