//! The `sanic` plugin: a profile's skills, linked into a run's directory so
//! `claude --plugin-dir` loads them, which `--restricted` otherwise doesn't.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr};

/// The plugin's name, which prefixes each of its skills: `sanic:<skill>`.
pub const NAME: &str = "sanic";

/// A built plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    /// What `--plugin-dir` gets.
    pub dir: PathBuf,
    /// Each skill's name and the directory it links to.
    pub skills: BTreeMap<String, PathBuf>,
}

impl Plugin {
    /// How the agent invokes `skill` through the Skill tool.
    #[must_use]
    pub fn invocable(skill: &str) -> String {
        format!("{NAME}:{skill}")
    }
}

/// Builds the plugin in `dir` from `sources`, each a skill directory (one
/// holding a `SKILL.md`) or a directory of them. A name in more than one
/// source keeps its first. Sources under `refused` are skipped: they're PR
/// checkouts, whose skills aren't trusted. `url` is the PR's, for logs.
pub fn build(dir: &Path, sources: &[PathBuf], refused: &Path, url: &str) -> Result<Plugin> {
    let refused = refused
        .canonicalize()
        .unwrap_or_else(|_| refused.to_owned());
    let mut skills: BTreeMap<String, PathBuf> = BTreeMap::new();
    for source in sources {
        let Ok(real) = source.canonicalize() else {
            tracing::warn!(url, dir = %source.display(), "skills directory is missing; skipping it");
            continue;
        };
        if real.starts_with(&refused) {
            tracing::warn!(url, dir = %source.display(), "skills directory is a PR checkout; skipping it");
            continue;
        }
        warn_if_off_default_branch(&real, url);
        for (name, skill) in skill_dirs(&real)? {
            // A skill dir may itself be a link, into a PR checkout too.
            let Ok(skill) = skill.canonicalize() else {
                continue;
            };
            if skill.starts_with(&refused) {
                tracing::warn!(url, skill = %skill.display(), "skill is in a PR checkout; skipping it");
                continue;
            }
            if !skill.starts_with(&real) {
                warn_if_off_default_branch(&skill, url);
            }
            match skills.get(&name) {
                Some(kept) => tracing::warn!(
                    url,
                    skill = name,
                    kept = %kept.display(),
                    skipped = %skill.display(),
                    "two skills share a name; the first configured is used"
                ),
                None => {
                    skills.insert(name, skill);
                }
            }
        }
    }

    if dir.exists() {
        std::fs::remove_dir_all(dir).wrap_err_with(|| format!("clearing {}", dir.display()))?;
    }
    let manifest = dir.join(".claude-plugin");
    let links = dir.join("skills");
    std::fs::create_dir_all(&manifest)
        .and_then(|()| std::fs::create_dir_all(&links))
        .wrap_err_with(|| format!("creating {}", dir.display()))?;
    let json = serde_json::json!({
        "name": NAME,
        "description": "The review profile's skills, linked by sanic-review",
    });
    std::fs::write(manifest.join("plugin.json"), json.to_string())
        .wrap_err_with(|| format!("writing {}", manifest.display()))?;
    for (name, skill) in &skills {
        let link = links.join(name);
        std::os::unix::fs::symlink(skill, &link)
            .wrap_err_with(|| format!("linking {} to {}", link.display(), skill.display()))?;
    }
    std::fs::write(
        dir.join("skills.json"),
        serde_json::to_string_pretty(&skills)?,
    )
    .wrap_err_with(|| format!("writing {}", dir.join("skills.json").display()))?;
    Ok(Plugin {
        dir: dir.to_owned(),
        skills,
    })
}

