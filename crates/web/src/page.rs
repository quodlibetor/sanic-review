//! The page frame every dashboard page shares, and bits pages have in
//! common.

use axum::http::StatusCode;
use maud::{DOCTYPE, Markup, html};
use sanic_core::{
    pr::PrKey,
    state::{PrState, Urgency},
};
use serde_json::json;

use crate::{
    App,
    guard::{TOKEN_FIELD, TOKEN_HEADER},
};

/// Which page it is, for the keyboard script and the top bar's counts.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// The index, and whether it shows archived PRs: only then does the top
    /// bar count their pending drafts.
    Index {
        archived: bool,
    },
    Pr,
    Confirm,
    Other,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Index { .. } => "index",
            Self::Pr => "pr",
            Self::Confirm => "confirm",
            Self::Other => "other",
        }
    }
}

/// Keys and what they do, for the help overlay. The same as the TUI's where
/// the TUI has the key.
const KEYS: &[(&str, &str)] = &[
    ("q", "close this help; otherwise back to the index"),
    ("Tab, Shift-Tab", "next, previous list"),
    ("j/k, Down/Up", "move in the list"),
    ("g/G, Home/End", "first, last row"),
    ("Enter", "open the selected PR"),
    ("r", "review now: failed, held or skipped (asks first)"),
    ("x", "archive or unarchive the selected PR"),
    ("X", "show or hide archived PRs"),
    ("i", "skip PRs with titles like the selected one"),
    ("v", "who reviewed the selected PR, and when; Esc closes"),
    ("d", "the selected PR's review run and drafts; Esc closes"),
    (
        "c",
        "chat with the agent that reviewed it: shows the command",
    ),
    (
        "f",
        "on a PR page: the drafts, or the files changed with the drafts in them",
    ),
    ("s", "in the files changed: one column, or old beside new"),
    (
        "a",
        "on a PR page: revise the selected draft with the agent, from a note",
    ),
    ("?, Esc", "show, close this help"),
];

pub fn layout(app: &App, kind: Kind, title: &str, content: &Markup) -> Markup {
    layout_in(app, kind, title, &[], content)
}

/// [`layout`], with where the page is (e.g. the PR it's about) in the top
/// bar after the home link.
pub fn layout_in(
    app: &App,
    kind: Kind,
    title: &str,
    crumbs: &[Markup],
    content: &Markup,
) -> Markup {
    let headers = json!({ TOKEN_HEADER: app.csrf.token() }).to_string();
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                // htmx's indicator styles would need an inline style the CSP
                // forbids; eval stays off since nothing needs it.
                meta name="htmx-config"
                    content=r#"{"includeIndicatorStyles":false,"allowEval":false,"allowScriptTags":false}"#;
                title { (title) " · sanic-review" }
                (icon_and_style())
                // The files view's highlighting.
                @if matches!(kind, Kind::Pr) { link rel="stylesheet" href="/assets/syntax.css"; }
                script src="/assets/htmx.min.js" defer {}
                script src="/assets/app.js" defer {}
            }
            body data-page=(kind.as_str()) hx-headers=(headers) {
                header.topbar {
                    a.home href="/" { "sanic-review" }
                    @for crumb in crumbs { span.crumb { "/ " (crumb) } }
                    (counts(app, matches!(kind, Kind::Index { archived: true })))
                }
                main { (content) }
                (help())
                // Where a confirm card opens over the page; see `card`.
                div #dialog .overlay hidden { div.popup.dialog role="dialog" aria-modal="true" {} }
                div #notice hidden {}
            }
        }
    }
}

/// The icon and stylesheet every page's `head` links.
fn icon_and_style() -> Markup {
    html! {
        link rel="icon" type="image/svg+xml" href="/assets/favicon.svg";
        link rel="stylesheet" href="/assets/style.css";
    }
}

/// The run and draft counts the TUI's status line has, and the keys hint.
/// The pending drafts are those of the PRs the index lists, archived ones
/// only `with_archived`, as the index shows them. The index refreshes it
/// with its lists. Counts the store can't read are left out, and logged,
/// rather than failing the page they head.
pub fn counts(app: &App, with_archived: bool) -> Markup {
    let since = crate::index::since(app);
    let counts = {
        let store = app.store();
        store.run_counts().and_then(|runs| {
            let pending = store.listed_pending_drafts(&app.me, since.as_deref(), with_archived)?;
            Ok((runs, pending))
        })
    }
    .inspect_err(|err| tracing::warn!("reading the run counts failed: {err:?}"))
    .ok();
    html! {
        span.counts #counts {
            @if app.manual_reviews { span.held { "manual reviews" } " · " }
            @if let Some((c, pending)) = counts {
                b { (c.queued) } " queued · " b { (c.running) } " running · "
                b.cnt { (pending) } " pending drafts"
                " " span.muted-sep { "|" } " "
            }
            (keycap("?")) " keys"
        }
    }
}

