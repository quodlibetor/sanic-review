//! Editing the config file in place, keeping its comments and layout.
//! `setup` and the TUI's and the dashboard's ignore editors write through
//! here.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

/// Sets `[table].key`, creating the table as needed. A replaced value keeps
/// its surrounding whitespace and trailing comment.
pub fn set_value(doc: &mut DocumentMut, table: &str, key: &str, mut value: Value) -> Result<()> {
    let item = doc.entry(table).or_insert_with(|| Item::Table(new_table()));
    let Some(item) = item.as_table_like_mut() else {
        bail!("`{table}` in the config is not a table");
    };
    if let Some(old) = item.get_mut(key).and_then(Item::as_value_mut) {
        *value.decor_mut() = old.decor().clone();
        *old = value;
    } else {
        item.insert(key, Item::Value(value));
    }
    Ok(())
}

/// A table that prints after a blank line.
pub fn new_table() -> Table {
    let mut table = Table::new();
    table.decor_mut().set_prefix("\n");
    table
}

/// Writes via a temporary file and rename, so `serve` never reads a
/// half-written config. A symlinked config, say from a dotfiles repo,
/// stays a symlink: the file it points at is the one replaced, so its
/// directory must be writable. One that isn't, such as the Nix store, is
/// an error rather than a link silently swapped for a file.
pub fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let target = &resolve_links(path)?;
    let linked = if target == path {
        String::new()
    } else {
        format!(", which {} links to", path.display())
    };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)
            .wrap_err_with(|| format!("creating {}{linked}", dir.display()))?;
    }
    let tmp = target.with_extension("toml.tmp");
    std::fs::write(&tmp, text).wrap_err_with(|| {
        format!(
            "writing {} beside {}{linked}",
            tmp.display(),
            target.display()
        )
    })?;
    std::fs::rename(&tmp, target).wrap_err_with(|| format!("replacing {}", target.display()))
}

/// Hops a symlink can take before it's taken to be a loop, as Linux's
/// own limit.
const MAX_LINKS: usize = 40;

/// The file `path` names once symlinks are followed, whether or not that
/// file exists yet; `path` itself if it isn't a symlink.
pub fn resolve_links(path: &Path) -> Result<PathBuf> {
    let mut path = path.to_owned();
    for _ in 0..MAX_LINKS {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&path)
                    .wrap_err_with(|| format!("reading the symlink {}", path.display()))?;
                // A relative target is relative to the link's directory.
                path = match path.parent() {
                    Some(dir) => dir.join(target),
                    None => target,
                };
            }
            _ => return Ok(path),
        }
    }
    bail!("{} is a symlink loop", path.display())
}

/// Adds `pattern` to `skip_titles` in `[review_requests]`, or in
/// `[profile.<name>]` for `Some(name)`. `false` if it's already there.
pub fn add_skip_title(doc: &mut DocumentMut, profile: Option<&str>, pattern: &str) -> Result<bool> {
    let table = match profile {
        None => doc
            .entry("review_requests")
            .or_insert_with(|| Item::Table(new_table()))
            .as_table_like_mut(),
        Some(name) => match doc
            .get_mut("profile")
            .and_then(Item::as_table_like_mut)
            .and_then(|profiles| profiles.get_mut(name))
        {
            Some(profile) => profile.as_table_like_mut(),
            None => bail!("there's no `[profile.{name}]` in the config"),
        },
    };
    let Some(table) = table else {
        bail!(
            "`{}` in the config is not a table",
            profile.map_or_else(|| "review_requests".into(), |n| format!("profile.{n}"))
        );
    };
    let item = table
        .entry("skip_titles")
        .or_insert(Item::Value(Value::Array(Array::new())));
    let Some(titles) = item.as_array_mut() else {
        bail!("`skip_titles` in the config is not a list");
    };
    if titles.iter().any(|t| t.as_str() == Some(pattern)) {
        return Ok(false);
    }
    titles.push(pattern);
    Ok(true)
}

