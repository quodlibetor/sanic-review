//! One entry of a profile's `repos`, in the three shapes the loader takes.

use toml_edit::{Array, InlineTable, Value};

/// A `repos` entry as written, before any path is expanded or remote
/// discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoEntry {
    /// `"path"`, or `{ repo = "path", remote = "..." }`.
    Checkout {
        path: String,
        remote: Option<String>,
    },
    /// `{ repo = "path", paths = [...] }`, optionally with `remote`.
    Scoped {
        path: String,
        paths: Vec<String>,
        remote: Option<String>,
    },
    /// `{ github = "org" | "owner/name" }`, optionally with `paths`.
    Github { name: String, paths: Vec<String> },
}

/// Which shape an entry has, for switching between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Checkout,
    Scoped,
    Github,
}

impl RepoEntry {
    /// `None` for a value in none of the three shapes, including one with
    /// a key the loader doesn't take, so it isn't rewritten without it.
    #[must_use]
    pub fn read(value: &Value) -> Option<Self> {
        let table = match value {
            Value::String(path) => {
                return Some(Self::Checkout {
                    path: path.value().clone(),
                    remote: None,
                });
            }
            Value::InlineTable(table) => table,
            _ => return None,
        };
        let text = |key: &str| match table.get(key) {
            None => Some(None),
            Some(value) => value.as_str().map(|s| Some(s.to_owned())),
        };
        let paths = match table.get("paths") {
            None => None,
            Some(value) => Some(
                value
                    .as_array()?
                    .iter()
                    .map(|p| p.as_str().map(String::from))
                    .collect::<Option<Vec<_>>>()?,
            ),
        };
        if table
            .iter()
            .any(|(key, _)| !["repo", "github", "paths", "remote"].contains(&key))
        {
            return None;
        }
        match (text("repo")?, text("github")?) {
            (Some(path), None) => {
                let remote = text("remote")?;
                Some(match paths {
                    Some(paths) => Self::Scoped {
                        path,
                        paths,
                        remote,
                    },
                    None => Self::Checkout { path, remote },
                })
            }
            (None, Some(name)) if !table.contains_key("remote") => Some(Self::Github {
                name,
                paths: paths.unwrap_or_default(),
            }),
            _ => None,
        }
    }

    /// The entry as the file writes it: a plain checkout is a string,
    /// everything else an inline table.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut table = InlineTable::new();
        match self {
            Self::Checkout { path, remote: None } => return path.as_str().into(),
            Self::Checkout {
                path,
                remote: Some(remote),
            } => {
                table.insert("repo", path.as_str().into());
                table.insert("remote", remote.as_str().into());
            }
            Self::Scoped {
                path,
                paths,
                remote,
            } => {
                table.insert("repo", path.as_str().into());
                table.insert("paths", globs(paths));
                if let Some(remote) = remote {
                    table.insert("remote", remote.as_str().into());
                }
            }
            Self::Github { name, paths } => {
                table.insert("github", name.as_str().into());
                if !paths.is_empty() {
                    table.insert("paths", globs(paths));
                }
            }
        }
        table.into()
    }

    #[must_use]
    pub fn kind(&self) -> EntryKind {
        match self {
            Self::Checkout { .. } => EntryKind::Checkout,
            Self::Scoped { .. } => EntryKind::Scoped,
            Self::Github { .. } => EntryKind::Github,
        }
    }

    /// This entry as `kind`, keeping what carries over: a checkout's path
    /// and remote, and the globs. A checkout becoming a `github` entry
    /// takes `repo`, the name its remote gave, when it's known; a `github`
    /// entry becoming a checkout starts with no path.
    #[must_use]
    pub fn with_kind(self, kind: EntryKind, repo: Option<&str>) -> Self {
        let (path, paths, remote, name) = match self {
            Self::Checkout { path, remote } => (path, Vec::new(), remote, None),
            Self::Scoped {
                path,
                paths,
                remote,
            } => (path, paths, remote, None),
            Self::Github { name, paths } => (String::new(), paths, None, Some(name)),
        };
        match kind {
            EntryKind::Checkout => Self::Checkout { path, remote },
            EntryKind::Scoped => Self::Scoped {
                path,
                paths,
                remote,
            },
            EntryKind::Github => Self::Github {
                name: name.or_else(|| repo.map(String::from)).unwrap_or_default(),
                paths,
            },
        }
    }
}

fn globs(paths: &[String]) -> Value {
    Value::Array(paths.iter().map(String::as_str).collect::<Array>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> Value {
        text.parse().unwrap()
    }

    #[test]
    fn entries_read_and_write_in_each_shape() {
        for (text, entry) in [
            (
                r#""~/src/a""#,
                RepoEntry::Checkout {
                    path: "~/src/a".into(),
                    remote: None,
                },
            ),
            (
                r#"{ repo = "~/src/a", remote = "upstream" }"#,
                RepoEntry::Checkout {
                    path: "~/src/a".into(),
                    remote: Some("upstream".into()),
                },
            ),
            (
                r#"{ repo = "~/src/a", paths = ["/v/**"] }"#,
                RepoEntry::Scoped {
                    path: "~/src/a".into(),
                    paths: vec!["/v/**".into()],
                    remote: None,
                },
            ),
            (
                r#"{ github = "org" }"#,
                RepoEntry::Github {
                    name: "org".into(),
                    paths: Vec::new(),
                },
            ),
            (
                r#"{ github = "o/r", paths = ["x"] }"#,
                RepoEntry::Github {
                    name: "o/r".into(),
                    paths: vec!["x".into()],
                },
            ),
        ] {
            assert_eq!(RepoEntry::read(&value(text)), Some(entry.clone()), "{text}");
            assert_eq!(entry.to_value().to_string().trim(), text);
        }
        // A plain checkout written as a table reads the same.
        assert_eq!(
            RepoEntry::read(&value(r#"{ repo = "a" }"#)),
            RepoEntry::read(&value(r#""a""#))
        );
    }

    #[test]
    fn shapes_the_loader_refuses_are_not_entries() {
        for text in [
            "3",
            r#"{ repo = "a", typo = 1 }"#,
            r#"{ repo = "a", github = "o" }"#,
            r#"{ github = "o", remote = "r" }"#,
            r#"{ repo = "a", paths = "x" }"#,
            "{ repo = 1 }",
            "{}",
        ] {
            assert_eq!(RepoEntry::read(&value(text)), None, "{text}");
        }
    }

    #[test]
    fn switching_kinds_keeps_what_carries_over() {
        let scoped = RepoEntry::Scoped {
            path: "~/s".into(),
            paths: vec!["/v/**".into()],
            remote: Some("up".into()),
        };
        assert_eq!(
            scoped.clone().with_kind(EntryKind::Checkout, None),
            RepoEntry::Checkout {
                path: "~/s".into(),
                remote: Some("up".into()),
            }
        );
        assert_eq!(
            scoped.with_kind(EntryKind::Github, Some("o/s")),
            RepoEntry::Github {
                name: "o/s".into(),
                paths: vec!["/v/**".into()],
            }
        );
        let github = RepoEntry::Github {
            name: "org".into(),
            paths: Vec::new(),
        };
        assert_eq!(
            github.with_kind(EntryKind::Scoped, None),
            RepoEntry::Scoped {
                path: String::new(),
                paths: Vec::new(),
                remote: None,
            }
        );
    }
}
