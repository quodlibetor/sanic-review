//! `sanic-review setup`: the config editor on its own, to write or change
//! the config without `serve` running.

use std::{io::IsTerminal, path::PathBuf, sync::Arc};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};
use sanic_core::config::{Config, DEFAULT_API_URL, default_config_path};
use sanic_github::{Client, Token};
use sanic_runner::vcs::VcsResolver;

use crate::{
    config_doc::{ConfigDoc, Key, Scalar, Setting, Table},
    tui::standalone::{Edited, edit},
};

#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// Config file to edit; defaults to ~/.config/sanic-review/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,
}

pub async fn run(args: SetupArgs) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(eyre!("`setup` is an editor, and needs a terminal"))
            .suggestion("edit the config by hand; docs/DESIGN.md describes it");
    }
    let path = std::path::absolute(match args.config {
        Some(path) => path,
        None => default_config_path()?,
    })
    .wrap_err("resolving the config path")?;
    // Without a token the editor still edits; it just can't count.
    let github = match Token::discover() {
        Ok(token) => Some(Arc::new(Client::new(&api_url(&path), token)?)),
        Err(_) => None,
    };
    let runtime = tokio::runtime::Handle::current();
    let edited = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || edit(&path, Arc::new(VcsResolver), github, runtime))
            .await
            .wrap_err("running the config editor")??
    };
    match edited {
        Edited::Written => println!(
            "Wrote {}. A running `serve` picks it up automatically.",
            path.display()
        ),
        Edited::Unwritten => println!("{} is unchanged.", path.display()),
    }
    Ok(())
}

/// The config's `github.api_url`, read leniently, since the editor is
/// there to fix a config that doesn't load.
fn api_url(path: &std::path::Path) -> String {
    let set = Config::read_text(path)
        .ok()
        .flatten()
        .and_then(|text| ConfigDoc::parse(Some(&text)).ok())
        .zip(Key::new(Table::Github, "api_url"))
        .and_then(|(doc, key)| match doc.scalar(&key) {
            Setting::Set(Scalar::Text(url)) => Some(url),
            _ => None,
        });
    set.unwrap_or_else(|| DEFAULT_API_URL.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_url_is_read_even_from_a_config_that_doesnt_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(api_url(&path), DEFAULT_API_URL);
        std::fs::write(
            &path,
            "[github]\napi_url = \"http://proxy\"\n[poll]\nreconcile_secs = 0\n",
        )
        .unwrap();
        assert_eq!(api_url(&path), "http://proxy");
        std::fs::write(&path, "[github").unwrap();
        assert_eq!(api_url(&path), DEFAULT_API_URL);
    }
}
