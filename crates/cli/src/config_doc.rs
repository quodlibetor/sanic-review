//! The config file as the editor sees it: a typed view over its TOML
//! covering every key, edited in place through [`crate::config_edit`]'s
//! helpers, so comments and layout stay. Each edit is an [`Op`], kept in
//! order, so the edits can be made again on the file as it is when they're
//! saved, if someone else wrote it meanwhile.

pub mod choices;
mod entry;
mod profiles;
pub mod schema;

use std::{fmt, path::Path};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use sanic_core::config::{CheckoutResolver, Config, Unloadable};
use toml_edit::{Array, DocumentMut, Item, TableLike, Value};

pub use self::entry::{EntryKind, RepoEntry};
use self::schema::{Field, Kind};
use crate::config_edit::{
    move_keeping_comments, new_table, push_on_own_line, remove_keeping_comments, set_in,
    unset_keeping_comments,
};

/// What a config file that didn't exist starts with.
pub const NEW_FILE_HEADER: &str =
    "# sanic-review config; the format is described in docs/DESIGN.md.\n";

/// A table of the config: one of the fixed sections, or a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Table {
    Github,
    Poll,
    ReviewRequests,
    Runner,
    Profile(String),
}

impl Table {
    /// The tables every config has, in the order the editor lists them.
    pub const SECTIONS: [Self; 4] = [Self::Github, Self::Poll, Self::ReviewRequests, Self::Runner];

    #[must_use]
    pub fn fields(&self) -> &'static [Field] {
        match self {
            Self::Github => schema::GITHUB,
            Self::Poll => schema::POLL,
            Self::ReviewRequests => schema::REVIEW_REQUESTS,
            Self::Runner => schema::RUNNER,
            Self::Profile(_) => schema::PROFILE,
        }
    }

    fn section(&self) -> Option<&'static str> {
        match self {
            Self::Github => Some("github"),
            Self::Poll => Some("poll"),
            Self::ReviewRequests => Some("review_requests"),
            Self::Runner => Some("runner"),
            Self::Profile(_) => None,
        }
    }
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.section(), self) {
            (Some(name), _) => f.write_str(name),
            (None, Self::Profile(name)) => write!(f, "profile.{name}"),
            (None, _) => Ok(()),
        }
    }
}

/// A key of the config, one [`schema`] lists.
#[derive(Debug, Clone)]
pub struct Key {
    pub table: Table,
    pub name: &'static str,
    field: &'static Field,
}

impl Key {
    /// `None` when `table` has no such key.
    #[must_use]
    pub fn new(table: Table, name: &str) -> Option<Self> {
        let field = table.fields().iter().find(|f| f.name == name)?;
        Some(Self {
            table,
            name: field.name,
            field,
        })
    }

    #[must_use]
    pub fn field(&self) -> &'static Field {
        self.field
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        (&self.table, self.name) == (&other.table, other.name)
    }
}

impl Eq for Key {}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.table, self.name)
    }
}

/// A value for a [`Kind::Text`], [`Kind::Number`] or [`Kind::Bool`] key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scalar {
    Text(String),
    Number(i64),
    Bool(bool),
}

impl Scalar {
    fn kind(&self) -> Kind {
        match self {
            Self::Text(_) => Kind::Text,
            Self::Number(_) => Kind::Number,
            Self::Bool(_) => Kind::Bool,
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Text(text) => text.as_str().into(),
            Self::Number(n) => (*n).into(),
            Self::Bool(b) => (*b).into(),
        }
    }
}

/// What the file says for a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting<T> {
    Unset,
    Set(T),
    /// A value of the wrong type, as written.
    Invalid(String),
}

/// A profile's `repos`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repos {
    /// Each entry, or as written when it's in none of the shapes.
    pub entries: Vec<Result<RepoEntry, String>>,
    /// Written as `[[profile.<name>.repos]]` tables, which the loader
    /// takes but the editor only shows: [`Op`]s on them are refused.
    pub as_tables: bool,
}

