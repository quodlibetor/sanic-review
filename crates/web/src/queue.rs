//! The run queue: what's running, what's next, and reordering or
//! cancelling what hasn't started.

use axum::{
    Form,
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use maud::{Markup, html};
use sanic_core::run::RunKind;
use sanic_store::{Cancelled, Direction, Moved, QueueEntry, Stale};
use serde::Deserialize;
use tracing::info;

use crate::{
    App, Error, Shared, cells,
    page::{self, Kind, csrf_field, first_line, keycap, pr_ref},
    pr::short,
    pr_href,
};

/// The queue, and the keys that act on it.
pub async fn page(State(app): State<Shared>) -> Result<Markup, Error> {
    let content = list(&app)?;
    Ok(page::layout_in(
        &app,
        Kind::Queue,
        "Queue",
        &[html! { "queue" }],
        &content,
    ))
}

/// The list on its own, so a move can swap just this.
fn list(app: &App) -> Result<Markup, Error> {
    let entries = app.store().run_queue().map_err(Error::internal)?;
    // Only queued reviews have a place: a regeneration never joins the
    // queue, so it gets no move buttons.
    let queued = entries.iter().filter(|e| e.is_movable()).count();
    let now = app.clock.now();
    let manual = app.manual_reviews();
    let any_held = entries
        .iter()
        .any(|e| !e.is_running() && manual.holds(&e.profile, &e.author));
    // Running runs have no place in the queue; the rest are numbered from 1.
    let mut place = 0;
    let placed: Vec<(usize, &QueueEntry)> = entries
        .iter()
        .map(|entry| {
            if entry.is_movable() {
                place += 1;
            }
            (place, entry)
        })
        .collect();
    Ok(html! {
        div #queue hx-get="/queue" hx-trigger="every 5s" hx-select="#queue"
            hx-select-oob="#counts" hx-swap="outerHTML" {
            h1 { "Queue" }
            @if queued > 0 {
                p.qtools {
                    a.linkbtn data-dialog href="/queue/clear-stale" {
                        "clear stale " (keycap("S"))
                    }
                }
            }
            @if any_held {
                p.held { "Manual reviews are holding some of these until you start them." }
            }
            @if entries.is_empty() {
                p.queue-empty { "Nothing is queued or running." }
            } @else {
                ol.queue {
                    @for (place, entry) in &placed {
                        (row(app, entry, *place, queued, now, manual.holds(&entry.profile, &entry.author)))
                    }
                }
            }
            p.help-foot {
                (keycap("j")) (keycap("k")) " move · "
                (keycap("K")) (keycap("J")) " move a queued run up, down · "
                (keycap("c")) " cancel · " (keycap("S")) " clear stale · "
                (keycap("Enter")) " open the PR · "
                (keycap("?")) " keys"
            }
        }
    })
}

/// One run. A running one is marked `▶` and can't be moved: its place is
/// behind it.
fn row(
    app: &App,
    entry: &QueueEntry,
    place: usize,
    queued: usize,
    now: std::time::SystemTime,
    held: bool,
) -> Markup {
    let running = entry.is_running();
    let movable = entry.is_movable();
    let cancel = format!("/queue/{}/cancel", entry.run_id);
    html! {
        li.qrow.running[running] id={ "run-" (entry.run_id) }
           data-row data-key=(entry.run_id) data-href=(pr_href(&entry.key))
           data-cancel=(cancel) data-movable[movable] {
            span.qpos { @if running { "▶" } @else { (place) } }
            span.qwhat {
                a.t href=(pr_href(&entry.key)) { (entry.title) }
                div.m {
                    (pr_ref(&entry.key)) " · " (entry.author) " · "
                    @match entry.kind {
                        RunKind::Review => { (trigger_word(&entry.trigger)) }
                        RunKind::Regenerate => {
                            "regeneration"
                            @if let Some(source) = entry.source_run {
                                " of run " (source)
                            }
                        }
                    }
                    " · " code { (short(&entry.head_sha)) } " · " (entry.profile)
                    @if entry.archived { " · archived" }
                }
                @if let Some(note) = &entry.instruction {
                    div.dim { (first_line(note)) }
                }
                @if let Some(err) = &entry.error {
                    div.error { "last attempt: " (first_line(err)) }
                }
            }
            span.qwhen {
                @if running {
                    "running"
                    @if let Some(at) = &entry.started_at { " · started " (cells::since(now, at)) }
                } @else {
                    @if held { "held · " }
                    "queued " (cells::since(now, &entry.queued_at))
                }
            }
            span.a {
                (move_form(app, entry.run_id, Direction::Up, movable && place > 1))
                (move_form(app, entry.run_id, Direction::Down, movable && place < queued))
                a.linkbtn data-dialog href=(cancel) { "cancel " (keycap("c")) }
            }
        }
    }
}

fn trigger_word(trigger: &str) -> &'static str {
    match trigger {
        "push" => "new commits",
        _ => "review requested",
    }
}

