//! The `sanic-review` command line.

mod archive;
pub mod logging;
pub mod poll;
pub mod schedule;
mod serve;
mod setup;
mod tui;
mod watch;
mod work;

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre::Result;

#[derive(Debug, Parser)]
#[command(version, about)]
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
    Archive(archive::ArchiveArgs),
    /// Undo `archive`.
    Unarchive(archive::ArchiveArgs),
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

    /// Watch and queue reviews, but don't run any. Queued reviews stay in
    /// the database and run on the next start without this flag.
    #[arg(long)]
    no_reviews: bool,
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
                archive::run(args, true)
            }
            Command::Unarchive(args) => {
                logging::init_stdout();
                archive::run(args, false)
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
