//! The config file as the editor sees it: a typed view over its TOML
//! covering every key, edited in place through [`crate::config_edit`]'s
//! helpers, so comments and layout stay. Each edit is an [`Op`], kept in
//! order, so the edits can be made again on the file as it is when they're
//! saved, if someone else wrote it meanwhile.

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
    move_keeping_comments, new_table, push_on_own_line, remove_keeping_comments, set_in, take_turn,
    unset_keeping_comments, write_atomically,
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

/// Equal when read from the same text and edited the same way.
impl PartialEq for ConfigDoc {
    fn eq(&self, other: &Self) -> bool {
        (&self.original, self.header, &self.ops) == (&other.original, other.header, &other.ops)
    }
}

impl Eq for ConfigDoc {}

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

    /// The text it was read from, or `None` for a file that didn't exist.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.header.is_empty().then_some(self.original.as_str())
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

    /// The comments written with `key`: the comment lines above it, and
    /// the one after its value on its line, each without its `#`.
    #[must_use]
    pub fn comments(&self, key: &Key) -> Comments {
        let Some(table) = self.table(&key.table) else {
            return Comments::default();
        };
        let above = table
            .key(key.name)
            .map(|k| comment_lines(k.leaf_decor().prefix()))
            .unwrap_or_default();
        let after = table
            .get(key.name)
            .and_then(Item::as_value)
            .and_then(|v| comment_lines(v.decor().suffix()).into_iter().next());
        Comments { above, after }
    }

    /// The comments written with a table's header: those right above it,
    /// its own, and the one after it on its line. Those set apart above
    /// them by a blank line close the table whose keys they follow in the
    /// file, [`ConfigDoc::closing_comments`], or open the file,
    /// [`ConfigDoc::head_comments`].
    /// An explicit `[profile]` header's go with the first profile, and a
    /// profile written as `name = { ... }` has those on its line.
    #[must_use]
    pub fn table_comments(&self, table: &Table) -> TableComments {
        let Table::Profile(name) = table else {
            return self
                .doc
                .get(&table.to_string())
                .and_then(Item::as_table)
                .map(|t| self.header_comments(t))
                .unwrap_or_default();
        };
        let mut found = TableComments::default();
        let Some(profiles) = self.doc.get("profile") else {
            return found;
        };
        // An explicit `[profile]`'s comments go with the first profile.
        if let Some(header) = profiles.as_table()
            && !header.is_implicit()
            && self.profiles().first() == Some(name)
        {
            found = self.header_comments(header);
        }
        let Some(profiles) = profiles.as_table_like() else {
            return found;
        };
        match profiles.get(name) {
            Some(Item::Table(t)) if !t.is_dotted() => {
                let own = self.header_comments(t);
                if let Some(after) = found.after.take() {
                    found.own.push(after);
                }
                found.own.extend(own.own);
                found.after = own.after;
            }
            Some(Item::Value(v)) => {
                if let Some(key) = profiles.key(name) {
                    found.own.extend(comment_lines(key.leaf_decor().prefix()));
                }
                let after = comment_lines(v.decor().suffix()).into_iter().next();
                found.after = found.after.or(after);
            }
            _ => {}
        }
        found
    }

    /// The comments set apart by a blank line after `table`'s keys in the
    /// file, before whatever header follows them, or at the end of the
    /// file: they close it, so they go with it, wherever the editor lists
    /// it.
    #[must_use]
    pub fn closing_comments(&self, table: &Table) -> Vec<String> {
        self.closing_of(&Closer::Table(table.clone()))
    }

    /// [`ConfigDoc::closing_comments`] of a table or key the config
    /// doesn't take, by its name.
    #[must_use]
    pub fn unknown_closing_comments(&self, name: &str) -> Vec<String> {
        self.closing_of(&Closer::Unknown(name.to_owned()))
    }

    fn closing_of(&self, closer: &Closer) -> Vec<String> {
        let headers = self.printed_headers();
        let mut closing = Vec::new();
        for at in 0..=headers.len() {
            if matches!(self.set_apart(&headers, at), SetApart::Closes(c) if c == *closer) {
                closing.extend(match headers.get(at) {
                    Some((_, next)) => comment_lines_in(profiles::split_prefix(next).0),
                    None => self.end_comments(),
                });
            }
        }
        closing
    }

    /// The comments the file ends with when they close none of its
    /// tables: it has none, or only an empty `[profile]`.
    #[must_use]
    pub fn loose_end_comments(&self) -> Vec<String> {
        let headers = self.printed_headers();
        match self.set_apart(&headers, headers.len()) {
            SetApart::Loose => self.end_comments(),
            _ => Vec::new(),
        }
    }

    /// Every header in the file, in the order it's printed, with whose it
    /// is. The order is `toml_edit`'s: by position, a table without one,
    /// just added, after the one it follows in the document.
    fn printed_headers(&self) -> Vec<(Owner, &toml_edit::Table)> {
        fn visit<'t>(
            table: &'t toml_edit::Table,
            path: &mut Vec<&'t str>,
            last: &mut isize,
            found: &mut Vec<(isize, Vec<&'t str>, &'t toml_edit::Table)>,
        ) {
            if !table.is_dotted() {
                if let Some(at) = table.position() {
                    *last = at;
                }
                if !table.is_implicit() && !path.is_empty() {
                    found.push((*last, path.clone(), table));
                }
            }
            for (key, item) in table {
                let tables: Vec<&toml_edit::Table> = match item {
                    Item::Table(t) => vec![t],
                    Item::ArrayOfTables(tables) => tables.iter().collect(),
                    _ => Vec::new(),
                };
                for t in tables {
                    path.push(key);
                    visit(t, path, last, found);
                    path.pop();
                }
            }
        }
        let mut found = Vec::new();
        visit(self.doc.as_table(), &mut Vec::new(), &mut 0, &mut found);
        found.sort_by_key(|(at, ..)| *at);
        found
            .into_iter()
            .filter_map(|(_, path, table)| {
                let owner = match path.as_slice() {
                    ["profile"] => Owner::Profiles,
                    ["profile", name, ..] => Owner::Is(Table::Profile((*name).to_owned())),
                    [first, ..] => match Table::SECTIONS
                        .into_iter()
                        .find(|s| s.to_string() == *first)
                    {
                        Some(section) if path.len() == 1 => Owner::Is(section),
                        Some(section) => Owner::Is(section),
                        None => Owner::Unknown((*first).to_owned()),
                    },
                    [] => return None,
                };
                Some((owner, table))
            })
            .collect()
    }

    /// What the comments set apart by a blank line above the header at
    /// `at` in `headers`, or past the last, at the end of the file, are.
    fn set_apart(&self, headers: &[(Owner, &toml_edit::Table)], at: usize) -> SetApart {
        let before = match at.checked_sub(1) {
            Some(prev) => self.keys_under(&headers[prev].0),
            None => self.keys_before_headers(),
        };
        let next = match headers.get(at) {
            Some((Owner::Is(t), _)) => Some(Closer::Table(t.clone())),
            Some((Owner::Unknown(name), _)) => Some(Closer::Unknown(name.clone())),
            _ => None,
        };
        match before {
            Some(t) if next.as_ref() == Some(&t) => SetApart::Own,
            Some(t) => SetApart::Closes(t),
            None if at == 0 || at == headers.len() => SetApart::Loose,
            None => SetApart::Own,
        }
    }

    /// The table whose keys follow a header of `owner`'s, up to the next.
    fn keys_under(&self, owner: &Owner) -> Option<Closer> {
        match owner {
            Owner::Is(t) => Some(Closer::Table(t.clone())),
            Owner::Unknown(name) => Some(Closer::Unknown(name.clone())),
            // Its profiles written on a line, or, with none, the first
            // profile, which its own comments go with.
            Owner::Profiles => {
                let profiles = self.doc.get("profile").and_then(Item::as_table)?;
                last_keyed(profiles)
                    .map(|(name, _)| name.to_owned())
                    .or_else(|| self.profiles().into_iter().next())
                    .map(|name| Closer::Table(Table::Profile(name)))
            }
        }
    }

    /// The table whose keys the file opens with, before any header.
    fn keys_before_headers(&self) -> Option<Closer> {
        let (name, item) = last_keyed(self.doc.as_table())?;
        if name == "profile" {
            let (profile, _) = last_keyed(item.as_table_like()?)?;
            return Some(Closer::Table(Table::Profile(profile.to_owned())));
        }
        Some(
            Table::SECTIONS
                .into_iter()
                .find(|s| s.to_string() == name)
                .map_or_else(|| Closer::Unknown(name.to_owned()), Closer::Table),
        )
    }

    /// The keys a table has that the config doesn't take, each as the file
    /// writes it, a line at a time, with its comments.
    #[must_use]
    pub fn unknown_keys(&self, table: &Table) -> Vec<RawLines> {
        let Some(found) = self.table(table) else {
            return Vec::new();
        };
        found
            .iter()
            .filter(|(name, _)| !table.fields().iter().any(|f| f.name == *name))
            .filter_map(|(name, item)| Some(self.raw_item(found.key(name)?, item)))
            .collect()
    }

    /// What's in the file that the config doesn't take at all, by name,
    /// each as the file writes it, a line at a time, with its comments.
    #[must_use]
    pub fn unknown_items(&self) -> Vec<(String, RawLines)> {
        let root = self.doc.as_table();
        root.iter()
            .filter(|(name, _)| {
                *name != "profile" && !Table::SECTIONS.iter().any(|s| s.to_string() == *name)
            })
            .filter_map(|(name, item)| {
                Some((name.to_owned(), self.raw_item(root.key(name)?, item)))
            })
            .collect()
    }

    /// `item`, under `key`, as the file writes it, less what's set apart
    /// above its headers that closes whatever's before them or opens the
    /// file: that shows there. What's set apart between two of its own
    /// parts stays with it.
    fn raw_item(&self, key: &toml_edit::Key, item: &Item) -> RawLines {
        fn own_only(
            doc: &ConfigDoc,
            headers: &[(Owner, &toml_edit::Table)],
            table: &toml_edit::Table,
            copy: &mut toml_edit::Table,
        ) {
            let elsewhere = headers
                .iter()
                .position(|(_, t)| std::ptr::eq(*t, table))
                .is_some_and(|at| !matches!(doc.set_apart(headers, at), SetApart::Own));
            if elsewhere {
                let own = profiles::split_prefix(table).1.to_owned();
                copy.decor_mut().set_prefix(own);
            }
            for (name, item) in copy.iter_mut() {
                match (table.get(name.get()), item) {
                    (Some(Item::Table(t)), Item::Table(c)) => own_only(doc, headers, t, c),
                    (Some(Item::ArrayOfTables(ts)), Item::ArrayOfTables(cs)) => {
                        for (t, c) in ts.iter().zip(cs.iter_mut()) {
                            own_only(doc, headers, t, c);
                        }
                    }
                    _ => {}
                }
            }
        }
        let headers = self.printed_headers();
        let mut copy = item.clone();
        match (item, &mut copy) {
            (Item::Table(t), Item::Table(c)) => own_only(self, &headers, t, c),
            (Item::ArrayOfTables(ts), Item::ArrayOfTables(cs)) => {
                for (t, c) in ts.iter().zip(cs.iter_mut()) {
                    own_only(self, &headers, t, c);
                }
            }
            _ => {}
        }
        let mut alone = DocumentMut::new();
        alone.insert_formatted(key, copy);
        let text = alone.to_string();
        split_comments(text.trim_matches('\n'))
            .into_iter()
            .filter(|(code, comment)| !code.trim().is_empty() || comment.is_some())
            .collect()
    }

    /// The comments around a header table's own header, with those set
    /// apart above them when they neither close another table nor open
    /// the file.
    fn header_comments(&self, header: &toml_edit::Table) -> TableComments {
        let (apart, own) = profiles::split_prefix(header);
        let headers = self.printed_headers();
        let mine = headers
            .iter()
            .position(|(_, t)| std::ptr::eq(*t, header))
            .is_some_and(|at| matches!(self.set_apart(&headers, at), SetApart::Own));
        let mut comments = if mine {
            comment_lines_in(apart)
        } else {
            Vec::new()
        };
        comments.extend(comment_lines_in(own));
        TableComments {
            own: comments,
            after: comment_lines(header.decor().suffix()).into_iter().next(),
        }
    }

    /// The comments the file opens with, set apart from its first table's
    /// own, before anything: what it says about itself.
    #[must_use]
    pub fn head_comments(&self) -> Vec<String> {
        let headers = self.printed_headers();
        match headers.first() {
            Some((_, first)) if matches!(self.set_apart(&headers, 0), SetApart::Loose) => {
                comment_lines_in(profiles::split_prefix(first).0)
            }
            _ => Vec::new(),
        }
    }

    /// The comments the file ends with, after its last key.
    #[must_use]
    pub fn end_comments(&self) -> Vec<String> {
        comment_lines_in(self.doc.trailing().as_str().unwrap_or_default())
    }

    /// The comments inside a list, a key's or a profile's `repos`: after
    /// its `[`, above and after each item, and before its `]`. A `repos`
    /// written as `[[profile.name.repos]]` tables has each table's.
    #[must_use]
    pub fn list_comments(&self, key: &Key) -> ListComments {
        let list = match self.table(&key.table).and_then(|t| t.get(key.name)) {
            Some(Item::Value(Value::Array(list))) => list,
            Some(Item::ArrayOfTables(tables)) => {
                return ListComments {
                    items: tables.iter().map(|t| self.entry_comments(t)).collect(),
                    ..ListComments::default()
                };
            }
            _ => return ListComments::default(),
        };
        let raw = |text: Option<&toml_edit::RawString>| {
            text.and_then(toml_edit::RawString::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        // What's between one item and the next, around the comma: its
        // first line ends the item before it, the rest is above the next.
        let values: Vec<&Value> = list.iter().collect();
        let mut gaps: Vec<String> = Vec::new();
        for (n, value) in values.iter().enumerate() {
            let before = n
                .checked_sub(1)
                .map(|p| raw(values[p].decor().suffix()))
                .unwrap_or_default();
            gaps.push(before + &raw(value.decor().prefix()));
        }
        let end = values
            .last()
            .map(|v| raw(v.decor().suffix()))
            .unwrap_or_default();
        gaps.push(end + list.trailing().as_str().unwrap_or_default());
        let split = |text: &str| -> (Option<String>, Vec<String>) {
            let (line, rest) = text.split_once('\n').unwrap_or((text, ""));
            (
                comment_lines_in(line).into_iter().next(),
                comment_lines_in(rest),
            )
        };
        let mut split: Vec<_> = gaps.iter().map(|gap| split(gap)).collect();
        let (opening, mut above) = split.remove(0);
        let mut items = Vec::new();
        for (after, next_above) in split {
            items.push(Comments {
                above: std::mem::replace(&mut above, next_above),
                after,
            });
        }
        // An empty list's only gap is its `[`'s line and what's before `]`.
        ListComments {
            opening,
            items,
            closing: above,
        }
    }

    /// The comments on a `[[...repos]]` table: its header's, and its keys'.
    fn entry_comments(&self, table: &toml_edit::Table) -> Comments {
        let header = self.header_comments(table);
        let mut above = header.own;
        for (name, item) in table {
            if let Some(key) = table.key(name) {
                above.extend(comment_lines(key.leaf_decor().prefix()));
            }
            if let Some(value) = item.as_value() {
                above.extend(comment_lines(value.decor().suffix()));
            }
        }
        Comments {
            above,
            after: header.after,
        }
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

/// What [`save`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub text: String,
    /// Someone else wrote the file since it was read, so the edits were
    /// made again on what they wrote.
    pub replayed: bool,
}

/// Writes `doc` to `path`, taking a turn with the file's other editors. If
/// the file changed since `doc` read it, the edits are made again on it as
/// it is now. Either way, it's written only if it loads.
pub fn save(path: &Path, doc: &ConfigDoc, resolver: &dyn CheckoutResolver) -> Result<Saved> {
    let _turn = take_turn();
    let now = Config::read_text(path)?;
    let replayed = now.as_deref() != doc.source();
    let doc = if replayed {
        doc.replay_onto(now.as_deref())?
    } else {
        doc.clone()
    };
    doc.load(path, resolver)
        .map_err(|Unloadable(err)| err)
        .wrap_err("the config wouldn't load, so it wasn't written")?;
    let text = doc.text();
    write_atomically(path, &text)?;
    Ok(Saved { text, replayed })
}

/// Lines of TOML as the file writes them, each split into what comes
/// before its comment and the comment.
pub type RawLines = Vec<(String, Option<String>)>;

/// A table, or something the config doesn't take, that comments can
/// close.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Closer {
    Table(Table),
    Unknown(String),
}

/// Whose a header is.
enum Owner {
    /// A table the config doesn't take, by its top-level name.
    Unknown(String),
    /// A table's own, or one inside it, such as a `[[profile.x.repos]]`
    /// entry.
    Is(Table),
    /// An explicit `[profile]`.
    Profiles,
}

/// What comments set apart by a blank line above a header, or at the end
/// of the file, are.
enum SetApart {
    /// They close the table whose keys they follow.
    Closes(Closer),
    /// They're between two parts of the header's table, or follow only a
    /// `[profile]` with no profiles: the header's own.
    Own,
    /// They open the file, or end it after nothing they could close.
    Loose,
}

/// The last key in `table` written as one, not under a header of its own.
fn last_keyed(table: &dyn TableLike) -> Option<(&str, &Item)> {
    table
        .iter()
        .filter(|(_, item)| {
            item.is_value() || item.as_table().is_some_and(toml_edit::Table::is_dotted)
        })
        .last()
}

/// The comments written with a table's header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableComments {
    pub own: Vec<String>,
    pub after: Option<String>,
}

