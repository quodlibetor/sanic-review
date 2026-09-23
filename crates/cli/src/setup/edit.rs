//! Applying `setup` choices to a config document, keeping its comments and
//! everything setup doesn't manage.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, bail};
use sanic_core::{
    config::{contract_path, expand_path},
    pr::TeamRef,
};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use crate::config_edit::{new_table, set_value};

/// What the user picked. Each list holds every option offered, with whether
/// it was selected; options that weren't offered are left alone.
#[derive(Debug, Default)]
pub struct Selections {
    pub teams: Vec<(TeamRef, bool)>,
    /// Discovered checkouts.
    pub repos: Vec<(PathBuf, bool)>,
    /// Profile that newly selected checkouts are added to.
    pub repo_profile: String,
    pub orgs: Vec<(String, bool)>,
    /// Profile that newly selected orgs are added to.
    pub org_profile: String,
    /// Written to `runner.model`; `None` leaves it as it is.
    pub model: Option<String>,
}

/// Rewrites `doc` to match `sel`. `base` is the config file's directory.
pub fn apply(doc: &mut DocumentMut, sel: &Selections, base: &Path) -> Result<()> {
    if !sel.teams.is_empty() {
        set_teams(doc, &sel.teams)?;
    }
    if let Some(model) = &sel.model {
        set_value(doc, "runner", "model", model.as_str().into())?;
    }

    let repo_selected = |p: &Path| sel.repos.iter().find(|(r, _)| r == p).map(|(_, on)| *on);
    let org_selected = |o: &str| {
        sel.orgs
            .iter()
            .find(|(org, _)| org.eq_ignore_ascii_case(o))
            .map(|(_, on)| *on)
    };

    // Drop deselected entries everywhere, noting what's already present.
    let mut present_repos: Vec<PathBuf> = Vec::new();
    let mut present_orgs: Vec<String> = Vec::new();
    if let Some(profiles) = doc.get_mut("profile").and_then(Item::as_table_like_mut) {
        for (_, profile) in profiles.iter_mut() {
            let Some(repos) = profile.get_mut("repos").and_then(Item::as_array_mut) else {
                continue;
            };
            let mut error = None;
            repos.retain(|entry| match entry_kind(entry, base) {
                Ok(Entry::Checkout { path, managed }) => {
                    let keep = !managed || repo_selected(&path) != Some(false);
                    if keep {
                        present_repos.push(path);
                    }
                    keep
                }
                Ok(Entry::Org(org)) => {
                    let keep = org_selected(&org) != Some(false);
                    if keep {
                        present_orgs.push(org);
                    }
                    keep
                }
                Ok(Entry::Other) => true,
                Err(err) => {
                    error.get_or_insert(err);
                    true
                }
            });
            if let Some(err) = error {
                return Err(err);
            }
        }
    }

    for (path, on) in &sel.repos {
        if *on && !present_repos.contains(path) {
            profile_repos(doc, &sel.repo_profile)?.push(contract_path(path));
        }
    }
    for (org, on) in &sel.orgs {
        if *on && !present_orgs.iter().any(|o| o.eq_ignore_ascii_case(org)) {
            let mut table = InlineTable::new();
            table.insert("github", org.as_str().into());
            profile_repos(doc, &sel.org_profile)?.push(table);
        }
    }
    drop_emptied_profiles(doc);
    Ok(())
}

/// `["*"]` plus an exclusion per deselected team, so teams you join later
/// count until you exclude them.
fn set_teams(doc: &mut DocumentMut, teams: &[(TeamRef, bool)]) -> Result<()> {
    let mut patterns = Array::new();
    patterns.push("*");
    for (team, on) in teams {
        if !on {
            patterns.push(format!("!{team}"));
        }
    }
    set_value(doc, "review_requests", "teams", Value::Array(patterns))
}

enum Entry {
    /// A local checkout. Only plain ones (`"path"` or `{ repo = "path" }`)
    /// are `managed`: setup never removes one with `paths` or `remote`,
    /// but still counts it as present so it isn't added again.
    Checkout { path: PathBuf, managed: bool },
    /// A whole-org `github` entry, lowercased.
    Org(String),
    /// Anything setup doesn't manage: `owner/name` entries, path-scoped
    /// entries, entries with a remote override.
    Other,
}

fn entry_kind(entry: &Value, base: &Path) -> Result<Entry> {
    match entry {
        Value::String(path) => Ok(Entry::Checkout {
            path: expand_path(path.value(), base)?,
            managed: true,
        }),
        Value::InlineTable(t) => {
            if let Some(path) = t.get("repo").and_then(Value::as_str) {
                Ok(Entry::Checkout {
                    path: expand_path(path, base)?,
                    managed: t.len() == 1,
                })
            } else if t.len() == 1
                && let Some(org) = t.get("github").and_then(Value::as_str)
                && !org.contains('/')
            {
                Ok(Entry::Org(org.to_ascii_lowercase()))
            } else {
                Ok(Entry::Other)
            }
        }
        _ => Ok(Entry::Other),
    }
}

/// A table set off from whatever precedes it by a blank line.
/// The `repos` array of `[profile.<name>]`, creating either as needed.
fn profile_repos<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Array> {
    let profiles = doc.entry("profile").or_insert_with(|| {
        let mut t = Table::new();
        // Only `[profile.<name>]` headers, no bare `[profile]`.
        t.set_implicit(true);
        Item::Table(t)
    });
    let Some(profiles) = profiles.as_table_like_mut() else {
        bail!("`profile` in the config is not a table");
    };
    let profile = profiles.entry(name).or_insert(Item::Table(new_table()));
    let Some(profile) = profile.as_table_like_mut() else {
        bail!("`profile.{name}` in the config is not a table");
    };
    let repos = profile
        .entry("repos")
        .or_insert(Item::Value(Value::Array(Array::new())));
    repos
        .as_array_mut()
        .ok_or_else(|| color_eyre::eyre::eyre!("`profile.{name}.repos` is not a list"))
}

