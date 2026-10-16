//! Which PRs manual reviews hold: `runner.manual_reviews`, the profiles
//! that override it, and the authors `auto_review_authors` lets through.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use color_eyre::eyre::{Result, bail};

use crate::pr::{TeamRef, without_bot_suffix};

/// Each team's members as last fetched, as lowercase logins.
pub type TeamMembers = HashMap<TeamRef, HashSet<String>>;

/// An `auto_review_authors` list: logins, and teams as `org/slug`, whose
/// PRs manual reviews don't hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoAuthors {
    /// Lowercase.
    pub logins: Vec<String>,
    pub teams: Vec<TeamRef>,
}

impl AutoAuthors {
    /// Fails on anything but a plain login or `org/slug`: a pattern could
    /// let through more authors than you meant, and each review spends
    /// tokens.
    pub fn new(entries: Vec<String>) -> Result<Self> {
        let mut auto = Self::default();
        for entry in entries {
            if entry.is_empty() {
                bail!("an entry is empty");
            }
            if without_bot_suffix(&entry).len() < entry.len() {
                bail!(
                    "`{entry}` can't match a PR author: GitHub gives app logins without \
                     `[bot]`; write `dependabot`"
                );
            }
            if entry.contains(['*', '?', '!', '[', ']', '{', '}']) {
                bail!("`{entry}` isn't a login or `org/team`: patterns aren't allowed");
            }
            // Each part goes into an API path, where `..` would widen a
            // team to its whole org.
            let name = |part: &str| {
                !part.is_empty()
                    && part.chars().any(|c| c.is_ascii_alphanumeric())
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            };
            match entry.split_once('/') {
                None if name(&entry) => auto.logins.push(entry.to_ascii_lowercase()),
                Some((org, slug)) if name(org) && name(slug) => {
                    auto.teams.push(TeamRef::new(org, slug));
                }
                _ => bail!("`{entry}` isn't a login or `org/team`"),
            }
        }
        Ok(auto)
    }

    /// Whether `author`, lowercase, is listed or in a listed team as
    /// `members` last had it. A team not yet fetched lets nobody through.
    fn lets_through(&self, author: &str, members: &TeamMembers) -> bool {
        self.logins.iter().any(|login| login == author)
            || self
                .teams
                .iter()
                .any(|team| members.get(team).is_some_and(|m| m.contains(author)))
    }
}

/// `runner.manual_reviews` and each profile's own `manual_reviews`, cheap
/// to clone and share with the scheduler, the worker, the TUI and the
/// dashboard. A profile's own wins either way; PRs of other profiles, and
/// of any the config no longer has, follow `runner.manual_reviews`. A PR
/// whose author `auto_review_authors` lets through is never held. The
/// default holds nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManualReviews {
    /// `runner.manual_reviews`.
    pub runner: bool,
    /// The profiles that set their own, in file order, with what they set.
    pub overrides: Vec<(String, bool)>,
    /// `runner.auto_review_authors`.
    pub auto: AutoAuthors,
    /// The profiles with their own `auto_review_authors`, added to the
    /// runner's for their PRs.
    pub profile_auto: Vec<(String, AutoAuthors)>,
    /// The members of the teams the lists name, as last fetched.
    pub members: Arc<TeamMembers>,
}

impl ManualReviews {
    /// Just `runner.manual_reviews`, with no profile overriding it.
    #[must_use]
    pub fn runner(on: bool) -> Self {
        Self {
            runner: on,
            ..Self::default()
        }
    }

    /// Whether a queued review of a PR by `author`, matched to `profile`,
    /// is held.
    #[must_use]
    pub fn holds(&self, profile: &str, author: &str) -> bool {
        self.holds_profile(profile) && !self.auto_reviews(profile, author)
    }

    /// Whether `profile` holds its PRs' queued reviews, but those of
    /// authors `auto_review_authors` lets through.
    #[must_use]
    pub fn holds_profile(&self, profile: &str) -> bool {
        self.overrides
            .iter()
            .find(|(name, _)| name == profile)
            .map_or(self.runner, |&(_, on)| on)
    }

