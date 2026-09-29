//! Editing the config file in place, keeping its comments and layout.
//! The config editor (through [`crate::config_doc`]), the TUI's and the
//! dashboard's ignore editors, their manual reviews switches and `serve
//! --manual-reviews` write through here.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use sanic_core::{
    config::{CheckoutResolver, Config, Unloadable},
    manual::ManualReviews,
};
use toml_edit::{Array, DocumentMut, Item, RawString, Table, TableLike, Value};

/// Sets `[table].key`, creating the table as needed. A replaced value keeps
/// its surrounding whitespace and trailing comment.
pub fn set_value(doc: &mut DocumentMut, table: &str, key: &str, value: Value) -> Result<()> {
    let item = doc.entry(table).or_insert_with(|| Item::Table(new_table()));
    let Some(item) = item.as_table_like_mut() else {
        bail!("`{table}` in the config is not a table");
    };
    set_in(item, key, value);
    Ok(())
}

/// Sets `key` in `table`. A replaced value keeps its surrounding
/// whitespace and trailing comment.
pub fn set_in(table: &mut dyn TableLike, key: &str, mut value: Value) {
    if let Some(old) = table.get_mut(key).and_then(Item::as_value_mut) {
        *value.decor_mut() = old.decor().clone();
        *old = value;
    } else {
        table.insert(key, Item::Value(value));
    }
}

/// Removes `key` from the table `item`, keeping the comment lines above
/// it: they go before the next key, or after the key before it when it was
/// the last, or after the table's header when it was the only one. A
/// comment on its own line goes with it. `false` if it wasn't there.
pub fn unset_keeping_comments(item: &mut Item, key: &str) -> bool {
    let Some(table) = item.as_table_like_mut() else {
        return false;
    };
    let values: Vec<String> = table
        .iter()
        .filter(|(_, item)| item.is_value())
        .map(|(k, _)| k.to_owned())
        .collect();
    let Some(at) = values.iter().position(|k| k == key) else {
        return table.remove(key).is_some();
    };
    let above = table
        .key(key)
        .map(|k| up_to_last_line(raw(k.leaf_decor().prefix())).to_owned())
        .unwrap_or_default();
    table.remove(key);
    if !above.contains('#') {
        return true;
    }
    if let Some(mut next) = values.get(at + 1).and_then(|k| table.key_mut(k)) {
        let prefix = format!("{above}\n{}", raw(next.leaf_decor().prefix()));
        next.leaf_decor_mut().set_prefix(prefix);
        return true;
    }
    if let Some(prev) = at
        .checked_sub(1)
        .and_then(|i| table.get_mut(&values[i]))
        .and_then(Item::as_value_mut)
    {
        // The line break after the value ends the comment's line.
        let suffix = format!("{}\n{above}", raw(prev.decor().suffix()));
        prev.decor_mut().set_suffix(suffix);
    } else if let Some(header) = item.as_table_mut() {
        // Likewise the one after the header.
        let suffix = format!("{}\n{above}", raw(header.decor().suffix()));
        header.decor_mut().set_suffix(suffix);
    }
    true
}

/// Moves `list[from]` to `to`, which counts the list without it, with its
/// comments: the lines above it and the one after its comma, which is
/// stored with the entry after it. Each place keeps its indent and what
/// follows it.
pub fn move_keeping_comments(list: &mut Array, from: usize, to: usize) {
    let to = to.min(list.len().saturating_sub(1));
    if from >= list.len() || from == to {
        return;
    }
    // An entry's prefix is: the comment after the previous entry's comma
    // (or on the `[` line), the entry's own comment lines, then its indent.
    let parts: Vec<(String, String, String)> = list
        .iter()
        .map(|v| {
            let prefix = raw(v.decor().prefix());
            match (prefix.find('\n'), prefix.rfind('\n')) {
                (Some(start), Some(end)) => (
                    prefix[..start].to_owned(),
                    prefix[start..end].to_owned(),
                    prefix[end..].to_owned(),
                ),
                _ => (String::new(), String::new(), prefix.to_owned()),
            }
        })
        .collect();
    // The last entry's line comment follows the trailing comma, in the
    // list's trailing space; without one, it's in the entry's suffix.
    let comma = list.trailing_comma();
    let final_place = list.len() - 1;
    let mut suffixes: Vec<String> = list
        .iter()
        .map(|v| raw(v.decor().suffix()).to_owned())
        .collect();
    let mut closing = raw(Some(list.trailing())).to_owned();
    let tail = if comma {
        &mut closing
    } else {
        &mut suffixes[final_place]
    };
    let last_comment = tail
        .drain(..tail.find('\n').unwrap_or(0))
        .collect::<String>();
    // The comment on each entry's line, by entry.
    let line_comments: Vec<String> = parts
        .iter()
        .skip(1)
        .map(|(after, _, _)| after.clone())
        .chain([last_comment])
        .collect();

    let mut order: Vec<usize> = (0..list.len()).collect();
    let moved = order.remove(from);
    order.insert(to, moved);
    let value = list.remove(from);
    list.insert_formatted(to, value);
    for (place, entry) in list.iter_mut().enumerate() {
        let before = match place.checked_sub(1) {
            None => &parts[0].0,
            Some(prev) => &line_comments[order[prev]],
        };
        let prefix = format!("{before}{}", parts[order[place]].1);
        entry
            .decor_mut()
            .set_prefix(end_comment_line(prefix, &parts[place].2));
        let suffix = if place == final_place && !comma {
            end_comment_line(
                line_comments[order[final_place]].clone(),
                &suffixes[final_place],
            )
        } else {
            suffixes[place].clone()
        };
        entry.decor_mut().set_suffix(suffix);
    }
    if comma {
        let after_final = line_comments[order[final_place]].clone();
        list.set_trailing(end_comment_line(after_final, &closing));
    }
}

