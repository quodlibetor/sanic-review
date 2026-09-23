//! Configuration file parsing and PR-to-profile matching.
//!
//! Parsing is pure: anything that needs to inspect a local checkout goes
//! through [`CheckoutResolver`], which the runner crate implements against
//! real jj/git repos.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, bail, eyre},
};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use indexmap::IndexMap;
use serde::Deserialize;

use crate::{pr::TeamRef, repo::RepoName};

const DEFAULT_API_URL: &str = "https://api.github.com";
const DEFAULT_RECONCILE: Duration = Duration::from_mins(5);
const DEFAULT_MIN_NOTIFICATION_POLL: Duration = Duration::from_secs(60);
const DEFAULT_QUIET: Duration = Duration::from_mins(2);
const DEFAULT_GIT_URL: &str = "https://github.com";
const DEFAULT_CLAUDE: &str = "claude";
const DEFAULT_MAX_RUNS: usize = 2;
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_mins(30);

/// The version control system backing a local checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vcs {
    Jj,
    Git,
}

/// Inspects local checkouts named in the config.
pub trait CheckoutResolver {
    /// Returns the checkout's VCS and the GitHub repository it tracks, using
    /// `remote` if given and remote discovery otherwise.
    fn resolve(&self, path: &Path, remote: Option<&str>) -> Result<(Vcs, RepoName)>;
}

#[derive(Debug)]
pub struct Config {
    pub github: GithubSettings,
    pub poll: PollSettings,
    pub review_requests: ReviewRequestSettings,
    pub runner: RunnerSettings,
    /// In file order; earlier profiles win ties when matching.
    pub profiles: Vec<Profile>,
}

#[derive(Debug)]
pub struct GithubSettings {
    pub api_url: String,
    /// Base URL that repo mirrors fetch from, as `<git_url>/<owner>/<name>.git`.
    pub git_url: String,
}

#[derive(Debug)]
pub struct PollSettings {
    pub reconcile_interval: Duration,
    /// Lower bound on the notifications poll interval; GitHub's
    /// `X-Poll-Interval` can only raise it.
    pub min_notification_interval: Duration,
    /// How long a PR must go without new triggers before its run starts.
    pub quiet_period: Duration,
}

#[derive(Debug)]
pub struct ReviewRequestSettings {
    /// Which of your teams' review requests count as requests to you.
    pub teams: TeamFilter,
}

/// Ordered team globs; the last pattern that matches a team decides, and a
/// leading `!` excludes. A pattern with a `/` matches `org/slug`, otherwise
/// just the slug. Teams no pattern matches are excluded.
#[derive(Debug)]
pub struct TeamFilter {
    pub patterns: Vec<String>,
    rules: Vec<(bool, globset::GlobMatcher, bool)>,
}

impl TeamFilter {
    pub fn new(patterns: Vec<String>) -> Result<Self> {
        let rules = patterns
            .iter()
            .map(|pattern| {
                let (allow, glob) = match pattern.strip_prefix('!') {
                    Some(rest) => (false, rest),
                    None => (true, pattern.as_str()),
                };
                let matcher = GlobBuilder::new(&glob.to_ascii_lowercase())
                    .literal_separator(true)
                    .build()
                    .wrap_err_with(|| format!("invalid team glob `{pattern}`"))?
                    .compile_matcher();
                Ok((allow, matcher, glob.contains('/')))
            })
            .collect::<Result<_>>()?;
        Ok(Self { patterns, rules })
    }

    #[must_use]
    pub fn allows(&self, team: &TeamRef) -> bool {
        let full = team.to_string();
        self.rules
            .iter()
            .rev()
            .find(|(_, glob, qualified)| glob.is_match(if *qualified { &full } else { &team.slug }))
            .is_some_and(|(allow, _, _)| *allow)
    }
}

