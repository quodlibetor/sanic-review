//! Submitting a review: a preview of the exact payload, then, only when
//! you confirm, the one GitHub write.

use std::fmt::Write as _;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use sanic_core::{pr::PrKey, run::Side};
use sanic_github::{ApiError, NewComment, NewReview, ReviewEvent};
use sanic_store::{DraftRow, PrPage, ReviewRun};
use serde::{Deserialize, de::IntoDeserializer as _};
use tracing::{info, warn};

use crate::{
    App, Error, Shared,
    page::{self, Card, Kind, Tone, csrf_field, keycap, pr_ref},
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
    /// Drafts still pending, which the review leaves out.
    pub pending: Vec<i64>,
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
        pending: drafts
            .iter()
            .filter(|d| d.status == "pending")
            .map(|d| d.id)
            .collect(),
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
    event: Verdict,
}

/// A verdict as the PR page's verdict form sends it. Approve's radio adds
/// [`App::approve_pick`] after the verdict, so the pick comes only with
/// Approve: a preview URL for another verdict, edited to say Approve,
/// doesn't have it.
#[derive(Debug, Deserialize)]
#[serde(try_from = "String")]
struct Verdict {
    event: ReviewEvent,
    picked: String,
}

impl TryFrom<String> for Verdict {
    type Error = serde::de::value::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let (event, picked) = value.split_once(':').unwrap_or((&value, ""));
        Ok(Self {
            event: ReviewEvent::deserialize(event.into_deserializer())?,
            picked: picked.to_owned(),
        })
    }
}

/// The value of the verdict form's Approve radio; see [`Verdict`].
pub fn approve_value(app: &App) -> String {
    verdict_value(ReviewEvent::Approve, app.approve_pick.token())
}

fn verdict_value(event: ReviewEvent, picked: &str) -> String {
    if event == ReviewEvent::Approve {
        format!("{}:{picked}", event.as_str())
    } else {
        event.as_str().to_owned()
    }
}

