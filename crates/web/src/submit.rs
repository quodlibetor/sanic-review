//! Submitting a review: a preview of the exact payload, then, only when
//! you confirm, the one GitHub write.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime},
};

use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use maud::{Markup, html};
use sanic_core::{
    clock::rfc3339,
    pr::{PrKey, Thread},
    run::Side,
};
use sanic_github::{
    ApiError, NewComment, NewReaction, NewReply, NewReview, PostError, ReviewEvent, ReviewStatus,
    Step, find_review_step, review_state_step,
};
use sanic_store::{DraftRow, OnGithub, PendingReview, PrPage, ReviewRun, SentReview, ThreadChoice};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    App, Error, Shared,
    guard::Csrf,
    links,
    page::{self, Card, Kind, Tone, csrf_field, keycap, pr_ref},
    pr, pr_href, threads,
};

#[derive(Debug, Deserialize)]
pub struct RunPath {
    owner: String,
    name: String,
    number: u32,
    pub run: i64,
}

impl RunPath {
    pub fn pr(&self) -> crate::PrPath {
        crate::PrPath {
            owner: self.owner.clone(),
            name: self.name.clone(),
            number: self.number,
        }
    }
}

/// What a run's accepted drafts post, and which drafts are in it: a
/// review with its replies in existing threads, then thumbs-ups.
#[derive(Debug)]
pub struct Built {
    /// `None` when all there is to post is thumbs-ups.
    pub review: Option<NewReview>,
    /// Posted in the review, in existing threads.
    pub replies: Vec<Reply>,
    /// Posted after the review, one at a time.
    pub reactions: Vec<Reaction>,
    /// Everything that's marked posted once GitHub has the review.
    pub included: Vec<i64>,
    /// The included drafts' bodies, as they go out, by draft.
    pub bodies: Vec<(i64, String)>,
    /// Accepted comments GitHub won't take inline, posted in the body.
    pub in_body: usize,
    /// Accepted drafts of kind `reply`, which can't be posted yet: they
    /// don't record their thread.
    pub unthreaded: usize,
    /// Drafts still pending, which the review leaves out.
    pub pending: Vec<i64>,
    /// The reviewed head, which the drafts' lines are lines of.
    pub head: String,
    /// A review an earlier submit on the PR left pending on GitHub, or
    /// sent without an answer, which is settled first; see [`settle`].
    pub left: Option<PendingReview>,
}

/// A draft posted as a reply in an existing thread.
#[derive(Debug)]
pub struct Reply {
    pub new: NewReply,
    pub draft: i64,
    pub thread: Thread,
}

/// A thumbs-up on an existing comment, in place of the drafts that chose
/// it.
#[derive(Debug)]
pub struct Reaction {
    pub new: NewReaction,
    pub drafts: Vec<i64>,
    pub thread: Thread,
    pub comment: sanic_core::pr::Comment,
}

/// What the confirm form carries back, to check nothing changed since the
/// preview: everything [`Built`] sends.
#[derive(Serialize)]
struct Wire<'a> {
    left: Option<&'a str>,
    review: Option<&'a NewReview>,
    replies: Vec<&'a NewReply>,
    reactions: Vec<&'a NewReaction>,
}

impl Built {
    fn wire(&self) -> Wire<'_> {
        Wire {
            left: self.left.as_ref().map(|l| match &l.on_github {
                OnGithub::Pending { node_id, .. } => node_id.as_str(),
                OnGithub::Sent(sent) => sent.after.as_str(),
            }),
            review: self.review.as_ref(),
            replies: self.replies.iter().map(|r| &r.new).collect(),
            reactions: self.reactions.iter().map(|r| &r.new).collect(),
        }
    }

    fn new_replies(&self) -> Vec<NewReply> {
        self.replies.iter().map(|r| r.new.clone()).collect()
    }

    /// Every request a confirm sends, in order, as `me`.
    fn steps(&self, key: &PrKey, me: &str) -> Vec<Step> {
        let left = self.left.iter().flat_map(|left| match &left.on_github {
            OnGithub::Pending { node_id, .. } => {
                let mut discard = review_state_step(node_id);
                discard.endpoint = "GraphQL deletePullRequestReview, only if it's still \
                                    pending, with"
                    .into();
                vec![review_state_step(node_id), discard]
            }
            OnGithub::Sent(_) => vec![find_review_step(key, me)],
        });
        let review = self
            .review
            .iter()
            .flat_map(|review| review.steps(key, &self.new_replies()));
        left.chain(review)
            .chain(self.reactions.iter().flat_map(|r| r.new.steps()))
            .collect()
    }

    fn event(&self) -> Option<ReviewEvent> {
        self.review.as_ref().map(|r| r.event)
    }
}