#[derive(Debug, Clone)]
pub struct RunnerSettings {
    /// The `claude` executable; a bare name is looked up on `PATH`.
    pub claude: PathBuf,
    /// Agent runs allowed at once, across all PRs.
    pub max_concurrent: usize,
    /// A run still going after this long is killed and marked failed.
    pub timeout: Duration,
    /// Extra directories the agent may read, besides the configured
    /// checkouts; see [`Config::reference_dirs`].
    pub read_paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct Profile {
    pub name: String,
    pub instructions: Vec<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub model: Option<String>,
    pub auto_fix: bool,
    pub targets: Vec<Target>,
}

/// One entry of a profile's `repos` list.
#[derive(Debug)]
pub struct Target {
    pub scope: Scope,
    pub paths: Option<PathFilter>,
    /// Set for entries that name a local checkout.
    pub checkout: Option<Checkout>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Lowercased org or user login.
    Org(String),
    Repo(RepoName),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub path: PathBuf,
    pub vcs: Vcs,
}

/// Globs relative to the repository root.
#[derive(Debug)]
pub struct PathFilter {
    pub patterns: Vec<String>,
    set: GlobSet,
}

impl PathFilter {
    fn new(patterns: Vec<String>) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &patterns {
            let glob = GlobBuilder::new(pattern.trim_start_matches('/'))
                .literal_separator(true)
                .build()
                .wrap_err_with(|| format!("invalid path glob `{pattern}`"))?;
            builder.add(glob);
        }
        let set = builder.build()?;
        Ok(Self { patterns, set })
    }

    #[must_use]
    pub fn matches_any<S: AsRef<str>>(&self, files: &[S]) -> bool {
        files.iter().any(|f| self.set.is_match(f.as_ref()))
    }
}

impl Target {
    fn covers(&self, repo: &RepoName) -> bool {
        match &self.scope {
            Scope::Org(org) => *org == repo.owner,
            Scope::Repo(r) => r == repo,
        }
    }

    fn specificity(&self) -> u8 {
        match (&self.scope, &self.paths) {
            (Scope::Org(_), _) => 0,
            (Scope::Repo(_), None) => 1,
            (Scope::Repo(_), Some(_)) => 2,
        }
    }
}

/// The profile and entry a PR matched.
#[derive(Debug, Clone, Copy)]
pub struct Match<'a> {
    pub profile: &'a Profile,
    pub target: &'a Target,
}

impl Config {
    /// Whether any entry could match a PR in `repo`. Cheap pre-filter for
    /// notifications, before fetching the PR.
    #[must_use]
    pub fn watches(&self, repo: &RepoName) -> bool {
        self.targets().any(|(_, t)| t.covers(repo))
    }

    /// Whether matching a PR in `repo` needs its changed files.
    #[must_use]
    pub fn needs_files(&self, repo: &RepoName) -> bool {
        self.targets()
            .any(|(_, t)| t.paths.is_some() && t.covers(repo))
    }

