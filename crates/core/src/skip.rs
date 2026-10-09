//! Which PRs are never reviewed automatically. They're still tracked and
//! shown, except that PRs by authors `authors` excludes are left off the
//! reviews you owe, and a review can still be started by hand.

use std::{collections::HashMap, fmt};

use color_eyre::eyre::{Result, WrapErr, bail};
use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};

use crate::pr::{is_login, without_bot_suffix};

/// Case-insensitive globs over PR titles. Only glob syntax is special, so
/// `build(deps)*` matches its parentheses literally.
#[derive(Debug, Clone, Default)]
pub struct TitleFilter {
    patterns: Vec<String>,
    set: GlobSet,
}

impl TitleFilter {
    pub fn new(patterns: Vec<String>) -> Result<Self> {
        let mut set = GlobSetBuilder::new();
        for pattern in &patterns {
            set.add(
                GlobBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .wrap_err_with(|| format!("invalid title glob `{pattern}`"))?,
            );
        }
        Ok(Self {
            set: set.build()?,
            patterns,
        })
    }

    /// Whether `pattern` is a valid title glob; the error says why not.
    pub fn check_pattern(pattern: &str) -> Result<()> {
        Self::new(vec![pattern.to_owned()]).map(|_| ())
    }

    /// A filter of just `pattern`, as an ignore editor previews it. The
    /// error is the glob crate's own message, without our context line, or
    /// says the pattern is empty.
    pub fn single(pattern: &str) -> Result<Self, String> {
        if pattern.is_empty() {
            return Err("empty".into());
        }
        Self::new(vec![pattern.to_owned()]).map_err(|err| err.root_cause().to_string())
    }

    /// The first pattern `title` matches.
    #[must_use]
    pub fn first_match(&self, title: &str) -> Option<&str> {
        let first = self.set.matches(title).into_iter().min()?;
        Some(self.patterns[first].as_str())
    }
}

/// Ordered globs over PR author logins, as `review_requests.teams` is over
/// teams: the last pattern that matches decides, a leading `!` excludes,
/// and an author no pattern matches is excluded. Case is ignored, and a
/// login's `[bot]` suffix is optional.
#[derive(Debug, Clone)]
pub struct AuthorFilter {
    rules: Vec<AuthorRule>,
}

#[derive(Debug, Clone)]
struct AuthorRule {
    pattern: String,
    allow: bool,
    login: Login,
}

#[derive(Debug, Clone)]
enum Login {
    Any,
    Glob(GlobMatcher),
}

impl Login {
    fn is_match(&self, login: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Glob(glob) => glob.is_match(login),
        }
    }
}

/// `pattern` with glob escapes (`\x`, `[[]`, `[]]`) taken as the characters
/// they stand for.
fn unescape(pattern: &str) -> String {
    pattern
        .replace("[[]", "[")
        .replace("[]]", "]")
        .replace('\\', "")
}

