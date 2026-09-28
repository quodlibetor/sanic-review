//! Adding, removing, renaming and reordering `[profile.<name>]` tables,
//! however the file writes them: header tables, dotted keys or an inline
//! table. Their order matters, since the first of equally specific matches
//! wins, so a rename keeps a profile in its place.

use color_eyre::eyre::{Result, bail, eyre};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Key, RawString, Table, TableLike, Value};

use crate::config_edit::new_table;

/// `[profile.<name>]`, which has to exist.
pub fn get_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut dyn TableLike> {
    doc.get_mut("profile")
        .and_then(Item::as_table_like_mut)
        .and_then(|profiles| profiles.get_mut(name))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| eyre!("there's no `[profile.{name}]` in the config"))
}

/// Adds `[profile.<name>]` with an empty `repos`, after the others.
pub fn add(doc: &mut DocumentMut, name: &str) -> Result<()> {
    let after = last_position(doc.as_table()) + 1;
    let profiles = doc.entry("profile").or_insert_with(|| {
        let mut table = Table::new();
        // Only `[profile.<name>]` headers, no bare `[profile]`.
        table.set_implicit(true);
        Item::Table(table)
    });
    if profiles
        .as_table_like()
        .is_some_and(|p| p.contains_key(name))
    {
        bail!("there's already a `[profile.{name}]`");
    }
    match profiles {
        Item::Value(Value::InlineTable(profiles)) => {
            let mut profile = InlineTable::new();
            profile.insert("repos", Value::Array(Array::new()));
            profiles.insert(name, profile.into());
        }
        Item::Table(profiles) => {
            let mut profile = new_table();
            profile.insert("repos", Item::Value(Value::Array(Array::new())));
            // Placed after every other table, where it's printed.
            profile.set_position(Some(after));
            profiles.insert(name, Item::Table(profile));
        }
        _ => bail!("`profile` in the config is not a table"),
    }
    Ok(())
}

/// Removes `[profile.<name>]` with its own comments; what's above those,
/// such as the file's opening comment, stays where it was.
pub fn remove(doc: &mut DocumentMut, name: &str) -> Result<()> {
    let profiles = profiles_mut(doc)?;
    let Some(removed) = profiles.remove(name) else {
        bail!("there's no `[profile.{name}]` in the config");
    };
    if let Item::Table(removed) = &removed
        && let above = split_prefix(removed).0.trim_end()
        && !above.is_empty()
    {
        let next = removed
            .position()
            .and_then(|at| {
                positions(doc.as_table())
                    .into_iter()
                    .filter(|p| *p > at)
                    .min()
            })
            .and_then(|at| at_position(doc.as_table_mut(), at));
        if let Some(next) = next {
            let prefix = next
                .decor()
                .prefix()
                .and_then(RawString::as_str)
                .unwrap_or_default();
            let prefix = format!("{above}\n{prefix}");
            next.decor_mut().set_prefix(prefix);
        } else {
            let trailing = doc.trailing().as_str().unwrap_or_default();
            let trailing = format!("{trailing}{above}\n");
            doc.set_trailing(trailing);
        }
    }
    let profiles = profiles_mut(doc)?;
    if profiles.is_empty()
        && doc
            .get("profile")
            .and_then(Item::as_table)
            .is_some_and(Table::is_implicit)
    {
        doc.remove("profile");
    }
    Ok(())
}

/// Renames `from` to `to` in its place, keeping its comments.
pub fn rename(doc: &mut DocumentMut, from: &str, to: &str) -> Result<()> {
    let order: Vec<String> = names(doc)
        .into_iter()
        .map(|name| if name == from { to.to_owned() } else { name })
        .collect();
    let profiles = profiles_mut(doc)?;
    if profiles.contains_key(to) {
        bail!("there's already a `[profile.{to}]`");
    }
    let (Some(old), Some(item)) = (profiles.key(from).cloned(), profiles.remove(from)) else {
        bail!("there's no `[profile.{from}]` in the config");
    };
    let key = Key::new(to)
        .with_leaf_decor(old.leaf_decor().clone())
        .with_dotted_decor(old.dotted_decor().clone());
    profiles.entry_format(&key).or_insert(item);
    reorder(doc, &order);
    Ok(())
}

/// Moves `name` to `to` among the profiles, counted without it.
pub fn move_to(doc: &mut DocumentMut, name: &str, to: usize) -> Result<()> {
    let mut order = names(doc);
    let Some(at) = order.iter().position(|n| n == name) else {
        bail!("there's no `[profile.{name}]` in the config");
    };
    // A profile's own `[[profile.<name>.repos]]` or other tables below it
    // are placed on their own, and would stay behind. That's so for any
    // profile the move hands another place, not only this one.
    if let Some(other) = order.iter().find(|n| has_tables(doc, n)) {
        bail!(
            "`[profile.{other}]` has tables of its own, so the profiles can only be reordered \
             by hand"
        );
    }
    let moved = order.remove(at);
    order.insert(to.min(order.len()), moved);
    reorder(doc, &order);
    Ok(())
}

