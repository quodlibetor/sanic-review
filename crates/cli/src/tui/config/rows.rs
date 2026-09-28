//! The rows the editor lists for the config, or for a repo entry it has
//! open, and what each shows.

use crate::config_doc::{
    ConfigDoc, EntryKind, Key, RepoEntry, Scalar, Setting, Table,
    schema::{Fallback, Kind},
};

/// A row of the config, as a file lists it: a table's header, then its
/// keys, a key on one row or a list over one row per item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// `[github]`, or `[profile.x]`, which Enter renames.
    Header(Table),
    Scalar(Key),
    /// A list's `n`th item.
    Item(Key, usize),
    /// An unset, empty or invalid list's one row.
    List(Key),
    /// The item being added to a list.
    NewItem(Key),
    /// A profile's `n`th repo entry.
    Entry(String, usize),
    /// A profile's `repos` with no entries, or one that isn't a list.
    NoEntries(String),
    /// After the last table, where `+` adds a profile.
    NewProfile,
}

impl Row {
    /// The key the row belongs to, if any.
    pub fn key(&self) -> Option<Key> {
        match self {
            Self::Scalar(key) | Self::Item(key, _) | Self::List(key) | Self::NewItem(key) => {
                Some(key.clone())
            }
            Self::Entry(profile, _) | Self::NoEntries(profile) => {
                Key::new(Table::Profile(profile.clone()), "repos")
            }
            Self::Header(_) | Self::NewProfile => None,
        }
    }
}

/// Every table's rows in turn, each under its header, with a row for
/// `adding` after its list's items, then [`Row::NewProfile`].
pub fn all_rows(doc: &ConfigDoc, tables: &[Table], adding: Option<&Key>) -> Vec<Row> {
    let mut all = Vec::new();
    for table in tables {
        all.push(Row::Header(table.clone()));
        all.extend(rows(doc, table, adding));
    }
    all.push(Row::NewProfile);
    all
}

/// `table`'s rows, with a row for `adding` after its items.
pub fn rows(doc: &ConfigDoc, table: &Table, adding: Option<&Key>) -> Vec<Row> {
    let mut rows = Vec::new();
    for field in table.fields() {
        let Some(key) = Key::new(table.clone(), field.name) else {
            continue;
        };
        match field.kind {
            Kind::Text | Kind::Number | Kind::Bool => rows.push(Row::Scalar(key)),
            Kind::List => {
                match doc.list(&key) {
                    Setting::Set(items) if !items.is_empty() => {
                        rows.extend((0..items.len()).map(|n| Row::Item(key.clone(), n)));
                    }
                    _ => rows.push(Row::List(key.clone())),
                }
                if adding == Some(&key) {
                    rows.push(Row::NewItem(key));
                }
            }
            Kind::Repos => {
                let Table::Profile(name) = table else {
                    continue;
                };
                match doc.repos(name) {
                    Setting::Set(repos) if !repos.entries.is_empty() => {
                        rows.extend((0..repos.entries.len()).map(|n| Row::Entry(name.clone(), n)));
                    }
                    _ => rows.push(Row::NoEntries(name.clone())),
                }
            }
        }
    }
    rows
}

/// What a key shows in the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    Set(String),
    /// Unset: what it means then, from its default or the key it inherits.
    Default(String),
    Invalid(String),
    Required,
}

