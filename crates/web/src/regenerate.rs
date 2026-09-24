//! "Agent…": revise a finished review with your instruction. It's asked
//! for on a confirm card, like Review now, since it spends tokens; `serve`
//! starts it through [`Control::regenerate`](crate::Control::regenerate).

use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use maud::{Markup, html};
use sanic_store::{PrPage, ReviewRun};
use serde::Deserialize;
use tracing::info;

use crate::{
    App, Error, Shared,
    page::{self, Card, Kind, Tone, csrf_field, keycap, pr_ref},
    pr::{crumb, short},
    pr_href,
    submit::RunPath,
};

/// The PR and its run at `path`, which must be one of its review runs,
/// numbered as the PR page's run list numbers it.
fn load(app: &App, path: &RunPath) -> Result<(PrPage, ReviewRun, usize), Error> {
    let key = path.pr().key()?;
    let store = app.store();
    let pr = store
        .pr_page(&key)
        .map_err(Error::pr(&key))?
        .ok_or_else(|| Error::NotFound(format!("{} isn't tracked", key.url())))?;
    let mut runs = store.review_runs(&key).map_err(Error::pr(&key))?;
    let i = runs
        .iter()
        .position(|run| run.id == path.run)
        .ok_or_else(|| Error::NotFound(format!("{} has no run {}", key.url(), path.run)))?;
    let number = runs.len() - i;
    Ok((pr, runs.swap_remove(i), number))
}

fn card(pr: &PrPage, (run, number): (&ReviewRun, usize), extra: Markup, go: Markup) -> Markup {
    let back = format!("{}?run={}", pr_href(&pr.key), run.id);
    page::card(&Card {
        kind: "Agent",
        heading: html! { "Revise this review?" },
        title: &pr.title,
        meta: html! {
            (pr_ref(&pr.key)) " · " (pr.author) " · run " (number) ", reviewed "
            code { (short(&run.head_sha)) }
        },
        extra,
        cost: Some(html! { "Spends tokens: the agent works again, with your instruction." }),
        go,
        back: (&back, "Cancel"),
        tone: Tone::Ask,
    })
}

pub async fn confirm(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
) -> Result<Markup, Error> {
    let (pr, run, number) = load(&app, &path)?;
    let action = format!("{}/runs/{}/regenerate", pr_href(&pr.key), run.id);
    let explain = html! {
        ul.explain {
            li {
                "It resumes run " (number) "'s agent session, not interactively, with your "
                "instruction."
            }
            li {
                "The result is a new run with a fresh set of drafts for the same reviewed "
                "commit, " code { (short(&run.head_sha)) } "."
            }
            li {
                "This run and its drafts, your edits included, stay as they are. The agent "
                "sees them, and keeps your accepted and edited drafts unless you ask "
                "otherwise; ones it keeps word for word stay accepted."
            }
            li {
                "It's refused if the PR has moved on since this review: start a fresh "
                "review instead."
            }
        }
        label.instruction for="instruction" { "Your instruction" }
        textarea #instruction form="confirm" name="instruction" rows="4" required autofocus
            placeholder="e.g. Drop the nits, and say more about the error handling." {}
    };
    let go = html! {
        form #confirm method="post" action=(action) {
            (csrf_field(&app))
            button.btn.go type="submit" { "Revise the review" (keycap("y")) }
        }
    };
    let content = card(&pr, (&run, number), explain, go);
    Ok(page::layout_in(
        &app,
        Kind::Confirm,
        "Revise the review",
        &[crumb(&pr.key), html! { "agent" }],
        &content,
    ))
}

#[derive(Debug, Deserialize)]
pub struct RegenerateForm {
    instruction: String,
}

pub async fn regenerate(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Form(form): Form<RegenerateForm>,
) -> Result<Response, Error> {
    let (pr, run, number) = load(&app, &path)?;
    let instruction = form.instruction.replace("\r\n", "\n");
    if instruction.trim().is_empty() {
        return Err(Error::Refused(
            "say what to change: the instruction is empty".into(),
        ));
    }
    let started = app
        .control
        .regenerate(run.id, None, &instruction)
        .map_err(Error::pr(&pr.key))?;
    match started {
        Ok(new) => {
            info!(url = %pr.key.url(), source_run = run.id, run = new, "regenerate requested from the dashboard");
            Ok(Redirect::to(&format!("{}?run={new}", pr_href(&pr.key))).into_response())
        }
        Err(why) => {
            let content = card(
                &pr,
                (&run, number),
                html! { p.note.error { "Not started: " (why) "." } },
                html! {},
            );
            let page = page::layout_in(
                &app,
                Kind::Other,
                "Not revised",
                &[crumb(&pr.key), html! { "agent" }],
                &content,
            );
            Ok((StatusCode::CONFLICT, page).into_response())
        }
    }
}
