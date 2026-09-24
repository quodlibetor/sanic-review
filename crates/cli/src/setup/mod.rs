//! `sanic-review setup`: write or update the config interactively.

mod edit;
mod models;
mod scan;
mod skills;

use std::{
    collections::BTreeSet,
    io::IsTerminal,
    path::{Path, PathBuf},
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, bail},
};
use inquire::{Confirm, MultiSelect, Select, Text};
use sanic_core::{
    config::{
        AUTO_MODEL, Config, TeamFilter, contract_path, default_config_path, expand_path,
        is_auto_model,
    },
    pr::TeamRef,
};
use sanic_github::{Client, Token};
use sanic_runner::vcs::VcsResolver;
use toml_edit::DocumentMut;

use self::edit::{Selections, apply, apply_extras};
use crate::config_edit::write_atomically;

const MULTI_HELP: &str = "space: toggle · →: all · ←: none · type to filter · enter: done";
const NEW_PROFILE: &str = "(new profile)";
const NEW_FILE_HEADER: &str = "# sanic-review config; the format is described in docs/DESIGN.md.\n";
const DEFAULT_DEPTH: usize = 3;

#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// Config file to write; defaults to ~/.config/sanic-review/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// How many directories below the scan directory to look for checkouts.
    #[arg(long, default_value_t = DEFAULT_DEPTH)]
    depth: usize,
}

pub async fn run(args: SetupArgs) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("`setup` is interactive and needs a terminal");
    }
    let path = match args.config {
        Some(path) => path,
        None => default_config_path()?,
    };
    let base = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let (original, header) = match std::fs::read_to_string(&path) {
        Ok(text) => (text, ""),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (String::new(), NEW_FILE_HEADER),
        Err(err) => return Err(err).wrap_err_with(|| format!("reading {}", path.display())),
    };
    let mut doc: DocumentMut = original
        .parse()
        .wrap_err_with(|| format!("parsing {}", path.display()))?;
    let current = Current::read(&doc, &base);

    let api_url = doc
        .get("github")
        .and_then(|g| g.get("api_url"))
        .and_then(toml_edit::Item::as_str)
        .unwrap_or("https://api.github.com");
    let github = Client::new(api_url, Token::discover()?)?;
    let teams = github
        .my_teams()
        .await
        .wrap_err("listing your teams")
        .suggestion("the token needs `read:org`: `gh auth refresh -s read:org`")?;
    let my_orgs = github.my_orgs().await.wrap_err("listing your orgs")?;

    let team_orgs: Vec<String> = teams.iter().map(|t| t.org.clone()).collect();
    let mut teams = choose_teams(teams, current.teams.as_ref())?;
    // Unchanged choices keep the hand-written patterns, or the default when
    // there are none, rather than rewriting them in setup's own form.
    if teams
        .iter()
        .all(|(t, on)| current.teams.as_ref().is_none_or(|f| f.allows(t)) == *on)
    {
        teams.clear();
    }
    let mut sel = Selections {
        teams,
        ..Selections::default()
    };

    let found = scan_for_repos(args.depth)?;
    sel.repos = choose_repos(&found, &current)?;
    let adding_repos = sel
        .repos
        .iter()
        .any(|(p, on)| *on && !current.checkouts.contains(p));
    if adding_repos {
        sel.repo_profile = choose_profile("Profile for newly added repos", &current.profiles)?;
    }

    let candidates: BTreeSet<String> = my_orgs
        .into_iter()
        .chain(team_orgs)
        .chain(found.iter().map(|f| f.repo.owner.clone()))
        .chain(current.orgs.iter().cloned())
        .collect();
    sel.orgs = choose_orgs(candidates, &current)?;
    let adding_orgs = sel
        .orgs
        .iter()
        .any(|(o, on)| *on && !current.orgs.contains(o));
    if adding_orgs {
        sel.org_profile = choose_profile(
            "Profile for newly added orgs (watches all their other PRs)",
            &current.profiles,
        )?;
    }

    sel.model = choose_model(current.model.as_deref())?;

    apply(&mut doc, &sel, &base)?;
    // After the repos are applied, so new profiles and checkouts are
    // searched for skills too.
    choose_skills(&mut doc, &base)?;
    // Prepended rather than parsed in: a document holding only a comment
    // would keep it after any tables setup adds.
    let updated = format!("{header}{doc}");
    if updated == original {
        println!("{} is already up to date.", path.display());
        return Ok(());
    }
    Config::parse(&updated, &base, &VcsResolver)
        .wrap_err("the updated config doesn't load, so it wasn't written")?;
    print_diff(&original, &updated, &path);
    if !Confirm::new(&format!("Write {}?", path.display()))
        .with_default(true)
        .prompt()?
    {
        println!("Not written.");
        return Ok(());
    }
    write_atomically(&path, &updated)?;
    println!(
        "Wrote {}. A running `serve` picks it up automatically.",
        path.display()
    );
    Ok(())
}