impl AuthorFilter {
    /// Fails on an invalid glob, or one that could never match a login.
    pub fn new(patterns: Vec<String>) -> Result<Self> {
        let rules = patterns
            .into_iter()
            .map(|pattern| {
                let (allow, body) = match pattern.strip_prefix('!') {
                    Some(rest) => (false, rest),
                    None => (true, pattern.as_str()),
                };
                if body.is_empty() {
                    bail!("`{pattern}` can't match a login: it's empty");
                }
                if body.contains('/') {
                    bail!(
                        "`{pattern}` can't match a login: write an app as its login, \
                         e.g. `dependabot`, not `app/dependabot`"
                    );
                }
                let literal = unescape(body);
                if without_bot_suffix(&literal).len() < literal.len() {
                    bail!(
                        "`{pattern}` can't match a login: GitHub gives app logins without \
                         `[bot]`; write `dependabot`, which matches `dependabot[bot]` too"
                    );
                }
                let login = if body == "*" {
                    Login::Any
                } else {
                    Login::Glob(
                        GlobBuilder::new(body)
                            .case_insensitive(true)
                            .build()
                            .wrap_err_with(|| format!("invalid author glob `{pattern}`"))?
                            .compile_matcher(),
                    )
                };
                Ok(AuthorRule {
                    pattern,
                    allow,
                    login,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self { rules })
    }

    /// No patterns, which on its own excludes everyone: a profile's when it
    /// sets none, deferring to the global ones.
    #[must_use]
    pub fn none() -> Self {
        Self { rules: Vec::new() }
    }

    /// `["*"]`, the default: every author.
    #[must_use]
    pub fn everyone() -> Self {
        Self {
            rules: vec![AuthorRule {
                pattern: "*".into(),
                allow: true,
                login: Login::Any,
            }],
        }
    }

    /// The last pattern that matches `author`, as written, and whether it
    /// lets them in.
    #[must_use]
    pub fn last_match(&self, author: &str) -> Option<(bool, &str)> {
        let bare = without_bot_suffix(author);
        self.rules
            .iter()
            .rev()
            .find(|r| r.login.is_match(author) || r.login.is_match(bare))
            .map(|r| (r.allow, r.pattern.as_str()))
    }
}

/// Why a PR isn't reviewed automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// You archived the PR.
    Archived,
    /// `review_requests.authors`, with the profile's after it, excludes the
    /// author: `pattern` is the `!` pattern that does, `None` when none
    /// matches.
    Author {
        login: String,
        pattern: Option<String>,
    },
    /// The PR is a draft, and `skip_drafts` is on.
    Draft,
    /// Someone already reviewed the PR's current head.
    Reviewed { by: ReviewedBy, head: String },
    /// The title matches `review_requests.skip_titles` or the profile's.
    Title { pattern: String },
}

impl Skip {
    /// A one-word reason.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Archived => "archived",
            Self::Author { .. } => "author",
            Self::Draft => "draft",
            Self::Reviewed { .. } => "reviewed",
            Self::Title { .. } => "title",
        }
    }

    /// The status to show for a PR skipped for this reason, as the TUI and
    /// the dashboard show it: `archived`, `skipped: draft`, `reviewed by
    /// you, alice`, `skipped: title` or `skipped: author`.
    #[must_use]
    pub fn status(&self) -> String {
        match self {
            Self::Archived => "archived".into(),
            Self::Reviewed { by, .. } => format!("reviewed by {by}"),
            Self::Author { .. } | Self::Draft | Self::Title { .. } => {
                format!("skipped: {}", self.label())
            }
        }
    }
}

/// Who already reviewed a PR's current head, bots aside. You come first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewedBy {
    pub you: bool,
    /// Other reviewers' logins, in the order they reviewed.
    pub others: Vec<String>,
}

impl ReviewedBy {
    /// Names shown before the rest are counted as `+N`.
    const SHOWN: usize = 2;

    /// From the logins of people who reviewed the head. `None` for nobody.
    #[must_use]
    pub fn new(me: &str, reviewers: &[String]) -> Option<Self> {
        if reviewers.is_empty() {
            return None;
        }
        let mut others: Vec<String> = Vec::new();
        for login in reviewers.iter().filter(|l| !is_login(l, me)) {
            if !others.iter().any(|o| is_login(o, login)) {
                others.push(login.clone());
            }
        }
        Some(Self {
            you: reviewers.iter().any(|l| is_login(l, me)),
            others,
        })
    }
}

impl fmt::Display for ReviewedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .you
            .then_some("you")
            .into_iter()
            .chain(self.others.iter().map(String::as_str))
            .collect();
        let shown = names.len().min(Self::SHOWN);
        write!(f, "{}", names[..shown].join(", "))?;
        if names.len() > shown {
            write!(f, " +{}", names.len() - shown)?;
        }
        Ok(())
    }
}

/// What decides whether one PR is reviewed automatically.
#[derive(Debug, Clone, Copy)]
pub struct PrFacts<'a> {
    pub profile: &'a str,
    pub title: &'a str,
    pub author: &'a str,
    pub is_draft: bool,
    pub archived: bool,
    pub head_sha: &'a str,
    /// People (not bots) whose submitted review is on `head_sha`.
    pub head_reviewers: &'a [String],
    /// The current user.
    pub me: &'a str,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Archived => write!(f, "it's archived"),
            Self::Author {
                pattern: Some(pattern),
                ..
            } => write!(f, "author matches `{pattern}`"),
            Self::Author {
                login,
                pattern: None,
            } => write!(f, "no `authors` pattern matches `{login}`"),
            Self::Draft => write!(f, "it's a draft"),
            Self::Reviewed { by, head } => {
                let short: String = head.chars().take(8).collect();
                write!(f, "already reviewed by {by} at {short}")
            }
            Self::Title { pattern } => write!(f, "title matches `{pattern}`"),
        }
    }
}