/// What `key` shows as a whole: its value, or what it means unset.
pub fn shown(doc: &ConfigDoc, key: &Key) -> Shown {
    let field = key.field();
    let written = match field.kind {
        Kind::List => match doc.list(key) {
            Setting::Set(items) if items.is_empty() => Setting::Set("[]".into()),
            Setting::Set(items) => Setting::Set(items.join(", ")),
            Setting::Unset => Setting::Unset,
            Setting::Invalid(raw) => Setting::Invalid(raw),
        },
        Kind::Repos => match &key.table {
            Table::Profile(name) => match doc.repos(name) {
                Setting::Set(repos) if repos.entries.is_empty() => Setting::Unset,
                Setting::Set(_) => Setting::Set(String::new()),
                Setting::Unset => Setting::Unset,
                Setting::Invalid(raw) => Setting::Invalid(raw),
            },
            _ => Setting::Unset,
        },
        Kind::Text | Kind::Number | Kind::Bool => match doc.scalar(key) {
            Setting::Set(value) => Setting::Set(scalar_text(&value)),
            Setting::Unset => Setting::Unset,
            Setting::Invalid(raw) => Setting::Invalid(raw),
        },
    };
    match written {
        Setting::Set(value) => Shown::Set(value),
        Setting::Invalid(raw) => Shown::Invalid(raw),
        Setting::Unset => match &field.fallback {
            Fallback::Value(value) => Shown::Default(value()),
            Fallback::Nothing if field.kind == Kind::Bool => Shown::Default("false".into()),
            Fallback::Nothing => Shown::Default("none".into()),
            Fallback::Inherits(table, name) => {
                Key::new(table.clone(), name).map_or(Shown::Required, |inherited| {
                    match shown(doc, &inherited) {
                        Shown::Set(value) | Shown::Default(value) => Shown::Default(value),
                        other => other,
                    }
                })
            }
            Fallback::Required => Shown::Required,
        },
    }
}

/// A list's items, or none when it isn't a list of strings.
pub fn items(doc: &ConfigDoc, key: &Key) -> Vec<String> {
    match doc.list(key) {
        Setting::Set(items) => items,
        Setting::Unset | Setting::Invalid(_) => Vec::new(),
    }
}

/// A profile's entries, each as read or as written when the editor can't
/// read it.
pub fn entries(doc: &ConfigDoc, profile: &str) -> Vec<Result<RepoEntry, String>> {
    match doc.repos(profile) {
        Setting::Set(repos) => repos.entries,
        Setting::Unset | Setting::Invalid(_) => Vec::new(),
    }
}

pub fn scalar_text(value: &Scalar) -> String {
    match value {
        Scalar::Text(text) => text.clone(),
        Scalar::Number(n) => n.to_string(),
        Scalar::Bool(b) => b.to_string(),
    }
}

/// An entry as the file writes it, on one line.
pub fn entry_text(entry: &RepoEntry) -> String {
    let mut value = entry.to_value();
    value.decor_mut().clear();
    value.to_string()
}

/// A repo entry open for editing, in place of its profile's keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryEdit {
    pub profile: String,
    pub entry: RepoEntry,
    /// Whether it's in the file yet: a new one goes in once it names a
    /// checkout or a repo.
    pub in_doc: bool,
    pub row: usize,
    /// The checkout path while it's a `github` entry, or the `github` name
    /// while it's a checkout, so switching back finds it again.
    pub set_aside: String,
    /// The globs while it's a plain checkout, likewise.
    pub globs_aside: Vec<String>,
}

/// A row of an open repo entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryRow {
    Kind,
    /// Its checkout path, or its `github` org or repo.
    Target,
    Remote,
    Glob(usize),
    /// Where globs go when there are none.
    NoGlobs,
    /// The glob being added.
    NewGlob,
}

impl EntryEdit {
    pub fn rows(&self, adding: bool) -> Vec<EntryRow> {
        let mut rows = vec![EntryRow::Kind, EntryRow::Target];
        if self.entry.kind() != EntryKind::Github {
            rows.push(EntryRow::Remote);
        }
        let globs = self.globs();
        if self.entry.kind() != EntryKind::Checkout {
            if globs.is_empty() {
                rows.push(EntryRow::NoGlobs);
            }
            rows.extend((0..globs.len()).map(EntryRow::Glob));
            if adding {
                rows.push(EntryRow::NewGlob);
            }
        }
        rows
    }

    pub fn current(&self, adding: bool) -> EntryRow {
        let rows = self.rows(adding);
        rows.get(self.row)
            .or(rows.last())
            .copied()
            .unwrap_or(EntryRow::Kind)
    }

    pub fn globs(&self) -> &[String] {
        match &self.entry {
            RepoEntry::Scoped { paths, .. } | RepoEntry::Github { paths, .. } => paths,
            RepoEntry::Checkout { .. } => &[],
        }
    }