/// One edit of the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Set {
        key: Key,
        value: Scalar,
    },
    /// Removes a key of any kind, leaving it to its default.
    Unset {
        key: Key,
    },
    /// Appends to a [`Kind::List`] key.
    Push {
        key: Key,
        value: String,
    },
    /// Removes the first item equal to `value`.
    Remove {
        key: Key,
        value: String,
    },
    Replace {
        key: Key,
        old: String,
        new: String,
    },
    /// Moves the first item equal to `value` to `to`, counted without it.
    Move {
        key: Key,
        value: String,
        to: usize,
    },
    PushEntry {
        profile: String,
        entry: RepoEntry,
    },
    RemoveEntry {
        profile: String,
        entry: RepoEntry,
    },
    /// Replaces an entry, e.g. with one of another [`EntryKind`].
    ReplaceEntry {
        profile: String,
        old: RepoEntry,
        new: RepoEntry,
    },
    MoveEntry {
        profile: String,
        entry: RepoEntry,
        to: usize,
    },
    /// A profile with an empty `repos`, after the others.
    AddProfile {
        name: String,
    },
    RemoveProfile {
        name: String,
    },
    RenameProfile {
        from: String,
        to: String,
    },
    /// Moves a profile to `to` in the file's order, counted without it;
    /// the first of equally specific matches wins.
    MoveProfile {
        name: String,
        to: usize,
    },
}

/// The config file's document, as read and as edited.
#[derive(Debug, Clone)]
pub struct ConfigDoc {
    original: String,
    doc: DocumentMut,
    /// Prepended to a file that didn't exist.
    header: &'static str,
    ops: Vec<Op>,
}

impl ConfigDoc {
    /// The file's text, or `None` for a file that doesn't exist yet. Text
    /// that isn't TOML is refused: it has to be fixed by hand. Text that is,
    /// but doesn't load, is fine; fixing it is what editing is for.
    pub fn parse(text: Option<&str>) -> Result<Self> {
        let original = text.unwrap_or_default();
        Ok(Self {
            original: original.to_owned(),
            doc: original.parse().wrap_err("the config isn't valid TOML")?,
            header: if text.is_some() { "" } else { NEW_FILE_HEADER },
            ops: Vec::new(),
        })
    }

