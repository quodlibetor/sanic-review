//! The `sanic-review` binary.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre::{Result, bail};
use tracing_error::ErrorLayer;
use tracing_subscriber::{EnvFilter, prelude::*};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Watch GitHub, run reviews, and serve the dashboard in the foreground.
    Serve(ServeArgs),
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// How to report progress in the terminal.
    #[arg(long, value_enum, default_value_t = Ui::Logs)]
    ui: Ui,

    /// Port for the dashboard, bound on 127.0.0.1.
    #[arg(long, default_value_t = 7117)]
    port: u16,

    /// Config file; defaults to ~/.config/sanic-review/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Ui {
    /// Structured log lines plus a summary line on each state change.
    Logs,
    /// A terminal summary of items you haven't looked at.
    Tui,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .with(ErrorLayer::default())
        .init();

    match Cli::parse().command {
        Command::Serve(args) => serve(&args),
    }
}

fn serve(args: &ServeArgs) -> Result<()> {
    tracing::debug!(?args, "serve");
    bail!("`serve` is not implemented yet")
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
        let Command::Serve(args) = cli.command;
        assert_eq!(args.ui, Ui::Logs);
    }
}