/// `dir` itself if it holds a `SKILL.md`, else each child that does, by
/// name.
fn skill_dirs(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    if dir.join("SKILL.md").is_file() {
        return Ok(name(dir).map(|n| (n, dir.to_owned())).into_iter().collect());
    }
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .wrap_err_with(|| format!("reading {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.join("SKILL.md").is_file())
        .filter_map(|p| Some((name(&p)?, p)))
        .collect();
    found.sort();
    Ok(found)
}

/// Warns when `dir` is in a git checkout of a branch other than its
/// remote's default: a PR branch checked out there would load the PR's
/// skills as yours.
fn warn_if_off_default_branch(dir: &Path, url: &str) {
    let Some((root, branch)) = checkout_branch(dir) else {
        return;
    };
    let off = match (&branch, default_branch(&root)) {
        (Some(branch), Some(default)) => *branch != default,
        (Some(branch), None) => !matches!(branch.as_str(), "main" | "master"),
        // Detached, as a jj working copy or a checked-out commit is.
        (None, _) => true,
    };
    if off {
        tracing::warn!(
            url,
            dir = %dir.display(),
            branch = branch.as_deref().unwrap_or("(none)"),
            "skills directory is in a checkout that isn't on its default branch; its skills load as yours"
        );
    }
}

/// The checkout holding `dir` and the branch it has checked out, `None` for
/// a detached head or a jj workspace without git; `None` outside a checkout.
fn checkout_branch(dir: &Path) -> Option<(PathBuf, Option<String>)> {
    let root = dir
        .ancestors()
        .find(|d| d.join(".git").exists() || d.join(".jj").exists())?;
    let git = root.join(".git");
    if !git.exists() {
        return Some((root.to_owned(), None));
    }
    let git_dir = if git.is_file() {
        let text = std::fs::read_to_string(&git).ok()?;
        PathBuf::from(text.strip_prefix("gitdir:")?.trim())
    } else {
        git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(String::from);
    Some((root.to_owned(), branch))
}

/// `origin`'s default branch as the checkout at `root` last fetched it.
fn default_branch(root: &Path) -> Option<String> {
    let git = root.join(".git");
    let common = if git.is_file() {
        let text = std::fs::read_to_string(&git).ok()?;
        let worktree = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        let common = std::fs::read_to_string(worktree.join("commondir")).ok()?;
        worktree.join(common.trim())
    } else {
        git
    };
    let head = std::fs::read_to_string(common.join("refs/remotes/origin/HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/remotes/origin/")?;
    Some(branch.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
    }

    #[test]
    fn links_skill_dirs_and_dirs_of_them_first_name_winning() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        skill(&t.join("one/review"));
        skill(&t.join("many/review"));
        skill(&t.join("many/lint"));
        std::fs::create_dir_all(t.join("many/notes")).unwrap();

        let sources = [t.join("one/review"), t.join("many"), t.join("gone")];
        let plugin = build(&t.join("plugin"), &sources, &t.join("worktrees"), "u").unwrap();

        let names: Vec<_> = plugin.skills.keys().map(String::as_str).collect();
        assert_eq!(names, ["lint", "review"]);
        let real = |p: &str| t.join(p).canonicalize().unwrap();
        assert_eq!(plugin.skills["review"], real("one/review"));
        let link = t.join("plugin/skills/review");
        assert_eq!(std::fs::read_link(&link).unwrap(), real("one/review"));
        let manifest =
            std::fs::read_to_string(t.join("plugin/.claude-plugin/plugin.json")).unwrap();
        assert!(manifest.contains(r#""name":"sanic""#), "{manifest}");
        assert!(t.join("plugin/skills.json").is_file());
    }

    #[test]
    fn skips_skills_in_pr_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        skill(&t.join("worktrees/7/.claude/skills/evil"));
        skill(&t.join("mine/good"));
        std::os::unix::fs::symlink(
            t.join("worktrees/7/.claude/skills/evil"),
            t.join("mine/linked"),
        )
        .unwrap();
        let sources = [t.join("worktrees/7/.claude/skills"), t.join("mine")];
        let plugin = build(&t.join("plugin"), &sources, &t.join("worktrees"), "u").unwrap();
        let names: Vec<_> = plugin.skills.keys().map(String::as_str).collect();
        assert_eq!(names, ["good"]);
    }

    #[test]
    fn reads_the_branch_a_checkout_and_its_worktree_have_out() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        std::fs::create_dir_all(t.join("repo/.git/refs/remotes/origin")).unwrap();
        std::fs::create_dir_all(t.join("repo/.git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(t.join("repo/a/b")).unwrap();
        std::fs::create_dir_all(t.join("wt/c")).unwrap();
        std::fs::write(t.join("repo/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            t.join("repo/.git/refs/remotes/origin/HEAD"),
            "ref: refs/remotes/origin/trunk\n",
        )
        .unwrap();
        let wt_git = t.join("repo/.git/worktrees/wt");
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/pr-9\n").unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        std::fs::write(t.join("wt/.git"), format!("gitdir: {}\n", wt_git.display())).unwrap();

        let (root, branch) = checkout_branch(&t.join("repo/a/b")).unwrap();
        assert_eq!((root, branch.as_deref()), (t.join("repo"), Some("main")));
        assert_eq!(default_branch(&t.join("repo")).as_deref(), Some("trunk"));
        let (root, branch) = checkout_branch(&t.join("wt/c")).unwrap();
        assert_eq!((root, branch.as_deref()), (t.join("wt"), Some("pr-9")));
        assert_eq!(default_branch(&t.join("wt")).as_deref(), Some("trunk"));
        std::fs::write(t.join("repo/.git/HEAD"), "0123abcd\n").unwrap();
        assert_eq!(checkout_branch(&t.join("repo/a")).unwrap().1, None);
        std::fs::create_dir_all(t.join("jj/.jj")).unwrap();
        assert_eq!(
            checkout_branch(&t.join("jj")).unwrap(),
            (t.join("jj"), None)
        );
    }
}
