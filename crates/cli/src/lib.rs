//! The `sanic-review` command line.

mod archive;
mod chat;
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

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre::Result;

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
    /// Ask the running `serve` to review a PR now, e.g. one held by
    /// `--manual-reviews`.
    Review(archive::PrArgs),
    /// Chat with the agent that reviewed a PR, in a copy of its worktree
    /// that's thrown away when the chat ends.
    Chat(chat::ChatArgs),
}

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// How to report progress in the terminal.
    #[arg(long, value_enum, default_value_t = Ui::Logs)]
    ui: Ui,

    /// Port for the dashboard, bound on 127.0.0.1.
    #[arg(long, default_value_t = 7117)]
    port: u16,

    /// Config file; defaults to ~/.config/sanic-review/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Where the database lives; defaults to ~/.local/share/sanic-review.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Queue reviews but only run the ones you start: `r` in the TUI, or
    /// `sanic-review review <PR url>`. The rest stay queued and run on the
    /// next start without this flag.
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
    fn serve_defaults_to_log_ui() {
        let cli = Cli::try_parse_from(["sanic-review", "serve"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(args.ui, Ui::Logs);
    }
}
