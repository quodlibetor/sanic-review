//! The `sanic-review` binary.

use std::io::IsTerminal;

use clap::Parser;
use color_eyre::{config::HookBuilder, config::Theme, eyre::Result};
use sanic_review::{Cli, logging};

#[tokio::main]
async fn main() -> Result<()> {
    let hook = HookBuilder::default();
    let hook = if logging::color() && std::io::stderr().is_terminal() {
        hook
    } else {
        hook.theme(Theme::new())
    };
    hook.install()?;
    // Each command sets up tracing, since where logs go depends on its UI.
    Cli::parse().run().await
}
