//! Skills and instruction files for each profile at `setup`: finding them
//! in the profile's checkouts, completing the paths you type, and the
//! prompts, behind [`Prompter`] so they run without a terminal in tests.

use std::path::{Component, Path, PathBuf};

use color_eyre::eyre::{Result, eyre};
use inquire::{
    Autocomplete, Confirm, CustomUserError, MultiSelect, Text, autocompletion::Replacement,
    validator::Validation,
};
use sanic_core::config::{contract_path_in, expand_path_in};
use toml_edit::{DocumentMut, Item, Value};

use super::MULTI_HELP;

/// Instruction files a checkout may hold for coding agents, relative to
/// each directory searched.
const INSTRUCTION_FILES: [&str; 3] = ["CLAUDE.md", "AGENTS.md", ".claude/CLAUDE.md"];

/// Longest description shown beside a skill's name.
const MAX_DESCRIPTION: usize = 72;

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

    /// `path` as it's shown and written: `~/` for paths under home.
    pub fn contract(&self, path: &Path) -> String {
        contract_path_in(path, self.home.as_deref())
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
fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
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

/// Whether `dir` is a skill or holds skills, as `skills` entries may be.
pub fn holds_skills(dir: &Path) -> bool {
    dir.join("SKILL.md").is_file()
        || std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().join("SKILL.md").is_file())
}

/// A profile as the config file has it, for these prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileView {
    pub name: String,
    /// Directories to look in for skills and instructions: for each
    /// path-scoped checkout, the directories its globs name and their
    /// parents below the checkout, nearest first; then each checkout.
    pub dirs: Vec<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub instructions: Vec<PathBuf>,
}

/// Every `[profile.*]` in `doc`, read leniently like [`super::Current`].
/// `base` is the config file's directory.
pub fn read_profiles(doc: &DocumentMut, base: &Path, places: &Places) -> Vec<ProfileView> {
    let Some(profiles) = doc.get("profile").and_then(Item::as_table_like) else {
        return Vec::new();
    };
    // Each path once, however often it's listed: deselecting it removes
    // every entry for it.
    let paths = |item: Option<&Item>| -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for path in item
            .and_then(Item::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| places.expand(v.as_str()?, base))
        {
            if !out.contains(&path) {
                out.push(path);
            }
        }
        out
    };
    profiles
        .iter()
        .filter_map(|(name, profile)| {
            let profile = profile.as_table_like()?;
            let repos: Vec<&Value> = profile
                .get("repos")
                .and_then(Item::as_array)
                .into_iter()
                .flatten()
                .collect();
            Some(ProfileView {
                name: name.to_owned(),
                dirs: search_dirs(&repos, base, places),
                skills: paths(profile.get("skills")),
                instructions: paths(profile.get("instructions")),
            })
        })
        .collect()
}

