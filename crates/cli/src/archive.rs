//! Commands on one PR, for when the TUI isn't in use: `archive` and
//! `unarchive` (the TUI's `x`) and `review` (its `r`). They write the store
//! directly, so they work while `serve` runs.

use std::path::PathBuf;

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};
use sanic_core::{config::default_data_dir, pr::PrKey};
use sanic_store::Store;

#[derive(Debug, clap::Args)]
pub struct PrArgs {
    /// The PR, as its github.com URL.
    url: String,

    /// Where the database lives; defaults to ~/.local/share/sanic-review.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

impl PrArgs {
    fn open(&self) -> Result<(PrKey, Store)> {
        let key = PrKey::parse_url(&self.url)?;
        let data_dir = match &self.data_dir {
            Some(dir) => dir.clone(),
            None => default_data_dir()?,
        };
        Ok((key, Store::open(&data_dir.join("state.db"))?))
    }
}

fn not_tracked(key: &PrKey) -> color_eyre::Report {
    eyre!("{} isn't tracked", key.url()).suggestion(
        "`serve` tracks open PRs in watched repos that request your review or involve \
         you; try again once it shows up",
    )
}

/// Asks the running `serve` to review the PR now: its held review under
/// `--manual-reviews`, or else a full review of its current head. `serve`
/// picks the request up within a few seconds, or when it next starts.
pub fn review(args: &PrArgs) -> Result<()> {
    let (key, store) = args.open()?;
    if store.pr_summary(&key)?.is_none() {
        return Err(not_tracked(&key));
    }
    store
        .request_start(&key)
        .wrap_err_with(|| format!("asking for a review of {}", key.url()))?;
    println!("asked serve to review {}", key.url());
    Ok(())
}

pub fn archive(args: &PrArgs, archived: bool) -> Result<()> {
    let (key, mut store) = args.open()?;
    let set = store
        .set_archived(&key, archived)
        .wrap_err_with(|| format!("updating {}", key.url()))?;
    if !set {
        return Err(not_tracked(&key));
    }
    let done = if archived { "archived" } else { "unarchived" };
    println!("{done} {}", key.url());
    Ok(())
}