    /// Whether `auto_review_authors` lets `author`'s PRs matched to
    /// `profile` through.
    #[must_use]
    pub fn auto_reviews(&self, profile: &str, author: &str) -> bool {
        let author = author.to_ascii_lowercase();
        self.auto.lets_through(&author, &self.members)
            || self
                .profile_auto
                .iter()
                .any(|(name, auto)| name == profile && auto.lets_through(&author, &self.members))
    }

    /// Every team the lists name, each once, for fetching their members.
    #[must_use]
    pub fn teams(&self) -> Vec<TeamRef> {
        let mut teams: Vec<TeamRef> = self
            .auto
            .teams
            .iter()
            .chain(self.profile_auto.iter().flat_map(|(_, auto)| &auto.teams))
            .cloned()
            .collect();
        teams.sort();
        teams.dedup();
        teams
    }

    /// Whether some profile holds its reviews, so the teams' members matter.
    #[must_use]
    pub fn holds_any(&self) -> bool {
        self.runner || self.holding_profiles().next().is_some()
    }

    /// `self` with `members` as the teams' members.
    #[must_use]
    pub fn with_members(mut self, members: Arc<TeamMembers>) -> Self {
        self.members = members;
        self
    }

    /// Whether `profile` follows `runner.manual_reviews`, so switching that
    /// switches whether its reviews are held.
    #[must_use]
    pub fn follows_runner(&self, profile: &str) -> bool {
        !self.overrides.iter().any(|(name, _)| name == profile)
    }

    /// Whether going from `self` to `after` starts the held review of a PR
    /// by `author` matched to `profile`.
    #[must_use]
    pub fn releases(&self, after: &Self, profile: &str, author: &str) -> bool {
        self.holds(profile, author) && !after.holds(profile, author)
    }

    /// Whether going from `self` to `after` may start some held review: a
    /// profile stops holding (one that neither names follows
    /// `runner.manual_reviews` in both), or who's let through changes.
    #[must_use]
    pub fn releases_any(&self, after: &Self) -> bool {
        let released = |name: &str| self.holds_profile(name) && !after.holds_profile(name);
        (self.runner && !after.runner)
            || self
                .overrides
                .iter()
                .chain(&after.overrides)
                .any(|(name, _)| released(name))
            || self.auto != after.auto
            || self.profile_auto != after.profile_auto
            || self.members != after.members
    }