fn has_tables(doc: &DocumentMut, name: &str) -> bool {
    doc.get("profile")
        .and_then(Item::as_table_like)
        .and_then(|profiles| profiles.get(name))
        .and_then(Item::as_table_like)
        .is_some_and(|profile| {
            profile
                .iter()
                .any(|(_, item)| item.is_table() || item.is_array_of_tables())
        })
}

fn names(doc: &DocumentMut) -> Vec<String> {
    doc.get("profile")
        .and_then(Item::as_table_like)
        .map(|p| p.iter().map(|(name, _)| name.to_owned()).collect())
        .unwrap_or_default()
}

fn profiles_mut(doc: &mut DocumentMut) -> Result<&mut dyn TableLike> {
    doc.get_mut("profile")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| eyre!("there are no profiles in the config"))
}

/// Puts the profiles in `order`: in the table, which is the order of
/// dotted keys and inline entries, and in the places header tables print
/// at, which are handed round among them. A header's own comments, the
/// lines right above it, go with it; what's above those stays in place.
fn reorder(doc: &mut DocumentMut, order: &[String]) {
    let rank = |key: &Key| order.iter().position(|n| n == key.get());
    match doc.get_mut("profile") {
        Some(Item::Table(profiles)) => {
            profiles.sort_values_by(|a, _, b, _| rank(a).cmp(&rank(b)));
            let mut headers: Vec<&mut Table> = profiles
                .iter_mut()
                .filter_map(|(_, item)| item.as_table_mut())
                .filter(|t| !t.is_dotted())
                .collect();
            let mut places: Vec<(Option<isize>, String)> = headers
                .iter()
                .map(|t| (t.position(), split_prefix(t).0.to_owned()))
                .collect();
            places.sort_by_key(|(position, _)| *position);
            for (table, (position, above)) in headers.iter_mut().zip(places) {
                let own = split_prefix(table).1.to_owned();
                table.set_position(position);
                table.decor_mut().set_prefix(format!("{above}{own}"));
            }
        }
        Some(Item::Value(Value::InlineTable(profiles))) => {
            profiles.sort_values_by(|a, _, b, _| rank(a).cmp(&rank(b)));
        }
        _ => {}
    }
}

/// A header table's prefix split into what's above its own comments, and
/// its own comments: the comment lines right above it, and its indent.
fn split_prefix(table: &Table) -> (&str, &str) {
    let prefix = table
        .decor()
        .prefix()
        .and_then(RawString::as_str)
        .unwrap_or_default();
    let mut own = prefix.len();
    for line in prefix.split_inclusive('\n').rev() {
        let text = line.trim();
        let indent = !line.ends_with('\n') && text.is_empty();
        if !indent && !text.starts_with('#') {
            break;
        }
        own -= line.len();
    }
    prefix.split_at(own)
}

/// The furthest place any table in `table` prints at.
fn last_position(table: &Table) -> isize {
    positions(table).into_iter().max().unwrap_or(0)
}

/// The places the tables in `table` print at.
fn positions(table: &Table) -> Vec<isize> {
    let mut found = Vec::new();
    for (_, item) in table {
        let tables: Vec<&Table> = match item {
            Item::Table(t) => vec![t],
            Item::ArrayOfTables(tables) => tables.iter().collect(),
            _ => Vec::new(),
        };
        for t in tables {
            if !t.is_implicit() {
                found.extend(t.position());
            }
            found.extend(positions(t));
        }
    }
    found
}