/// A profile left with no repos is invalid; remove it when `repos` was all
/// it had, so nothing the user wrote by hand is lost.
fn drop_emptied_profiles(doc: &mut DocumentMut) {
    let Some(profiles) = doc.get_mut("profile").and_then(Item::as_table_like_mut) else {
        return;
    };
    let emptied: Vec<String> = profiles
        .iter()
        .filter(|(_, p)| {
            p.as_table_like().is_some_and(|t| {
                t.len() == 1
                    && t.get("repos")
                        .and_then(Item::as_array)
                        .is_some_and(Array::is_empty)
            })
        })
        .map(|(name, _)| name.to_owned())
        .collect();
    for name in emptied {
        profiles.remove(&name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "/base";

    fn apply_to(text: &str, sel: &Selections) -> String {
        let mut doc: DocumentMut = text.parse().unwrap();
        apply(&mut doc, sel, Path::new(BASE)).unwrap();
        doc.to_string()
    }

    #[test]
    fn teams_become_star_plus_exclusions() {
        let sel = Selections {
            teams: vec![
                (TeamRef::new("org", "keep"), true),
                (TeamRef::new("org", "drop"), false),
            ],
            ..Selections::default()
        };
        let out = apply_to("# mine\n[profile.p]\nrepos = [\"/x\"]\n", &sel);
        assert!(out.starts_with("# mine\n[profile.p]"), "{out}");
        assert!(out.contains(r#"teams = ["*", "!org/drop"]"#), "{out}");
    }

    #[test]
    fn repos_and_orgs_are_added_removed_and_others_kept() {
        let text = r#"# top comment
[profile.vuln]
model = "m"
repos = [
  "/base/old",            # going away
  { repo = "/base/keep" },
  { github = "o/r", paths = ["x/**"] },
]

[profile.default]
repos = [{ github = "gone-org" }]
"#;
        let sel = Selections {
            repos: vec![
                ("/base/old".into(), false),
                ("/base/keep".into(), true),
                ("/base/new".into(), true),
                ("/base/unpicked".into(), false),
            ],
            repo_profile: "vuln".into(),
            orgs: vec![("gone-org".into(), false), ("new-org".into(), true)],
            org_profile: "orgs".into(),
            ..Selections::default()
        };
        let out = apply_to(text, &sel);
        assert!(out.contains("# top comment"), "{out}");
        assert!(!out.contains("/base/old"), "{out}");
        assert!(out.contains(r#"{ repo = "/base/keep" }"#), "{out}");
        assert!(
            out.contains(r#"{ github = "o/r", paths = ["x/**"] }"#),
            "{out}"
        );
        assert!(out.contains(r#""/base/new""#), "{out}");
        assert!(!out.contains("unpicked"), "{out}");
        // `default` held only the removed org, so it goes; `orgs` is new.
        assert!(!out.contains("[profile.default]"), "{out}");
        assert!(out.contains("[profile.orgs]"), "{out}");
        assert!(out.contains(r#"{ github = "new-org" }"#), "{out}");
        assert!(!out.contains("\n[profile]\n"), "{out}");
    }

    #[test]
    fn path_scoped_checkouts_are_neither_removed_nor_duplicated() {
        let text = "[profile.p]\nrepos = [{ repo = \"/base/s\", paths = [\"v/**\"] }]\n";
        for on in [true, false] {
            let sel = Selections {
                repos: vec![("/base/s".into(), on)],
                repo_profile: "p".into(),
                ..Selections::default()
            };
            assert_eq!(apply_to(text, &sel), text, "selected: {on}");
        }
    }

    #[test]
    fn model_is_set_replaced_and_kept() {
        let set = |model: Option<&str>| Selections {
            model: model.map(String::from),
            ..Selections::default()
        };
        let base = "[profile.p]\nrepos = [\"/x\"]\n";
        let with = apply_to(base, &set(Some("claude-sonnet-5")));
        assert!(
            with.contains("[runner]\nmodel = \"claude-sonnet-5\""),
            "{with}"
        );
        let auto = apply_to(&with, &set(Some("auto")));
        assert!(auto.contains("model = \"auto\""), "{auto}");
        assert!(!auto.contains("sonnet"), "{auto}");
        assert_eq!(apply_to(&with, &set(None)), with);
        assert_eq!(apply_to(base, &set(None)), base);

        let commented = "[runner]\nmodel = \"m\"  # mine\n";
        assert_eq!(
            apply_to(commented, &set(Some("n"))),
            "[runner]\nmodel = \"n\"  # mine\n"
        );
        let mut doc: DocumentMut = "runner = \"x\"\n".parse().unwrap();
        let err = apply(&mut doc, &set(Some("n")), Path::new(BASE)).unwrap_err();
        assert!(err.to_string().contains("not a table"), "{err}");
    }

    #[test]
    fn existing_entries_are_not_duplicated() {
        let text = "[profile.p]\nrepos = [\"/base/a\", { github = \"Org\" }]\n";
        let sel = Selections {
            repos: vec![("/base/a".into(), true)],
            repo_profile: "p".into(),
            orgs: vec![("org".into(), true)],
            org_profile: "p".into(),
            ..Selections::default()
        };
        assert_eq!(apply_to(text, &sel), text);
    }
}
