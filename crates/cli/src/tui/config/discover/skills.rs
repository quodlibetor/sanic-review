//! Skills and instruction files a profile could use: the `SKILL.md`
//! directories and agent instruction files in its checkouts and your
//! user-level Claude directory.

use std::path::{Component, Path, PathBuf};

use sanic_core::config::expand_path_in;

use crate::config_doc::RepoEntry;

/// Instruction files a checkout may hold for coding agents, relative to
/// each directory searched.
const INSTRUCTION_FILES: [&str; 3] = ["CLAUDE.md", "AGENTS.md", ".claude/CLAUDE.md"];

/// Where paths resolve: typed ones against `cwd`, `~` against `home`.
#[derive(Debug, Clone)]
pub struct Places {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    /// Your user-level skills directory, offered to every profile.
    pub user_skills: Option<PathBuf>,
}

impl Places {
    /// `raw` with `~` expanded and resolved against `base`, without `.`
    /// components; `None` for a `~` path when there's no home directory.
    pub fn expand(&self, raw: &str, base: &Path) -> Option<PathBuf> {
        let path = expand_path_in(raw, base, self.home.as_deref()).ok()?;
        Some(path.components().collect())
    }
}

/// A directory holding a `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub dir: PathBuf,
    /// From the frontmatter, else the directory's name.
    pub name: String,
    pub description: Option<String>,
}

/// The skill in `dir`, if it holds a `SKILL.md`.
pub fn read_skill(dir: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let (name, description) = frontmatter(&text);
    let name = name.or_else(|| Some(dir.file_name()?.to_string_lossy().into_owned()))?;
    Some(Skill {
        dir: dir.to_owned(),
        name,
        description,
    })
}

/// `name` and `description` from a `SKILL.md`'s YAML frontmatter. Only the
/// forms skills use are read: plain or quoted scalars, possibly continued
/// on indented lines, and `|` or `>` blocks, which are joined into a line.
pub fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (None, None);
    }
    let lines: Vec<&str> = lines.take_while(|l| l.trim_end() != "---").collect();
    (field(&lines, "name"), field(&lines, "description"))
}

fn field(lines: &[&str], key: &str) -> Option<String> {
    let at = lines.iter().position(|l| {
        l.strip_prefix(key)
            .is_some_and(|rest| rest.starts_with(':'))
    })?;
    let first = lines[at][key.len() + 1..].trim();
    let first = if first.starts_with(['|', '>']) {
        ""
    } else {
        first
    };
    let rest = lines[at + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']))
        .map(|l| l.trim());
    let joined = std::iter::once(first)
        .chain(rest)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    // Printed to the terminal, and the checkout's text isn't trusted.
    let value: String = unquote(&joined)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// The skills in each of `roots` (`<root>/<skill>/SKILL.md`), in root
/// order and by name within a root, each once.
pub fn find_skills(roots: &[PathBuf]) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut found: Vec<Skill> = entries
            .flatten()
            .filter_map(|e| read_skill(&e.path()))
            .filter(|s| !skills.iter().any(|k| k.dir == s.dir))
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.dir.cmp(&b.dir)));
        skills.extend(found);
    }
    skills
}

/// The [`INSTRUCTION_FILES`] in each of `dirs`, in order, each once: a
/// searched `.claude` directory's `CLAUDE.md` is its parent's too.
pub fn find_instructions(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for file in dirs
        .iter()
        .flat_map(|d| INSTRUCTION_FILES.map(|f| d.join(f)))
    {
        if file.is_file() && !found.contains(&file) {
            found.push(file);
        }
    }
    found
}