/// The comments inside a list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListComments {
    /// After its `[`.
    pub opening: Option<String>,
    /// Each item's.
    pub items: Vec<Comments>,
    /// Before its `]`.
    pub closing: Vec<String>,
}

impl ListComments {
    #[must_use]
    pub fn any(&self) -> bool {
        self.opening.is_some()
            || !self.closing.is_empty()
            || self
                .items
                .iter()
                .any(|c| !c.above.is_empty() || c.after.is_some())
    }
}

/// The comments a key or table carries in the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comments {
    pub above: Vec<String>,
    pub after: Option<String>,
}

/// The comments in a decor, one per line, without their `#`.
fn comment_lines(text: Option<&toml_edit::RawString>) -> Vec<String> {
    comment_lines_in(
        text.and_then(toml_edit::RawString::as_str)
            .unwrap_or_default(),
    )
}

/// Each line of TOML `text`, split into what comes before its comment and
/// the comment, without its `#`: strings, multi-line ones included, can
/// hold a `#` that isn't one.
#[must_use]
pub fn split_comments(text: &str) -> Vec<(String, Option<String>)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum In {
        Code,
        Basic,
        Literal,
        MultiBasic,
        MultiLiteral,
    }
    let mut state = In::Code;
    let mut lines = Vec::new();
    for line in text.split('\n') {
        // A one-line string ends with its line.
        if matches!(state, In::Basic | In::Literal) {
            state = In::Code;
        }
        let mut hash = None;
        let mut chars = line.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            let rest = &line[at..];
            match state {
                In::Code if c == '#' => {
                    hash = Some(at);
                    break;
                }
                In::Code if rest.starts_with("\"\"\"") => {
                    state = In::MultiBasic;
                    chars.nth(1);
                }
                In::Code if rest.starts_with("'''") => {
                    state = In::MultiLiteral;
                    chars.nth(1);
                }
                In::Code if c == '"' => state = In::Basic,
                In::Code if c == '\'' => state = In::Literal,
                In::Basic | In::MultiBasic if c == '\\' => {
                    chars.next();
                }
                In::Basic if c == '"' => state = In::Code,
                In::Literal if c == '\'' => state = In::Code,
                // Up to two quotes of its own can come right before the
                // three that end it, as in `"""a""""`.
                In::MultiBasic if rest.starts_with("\"\"\"") => {
                    state = In::Code;
                    chars.nth(closing_quotes(rest, '"') - 2);
                }
                In::MultiLiteral if rest.starts_with("'''") => {
                    state = In::Code;
                    chars.nth(closing_quotes(rest, '\'') - 2);
                }
                _ => {}
            }
        }
        lines.push(match hash {
            Some(at) => {
                let comment = &line[at + 1..];
                let comment = comment.strip_prefix(' ').unwrap_or(comment).trim_end();
                (line[..at].trim_end().to_owned(), Some(comment.to_owned()))
            }
            None => (line.to_owned(), None),
        });
    }
    lines
}

