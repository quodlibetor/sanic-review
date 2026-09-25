//! Settings kept in the config file. Saving one edits the file in place,
//! as the TUI does, and `serve` applies it through the same reload as a
//! hand edit.

use std::sync::Arc;

use axum::{Form, extract::State};
use color_eyre::eyre::WrapErr;
use maud::{Markup, html};
use serde::Deserialize;
use tracing::info;

use crate::{
    App, Error, Shared,
    cells::plural,
    page::{self, Card, Kind, Tone, csrf_field, keycap},
};

/// How many reviews are queued, which manual reviews hold while they're on.
fn held(app: &App) -> Result<u32, Error> {
    Ok(app.store().queued_review_count()?)
}

/// `n` held reviews, in words.
fn held_reviews(n: u32) -> String {
    plural(n, "held review", "held reviews")
}

/// A form that switches manual reviews `on` or off, after you've been told
/// how many reviews that starts.
fn switch(app: &App, on: bool, held: u32, id: Option<&str>, label: &Markup) -> Markup {
    html! {
        form id=[id] method="post" action="/settings/manual-reviews" {
            (csrf_field(app))
            input type="hidden" name="on" value=(on);
            input type="hidden" name="held" value=(held);
            button.btn.go[id.is_some()] type="submit" { (label) }
        }
    }
}

fn crumbs() -> [Markup; 1] {
    [html! { a href="/settings" { "settings" } }]
}

pub async fn page(State(app): State<Shared>) -> Result<Markup, Error> {
    let on = app.manual_reviews();
    let held = if on { held(&app)? } else { 0 };
    let content = html! {
        h1 { "Settings" }
        p.dim {
            "Saved to " code { (app.config_path.display()) } " in place, keeping its "
            "comments and layout. serve applies a change as it reloads the config, as it "
            "does a hand edit."
        }
        h2 #manual-reviews {
            "Manual reviews: "
            @if on { span.held { "on" } } @else { "off" }
        }
        p {
            code { "runner.manual_reviews" } ". While it's on, reviews are queued but held "
            "until you start one: Review now on its PR, " kbd { "r" } " in the TUI, or "
            code { "sanic-review review <PR url>" } ". It's on unless the config turns it "
            "off, so starting serve doesn't review everything waiting on you at once."
        }
        @if on {
            p {
                (held_reviews(held)) " now. Turning it off starts them, "
                code { "runner.max_concurrent" } " at a time."
            }
            @if held > 0 {
                a.btn href="/settings/manual-reviews" { "Turn off…" }
            } @else {
                (switch(&app, false, 0, None, &html! { "Turn off" }))
            }
        } @else {
            p {
                "Queued reviews run by themselves, " code { "runner.max_concurrent" }
                " at a time. Turning it on holds what's queued from then on; reviews "
                "already running carry on."
            }
            (switch(&app, true, 0, None, &html! { "Turn on" }))
        }
    };
    Ok(page::layout_in(
        &app,
        Kind::Other,
        "Settings",
        &crumbs(),
        &content,
    ))
}

/// Asks before turning manual reviews off, saying how many held reviews
/// that starts.
fn confirm(app: &App, held: u32) -> Markup {
    let heading = if held == 0 {
        html! { "Turn manual reviews off?" }
    } else {
        html! { (held_reviews(held)) " will start. Continue?" }
    };
    let config = app.config_path.display().to_string();
    let content = page::card(&Card {
        kind: "Settings",
        heading,
        title: "Turn manual reviews off",
        meta: html! { code { "runner.manual_reviews = false" } " in " code { (config) } },
        extra: html! {
            p.note {
                "Queued reviews then run by themselves, at most "
                code { "runner.max_concurrent" } " at a time. Turning it back on holds what's "
                "queued from then on."
            }
        },
        cost: (held > 0).then(|| html! { "Spends tokens: each held review runs." }),
        go: switch(
            app,
            false,
            held,
            Some("confirm"),
            &html! { @if held == 0 { "Turn off" } @else { "Start them" } (keycap("y")) },
        ),
        back: ("/settings", "Cancel"),
        tone: Tone::Ask,
    });
    let [settings] = crumbs();
    page::layout_in(
        app,
        Kind::Confirm,
        "Manual reviews",
        &[settings, html! { "manual reviews" }],
        &content,
    )
}

/// Asked for by the page's Turn off, which may have been drawn before a
/// switch elsewhere; turning them off again is harmless, so it asks
/// whether or not the last reload had them on.
pub async fn confirm_manual_reviews(State(app): State<Shared>) -> Result<Markup, Error> {
    let held = held(&app)?;
    Ok(confirm(&app, held))
}

#[derive(Debug, Deserialize)]
pub struct SwitchForm {
    on: bool,
    /// How many held reviews you were told turning them off starts.
    #[serde(default)]
    held: u32,
}

pub async fn set_manual_reviews(
    State(app): State<Shared>,
    Form(form): Form<SwitchForm>,
) -> Result<Markup, Error> {
    // Whatever the reload last said: the TUI may have just turned them on,
    // so reviews queued now may be held. More than you were told, say
    // after a reconcile, and it asks again with the count as it is.
    if !form.on {
        let held = held(&app)?;
        if held > form.held {
            return Ok(confirm(&app, held));
        }
    }
    // The control edits the config file.
    let changed = {
        let control = Arc::clone(&app.control);
        tokio::task::spawn_blocking(move || control.set_manual_reviews(form.on))
            .await
            .wrap_err("switching manual reviews")??
            .map_err(|why| {
                Error::Refused(format!(
                    "Manual reviews weren't switched: the config doesn't load, so it's left as \
                     it is. {why}"
                ))
            })?
    };
    if changed {
        info!(on = form.on, "manual reviews switched from the dashboard");
    }
    let content = page::card(&Card {
        kind: "Settings",
        heading: html! { "Manual reviews " @if form.on { "on" } @else { "off" } },
        title: "runner.manual_reviews",
        meta: html! { code { (app.config_path.display()) } },
        extra: html! {
            p {
                @if !changed {
                    "The config already said so."
                } @else if form.on {
                    "serve holds what's queued from the moment it reloads the config; "
                    "reviews already running carry on."
                } @else {
                    "serve starts the held reviews as it reloads the config, "
                    code { "runner.max_concurrent" } " at a time."
                }
            }
        },
        cost: None,
        go: html! { a.btn.go href="/" { "The index" } },
        back: ("/settings", "Back to settings"),
        tone: Tone::Done,
    });
    Ok(page::layout_in(
        &app,
        Kind::Other,
        "Manual reviews",
        &crumbs(),
        &content,
    ))
}