/// [`add_skip_title`] on the config file at `path`. The TUI and the
/// dashboard's handlers call this concurrently, so edits take turns: each
/// rereads the file, and none writes over another's temporary file.
pub fn add_skip_title_to_file(path: &Path, profile: Option<&str>, pattern: &str) -> Result<bool> {
    static EDITING: Mutex<()> = Mutex::new(());
    let _turn = EDITING.lock().unwrap_or_else(PoisonError::into_inner);
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading config {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .wrap_err_with(|| format!("parsing config {}", path.display()))?;
    if !add_skip_title(&mut doc, profile, pattern)? {
        return Ok(false);
    }
    write_atomically(path, &doc.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"# My config.
[review_requests]
teams = ["*"] # every team

[profile.default]
# Reviews for the org.
repos = [{ github = "org" }]
"#;

    #[test]
    fn skip_titles_are_added_once_keeping_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();

        assert!(add_skip_title_to_file(&path, None, "build(deps)*").unwrap());
        assert!(!add_skip_title_to_file(&path, None, "build(deps)*").unwrap());
        assert!(add_skip_title_to_file(&path, Some("default"), "wip*").unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"# My config.
[review_requests]
teams = ["*"] # every team
skip_titles = ["build(deps)*"]

[profile.default]
# Reviews for the org.
repos = [{ github = "org" }]
skip_titles = ["wip*"]
"#
        );
        let err = add_skip_title_to_file(&path, Some("nope"), "x").unwrap_err();
        assert!(err.to_string().contains("`[profile.nope]`"), "{err}");
    }

    #[test]
    fn concurrent_additions_all_land() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let patterns: Vec<String> = (0..8).map(|i| format!("p{i}*")).collect();
        std::thread::scope(|scope| {
            for pattern in &patterns {
                let path = &path;
                scope.spawn(move || add_skip_title_to_file(path, None, pattern).unwrap());
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        for pattern in &patterns {
            assert!(
                text.contains(&format!("\"{pattern}\"")),
                "{pattern} lost:\n{text}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_stays_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        let target = dotfiles.join("sanic.toml");
        std::fs::write(&target, CONFIG).unwrap();
        // Linked relatively, through a second link.
        let middle = config_dir.join("middle.toml");
        symlink("../dotfiles/sanic.toml", &middle).unwrap();
        let path = config_dir.join("config.toml");
        symlink(&middle, &path).unwrap();

        assert_eq!(
            resolve_links(&path).unwrap(),
            config_dir.join("../dotfiles/sanic.toml")
        );
        assert!(add_skip_title_to_file(&path, None, "wip*").unwrap());
        for link in [&path, &middle] {
            assert!(
                std::fs::symlink_metadata(link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains(r#"skip_titles = ["wip*"]"#), "{text}");
        assert!(text.starts_with("# My config."), "{text}");
        // No temporary file is left beside either.
        assert!(!config_dir.join("config.toml.tmp").exists());
        assert!(!dotfiles.join("sanic.toml.tmp").exists());

        // A dangling link gets its target created; a loop is an error.
        let dangling = config_dir.join("dangling.toml");
        symlink("../dotfiles/new.toml", &dangling).unwrap();
        write_atomically(&dangling, "x = 1\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("new.toml")).unwrap(),
            "x = 1\n"
        );
        let looped = config_dir.join("loop.toml");
        symlink(&looped, &looped).unwrap();
        assert!(write_atomically(&looped, "").is_err());
    }

    #[test]
    fn a_missing_review_requests_table_is_created() {
        let mut doc: DocumentMut = "[profile.a]\nrepos = []\n".parse().unwrap();
        add_skip_title(&mut doc, None, "x*").unwrap();
        assert_eq!(
            doc.to_string(),
            "[profile.a]\nrepos = []\n\n[review_requests]\nskip_titles = [\"x*\"]\n"
        );
    }
}