/// How many of the `quote`s `rest` starts with end a multi-line string:
/// its last three, and up to two before them that are in it.
fn closing_quotes(rest: &str, quote: char) -> usize {
    rest.chars().take_while(|&c| c == quote).count().min(5)
}

fn comment_lines_in(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix('#'))
        .map(|comment| comment.strip_prefix(' ').unwrap_or(comment).to_owned())
        .collect()
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
    fn saving_writes_or_replays_onto_what_changed_and_refuses_what_wont_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let text = "# Mine.\n[profile.p]\nrepos = [{ github = \"org\" }]\n";
        std::fs::write(&path, text).unwrap();
        let mut doc = ConfigDoc::parse(Config::read_text(&path).unwrap().as_deref()).unwrap();
        doc.apply(Op::Set {
            key: key(Table::Poll, "quiet_secs"),
            value: Scalar::Number(30),
        })
        .unwrap();
        let saved = save(&path, &doc, &NoCheckouts).unwrap();
        assert!(!saved.replayed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved.text);
        assert!(saved.text.contains("quiet_secs = 30"));

        // Changed by hand meanwhile: both edits stay.
        std::fs::write(&path, format!("# by hand\n{text}")).unwrap();
        let saved = save(&path, &doc, &NoCheckouts).unwrap();
        assert!(saved.replayed);
        assert!(saved.text.starts_with("# by hand\n"), "{}", saved.text);
        assert!(saved.text.contains("quiet_secs = 30"));

        // What wouldn't load isn't written.
        doc.apply(Op::Set {
            key: key(Table::Poll, "reconcile_secs"),
            value: Scalar::Number(0),
        })
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let err = save(&path, &doc, &NoCheckouts).unwrap_err();
        assert!(format!("{err:#}").contains("wouldn't load"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // A new file is written with its header.
        let new = dir.path().join("new/config.toml");
        let mut doc = ConfigDoc::parse(None).unwrap();
        doc.apply(Op::AddProfile { name: "p".into() }).unwrap();
        doc.apply(Op::PushEntry {
            profile: "p".into(),
            entry: RepoEntry::Github {
                name: "org".into(),
                paths: Vec::new(),
            },
        })
        .unwrap();
        let saved = save(&new, &doc, &NoCheckouts).unwrap();
        assert!(!saved.replayed);
        assert!(saved.text.starts_with(NEW_FILE_HEADER));
    }

    #[test]
    fn comments_are_read_from_above_keys_tables_and_after_values() {
        let doc = doc();
        let model = key(Table::Runner, "model");
        assert_eq!(
            doc.comments(&model),
            Comments {
                above: vec!["Reviews are expensive.".into()],
                after: Some("for now".into()),
            }
        );
        assert_eq!(
            doc.comments(&key(Table::Runner, "max_concurrent")),
            Comments::default()
        );
        assert_eq!(doc.table_comments(&Table::Runner).own, ["Mine."]);
        assert!(doc.head_comments().is_empty());
        // A blank line sets what the file opens with apart from the first
        // table's own.
        let doc = ConfigDoc::parse(Some("# About.\n\n# Its own.\n[runner]\n")).unwrap();
        assert_eq!(doc.head_comments(), ["About."]);
        assert_eq!(doc.table_comments(&Table::Runner).own, ["Its own."]);
        assert!(
            doc.closing_comments(&Table::Runner).is_empty(),
            "nothing closes it"
        );
        assert_eq!(
            doc.table_comments(&profile("ring")),
            TableComments::default()
        );
    }

    #[test]
    fn every_comment_is_read_wherever_it_sits() {
        let doc = ConfigDoc::parse(Some(
            "# About.\n\n[runner] # the runner\nmodel = \"m\"\n# closing runner\n\n# above poll\n[poll]\nquiet_secs = 1\n\n[profile.p]\nrepos = [ # the repos\n  # about a\n  \"a\", # on a\n  \"b\",\n  # more later\n]\n# the end\n",
        ))
        .unwrap();
        assert_eq!(doc.head_comments(), ["About."]);
        assert_eq!(
            doc.table_comments(&Table::Runner),
            TableComments {
                own: Vec::new(),
                after: Some("the runner".into()),
            }
        );
        // Set apart from the next header, they close the table before it.
        assert_eq!(doc.closing_comments(&Table::Runner), ["closing runner"]);
        assert_eq!(
            doc.table_comments(&Table::Poll),
            TableComments {
                own: vec!["above poll".into()],
                after: None,
            }
        );
        // The file's last table ends with what the file ends with.
        assert_eq!(doc.closing_comments(&profile("p")), ["the end"]);
        let repos = doc.list_comments(&key(profile("p"), "repos"));
        assert_eq!(repos.opening.as_deref(), Some("the repos"));
        assert_eq!(repos.items[0].above, ["about a"]);
        assert_eq!(repos.items[0].after.as_deref(), Some("on a"));
        assert_eq!(repos.items[1], Comments::default());
        assert_eq!(repos.closing, ["more later"]);
        assert!(repos.any());
        assert_eq!(doc.end_comments(), ["the end"]);
    }

    #[test]
    fn comments_at_a_lists_ends_are_read_with_or_without_a_comma() {
        let repos = |text: &str| {
            ConfigDoc::parse(Some(text))
                .unwrap()
                .list_comments(&key(profile("p"), "repos"))
        };
        for comma in ["", ","] {
            let found = repos(&format!(
                "[profile.p]\nrepos = [\n  \"a\" # on a\n  , \"b\"{comma} # on b\n  # closing\n]\n"
            ));
            assert_eq!(found.items[0].after.as_deref(), Some("on a"), "{comma:?}");
            assert_eq!(found.items[1].after.as_deref(), Some("on b"), "{comma:?}");
            assert_eq!(found.closing, ["closing"], "{comma:?}");
        }
        let empty = repos("[profile.p]\nrepos = [ # none\n  # yet\n]\n");
        assert_eq!(empty.opening.as_deref(), Some("none"));
        assert_eq!(empty.closing, ["yet"]);
        let tables = repos(
            "[profile.p]\n# about a\n[[profile.p.repos]] # on a\n# its path\npath = \"a\" # here\n",
        );
        assert_eq!(
            tables.items,
            [Comments {
                above: vec!["about a".into(), "its path".into(), "here".into()],
                after: Some("on a".into()),
            }]
        );
    }

    #[test]
    fn a_profile_tables_comments_go_with_its_first_profile() {
        let doc = ConfigDoc::parse(Some(
            "[runner]\n\n# about profiles\n\n# own\n[profile] # after\n# above p\np.repos = [\"a\"]\n# above q\nq = { repos = [\"b\"] } # after q\n",
        ))
        .unwrap();
        assert_eq!(
            doc.table_comments(&profile("p")),
            TableComments {
                own: vec!["own".into()],
                after: Some("after".into()),
            }
        );
        assert_eq!(doc.closing_comments(&Table::Runner), ["about profiles"]);
        assert_eq!(doc.comments(&key(profile("p"), "repos")).above, ["above p"]);
        assert_eq!(
            doc.table_comments(&profile("q")),
            TableComments {
                own: vec!["above q".into()],
                after: Some("after q".into()),
            }
        );
    }

    #[test]
    fn set_apart_comments_close_the_table_whose_keys_they_follow() {
        let parse = |text: &str| ConfigDoc::parse(Some(text)).unwrap();
        let repos = |doc: &ConfigDoc| doc.list_comments(&key(profile("a"), "repos")).items;
        // Profiles under an explicit `[profile]`, on a line or dotted.
        for a in ["a = { repos = [] }", "a.repos = []"] {
            let doc = parse(&format!("[runner]\n[profile]\n{a}\n\n# the end\n"));
            assert_eq!(doc.closing_comments(&profile("a")), ["the end"], "{a}");
            assert!(doc.loose_end_comments().is_empty(), "{a}");
            let doc = parse(&format!("[profile]\n{a}\n# closes a\n\n[runner]\n"));
            assert_eq!(doc.closing_comments(&profile("a")), ["closes a"], "{a}");
            assert!(doc.table_comments(&Table::Runner).own.is_empty(), "{a}");
        }
        // A `[profile]` with none on a line: its first profile's, shown
        // once, as its own right under it and closing it otherwise.
        let doc = parse("[profile]\n# c\n\n[profile.a]\n");
        assert_eq!(doc.table_comments(&profile("a")).own, ["c"]);
        assert!(doc.closing_comments(&profile("a")).is_empty());
        let doc = parse("[profile]\n# c\n\n[runner]\n# d\n\n[profile.a]\n");
        assert_eq!(doc.closing_comments(&profile("a")), ["c"]);
        assert_eq!(doc.closing_comments(&Table::Runner), ["d"]);
        assert!(doc.table_comments(&profile("a")).own.is_empty());
        // Keys before any header.
        let doc = parse("profile.a.repos = []\n# x\n\n[runner]\n");
        assert!(doc.head_comments().is_empty());
        assert_eq!(doc.closing_comments(&profile("a")), ["x"]);
        let doc = parse("profile.a.repos = []\n\n# the end\n");
        assert_eq!(doc.closing_comments(&profile("a")), ["the end"]);
        assert!(doc.loose_end_comments().is_empty());
        assert_eq!(parse("# only\n").loose_end_comments(), ["only"]);
        // A `[[repos]]` entry's are its own only after its profile's.
        let doc = parse("[runner]\n# c\n\n[[profile.a.repos]]\ngithub = \"o\"\n");
        assert_eq!(doc.closing_comments(&Table::Runner), ["c"]);
        assert!(repos(&doc)[0].above.is_empty());
        let doc = parse(
            "[[profile.a.repos]]\ngithub = \"o\"\n# c\n\n[[profile.a.repos]]\ngithub = \"p\"\n",
        );
        assert_eq!(repos(&doc)[1].above, ["c"]);
        assert!(doc.closing_comments(&profile("a")).is_empty());
        // A section just added prints last, so it's the one the end closes.
        let mut doc = parse("# head\n\n[runner]\nmodel = \"m\"\n# the end\n\n");
        doc.apply(Op::Set {
            key: key(Table::Github, "api_url"),
            value: Scalar::Text("x".into()),
        })
        .unwrap();
        assert_eq!(doc.head_comments(), ["head"]);
        assert!(doc.closing_comments(&Table::Runner).is_empty());
        assert_eq!(doc.closing_comments(&Table::Github), ["the end"]);
    }

    #[test]
    fn text_that_isnt_toml_is_refused() {
        let err = ConfigDoc::parse(Some("[runner")).unwrap_err();
        assert!(err.to_string().contains("valid TOML"), "{err}");
    }
}
