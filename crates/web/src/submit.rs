//! Submitting a review: a preview of the exact payload, then, only when
//! you confirm, the one GitHub write.

use std::fmt::Write as _;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use maud::html;
use sanic_core::run::Side;
use sanic_github::{ApiError, NewComment, NewReview, ReviewEvent};
use sanic_store::{DraftRow, PrPage, ReviewRun};
use serde::Deserialize;
use tracing::{info, warn};

use crate::{
    App, Error, Shared,
    page::{self, Kind, csrf_field, github_link},
    pr, pr_href,
};

#[derive(Debug, Deserialize)]
pub struct RunPath {
    owner: String,
    name: String,
    number: u32,
    run: i64,
}

impl RunPath {
    fn pr(&self) -> crate::PrPath {
        crate::PrPath {
            owner: self.owner.clone(),
            name: self.name.clone(),
            number: self.number,
        }
    }
}

/// The review a run's accepted drafts make, and which drafts are in it.
#[derive(Debug)]
pub struct Built {
    pub review: NewReview,
    /// Everything that's marked posted once GitHub has the review.
    pub included: Vec<i64>,
    /// Accepted comments GitHub won't take inline, posted in the body.
    pub in_body: usize,
    /// Accepted replies, which can't be posted yet.
    pub replies: usize,
}

/// The review of `run` with the verdict you picked: the summary as its
/// body if you accepted it, and your accepted comments. Accepted comments
/// that aren't on a line of the diff are added to the body. `Err` says why
/// there's nothing to post.
pub fn build(run: &ReviewRun, drafts: &[DraftRow], event: ReviewEvent) -> Result<Built, String> {
    if run.status != "succeeded" {
        return Err(format!(
            "run {} is {}, so it has no drafts",
            run.id, run.status
        ));
    }
    let accepted = || drafts.iter().filter(|d| d.status == "accepted");
    let mut sections = Vec::new();
    let mut included = Vec::new();
    let mut comments = Vec::new();
    let mut in_body = 0;
    for draft in accepted().filter(|d| d.kind == "summary") {
        sections.push(draft.body().to_owned());
        included.push(draft.id);
    }
    for draft in accepted().filter(|d| d.kind == "comment") {
        included.push(draft.id);
        if let Some(comment) = inline(draft) {
            comments.push(comment);
        } else {
            let anchor = pr::anchor(draft);
            let heading = match blob_link(draft, &run.head_sha) {
                // Brackets in a path would end the link text early, and a
                // backslash before one would undo its escape.
                Some(url) => format!(
                    "[{}]({url})",
                    anchor
                        .replace('\\', "\\\\")
                        .replace('[', "\\[")
                        .replace(']', "\\]")
                ),
                None => anchor,
            };
            sections.push(format!("**{heading}**\n\n{}", draft.body()));
            in_body += 1;
        }
    }
    let body = sections.join("\n\n");
    if event != ReviewEvent::Approve && body.trim().is_empty() && comments.is_empty() {
        return Err("nothing is accepted, so there's nothing to post".into());
    }
    Ok(Built {
        review: NewReview {
            commit_id: run.head_sha.clone(),
            body,
            event,
            comments,
        },
        included,
        in_body,
        replies: accepted().filter(|d| d.kind == "reply").count(),
    })
}

/// A link to `draft`'s lines in the file at `head`, so a comment that
/// can't go inline still points at them. `None` without a path and line,
/// or for lines of the old file, which `head` doesn't have. `plain=1`, or
/// GitHub shows a Markdown file rendered, without its lines.
fn blob_link(draft: &DraftRow, head: &str) -> Option<String> {
    let (path, line) = (draft.path.as_deref()?, draft.line?);
    if draft.side.as_deref() == Some("LEFT") {
        return None;
    }
    let path: Vec<String> = path.split('/').map(percent_encode).collect();
    let lines = match draft.start_line {
        Some(start) if start != line => format!("L{start}-L{line}"),
        _ => format!("L{line}"),
    };
    Some(format!(
        "https://github.com/{}/blob/{head}/{}?plain=1#{lines}",
        draft.key.repo,
        path.join("/")
    ))
}