    /// The text to write.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}{}", self.header, self.doc)
    }

    /// The text it was read from; empty for a new file.
    #[must_use]
    pub fn original(&self) -> &str {
        &self.original
    }

    /// Whether writing it would change the file.
    #[must_use]
    pub fn is_changed(&self) -> bool {
        !self.ops.is_empty() && self.text() != self.original
    }

    /// The edits so far, in order.
    #[must_use]
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Makes `op`, or leaves the document as it was and says why not.
    pub fn apply(&mut self, op: Op) -> Result<()> {
        let mut doc = self.doc.clone();
        op.apply(&mut doc)?;
        self.doc = doc;
        self.ops.push(op);
        Ok(())
    }

    /// These edits made again on `text`, the file as it is now: an error
    /// names the first that no longer applies.
    pub fn replay_onto(&self, text: Option<&str>) -> Result<Self> {
        let mut fresh = Self::parse(text)?;
        for op in &self.ops {
            fresh
                .apply(op.clone())
                .wrap_err("the file changed since, and this edit no longer applies")?;
        }
        Ok(fresh)
    }

    /// Loads the edited text as `serve` would load it from `path`.
    pub fn load(&self, path: &Path, resolver: &dyn CheckoutResolver) -> Result<Config, Unloadable> {
        Config::parse_file(&self.text(), path, resolver).map_err(Unloadable)
    }

    #[must_use]
    pub fn scalar(&self, key: &Key) -> Setting<Scalar> {
        let Some(item) = self.table(&key.table).and_then(|t| t.get(key.name)) else {
            return Setting::Unset;
        };
        let scalar = match (key.field().kind, item.as_value()) {
            (Kind::Text, Some(Value::String(s))) => Scalar::Text(s.value().clone()),
            (Kind::Number, Some(Value::Integer(n))) => Scalar::Number(*n.value()),
            (Kind::Bool, Some(Value::Boolean(b))) => Scalar::Bool(*b.value()),
            _ => return Setting::Invalid(written(item)),
        };
        Setting::Set(scalar)
    }

    #[must_use]
    pub fn list(&self, key: &Key) -> Setting<Vec<String>> {
        let Some(item) = self.table(&key.table).and_then(|t| t.get(key.name)) else {
            return Setting::Unset;
        };
        item.as_array()
            .and_then(|list| {
                list.iter()
                    .map(|v| v.as_str().map(String::from))
                    .collect::<Option<Vec<_>>>()
            })
            .map_or_else(|| Setting::Invalid(written(item)), Setting::Set)
    }

    /// The profiles' names, in the file's order.
    #[must_use]
    pub fn profiles(&self) -> Vec<String> {
        self.doc
            .get("profile")
            .and_then(Item::as_table_like)
            .map(|p| p.iter().map(|(name, _)| name.to_owned()).collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn repos(&self, profile: &str) -> Setting<Repos> {
        let Some(item) = self
            .table(&Table::Profile(profile.to_owned()))
            .and_then(|t| t.get("repos"))
        else {
            return Setting::Unset;
        };
        let read = |value: &Value| RepoEntry::read(value).ok_or_else(|| written_value(value));
        match item {
            Item::Value(Value::Array(list)) => Setting::Set(Repos {
                entries: list.iter().map(read).collect(),
                as_tables: false,
            }),
            Item::ArrayOfTables(tables) => Setting::Set(Repos {
                entries: tables
                    .iter()
                    .map(|t| read(&Value::InlineTable(t.clone().into_inline_table())))
                    .collect(),
                as_tables: true,
            }),
            _ => Setting::Invalid(written(item)),
        }
    }

    fn table(&self, table: &Table) -> Option<&dyn TableLike> {
        match table {
            Table::Profile(name) => self
                .doc
                .get("profile")
                .and_then(Item::as_table_like)?
                .get(name)?
                .as_table_like(),
            _ => self.doc.get(&table.to_string())?.as_table_like(),
        }
    }
}

/// An item as the file writes it, on one line.
fn written(item: &Item) -> String {
    match item {
        Item::Value(value) => written_value(value),
        Item::Table(_) => "a table".into(),
        Item::ArrayOfTables(_) => "a list of tables".into(),
        Item::None => String::new(),
    }
}

fn written_value(value: &Value) -> String {
    let mut value = value.clone();
    value.decor_mut().clear();
    value.to_string()
}

impl Op {
    fn apply(&self, doc: &mut DocumentMut) -> Result<()> {
        match self {
            Self::Set { key, value } => {
                if key.field().kind != value.kind() {
                    bail!("`{key}` doesn't take {value:?}");
                }
                set_in(table_mut(doc, &key.table)?, key.name, value.to_value());
            }
            Self::Unset { key } => {
                unset_keeping_comments(item_mut(doc, &key.table)?, key.name);
                drop_if_empty(doc, &key.table);
            }
            Self::Push { key, value } => push_on_own_line(list_mut(doc, key, true)?, value.into()),
            Self::Remove { key, value } => {
                let list = list_mut(doc, key, false)?;
                position(list, |v| v.as_str() == Some(value))
                    .ok_or_else(|| eyre!("`{value}` isn't in `{key}`"))?;
                remove_first(list, |v| v.as_str() == Some(value));
            }
            Self::Replace { key, old, new } => {
                let list = list_mut(doc, key, false)?;
                let at = position(list, |v| v.as_str() == Some(old))
                    .ok_or_else(|| eyre!("`{old}` isn't in `{key}`"))?;
                replace(list, at, new.into());
            }
            Self::Move { key, value, to } => {
                let list = list_mut(doc, key, false)?;
                let at = position(list, |v| v.as_str() == Some(value))
                    .ok_or_else(|| eyre!("`{value}` isn't in `{key}`"))?;
                move_keeping_comments(list, at, *to);
            }
            Self::PushEntry { profile, entry } => {
                push_on_own_line(repos_mut(doc, profile)?, entry.to_value());
            }
            Self::RemoveEntry { profile, entry } => {
                let list = repos_mut(doc, profile)?;
                entry_at(list, profile, entry)?;
                remove_first(list, |v| RepoEntry::read(v).as_ref() == Some(entry));
            }
            Self::ReplaceEntry { profile, old, new } => {
                let list = repos_mut(doc, profile)?;
                let at = entry_at(list, profile, old)?;
                replace(list, at, new.to_value());
            }
            Self::MoveEntry { profile, entry, to } => {
                let list = repos_mut(doc, profile)?;
                let at = entry_at(list, profile, entry)?;
                move_keeping_comments(list, at, *to);
            }
            Self::AddProfile { name } => profiles::add(doc, name)?,
            Self::RemoveProfile { name } => profiles::remove(doc, name)?,
            Self::RenameProfile { from, to } => profiles::rename(doc, from, to)?,
            Self::MoveProfile { name, to } => profiles::move_to(doc, name, *to)?,
        }
        Ok(())
    }
}

/// `table`, creating a missing section; a profile has to exist.
fn table_mut<'a>(doc: &'a mut DocumentMut, table: &Table) -> Result<&'a mut dyn TableLike> {
    item_mut(doc, table)?
        .as_table_like_mut()
        .ok_or_else(|| eyre!("`{table}` in the config is not a table"))
}