/// The skip settings of a whole config, cheap to clone and share with the
/// scheduler and the TUI.
#[derive(Debug, Clone)]
pub struct SkipRules {
    pub(crate) titles: TitleFilter,
    pub(crate) authors: AuthorFilter,
    pub(crate) drafts: bool,
    pub(crate) profiles: HashMap<String, ProfileSkips>,
    /// Profile names in config file order.
    pub(crate) profile_names: Vec<String>,
}

impl Default for SkipRules {
    fn default() -> Self {
        Self {
            titles: TitleFilter::default(),
            authors: AuthorFilter::everyone(),
            drafts: true,
            profiles: HashMap::new(),
            profile_names: Vec::new(),
        }
    }
}

/// A profile's own skip settings.
#[derive(Debug, Clone)]
pub struct ProfileSkips {
    /// Added to the global `skip_titles`.
    pub titles: TitleFilter,
    /// Checked after the global `authors`.
    pub authors: AuthorFilter,
    /// Overrides the global `skip_drafts`.
    pub drafts: Option<bool>,
}

impl SkipRules {
    /// The config's profiles, in file order.
    #[must_use]
    pub fn profile_names(&self) -> &[String] {
        &self.profile_names
    }

    /// Why `pr` isn't reviewed automatically, if it isn't. When several
    /// reasons apply, the first of these wins: archived, author, draft,
    /// already reviewed, title.
    #[must_use]
    pub fn decide(&self, pr: &PrFacts<'_>) -> Option<Skip> {
        if pr.archived {
            return Some(Skip::Archived);
        }
        if let Some(skip) = self.author(pr.profile, pr.author) {
            return Some(skip);
        }
        if let Some(skip) = self.draft(pr.profile, pr.is_draft) {
            return Some(skip);
        }
        if let Some(by) = ReviewedBy::new(pr.me, pr.head_reviewers) {
            return Some(Skip::Reviewed {
                by,
                head: pr.head_sha.to_owned(),
            });
        }
        self.title(pr.profile, pr.title)
    }

    /// Why a PR matched to `profile` isn't reviewed automatically, if it
    /// isn't, by its draft state and title alone. [`SkipRules::decide`]
    /// covers every reason.
    #[must_use]
    pub fn check(&self, profile: &str, title: &str, is_draft: bool) -> Option<Skip> {
        self.draft(profile, is_draft)
            .or_else(|| self.title(profile, title))
    }

    /// Whether a PR by `author`, matched to `profile`, is left off the
    /// reviews you owe: `authors` excludes its author.
    #[must_use]
    pub fn unlisted(&self, profile: &str, author: &str) -> bool {
        self.author(profile, author).is_some()
    }

    /// [`Skip::Author`] if `authors` excludes `author`: the last pattern
    /// that matches decides, `profile`'s checked after the global ones.
    #[must_use]
    pub fn author(&self, profile: &str, author: &str) -> Option<Skip> {
        let own = self.profiles.get(profile);
        let last = own
            .and_then(|p| p.authors.last_match(author))
            .or_else(|| self.authors.last_match(author));
        match last {
            Some((true, _)) => None,
            Some((false, pattern)) => Some(Skip::Author {
                login: author.to_owned(),
                pattern: Some(pattern.to_owned()),
            }),
            None => Some(Skip::Author {
                login: author.to_owned(),
                pattern: None,
            }),
        }
    }

    fn draft(&self, profile: &str, is_draft: bool) -> Option<Skip> {
        let own = self.profiles.get(profile);
        (is_draft && own.and_then(|p| p.drafts).unwrap_or(self.drafts)).then_some(Skip::Draft)
    }

    fn title(&self, profile: &str, title: &str) -> Option<Skip> {
        let own = self.profiles.get(profile);
        let pattern = self
            .titles
            .first_match(title)
            .or_else(|| own.and_then(|p| p.titles.first_match(title)))?;
        Some(Skip::Title {
            pattern: pattern.to_owned(),
        })
    }
}

