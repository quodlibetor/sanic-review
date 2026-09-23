//! The `sanic-review` command line.

pub mod poll;
pub mod schedule;
mod serve;
mod setup;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Ui {
    /// Structured log lines plus a summary line on each state change.
    Logs,
    /// A terminal summary of items you haven't looked at.
    Tui,
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Serve(args) => serve::run(args).await,
            Command::Setup(args) => setup::run(args).await,
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