/// `table`'s item, creating a missing section; a profile has to exist.
fn item_mut<'a>(doc: &'a mut DocumentMut, table: &Table) -> Result<&'a mut Item> {
    match table {
        Table::Profile(name) => doc
            .get_mut("profile")
            .and_then(Item::as_table_like_mut)
            .and_then(|profiles| profiles.get_mut(name))
            .ok_or_else(|| eyre!("there's no `[profile.{name}]` in the config")),
        _ => Ok(doc
            .entry(&table.to_string())
            .or_insert_with(|| Item::Table(new_table()))),
    }
}

/// Removes a section left with nothing in it, and nothing written in its
/// header's line or above it.
fn drop_if_empty(doc: &mut DocumentMut, table: &Table) {
    let Some(name) = table.section() else {
        return;
    };
    let bare = doc.get(name).and_then(Item::as_table).is_some_and(|t| {
        let decor = t.decor();
        t.is_empty()
            && [decor.prefix(), decor.suffix()].into_iter().all(|text| {
                text.and_then(|t| t.as_str())
                    .is_none_or(|t| !t.contains('#'))
            })
    });
    if bare {
        doc.remove(name);
    }
}

fn list_mut<'a>(doc: &'a mut DocumentMut, key: &Key, create: bool) -> Result<&'a mut Array> {
    if key.field().kind != Kind::List {
        bail!("`{key}` isn't a list");
    }
    let table = table_mut(doc, &key.table)?;
    if create && !table.contains_key(key.name) {
        table.insert(key.name, Item::Value(Value::Array(Array::new())));
    }
    table
        .get_mut(key.name)
        .ok_or_else(|| eyre!("`{key}` isn't set"))?
        .as_array_mut()
        .ok_or_else(|| eyre!("`{key}` in the config is not a list"))
}

fn repos_mut<'a>(doc: &'a mut DocumentMut, profile: &str) -> Result<&'a mut Array> {
    let table = profiles::get_mut(doc, profile)?;
    let item = table
        .entry("repos")
        .or_insert(Item::Value(Value::Array(Array::new())));
    if item.is_array_of_tables() {
        bail!(
            "`profile.{profile}.repos` is written as `[[profile.{profile}.repos]]` tables, \
             which can only be edited by hand; write it as a `repos = [...]` list to edit it here"
        );
    }
    item.as_array_mut()
        .ok_or_else(|| eyre!("`profile.{profile}.repos` in the config is not a list"))
}

fn position(list: &Array, mut matches: impl FnMut(&Value) -> bool) -> Option<usize> {
    list.iter().position(&mut matches)
}

fn entry_at(list: &Array, profile: &str, entry: &RepoEntry) -> Result<usize> {
    position(list, |v| RepoEntry::read(v).as_ref() == Some(entry)).ok_or_else(|| {
        eyre!(
            "`{}` isn't in `profile.{profile}.repos`",
            written_value(&entry.to_value())
        )
    })
}

fn remove_first(list: &mut Array, mut matches: impl FnMut(&Value) -> bool) {
    let mut done = false;
    remove_keeping_comments(list, |v| {
        let this = !done && matches(v);
        done |= this;
        this
    });
}

