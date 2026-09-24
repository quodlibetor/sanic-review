//! The index row's cells that open a popover: the lead's run and draft
//! details, and who reviewed the PR.

use std::time::SystemTime;

use maud::{Markup, html};
use sanic_core::{
    clock::{ago, parse_rfc3339},
    pr::{PrKey, is_login},
    reviewers::{Reviewer, Stance},
    skip::Skip,
};
use sanic_store::{Decided, RowFacts};

/// How long ago `at` was, e.g. `3h ago`, with the time itself on hover.
/// Nothing if `at` isn't a time.
pub fn since(now: SystemTime, at: &str) -> Markup {
    html! {
        @if let Some(then) = parse_rfc3339(at) {
            time.ago datetime=(at) title=(at) {
                @match ago(now, then).as_str() {
                    "now" => "just now",
                    ago => { (ago) " ago" }
                }
            }
        }
    }
}

/// An element id for `what` about `key`, as `owner/name#N`: names with
/// hyphens in them can't run together.
fn id(what: &str, key: &PrKey) -> String {
    format!("{what}-{key}")
}

/// A reviewer's verdict, as a mark with its word on hover.
fn mark(stance: Stance) -> Markup {
    let (class, mark) = match stance {
        Stance::Approved => ("ap", "✓"),
        Stance::ChangesRequested => ("rc", "✗"),
        Stance::Commented => ("cm", "○"),
    };
    html! { span.vm.(class) aria-label=(stance.word()) title=(stance.word()) { (mark) } }
}

/// `n` and the word for that many, e.g. `2 pushes`.
pub fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The lead, with a popover of what's behind it: the latest run, why the
/// PR is skipped, its drafts by status, and a failed run's whole error.
pub struct LeadPop<'a> {
    pub key: &'a PrKey,
    pub text: &'a str,
    pub class: &'a str,
    pub facts: Option<&'a RowFacts>,
    /// The latest run's status and error, if it has run.
    pub status: Option<&'a str>,
    pub error: Option<&'a str>,
    pub skip: Option<&'a Skip>,
    /// The status shown for a review waiting out the quiet period.
    pub waiting: Option<&'a str>,
    pub manual_reviews: bool,
    pub now: SystemTime,
}

impl LeadPop<'_> {
    pub fn render(&self) -> Markup {
        let pid = id("lead", self.key);
        let facts = self.facts;
        let times = facts.and_then(|f| f.latest_run.as_ref());
        let time = |at: Option<&String>| {
            html! { @if let Some(at) = at { " " (since(self.now, at)) } }
        };
        let run = html! {
            @if let Some(waiting) = self.waiting {
                "waiting out the quiet period: " (waiting)
            } @else {
                @match self.status {
                    None => "none yet",
                    Some("queued") => {
                        "queued" (time(times.map(|t| &t.queued_at)))
                        @if self.manual_reviews {
                            "; --manual-reviews holds it until you start it (r)"
                        } @else {
                            "; it starts when a slot is free"
                        }
                    }
                    Some("running") => {
                        "started" (time(times.and_then(|t| t.started_at.as_ref())))
                    }
                    Some("succeeded") => {
                        "succeeded, finished" (time(times.and_then(|t| t.finished_at.as_ref())))
                    }
                    Some("superseded") => "superseded: replaced before it finished",
                    Some(status) => {
                        (status) (time(times.and_then(|t| t.finished_at.as_ref())))
                    }
                }
            }
        };
        let decided = facts.and_then(|f| f.decided.as_ref());
        html! {
            span.pp tabindex="0" aria-describedby=(pid) {
                span.(self.class) { (self.text) }
                span.pop role="tooltip" id=(pid) {
                    table {
                        @if let Some(skip) = self.skip {
                            tr { td.when { "status" } td { (skip_words(skip)) } }
                        }
                        tr { td.when { "review run" } td { (run) } }
                        @if let Some(decided) = decided.filter(|d| has_drafts(d)) {
                            tr { td.when { "drafts" } td { (self.drafts(decided)) } }
                        }
                    }
                    @if let Some(error) = self.error {
                        pre.error.perr { (error) }
                    }
                }
            }
        }
    }

    fn drafts(&self, d: &Decided) -> Markup {
        let mut parts: Vec<Markup> = Vec::new();
        if d.pending > 0 {
            parts.push(html! { span.cnt { (d.pending) " pending" } });
        }
        if d.accepted > 0 {
            parts.push(html! { span.sig.post { (d.accepted) " accepted, not posted" } });
        }
        if d.rejected > 0 {
            let all = d.pending + d.accepted + d.posted == 0;
            parts.push(html! {
                span.sig.decided { @if all && d.rejected > 1 { "all " } (d.rejected) " rejected" }
            });
        }
        if d.posted > 0 {
            parts.push(html! {
                span.sig.decided {
                    (d.posted) " posted"
                    @if let Some(at) = &d.posted_at { " " (since(self.now, at)) }
                }
            });
        }
        html! {
            @for (i, part) in parts.iter().enumerate() {
                @if i > 0 { " · " }
                (part)
            }
        }
    }
}