fn search_dirs(repos: &[&Value], base: &Path, places: &Places) -> Vec<PathBuf> {
    let mut scoped = Vec::new();
    let mut roots = Vec::new();
    for entry in repos {
        let (raw, globs) = match entry {
            Value::String(path) => (path.value().as_str(), None),
            Value::InlineTable(t) => match t.get("repo").and_then(Value::as_str) {
                Some(path) => (path, t.get("paths").and_then(Value::as_array)),
                None => continue,
            },
            _ => continue,
        };
        let Some(root) = places.expand(raw, base) else {
            continue;
        };
        for glob in globs.into_iter().flatten().filter_map(Value::as_str) {
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

/// The leading components of a `paths` glob that have no wildcards.
fn literal_prefix(glob: &str) -> PathBuf {
    Path::new(glob.trim_start_matches('/'))
        .components()
        .take_while(|c| {
            matches!(c, Component::Normal(s)
                if !s.to_string_lossy().contains(['*', '?', '[', '{']))
        })
        .collect()
}

/// What to ask a path for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    SkillDir,
    InstructionFile,
}

/// The prompts this step asks.
pub trait Prompter {
    fn confirm(&mut self, message: &str, default: bool) -> Result<bool>;
    /// The indices of the chosen `options`.
    fn multi_select(
        &mut self,
        message: &str,
        options: Vec<String>,
        defaults: &[usize],
    ) -> Result<Vec<usize>>;
    /// A path that passes [`check_path`], as typed; `None` for a blank
    /// answer.
    fn path(&mut self, message: &str, kind: PathKind) -> Result<Option<String>>;
}

/// The skills and instructions offered for a profile, each with whether
/// it was chosen; ones not offered are left as they are.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProfileExtras {
    pub profile: String,
    pub skills: Vec<(PathBuf, bool)>,
    pub instructions: Vec<(PathBuf, bool)>,
}

/// For each profile, offers to change its skills and instructions: the
/// current ones, pre-selected, then the ones found for it, then any you
/// type. Profiles you decline are left out.
pub fn choose(
    prompter: &mut dyn Prompter,
    profiles: &[ProfileView],
    places: &Places,
) -> Result<Vec<ProfileExtras>> {
    let mut chosen = Vec::new();
    for profile in profiles {
        let message = format!(
            "Change skills and instructions for profile `{}`? It has {}.",
            profile.name,
            describe(profile)
        );
        if !prompter.confirm(&message, false)? {
            continue;
        }
        let mut roots: Vec<PathBuf> = profile
            .dirs
            .iter()
            .map(|d| d.join(".claude/skills"))
            .collect();
        roots.extend(places.user_skills.clone());
        let found: Vec<Skill> = find_skills(&roots)
            .into_iter()
            .filter(|s| {
                !profile
                    .skills
                    .iter()
                    .any(|p| *p == s.dir || s.dir.parent() == Some(p))
            })
            .collect();
        let offered: Vec<(PathBuf, String)> = profile
            .skills
            .iter()
            .map(|p| {
                let label = match read_skill(p) {
                    Some(skill) => skill_label(&skill, places),
                    None => places.contract(p),
                };
                (p.clone(), label)
            })
            .chain(
                found
                    .iter()
                    .map(|s| (s.dir.clone(), skill_label(s, places))),
            )
            .collect();
        let skills = pick(
            prompter,
            &format!("Skills for `{}`:", profile.name),
            offered,
            profile.skills.len(),
            PathKind::SkillDir,
            places,
        )?;

        let found = find_instructions(&profile.dirs)
            .into_iter()
            .filter(|f| !profile.instructions.contains(f));
        let offered = profile
            .instructions
            .iter()
            .cloned()
            .chain(found)
            .map(|p| {
                let label = places.contract(&p);
                (p, label)
            })
            .collect();
        let instructions = pick(
            prompter,
            &format!(
                "Instruction files for `{}`, appended to its system prompt:",
                profile.name
            ),
            offered,
            profile.instructions.len(),
            PathKind::InstructionFile,
            places,
        )?;
        chosen.push(ProfileExtras {
            profile: profile.name.clone(),
            skills,
            instructions,
        });
    }
    Ok(chosen)
}

fn describe(profile: &ProfileView) -> String {
    let count = |n: usize, one: &str, many: &str| match n {
        0 => format!("no {many}"),
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    };
    format!(
        "{} and {}",
        count(profile.skills.len(), "skill entry", "skill entries"),
        count(
            profile.instructions.len(),
            "instruction file",
            "instruction files"
        )
    )
}

fn skill_label(skill: &Skill, places: &Places) -> String {
    let path = places.contract(&skill.dir);
    match &skill.description {
        Some(d) if d.chars().count() > MAX_DESCRIPTION => {
            let cut: String = d.chars().take(MAX_DESCRIPTION - 1).collect();
            format!("{}: {}…  ({path})", skill.name, cut.trim_end())
        }
        Some(d) => format!("{}: {d}  ({path})", skill.name),
        None => format!("{}  ({path})", skill.name),
    }
}

/// Multi-selects among `offered`, the first `current` of which are
/// pre-selected, then takes typed paths until a blank one.
fn pick(
    prompter: &mut dyn Prompter,
    message: &str,
    offered: Vec<(PathBuf, String)>,
    current: usize,
    kind: PathKind,
    places: &Places,
) -> Result<Vec<(PathBuf, bool)>> {
    let (paths, labels): (Vec<PathBuf>, Vec<String>) = offered.into_iter().unzip();
    let picked = if labels.is_empty() {
        Vec::new()
    } else {
        let defaults: Vec<usize> = (0..current).collect();
        prompter.multi_select(message, labels, &defaults)?
    };
    let mut out: Vec<(PathBuf, bool)> = paths
        .into_iter()
        .enumerate()
        .map(|(i, p)| (p, picked.contains(&i)))
        .collect();
    let ask = match kind {
        PathKind::SkillDir => "Another skill directory (tab completes, blank when done):",
        PathKind::InstructionFile => "Another instruction file (tab completes, blank when done):",
    };
    while let Some(raw) = prompter.path(ask, kind)? {
        let path = check_path(&raw, kind, places).map_err(|e| eyre!("{raw}: {e}"))?;
        match out.iter_mut().find(|(p, _)| *p == path) {
            Some((_, on)) => *on = true,
            None => out.push((path, true)),
        }
    }
    Ok(out)
}

/// The path `input` names, if it's the `kind` asked for.
pub fn check_path(input: &str, kind: PathKind, places: &Places) -> Result<PathBuf, String> {
    let path = places
        .expand(input.trim(), &places.cwd)
        .ok_or("there's no home directory for `~`")?;
    if !path.exists() {
        return Err("doesn't exist".into());
    }
    match kind {
        PathKind::SkillDir if !path.is_dir() => Err("isn't a directory".into()),
        PathKind::SkillDir if !holds_skills(&path) => {
            Err("holds no SKILL.md, itself or in a subdirectory".into())
        }
        PathKind::InstructionFile if !path.is_file() => Err("isn't a file".into()),
        _ => Ok(path),
    }
}

/// The entries that complete `input` as a path, spelled as typed so far,
/// with a `/` after directories. Hidden entries are offered once `input`'s
/// last component starts with a dot.
pub fn completions(input: &str, places: &Places) -> Vec<String> {
    if input == "~" {
        return vec!["~/".into()];
    }
    let (typed_dir, prefix) = match input.rfind('/') {
        Some(i) => input.split_at(i + 1),
        None => ("", input),
    };
    let dir = if typed_dir.is_empty() {
        Some(places.cwd.clone())
    } else {
        places.expand(typed_dir, &places.cwd)
    };
    let Some(entries) = dir.and_then(|d| std::fs::read_dir(d).ok()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let shown =
                name.starts_with(prefix) && (prefix.starts_with('.') || !name.starts_with('.'));
            // `metadata` follows symlinks, so a linked directory gets a `/`.
            let slash = if e.path().is_dir() { "/" } else { "" };
            shown.then(|| format!("{typed_dir}{name}{slash}"))
        })
        .collect();
    out.sort();
    out
}

