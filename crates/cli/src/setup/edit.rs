//! Applying `setup` choices to a config document, keeping its comments and
//! everything setup doesn't manage.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, bail};
use sanic_core::{
    config::{contract_path, expand_path},
    pr::TeamRef,
};
use toml_edit::{Array, DocumentMut, InlineTable, Item, RawString, Table, Value};

use super::skills::{Places, ProfileExtras};
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
            remove_keeping_comments(repos, |entry| !match entry_kind(entry, base) {
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
            push_on_own_line(
                profile_repos(doc, &sel.repo_profile)?,
                contract_path(path).into(),
            );
        }
    }
    for (org, on) in &sel.orgs {
        if *on && !present_orgs.iter().any(|o| o.eq_ignore_ascii_case(org)) {
            let mut table = InlineTable::new();
            table.insert("github", org.as_str().into());
            push_on_own_line(profile_repos(doc, &sel.org_profile)?, table.into());
        }
    }
    drop_emptied_profiles(doc);
    Ok(())
}

/// Adds and removes each profile's skills and instructions as `extras`
/// chose, writing paths under home with `~/`. Entries that weren't offered
/// are kept, and a path already present isn't added again. `base` is the
/// config file's directory.
pub fn apply_extras(
    doc: &mut DocumentMut,
    extras: &[ProfileExtras],
    base: &Path,
    places: &Places,
) -> Result<()> {
    for extra in extras {
        let name = &extra.profile;
        let Some(profile) = doc
            .get_mut("profile")
            .and_then(Item::as_table_like_mut)
            .and_then(|profiles| profiles.get_mut(name))
            .and_then(Item::as_table_like_mut)
        else {
            bail!("there's no `[profile.{name}]` in the config");
        };
        for (key, chosen) in [
            ("skills", &extra.skills),
            ("instructions", &extra.instructions),
        ] {
            let adding = chosen.iter().any(|(_, on)| *on);
            if !adding && profile.get(key).is_none() {
                continue;
            }
            let item = profile
                .entry(key)
                .or_insert(Item::Value(Value::Array(Array::new())));
            let Some(list) = item.as_array_mut() else {
                bail!("`profile.{name}.{key}` in the config is not a list");
            };
            let selected = |p: &Path| chosen.iter().find(|(c, _)| c == p).map(|(_, on)| *on);
            let path = |entry: &Value| places.expand(entry.as_str()?, base);
            remove_keeping_comments(list, |entry| {
                path(entry).is_some_and(|p| selected(&p) == Some(false))
            });
            let mut present: Vec<PathBuf> = list.iter().filter_map(path).collect();
            for (path, on) in chosen {
                if *on && !present.contains(path) {
                    push_on_own_line(list, places.contract(path).into());
                    present.push(path.clone());
                }
            }
        }
    }
    Ok(())
}

