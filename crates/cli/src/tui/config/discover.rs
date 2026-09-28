//! Finding what the config could name: your checkouts, the models
//! `claude` knows, and the skills and instruction files in a profile's
//! checkouts. `setup` finds them here too.

pub mod models;
pub mod scan;
pub mod skills;

use std::path::Path;

use sanic_core::config::{AUTO_MODEL, CheckoutResolver, expand_path};

use super::suggest::{Find, Found};

/// Finds what `find` asks for, reading the disk and, for checkouts, their
/// remotes: slow, so it's run off the UI thread. `config` is the config
/// file's path, which relative paths are resolved against.
pub fn find(find: Find, config: &Path, resolver: &dyn CheckoutResolver) -> Found {
    let base = config.parent().unwrap_or(Path::new("."));
    let places = places();
    match find {
        Find::Checkouts { root, depth } => match expand_path(root.trim(), &places.cwd) {
            Ok(root) if root.is_dir() => Found::Checkouts(scan::scan(&root, depth, resolver).0),
            Ok(root) => Found::Failed(format!("{} is not a directory", root.display())),
            Err(err) => Found::Failed(format!("{err:#}")),
        },
        Find::Extras {
            profile,
            entries,
            skills,
            instructions,
        } => {
            let expand = |listed: &[String]| -> Vec<_> {
                listed
                    .iter()
                    .filter_map(|p| places.expand(p, base))
                    .collect()
            };
            let dirs = skills::profile_dirs(&entries, base, &places);
            Found::Extras {
                profile,
                skills: skills::skills_for(&dirs, &expand(&skills), &places),
                instructions: skills::instructions_for(&dirs, &expand(&instructions)),
            }
        }
    }
}

/// Where paths resolve: typed ones against the working directory, `~`
/// against your home, and your own skills in your Claude directory.
#[must_use]
pub fn places() -> skills::Places {
    let home = std::env::home_dir();
    let cwd = std::env::current_dir().unwrap_or_default();
    let user_skills = models::claude_dir(|var| std::env::var(var).ok(), home.as_deref())
        .map(|dir| cwd.join(dir).join("skills").components().collect());
    skills::Places {
        cwd,
        home,
        user_skills,
    }
}

/// The models to suggest, given the config's `runner.model`.
#[must_use]
pub fn known_models(current: Option<&str>) -> Vec<String> {
    let current = current.filter(|m| !m.eq_ignore_ascii_case(AUTO_MODEL));
    models::known_models(current, models::read_claude_settings().as_deref(), |var| {
        std::env::var(var).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config_doc::RepoEntry, poll::tests::NoCheckouts};

    #[test]
    fn finding_reads_the_disk_relative_to_the_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::create_dir_all(dir.path().join("repo/.claude/skills/review")).unwrap();
        std::fs::write(dir.path().join("repo/.claude/skills/review/SKILL.md"), "").unwrap();
        std::fs::write(dir.path().join("repo/AGENTS.md"), "").unwrap();
        let found = find(
            Find::Extras {
                profile: "p".into(),
                entries: vec![RepoEntry::Checkout {
                    path: "repo".into(),
                    remote: None,
                }],
                skills: Vec::new(),
                instructions: vec!["repo/AGENTS.md".into()],
            },
            &config,
            &NoCheckouts,
        );
        let Found::Extras {
            skills,
            instructions,
            ..
        } = found
        else {
            panic!("{found:?}");
        };
        assert_eq!(skills[0].dir, dir.path().join("repo/.claude/skills/review"));
        assert!(instructions.is_empty(), "AGENTS.md is there already");

        let missing = dir.path().join("nope");
        let found = find(
            Find::Checkouts {
                root: missing.display().to_string(),
                depth: 1,
            },
            &config,
            &NoCheckouts,
        );
        assert!(matches!(found, Found::Failed(why) if why.contains("not a directory")));
    }
}