/// A key, as buttons and hints show it.
pub fn keycap(k: &str) -> Markup {
    html! { span.k { (k) } }
}

fn help() -> Markup {
    html! {
        div #help .overlay hidden {
            div.popup {
                h2 { "Keys" }
                table {
                    @for (keys, what) in KEYS {
                        tr { th { kbd { (keys) } } td { (what) } }
                    }
                }
                p.dim {
                    "On a PR page j/k move between drafts, and r and a act on the PR. "
                    "On a confirm page y confirms and Esc or q cancels."
                }
            }
        }
    }
}

/// The hidden field plain forms carry the CSRF token in.
pub fn csrf_field(app: &App) -> Markup {
    html! { input type="hidden" name=(TOKEN_FIELD) value=(app.csrf.token()); }
}

/// A PR's github.com URL, as a link.
pub fn github_link(key: &PrKey) -> Markup {
    let url = key.url();
    html! { a.gh href=(url) { (url) } }
}

/// A PR as `owner/name#N ↗`, linking to it on github.com; the whole URL is
/// on hover.
pub fn pr_ref(key: &PrKey) -> Markup {
    let url = key.url();
    html! { a.ref href=(url) title=(url) { (key) " ↗" } }
}

/// A standalone error page; it needs nothing from the app, so it works
/// whatever failed.
pub fn error(status: StatusCode, message: &str) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { (status.as_str()) " · sanic-review" }
                (icon_and_style())
            }
            body {
                header.top { a.home href="/" { "sanic-review" } }
                main {
                    h1 { (status.canonical_reason().unwrap_or("Error")) }
                    pre.error { (message) }
                    p { a href="/" { "Back to the index" } }
                }
            }
        }
    }
}

/// The first line of `text`, for errors shown in a list.
pub fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// `m:ss`, or `h:mm:ss` from an hour up, as the TUI shows countdowns.
pub fn countdown(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A pending draft count; blank when there are none.
pub fn drafts(n: u32) -> String {
    match n {
        0 => String::new(),
        1 => "1 draft".into(),
        n => format!("{n} drafts"),
    }
}

/// What a page another site sent you to shows instead: a link to it, with
/// no keys and no forms.
pub fn elsewhere(target: &str) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "Continue · sanic-review" }
                (icon_and_style())
            }
            body {
                header.top { a.home href="/" { "sanic-review" } }
                main {
                    h1 { "Another site sent you here" }
                    p { "So nothing on this page acts until you open it yourself." }
                    p { a href=(target) { "Continue to " code { (target) } } }
                }
            }
        }
    }
}

/// Where a PR stands, in full, styled by how much it asks of you: as the
/// TUI's state column, which has less room.
pub fn state_cell(state: PrState) -> Markup {
    let status = state.status();
    html! { span.state.(urgency_class(state)) { (status) } }
}

fn urgency_class(state: PrState) -> &'static str {
    match state.urgency() {
        Urgency::Act => "act",
        Urgency::Good => "good",
        Urgency::Quiet => "quiet",
    }
}

/// How a [`card`] looks: asking, or saying how something went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ask,
    Done,
    Failed,
}

/// A confirm or result card: what kind of thing it is, the question (or
/// outcome), the PR it's about, anything more, what it costs, then its
/// buttons: `go` and a way back. The keyboard script also opens a confirm
/// page's card as a dialog over the page you asked from.
pub struct Card<'a> {
    pub kind: &'a str,
    pub heading: Markup,
    pub title: &'a str,
    pub meta: Markup,
    pub extra: Markup,
    pub cost: Option<Markup>,
    pub go: Markup,
    pub back: (&'a str, &'a str),
    pub tone: Tone,
}

pub fn card(card: &Card<'_>) -> Markup {
    let (back, back_label) = card.back;
    html! {
        div.cf.ok[card.tone == Tone::Done].bad[card.tone == Tone::Failed] {
            p.kind { (card.kind) }
            h1 { (card.heading) }
            div.who {
                div.t { (card.title) }
                div.m { (card.meta) }
                (card.extra)
            }
            @if let Some(cost) = &card.cost { p.cost { (cost) } }
            div.btns {
                (card.go)
                a.btn #cancel href=(back) { (back_label) (keycap("Esc")) }
            }
        }
    }
}