/// The longest start all of `options` share.
fn common_prefix(options: &[String]) -> Option<String> {
    let (first, rest) = options.split_first()?;
    let len = rest.iter().fold(first.len(), |len, o| {
        first[..len]
            .char_indices()
            .zip(o.chars())
            .find(|((_, a), b)| a != b)
            .map_or(len.min(o.len()), |((i, _), _)| i)
    });
    Some(first[..len].to_owned())
}

/// Tab completion of filesystem paths for [`inquire::Text`].
#[derive(Clone)]
struct PathCompleter(Places);

impl Autocomplete for PathCompleter {
    fn get_suggestions(&mut self, input: &str) -> Result<Vec<String>, CustomUserError> {
        Ok(completions(input, &self.0))
    }

    /// The highlighted suggestion, else as far as every suggestion agrees,
    /// as a shell completes.
    fn get_completion(
        &mut self,
        input: &str,
        highlighted: Option<String>,
    ) -> Result<Replacement, CustomUserError> {
        if highlighted.is_some() {
            return Ok(highlighted);
        }
        Ok(common_prefix(&completions(input, &self.0)).filter(|p| p.len() > input.len()))
    }
}

/// [`Prompter`] on the terminal.
pub struct Terminal(pub Places);

impl Prompter for Terminal {
    fn confirm(&mut self, message: &str, default: bool) -> Result<bool> {
        Ok(Confirm::new(message).with_default(default).prompt()?)
    }

