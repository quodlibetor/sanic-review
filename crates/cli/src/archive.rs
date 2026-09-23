//! `sanic-review archive` and `unarchive`: the TUI's `a` key, for when the
//! TUI isn't in use. They write the store directly, so they work while
//! `serve` runs.

use std::path::PathBuf;

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};
use sanic_core::{config::default_data_dir, pr::PrKey};
use sanic_store::Store;

#[derive(Debug, clap::Args)]
pub struct ArchiveArgs {
    /// The PR, as its github.com URL.
    url: String,

    /// Where the database lives; defaults to ~/.local/share/sanic-review.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

pub fn run(args: ArchiveArgs, archived: bool) -> Result<()> {
    let key = PrKey::parse_url(&args.url)?;
    let data_dir = match args.data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    let mut store = Store::open(&data_dir.join("state.db"))?;
    let set = store
        .set_archived(&key, archived)
        .wrap_err_with(|| format!("updating {}", key.url()))?;
    if !set {
        return Err(eyre!("{} isn't tracked", key.url())).suggestion(
            "`serve` tracks open PRs in watched repos that request your review or involve \
             you; archive it once it shows up",
        );
    }
    let done = if archived { "archived" } else { "unarchived" };
    println!("{done} {}", key.url());
    Ok(())
}