/// The header table in `table` that prints at `at`.
fn at_position(table: &mut Table, at: isize) -> Option<&mut Table> {
    for (_, item) in table.iter_mut() {
        let tables: Vec<&mut Table> = match item {
            Item::Table(t) => vec![t],
            Item::ArrayOfTables(tables) => tables.iter_mut().collect(),
            _ => Vec::new(),
        };
        for t in tables {
            if t.position() == Some(at) && !t.is_implicit() {
                return Some(t);
            }
            if let Some(found) = at_position(t, at) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sanic_core::config::Config;

    use super::*;
    use crate::poll::tests::NoCheckouts;

    fn edit(text: &str, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> String {
        let mut doc: DocumentMut = text.parse().unwrap();
        f(&mut doc).unwrap();
        doc.to_string()
    }

    /// The profiles in the order the loader reads them.
    fn loaded(text: &str) -> Vec<String> {
        Config::parse(text, Path::new("/"), &NoCheckouts)
            .unwrap()
            .profiles
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    const HEADERS: &str = r#"# Mine.

# The ring team's.
[profile.ring]
repos = [{ github = "a" }]

[runner]
model = "m"

[profile.default] # everything else
repos = [{ github = "b" }]
"#;

    #[test]
    fn header_profiles_move_and_rename_in_place_with_their_comments() {
        let moved = edit(HEADERS, |doc| move_to(doc, "default", 0));
        assert_eq!(
            moved,
            r#"# Mine.

[profile.default] # everything else
repos = [{ github = "b" }]

[runner]
model = "m"

# The ring team's.
[profile.ring]
repos = [{ github = "a" }]
"#
        );
        assert_eq!(loaded(&moved), ["default", "ring"]);
        assert_eq!(edit(&moved, |doc| move_to(doc, "default", 1)), HEADERS);

        let renamed = edit(HEADERS, |doc| rename(doc, "ring", "security team"));
        assert!(
            renamed.contains("# The ring team's.\n[profile.\"security team\"]\n"),
            "{renamed}"
        );
        assert_eq!(loaded(&renamed), ["security team", "default"]);
    }

    #[test]
    fn profiles_are_added_last_and_removed() {
        let added = edit(HEADERS, |doc| add(doc, "new"));
        assert!(added.ends_with("\n[profile.new]\nrepos = []\n"), "{added}");
        let moved = edit(&added, |doc| move_to(doc, "new", 0));
        assert!(
            moved.starts_with("# Mine.\n\n[profile.new]\nrepos = []\n"),
            "{moved}"
        );
        let removed = edit(HEADERS, |doc| remove(doc, "ring"));
        assert!(!removed.contains("ring"), "{removed}");
        assert!(removed.starts_with("# Mine.\n\n[runner]"), "{removed}");
        let none = edit(&removed, |doc| remove(doc, "default"));
        assert_eq!(none, "# Mine.\n\n[runner]\nmodel = \"m\"\n");
        assert_eq!(edit("", |doc| add(doc, "p")), "\n[profile.p]\nrepos = []\n");
    }

    #[test]
    fn dotted_and_inline_profiles_are_edited_in_their_own_form() {
        let dotted =
            "profile.a.repos = [{ github = \"a\" }]\nprofile.b.repos = [{ github = \"b\" }]\n";
        let moved = edit(dotted, |doc| move_to(doc, "b", 0));
        assert_eq!(
            moved,
            "profile.b.repos = [{ github = \"b\" }]\nprofile.a.repos = [{ github = \"a\" }]\n"
        );
        let renamed = edit(dotted, |doc| rename(doc, "a", "z"));
        assert_eq!(loaded(&renamed), ["z", "b"]);
        let added = edit(dotted, |doc| add(doc, "c"));
        assert_eq!(
            Config::parse(&added, Path::new("/"), &NoCheckouts)
                .unwrap_err()
                .to_string(),
            "in profile `c`"
        );

        let inline = "profile = { a = { repos = [{ github = \"a\" }] }, b = { repos = [{ github = \"b\" }] } }\n";
        let moved = edit(inline, |doc| move_to(doc, "b", 0));
        assert_eq!(loaded(&moved), ["b", "a"]);
        let renamed = edit(inline, |doc| rename(doc, "a", "z"));
        assert_eq!(loaded(&renamed), ["z", "b"]);
        let added = edit(inline, |doc| add(doc, "c"));
        assert!(added.contains("c = { repos = [] }"), "{added}");
    }

    #[test]
    fn edits_on_missing_or_clashing_profiles_are_refused() {
        let mut doc: DocumentMut = HEADERS.parse().unwrap();
        for (result, needle) in [
            (add(&mut doc, "ring"), "already"),
            (rename(&mut doc, "ring", "default"), "already"),
            (rename(&mut doc, "nope", "x"), "no `[profile.nope]`"),
            (remove(&mut doc, "nope"), "no `[profile.nope]`"),
            (move_to(&mut doc, "nope", 0), "no `[profile.nope]`"),
        ] {
            let err = result.unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
        }
        assert_eq!(doc.to_string(), HEADERS);

        let mut doc: DocumentMut =
            "[profile.p]\n[[profile.p.repos]]\ngithub = \"o\"\n\n[profile.q]\nrepos = []\n"
                .parse()
                .unwrap();
        let err = move_to(&mut doc, "p", 1).unwrap_err();
        assert!(err.to_string().contains("by hand"), "{err}");
        // Moving another profile hands `p` a place too.
        let err = move_to(&mut doc, "q", 0).unwrap_err();
        assert!(err.to_string().contains("`[profile.p]`"), "{err}");
    }
}