    /// The most specific matching entry (path-scoped, then repo, then org);
    /// ties go to the entry that appears first in the file.
    ///
    /// A repo entry claims its repo: once one exists, org entries no longer
    /// apply to that repo, so a PR outside a path-scoped entry's globs
    /// matches nothing rather than falling back to the org.
    #[must_use]
    pub fn match_pr<S: AsRef<str>>(&self, repo: &RepoName, files: &[S]) -> Option<Match<'_>> {
        let claimed = self
            .targets()
            .any(|(_, t)| matches!(t.scope, Scope::Repo(_)) && t.covers(repo));
        let mut best: Option<Match<'_>> = None;
        for (profile, target) in self.targets() {
            if !target.covers(repo) || (claimed && matches!(target.scope, Scope::Org(_))) {
                continue;
            }
            if let Some(paths) = &target.paths
                && !paths.matches_any(files)
            {
                continue;
            }
            if best.is_none_or(|b| target.specificity() > b.target.specificity()) {
                best = Some(Match { profile, target });
            }
        }
        best
    }

    /// Directories a review agent may read for context beyond the PR: every
    /// local checkout in any profile, then `runner.read_paths`, without
    /// duplicates. They may be at other revisions than the PR.
    #[must_use]
    pub fn reference_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let checkouts = self
            .targets()
            .filter_map(|(_, t)| t.checkout.as_ref().map(|c| &c.path));
        for dir in checkouts.chain(&self.runner.read_paths) {
            if !dirs.contains(dir) {
                dirs.push(dir.clone());
            }
        }
        dirs
    }

    fn targets(&self) -> impl Iterator<Item = (&Profile, &Target)> {
        self.profiles
            .iter()
            .flat_map(|p| p.targets.iter().map(move |t| (p, t)))
    }

    /// Reads and resolves the config at `path`.
    pub fn load(path: &Path, resolver: &dyn CheckoutResolver) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("reading config {}", path.display()))
            .suggestion("create it; see docs/DESIGN.md for the format")?;
        let base = path.parent().unwrap_or(Path::new("."));
        Self::parse(&text, base, resolver).wrap_err_with(|| format!("in config {}", path.display()))
    }

    /// Parses config text. Relative paths resolve against `base`.
    pub fn parse(text: &str, base: &Path, resolver: &dyn CheckoutResolver) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text)?;
        // Zero would make `serve` poll GitHub in a tight loop.
        for (key, value) in [
            ("reconcile_secs", raw.poll.reconcile_secs),
            ("min_notification_secs", raw.poll.min_notification_secs),
        ] {
            if value == Some(0) {
                bail!("`poll.{key}` must be at least 1");
            }
        }
        if raw.runner.max_concurrent == Some(0) {
            bail!("`runner.max_concurrent` must be at least 1");
        }
        if raw.runner.timeout_secs == Some(0) {
            bail!("`runner.timeout_secs` must be at least 1");
        }
        if raw.profile.is_empty() {
            return Err(eyre!("no profiles configured"))
                .suggestion("add a `[profile.<name>]` table with a `repos` list");
        }
        let paths = PathContext { base };
        let claude = match raw.runner.claude {
            // A bare name means PATH lookup, not a file next to the config.
            Some(claude) if claude.contains('/') || claude.starts_with('~') => {
                paths.expand(&claude)?
            }
            Some(claude) => PathBuf::from(claude),
            None => PathBuf::from(DEFAULT_CLAUDE),
        };
        let read_paths = raw
            .runner
            .read_paths
            .iter()
            .map(|p| paths.expand(p))
            .collect::<Result<_>>()
            .wrap_err("in `runner.read_paths`")?;
        let profiles = raw
            .profile
            .into_iter()
            .map(|(name, p)| {
                resolve_profile(&name, p, &paths, resolver)
                    .wrap_err_with(|| format!("in profile `{name}`"))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            github: GithubSettings {
                api_url: raw.github.api_url.unwrap_or_else(|| DEFAULT_API_URL.into()),
                git_url: raw.github.git_url.unwrap_or_else(|| DEFAULT_GIT_URL.into()),
            },
            poll: PollSettings {
                reconcile_interval: raw
                    .poll
                    .reconcile_secs
                    .map_or(DEFAULT_RECONCILE, Duration::from_secs),
                min_notification_interval: raw
                    .poll
                    .min_notification_secs
                    .map_or(DEFAULT_MIN_NOTIFICATION_POLL, Duration::from_secs),
                quiet_period: raw
                    .poll
                    .quiet_secs
                    .map_or(DEFAULT_QUIET, Duration::from_secs),
            },
            review_requests: ReviewRequestSettings {
                teams: TeamFilter::new(
                    raw.review_requests
                        .teams
                        .unwrap_or_else(|| vec!["*".into()]),
                )
                .wrap_err("in `review_requests.teams`")?,
            },
            runner: RunnerSettings {
                claude,
                max_concurrent: raw.runner.max_concurrent.unwrap_or(DEFAULT_MAX_RUNS),
                timeout: raw
                    .runner
                    .timeout_secs
                    .map_or(DEFAULT_RUN_TIMEOUT, Duration::from_secs),
                read_paths,
            },
            profiles,
        })
    }
}

/// `$XDG_CONFIG_HOME/sanic-review/config.toml`, falling back to `~/.config`.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
        .join("sanic-review")
        .join("config.toml"))
}

/// `$XDG_DATA_HOME/sanic-review`, falling back to `~/.local/share`.
pub fn default_data_dir() -> Result<PathBuf> {
    Ok(xdg_dir("XDG_DATA_HOME", ".local/share")?.join("sanic-review"))
}

fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(home()?.join(fallback)),
    }
}

fn home() -> Result<PathBuf> {
    std::env::home_dir().ok_or_else(|| eyre!("cannot determine the home directory"))
}

struct PathContext<'a> {
    base: &'a Path,
}

impl PathContext<'_> {
    fn expand(&self, raw: &str) -> Result<PathBuf> {
        let path = match raw.strip_prefix("~/") {
            Some(rest) => home()?.join(rest),
            None if raw == "~" => home()?,
            None => PathBuf::from(raw),
        };
        Ok(if path.is_absolute() {
            path
        } else {
            self.base.join(path)
        })
    }
}