/// Removes the entries `drop` picks, keeping the comments around them: a
/// comment after an entry's comma is stored with the next entry, so it's
/// handed on to whichever entry follows a removed one, or to the end of the
/// list. Only a comment on the removed entry's own line goes with it.
fn remove_keeping_comments(list: &mut Array, mut drop: impl FnMut(&Value) -> bool) {
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
        let trailing = match after.find('\n') {
            Some(line_end) => format!("{}{}", up_to_last_line(before), &after[line_end..]),
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

/// Appends `value`, on a line of its own when the list's last entry is.
/// What follows the last entry is handed on, so the new comma goes
/// straight after it (or after its trailing comma).
fn push_on_own_line(list: &mut Array, mut value: Value) {
    let trailing_comma = list.trailing_comma();
    let Some(final_entry) = list.iter_mut().last() else {
        list.push(value);
        return;
    };
    let before = raw(final_entry.decor().prefix());
    let indent = before.rfind('\n').map(|end| before[end..].to_owned());
    // What follows the last entry: without a trailing comma, its suffix;
    // then the list's trailing space, which a removal may have filled even
    // without one.
    let mut tail = String::new();
    if !trailing_comma {
        tail.push_str(raw(final_entry.decor().suffix()));
        final_entry.decor_mut().set_suffix("");
    }
    tail.push_str(raw(Some(list.trailing())));
    // A comment after the last entry stays on its line; the space before
    // the `]` moves after the new entry.
    let (comment, close) = tail.split_at(tail.rfind('\n').unwrap_or(0));
    let prefix = match indent {
        Some(indent) => format!("{comment}{indent}"),
        None if comment.trim().is_empty() => " ".into(),
        None => format!("{comment}\n"),
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
    fn removed_repos_keep_the_comments_beside_them() {
        let text = r#"[profile.p]
repos = [
  "/base/a", # on a
  "/base/gone", # on gone
  # before b
  { github = "gone-org" },
  "/base/b",
]
"#;
        let sel = Selections {
            repos: vec![("/base/gone".into(), false)],
            orgs: vec![("gone-org".into(), false)],
            ..Selections::default()
        };
        assert_eq!(
            apply_to(text, &sel),
            r#"[profile.p]
repos = [
  "/base/a", # on a
  # before b
  "/base/b",
]
"#
        );
    }

    #[test]
    fn added_repos_go_on_their_own_lines_in_a_multi_line_list() {
        let sel = Selections {
            repos: vec![("/base/new".into(), true)],
            repo_profile: "p".into(),
            orgs: vec![("new-org".into(), true)],
            org_profile: "p".into(),
            ..Selections::default()
        };
        assert_eq!(
            apply_to("[profile.p]\nrepos = [\n  \"/base/a\", # on a\n]\n", &sel),
            "[profile.p]\nrepos = [\n  \"/base/a\", # on a\n  \"/base/new\",\n  { github = \"new-org\" },\n]\n"
        );
        assert_eq!(
            apply_to("[profile.p]\nrepos = [\"/base/a\"]\n", &sel),
            "[profile.p]\nrepos = [\"/base/a\", \"/base/new\", { github = \"new-org\" }]\n"
        );
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
    fn skills_and_instructions_are_added_removed_and_kept() {
        let places = Places {
            cwd: "/cwd".into(),
            home: Some("/home/u".into()),
            user_skills: None,
        };
        let text = r#"# top comment
[profile.vuln]
# Skills for vuln reviews.
skills = [
  "~/s/keep",   # mine
  "~/s/drop",
  "/abs/untouched",
]
repos = ["/x"]

[profile.bare]
repos = ["/y"] # only repos
"#;
        let extras = [
            ProfileExtras {
                profile: "vuln".into(),
                skills: vec![
                    ("/home/u/s/keep".into(), true),
                    ("/home/u/s/drop".into(), false),
                    ("/home/u/s/new".into(), true),
                    ("/home/u/s/new".into(), true),
                    ("/elsewhere/unpicked".into(), false),
                ],
                instructions: vec![("/home/u/vuln.md".into(), true)],
            },
            ProfileExtras {
                profile: "bare".into(),
                skills: vec![("/home/u/s/unpicked".into(), false)],
                instructions: vec![("/etc/review.md".into(), true)],
            },
        ];
        let mut doc: DocumentMut = text.parse().unwrap();
        apply_extras(&mut doc, &extras, Path::new(BASE), &places).unwrap();
        assert_eq!(
            doc.to_string(),
            r#"# top comment
[profile.vuln]
# Skills for vuln reviews.
skills = [
  "~/s/keep",   # mine
  "/abs/untouched",
  "~/s/new",
]
repos = ["/x"]
instructions = ["~/vuln.md"]

[profile.bare]
repos = ["/y"] # only repos
instructions = ["/etc/review.md"]
"#
        );
        // Applied again, nothing changes.
        let once = doc.to_string();
        apply_extras(&mut doc, &extras, Path::new(BASE), &places).unwrap();
        assert_eq!(doc.to_string(), once);

        let missing = [ProfileExtras {
            profile: "nope".into(),
            ..ProfileExtras::default()
        }];
        let err = apply_extras(&mut doc, &missing, Path::new(BASE), &places).unwrap_err();
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