/// A button moving a queued run one place. Disabled at the ends, so the
/// row keeps its shape.
fn move_form(app: &App, run: i64, dir: Direction, enabled: bool) -> Markup {
    let (name, arrow, key) = match dir {
        Direction::Up => ("up", "↑", "K"),
        Direction::Down => ("down", "↓", "J"),
    };
    let action = format!("/queue/{run}/move");
    html! {
        form.qmove method="post" action=(action) data-dir=(name)
            hx-post=(action) hx-target="#queue" hx-swap="outerHTML" {
            (csrf_field(app))
            input type="hidden" name="dir" value=(name);
            button.linkbtn id={ "move-" (name) "-" (run) } type="submit" disabled[!enabled]
                title={ "move " (name) " the queue" } { (arrow) " " (keycap(key)) }
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Up,
    Down,
}

#[derive(Debug, Deserialize)]
pub struct MoveForm {
    dir: Dir,
}

/// Moves a queued run one place, straight in the store: the order it
/// writes is the order the worker takes them in.
pub async fn move_run(
    State(app): State<Shared>,
    Path(run): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<MoveForm>,
) -> Result<Response, Error> {
    let dir = match form.dir {
        Dir::Up => Direction::Up,
        Dir::Down => Direction::Down,
    };
    let moved = app.store().move_run(run, dir).map_err(Error::internal)?;
    match moved {
        Moved::NoSuchRun => return Err(Error::NotFound(format!("there's no run {run}"))),
        Moved::NotQueued { status } => {
            return Err(Error::Refused(format!(
                "run {run} is {status}; only a queued run can be moved"
            )));
        }
        // A press at the end of the list redraws rather than erroring.
        Moved::AlreadyThere { .. } => {}
        Moved::Moved { key, from, to } => {
            info!(url = %key.url(), run, from, to, "a queued run was moved");
        }
    }
    if headers.contains_key("hx-request") {
        return Ok(list(&app)?.into_response());
    }
    Ok(Redirect::to(&format!("/queue#run-{run}")).into_response())
}

/// Asks before cancelling: a running review's work is spent, and its
/// drafts go with it.
pub async fn confirm_cancel(
    State(app): State<Shared>,
    Path(run): Path<i64>,
) -> Result<Markup, Error> {
    let entries = app.store().run_queue().map_err(Error::internal)?;
    let entry = entries
        .iter()
        .find(|e| e.run_id == run)
        .ok_or_else(|| Error::NotFound(format!("run {run} isn't queued or running")))?;
    let running = entry.is_running();
    let card = page::Card {
        kind: "Cancel run",
        heading: if running {
            html! { "Stop this review and throw away its work?" }
        } else {
            html! { "Take this review out of the queue?" }
        },
        title: &entry.title,
        meta: html! {
            (pr_ref(&entry.key)) " · " (entry.author) " · " code { (short(&entry.head_sha)) }
        },
        extra: if running {
            html! {
                p.extra {
                    "Its agent is killed and its worktree removed. The drafts it \
                     has written are lost, and the tokens it has spent are spent."
                }
            }
        } else {
            html! { p.extra { "It never started, so nothing has been spent." } }
        },
        cost: Some(html! {
            "Nothing is posted to GitHub. "
            (keycap("r")) " reviews it again later."
        }),
        go: html! {
            form #confirm method="post" action={ "/queue/" (run) "/cancel" } data-in-place {
                (csrf_field(&app))
                button.btn.go type="submit" {
                    @if running { "Stop the review " } @else { "Take it out of the queue " }
                    (keycap("y"))
                }
            }
        },
        back: ("/queue", "Keep it"),
        tone: page::Tone::Ask,
    };
    Ok(page::layout(
        &app,
        Kind::Confirm,
        "Cancel run",
        &page::card(&card),
    ))
}

fn stale_reviews(n: u32) -> String {
    match n {
        1 => "1 queued review".into(),
        n => format!("{n} queued reviews"),
    }
}

/// Asks before dropping the queued reviews of PRs that no longer need one,
/// saying how many.
fn confirm_stale(app: &App, stale: u32) -> Markup {
    let card = page::Card {
        kind: "Clear stale",
        heading: html! { "Drop " (stale_reviews(stale)) "?" },
        title: "Queued reviews of PRs that no longer need one",
        meta: html! {
            "closed or merged · reviewed on their current head · yours · "
            "review request withdrawn"
        },
        extra: html! {
            p.extra {
                "They never started, so nothing has been spent. Running reviews "
                "finish as they are."
            }
        },
        cost: Some(html! {
            "Nothing is posted to GitHub. A new push or request reviews them again."
        }),
        go: html! {
            form #confirm method="post" action="/queue/clear-stale" data-in-place {
                (csrf_field(app))
                input type="hidden" name="stale" value=(stale);
                button.btn.go type="submit" { "Drop them " (keycap("y")) }
            }
        },
        back: ("/queue", "Keep them"),
        tone: page::Tone::Ask,
    };
    page::layout(app, Kind::Confirm, "Clear stale", &page::card(&card))
}

