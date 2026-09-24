//! The ignore editor, as the TUI's `i`: turn a PR's title into a
//! `skip_titles` glob, previewing which reviews you owe it would skip, then
//! pick where it goes.

use std::sync::Arc;

use axum::{
    Form,
    extract::{Path, Query, State},
};
use maud::{Markup, html};
use sanic_core::skip::{TitleFilter, escape_title};
use sanic_store::OwedReview;
use serde::Deserialize;
use tracing::info;

use crate::{
    App, Error, PrPath, Shared,
    index::owed_reviews,
    markdown,
    page::{self, Kind, csrf_field, github_link},
    pr_href,
};

/// The reviews `pattern` would skip in `profile`, or in every profile for
/// `None`, or why it isn't a valid glob.
fn matches<'a>(
    pattern: &str,
    profile: Option<&str>,
    owed: &'a [OwedReview],
) -> Result<Vec<&'a OwedReview>, String> {
    let filter = TitleFilter::single(pattern)?;
    Ok(owed
        .iter()
        .filter(|pr| profile.is_none_or(|p| pr.profile == p))
        .filter(|pr| filter.first_match(&pr.title).is_some())
        .collect())
}

/// The editor's live list of what the pattern would skip where it's to go.
fn preview_list(pattern: &str, profile: Option<&str>, owed: &[OwedReview]) -> Markup {
    html! {
        div #matches {
            @match matches(pattern, profile, owed) {
                Ok(hits) => {
                    h2 { "Would skip (" (hits.len()) ")" }
                    ul {
                        @for pr in hits {
                            li { (github_link(&pr.key)) " " (pr.title) }
                        }
                    }
                }
                Err(why) => {
                    h2 { "Would skip" }
                    p.error #pattern-error { "Not a valid pattern: " (why) }
                }
            }
        }
    }
}

/// The reviews you owe, and the one at `path`.
fn load(app: &App, path: &PrPath) -> Result<(Vec<OwedReview>, OwedReview), Error> {
    let key = path.key()?;
    let owed = owed_reviews(app).map_err(Error::pr(&key))?;
    let pr = owed
        .iter()
        .find(|pr| pr.key == key)
        .cloned()
        .ok_or_else(|| {
            Error::NotFound(format!(
                "{} isn't a review you owe; only those are skipped by title",
                key.url()
            ))
        })?;
    Ok((owed, pr))
}

pub async fn editor(State(app): State<Shared>, Path(path): Path<PrPath>) -> Result<Markup, Error> {
    let (owed, pr) = load(&app, &path)?;
    let pattern = escape_title(&pr.title);
    let href = pr_href(&pr.key);
    let preview = format!("{href}/ignore/preview");
    let profiles = app.skips.borrow().profile_names().to_vec();
    let content = html! {
        h1 { "Ignore by title" }
        p.meta { (github_link(&pr.key)) " by " (pr.author) }
        h2 { "Title" }
        pre.title { (pr.title) }
        details.description open {
            summary { "Description" }
            @if pr.body.trim().is_empty() { p.dim { "No description." } }
            @else { (markdown::render(&pr.body, &markdown::Context::default())) }
        }
        // Sent again when you come back to fix a refused pattern: adding
        // one twice leaves it there once.
        form #confirm method="post" action={ (href) "/ignore" } data-resend {
            (csrf_field(&app))
            p {
                label for="pattern" { "Pattern " }
                span.dim {
                    "A glob over whole titles, ignoring case; only glob syntax is "
                    "special. Edit it down, e.g. to " code { "build(deps)*" } "."
                }
            }
            input #pattern type="text" name="pattern" value=(pattern) size="60"
                autocomplete="off" spellcheck="false"
                hx-get=(preview) hx-trigger="input changed delay:150ms"
                hx-include="[name='profile']:checked" hx-sync="#confirm:replace"
                hx-target="#matches" hx-swap="outerHTML";
            (preview_list(&pattern, None, &owed))
            fieldset {
                legend { "Add it to skip_titles in" }
                label {
                    input type="radio" name="profile" value="" checked
                        hx-get=(preview) hx-trigger="change" hx-include="#pattern"
                        hx-sync="#confirm:replace" hx-target="#matches" hx-swap="outerHTML";
                    " " code { "[review_requests]" } ": every profile"
                }
                @for profile in &profiles {
                    br;
                    label {
                        input type="radio" name="profile" value=(profile)
                            hx-get=(preview) hx-trigger="change" hx-include="#pattern"
                            hx-sync="#confirm:replace" hx-target="#matches" hx-swap="outerHTML";
                        " " code { "[profile." (profile) "]" }
                    }
                }
            }
            button type="submit" { kbd { "y" } " Save to the config" }
            " "
            a #cancel href=(href) { kbd { "Esc" } " Cancel" }
        }
    };
    Ok(page::layout(
        &app,
        Kind::Confirm,
        "Ignore by title",
        &content,
    ))
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    #[serde(default)]
    pattern: String,
    /// A profile's name, or empty or absent for every profile.
    #[serde(default)]
    profile: String,
}

pub async fn preview(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
    Query(query): Query<PreviewQuery>,
) -> Result<Markup, Error> {
    let (owed, _) = load(&app, &path)?;
    let profile = Some(query.profile.as_str()).filter(|p| !p.is_empty());
    Ok(preview_list(&query.pattern, profile, &owed))
}

#[derive(Debug, Deserialize)]
pub struct SaveForm {
    pattern: String,
    /// A profile's name, or empty for `[review_requests]`.
    profile: String,
}

pub async fn save(
    State(app): State<Shared>,
    Path(path): Path<PrPath>,
    Form(form): Form<SaveForm>,
) -> Result<Markup, Error> {
    let (_, pr) = load(&app, &path)?;
    if let Err(why) = TitleFilter::single(&form.pattern) {
        return Err(Error::Refused(format!("not a valid pattern: {why}")));
    }
    let profile = if form.profile.is_empty() {
        None
    } else if app.skips.borrow().profile_names().contains(&form.profile) {
        Some(form.profile.as_str())
    } else {
        return Err(Error::Refused(format!(
            "there's no `[profile.{}]` in the config",
            form.profile
        )));
    };
    let place = profile.map_or_else(
        || "[review_requests]".to_owned(),
        |p| format!("[profile.{p}]"),
    );
    // The control edits the config file.
    let added = {
        let control = Arc::clone(&app.control);
        let (pattern, profile) = (form.pattern.clone(), profile.map(str::to_owned));
        tokio::task::spawn_blocking(move || control.add_skip_title(&pattern, profile.as_deref()))
            .await
            .map_err(|err| Error::pr(&pr.key)(err.into()))?
            .map_err(Error::pr(&pr.key))?
    };
    if added {
        info!(url = %pr.key.url(), pattern = %form.pattern, "skip_titles pattern added from the dashboard");
    }
    let href = pr_href(&pr.key);
    let content = html! {
        h1 { @if added { "Added" } @else { "Already there" } }
        p {
            code { (form.pattern) }
            @if added { " is now in " } @else { " was already in " }
            code { (place) } " skip_titles. serve picks the change up as it reloads the config."
        }
        p { a #cancel href=(href) { "Back to the PR" } " · " a href="/" { "the index" } }
    };
    Ok(page::layout(&app, Kind::Other, "Ignore by title", &content))
}