    /// The profiles that hold their reviews while `runner.manual_reviews`
    /// is off, in file order.
    pub fn holding_profiles(&self) -> impl Iterator<Item = &str> {
        self.overrides
            .iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| name.as_str())
    }

    /// What the TUI's status bar and the dashboard's top bar say:
    /// `manual reviews` while `runner.manual_reviews` is on, `manual
    /// reviews: <profiles>` while it's off and those profiles hold theirs,
    /// and nothing while no reviews are held.
    #[must_use]
    pub fn badge(&self) -> Option<String> {
        if self.runner {
            return Some("manual reviews".into());
        }
        let holding: Vec<&str> = self.holding_profiles().collect();
        (!holding.is_empty()).then(|| format!("manual reviews: {}", holding.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manual(runner: bool, overrides: &[(&str, bool)]) -> ManualReviews {
        ManualReviews {
            runner,
            overrides: overrides
                .iter()
                .map(|&(name, on)| (name.into(), on))
                .collect(),
            ..ManualReviews::default()
        }
    }

    fn auto(entries: &[&str]) -> AutoAuthors {
        AutoAuthors::new(entries.iter().map(|&e| e.into()).collect()).unwrap()
    }

    #[test]
    fn listed_authors_and_fetched_team_members_are_let_through() {
        let team = TeamRef::new("Org", "Vuln-Backend");
        let mut m = ManualReviews {
            auto: auto(&["Alice", "org/vuln-backend"]),
            profile_auto: vec![("backend".into(), auto(&["bob"]))],
            ..ManualReviews::runner(true)
        };
        assert!(!m.holds("default", "alice"));
        assert!(m.holds("default", "alice-2"));
        // A profile's own adds to the runner's, for its PRs only.
        assert!(!m.holds("backend", "BOB"));
        assert!(m.holds("default", "bob"));
        // A team lets nobody through until its members are known.
        assert!(m.holds("default", "carol"));
        assert_eq!(m.teams(), std::slice::from_ref(&team));
        let before = m.clone();
        m = m.with_members(Arc::new(HashMap::from([(
            team,
            HashSet::from(["carol".to_owned()]),
        )])));
        assert!(!m.holds("default", "Carol"));
        assert!(before.releases(&m, "default", "carol"));
        assert!(before.releases_any(&m));
        assert!(!m.releases_any(&m.clone()));
        // It only lifts a hold.
        assert!(!ManualReviews::runner(false).holds("default", "dave"));
    }

    #[test]
    fn auto_review_authors_takes_only_logins_and_teams() {
        for bad in [
            "",
            "*",
            "al*ce",
            "!bob",
            "dependabot[bot]",
            "a/b/c",
            "/team",
            "org/",
            "acme/..",
            "acme/.",
            "../members",
            "acme/team%2f..",
            "acme/team#x",
            "a b",
        ] {
            assert!(
                AutoAuthors::new(vec![bad.into()]).is_err(),
                "{bad:?} should be refused"
            );
        }
        let ok = auto(&["Alice", "Org/Team"]);
        assert_eq!(ok.logins, ["alice"]);
        assert_eq!(ok.teams, [TeamRef::new("org", "team")]);
    }

    #[test]
    fn a_profiles_own_wins_either_way_and_others_follow_the_runner() {
        for runner in [false, true] {
            let m = manual(runner, &[("on", true), ("off", false)]);
            assert!(m.holds_profile("on"));
            assert!(!m.holds_profile("off"));
            // Unset, or a profile the config no longer has.
            assert_eq!(m.holds_profile("other"), runner);
            assert!(m.follows_runner("other"));
            assert!(!m.follows_runner("on") && !m.follows_runner("off"));
        }
    }

    #[test]
    fn releases_only_what_was_held_and_no_longer_is() {
        let before = manual(true, &[("ring", true), ("docs", false)]);
        let off = ManualReviews {
            runner: false,
            ..before.clone()
        };
        assert!(before.releases(&off, "default", "carol"));
        // Its own still holds it, and the other was never held.
        assert!(!before.releases(&off, "ring", "carol"));
        assert!(!before.releases(&off, "docs", "carol"));
        // Turning a profile's override off releases just that profile's.
        let ring_off = manual(false, &[("ring", false)]);
        assert!(off.releases(&ring_off, "ring", "carol"));
        assert!(!off.releases(&ring_off, "default", "carol"));
        assert!(!ring_off.releases(&before, "ring", "carol"));
        // Any profile: the runner's turned off, or one's own.
        assert!(before.releases_any(&off));
        assert!(off.releases_any(&ring_off));
        // A profile newly holding its own, or dropping an own that
        // matched the runner's, releases nothing.
        assert!(!off.releases_any(&before));
        assert!(!off.releases_any(&manual(false, &[("ring", true)])));
        assert!(!before.releases_any(&manual(true, &[])));
        // Unset, a profile whose own was on follows the runner's, off.
        assert!(manual(false, &[("ring", true)]).releases_any(&manual(false, &[])));
    }

    #[test]
    fn the_badge_names_the_holding_profiles_while_the_runner_is_off() {
        assert_eq!(
            manual(true, &[("ring", false)]).badge().as_deref(),
            Some("manual reviews")
        );
        let some = manual(false, &[("ring", true), ("docs", false), ("infra", true)]);
        assert_eq!(some.badge().as_deref(), Some("manual reviews: ring, infra"));
        let none = manual(false, &[("docs", false)]);
        assert_eq!(none.badge(), None);
    }
}