fn has_drafts(d: &Decided) -> bool {
    d.pending + d.accepted + d.rejected + d.posted > 0
}

/// Why a PR isn't reviewed automatically, in a sentence.
fn skip_words(skip: &Skip) -> Markup {
    html! {
        @match skip {
            Skip::Archived => "archived by you: not reviewed automatically",
            Skip::Draft => "not reviewed automatically: it's a draft PR",
            Skip::Reviewed { .. } => {
                "not reviewed automatically: someone reviewed the current head (see reviewed by)"
            }
            Skip::Title { pattern } => {
                "not reviewed automatically: the title matches " code { (pattern) }
            }
        }
    }
}

/// Who reviewed the PR, each with their verdict, and a popover with each
/// one's latest review. Always shown: "no reviews" says so.
pub struct ReviewedBy<'a> {
    pub key: &'a PrKey,
    pub me: &'a str,
    pub head: &'a str,
    pub facts: Option<&'a RowFacts>,
    pub now: SystemTime,
}

impl ReviewedBy<'_> {
    pub fn render(&self) -> Markup {
        let pid = id("rb", self.key);
        let reviewers = self.facts.map_or(&[][..], |f| &f.reviewers[..]);
        let you = reviewers.iter().find(|r| is_login(&r.login, self.me));
        let since_yours = you.map_or(0, |r| r.pushes_since);
        html! {
            span.rb tabindex="0" aria-describedby=(pid) {
                (self.names(reviewers))
                @if since_yours > 0 {
                    " " span.dim { "·" } " "
                    span.since { (plural(since_yours, "push", "pushes")) " since your review" }
                }
                span.pop role="tooltip" id=(pid) { (self.popover(reviewers)) }
            }
        }
    }

    fn who(&self, r: &Reviewer) -> Markup {
        let name = if is_login(&r.login, self.me) {
            "you".to_owned()
        } else {
            format!("@{}", r.login)
        };
        html! {
            span.who.stale[!r.on_head()]
                title=[(!r.on_head()).then_some("before the latest push")] {
                (name) (mark(r.stance))
            }
        }
    }

    /// `N others`, then one mark for each verdict among them.
    fn others(others: &[&Reviewer]) -> Markup {
        let mut stances: Vec<Stance> = others.iter().map(|r| r.stance).collect();
        stances.sort();
        stances.dedup();
        let n = u32::try_from(others.len()).unwrap_or(u32::MAX);
        html! {
            span.others { (plural(n, "other", "others")) }
            " (" @for stance in stances { (mark(stance)) } ")"
        }
    }

    fn names(&self, reviewers: &[Reviewer]) -> Markup {
        let you = reviewers.iter().find(|r| is_login(&r.login, self.me));
        let others: Vec<&Reviewer> = reviewers
            .iter()
            .filter(|r| !is_login(&r.login, self.me))
            .collect();
        html! {
            @if reviewers.is_empty() {
                span.none { "no reviews" }
            } @else {
                span.lbl { "reviewed by " }
                @if let Some(you) = you {
                    (self.who(you))
                    @match others.as_slice() {
                        [] => {}
                        [other] => { " and " (self.who(other)) }
                        others => { " and " (Self::others(others)) }
                    }
                } @else {
                    @match others.as_slice() {
                        [] => {}
                        [one] => (self.who(one)),
                        [one, two] => { (self.who(one)) " and " (self.who(two)) }
                        [first, rest @ ..] => { (self.who(first)) " + " (Self::others(rest)) }
                    }
                }
            }
        }
    }

    fn popover(&self, reviewers: &[Reviewer]) -> Markup {
        let short: String = self.head.chars().take(7).collect();
        let seen = self.facts.and_then(|f| f.head_seen_at.as_deref());
        html! {
            div.ph {
                "head " b.mono { (short) }
                @if let Some(seen) = seen { ", first seen " (since(self.now, seen)) }
            }
            @if reviewers.is_empty() {
                p.dim.nobody { "Nobody has reviewed it yet." }
            } @else {
                table {
                    @for r in reviewers {
                        tr {
                            td {
                                b {
                                    @if is_login(&r.login, self.me) { "you" }
                                    @else { "@" (r.login) }
                                }
                            }
                            td { (mark(r.stance)) " " (r.stance.word()) }
                            td.when {
                                time datetime=(r.submitted_at) { (r.submitted_at) }
                                " · " (since(self.now, &r.submitted_at))
                            }
                            td {
                                @if r.on_head() {
                                    span.head { "on the latest push" }
                                } @else {
                                    span.before {
                                        "before the latest push · "
                                        (plural(r.pushes_since, "push", "pushes")) " since"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div.foot { "Each person's latest review; bots and dismissed reviews left out." }
        }
    }
}