/// `text` then `rest`, with a line break between them when `text` ends in
/// a comment and `rest` doesn't start a line, so the comment can't swallow
/// what follows.
fn end_comment_line(mut text: String, rest: &str) -> String {
    let open = text
        .rsplit('\n')
        .next()
        .is_some_and(|line| line.contains('#'));
    if open && !rest.starts_with('\n') {
        text.push('\n');
    }
    text.push_str(rest);
    text
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

/// [`add_skip_title`] on the config file at `path`.
pub fn add_skip_title_to_file(path: &Path, profile: Option<&str>, pattern: &str) -> Result<bool> {
    edit_file(path, |doc| add_skip_title(doc, profile, pattern))
}

/// Sets `runner.manual_reviews` to `on`, in a config that loads, so the
/// key is a bool if it's there at all. `false` if the file already says
/// so; left unset, it's on, but turning it on still writes it.
fn set_manual_reviews(doc: &mut DocumentMut, on: bool) -> Result<bool> {
    let now = doc
        .get("runner")
        .and_then(|runner| runner.get("manual_reviews"))
        .and_then(Item::as_bool);
    if now == Some(on) {
        return Ok(false);
    }
    set_value(doc, "runner", "manual_reviews", on.into())?;
    Ok(true)
}

/// What manual reviews hold by the config file at `path`, as `serve`
/// would load it now. A value that isn't a bool, or anything else that
/// keeps the file from loading, is an error, not the default.
pub fn manual_reviews_in_file(
    path: &Path,
    resolver: &dyn CheckoutResolver,
) -> Result<ManualReviews, Unloadable> {
    Config::load(path, resolver)
        .map(|config| config.manual_reviews())
        .map_err(Unloadable)
}

/// [`set_manual_reviews`] on the config file at `path`, if it loads as
/// `serve` would load it. If it doesn't, `serve` is still on the config it
/// last loaded, so the file is left alone and the error says why.
pub fn set_manual_reviews_in_file(
    path: &Path,
    on: bool,
    resolver: &dyn CheckoutResolver,
) -> Result<Result<bool, Unloadable>> {
    let _turn = take_turn();
    let text = match Config::load_with_text(path, resolver) {
        Ok((_, text)) => text,
        Err(err) => return Ok(Err(Unloadable(err))),
    };
    edit_text(path, &text, |doc| set_manual_reviews(doc, on)).map(Ok)
}

/// Applies `edit` to the config file at `path`, writing it back if `edit`
/// says it changed anything.
fn edit_file(path: &Path, edit: impl FnOnce(&mut DocumentMut) -> Result<bool>) -> Result<bool> {
    let _turn = take_turn();
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading config {}", path.display()))?;
    edit_text(path, &text, edit)
}

/// [`edit_file`] on `text`, which was read from `path` during this turn.
fn edit_text(
    path: &Path,
    text: &str,
    edit: impl FnOnce(&mut DocumentMut) -> Result<bool>,
) -> Result<bool> {
    let mut doc: DocumentMut = text
        .parse()
        .wrap_err_with(|| format!("parsing config {}", path.display()))?;
    if !edit(&mut doc)? {
        return Ok(false);
    }
    write_atomically(path, &doc.to_string())?;
    Ok(true)
}

/// The TUI and the dashboard's handlers edit the config file concurrently,
/// so edits take turns: each reads the file during its turn, and none
/// writes over another's temporary file.
pub fn take_turn() -> MutexGuard<'static, ()> {
    static EDITING: Mutex<()> = Mutex::new(());
    EDITING.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::tests::NoCheckouts;

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
    fn moved_entries_take_their_comments_and_leave_the_layout() {
        let moved = |text: &str, from, to| {
            let mut doc: DocumentMut = text.parse().unwrap();
            move_keeping_comments(doc["l"].as_array_mut().unwrap(), from, to);
            doc.to_string()
        };
        assert_eq!(
            moved("l = [\"a\", \"b\", \"c\"]\n", 2, 0),
            "l = [\"c\", \"a\", \"b\"]\n"
        );
        assert_eq!(
            moved(
                "l = [\n  # about a\n  \"a\", # on a\n  \"b\",\n  \"c\", # on c\n]\n",
                0,
                2
            ),
            "l = [\n  \"b\",\n  \"c\", # on c\n  # about a\n  \"a\", # on a\n]\n"
        );
        assert_eq!(
            moved("l = [ # the list\n  \"a\",\n  \"b\", # on b\n]\n", 1, 0),
            "l = [ # the list\n  \"b\", # on b\n  \"a\",\n]\n"
        );
        // A comment never ends up before what followed it on its line.
        for (text, from, to, want) in [
            (
                "l = [\"a\", # on a\n  \"b\"]\n",
                0,
                1,
                "l = [\"b\",\n  \"a\" # on a\n]\n",
            ),
            (
                "l = [\"a\", # on a\n \"b\", \"c\"]\n",
                2,
                0,
                "l = [\"c\",\n \"a\", # on a\n \"b\"]\n",
            ),
            (
                "l = [\n  \"a\", # on a\n  \"b\" # on b\n]\n",
                1,
                0,
                "l = [\n  \"b\", # on b\n  \"a\" # on a\n]\n",
            ),
        ] {
            let out = moved(text, from, to);
            assert_eq!(out, want);
            out.parse::<DocumentMut>().unwrap();
        }
        // Out of range, or to where it is, it stays.
        assert_eq!(moved("l = [\"a\", \"b\"]\n", 1, 9), "l = [\"a\", \"b\"]\n");
        assert_eq!(moved("l = [\"a\", \"b\"]\n", 5, 0), "l = [\"a\", \"b\"]\n");
    }

    #[test]
    fn unset_keys_leave_the_comments_above_them() {
        let unset = |text: &str, key: &str| {
            let mut doc: DocumentMut = text.parse().unwrap();
            assert!(unset_keeping_comments(&mut doc["t"], key));
            doc.to_string()
        };
        assert_eq!(
            unset("[t]\n# about a\na = 1 # on a\nb = 2\n", "a"),
            "[t]\n# about a\nb = 2\n"
        );
        assert_eq!(
            unset("[t]\na = 1\n\n# about b\nb = 2 # on b\n", "b"),
            "[t]\na = 1\n\n# about b\n"
        );
        assert_eq!(unset("[t]\na = 1\nb = 2\n", "a"), "[t]\nb = 2\n");
        assert_eq!(unset("[t]\n# about a\na = 1\n", "a"), "[t]\n# about a\n");
        let mut doc: DocumentMut = "[t]\na = 1\n".parse().unwrap();
        assert!(!unset_keeping_comments(&mut doc["t"], "z"));
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
    fn manual_reviews_are_switched_in_runner_keeping_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();

        // Unset is on, but turning it on says so in the file.
        let switch = |on| set_manual_reviews_in_file(&path, on, &NoCheckouts).unwrap();
        assert!(switch(true).unwrap());
        assert!(!switch(true).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.ends_with("\n[runner]\nmanual_reviews = true\n"),
            "{text}"
        );

        let mut doc: DocumentMut = "[runner]\nmanual_reviews = true # hold them\nclaude = \"c\"\n"
            .parse()
            .unwrap();
        assert!(set_manual_reviews(&mut doc, false).unwrap());
        assert_eq!(
            doc.to_string(),
            "[runner]\nmanual_reviews = false # hold them\nclaude = \"c\"\n"
        );
    }

    #[test]
    fn manual_reviews_are_left_alone_in_a_config_that_doesnt_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        for (broken, why) in [
            // Read raw, "no" isn't a bool, so it would look unset: on.
            ("[runner]\nmanual_reviews = \"no\"\n", "manual_reviews"),
            ("[poll]\nreconcile_secs = 0\n", "reconcile_secs"),
        ] {
            let text = format!("{broken}{CONFIG}");
            std::fs::write(&path, &text).unwrap();
            let err = manual_reviews_in_file(&path, &NoCheckouts).unwrap_err();
            assert!(err.to_string().contains(why), "{err}");
            for on in [true, false] {
                let err = set_manual_reviews_in_file(&path, on, &NoCheckouts)
                    .unwrap()
                    .unwrap_err();
                assert!(err.to_string().contains(why), "{err}");
            }
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
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