/// `segment` with everything but RFC 3986's unreserved characters
/// percent-encoded, byte by byte.
fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// `draft` as an inline comment, if GitHub will take it inline.
fn inline(draft: &DraftRow) -> Option<NewComment> {
    if draft.unanchored {
        return None;
    }
    let side = match draft.side.as_deref() {
        Some("LEFT") => Side::Left,
        Some("RIGHT") => Side::Right,
        _ => return None,
    };
    let line = draft.line?;
    let start_line = draft.start_line.filter(|&start| start != line);
    Some(NewComment {
        path: draft.path.clone()?,
        body: draft.body().to_owned(),
        line,
        side,
        start_line,
        start_side: start_line.map(|_| side),
    })
}

/// What in `texts` does more on GitHub than it looks like here: drafts
/// are the agent's, and PR text can steer the agent.
fn easy_to_miss<'a>(texts: impl Iterator<Item = &'a String>) -> Vec<&'static str> {
    let mut found = Vec::new();
    let texts: Vec<&str> = texts.map(String::as_str).collect();
    let any = |f: &dyn Fn(&str) -> bool| texts.iter().any(|t| f(t));
    if any(&|t| {
        t.match_indices('@').any(|(i, _)| {
            let before = t[..i].chars().next_back();
            let after = t[i + 1..].chars().next();
            !before.is_some_and(char::is_alphanumeric)
                && after.is_some_and(|c| c.is_ascii_alphanumeric())
        })
    }) {
        found.push("@-mentions, which notify people");
    }
    if any(&|t| t.contains("<!--")) {
        found.push("HTML comments, which GitHub hides");
    }
    if any(&|t| t.contains("![")) {
        found.push("images, which load from wherever they point");
    }
    if any(&|t| {
        t.match_indices('<')
            .any(|(i, _)| t[i + 1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '/'))
    }) {
        found.push("HTML tags");
    }
    if any(&|t| {
        t.chars().any(|c| {
            matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}' | '\u{feff}')
        })
    }) {
        found.push("invisible or text-direction characters");
    }
    found
}

/// Loads `run` of the PR at `path` and builds its review.
fn load(
    app: &App,
    path: &RunPath,
    event: ReviewEvent,
) -> Result<(PrPage, Result<Built, String>), Error> {
    let key = path.pr().key()?;
    let store = app.store();
    let pr = store
        .pr_page(&key)
        .map_err(Error::pr(&key))?
        .ok_or_else(|| Error::NotFound(format!("{} isn't tracked", key.url())))?;
    let run = store
        .review_runs(&key)
        .map_err(Error::pr(&key))?
        .into_iter()
        .find(|run| run.id == path.run)
        .ok_or_else(|| Error::NotFound(format!("{} has no run {}", key.url(), path.run)))?;
    let drafts = store.draft_rows(run.id).map_err(Error::pr(&key))?;
    Ok((pr, build(&run, &drafts, event)))
}

/// The payload as the confirm form carries it back. Compact, so it has no
/// line breaks for the browser to rewrite.
fn wire(review: &NewReview) -> Result<String, Error> {
    Ok(serde_json::to_string(review).map_err(color_eyre::Report::from)?)
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    event: ReviewEvent,
}