/// Replaces `list[at]` with `value`, in its place and with its comments.
fn replace(list: &mut Array, at: usize, mut value: Value) {
    if let Some(old) = list.get(at) {
        *value.decor_mut() = old.decor().clone();
        list.replace_formatted(at, value);
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;
    use crate::poll::tests::NoCheckouts;

    const CONFIG: &str = r#"# Mine.
[runner]
# Reviews are expensive.
model = "claude-sonnet-5" # for now
max_concurrent = 3

[profile.ring]
skills = [
  "~/s/a", # first
  "~/s/b",
]
repos = [
  "~/github/sanic-cli",
  { repo = "~/github/services", paths = ["/documentation/**"] },
]

[profile.default]
repos = [{ github = "org" }]
"#;

    fn doc() -> ConfigDoc {
        ConfigDoc::parse(Some(CONFIG)).unwrap()
    }

    fn key(table: Table, name: &str) -> Key {
        Key::new(table, name).unwrap()
    }

    fn profile(name: &str) -> Table {
        Table::Profile(name.into())
    }

    #[test]
    fn every_key_the_loader_takes_is_in_the_schema() {
        // Each key set to a valid value loads, and one the schema doesn't
        // know is refused, so the schema and the loader agree.
        let mut text = String::new();
        for table in Table::SECTIONS.into_iter().chain([profile("p")]) {
            writeln!(text, "[{table}]").unwrap();
            for field in table.fields() {
                let value = match (field.name, field.kind) {
                    ("model", _) => "\"auto\"",
                    ("repos", _) => "[{ github = \"org\" }]",
                    (_, Kind::Text) => "\"x\"",
                    (_, Kind::Number) => "1",
                    (_, Kind::Bool) => "true",
                    (_, Kind::List | Kind::Repos) => "[]",
                };
                writeln!(text, "{} = {value}", field.name).unwrap();
            }
        }
        Config::parse(&text, Path::new("/"), &NoCheckouts).unwrap();
        assert_eq!(Key::new(Table::Runner, "bogus"), None);
    }

    #[test]
    fn keys_read_as_set_unset_or_invalid() {
        let mut doc = doc();
        assert_eq!(
            doc.scalar(&key(Table::Runner, "model")),
            Setting::Set(Scalar::Text("claude-sonnet-5".into()))
        );
        assert_eq!(
            doc.scalar(&key(Table::Runner, "max_concurrent")),
            Setting::Set(Scalar::Number(3))
        );
        assert_eq!(doc.scalar(&key(Table::Poll, "quiet_secs")), Setting::Unset);
        assert_eq!(
            doc.list(&key(profile("ring"), "skills")),
            Setting::Set(vec!["~/s/a".into(), "~/s/b".into()])
        );
        assert_eq!(doc.profiles(), ["ring", "default"]);
        let Setting::Set(repos) = doc.repos("ring") else {
            panic!("no repos");
        };
        assert_eq!(repos.entries.len(), 2);
        assert!(!repos.as_tables);

        doc = ConfigDoc::parse(Some("[runner]\nmodel = 5\nread_paths = \"x\"\n")).unwrap();
        assert_eq!(
            doc.scalar(&key(Table::Runner, "model")),
            Setting::Invalid("5".into())
        );
        assert_eq!(
            doc.list(&key(Table::Runner, "read_paths")),
            Setting::Invalid("\"x\"".into())
        );
    }

    #[test]
    fn scalars_are_set_and_unset_keeping_comments() {
        let mut doc = doc();
        let model = key(Table::Runner, "model");
        doc.apply(Op::Set {
            key: model.clone(),
            value: Scalar::Text("auto".into()),
        })
        .unwrap();
        assert!(
            doc.text().contains("model = \"auto\" # for now\n"),
            "{}",
            doc.text()
        );
        doc.apply(Op::Unset { key: model.clone() }).unwrap();
        assert!(
            doc.text()
                .contains("[runner]\n# Reviews are expensive.\nmax_concurrent = 3\n"),
            "{}",
            doc.text()
        );
        // The last key's comment stays after the key before it.
        let mut doc = ConfigDoc::parse(Some(
            "[runner]\nclaude = \"c\"\n# the model\nmodel = \"m\"\n\n[poll]\nquiet_secs = 1\n",
        ))
        .unwrap();
        doc.apply(Op::Unset { key: model }).unwrap();
        assert_eq!(
            doc.text(),
            "[runner]\nclaude = \"c\"\n# the model\n\n[poll]\nquiet_secs = 1\n"
        );

        // A section left empty goes; one that's new is made.
        let quiet = key(Table::Poll, "quiet_secs");
        doc.apply(Op::Unset { key: quiet.clone() }).unwrap();
        assert_eq!(doc.text(), "[runner]\nclaude = \"c\"\n# the model\n");
        doc.apply(Op::Set {
            key: quiet,
            value: Scalar::Number(5),
        })
        .unwrap();
        assert!(
            doc.text().ends_with("\n[poll]\nquiet_secs = 5\n"),
            "{}",
            doc.text()
        );

        let err = doc
            .apply(Op::Set {
                key: key(Table::Poll, "quiet_secs"),
                value: Scalar::Bool(true),
            })
            .unwrap_err();
        assert!(err.to_string().contains("poll.quiet_secs"), "{err}");
    }

    #[test]
    fn list_items_are_added_removed_replaced_and_moved() {
        let mut doc = doc();
        let skills = key(profile("ring"), "skills");
        let op = |doc: &mut ConfigDoc, op| doc.apply(op).unwrap();
        op(
            &mut doc,
            Op::Push {
                key: skills.clone(),
                value: "~/s/c".into(),
            },
        );
        op(
            &mut doc,
            Op::Move {
                key: skills.clone(),
                value: "~/s/a".into(),
                to: 2,
            },
        );
        op(
            &mut doc,
            Op::Replace {
                key: skills.clone(),
                old: "~/s/b".into(),
                new: "~/s/B".into(),
            },
        );
        assert!(
            doc.text()
                .contains("skills = [\n  \"~/s/B\",\n  \"~/s/c\",\n  \"~/s/a\", # first\n]"),
            "{}",
            doc.text()
        );
        op(
            &mut doc,
            Op::Remove {
                key: skills.clone(),
                value: "~/s/a".into(),
            },
        );
        assert!(
            doc.text()
                .contains("skills = [\n  \"~/s/B\",\n  \"~/s/c\",\n]"),
            "{}",
            doc.text()
        );
        let err = doc
            .apply(Op::Remove {
                key: skills,
                value: "~/s/a".into(),
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("isn't in `profile.ring.skills`"),
            "{err}"
        );

        // A missing list is made on the first push.
        let titles = key(Table::ReviewRequests, "skip_titles");
        op(
            &mut doc,
            Op::Push {
                key: titles,
                value: "wip*".into(),
            },
        );
        assert!(
            doc.text()
                .contains("[review_requests]\nskip_titles = [\"wip*\"]\n"),
            "{}",
            doc.text()
        );
    }

    #[test]
    fn repo_entries_are_edited_and_switch_kind_in_place() {
        let mut doc = doc();
        let services = RepoEntry::Scoped {
            path: "~/github/services".into(),
            paths: vec!["/documentation/**".into()],
            remote: None,
        };
        let github = services
            .clone()
            .with_kind(EntryKind::Github, Some("sanic-hq/services"));
        doc.apply(Op::ReplaceEntry {
            profile: "ring".into(),
            old: services,
            new: github.clone(),
        })
        .unwrap();
        doc.apply(Op::MoveEntry {
            profile: "ring".into(),
            entry: github.clone(),
            to: 0,
        })
        .unwrap();
        doc.apply(Op::PushEntry {
            profile: "default".into(),
            entry: RepoEntry::Checkout {
                path: "~/src/x".into(),
                remote: Some("upstream".into()),
            },
        })
        .unwrap();
        assert!(
            doc.text().contains(
                "repos = [\n  { github = \"sanic-hq/services\", paths = [\"/documentation/**\"] },\n  \"~/github/sanic-cli\",\n]"
            ),
            "{}",
            doc.text()
        );
        assert!(
            doc.text().contains(
                "repos = [{ github = \"org\" }, { repo = \"~/src/x\", remote = \"upstream\" }]"
            ),
            "{}",
            doc.text()
        );
        doc.apply(Op::RemoveEntry {
            profile: "ring".into(),
            entry: github,
        })
        .unwrap();
        assert!(
            doc.text()
                .contains("repos = [\n  \"~/github/sanic-cli\",\n]"),
            "{}",
            doc.text()
        );
    }

    #[test]
    fn repos_written_as_tables_are_shown_but_not_edited() {
        let text = "[profile.p]\n[[profile.p.repos]]\ngithub = \"org\"\n\n[[profile.p.repos]]\nbogus = 1\n";
        let mut doc = ConfigDoc::parse(Some(text)).unwrap();
        assert_eq!(
            doc.repos("p"),
            Setting::Set(Repos {
                entries: vec![
                    Ok(RepoEntry::Github {
                        name: "org".into(),
                        paths: Vec::new(),
                    }),
                    Err("{ bogus = 1 }".into()),
                ],
                as_tables: true,
            })
        );
        let err = doc
            .apply(Op::PushEntry {
                profile: "p".into(),
                entry: RepoEntry::Github {
                    name: "o".into(),
                    paths: Vec::new(),
                },
            })
            .unwrap_err();
        assert!(err.to_string().contains("by hand"), "{err}");
        assert_eq!(doc.text(), text);
        assert!(!doc.is_changed());
    }

    #[test]
    fn a_failed_edit_changes_nothing() {
        let mut doc = doc();
        let _ = doc
            .apply(Op::Remove {
                key: key(profile("nope"), "skills"),
                value: "x".into(),
            })
            .unwrap_err();
        assert_eq!(doc.text(), CONFIG);
        assert!(doc.ops().is_empty());
        assert!(!doc.is_changed());
    }

    #[test]
    fn edits_replay_onto_a_file_changed_meanwhile() {
        let mut doc = doc();
        let skills = key(profile("ring"), "skills");
        doc.apply(Op::Push {
            key: skills.clone(),
            value: "~/s/c".into(),
        })
        .unwrap();
        doc.apply(Op::Remove {
            key: skills.clone(),
            value: "~/s/a".into(),
        })
        .unwrap();

        // Someone added a skip title from the dashboard.
        let changed = CONFIG.replace(
            "[runner]",
            "[review_requests]\nskip_titles = [\"x\"]\n\n[runner]",
        );
        let replayed = doc.replay_onto(Some(&changed)).unwrap();
        assert!(replayed.text().contains("skip_titles = [\"x\"]"));
        assert_eq!(
            replayed.list(&skills),
            Setting::Set(vec!["~/s/b".into(), "~/s/c".into()])
        );

        // Someone removed what the edits remove: they no longer apply.
        let err = doc
            .replay_onto(Some(&CONFIG.replace("  \"~/s/a\", # first\n", "")))
            .unwrap_err();
        assert!(format!("{err:#}").contains("`~/s/a` isn't in"), "{err:#}");
    }

    #[test]
    fn a_new_file_gets_the_header_and_loads_once_complete() {
        let mut doc = ConfigDoc::parse(None).unwrap();
        assert!(!doc.is_changed());
        doc.apply(Op::AddProfile {
            name: "default".into(),
        })
        .unwrap();
        let path = Path::new("/c/config.toml");
        let err = doc.load(path, &NoCheckouts).unwrap_err();
        assert!(err.to_string().contains("`repos` is empty"), "{err}");
        doc.apply(Op::PushEntry {
            profile: "default".into(),
            entry: RepoEntry::Github {
                name: "org".into(),
                paths: Vec::new(),
            },
        })
        .unwrap();
        assert!(doc.is_changed());
        assert_eq!(
            doc.text(),
            format!("{NEW_FILE_HEADER}\n[profile.default]\nrepos = [{{ github = \"org\" }}]\n")
        );
        doc.load(path, &NoCheckouts).unwrap();
    }

    #[test]
    fn text_that_isnt_toml_is_refused() {
        let err = ConfigDoc::parse(Some("[runner")).unwrap_err();
        assert!(err.to_string().contains("valid TOML"), "{err}");
    }
}
