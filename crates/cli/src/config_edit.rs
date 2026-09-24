//! Editing the config file in place, keeping its comments and layout.
//! `setup` and the TUI's and the dashboard's ignore editors write through
//! here.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use toml_edit::{Array, DocumentMut, Item, RawString, Table, Value};

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

/// Removes the entries `drop` picks, keeping the comments around them: a
/// comment after an entry's comma is stored with the next entry, so it's
/// handed on to whichever entry follows a removed one, or to the end of the
/// list. Only a comment on the removed entry's own line goes with it.
pub fn remove_keeping_comments(list: &mut Array, mut drop: impl FnMut(&Value) -> bool) {
    let mut i = 0;
    while let Some(entry) = list.get(i) {
        if !drop(entry) {
            i += 1;
            continue;
        }
        let removed = list.remove(i);
        let before = raw(removed.decor().prefix());
        if let Some(next) = list.get_mut(i) {
            let after = raw(next.decor().prefix());
            // On the removed entry's line, `next` takes its place.
            let prefix = match after.find('\n') {
                Some(line_end) => format!("{}{}", up_to_last_line(before), &after[line_end..]),
                None => before.to_owned(),
            };
            next.decor_mut().set_prefix(prefix);
            continue;
        }
        // The last entry: what followed it is its suffix, and after any
        // trailing comma, the list's trailing space.
        let after = format!(
            "{}{}",
            raw(removed.decor().suffix()),
            raw(Some(list.trailing()))
        );
        let kept = up_to_last_line(before);
        let trailing = match after.find('\n') {
            Some(line_end) => format!("{kept}{}", &after[line_end..]),
            // The `]` was on the removed entry's line: the comment lines
            // before it stay, with the `]` on the line after them.
            None if !kept.trim().is_empty() => format!("{kept}\n{after}"),
            None => after,
        };
        if list.is_empty() && trailing.trim().is_empty() {
            // Emptied with nothing to keep: `[]`, so entries added later
            // don't start on the `[` line with the `]` on its own.
            list.set_trailing("");
            list.set_trailing_comma(false);
        } else {
            list.set_trailing(trailing);
        }
    }
}

/// Appends `value`, on a line of its own when the list's last entry is, or
/// when comment lines precede the `]`. What follows the last entry is
/// handed on, so the new comma goes straight after it (or after its
/// trailing comma).
pub fn push_on_own_line(list: &mut Array, mut value: Value) {
    let trailing_comma = list.trailing_comma();
    // What follows the last entry, or the `[` when there's none: without a
    // trailing comma, the entry's suffix; then the list's trailing space,
    // which a removal may have filled even without one.
    let mut tail = String::new();
    let mut indent = None;
    if let Some(final_entry) = list.iter_mut().last() {
        let before = raw(final_entry.decor().prefix());
        indent = before.rfind('\n').map(|end| before[end..].to_owned());
        if !trailing_comma {
            tail.push_str(raw(final_entry.decor().suffix()));
            final_entry.decor_mut().set_suffix("");
        }
    }
    tail.push_str(raw(Some(list.trailing())));
    // A comment after the last entry stays on its line; the space before
    // the `]` moves after the new entry.
    let (comment, close) = tail.split_at(tail.rfind('\n').unwrap_or(0));
    let prefix = match indent {
        Some(indent) => format!("{comment}{indent}"),
        None if comment.trim().is_empty() => if list.is_empty() { "" } else { " " }.into(),
        // After the comments, indented as the last one on a line of its own.
        None => format!("{comment}\n{}", last_line_indent(comment)),
    };
    value.decor_mut().set_prefix(prefix);
    list.set_trailing(close);
    list.push_formatted(value);
}

/// A decor's text, blank when unset.
fn raw(text: Option<&RawString>) -> &str {
    text.and_then(RawString::as_str).unwrap_or_default()
}

/// `text` up to its last line break: what precedes an entry on its own
/// line, without that line's indent.
fn up_to_last_line(text: &str) -> &str {
    text.rfind('\n').map_or("", |end| &text[..end])
}