pub async fn preview(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Query(query): Query<PreviewQuery>,
) -> Result<Response, Error> {
    let (pr, built) = load(&app, &path, query.event)?;
    let back = format!("{}?run={}", pr_href(&pr.key), path.run);
    let built = match built {
        Ok(built) => built,
        Err(why) => {
            let content = html! {
                h1 { "Nothing to submit" }
                p { (why) }
                p { a #cancel href=(back) { "Back to the drafts" } }
            };
            let page = page::layout(&app, Kind::Other, "Nothing to submit", &content);
            return Ok((StatusCode::CONFLICT, page).into_response());
        }
    };
    let endpoint = format!(
        "POST /repos/{}/{}/pulls/{}/reviews",
        pr.key.repo.owner, pr.key.repo.name, pr.key.number
    );
    let pretty = serde_json::to_string_pretty(&built.review).map_err(color_eyre::Report::from)?;
    let action = format!("{}/runs/{}/submit", pr_href(&pr.key), path.run);
    let review = &built.review;
    let content = html! {
        h1 { "Submit review" }
        p.meta { (github_link(&pr.key)) " · " (pr.title) }
        p.verdict { "Verdict: " strong { (review.event.as_str()) } }
        @if review.event == ReviewEvent::Approve {
            p.warn { "You picked Approve. The agent never suggests it." }
        }
        @if built.in_body > 0 {
            p.warn {
                "Accepted comments that aren't on a line of the diff are in the body "
                "instead of inline, each headed by a link to its lines at the reviewed "
                "commit: " (built.in_body) "."
            }
        }
        @if built.replies > 0 {
            p.warn {
                "Accepted replies aren't included, since posting replies isn't supported "
                "yet: " (built.replies) "."
            }
        }
        @let texts = std::iter::once(&review.body).chain(review.comments.iter().map(|c| &c.body));
        @let missable = easy_to_miss(texts);
        @if !missable.is_empty() {
            p.warn {
                "GitHub renders this as Markdown, and it has things that are easy to miss "
                "in the raw text: " (missable.join("; ")) "."
            }
        }
        h2 { "Body" }
        @if review.body.is_empty() { p.dim { "(empty)" } } @else { pre.body { (review.body) } }
        h2 { "Inline comments (" (review.comments.len()) ")" }
        @for c in &review.comments {
            div.comment {
                code {
                    (c.path) ":"
                    @if let Some(start) = c.start_line { (start) "-" }
                    (c.line) " " (c.side.as_str())
                }
                pre { (c.body) }
            }
        }
        h2 { "Exactly what's sent" }
        p { code { (endpoint) } " at commit " code { (review.commit_id) } }
        pre #payload { (pretty) }
        form #confirm method="post" action=(action) {
            (csrf_field(&app))
            input type="hidden" name="event" value=(review.event.as_str());
            input type="hidden" name="payload" value=(wire(review)?);
            // Approving is never just a verdict carried in the URL: it
            // takes its own tick here, which a stray `y` can't give.
            @if review.event == ReviewEvent::Approve {
                p {
                    label {
                        input type="checkbox" name="approve" value="yes" required autocomplete="off";
                        " I approve this PR"
                    }
                }
            }
            button type="submit" { kbd { "y" } " Confirm and post to GitHub" }
            " "
            a #cancel href=(back) { kbd { "Esc" } " Back to the drafts" }
        }
    };
    Ok(page::layout(&app, Kind::Confirm, "Submit review", &content).into_response())
}

#[derive(Debug, Deserialize)]
pub struct SubmitForm {
    event: ReviewEvent,
    /// The payload the preview showed, as [`wire`] wrote it.
    payload: String,
    /// `yes` when you ticked the approval box, which an approval needs.
    approve: Option<String>,
}