    pub fn target(&self) -> &str {
        match &self.entry {
            RepoEntry::Checkout { path, .. } | RepoEntry::Scoped { path, .. } => path,
            RepoEntry::Github { name, .. } => name,
        }
    }

    pub fn remote(&self) -> Option<&str> {
        match &self.entry {
            RepoEntry::Checkout { remote, .. } | RepoEntry::Scoped { remote, .. } => {
                remote.as_deref()
            }
            RepoEntry::Github { .. } => None,
        }
    }

    /// The entry with `change` made to its target, remote and globs.
    pub fn changed(
        &self,
        change: impl FnOnce(&mut String, &mut Option<String>, &mut Vec<String>),
    ) -> RepoEntry {
        let mut entry = self.entry.clone();
        let mut no_remote = None;
        let mut no_globs = Vec::new();
        let (target, remote, globs) = match &mut entry {
            RepoEntry::Checkout { path, remote } => (path, remote, &mut no_globs),
            RepoEntry::Scoped {
                path,
                paths,
                remote,
            } => (path, remote, paths),
            RepoEntry::Github { name, paths } => (name, &mut no_remote, paths),
        };
        change(target, remote, globs);
        entry
    }
}

/// The kinds in the order Space steps through them.
pub const KINDS: [EntryKind; 3] = [EntryKind::Checkout, EntryKind::Scoped, EntryKind::Github];

pub fn kind_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Checkout => "checkout",
        EntryKind::Scoped => "checkout + paths",
        EntryKind::Github => "github",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"[review_requests]
teams = ["*", "!org/x"]

[profile.p]
skills = []
repos = [{ github = "org" }, { repo = "~/s", paths = ["/v/**"] }]
"#;

    #[test]
    fn lists_get_a_row_per_item_or_one_when_empty() {
        let doc = ConfigDoc::parse(Some(CONFIG)).unwrap();
        let teams = Key::new(Table::ReviewRequests, "teams").unwrap();
        let titles = Key::new(Table::ReviewRequests, "skip_titles").unwrap();
        let drafts = Key::new(Table::ReviewRequests, "skip_drafts").unwrap();
        assert_eq!(
            rows(&doc, &Table::ReviewRequests, Some(&titles)),
            [
                Row::Item(teams.clone(), 0),
                Row::Item(teams, 1),
                Row::List(titles.clone()),
                Row::NewItem(titles),
                Row::Scalar(drafts),
            ]
        );
        let profile = rows(&doc, &Table::Profile("p".into()), None);
        assert_eq!(
            profile[profile.len() - 2..],
            [Row::Entry("p".into(), 0), Row::Entry("p".into(), 1)]
        );
        // The whole config: each table under its header, then the row
        // where a profile's added.
        let tables = [Table::Github, Table::Profile("p".into())];
        let all = all_rows(&doc, &tables, None);
        assert_eq!(all[0], Row::Header(Table::Github));
        assert_eq!(
            all.iter().filter(|r| matches!(r, Row::Header(_))).count(),
            2
        );
        assert_eq!(all.last(), Some(&Row::NewProfile));
    }

    #[test]
    fn entries_show_the_rows_their_kind_takes() {
        let mut edit = EntryEdit {
            profile: "p".into(),
            entry: RepoEntry::Checkout {
                path: "~/s".into(),
                remote: None,
            },
            in_doc: true,
            row: 0,
            set_aside: String::new(),
            globs_aside: Vec::new(),
        };
        assert_eq!(
            edit.rows(false),
            [EntryRow::Kind, EntryRow::Target, EntryRow::Remote]
        );
        edit.entry = edit.entry.clone().with_kind(EntryKind::Github, None);
        assert_eq!(
            edit.rows(true),
            [
                EntryRow::Kind,
                EntryRow::Target,
                EntryRow::NoGlobs,
                EntryRow::NewGlob
            ]
        );
        let changed = edit.changed(|name, _, globs| {
            name.push_str("o/s");
            globs.push("x".into());
        });
        assert_eq!(
            changed,
            RepoEntry::Github {
                name: "o/s".into(),
                paths: vec!["x".into()],
            }
        );
    }
}
