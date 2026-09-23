//! Editing the config file in place, keeping its comments and layout.
//! `setup` and the TUI's ignore editor both write through here.

use std::path::Path;

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
/// half-written config.
pub fn write_atomically(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).wrap_err_with(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).wrap_err_with(|| format!("replacing {}", path.display()))
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

/// [`add_skip_title`] on the config file at `path`.
pub fn add_skip_title_to_file(path: &Path, profile: Option<&str>, pattern: &str) -> Result<bool> {
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
    fn a_missing_review_requests_table_is_created() {
        let mut doc: DocumentMut = "[profile.a]\nrepos = []\n".parse().unwrap();
        add_skip_title(&mut doc, None, "x*").unwrap();
        assert_eq!(
            doc.to_string(),
            "[profile.a]\nrepos = []\n\n[review_requests]\nskip_titles = [\"x*\"]\n"
        );
    }
}
