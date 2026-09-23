//! Which PRs are never reviewed automatically. They're still tracked and
//! shown, and a review can still be started by hand.

use std::{collections::HashMap, fmt};

use color_eyre::eyre::{Result, WrapErr};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

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

    /// The first pattern `title` matches.
    #[must_use]
    pub fn first_match(&self, title: &str) -> Option<&str> {
        let first = self.set.matches(title).into_iter().min()?;
        Some(self.patterns[first].as_str())
    }
}

/// Why a PR isn't reviewed automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// You archived the PR.
    Archived,
    /// The PR is a draft, and `skip_drafts` is on.
    Draft,
    /// The title matches `review_requests.skip_titles` or the profile's.
    Title { pattern: String },
}

impl Skip {
    /// A one-word reason, for the TUI's status column.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Archived => "archived",
            Self::Draft => "draft",
            Self::Title { .. } => "title",
        }
    }
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Archived => write!(f, "it's archived"),
            Self::Draft => write!(f, "it's a draft"),
            Self::Title { pattern } => write!(f, "title matches `{pattern}`"),
        }
    }
}

/// The skip settings of a whole config, cheap to clone and share with the
/// scheduler and the TUI.
#[derive(Debug, Clone)]
pub struct SkipRules {
    pub(crate) titles: TitleFilter,
    pub(crate) drafts: bool,
    pub(crate) profiles: HashMap<String, ProfileSkips>,
}

impl Default for SkipRules {
    fn default() -> Self {
        Self {
            titles: TitleFilter::default(),
            drafts: true,
            profiles: HashMap::new(),
        }
    }
}

/// A profile's own skip settings.
#[derive(Debug, Clone, Default)]
pub struct ProfileSkips {
    /// Added to the global `skip_titles`.
    pub titles: TitleFilter,
    /// Overrides the global `skip_drafts`.
    pub drafts: Option<bool>,
}

impl SkipRules {
    /// Why a PR matched to `profile` isn't reviewed automatically, if it
    /// isn't.
    #[must_use]
    pub fn check(&self, profile: &str, title: &str, is_draft: bool) -> Option<Skip> {
        let own = self.profiles.get(profile);
        if is_draft && own.and_then(|p| p.drafts).unwrap_or(self.drafts) {
            return Some(Skip::Draft);
        }
        let pattern = self
            .titles
            .first_match(title)
            .or_else(|| own.and_then(|p| p.titles.first_match(title)))?;
        Some(Skip::Title {
            pattern: pattern.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(global: &[&str], profile: &[&str]) -> SkipRules {
        let filter = |p: &[&str]| TitleFilter::new(p.iter().map(|s| (*s).into()).collect());
        SkipRules {
            titles: filter(global).unwrap(),
            drafts: true,
            profiles: HashMap::from([(
                "p".to_owned(),
                ProfileSkips {
                    titles: filter(profile).unwrap(),
                    drafts: None,
                },
            )]),
        }
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
    fn bad_globs_are_errors() {
        let err = TitleFilter::new(vec!["[".into()]).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid title glob `[`"),
            "{err:#}"
        );
    }
}
