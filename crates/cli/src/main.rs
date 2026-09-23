//! The `sanic-review` binary.

use std::io::IsTerminal;

use clap::Parser;
use color_eyre::{config::HookBuilder, config::Theme, eyre::Result};
use sanic_review::Cli;
use tracing_error::ErrorLayer;
use tracing_subscriber::{EnvFilter, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    // Honour NO_COLOR, and keep escapes out of pipes and log files.
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let hook = HookBuilder::default();
    let hook = if color && std::io::stderr().is_terminal() {
        hook
    } else {
        hook.theme(Theme::new())
    };
    hook.install()?;
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().with_ansi(color && std::io::stdout().is_terminal()))
        .with(ErrorLayer::default())
        .init();

    Cli::parse().run().await
}