fn resolve_profile(
    name: &str,
    raw: RawProfile,
    paths: &PathContext<'_>,
    resolver: &dyn CheckoutResolver,
) -> Result<Profile> {
    if raw.repos.is_empty() {
        return Err(eyre!("`repos` is empty"))
            .suggestion("list local checkouts or `{ github = \"org\" }` entries");
    }
    let targets = raw
        .repos
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            resolve_target(entry, paths, resolver).wrap_err_with(|| format!("in `repos[{i}]`"))
        })
        .collect::<Result<_>>()?;
    let expand_all = |list: &[String]| -> Result<Vec<PathBuf>> {
        list.iter().map(|p| paths.expand(p)).collect()
    };
    Ok(Profile {
        name: name.into(),
        instructions: expand_all(&raw.instructions)?,
        skills: expand_all(&raw.skills)?,
        model: raw.model,
        auto_fix: raw.auto_fix,
        targets,
    })
}

const TARGET_SHAPE: &str = "expected a checkout path, `{ repo = \"<path>\", paths = [...], remote = \"...\" }`, \
     or `{ github = \"org\" | \"owner/name\", paths = [...] }`";

fn resolve_target(
    entry: &toml::Value,
    paths: &PathContext<'_>,
    resolver: &dyn CheckoutResolver,
) -> Result<Target> {
    let table = match entry {
        toml::Value::String(path) => return local_target(path, None, None, paths, resolver),
        toml::Value::Table(table) => table,
        _ => bail!("{TARGET_SHAPE}"),
    };
    let str_field = |key: &str| -> Result<Option<&str>> {
        table.get(key).map_or(Ok(None), |v| {
            v.as_str()
                .map(Some)
                .ok_or_else(|| eyre!("`{key}` must be a string"))
        })
    };
    let path_globs = match table.get("paths") {
        None => None,
        Some(toml::Value::Array(items)) => Some(
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .ok_or_else(|| eyre!("`paths` must be a list of strings"))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        Some(_) => bail!("`paths` must be a list of strings"),
    };
    for key in table.keys() {
        if !["repo", "github", "paths", "remote"].contains(&key.as_str()) {
            return Err(eyre!("unknown key `{key}`")).note(TARGET_SHAPE);
        }
    }

    match (str_field("repo")?, str_field("github")?) {
        (Some(path), None) => local_target(path, path_globs, str_field("remote")?, paths, resolver),
        (None, Some(github)) => {
            if table.contains_key("remote") {
                bail!("`remote` only applies to `repo` entries");
            }
            let scope = if github.contains('/') {
                Scope::Repo(RepoName::parse(github)?)
            } else {
                Scope::Org(github.to_ascii_lowercase())
            };
            if matches!(scope, Scope::Org(_)) && path_globs.is_some() {
                return Err(eyre!("`paths` needs a single repository"))
                    .suggestion(format!("use `github = \"{github}/<name>\"`"));
            }
            Ok(Target {
                scope,
                paths: path_globs.map(PathFilter::new).transpose()?,
                checkout: None,
            })
        }
        (Some(_), Some(_)) => bail!("set either `repo` or `github`, not both"),
        (None, None) => bail!("{TARGET_SHAPE}"),
    }
}

fn local_target(
    raw_path: &str,
    path_globs: Option<Vec<String>>,
    remote: Option<&str>,
    paths: &PathContext<'_>,
    resolver: &dyn CheckoutResolver,
) -> Result<Target> {
    let path = paths.expand(raw_path)?;
    let (vcs, repo) = resolver
        .resolve(&path, remote)
        .wrap_err_with(|| format!("resolving checkout {}", path.display()))?;
    Ok(Target {
        scope: Scope::Repo(repo),
        paths: path_globs.map(PathFilter::new).transpose()?,
        checkout: Some(Checkout { path, vcs }),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    github: RawGithub,
    #[serde(default)]
    poll: RawPoll,
    #[serde(default)]
    review_requests: RawReviewRequests,
    #[serde(default)]
    runner: RawRunner,
    #[serde(default)]
    profile: IndexMap<String, RawProfile>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawGithub {
    api_url: Option<String>,
    git_url: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRunner {
    claude: Option<String>,
    max_concurrent: Option<usize>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    read_paths: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names, reason = "the names are the TOML keys")]
struct RawPoll {
    reconcile_secs: Option<u64>,
    min_notification_secs: Option<u64>,
    quiet_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawReviewRequests {
    teams: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    #[serde(default)]
    instructions: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    model: Option<String>,
    #[serde(default)]
    auto_fix: bool,
    // Entries are converted by hand so errors can say which shape was meant.
    repos: Vec<toml::Value>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Maps checkout paths (relative to `/base`) to what they resolve to.
    struct FakeResolver(HashMap<PathBuf, (Vcs, RepoName)>);

    impl FakeResolver {
        fn new(entries: &[(&str, Vcs, &str)]) -> Self {
            Self(
                entries
                    .iter()
                    .map(|(path, vcs, repo)| {
                        (
                            Path::new("/base").join(path),
                            (*vcs, RepoName::parse(repo).unwrap()),
                        )
                    })
                    .collect(),
            )
        }
    }

    impl CheckoutResolver for FakeResolver {
        fn resolve(&self, path: &Path, remote: Option<&str>) -> Result<(Vcs, RepoName)> {
            let (vcs, repo) = self
                .0
                .get(path)
                .cloned()
                .ok_or_else(|| eyre!("no checkout at {}", path.display()))?;
            Ok(match remote {
                Some(owner) => (vcs, RepoName::new(owner, &repo.name)),
                None => (vcs, repo),
            })
        }
    }

    fn parse(text: &str) -> Result<Config> {
        let resolver = FakeResolver::new(&[
            ("vuln-eval", Vcs::Jj, "lacework-dev/vuln-eval"),
            ("services", Vcs::Git, "lacework/services"),
        ]);
        Config::parse(text, Path::new("/base"), &resolver)
    }

    fn matched(config: &Config, repo: &str, files: &[&str]) -> Option<String> {
        config
            .match_pr(&RepoName::parse(repo).unwrap(), files)
            .map(|m| m.profile.name.clone())
    }

    const EXAMPLE: &str = r#"
        [profile.default]
        instructions = ["~/general.md"]
        repos = [{ github = "lacework" }]

        [profile.vuln]
        model = "claude-sonnet-5"
        auto_fix = true
        repos = [
          "vuln-eval",
          { repo = "services", paths = ["/vulnerability/**"] },
        ]
    "#;

    #[test]
    fn parses_the_documented_shapes() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(config.github.api_url, DEFAULT_API_URL);
        assert_eq!(config.runner.claude, Path::new("claude"));
        assert_eq!(config.profiles.len(), 2);

        let vuln = &config.profiles[1];
        assert!(vuln.auto_fix);
        assert_eq!(vuln.targets.len(), 2);
        assert_eq!(
            vuln.targets[0].checkout,
            Some(Checkout {
                path: "/base/vuln-eval".into(),
                vcs: Vcs::Jj
            })
        );
        assert_eq!(
            vuln.targets[1].scope,
            Scope::Repo(RepoName::new("lacework", "services"))
        );
        assert!(config.profiles[0].instructions[0].ends_with("general.md"));
        assert!(config.profiles[0].instructions[0].is_absolute());
    }

    #[test]
    fn most_specific_entry_wins() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(
            matched(&config, "lacework/services", &["vulnerability/src/a.rs"]).as_deref(),
            Some("vuln")
        );
        // The repo entry claims the repo, so outside the tree nothing
        // matches, not even the org entry.
        assert_eq!(matched(&config, "lacework/services", &["other/a.rs"]), None);
        // Other repos in the org still fall back to it.
        assert_eq!(
            matched(&config, "lacework/other", &["a"]).as_deref(),
            Some("default")
        );
        assert_eq!(matched(&config, "lacework-dev/other", &["a"]), None);
    }

    #[test]
    fn unscoped_repo_entry_still_matches_outside_another_entrys_paths() {
        let config = parse(
            r#"
            [profile.narrow]
            repos = [{ github = "org/repo", paths = ["a/**"] }]
            [profile.whole]
            repos = [{ github = "org/repo" }]
            [profile.org]
            repos = [{ github = "org" }]
            "#,
        )
        .unwrap();
        assert_eq!(
            matched(&config, "org/repo", &["a/x"]).as_deref(),
            Some("narrow")
        );
        assert_eq!(
            matched(&config, "org/repo", &["b/x"]).as_deref(),
            Some("whole")
        );
    }

    #[test]
    fn team_filter_defaults_to_all_teams() {
        let config = parse(EXAMPLE).unwrap();
        let teams = &config.review_requests.teams;
        assert!(teams.allows(&TeamRef::new("lacework-dev", "storage-platform")));
    }

    #[test]
    fn team_filter_last_match_wins_and_qualified_globs_match_org() {
        let filter = TeamFilter::new(vec![
            "*".into(),
            "!storage-*".into(),
            "lacework/storage-core".into(),
        ])
        .unwrap();
        assert!(filter.allows(&TeamRef::new("Lacework", "vuln-backend-dev")));
        assert!(!filter.allows(&TeamRef::new("lacework-dev", "storage-platform")));
        assert!(filter.allows(&TeamRef::new("lacework", "storage-core")));
        assert!(!filter.allows(&TeamRef::new("lacework-dev", "storage-core")));
        assert!(
            !TeamFilter::new(vec![])
                .unwrap()
                .allows(&TeamRef::new("o", "t"))
        );
    }

    #[test]
    fn reference_dirs_are_checkouts_then_read_paths_deduplicated() {
        let config = parse(
            r#"
            [runner]
            read_paths = ["extra", "vuln-eval"]
            [profile.a]
            repos = ["vuln-eval", { github = "org" }]
            [profile.b]
            repos = [{ repo = "services", paths = ["x/**"] }, "vuln-eval"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.reference_dirs(),
            [
                PathBuf::from("/base/vuln-eval"),
                "/base/services".into(),
                "/base/extra".into()
            ]
        );
    }

    #[test]
    fn earlier_profile_wins_ties() {
        let config = parse(
            r#"
            [profile.first]
            repos = [{ github = "org/repo" }]
            [profile.second]
            repos = [{ github = "org/repo" }]
            "#,
        )
        .unwrap();
        assert_eq!(
            matched(&config, "org/repo", &["x"]).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn path_globs_do_not_cross_directories_with_single_star() {
        let config = parse(
            r#"
            [profile.p]
            repos = [{ github = "org/repo", paths = ["src/*.rs"] }]
            "#,
        )
        .unwrap();
        assert!(matched(&config, "org/repo", &["src/lib.rs"]).is_some());
        assert!(matched(&config, "org/repo", &["src/nested/lib.rs"]).is_none());
    }

    #[test]
    fn needs_files_only_for_path_scoped_repos() {
        let config = parse(EXAMPLE).unwrap();
        assert!(config.needs_files(&RepoName::new("lacework", "services")));
        assert!(!config.needs_files(&RepoName::new("lacework", "other")));
        assert!(config.watches(&RepoName::new("lacework", "other")));
        assert!(!config.watches(&RepoName::new("someone", "else")));
    }

    #[test]
    fn remote_override_is_passed_to_the_resolver() {
        let config = parse(
            r#"
            [profile.p]
            repos = [{ repo = "services", remote = "fork" }]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.profiles[0].targets[0].scope,
            Scope::Repo(RepoName::new("fork", "services"))
        );
    }

    #[test]
    fn errors_name_the_offending_entry() {
        for (text, needle) in [
            ("", "no profiles"),
            ("[profile.p]\nrepos = []", "`repos` is empty"),
            ("[profile.p]\nrepos = [3]", "repos[0]"),
            (
                "[profile.p]\nrepos = [{ github = \"org\", paths = [\"a\"] }]",
                "single repository",
            ),
            (
                "[profile.p]\nrepos = [{ repo = \"missing\" }]",
                "resolving checkout",
            ),
            (
                "[profile.p]\nrepos = [{ github = \"o/r\", typo = 1 }]",
                "unknown key",
            ),
            ("[profile.p]\nrepos = [\"services\"]\nbogus = 1", "bogus"),
            ("[poll]\nreconcile_secs = 0", "poll.reconcile_secs"),
            ("[runner]\nmax_concurrent = 0", "runner.max_concurrent"),
        ] {
            let err = format!("{:?}", parse(text).unwrap_err());
            assert!(err.contains(needle), "{text:?}: {err}");
        }
    }
}
