//! The `sanic-review` command line.

mod archive;
mod chat;
mod config_doc;
mod config_edit;
pub mod logging;
pub mod poll;
pub mod schedule;
mod serve;
mod setup;
mod tui;
mod version;
mod watch;
mod work;

use std::{io::IsTerminal, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::{
    Section,
    eyre::{Result, eyre},
};

#[derive(Debug, Parser)]
#[command(version = version::VERSION.as_str(), about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Watch GitHub, run reviews, and serve the dashboard in the foreground.
    Serve(ServeArgs),
    /// Write or update the config: pick teams, local checkouts and orgs.
    Setup(setup::SetupArgs),
    /// Stop reviewing a PR automatically, and hide it in the TUI.
    Archive(archive::PrArgs),
    /// Undo `archive`.
    Unarchive(archive::PrArgs),
    /// Ask the running `serve` to review a PR now, e.g. one manual reviews
    /// hold.
    Review(archive::PrArgs),
    /// Chat with the agent that reviewed a PR, in a copy of its worktree
    /// that's thrown away when the chat ends.
    Chat(chat::ChatArgs),
}

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// How to report progress in the terminal: `tui` when stdout is a
    /// terminal `serve` has the foreground of, otherwise `logs`.
    #[arg(long, value_enum)]
    ui: Option<Ui>,

    /// Port for the dashboard, bound on 127.0.0.1.
    #[arg(long, default_value_t = 7117)]
    port: u16,

    /// Config file; defaults to ~/.config/sanic-review/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Where the database lives; defaults to ~/.local/share/sanic-review.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Turn `runner.manual_reviews` on in the config file: queue reviews
    /// but only run the ones you start. It's on unless the config turns it
    /// off; `m` in the TUI and the dashboard's settings switch it.
    #[arg(long)]
    manual_reviews: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Ui {
    /// Structured log lines plus a summary line on each state change.
    Logs,
    /// A terminal summary of tracked PRs, run activity and the log. Logs go
    /// to `serve.log` in the data directory as well.
    Tui,
}

impl Ui {
    /// The UI `serve` runs: the one asked for, else the TUI on a terminal
    /// and logs anywhere else, e.g. under systemd or piped to a file. The
    /// TUI draws on stdout and puts the terminal in raw mode, so asking for
    /// it without a terminal, or in the background (`serve &` in a shell
    /// with job control, where the terminal would stop it), fails rather
    /// than drawing into a pipe or stopping.
    fn resolve(explicit: Option<Self>, stdout: Stdout) -> Result<Self> {
        match (explicit, stdout) {
            (Some(Self::Tui), Stdout::NotTerminal) => {
                Err(eyre!("`--ui tui` needs a terminal, and stdout isn't one"))
                    .suggestion("drop `--ui` or pass `--ui logs`")
            }
            (Some(Self::Tui), Stdout::Background) => Err(eyre!(
                "`--ui tui` needs the terminal to itself, and `serve` is in the background"
            ))
            .suggestion("run it in the foreground, or pass `--ui logs`"),
            (Some(ui), _) => Ok(ui),
            (None, Stdout::Foreground) => Ok(Self::Tui),
            (None, Stdout::Background | Stdout::NotTerminal) => Ok(Self::Logs),
        }
    }
}

/// What `serve`'s stdout is, for picking its UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stdout {
    /// A terminal this process is in the foreground of.
    Foreground,
    /// A terminal another process group has the foreground of, as after
    /// `serve &`.
    Background,
    NotTerminal,
}

impl Stdout {
    fn detect() -> Self {
        let stdout = std::io::stdout();
        if !stdout.is_terminal() {
            return Self::NotTerminal;
        }
        #[cfg(unix)]
        if rustix::termios::tcgetpgrp(&stdout)
            .is_ok_and(|group| group != rustix::process::getpgrp())
        {
            return Self::Background;
        }
        Self::Foreground
    }
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Serve(args) => serve::run(args).await,
            Command::Setup(args) => {
                logging::init_stdout();
                setup::run(args).await
            }
            Command::Archive(args) => {
                logging::init_stdout();
                archive::archive(&args, true)
            }
            Command::Unarchive(args) => {
                logging::init_stdout();
                archive::archive(&args, false)
            }
            Command::Review(args) => {
                logging::init_stdout();
                archive::review(&args)
            }
            Command::Chat(args) => {
                logging::init_stdout();
                chat::run(args).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_picks_its_ui_from_the_terminal_unless_told() {
        let cli = Cli::try_parse_from(["sanic-review", "serve"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(args.ui, None);
        assert_eq!(Ui::resolve(None, Stdout::Foreground).unwrap(), Ui::Tui);
        assert_eq!(Ui::resolve(None, Stdout::Background).unwrap(), Ui::Logs);
        assert_eq!(Ui::resolve(None, Stdout::NotTerminal).unwrap(), Ui::Logs);
        for stdout in [Stdout::Foreground, Stdout::Background, Stdout::NotTerminal] {
            assert_eq!(Ui::resolve(Some(Ui::Logs), stdout).unwrap(), Ui::Logs);
        }
        assert_eq!(
            Ui::resolve(Some(Ui::Tui), Stdout::Foreground).unwrap(),
            Ui::Tui
        );
        let err = Ui::resolve(Some(Ui::Tui), Stdout::NotTerminal).unwrap_err();
        assert!(err.to_string().contains("needs a terminal"), "{err}");
        let err = Ui::resolve(Some(Ui::Tui), Stdout::Background).unwrap_err();
        assert!(err.to_string().contains("in the background"), "{err}");
    }
}