/// The review of `run` with the verdict you picked: the summary as its
/// body if you accepted it, and your accepted comments. Accepted comments
/// that aren't on a line of the diff are added to the body. A comment you
/// chose to post in an existing thread of `threads` goes in the review as
/// a reply there, or as a thumbs-up on the comment you picked instead of
/// its text. `Err` says why there's nothing to post.
pub fn build(
    run: &ReviewRun,
    drafts: &[DraftRow],
    threads: &[Thread],
    event: ReviewEvent,
) -> Result<Built, String> {
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
    let mut replies = Vec::new();
    let mut reactions: Vec<Reaction> = Vec::new();
    let mut in_body = 0;
    for draft in accepted().filter(|d| d.kind == "summary") {
        sections.push(draft.body().to_owned());
        included.push(draft.id);
    }
    for draft in accepted().filter(|d| d.kind == "comment") {
        if let Some(choice) = &draft.choice {
            if matches!(choice, ThreadChoice::Reply { .. }) {
                included.push(draft.id);
            }
            in_thread(draft, choice, threads, &mut replies, &mut reactions)?;
            continue;
        }
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
    let empty = body.trim().is_empty() && comments.is_empty() && replies.is_empty();
    let review = match event {
        ReviewEvent::Approve => true,
        _ if !empty => true,
        _ if reactions.is_empty() => {
            return Err("nothing is accepted, so there's nothing to post".into());
        }
        ReviewEvent::RequestChanges => {
            return Err(
                "requesting changes needs a review body, a comment or a reply, and \
                        only thumbs-ups are accepted"
                    .into(),
            );
        }
        ReviewEvent::Comment => false,
    };
    let bodies = accepted()
        .filter(|d| included.contains(&d.id))
        .map(|d| (d.id, d.body().to_owned()))
        .collect();
    Ok(Built {
        review: review.then(|| NewReview {
            commit_id: run.head_sha.clone(),
            body,
            event,
            comments,
        }),
        replies,
        reactions,
        included,
        in_body,
        unthreaded: accepted().filter(|d| d.kind == "reply").count(),
        pending: drafts
            .iter()
            .filter(|d| d.status == "pending")
            .map(|d| d.id)
            .collect(),
        bodies,
        head: run.head_sha.clone(),
        left: None,
    })
}

/// Adds `draft`, accepted to post in an existing thread as `choice`, to
/// the `replies` in the review or the `reactions` after it. Drafts that
/// react to one comment share its reaction.
fn in_thread(
    draft: &DraftRow,
    choice: &ThreadChoice,
    threads: &[Thread],
    replies: &mut Vec<Reply>,
    reactions: &mut Vec<Reaction>,
) -> Result<(), String> {
    let thread = threads
        .iter()
        .find(|t| t.id == choice.thread() && !t.comments.is_empty())
        .cloned()
        .ok_or_else(|| {
            format!(
                "draft {} is for a thread that isn't on the PR any more; decide on it again",
                draft.id
            )
        })?;
    match choice {
        ThreadChoice::Reply { thread: id } => replies.push(Reply {
            new: NewReply {
                thread_id: id.clone(),
                body: draft.body().to_owned(),
            },
            draft: draft.id,
            thread,
        }),
        ThreadChoice::React { comment, .. } => {
            if let Some(same) = reactions.iter_mut().find(|r| r.new.comment_id == *comment) {
                same.drafts.push(draft.id);
                return Ok(());
            }
            let Some(target) = thread.comments.iter().find(|c| c.id == *comment).cloned() else {
                return Err(format!(
                    "draft {} is for a comment that isn't in its thread any more; decide on \
                     it again",
                    draft.id
                ));
            };
            reactions.push(Reaction {
                new: NewReaction {
                    comment_id: comment.clone(),
                },
                drafts: vec![draft.id],
                thread,
                comment: target,
            });
        }
    }
    Ok(())
}

/// A link to `draft`'s lines in the file at `head`, so a comment that
/// can't go inline still points at them. `None` without a path and line,
/// or for lines of the old file, which `head` doesn't have.
pub(crate) fn blob_link(draft: &DraftRow, head: &str) -> Option<String> {
    let (path, side, lines) = threads::lines(draft)?;
    (side == Side::Right).then(|| links::blob_url(&draft.key, head, path, lines))
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

/// A PR as a submit sees it: its page, and a run's review.
type Loaded = (PrPage, Result<Built, String>);

/// Loads `run` of the PR at `path` and builds its review.
fn load(app: &App, path: &RunPath, event: ReviewEvent) -> Result<Loaded, Error> {
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
    let mut drafts = store.draft_rows(run.id).map_err(Error::pr(&key))?;
    // GitHub has these, though the store couldn't say so.
    let taken = app.taken();
    for draft in &mut drafts {
        if taken.contains(&draft.id) {
            draft.status = "posted".into();
        }
    }
    let threads = store.threads(&key).map_err(Error::pr(&key))?;
    // Any run's: a regeneration copies drafts GitHub may have in it.
    let left = store.pending_review(&key).map_err(Error::pr(&key))?;
    let built = build(&run, &drafts, &threads, event).map(|built| Built { left, ..built });
    Ok((pr, built))
}

/// What's sent, as the confirm form carries it back. Compact, so it has no
/// line breaks for the browser to rewrite.
fn wire(built: &Built) -> Result<String, Error> {
    Ok(serde_json::to_string(&built.wire()).map_err(color_eyre::Report::from)?)
}

/// How long a pick lasts: long enough to read the preview, short enough
/// that an old preview URL, from history, has none.
pub(crate) const PICK_TTL: Duration = Duration::from_mins(10);

/// An approval picked on the PR page's verdict form. Approve is never
/// taken from a URL alone: only that form, a post behind the CSRF guard,
/// hands out a pick, and its id goes in the preview URL and the confirm
/// form. Posting the approval uses it up, and it expires, so replaying a
/// preview from history doesn't bring an approval back.
#[derive(Debug)]
pub struct Pick {
    run: i64,
    expires: SystemTime,
    /// The payload its preview last showed, the one it may post.
    shown: Option<String>,
}

impl Pick {
    fn live(&self, run: i64, now: SystemTime) -> bool {
        self.run == run && now < self.expires
    }
}

/// What an approval without a live pick is told.
const PICK_AGAIN: &str =
    "This approval's pick is used up or expired: pick Approve again on the PR page.";

#[derive(Debug, Deserialize)]
pub struct VerdictForm {
    event: ReviewEvent,
}

/// The PR page's verdict form: on to the preview, with a new pick for an
/// approval.
pub async fn pick(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Form(form): Form<VerdictForm>,
) -> Result<Response, Error> {
    let key = path.pr().key()?;
    let preview = format!(
        "{}/runs/{}/preview?event={}",
        pr_href(&key),
        path.run,
        form.event.as_str()
    );
    if form.event != ReviewEvent::Approve {
        return Ok(Redirect::to(&preview).into_response());
    }
    let id = Csrf::generate()?.token().to_owned();
    let now = app.clock.now();
    let mut picks = app.picks();
    picks.retain(|_, pick| now < pick.expires);
    picks.insert(
        id.clone(),
        Pick {
            run: path.run,
            expires: now + PICK_TTL,
            shown: None,
        },
    );
    Ok(Redirect::to(&format!("{preview}&pick={id}")).into_response())
}

/// Whether pick `id` is live and for `run`.
fn picked(app: &App, id: &str, run: i64) -> bool {
    let now = app.clock.now();
    app.picks().get(id).is_some_and(|pick| pick.live(run, now))
}

/// Uses up pick `id` to post `payload` from `run`, if it's live, for that
/// run, and its preview showed that payload.
fn take_pick(app: &App, id: &str, run: i64, payload: &str) -> bool {
    let now = app.clock.now();
    app.picks()
        .remove(id)
        .is_some_and(|pick| pick.live(run, now) && pick.shown.as_deref() == Some(payload))
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    event: ReviewEvent,
    /// An approval's pick; see [`Pick`].
    #[serde(default)]
    pick: String,
}

pub async fn preview(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Query(query): Query<PreviewQuery>,
) -> Result<Response, Error> {
    let PreviewQuery { event, pick } = query;
    if event == ReviewEvent::Approve && !picked(&app, &pick, path.run) {
        return Err(Error::Refused(PICK_AGAIN.into()));
    }
    let (pr, built) = load(&app, &path, event)?;
    let back = format!("{}?run={}", pr_href(&pr.key), path.run);
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
    let steps = built.steps(&pr.key, &app.me);
    let action = format!("{}/runs/{}/submit", pr_href(&pr.key), path.run);
    let approve = built.event() == Some(ReviewEvent::Approve);
    let payload = wire(&built)?;
    if approve {
        // The confirm may post only what this preview shows.
        if let Some(pick) = app.picks().get_mut(&pick) {
            pick.shown = Some(payload.clone());
        }
    }
    let bare = built
        .review
        .as_ref()
        .is_some_and(|r| r.body.is_empty() && r.comments.is_empty())
        && built.replies.is_empty();
    let content = html! {
        h1 { "Post this review?" }
        (sumline(&pr, &built))
        div.cols {
            div {
                (checklist(&built, &pr, &back))
                (readable_review(
                    links::At {
                        key: &pr.key,
                        reviewed: &built.head,
                        current: &pr.head_sha,
                    },
                    &built,
                ))
            }
            div.wirecol {
                @for (i, step) in steps.iter().enumerate() {
                    h2 {
                        "Exactly what's sent"
                        @if steps.len() > 1 { ", " (i + 1) " of " (steps.len()) }
                        " · " code { (step.endpoint) }
                    }
                    pre.wire { (step.body) }
                }
            }
        }
        form.foot #confirm method="post" action=(action) {
            (csrf_field(&app))
            input type="hidden" name="event" value=(event.as_str());
            input type="hidden" name="payload" value=(payload);
            @if approve {
                input type="hidden" name="pick" value=(pick);
            }
            button.btn.go type="submit" {
                @if approve && bare {
                    "Approve this PR"
                } @else if approve {
                    "Approve this PR with the above comments"
                } @else {
                    "Confirm and post to GitHub"
                }
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

/// The preview's summary line: the verdict, the PR, what's sent and at
/// which commit.
fn sumline(pr: &PrPage, built: &Built) -> Markup {
    let (chip, verdict_class) = match built.event() {
        Some(ReviewEvent::Comment) => ("COMMENT", "cm"),
        Some(ReviewEvent::RequestChanges) => ("REQUEST CHANGES", "rc"),
        Some(ReviewEvent::Approve) => ("APPROVE", "ap"),
        None => ("NO REVIEW", "cm"),
    };
    let review = built.review.as_ref();
    let comments = review.map_or(0, |r| r.comments.len());
    let (replies, reactions) = (built.replies.len(), built.reactions.len());
    html! {
        div.sumline {
            span.verdict-chip.(verdict_class) { (chip) }
            span { (pr.title) " " (pr_ref(&pr.key)) }
            span {
                @if let Some(review) = review {
                    @if review.body.is_empty() { "no body" } @else { "body" }
                    " + " (comments) @if comments == 1 { " inline comment" } @else { " inline comments" }
                    @if replies > 0 {
                        " + " (replies) @if replies == 1 { " reply" } @else { " replies" }
                    }
                } @else {
                    "no review"
                }
                @if reactions > 0 { " + " (reactions) " 👍" }
            }
            @if let Some(review) = review {
                span { "at " code { (pr::short(&review.commit_id)) } }
            }
        }
    }
}

/// What to check before posting: a stale head, comments moved into the
/// body, replies and thumbs-ups in existing threads, Markdown that hides
/// things, and drafts left out.
fn checklist(built: &Built, pr: &PrPage, back: &str) -> Markup {
    let review = built.review.as_ref();
    let texts = review
        .into_iter()
        .flat_map(|r| std::iter::once(&r.body).chain(r.comments.iter().map(|c| &c.body)))
        .chain(built.replies.iter().map(|r| &r.new.body));
    let missable = easy_to_miss(texts);
    let mut checks = Vec::new();
    match built.left.as_ref().map(|left| &left.on_github) {
        Some(OnGithub::Pending { .. }) => checks.push(html! {
            b { "An earlier submit on this PR left a review pending." }
            " It's checked first: if GitHub submitted it after all, its drafts are marked "
            "posted and nothing else is sent; if it's still pending, it's deleted, then "
            "this is posted."
        }),
        Some(OnGithub::Sent(_)) => checks.push(html! {
            b { "An earlier submit on this PR sent a review GitHub never answered." }
            " It's looked for first: if GitHub has it, its drafts are marked posted and "
            "nothing else is sent; if not, this is posted."
        }),
        None => {}
    }
    if let Some(review) = review
        && review.commit_id != pr.head_sha
    {
        checks.push(pr::moved_on(&review.commit_id, &pr.head_sha));
    }
    if built.in_body > 0 {
        checks.push(html! {
            b { (built.in_body) }
            " accepted comment(s) aren't on a line of the diff, so "
            "they're in the body, each headed by a link to its lines."
        });
    }
    if !built.replies.is_empty() {
        checks.push(html! {
            b { (built.replies.len()) }
            " draft(s) go as replies in existing threads, in the review. If a reply "
            "fails, the pending review is deleted, so nothing is posted."
        });
    }
    if !built.reactions.is_empty() {
        checks.push(html! {
            b { (built.reactions.len()) }
            " 👍 go on existing comments, in place of those drafts' text"
            @if review.is_some() { ", after the review" }
            "."
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
    if built.unthreaded > 0 {
        checks.push(html! {
            b { (built.unthreaded) }
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

/// The review as GitHub will show it: its body, then each inline comment;
/// then the replies in existing threads, and the thumbs-ups.
fn readable_review(at: links::At<'_>, built: &Built) -> Markup {
    html! {
    @if let Some(review) = &built.review {
        @let comments = review.comments.len();
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
    } @else {
        h2.sec { "No review" }
        p.dim { "Only thumbs-ups are accepted, so no review is posted." }
    }
    @if !built.replies.is_empty() {
        h2.sec { "Replies in existing threads " span.dim { (built.replies.len()) } }
        @for reply in &built.replies {
            div.rv {
                (threads::thread_box(at, &reply.thread))
                div.dim { "draft " (reply.draft) ", in reply:" }
                pre { (reply.new.body) }
            }
        }
    }
    @if !built.reactions.is_empty() {
        h2.sec { "👍 on existing comments " span.dim { (built.reactions.len()) } }
        @for reaction in &built.reactions {
            div.rv {
                (threads::thread_head(at, &reaction.thread))
                div { "👍 on " (threads::said(&reaction.comment)) }
                div.dim {
                    "in place of "
                    @for (i, id) in reaction.drafts.iter().enumerate() {
                        @if i > 0 { ", " }
                        "draft " (id)
                    }
                }
            }
        }
    }
    }
}

#[derive(Debug, Deserialize)]
pub struct SubmitForm {
    event: ReviewEvent,
    /// The payload the preview showed, as [`wire`] wrote it.
    payload: String,
    /// An approval's pick, as the preview had it; see [`Pick`].
    #[serde(default)]
    pick: String,
}

/// Posts the review with its replies, then the thumbs-ups, if they're
/// still exactly what the preview showed. Each request is sent once: a
/// failure is shown, never retried. What GitHub took is marked posted as
/// soon as it has it, so a submit after a failure sends only the rest.
pub async fn submit(
    State(app): State<Shared>,
    Path(path): Path<RunPath>,
    Form(form): Form<SubmitForm>,
) -> Result<Response, Error> {
    let approve = form.event == ReviewEvent::Approve;
    if approve && !take_pick(&app, &form.pick, path.run, &form.payload) {
        return Err(Error::Refused(format!("{PICK_AGAIN} Nothing was sent.")));
    }
    let mut posted_before = app.posting.lock().await;
    let (pr, built) = load(&app, &path, form.event)?;
    let key = &pr.key;
    let back = format!("{}?run={}", pr_href(key), path.run);
    let sent = format!("{}:{}", path.run, form.payload);
    let built = match built {
        Ok(built) if wire(&built)? == form.payload && !posted_before.contains(&sent) => built,
        _ => {
            let content = changed_since_preview(&pr, path.run, form.event, &back);
            let page = result_page(&app, key, "Not posted", &content);
            return Ok((StatusCode::CONFLICT, page).into_response());
        }
    };
    let mut marking = Vec::new();
    if let Some(left) = &built.left {
        match settle(&app, key, path.run, left).await {
            Ok(Settled::Cleared) => {}
            Ok(Settled::Submitted(done)) => {
                posted_before.insert(sent);
                app.control.refresh(key.clone());
                let content = submitted_after_all(&pr, left.run, &done, &back, path.run);
                let page = result_page(&app, key, "Posted earlier", &content);
                return Ok((StatusCode::CONFLICT, page).into_response());
            }
            Err(failure) => {
                let content = review_failed(&pr, &built, &failure, &back);
                let page = result_page(&app, key, "Not posted", &content);
                return Ok((StatusCode::BAD_GATEWAY, page).into_response());
            }
        }
    }
    let mut posted = None;
    let (mut copies, mut copies_marked) = (Vec::new(), true);
    if let Some(review) = &built.review {
        match post_review(&app, key, path.run, review, &built).await {
            Ok(review) => {
                info!(url = %key.url(), review = %review, "review posted");
                posted_before.insert(sent.clone());
                let failed;
                (copies, failed) = mark_review(&app, key, path.run, &built);
                copies_marked = failed.is_none();
                marking.extend(failed);
                posted = Some(review);
            }
            Err(failure) => {
                let content = review_failed(&pr, &built, &failure, &back);
                let page = result_page(&app, key, "Not posted", &content);
                return Ok((StatusCode::BAD_GATEWAY, page).into_response());
            }
        }
    }
    // After the review, so a failure here leaves it posted and marked, and
    // a submit again sends only the thumbs-ups left.
    let (reacted, failed) = react(&app, key, &built.reactions, &mut marking).await;
    // Whatever GitHub took shows here now, not at the next reconcile.
    if posted.is_some() || reacted > 0 {
        app.control.refresh(key.clone());
    }
    if failed.is_none() {
        posted_before.insert(sent);
    } else {
        info!(url = %key.url(), reacted, "thumbs-ups added before one failed");
    }
    let outcome = Outcome {
        posted: posted.as_deref(),
        reacted,
        failed: failed.as_ref().map(|(r, err)| (*r, plain(err))),
        copies,
        copies_marked,
        marking,
    };
    let status = if outcome.failed.is_some() {
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::OK
    };
    let content = posted_card(&pr, &built, &outcome, &back, path.run);
    let page = result_page(&app, key, outcome.heading(), &content);
    Ok((status, page).into_response())
}

/// What a confirm whose drafts changed since its preview, or were already
/// posted, says: nothing was sent.
fn changed_since_preview(pr: &PrPage, run: i64, event: ReviewEvent, back: &str) -> Markup {
    let approve = event == ReviewEvent::Approve;
    let preview = format!(
        "{}/runs/{run}/preview?event={}",
        pr_href(&pr.key),
        event.as_str()
    );
    result_card(
        pr,
        "Not posted",
        html! {
            p.note {
                "The drafts changed since the preview, or were already posted, so "
                "nothing was sent."
                // Its pick is used up.
                @if approve { " Pick Approve again on the PR page." }
            }
        },
        html! {
            @if !approve { a.btn.primary href=(preview) { "Preview again" } }
        },
        (back, "Back to the drafts"),
        Tone::Ask,
    )
}

/// How far a review got, when GitHub didn't take all of it.
#[derive(Debug)]
enum Failure {
    /// Checking on the review an earlier submit left pending, or sent
    /// without an answer, failed.
    Check(ApiError),
    /// Deleting the review an earlier submit left pending failed.
    Discard(ApiError),
    /// GitHub didn't create the pending review.
    NotCreated(ApiError),
    /// A review sent in one call couldn't be recorded here first, so it
    /// wasn't sent.
    NotRecorded(color_eyre::Report),
    /// GitHub refused a review sent in one call, so it isn't posted.
    Refused(ApiError),
    /// A review sent in one call failed, and GitHub may have it anyway.
    Unanswered(ApiError),
    /// The pending review couldn't be recorded here, so it was deleted
    /// rather than risk not knowing about it; `discarded` says how that went.
    Unrecorded {
        err: color_eyre::Report,
        discarded: Result<(), ApiError>,
    },
    /// Reply `index` failed; `discarded` says how deleting the pending
    /// review went.
    Reply {
        index: usize,
        err: ApiError,
        discarded: Result<(), ApiError>,
    },
    /// Submitting the pending review failed: it may be submitted anyway.
    NotSubmitted(ApiError),
}

/// How [`settle`] left the review an earlier submit left.
enum Settled {
    /// Deleted, gone or not found, and forgotten: post afresh.
    Cleared,
    /// GitHub had it submitted after all, and nothing else is sent.
    Submitted(Submitted),
}

/// What settling a review GitHub had submitted after all did.
struct Submitted {
    /// Its page on GitHub.
    html_url: String,
    /// Its drafts' copies in the PR's runs.
    copies: Copies,
    /// Why they couldn't be found or marked, if they couldn't. The record
    /// is kept, so the next submit, after a restart too, finds it again.
    marking: Option<color_eyre::Report>,
}

/// Settles `left`, the review an earlier submit on the PR left pending or
/// sent without an answer, before run `run` posts anything.
async fn settle(
    app: &App,
    key: &PrKey,
    run: i64,
    left: &PendingReview,
) -> Result<Settled, Failure> {
    let submitted = match &left.on_github {
        OnGithub::Pending { node_id, html_url } => {
            let state = app
                .github
                .review_state(key, node_id)
                .await
                .map_err(Failure::Check)?;
            match state {
                ReviewStatus::Submitted => Some(html_url.clone()),
                ReviewStatus::Pending => {
                    app.github
                        .delete_review(key, node_id)
                        .await
                        .map_err(Failure::Discard)?;
                    None
                }
                ReviewStatus::Gone => None,
            }
        }
        OnGithub::Sent(sent) => {
            let event = ReviewEvent::parse(&sent.event).ok_or_else(|| {
                Failure::Check(color_eyre::eyre::eyre!("`{}` isn't a verdict", sent.event).into())
            })?;
            let sent = sanic_github::SentReview {
                commit_id: &sent.commit_id,
                event,
                body: &sent.body,
                comments: &sent.comments,
                after: &sent.after,
            };
            app.github
                .find_review(key, &app.me, &sent)
                .await
                .map_err(Failure::Check)?
        }
    };
    if let Some(html_url) = submitted {
        info!(url = %key.url(), review = %html_url, "the review an earlier submit left was posted");
        let ids: Vec<i64> = left.drafts.iter().map(|(id, _)| *id).collect();
        let (copies, marking) = mark_with_copies(app, key, run, &left.drafts, &ids);
        if marking.is_none() {
            forget(app, key);
        }
        return Ok(Settled::Submitted(Submitted {
            html_url,
            copies,
            marking,
        }));
    }
    forget(app, key);
    Ok(Settled::Cleared)
}

/// Drafts of a PR's runs related to ones a review posted.
#[derive(Default)]
struct Copies {
    /// Word for word ones it posted, kind and anchor too, so they're
    /// marked posted as well: each with its run.
    copies: Vec<(i64, i64)>,
    /// The submitted run's that share revisions with ones it posted but
    /// aren't word for word those, which may repeat them, left as they are
    /// for you to check.
    revised: Vec<i64>,
}

/// Marks the drafts of `run`'s review, which GitHub just took, posted with
/// their copies, and forgets its record. If marking fails, it's
/// kept, for the next submit, after a restart too, to find submitted and
/// mark. The copies marked, as `(run, draft)`, and why marking failed.
fn mark_review(
    app: &App,
    key: &PrKey,
    run: i64,
    built: &Built,
) -> (Vec<(i64, i64)>, Option<color_eyre::Report>) {
    let (found, failed) = mark_with_copies(app, key, run, &built.bodies, &built.included);
    if failed.is_none() {
        forget(app, key);
    }
    (found.copies, failed)
}

/// Marks `ids`, the drafts GitHub took, posted, with their copies in the
/// PR's runs (see [`copies_of`]), so a submit of any of them doesn't post
/// them again. `posted` has what each draft went out as. Returns the
/// copies, and why finding or marking them failed, if it did.
fn mark_with_copies(
    app: &App,
    key: &PrKey,
    run: i64,
    posted: &[(i64, String)],
    ids: &[i64],
) -> (Copies, Option<color_eyre::Report>) {
    let (found, failed) = match copies_of(app, key, run, posted) {
        Ok(found) => (found, None),
        Err(err) => {
            warn!(url = %key.url(), "finding copies of posted drafts failed: {err:?}");
            (Copies::default(), Some(err))
        }
    };
    let mut ids = ids.to_vec();
    ids.extend(found.copies.iter().map(|(_, id)| *id));
    let marking = mark(app, key, &ids).or(failed);
    (found, marking)
}

/// The copies GitHub has of `posted`, drafts with the bodies they went out
/// with, in `key`'s runs: drafts that share a line of revisions with one
/// (through `based_on`: revised from it, it from them, or both from one
/// draft), and are word for word what it posted, with the same kind and
/// anchor. Each comes with its run. Also, `run`'s drafts that share one
/// but aren't copies, which may repeat what was posted.
fn copies_of(
    app: &App,
    key: &PrKey,
    run: i64,
    posted: &[(i64, String)],
) -> color_eyre::Result<Copies> {
    let rows: HashMap<i64, DraftRow> = {
        let store = app.store();
        let mut rows = HashMap::new();
        for review in store.review_runs(key)? {
            rows.extend(store.draft_rows(review.id)?.into_iter().map(|d| (d.id, d)));
        }
        rows
    };
    let taken = app.taken().clone();
    // A draft and what it's revised from, up the chain a run at a time.
    let lineage = |id: i64| {
        let mut chain = HashSet::new();
        let mut from = Some(id);
        while let Some(id) = from.filter(|_| chain.len() < 64) {
            if !chain.insert(id) {
                break;
            }
            from = rows.get(&id).and_then(|d| d.based_on);
        }
        chain
    };
    let posted: Vec<(&DraftRow, &String, HashSet<i64>)> = posted
        .iter()
        .filter_map(|(id, body)| Some((rows.get(id)?, body, lineage(*id))))
        .collect();
    let mut drafts: Vec<&DraftRow> = rows
        .values()
        .filter(|d| {
            d.status != "posted"
                && !taken.contains(&d.id)
                && !posted.iter().any(|(p, ..)| p.id == d.id)
        })
        .collect();
    drafts.sort_unstable_by_key(|d| d.id);
    let (mut copies, mut revised) = (Vec::new(), Vec::new());
    for draft in drafts {
        let chain = lineage(draft.id);
        let mut related = posted
            .iter()
            .filter(|(.., theirs)| !theirs.is_disjoint(&chain))
            .peekable();
        if related.peek().is_none() {
            continue;
        }
        let copy = related.any(|(base, body, _)| {
            base.kind == draft.kind
                && (&base.path, base.line, base.start_line, &base.side)
                    == (&draft.path, draft.line, draft.start_line, &draft.side)
                && body.as_str() == draft.body()
        });
        if copy {
            copies.push((draft.run_id, draft.id));
        } else if draft.run_id == run {
            revised.push(draft.id);
        }
    }
    Ok(Copies { copies, revised })
}

/// How far before sending a review sent in one call a review of yours
/// may be dated and still be it, for GitHub's clock being behind ours.
const CLOCK_SKEW: Duration = Duration::from_mins(10);

/// Posts `review` with `built`'s replies, returning its page. The record
/// made here makes the next submit look for it first, so a submit that
/// failed after GitHub took it isn't posted twice; it stays until the
/// caller has marked its drafts posted.
///
/// Without replies it's one call that creates and submits it, recorded
/// before it's sent. With them it's created pending, recorded, given its
/// replies and submitted; until the submit, nothing in it is visible to
/// anyone else.
async fn post_review(
    app: &App,
    key: &PrKey,
    run: i64,
    review: &NewReview,
    built: &Built,
) -> Result<String, Failure> {
    let github = &app.github;
    if built.replies.is_empty() {
        let after = app
            .clock
            .now()
            .checked_sub(CLOCK_SKEW)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let sent = PendingReview {
            run,
            drafts: built.bodies.clone(),
            on_github: OnGithub::Sent(SentReview {
                commit_id: review.commit_id.clone(),
                event: review.event.as_str().to_owned(),
                body: review.body.clone(),
                comments: review.comments.iter().map(|c| c.body.clone()).collect(),
                after: rfc3339(after),
            }),
        };
        app.store()
            .record_pending_review(key, &sent)
            .map_err(Failure::NotRecorded)?;
        return match github.create_review(key, review).await {
            Ok(created) => Ok(created.html_url),
            // Known not to be posted, so there's nothing to look for.
            Err(PostError::Refused(err)) => {
                forget(app, key);
                Err(Failure::Refused(err))
            }
            Err(PostError::Unknown(err)) => Err(Failure::Unanswered(err)),
        };
    }
    let created = github
        .create_pending_review(key, review)
        .await
        .map_err(Failure::NotCreated)?;
    let pending = PendingReview {
        run,
        drafts: built.bodies.clone(),
        on_github: OnGithub::Pending {
            node_id: created.node_id.clone(),
            html_url: created.html_url.clone(),
        },
    };
    let node_id = &created.node_id;
    let recorded = app.store().record_pending_review(key, &pending);
    if let Err(err) = recorded {
        let discarded = github.delete_review(key, node_id).await;
        return Err(Failure::Unrecorded { err, discarded });
    }
    for (index, reply) in built.replies.iter().enumerate() {
        if let Err(err) = github.add_reply(key, node_id, &reply.new).await {
            let discarded = github.delete_review(key, node_id).await;
            if discarded.is_ok() {
                forget(app, key);
            }
            return Err(Failure::Reply {
                index,
                err,
                discarded,
            });
        }
    }
    github
        .submit_review(key, node_id, review)
        .await
        .map_err(Failure::NotSubmitted)?;
    Ok(created.html_url)
}

/// Forgets the PR's recorded review. If that fails, the next submit
/// checks it again, which finds it posted or not; nothing is sent twice.
fn forget(app: &App, key: &PrKey) {
    if let Err(err) = app.store().clear_pending_review(key) {
        warn!(url = %key.url(), "forgetting the pending review failed: {err:?}");
    }
}

/// Sends `reactions` in turn, marking each one's drafts posted as GitHub
/// takes it, until one fails: how many were sent, and the one that failed.
async fn react<'a>(
    app: &App,
    key: &PrKey,
    reactions: &'a [Reaction],
    marking: &mut Vec<color_eyre::Report>,
) -> (usize, Option<(&'a Reaction, ApiError)>) {
    let mut reacted = 0;
    for reaction in reactions {
        // Checked first, so one whose answer was lost isn't sent again.
        let sent = match app.github.has_thumbs_up(&reaction.new).await {
            Ok(true) => Ok(()),
            Ok(false) => app.github.add_reaction(&reaction.new).await,
            Err(err) => Err(err),
        };
        if let Err(err) = sent {
            warn!(url = %key.url(), "adding a thumbs-up failed: {err}");
            return (reacted, Some((reaction, err)));
        }
        reacted += 1;
        marking.extend(mark(app, key, &reaction.drafts));
    }
    (reacted, None)
}

/// Marks `ids` posted, returning the failure, if any, to show. If the
/// store can't, they're remembered as posted until `serve` restarts.
fn mark(app: &App, key: &PrKey, ids: &[i64]) -> Option<color_eyre::Report> {
    let marked = app.store().mark_posted(ids);
    if let Err(err) = &marked {
        warn!(url = %key.url(), "marking drafts posted failed: {err:?}");
        app.taken().extend(ids);
    }
    marked.err()
}

/// What a submit that found the earlier attempt's review submitted after
/// all says: nothing else was sent, and the rest needs a new preview.
fn submitted_after_all(
    pr: &PrPage,
    left_run: i64,
    done: &Submitted,
    back: &str,
    run: i64,
) -> Markup {
    let marking = done.marking.as_ref();
    let list = |ids: &[i64]| {
        html! {
            @for (i, id) in ids.iter().enumerate() {
                @if i > 0 { ", " }
                a href={ (back) "#draft-" (id) } { "draft " (id) }
            }
        }
    };
    let preview = format!(
        "{}/runs/{run}/preview?event={}",
        pr_href(&pr.key),
        ReviewEvent::Comment.as_str()
    );
    result_card(
        pr,
        "Posted earlier",
        html! {
            p.note {
                "The review an earlier submit left was submitted after all, so its drafts "
                "are now marked posted and nothing else was sent."
                @if left_run != run { " It was from " a href={ (pr_href(&pr.key)) "?run=" (left_run) } { "another run" } "." }
            }
            (copies_note(&pr.key, &done.copies.copies, marking.is_none()))
            @if !done.copies.revised.is_empty() {
                p.warn {
                    "This run's " (list(&done.copies.revised)) " are revised from drafts it posted, "
                    "and may repeat them: check them against "
                    a href=(done.html_url) { "the posted review" } " before you submit again."
                }
            }
            p.note { "Preview again for anything left." }
            @if let Some(err) = marking {
                p.warn {
                    "GitHub has it, but marking its drafts posted failed: " (err)
                    ". Don't submit them again."
                }
            }
        },
        html! {
            a.btn.primary href=(preview) { "Preview the rest with Comment" }
            @if done.html_url.starts_with("https://") {
                a.btn href=(done.html_url) { "Open it on GitHub ↗" }
            }
        },
        (back, "Back to the drafts"),
        Tone::Ask,
    )
}

/// An error as plain text: the report's `Debug` carries terminal colours
/// and a backtrace hint.
fn plain(err: &ApiError) -> String {
    match err {
        ApiError::Other(report) => format!("{report:#}"),
        other => other.to_string(),
    }
}

/// What a review GitHub didn't take says: how far it got, and what that
/// leaves on GitHub. Nothing is marked posted.
fn review_failed(pr: &PrPage, built: &Built, failure: &Failure, back: &str) -> Markup {
    let key = &pr.key;
    let replies = built.replies.len();
    // Whether the next submit finds it depends on whether it's recorded.
    let discard_note = |discarded: &Result<(), ApiError>, recorded: bool| match discarded {
        Ok(()) => html! { "The pending review was deleted, so nothing was posted." },
        Err(err) => html! {
            "Deleting the pending review failed too: " code { (plain(err)) } ". It's on "
            a href=(key.url()) { "the PR" } ", visible only to you; "
            @if recorded {
                "the next submit checks it first and deletes it."
            } @else {
                "discard it there: GitHub takes only one pending review of yours per PR."
            }
        },
    };
    let (what, err, note) = match failure {
        Failure::Check(err) => (
            "checking the review left pending",
            plain(err),
            html! {
                "An earlier submit left a review pending, and it couldn't be checked, so "
                "nothing was sent: it may have been submitted after all."
            },
        ),
        Failure::Discard(err) => (
            "deleting the review left pending",
            plain(err),
            html! {
                "An earlier submit left a review pending, visible only to you, and it "
                "couldn't be deleted, so nothing was sent."
            },
        ),
        Failure::NotCreated(err) => (
            "creating the pending review",
            plain(err),
            html! {
                "Nothing was posted. If GitHub created it anyway, it's pending on "
                a href=(key.url()) { "the PR" } ", visible only to you: discard it there."
            },
        ),
        Failure::NotRecorded(err) => (
            "recording the review before sending it",
            format!("{err:#}"),
            html! { "It couldn't be recorded here first, so it wasn't sent." },
        ),
        Failure::Refused(err) => (
            "posting the review",
            plain(err),
            html! { "GitHub refused it, so it wasn't posted." },
        ),
        Failure::Unanswered(err) => (
            "posting the review",
            plain(err),
            html! {
                "GitHub may have taken it anyway. The next submit looks for it first: if "
                "GitHub has it, its drafts are marked posted and it isn't sent again."
            },
        ),
        Failure::Unrecorded { err, discarded } => (
            "recording the pending review",
            format!("{err:#}"),
            html! {
                "The review was created pending but couldn't be recorded here, so it isn't "
                "submitted. " (discard_note(discarded, false))
            },
        ),
        Failure::Reply {
            index,
            err,
            discarded,
        } => (
            "adding a reply to the pending review",
            plain(err),
            html! { "Adding reply " (index + 1) " of " (replies) " failed. " (discard_note(discarded, true)) },
        ),
        Failure::NotSubmitted(err) => (
            "submitting the pending review",
            plain(err),
            html! {
                "Submitting the review failed, but GitHub may have taken it. The next submit "
                "checks it first: if it was submitted, its drafts are marked posted and it "
                "isn't sent again; if it's still pending, it's deleted and posted afresh."
            },
        ),
    };
    warn!(url = %key.url(), "{what} failed: {err}");
    result_card(
        pr,
        "Posting failed",
        html! {
            pre.error { (err) }
            p.note { (note) " Nothing was marked posted, and nothing is retried." }
            @if !built.reactions.is_empty() {
                p.note { "No 👍 were sent: they go after the review." }
            }
        },
        html! {},
        (back, "Back to the drafts"),
        Tone::Failed,
    )
}

/// How a submit went, once GitHub took something.
struct Outcome<'a> {
    /// The review's page, if there was one to post.
    posted: Option<&'a str>,
    /// Thumbs-ups added.
    reacted: usize,
    /// The thumbs-up that failed, and why; none after it were sent.
    failed: Option<(&'a Reaction, String)>,
    /// Word for word copies of the posted drafts in the PR's runs, as
    /// `(run, draft)`, marked posted with them.
    copies: Vec<(i64, i64)>,
    /// Whether marking the review's drafts and `copies` posted worked.
    copies_marked: bool,
    /// Failures marking what GitHub took as posted.
    marking: Vec<color_eyre::Report>,
}

impl Outcome<'_> {
    /// "Not posted" when the only request, or the first of only
    /// thumbs-ups, failed.
    fn heading(&self) -> &'static str {
        match (&self.failed, self.posted, self.reacted) {
            (None, ..) => "Posted",
            (Some(_), None, 0) => "Not posted",
            (Some(_), ..) => "Partly posted",
        }
    }
}

/// The drafts that are word for word `copies` of posted ones, each linked
/// through its own run, and whether they were `marked` posted with them.
/// Nothing when there are none.
fn copies_note(key: &PrKey, copies: &[(i64, i64)], marked: bool) -> Markup {
    html! {
        @if !copies.is_empty() {
            p.note {
                @for (i, (run, id)) in copies.iter().enumerate() {
                    @if i > 0 { ", " }
                    a href={ (pr_href(key)) "?run=" (run) "#draft-" (id) } { "draft " (id) }
                }
                @if copies.len() == 1 { " is" } @else { " are" }
                " word for word what was posted, so "
                @if marked {
                    @if copies.len() == 1 { "it's" } @else { "they're" }
                    " marked posted too."
                } @else {
                    "don't submit " @if copies.len() == 1 { "it" } @else { "them" } " either."
                }
            }
        }
    }
}

/// What a submit GitHub took looks like: the card, with a link to the
/// review there, the thumbs-ups added and any that failed, with a way to
/// preview the rest, and a warning if drafts couldn't be marked posted.
fn posted_card(pr: &PrPage, built: &Built, outcome: &Outcome<'_>, back: &str, run: i64) -> Markup {
    let preview_rest = format!(
        "{}/runs/{run}/preview?event={}",
        pr_href(&pr.key),
        ReviewEvent::Comment.as_str()
    );
    let key = &pr.key;
    let comments = built.review.as_ref().map_or(0, |r| r.comments.len());
    let replies = built.replies.len();
    let meta = html! {
        (pr_ref(key))
        @if let Some(event) = built.event() {
            " · " (verdict(event)) " · "
            (comments) @if comments == 1 { " inline comment" } @else { " inline comments" }
            @if replies > 0 { " · " (replies) @if replies == 1 { " reply" } @else { " replies" } }
        }
        @if outcome.reacted > 0 { " · " (outcome.reacted) " 👍" }
    };
    let extra = html! {
        (copies_note(key, &outcome.copies, outcome.copies_marked))
        @if let Some((reaction, why)) = &outcome.failed {
            p.warn {
                "The 👍 on " (reaction.comment.author) "'s comment failed"
                @let left = built.reactions.len() - outcome.reacted;
                @if left > 1 { ", so it and the " (left - 1) " after it weren't sent" }
                ": "
            }
            pre.error { (why) }
            p.note {
                "Everything GitHub took is marked posted. To send the rest, preview again "
                "with Comment, which sends only what's left: Approve would post a second "
                "review, and Request changes needs one. A 👍 the comment already has "
                "from you isn't sent again."
            }
        }
        @for err in &outcome.marking {
            p.warn {
                "GitHub has it, but marking its drafts posted failed: " (err)
                ". Don't submit them again."
            }
        }
    };
    let go = html! {
        @if outcome.failed.is_some() {
            // Comment: an approval would post a second review.
            a.btn.primary href=(preview_rest) { "Preview the rest with Comment" }
        }
        // It's GitHub's to say, but only a web link is a link.
        @match outcome.posted {
            Some(url) if url.starts_with("https://") => {
                a.btn.primary href=(url) { "Open it on GitHub ↗" }
            }
            Some(url) => code { (url) },
            None => a.btn.primary href=(key.url()) { "Open the PR on GitHub ↗" },
        }
    };
    let tone = if outcome.failed.is_some() {
        Tone::Failed
    } else {
        Tone::Done
    };
    let heading = outcome.heading();
    page::card(&Card {
        kind: "Submit review",
        heading: html! { (heading) },
        title: &pr.title,
        meta,
        extra,
        cost: None,
        go,
        back: (back, "Back to the PR"),
        tone,
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
            choice: None,
            note: None,
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
