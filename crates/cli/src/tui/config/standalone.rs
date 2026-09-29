//! The config editor on its own, for `setup` and for a first `serve`
//! with no config: the whole terminal, before or without `serve`'s TUI,
//! and counts only with a GitHub token.

use std::{path::Path, sync::Arc, time::SystemTime};

use color_eyre::eyre::{Result, WrapErr, eyre};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use sanic_core::config::CheckoutResolver;
use sanic_github::Client;
use tokio::sync::watch;

use super::{Outcome, counts::Counter, field::guessed_keys, host::Host};
use crate::tui::{TICK, init_terminal};

/// Whether the editor wrote the file before it closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edited {
    Written,
    Unwritten,
}

/// When the editor closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    /// When you leave it.
    Left,
    /// Once a save is written, as when `serve` has no config to start
    /// with, or when you leave it.
    Written,
}

/// Runs the editor on the config at `path` until `until`. Without
/// `github`, nothing is counted and GitHub suggests nothing, and the editor
/// says why.
pub fn edit(
    path: &Path,
    resolver: Arc<dyn CheckoutResolver + Send + Sync>,
    github: Option<Arc<Client>>,
    runtime: tokio::runtime::Handle,
    until: Until,
) -> Result<Edited> {
    let path = path.to_owned();
    // Its own thread, named as the TUI's, so a panic restores the terminal.
    // The editor's made there, as it can't be sent to it.
    let thread = std::thread::Builder::new()
        .name("tui".into())
        .spawn(move || -> Result<Edited> {
            let (mut editor, host) =
                Host::open(&path, resolver, guessed_keys()).map_err(|why| eyre!(why))?;
            // Nothing pauses these counts: no poller shares the token.
            let (_pause, paused) = watch::channel(None);
            let counter = github.map(|github| Counter::start(&runtime, github, paused));
            if counter.is_none() {
                editor.set_offline(
                    "no GitHub token, so nothing's counted: `gh auth login`, then run this again",
                );
            }
            let mut terminal = init_terminal().wrap_err("starting the config editor")?;
            let result = (|| loop {
                host.tend(&mut editor, counter.as_ref(), SystemTime::now());
                if until == Until::Written && editor.wrote() {
                    return Ok(Edited::Written);
                }
                terminal
                    .draw(|frame| editor.render(frame))
                    .wrap_err("drawing the config editor")?;
                if !event::poll(TICK).wrap_err("reading terminal input")? {
                    continue;
                }
                let Event::Key(key) = event::read().wrap_err("reading terminal input")? else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match editor.handle_key(key) {
                    Outcome::Open => {}
                    Outcome::Close | Outcome::Quit => {
                        // Ctrl-C quits mid-save: let the save land, so
                        // what's said of the file is so and the process
                        // doesn't exit under the write.
                        while editor.is_saving() {
                            std::thread::sleep(TICK);
                            host.tend(&mut editor, None, SystemTime::now());
                        }
                        return Ok(if editor.wrote() {
                            Edited::Written
                        } else {
                            Edited::Unwritten
                        });
                    }
                    Outcome::Check { generation, text } => host.check(generation, text),
                    Outcome::Save { .. } => host.save(&mut editor),
                    Outcome::Find(find) => host.find(find),
                }
            })();
            ratatui::restore();
            result
        })
        .wrap_err("starting the config editor")?;
    thread
        .join()
        .map_err(|_| eyre!("the config editor stopped unexpectedly"))?
}
