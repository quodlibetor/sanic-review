//! The page frame every dashboard page shares, and bits pages have in
//! common.

use axum::http::StatusCode;
use maud::{DOCTYPE, Markup, html};
use sanic_core::pr::PrKey;
use serde_json::json;

use crate::{
    App,
    guard::{TOKEN_FIELD, TOKEN_HEADER},
};

/// Which page it is, for the keyboard script.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Index,
    Pr,
    Confirm,
    Other,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Index => "index",
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
    ("a", "archive or unarchive the selected PR"),
    ("A", "show or hide archived PRs"),
    ("i", "skip PRs by title: use i in the TUI for now"),
    ("?, Esc", "show, close this help"),
];

pub fn layout(app: &App, kind: Kind, title: &str, content: &Markup) -> Markup {
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
                link rel="stylesheet" href="/assets/style.css";
                script src="/assets/htmx.min.js" defer {}
                script src="/assets/app.js" defer {}
            }
            body data-page=(kind.as_str()) hx-headers=(headers) {
                header.top {
                    a.home href="/" { "sanic-review" }
                    span.hint { "? keys" }
                }
                main { (content) }
                (help())
                div #notice hidden {}
            }
        }
    }
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

/// A standalone error page; it needs nothing from the app, so it works
/// whatever failed.
pub fn error(status: StatusCode, message: &str) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { (status.as_str()) " · sanic-review" }
                link rel="stylesheet" href="/assets/style.css";
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
                link rel="stylesheet" href="/assets/style.css";
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