/// What the existing config already says, for pre-selecting options.
struct Current {
    teams: Option<TeamFilter>,
    model: Option<String>,
    checkouts: Vec<PathBuf>,
    orgs: Vec<String>,
    profiles: Vec<String>,
}

impl Current {
    /// Reads the document leniently: a config that doesn't fully load (say,
    /// a checkout that moved) is exactly what setup should be able to fix.
    fn read(doc: &DocumentMut, base: &Path) -> Self {
        let teams = doc
            .get("review_requests")
            .and_then(|t| t.get("teams"))
            .and_then(toml_edit::Item::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .and_then(|patterns| TeamFilter::new(patterns).ok());
        // A value that isn't a string reads as blank, so the prompt offers
        // to replace it.
        let model = doc
            .get("runner")
            .and_then(|r| r.get("model"))
            .map(|m| m.as_str().unwrap_or_default().to_owned());
        let mut current = Self {
            teams,
            model,
            checkouts: Vec::new(),
            orgs: Vec::new(),
            profiles: Vec::new(),
        };
        let Some(profiles) = doc.get("profile").and_then(toml_edit::Item::as_table_like) else {
            return current;
        };
        for (name, profile) in profiles.iter() {
            current.profiles.push(name.to_owned());
            let Some(repos) = profile.get("repos").and_then(toml_edit::Item::as_array) else {
                continue;
            };
            for entry in repos {
                let checkout = entry
                    .as_str()
                    .or_else(|| entry.as_inline_table()?.get("repo")?.as_str());
                if let Some(raw) = checkout
                    && let Ok(path) = expand_path(raw, base)
                {
                    current.checkouts.push(path);
                }
                if let Some(org) = entry
                    .as_inline_table()
                    .and_then(|t| t.get("github")?.as_str())
                    .filter(|o| !o.contains('/'))
                {
                    current.orgs.push(org.to_ascii_lowercase());
                }
            }
        }
        current
    }
}

fn choose_teams(
    mut teams: Vec<TeamRef>,
    filter: Option<&TeamFilter>,
) -> Result<Vec<(TeamRef, bool)>> {
    if teams.is_empty() {
        println!("You aren't in any teams; skipping team review requests.");
        return Ok(Vec::new());
    }
    teams.sort();
    let labels: Vec<String> = teams.iter().map(ToString::to_string).collect();
    let defaults: Vec<usize> = teams
        .iter()
        .enumerate()
        .filter(|(_, t)| filter.is_none_or(|f| f.allows(t)))
        .map(|(i, _)| i)
        .collect();
    let picked = MultiSelect::new(
        "Count review requests to these teams as requests to you:",
        labels.clone(),
    )
    .with_default(&defaults)
    .with_help_message(MULTI_HELP)
    .with_page_size(15)
    .prompt()?;
    Ok(teams
        .into_iter()
        .zip(labels)
        .map(|(t, l)| (t, picked.contains(&l)))
        .collect())
}

fn scan_for_repos(depth: usize) -> Result<Vec<scan::Found>> {
    let dir = Text::new("Directory to scan for checkouts:")
        .with_default("~")
        .with_help_message("jj and git checkouts with a GitHub remote are offered")
        .prompt()?;
    // Relative to where setup runs; found checkouts are written as absolute
    // paths, since the config resolves relative ones against its own dir.
    let cwd = std::env::current_dir().wrap_err("finding the current directory")?;
    let root = expand_path(dir.trim(), &cwd)?;
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    println!("Scanning {} ({depth} levels)...", root.display());
    let (found, skipped) = scan::scan(&root, depth, &VcsResolver);
    for s in &skipped {
        println!("  skipped {}: {}", contract_path(&s.path), s.reason);
    }
    Ok(found)
}

fn choose_repos(found: &[scan::Found], current: &Current) -> Result<Vec<(PathBuf, bool)>> {
    if found.is_empty() {
        println!("No checkouts with a GitHub remote found.");
        return Ok(Vec::new());
    }
    let labels: Vec<String> = found
        .iter()
        .map(|f| format!("{}  ({})", f.repo, contract_path(&f.path)))
        .collect();
    let defaults: Vec<usize> = found
        .iter()
        .enumerate()
        .filter(|(_, f)| current.checkouts.contains(&f.path))
        .map(|(i, _)| i)
        .collect();
    let picked = MultiSelect::new("Watch PRs in these checkouts:", labels.clone())
        .with_default(&defaults)
        .with_help_message(MULTI_HELP)
        .with_page_size(15)
        .prompt()?;
    Ok(found
        .iter()
        .zip(labels)
        .map(|(f, l)| (f.path.clone(), picked.contains(&l)))
        .collect())
}

fn choose_orgs(candidates: BTreeSet<String>, current: &Current) -> Result<Vec<(String, bool)>> {
    let mut orgs: Vec<(String, bool)> = Vec::new();
    if !candidates.is_empty() {
        let labels: Vec<String> = candidates.into_iter().collect();
        let defaults: Vec<usize> = labels
            .iter()
            .enumerate()
            .filter(|(_, o)| current.orgs.contains(o))
            .map(|(i, _)| i)
            .collect();
        let picked = MultiSelect::new(
            "Watch all PRs in these orgs (checkouts above still take precedence):",
            labels.clone(),
        )
        .with_default(&defaults)
        .with_help_message(MULTI_HELP)
        .prompt()?;
        orgs = labels
            .into_iter()
            .map(|o| {
                let on = picked.contains(&o);
                (o, on)
            })
            .collect();
    }
    let extra = Text::new("Other orgs to watch (comma-separated, blank for none):").prompt()?;
    for org in extra.split(',').map(str::trim).filter(|o| !o.is_empty()) {
        let org = org.to_ascii_lowercase();
        match orgs.iter_mut().find(|(o, _)| *o == org) {
            Some((_, on)) => *on = true,
            None => orgs.push((org, true)),
        }
    }
    Ok(orgs)
}

fn choose_model(current: Option<&str>) -> Result<Option<String>> {
    let known = models::known_models(current, models::read_claude_settings().as_deref(), |var| {
        std::env::var(var).ok()
    });
    let answer = Text::new("Default model for reviews:")
        .with_default(model_default(current))
        .with_autocomplete(move |input: &str| Ok(models::matching(&known, input)))
        .with_help_message(&format!(
            "\"{AUTO_MODEL}\" uses your Claude default; any model id works. \
             A profile's `model` overrides it"
        ))
        .prompt()?;
    Ok(model_change(current, &answer))
}

/// The current value, or [`AUTO_MODEL`] when it's unset or blank.
fn model_default(current: Option<&str>) -> &str {
    current
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(AUTO_MODEL)
}

/// The `runner.model` to write, or `None` when the answer matches what the
/// config already says; unset already means [`AUTO_MODEL`], in any case. A
/// blank answer takes the default, which replaces a blank value that doesn't
/// load.
fn model_change(current: Option<&str>, answer: &str) -> Option<String> {
    let answer = Some(answer.trim())
        .filter(|a| !a.is_empty())
        .unwrap_or(model_default(current));
    let current = current.unwrap_or(AUTO_MODEL).trim();
    if is_auto_model(answer) {
        return (!is_auto_model(current)).then(|| AUTO_MODEL.to_owned());
    }
    (answer != current).then(|| answer.to_owned())
}

/// Asks, per profile in `doc`, which skills and instruction files it
/// gets, and applies the answers. `base` is the config file's directory.
fn choose_skills(doc: &mut DocumentMut, base: &Path) -> Result<()> {
    let home = std::env::home_dir();
    let cwd = std::env::current_dir().wrap_err("finding the current directory")?;
    // Absolute and without `.`s like other paths here, so the skills found
    // in it match config entries and aren't written relative to the config.
    let user_skills = models::claude_dir(|var| std::env::var(var).ok(), home.as_deref())
        .map(|d| cwd.join(d).join("skills").components().collect());
    let places = skills::Places {
        cwd,
        home,
        user_skills,
    };
    let profiles = skills::read_profiles(doc, base, &places);
    let extras = skills::choose(&mut skills::Terminal(places.clone()), &profiles, &places)?;
    apply_extras(doc, &extras, base, &places)
}

fn choose_profile(message: &str, existing: &[String]) -> Result<String> {
    if existing.is_empty() {
        return Ok(Text::new(&format!("{message}:"))
            .with_default("default")
            .prompt()?);
    }
    let mut options: Vec<String> = existing.to_vec();
    options.push(NEW_PROFILE.into());
    let choice = Select::new(message, options).prompt()?;
    if choice == NEW_PROFILE {
        Ok(Text::new("New profile name:").prompt()?)
    } else {
        Ok(choice)
    }
}

fn print_diff(old: &str, new: &str, path: &Path) {
    let diff = similar::TextDiff::from_lines(old, new);
    let name = path.display().to_string();
    print!("{}", diff.unified_diff().header(&name, &name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_answers_matching_the_config_change_nothing() {
        assert_eq!(model_change(None, "auto"), None, "unset is already auto");
        assert_eq!(model_change(None, " "), None);
        assert_eq!(model_change(Some("m"), "m"), None);
        assert_eq!(model_change(Some("m"), ""), None);
        assert_eq!(model_change(None, " m "), Some("m".into()));
        assert_eq!(model_change(Some("m"), "auto"), Some("auto".into()));
        assert_eq!(model_change(Some("auto"), "n"), Some("n".into()));
        assert_eq!(model_change(Some(" m "), ""), None, "loads as `m`");
    }

    #[test]
    fn auto_answers_ignore_case_and_are_written_lowercase() {
        assert_eq!(model_change(None, "AUTO"), None);
        assert_eq!(model_change(Some("Auto"), "auto"), None);
        assert_eq!(model_change(Some("Auto"), ""), None);
        assert_eq!(model_change(Some("m"), "Auto"), Some("auto".into()));
    }

    #[test]
    fn a_blank_model_is_replaced_by_accepting_the_default() {
        assert_eq!(model_change(Some(""), ""), Some("auto".into()));
        assert_eq!(model_change(Some(" "), "m"), Some("m".into()));
    }

    #[test]
    fn current_reads_what_setup_manages_even_from_a_broken_config() {
        let doc: DocumentMut = r#"
            [review_requests]
            teams = ["*", "!org/x"]
            [runner]
            model = 5
            [profile.p]
            repos = ["/base/a", { repo = "/base/b", paths = ["z"] }, { github = "Org" }, { github = "o/r" }]
            [profile.q]
            bogus = 1
        "#
        .parse()
        .unwrap();
        let current = Current::read(&doc, Path::new("/base"));
        assert!(!current.teams.unwrap().allows(&TeamRef::new("org", "x")));
        assert_eq!(current.model.as_deref(), Some(""), "not a string");
        assert_eq!(
            current.checkouts,
            [PathBuf::from("/base/a"), PathBuf::from("/base/b")]
        );
        assert_eq!(current.orgs, ["org"]);
        assert_eq!(current.profiles, ["p", "q"]);
    }
}