    fn multi_select(
        &mut self,
        message: &str,
        options: Vec<String>,
        defaults: &[usize],
    ) -> Result<Vec<usize>> {
        Ok(MultiSelect::new(message, options)
            .with_default(defaults)
            .with_help_message(MULTI_HELP)
            .with_page_size(15)
            .raw_prompt()?
            .into_iter()
            .map(|o| o.index)
            .collect())
    }

    fn path(&mut self, message: &str, kind: PathKind) -> Result<Option<String>> {
        let places = self.0.clone();
        let answer = Text::new(message)
            .with_autocomplete(PathCompleter(self.0.clone()))
            .with_validator(move |input: &str| {
                if input.trim().is_empty() {
                    return Ok(Validation::Valid);
                }
                Ok(match check_path(input, kind, &places) {
                    Ok(_) => Validation::Valid,
                    Err(e) => Validation::Invalid(e.into()),
                })
            })
            .prompt()?;
        let answer = answer.trim();
        Ok((!answer.is_empty()).then(|| answer.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

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

    fn places(root: &Path) -> Places {
        Places {
            cwd: root.join("cwd"),
            home: Some(root.join("home")),
            user_skills: Some(root.join("home/.claude/skills")),
        }
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
        assert!(holds_skills(&root.join("a")));
        assert!(holds_skills(&root.join("a/zeta")));
        assert!(!holds_skills(&root.join("a/notes")));

        write(root, "r/.claude/CLAUDE.md", "");
        assert_eq!(
            find_instructions(&[root.join("r/.claude"), root.join("r")]),
            [root.join("r/.claude/CLAUDE.md")]
        );
    }

    #[test]
    fn profiles_search_scoped_paths_first_then_checkouts() {
        let doc: DocumentMut = r#"
            [profile.vuln]
            skills = ["~/s", "rel", "/home/u/s/"]
            instructions = ["/i.md", 5]
            repos = [
              "~/eval",
              { repo = "/src/services", paths = ["/vulnerability/api/**", "go.mod", "*.md"] },
              { github = "org" },
            ]
            [profile.bare]
            repos = [{ github = "org" }]
        "#
        .parse()
        .unwrap();
        let places = Places {
            cwd: "/cwd".into(),
            home: Some("/home/u".into()),
            user_skills: None,
        };
        let profiles = read_profiles(&doc, Path::new("/cfg"), &places);
        assert_eq!(
            profiles[0],
            ProfileView {
                name: "vuln".into(),
                dirs: [
                    "/src/services/vulnerability/api",
                    "/src/services/vulnerability",
                    "/src/services/go.mod",
                    "/home/u/eval",
                    "/src/services",
                ]
                .map(PathBuf::from)
                .into(),
                skills: vec!["/home/u/s".into(), "/cfg/rel".into()],
                instructions: vec!["/i.md".into()],
            }
        );
        assert_eq!(profiles[1].dirs, Vec::<PathBuf>::new());
    }

    #[test]
    fn completions_list_matching_entries_as_typed() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let places = places(root);
        write(root, "home/proj/.claude/skills/x/SKILL.md", "");
        write(root, "home/proj/CLAUDE.md", "");
        write(root, "home/profile.md", "");
        write(root, "cwd/local/file", "");

        assert_eq!(completions("~", &places), ["~/"]);
        assert_eq!(completions("~/pro", &places), ["~/profile.md", "~/proj/"]);
        assert_eq!(completions("~/proj/", &places), ["~/proj/CLAUDE.md"]);
        assert_eq!(completions("~/proj/.", &places), ["~/proj/.claude/"]);
        assert_eq!(completions("lo", &places), ["local/"]);
        assert_eq!(completions("local/f", &places), ["local/file"]);
        assert!(completions("~/nope/", &places).is_empty());

        let mut completer = PathCompleter(places);
        assert_eq!(
            completer.get_completion("~/pr", None).unwrap(),
            Some("~/pro".into())
        );
        assert_eq!(completer.get_completion("~/pro", None).unwrap(), None);
        assert_eq!(
            completer
                .get_completion("~/pro", Some("~/proj/".into()))
                .unwrap(),
            Some("~/proj/".into())
        );
    }

    #[test]
    fn common_prefix_stops_at_the_first_difference() {
        let opts = |o: &[&str]| o.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(common_prefix(&opts(&["abc", "abd"])), Some("ab".into()));
        assert_eq!(common_prefix(&opts(&["ab/", "ab"])), Some("ab".into()));
        assert_eq!(common_prefix(&opts(&["é1", "é2"])), Some("é".into()));
        assert_eq!(common_prefix(&[]), None);
    }

    #[test]
    fn typed_paths_expand_and_are_checked() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let places = places(root);
        write(root, "home/skills/one/SKILL.md", "");
        write(root, "home/skills/one/ref.md", "");
        write(root, "home/empty/README.md", "");
        write(root, "cwd/AGENTS.md", "");

        let home = root.join("home");
        let skill = PathKind::SkillDir;
        let file = PathKind::InstructionFile;
        assert_eq!(
            check_path("~/skills", skill, &places),
            Ok(home.join("skills"))
        );
        assert_eq!(
            check_path(" ~/skills/./one/ ", skill, &places),
            Ok(home.join("skills/one"))
        );
        assert_eq!(
            check_path("./AGENTS.md", file, &places),
            Ok(root.join("cwd/AGENTS.md"))
        );
        assert!(
            check_path("~/empty", skill, &places)
                .unwrap_err()
                .contains("SKILL.md")
        );
        assert!(
            check_path("~/missing", skill, &places)
                .unwrap_err()
                .contains("exist")
        );
        assert!(
            check_path("AGENTS.md", skill, &places)
                .unwrap_err()
                .contains("directory")
        );
        assert!(
            check_path("~/skills", file, &places)
                .unwrap_err()
                .contains("file")
        );
        let homeless = Places {
            home: None,
            ..places
        };
        assert!(check_path("~/skills", skill, &homeless).is_err());
    }

    enum Answer {
        Confirm(bool),
        Select(Vec<usize>),
        Path(Option<&'static str>),
    }

    /// Answers prompts in order, recording the options each select offered.
    struct Scripted {
        answers: VecDeque<Answer>,
        offered: Vec<(Vec<String>, Vec<usize>)>,
    }

    impl Scripted {
        fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                offered: Vec::new(),
            }
        }
    }

