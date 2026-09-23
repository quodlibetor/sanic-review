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
    /// The title matches `review_requests.skip_titles` or the profile's.
    Title { pattern: String },
}

impl Skip {
    /// A one-word reason, for the TUI's status column.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Title { .. } => "title",
        }
    }
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Title { pattern } => write!(f, "title matches `{pattern}`"),
        }
    }
}

/// The skip settings of a whole config, cheap to clone and share with the
/// scheduler and the TUI.
#[derive(Debug, Clone, Default)]
pub struct SkipRules {
    pub(crate) titles: TitleFilter,
    /// Each profile's own `skip_titles`, which add to the global ones.
    pub(crate) profiles: HashMap<String, TitleFilter>,
}

impl SkipRules {
    /// Why a PR matched to `profile` isn't reviewed automatically, if it
    /// isn't.
    #[must_use]
    pub fn check(&self, profile: &str, title: &str) -> Option<Skip> {
        let pattern = self.titles.first_match(title).or_else(|| {
            self.profiles
                .get(profile)
                .and_then(|titles| titles.first_match(title))
        })?;
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
            profiles: HashMap::from([("p".to_owned(), filter(profile).unwrap())]),
        }
    }

    #[test]
    fn titles_match_ignoring_case_with_literal_parentheses() {
        let rules = rules(&["build(deps)*"], &[]);
        let skip = Skip::Title {
            pattern: "build(deps)*".into(),
        };
        assert_eq!(rules.check("p", "Build(deps): bump serde"), Some(skip));
        assert_eq!(rules.check("p", "build deps: bump serde"), None);
        assert_eq!(rules.check("p", "chore: build(deps)"), None);
    }

    #[test]
    fn profile_titles_add_to_the_global_ones() {
        let rules = rules(&["build(deps)*"], &["wip*"]);
        assert!(rules.check("p", "WIP: try things").is_some());
        assert!(rules.check("other", "WIP: try things").is_none());
        assert!(rules.check("other", "build(deps): x").is_some());
        assert_eq!(
            rules.check("p", "wip").unwrap().to_string(),
            "title matches `wip*`"
        );
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