/// The indent of `text`'s last line, when that's a line of its own.
fn last_line_indent(text: &str) -> &str {
    text.rfind('\n').map_or("", |end| {
        let line = &text[end + 1..];
        &line[..line.len() - line.trim_start().len()]
    })
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
    // Title globs ignore case, so one differing only in case is there too.
    let lowered = pattern.to_lowercase();
    if titles
        .iter()
        .any(|t| t.as_str().is_some_and(|t| t.to_lowercase() == lowered))
    {
        return Ok(false);
    }
    push_on_own_line(titles, pattern.into());
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
        assert!(!add_skip_title_to_file(&path, None, "Build(Deps)*").unwrap());
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
    fn removed_entries_hand_on_the_comment_before_them() {
        let edit = |text: &str, add: Option<&str>| {
            let mut doc: DocumentMut = text.parse().unwrap();
            let list = doc["l"].as_array_mut().unwrap();
            remove_keeping_comments(list, |v| v.as_str() == Some("x"));
            if let Some(add) = add {
                push_on_own_line(list, add.into());
            }
            doc.to_string()
        };
        assert_eq!(
            edit("l = [\n  \"a\", # on a\n  \"x\", # on x\n]\n", None),
            "l = [\n  \"a\", # on a\n]\n"
        );
        assert_eq!(
            edit("l = [\n  \"x\",\n  \"b\",\n]\n", Some("c")),
            "l = [\n  \"b\",\n  \"c\",\n]\n"
        );
        assert_eq!(
            edit("l = [\"x\", \"b\"]\n", Some("c")),
            "l = [\"b\", \"c\"]\n"
        );
        assert_eq!(edit("l = [\"x\"]\n", Some("c")), "l = [\"c\"]\n");
        // Without a trailing comma, the comment handed on to the `]` stays
        // beside the entry it was on, not the one added after it.
        assert_eq!(
            edit("l = [\n  \"a\", # on a\n  \"x\"\n]\n", Some("c")),
            "l = [\n  \"a\", # on a\n  \"c\"\n]\n"
        );
        assert_eq!(edit("l = [ \"a\", \"x\" ]\n", None), "l = [ \"a\" ]\n");
        // Comment lines before the next entry, or before the `]`, stay.
        assert_eq!(
            edit("l = [\n  \"x\",\n  # the good one\n  \"b\",\n]\n", None),
            "l = [\n  # the good one\n  \"b\",\n]\n"
        );
        assert_eq!(
            edit("l = [\n  \"a\",\n  \"x\",\n  # more to come\n]\n", None),
            "l = [\n  \"a\",\n  # more to come\n]\n"
        );
        // An emptied list is `[]`, so what's added isn't split around it.
        assert_eq!(edit("l = [\n  \"x\",\n]\n", None), "l = []\n");
        assert_eq!(edit("l = [\n  \"x\",\n]\n", Some("c")), "l = [\"c\"]\n");
        // One emptied but for comments gets what's added after them.
        assert_eq!(
            edit("l = [\n  \"x\",\n  # more to come\n]\n", Some("c")),
            "l = [\n  # more to come\n  \"c\",\n]\n"
        );
        // With the `]` on the removed entry's line, the comment before it
        // stays, on the line of the entry it follows.
        assert_eq!(
            edit("l = [\"a\", # on a\n  \"x\"]\n", None),
            "l = [\"a\" # on a\n]\n"
        );
    }

    #[test]
    fn entries_are_appended_after_a_last_entry_without_a_comma() {
        let push = |text: &str| {
            let mut doc: DocumentMut = text.parse().unwrap();
            push_on_own_line(doc["l"].as_array_mut().unwrap(), "c".into());
            doc.to_string()
        };
        assert_eq!(
            push("l = [\n  \"a\",\n  \"b\"\n]\n"),
            "l = [\n  \"a\",\n  \"b\",\n  \"c\"\n]\n"
        );
        assert_eq!(
            push("l = [\n  \"a\",\n  \"b\"  # last\n]\n"),
            "l = [\n  \"a\",\n  \"b\",  # last\n  \"c\"\n]\n"
        );
        assert_eq!(
            push("l = [ \"a\", \"b\" ]\n"),
            "l = [ \"a\", \"b\", \"c\" ]\n"
        );
        assert_eq!(push("l = []\n"), "l = [\"c\"]\n");
        // Comment lines before the `]` stay before what's added.
        assert_eq!(
            push("l = [\n  # \"wip*\",\n]\n"),
            "l = [\n  # \"wip*\",\n  \"c\"\n]\n"
        );
        assert_eq!(
            push("l = [\"a\",\n  # later\n]\n"),
            "l = [\"a\",\n  # later\n  \"c\",\n]\n"
        );
    }

    #[test]
    fn skip_titles_go_on_their_own_line_after_a_comment() {
        let mut doc: DocumentMut = "[review_requests]\nskip_titles = [\n  \"wip*\", # mine\n]\n"
            .parse()
            .unwrap();
        assert!(add_skip_title(&mut doc, None, "build(deps)*").unwrap());
        assert_eq!(
            doc.to_string(),
            "[review_requests]\nskip_titles = [\n  \"wip*\", # mine\n  \"build(deps)*\",\n]\n"
        );
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