/// A glob that matches exactly `title`: glob syntax in it is escaped.
#[must_use]
pub fn escape_title(title: &str) -> String {
    let mut glob = String::with_capacity(title.len());
    for c in title.chars() {
        match c {
            '*' | '?' | '[' | ']' | '{' | '}' | '\\' => {
                glob.push('[');
                glob.push(c);
                glob.push(']');
            }
            _ => glob.push(c),
        }
    }
    glob
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(global: &[&str], profile: &[&str]) -> SkipRules {
        let filter = |p: &[&str]| TitleFilter::new(p.iter().map(|s| (*s).into()).collect());
        SkipRules {
            titles: filter(global).unwrap(),
            authors: AuthorFilter::everyone(),
            drafts: true,
            profile_names: vec!["p".into()],
            profiles: HashMap::from([(
                "p".to_owned(),
                ProfileSkips {
                    titles: filter(profile).unwrap(),
                    authors: AuthorFilter::none(),
                    drafts: None,
                },
            )]),
        }
    }

    fn authors(logins: &[&str]) -> AuthorFilter {
        AuthorFilter::new(logins.iter().map(|s| (*s).into()).collect()).unwrap()
    }

    #[test]
    fn titles_match_ignoring_case_with_literal_parentheses() {
        let rules = rules(&["build(deps)*"], &[]);
        let skip = Skip::Title {
            pattern: "build(deps)*".into(),
        };
        assert_eq!(
            rules.check("p", "Build(deps): bump serde", false),
            Some(skip)
        );
        assert_eq!(rules.check("p", "build deps: bump serde", false), None);
        assert_eq!(rules.check("p", "chore: build(deps)", false), None);
    }

    #[test]
    fn profile_titles_add_to_the_global_ones() {
        let rules = rules(&["build(deps)*"], &["wip*"]);
        assert!(rules.check("p", "WIP: try things", false).is_some());
        assert!(rules.check("other", "WIP: try things", false).is_none());
        assert!(rules.check("other", "build(deps): x", false).is_some());
        assert_eq!(
            rules.check("p", "wip", false).unwrap().to_string(),
            "title matches `wip*`"
        );
    }

    #[test]
    fn drafts_are_skipped_unless_turned_off() {
        let mut rules = rules(&["wip*"], &[]);
        assert_eq!(rules.check("p", "wip", true), Some(Skip::Draft));
        assert_eq!(rules.check("other", "ok", false), None);
        rules.drafts = false;
        assert_eq!(rules.check("other", "ok", true), None);
        rules.profiles.get_mut("p").unwrap().drafts = Some(true);
        assert_eq!(rules.check("p", "ok", true), Some(Skip::Draft));
    }

    #[test]
    fn escaped_titles_match_only_themselves() {
        for title in [
            "fix: handle * and ? in [brackets]",
            "chore: {a,b} and a \\ backslash",
            "plain",
        ] {
            let glob = escape_title(title);
            let filter = TitleFilter::new(vec![glob.clone()]).unwrap();
            assert!(filter.first_match(title).is_some(), "{glob}");
            assert!(filter.first_match(&format!("{title}x")).is_none(), "{glob}");
        }
        assert_eq!(escape_title("build(deps): x*"), "build(deps): x[*]");
    }

    fn facts(reviewers: &[String]) -> PrFacts<'_> {
        PrFacts {
            profile: "p",
            title: "wip: x",
            author: "dependabot",
            is_draft: false,
            archived: false,
            head_sha: "0123456789abcdef",
            head_reviewers: reviewers,
            me: "Me",
        }
    }

    fn logins(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn reviewers_of_the_head_are_listed_you_first() {
        let rules = rules(&[], &[]);
        let reviewed = |names: &[&str]| {
            let names = logins(names);
            rules.decide(&facts(&names)).map(|s| s.status())
        };
        assert_eq!(reviewed(&[]), None);
        assert_eq!(reviewed(&["me"]).as_deref(), Some("reviewed by you"));
        assert_eq!(
            reviewed(&["alice", "ME", "alice"]).as_deref(),
            Some("reviewed by you, alice")
        );
        assert_eq!(
            reviewed(&["alice", "Alice"]).as_deref(),
            Some("reviewed by alice")
        );
        assert_eq!(
            reviewed(&["alice", "bob", "carol", "dave"]).as_deref(),
            Some("reviewed by alice, bob +2")
        );
        let names = logins(&["alice"]);
        assert_eq!(
            rules.decide(&facts(&names)).unwrap().to_string(),
            "already reviewed by alice at 01234567"
        );
    }

    #[test]
    fn archived_then_author_then_draft_then_reviewed_then_title() {
        let mut rules = rules(&["wip*"], &[]);
        rules.authors = authors(&["*", "!dependabot"]);
        let names = logins(&["alice"]);
        let base = facts(&names);
        let label = |pr: PrFacts<'_>| rules.decide(&pr).map(|s| s.label());
        assert_eq!(
            label(PrFacts {
                archived: true,
                is_draft: true,
                ..base
            }),
            Some("archived")
        );
        assert_eq!(
            label(PrFacts {
                is_draft: true,
                ..base
            }),
            Some("author")
        );
        let base = PrFacts {
            author: "alice",
            ..base
        };
        assert_eq!(
            label(PrFacts {
                is_draft: true,
                ..base
            }),
            Some("draft")
        );
        assert_eq!(label(base), Some("reviewed"));
        assert_eq!(
            label(PrFacts {
                head_reviewers: &[],
                ..base
            }),
            Some("title")
        );
        assert_eq!(
            label(PrFacts {
                head_reviewers: &[],
                title: "fix",
                ..base
            }),
            None
        );
    }

    #[test]
    fn the_last_author_pattern_decides_with_the_profiles_after() {
        let mut rules = rules(&[], &[]);
        rules.authors = authors(&["*", "!dependabot", "!bot-*", "bot-ok"]);
        assert!(!rules.unlisted("other", "alice"));
        assert!(rules.unlisted("other", "Dependabot"));
        assert!(rules.unlisted("other", "dependabot[bot]"));
        assert!(rules.unlisted("other", "bot-x"));
        assert!(!rules.unlisted("other", "BOT-OK"));
        assert!(!rules.unlisted("other", "dependabot-preview"));

        rules.profiles.get_mut("p").unwrap().authors = authors(&["dependabot", "!renovate"]);
        assert!(!rules.unlisted("p", "dependabot"));
        assert!(!rules.unlisted("p", "Dependabot[bot]"));
        assert!(rules.unlisted("p", "renovate[bot]"));
        assert!(!rules.unlisted("other", "renovate"));

        let skip = rules.author("p", "renovate").unwrap();
        assert_eq!(skip.to_string(), "author matches `!renovate`");
        assert_eq!(skip.status(), "skipped: author");
    }

    #[test]
    fn authors_no_pattern_matches_are_excluded() {
        let mut rules = rules(&[], &[]);
        rules.authors = authors(&["!dependabot"]);
        let skip = rules.author("other", "alice").unwrap();
        assert_eq!(skip.to_string(), "no `authors` pattern matches `alice`");
        rules.authors = authors(&["*", "!*"]);
        assert!(rules.unlisted("other", "alice"));
        rules.authors = authors(&[]);
        assert!(rules.unlisted("other", "alice"));
        assert!(!SkipRules::default().unlisted("other", "alice"));
    }

    #[test]
    fn author_patterns_that_cant_match_a_login_are_errors() {
        assert!(AuthorFilter::new(vec!["alice_acme".into(), "{a,b}*".into()]).is_ok());
        for (bad, why) in [
            ("app/dependabot", "not `app/dependabot`"),
            ("!dependabot[bot]", "without `[bot]`"),
            ("!*\\[bot\\]", "without `[bot]`"),
            ("x[[]BOT[]]", "without `[bot]`"),
            ("!", "it's empty"),
            ("", "it's empty"),
            ("[", "invalid author glob `[`"),
        ] {
            let err = format!("{:#}", AuthorFilter::new(vec![bad.into()]).unwrap_err());
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn bad_globs_are_errors() {
        let err = TitleFilter::new(vec!["[".into()]).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid title glob `[`"),
            "{err:#}"
        );
    }
}