/// Posts the review, if it's still exactly what the preview showed. One
/// attempt: a failure is shown, never retried.
pub async fn submit(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Form(form): Form<SubmitForm>,
) -> Result<Response, Error> {
    if form.event == ReviewEvent::Approve && form.approve.as_deref() != Some("yes") {
        return Err(Error::Refused(
            "an approval needs the \"I approve this PR\" box ticked; nothing was sent".into(),
        ));
    }
    let mut posted_before = app.posting.lock().await;
    let (pr, built) = load(&app, &path, form.event)?;
    let key = &pr.key;
    let back = format!("{}?run={}", pr_href(key), path.run);
    let sent = format!("{}:{}", path.run, form.payload);
    let built = match built {
        Ok(built) if wire(&built.review)? == form.payload && !posted_before.contains(&sent) => {
            built
        }
        _ => {
            let preview = format!(
                "{}/runs/{}/preview?event={}",
                pr_href(key),
                path.run,
                form.event.as_str()
            );
            let content = html! {
                h1 { "Not posted" }
                p {
                    "The drafts changed since the preview, or were already posted, so "
                    "nothing was sent."
                }
                p { a href=(preview) { "Preview again" } " · " a #cancel href=(back) { "Back" } }
            };
            let page = page::layout(&app, Kind::Other, "Not posted", &content);
            return Ok((StatusCode::CONFLICT, page).into_response());
        }
    };
    let posted = app.github.post_review(key, &built.review).await;
    let content = match posted {
        Ok(posted) => {
            info!(url = %key.url(), review = %posted.html_url, "review posted");
            posted_before.insert(sent);
            let marked = app.store().mark_posted(&built.included);
            if let Err(err) = &marked {
                warn!(url = %key.url(), "marking drafts posted failed: {err:?}");
            }
            html! {
                h1 { "Posted" }
                p {
                    // It's GitHub's to say, but only a web link is a link.
                    @if posted.html_url.starts_with("https://") {
                        a.gh href=(posted.html_url) { (posted.html_url) }
                    } @else {
                        code { (posted.html_url) }
                    }
                }
                @if let Err(err) = marked {
                    p.warn {
                        "GitHub has the review, but marking its drafts posted failed: " (err)
                        ". Don't submit them again."
                    }
                }
                p { a #cancel href=(back) { "Back to the PR" } }
            }
        }
        Err(err) => {
            warn!(url = %key.url(), "posting the review failed: {err}");
            // Plain text: the report's `Debug` carries terminal colours and
            // a backtrace hint.
            let why = match &err {
                ApiError::Other(report) => format!("{report:#}"),
                other => other.to_string(),
            };
            let content = html! {
                h1 { "Posting the review failed" }
                pre.error { (why) }
                p {
                    "Nothing was marked posted, and it won't be retried. If the failure "
                    "came after GitHub took the request, the review may be there anyway: "
                    "check " (github_link(key)) " before you submit again."
                }
                p { a #cancel href=(back) { "Back to the drafts" } }
            };
            let page = page::layout(&app, Kind::Other, "Not posted", &content);
            return Ok((StatusCode::BAD_GATEWAY, page).into_response());
        }
    };
    Ok(page::layout(&app, Kind::Other, "Posted", &content).into_response())
}

#[cfg(test)]
mod tests {
    use sanic_core::{pr::PrKey, repo::RepoName};

    use super::*;

    fn draft(path: Option<&str>, start: Option<u32>, line: Option<u32>, side: &str) -> DraftRow {
        DraftRow {
            id: 1,
            run_id: 1,
            key: PrKey {
                repo: RepoName::new("Org", "Repo"),
                number: 7,
            },
            kind: "comment".into(),
            path: path.map(Into::into),
            line,
            start_line: start,
            side: Some(side.into()),
            severity: None,
            confidence: None,
            original_body: String::new(),
            edited_body: None,
            based_on: None,
            status: "accepted".into(),
            unanchored: true,
        }
    }

    #[test]
    fn body_links_point_at_the_reviewed_commit() {
        let link = |d: &DraftRow| blob_link(d, "abc123");
        assert_eq!(
            link(&draft(Some("src/lib.rs"), None, Some(4), "RIGHT")).as_deref(),
            Some("https://github.com/org/repo/blob/abc123/src/lib.rs?plain=1#L4")
        );
        assert_eq!(
            link(&draft(Some("src/lib.rs"), Some(2), Some(4), "RIGHT")).as_deref(),
            Some("https://github.com/org/repo/blob/abc123/src/lib.rs?plain=1#L2-L4")
        );
        assert_eq!(
            link(&draft(Some("docs/a b#c?é.md"), Some(4), Some(4), "RIGHT")).as_deref(),
            Some("https://github.com/org/repo/blob/abc123/docs/a%20b%23c%3F%C3%A9.md?plain=1#L4")
        );
        // Lines of the old file aren't in the reviewed commit; no line, no link.
        assert_eq!(
            link(&draft(Some("src/lib.rs"), None, Some(4), "LEFT")),
            None
        );
        assert_eq!(link(&draft(Some("src/lib.rs"), None, None, "RIGHT")), None);
        assert_eq!(link(&draft(None, None, Some(4), "RIGHT")), None);
    }
}