    impl Prompter for Scripted {
        fn confirm(&mut self, message: &str, _: bool) -> Result<bool> {
            match self.answers.pop_front() {
                Some(Answer::Confirm(yes)) => Ok(yes),
                _ => panic!("unexpected confirm: {message}"),
            }
        }

        fn multi_select(
            &mut self,
            message: &str,
            options: Vec<String>,
            defaults: &[usize],
        ) -> Result<Vec<usize>> {
            self.offered.push((options, defaults.to_vec()));
            match self.answers.pop_front() {
                Some(Answer::Select(picked)) => Ok(picked),
                _ => panic!("unexpected select: {message}"),
            }
        }

        fn path(&mut self, message: &str, _: PathKind) -> Result<Option<String>> {
            match self.answers.pop_front() {
                Some(Answer::Path(p)) => Ok(p.map(String::from)),
                _ => panic!("unexpected path prompt: {message}"),
            }
        }
    }

    #[test]
    fn each_profile_is_offered_its_own_skills_and_instructions() {
        use Answer::{Confirm, Path as Typed, Select};

        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let places = places(root);
        let home = root.join("home");
        write(
            root,
            "home/.claude/skills/mine/SKILL.md",
            &skill_md("mine", "Mine."),
        );
        write(
            root,
            "home/svc/.claude/skills/root-skill/SKILL.md",
            &skill_md("root-skill", "R."),
        );
        write(
            root,
            "home/svc/vuln/.claude/skills/v1/SKILL.md",
            &skill_md("v1", "V1."),
        );
        write(
            root,
            "home/svc/vuln/.claude/skills/v2/SKILL.md",
            &skill_md("v2", "V2."),
        );
        write(root, "home/svc/vuln/CLAUDE.md", "");
        write(root, "home/svc/AGENTS.md", "");
        write(
            root,
            "home/other/.claude/skills/o/SKILL.md",
            &skill_md("o", "O."),
        );
        write(root, "home/extra/e/SKILL.md", &skill_md("e", "E."));
        write(root, "home/general.md", "");

        let doc: DocumentMut = r#"
            [profile.skipped]
            repos = ["~/other"]
            [profile.vuln]
            skills = ["~/svc/vuln/.claude/skills/v1", "~/gone"]
            instructions = ["~/general.md"]
            repos = [{ repo = "~/svc", paths = ["/vuln/**"] }]
        "#
        .parse()
        .unwrap();
        let profiles = read_profiles(&doc, &root.join("cfg"), &places);
        let mut prompter = Scripted::new([
            Confirm(false),
            Confirm(true),
            // v1, ~/gone, then found: v2, root-skill, mine. Drop ~/gone,
            // add v2.
            Select(vec![0, 2]),
            Typed(Some("~/extra")),
            Typed(Some("~/svc/vuln/.claude/skills/v1/")),
            Typed(None),
            // general.md, then vuln/CLAUDE.md, AGENTS.md.
            Select(vec![0, 1]),
            Typed(None),
        ]);
        let chosen = choose(&mut prompter, &profiles, &places).unwrap();
        assert_eq!(
            chosen,
            [ProfileExtras {
                profile: "vuln".into(),
                skills: vec![
                    (home.join("svc/vuln/.claude/skills/v1"), true),
                    (home.join("gone"), false),
                    (home.join("svc/vuln/.claude/skills/v2"), true),
                    (home.join("svc/.claude/skills/root-skill"), false),
                    (home.join(".claude/skills/mine"), false),
                    (home.join("extra"), true),
                ],
                instructions: vec![
                    (home.join("general.md"), true),
                    (home.join("svc/vuln/CLAUDE.md"), true),
                    (home.join("svc/AGENTS.md"), false),
                ],
            }]
        );
        let (skills, defaults) = &prompter.offered[0];
        assert_eq!(
            skills,
            &[
                "v1: V1.  (~/svc/vuln/.claude/skills/v1)",
                "~/gone",
                "v2: V2.  (~/svc/vuln/.claude/skills/v2)",
                "root-skill: R.  (~/svc/.claude/skills/root-skill)",
                "mine: Mine.  (~/.claude/skills/mine)",
            ]
        );
        assert_eq!(defaults, &[0, 1]);
        assert!(prompter.answers.is_empty());
    }