/// What dropping stale reviews did, or that there was nothing to drop.
fn stale_done(app: &App, dropped: &[(sanic_core::pr::PrKey, Stale)]) -> Markup {
    let card = page::Card {
        kind: "Clear stale",
        heading: if dropped.is_empty() {
            html! { "Nothing in the queue is stale" }
        } else {
            html! {
                "Dropped the queued reviews of " (dropped.len())
                @if dropped.len() == 1 { " PR" } @else { " PRs" }
            }
        },
        title: "Queued reviews of PRs that no longer need one",
        meta: html! {},
        extra: html! {
            @if !dropped.is_empty() {
                ul.extra {
                    @for (key, why) in dropped {
                        li { a href=(pr_href(key)) { (pr_ref(key)) } " · " (why) }
                    }
                }
            }
        },
        cost: None,
        go: html! { a.btn.go href="/queue" { "The queue" } },
        back: ("/", "The index"),
        tone: page::Tone::Done,
    };
    page::layout(app, Kind::Other, "Clear stale", &page::card(&card))
}

pub async fn confirm_clear_stale(State(app): State<Shared>) -> Result<Markup, Error> {
    let stale = app
        .store()
        .stale_review_count(&app.me)
        .map_err(Error::internal)?;
    if stale == 0 {
        return Ok(stale_done(&app, &[]));
    }
    Ok(confirm_stale(&app, stale))
}

#[derive(Debug, Deserialize)]
pub struct StaleForm {
    /// How many stale reviews you were told this drops.
    stale: u32,
}

/// Supersedes the stale queued reviews straight in the store, as archiving
/// does: none has started, so `serve` has nothing to stop.
pub async fn clear_stale(
    State(app): State<Shared>,
    Form(form): Form<StaleForm>,
) -> Result<Markup, Error> {
    let mut store = app.store();
    // More than you were told, say after a refresh, and it asks again.
    let stale = store.stale_review_count(&app.me).map_err(Error::internal)?;
    if stale > form.stale {
        drop(store);
        return Ok(confirm_stale(&app, stale));
    }
    let dropped = store
        .supersede_stale_reviews(&app.me, None)
        .map_err(Error::internal)?;
    drop(store);
    for (key, why) in &dropped {
        info!(url = %key.url(), "queued review superseded from the dashboard: {why}");
    }
    Ok(stale_done(&app, &dropped))
}

/// Cancels the run through `serve`, which stops its agent if it's running.
pub async fn cancel(State(app): State<Shared>, Path(run): Path<i64>) -> Result<Redirect, Error> {
    match app.control.cancel_run(run).map_err(Error::internal)? {
        Cancelled::NoSuchRun => Err(Error::NotFound(format!("there's no run {run}"))),
        Cancelled::Finished { status } => Err(Error::Refused(format!(
            "run {run} is {status} already; there's nothing to cancel"
        ))),
        Cancelled::Queued { key } | Cancelled::Running { key } => {
            info!(url = %key.url(), run, "cancelled from the dashboard");
            Ok(Redirect::to("/queue"))
        }
    }
}