/// Where to look for a profile's skills and instructions: for each
/// path-scoped checkout, the directories its globs name and their parents
/// below the checkout, nearest first; then each checkout. `base` is the
/// config file's directory.
#[must_use]
pub fn profile_dirs(entries: &[RepoEntry], base: &Path, places: &Places) -> Vec<PathBuf> {
    let mut scoped = Vec::new();
    let mut roots = Vec::new();
    for entry in entries {
        let (raw, globs) = match entry {
            RepoEntry::Checkout { path, .. } => (path, &[][..]),
            RepoEntry::Scoped { path, paths, .. } => (path, &paths[..]),
            RepoEntry::Github { .. } => continue,
        };
        let Some(root) = places.expand(raw, base) else {
            continue;
        };
        for glob in globs {
            let mut dir = root.join(literal_prefix(glob));
            while dir != root && dir.starts_with(&root) {
                scoped.push(dir.clone());
                if !dir.pop() {
                    break;
                }
            }
        }
        roots.push(root);
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in scoped.into_iter().chain(roots) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The skills to suggest for a profile searching `dirs`: those under each
/// one's `.claude/skills` and your own, less any `present` covers, as
/// itself or its parent.
#[must_use]
pub fn skills_for(dirs: &[PathBuf], present: &[PathBuf], places: &Places) -> Vec<Skill> {
    let mut roots: Vec<PathBuf> = dirs.iter().map(|d| d.join(".claude/skills")).collect();
    roots.extend(places.user_skills.clone());
    find_skills(&roots)
        .into_iter()
        .filter(|s| {
            !present
                .iter()
                .any(|p| *p == s.dir || s.dir.parent() == Some(p))
        })
        .collect()
}

/// The instruction files to suggest for a profile searching `dirs`, less
/// those `present`.
#[must_use]
pub fn instructions_for(dirs: &[PathBuf], present: &[PathBuf]) -> Vec<PathBuf> {
    find_instructions(dirs)
        .into_iter()
        .filter(|f| !present.contains(f))
        .collect()
}

/// The leading components of a `paths` glob that have no wildcards.
pub fn literal_prefix(glob: &str) -> PathBuf {
    Path::new(glob.trim_start_matches('/'))
        .components()
        .take_while(|c| {
            matches!(c, Component::Normal(s)
                if !s.to_string_lossy().contains(['*', '?', '[', '{']))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn skill_md(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# Body\n")
    }

    #[test]
    fn a_profile_is_searched_in_its_scoped_paths_then_its_checkouts() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            root,
            "src/services/documentation/.claude/skills/triage/SKILL.md",
            &skill_md("triage", "T."),
        );
        write(
            root,
            "src/services/.claude/skills/general/SKILL.md",
            &skill_md("general", "G."),
        );
        write(root, "src/services/documentation/CLAUDE.md", "");
        let places = Places {
            cwd: root.to_owned(),
            home: Some(root.join("home")),
            user_skills: None,
        };
        let entries = [
            RepoEntry::Scoped {
                path: "src/services".into(),
                paths: vec!["/documentation/**".into()],
                remote: None,
            },
            RepoEntry::Github {
                name: "org".into(),
                paths: Vec::new(),
            },
        ];
        let dirs = profile_dirs(&entries, root, &places);
        assert_eq!(
            dirs,
            [
                root.join("src/services/documentation"),
                root.join("src/services")
            ]
        );
        let names = |present: &[PathBuf]| -> Vec<String> {
            skills_for(&dirs, present, &places)
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(names(&[]), ["triage", "general"]);
        // One already there, or its parent, isn't suggested again.
        assert_eq!(
            names(&[root.join("src/services/.claude/skills")]),
            ["triage"]
        );
        assert_eq!(
            instructions_for(&dirs, &[]),
            [root.join("src/services/documentation/CLAUDE.md")]
        );
        assert!(
            instructions_for(&dirs, &[root.join("src/services/documentation/CLAUDE.md")])
                .is_empty()
        );
    }

    #[test]
    fn frontmatter_reads_plain_quoted_and_block_values() {
        assert_eq!(
            frontmatter("---\nname: a\ndescription: \"Does: things\"\n---\nname: body\n"),
            (Some("a".into()), Some("Does: things".into()))
        );
        assert_eq!(
            frontmatter("---\ndescription: >\n  Folded\n  text.\nname: 'b'\n---\n"),
            (Some("b".into()), Some("Folded text.".into()))
        );
        assert_eq!(
            frontmatter("---\nname: c\ndescription: starts here\n  and goes on\n---\n"),
            (Some("c".into()), Some("starts here and goes on".into()))
        );
        assert_eq!(frontmatter("# no frontmatter\nname: x\n"), (None, None));
        assert_eq!(frontmatter("---\nnames: x\n---\n"), (None, None));
        assert_eq!(
            frontmatter("---\nname: \u{1b}[2Jwiped\u{7}\n---\n"),
            (Some("[2Jwiped".into()), None),
            "control characters are dropped"
        );
    }

    #[test]
    fn skills_are_found_per_root_by_name_once_each() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "a/zeta/SKILL.md", &skill_md("zeta", "Z."));
        write(root, "a/alpha/SKILL.md", &skill_md("alpha", "A."));
        write(root, "a/notes/README.md", "not a skill");
        write(root, "b/unnamed/SKILL.md", "no frontmatter\n");
        let skills = find_skills(&[
            root.join("a"),
            root.join("b"),
            root.join("a"),
            root.join("none"),
        ]);
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta", "unnamed"]);
        assert_eq!(skills[0].description.as_deref(), Some("A."));
        assert_eq!(skills[2].description, None);

        write(root, "r/.claude/CLAUDE.md", "");
        assert_eq!(
            find_instructions(&[root.join("r/.claude"), root.join("r")]),
            [root.join("r/.claude/CLAUDE.md")]
        );
    }
}