    #[test]
    fn skills_under_a_configured_skills_dir_are_not_offered_again() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let places = Places {
            user_skills: None,
            ..places(root)
        };
        write(
            root,
            "home/r/.claude/skills/a/SKILL.md",
            &skill_md("a", "A."),
        );
        let doc: DocumentMut =
            "[profile.p]\nskills = [\"~/r/.claude/skills\"]\nrepos = [\"~/r\"]\n"
                .parse()
                .unwrap();
        let profiles = read_profiles(&doc, root, &places);
        let mut prompter = Scripted::new([
            Answer::Confirm(true),
            Answer::Select(vec![0]),
            Answer::Path(None),
            // No instruction files to offer, so no select.
            Answer::Path(None),
        ]);
        let chosen = choose(&mut prompter, &profiles, &places).unwrap();
        assert_eq!(prompter.offered[0].0, ["~/r/.claude/skills"]);
        assert_eq!(
            chosen[0].skills,
            [(root.join("home/r/.claude/skills"), true)]
        );
        assert!(chosen[0].instructions.is_empty());
    }

    #[test]
    fn long_descriptions_are_cut() {
        let skill = Skill {
            dir: "/s".into(),
            name: "n".into(),
            description: Some("x".repeat(MAX_DESCRIPTION + 5)),
        };
        let places = Places {
            cwd: "/".into(),
            home: None,
            user_skills: None,
        };
        let label = skill_label(&skill, &places);
        assert!(
            label.contains(&format!("{}…  (/s)", "x".repeat(MAX_DESCRIPTION - 1))),
            "{label}"
        );
    }
}
