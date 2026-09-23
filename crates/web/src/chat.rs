//! "Chat with the reviewer": the commands that resume the agent session of
//! a PR's latest review, to copy into a terminal. The dashboard runs
//! nothing; `sanic-review chat` does the work where you paste it.

use maud::{Markup, html};
use sanic_core::pr::PrKey;
use sanic_runner::{chat::quote, review::ReviewRunner};
use sanic_store::SessionRun;

use crate::App;

/// `sanic-review chat <args>` against `serve`'s own config and data, run
/// by the binary `serve` is, so the pasted command agrees with it.
fn cli(app: &App, args: &str) -> String {
    let program = std::env::current_exe().map_or_else(
        |_| "sanic-review".to_owned(),
        |exe| quote(&exe.display().to_string()),
    );
    format!(
        "{program} chat {args} --config {} --data-dir {}",
        quote(&app.config_path.display().to_string()),
        quote(&app.data_dir.display().to_string())
    )
}

/// The command `sanic-review chat` runs for `session`, or why it can't be
/// worked out now, which `sanic-review chat` would refuse for too.
fn claude_line(app: &App, session: &SessionRun) -> Result<String, String> {
    let run = &session.run;
    let settings = app
        .control
        .run_settings(&run.request.profile)
        .map_err(|err| format!("{err:#}"))?;
    Ok(ReviewRunner::new(&app.data_dir)
        .chat_command(run, &session.session_id, &settings, false)
        .shell_line())
}

/// The PR page's chat section, if a review of `key` has a session: the
/// run whose drafts the page shows, `shown`, if it has one, else the
/// latest that does. A failed read shows in the section, not instead of
/// the page.
pub fn section(app: &App, key: &PrKey, shown: Option<i64>) -> Option<Markup> {
    let found = {
        let store = app.store();
        match shown.map(|id| store.session_run(id)).transpose() {
            Ok(Some(Some(session))) => Ok(Some(session)),
            Ok(_) => store.latest_session_run(key),
            Err(err) => Err(err),
        }
    };
    let session = match found {
        Ok(session) => session?,
        Err(err) => {
            tracing::warn!(url = %key.url(), "finding a session to chat with failed: {err:?}");
            return Some(html! {
                section.chat #chat {
                    h2 { "Chat with the reviewer" }
                    p.error { "Couldn't look for a session to chat with: " (format!("{err:#}")) }
                }
            });
        }
    };
    let id = session.run.id;
    let claude = claude_line(app, &session);
    Some(html! {
        section.chat #chat {
            h2 {
                "Chat with the reviewer "
                span.dim { "run " (id) @if claude.is_ok() { " · paste into a terminal" } }
            }
            @match &claude {
                Err(why) => { p.error { "Can't chat with run " (id) ": " (why) } }
                Ok(line) => {
                    (copy_row("chat-command", &cli(app, &id.to_string())))
                    p.note {
                        "Checks the review's worktree out again and removes it when the chat "
                        "ends. Read-only, no GitHub token; add " code { "--allow-edits" }
                        " to let it edit that copy."
                    }
                    details {
                        summary { "Run claude yourself instead" }
                        ol {
                            li {
                                span { "Check out the worktree and print the claude line" }
                                (copy_row("print-command", &cli(app, &format!("--print-command {id}"))))
                            }
                            li {
                                span { "Run it" }
                                (copy_row("claude-command", line))
                            }
                            li {
                                span { "Remove the worktree afterwards" }
                                (copy_row("cleanup-command", &cli(app, &format!("--cleanup {id}"))))
                            }
                        }
                    }
                }
            }
        }
    })
}

/// A command on one line, scrolling sideways rather than wrapping, with a
/// button that copies it.
fn copy_row(id: &str, command: &str) -> Markup {
    html! {
        div.copyrow {
            pre #(id) { (command) }
            // Named by the command it copies, where there are several.
            button.btn type="button" data-copy={ "#" (id) } aria-describedby=(id) { "Copy" }
        }
    }
}