pub async fn preview(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Query(query): Query<PreviewQuery>,
) -> Result<Response, Error> {
    let Verdict { event, picked } = query.event;
    let (pr, built) = load(&app, &path, event)?;
    let back = format!("{}?run={}", pr_href(&pr.key), path.run);
    // Approve is never taken from a URL alone: it's picked on the PR page.
    let built = if event == ReviewEvent::Approve && !app.approve_pick.matches(&picked) {
        Err("Approve is picked on the PR page, with the verdict: go back and pick it there.".into())
    } else {
        built
    };
    let built = match built {
        Ok(built) => built,
        Err(why) => {
            let content = result_card(
                &pr,
                "Nothing to submit",
                html! { p.note { (why) } },
                html! {},
                (&back, "Back to the drafts"),
                Tone::Ask,
            );
            let page = result_page(&app, &pr.key, "Nothing to submit", &content);
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
    let approve = review.event == ReviewEvent::Approve;
    let (chip, verdict_class) = match review.event {
        ReviewEvent::Comment => ("COMMENT", "cm"),
        ReviewEvent::RequestChanges => ("REQUEST CHANGES", "rc"),
        ReviewEvent::Approve => ("APPROVE", "ap"),
    };
    let comments = review.comments.len();
    let content = html! {
        h1 { "Post this review?" }
        div.sumline {
            span.verdict-chip.(verdict_class) { (chip) }
            span { (pr.title) " " (pr_ref(&pr.key)) }
            span {
                @if review.body.is_empty() { "no body" } @else { "body" }
                " + " (comments) @if comments == 1 { " inline comment" } @else { " inline comments" }
            }
            span { "at " code { (pr::short(&review.commit_id)) } }
        }
        div.cols {
            div {
                (checklist(&built, &pr, &back))
                (readable_review(review))
            }
            div.wirecol {
                h2 { "Exactly what's sent · " code { (endpoint) } }
                pre #payload { (pretty) }
            }
        }
        form.foot #confirm method="post" action=(action) {
            (csrf_field(&app))
            input type="hidden" name="event" value=(review.event.as_str());
            input type="hidden" name="payload" value=(wire(review)?);
            @if approve {
                input type="hidden" name="picked" value=(picked);
            }
            button.btn.go type="submit" {
                @if approve { "Approve this PR with the above comments" }
                @else { "Confirm and post to GitHub" }
                (keycap("y"))
            }
            a.btn #cancel href=(back) { "Back to the drafts" (keycap("Esc")) }
            span.dim { "Sent once, only if the drafts still match this preview." }
        }
    };
    Ok(page::layout_in(
        &app,
        Kind::Confirm,
        "Submit review",
        &[pr::crumb(&pr.key), html! { "submit" }],
        &content,
    )
    .into_response())
}

/// What to check before posting: an approval, a stale head, comments moved
/// into the body, Markdown that hides things, and drafts left out.
fn checklist(built: &Built, pr: &PrPage, back: &str) -> Markup {
    let review = &built.review;
    let texts = std::iter::once(&review.body).chain(review.comments.iter().map(|c| &c.body));
    let missable = easy_to_miss(texts);
    let mut checks = Vec::new();
    if review.event == ReviewEvent::Approve {
        checks.push(html! {
            b { "You picked Approve." }
            " The agent never suggests approving."
        });
    }
    if review.commit_id != pr.head_sha {
        checks.push(html! {
            "The review is of " code { (pr::short(&review.commit_id)) }
            "; the PR is now at " code { (pr::short(&pr.head_sha)) }
            ". Comments land on the older commit."
        });
    }
    if built.in_body > 0 {
        checks.push(html! {
            b { (built.in_body) }
            " accepted comment(s) aren't on a line of the diff, so "
            "they're in the body, each headed by a link to its lines."
        });
    }
    if !missable.is_empty() {
        checks.push(html! {
            "GitHub renders this as Markdown, which makes some of it "
            "easy to miss: " (missable.join("; ")) "."
        });
    }
    if !built.pending.is_empty() {
        checks.push(html! {
            b { (built.pending.len()) }
            " draft(s) still pending aren't included: "
            @for (i, id) in built.pending.iter().enumerate() {
                @if i > 0 { ", " }
                a href={ (back) "#draft-" (id) } { "draft " (id) }
            }
            "."
        });
    }
    if built.replies > 0 {
        checks.push(html! {
            b { (built.replies) }
            " accepted repl(ies) aren't included: posting replies "
            "isn't supported yet."
        });
    }
    html! {
        @if !checks.is_empty() {
            div.check {
                h2 { "Check before posting" }
                ul { @for check in checks { li { (check) } } }
            }
        }
    }
}

/// The review as GitHub will show it: its body, then each inline comment.
fn readable_review(review: &NewReview) -> Markup {
    let comments = review.comments.len();
    html! {
    h2.sec { "Review body" }
    div.rv {
        @if review.body.is_empty() { pre.dim { "(empty)" } } @else { pre.body { (review.body) } }
    }
    h2.sec { "Inline comments " span.dim { (comments) } }
    @for c in &review.comments {
        div.rv {
            div.h {
                span.mono {
                    (c.path) ":"
                    @if let Some(start) = c.start_line { (start) "-" }
                    (c.line)
                }
                span.sp { (c.side.as_str()) }
            }
            pre { (c.body) }
        }
    }
    }
}

#[derive(Debug, Deserialize)]
pub struct SubmitForm {
    event: ReviewEvent,
    /// The payload the preview showed, as [`wire`] wrote it.
    payload: String,
    /// As the preview had it; see [`Verdict`].
    #[serde(default)]
    picked: String,
}

/// Posts the review, if it's still exactly what the preview showed. One
/// attempt: a failure is shown, never retried.
pub async fn submit(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Form(form): Form<SubmitForm>,
) -> Result<Response, Error> {
    if form.event == ReviewEvent::Approve && !app.approve_pick.matches(&form.picked) {
        return Err(Error::Refused(
            "Approve is picked on the PR page; nothing was sent".into(),
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
            // The pick is already checked, so it can go along: without it,
            // an approval's preview would be refused.
            let preview = format!(
                "{}/runs/{}/preview?event={}",
                pr_href(key),
                path.run,
                verdict_value(form.event, &form.picked)
            );
            let content = result_card(
                &pr,
                "Not posted",
                html! {
                    p.note {
                        "The drafts changed since the preview, or were already posted, so "
                        "nothing was sent."
                    }
                },
                html! { a.btn.primary href=(preview) { "Preview again" } },
                (&back, "Back to the drafts"),
                Tone::Ask,
            );
            let page = result_page(&app, key, "Not posted", &content);
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
            posted_card(&pr, &built, &posted.html_url, marked.err(), &back)
        }
        Err(err) => {
            warn!(url = %key.url(), "posting the review failed: {err}");
            // Plain text: the report's `Debug` carries terminal colours and
            // a backtrace hint.
            let why = match &err {
                ApiError::Other(report) => format!("{report:#}"),
                other => other.to_string(),
            };
            let content = result_card(
                &pr,
                "Posting failed",
                html! {
                    pre.error { (why) }
                    p.note {
                        "Nothing was marked posted, and it won't be retried. If the failure "
                        "came after GitHub took the request, the review may be there "
                        "anyway: " a href=(key.url()) { "check the PR" }
                        " before you submit again."
                    }
                },
                html! {},
                (&back, "Back to the drafts"),
                Tone::Failed,
            );
            let page = result_page(&app, key, "Not posted", &content);
            return Ok((StatusCode::BAD_GATEWAY, page).into_response());
        }
    };
    Ok(result_page(&app, key, "Posted", &content).into_response())
}

/// What a review GitHub took looks like: the card, with a link to it
/// there, and a warning if its drafts couldn't be marked posted.
fn posted_card(
    pr: &PrPage,
    built: &Built,
    html_url: &str,
    marked: Option<color_eyre::Report>,
    back: &str,
) -> Markup {
    let key = &pr.key;
    let comments = built.review.comments.len();
    let meta = html! {
        (pr_ref(key)) " · " (verdict(built.review.event)) " · "
        (comments) @if comments == 1 { " inline comment" } @else { " inline comments" }
    };
    let extra = html! {
        @if let Some(err) = marked {
            p.warn {
                "GitHub has the review, but marking its drafts posted failed: " (err)
                ". Don't submit them again."
            }
        }
    };
    let go = html! {
        // It's GitHub's to say, but only a web link is a link.
        @if html_url.starts_with("https://") {
            a.btn.primary href=(html_url) { "Open it on GitHub ↗" }
        } @else {
            code { (html_url) }
        }
    };
    page::card(&Card {
        kind: "Submit review",
        heading: html! { "Posted" },
        title: &pr.title,
        meta,
        extra,
        cost: None,
        go,
        back: (back, "Back to the PR"),
        tone: Tone::Done,
    })
}

/// A submit's outcome, in the card confirm pages use.
fn result_card(
    pr: &PrPage,
    heading: &str,
    extra: Markup,
    go: Markup,
    back: (&str, &str),
    tone: Tone,
) -> Markup {
    page::card(&Card {
        kind: "Submit review",
        heading: html! { (heading) },
        title: &pr.title,
        meta: pr_ref(&pr.key),
        extra,
        cost: None,
        go,
        back,
        tone,
    })
}

fn result_page(app: &App, key: &PrKey, title: &str, content: &Markup) -> Markup {
    page::layout_in(
        app,
        Kind::Other,
        title,
        &[pr::crumb(key), html! { "submit" }],
        content,
    )
}

/// A verdict as people say it.
fn verdict(event: ReviewEvent) -> &'static str {
    match event {
        ReviewEvent::Comment => "comment",
        ReviewEvent::RequestChanges => "request changes",
        ReviewEvent::Approve => "approve",
    }
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
