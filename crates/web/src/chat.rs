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

/// The PR page's chat section, if a review of `key` has a session. A
/// failed read shows in the section, not instead of the page.
pub fn section(app: &App, key: &PrKey) -> Option<Markup> {
    let session = match app.store().latest_session_run(key) {
        Ok(session) => session?,
        Err(err) => {
            tracing::warn!(url = %key.url(), "finding a session to chat with failed: {err:?}");
            return Some(html! {
                section #chat {
                    h2 { "Chat with the reviewer" }
                    p.error { "Couldn't look for a session to chat with: " (format!("{err:#}")) }
                }
            });
        }
    };
    let id = session.run.id;
    let chat = cli(app, &id.to_string());
    let claude = claude_line(app, &session);
    Some(html! {
        section #chat {
            h2 { "Chat with the reviewer" }
            @match &claude {
                Err(why) => { p.error { "Can't chat with run " (id) ": " (why) } }
                Ok(line) => {
                    p {
                        "Ask the agent that did run " (id) " about its review. Paste this into a "
                        "terminal:"
                    }
                    div.copyable {
                        pre #chat-command { code { (chat) } }
                        button type="button" data-copy="#chat-command" { "Copy" }
                    }
                    p.dim {
                        "It checks the review's worktree out again, at the path the review used, "
                        "and removes it when the chat ends. The agent keeps the review's limits: "
                        "read-only tools and no GitHub token. Add " code { "--allow-edits" }
                        " to let it edit that copy of the worktree."
                    }
                    details {
                        summary { "Or run claude yourself" }
                        p {
                            "This needs the worktree, which "
                            code { (cli(app, &format!("--print-command {id}"))) }
                            " checks out and prints this line for. Remove the worktree "
                            "afterwards with " code { (cli(app, &format!("--cleanup {id}"))) } "."
                        }
                        div.copyable {
                            pre #claude-command { code { (line) } }
                            button type="button" data-copy="#claude-command" { "Copy" }
                        }
                    }
                }
            }
        }
    })
}
