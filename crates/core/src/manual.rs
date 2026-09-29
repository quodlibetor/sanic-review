//! Which PRs manual reviews hold: `runner.manual_reviews`, and the
//! profiles that override it.

/// `runner.manual_reviews` and each profile's own `manual_reviews`, cheap
/// to clone and share with the scheduler, the worker, the TUI and the
/// dashboard. A profile's own wins either way; PRs of other profiles, and
/// of any the config no longer has, follow `runner.manual_reviews`. The
/// default holds nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManualReviews {
    /// `runner.manual_reviews`.
    pub runner: bool,
    /// The profiles that set their own, in file order, with what they set.
    pub overrides: Vec<(String, bool)>,
}

impl ManualReviews {
    /// Just `runner.manual_reviews`, with no profile overriding it.
    #[must_use]
    pub fn runner(on: bool) -> Self {
        Self {
            runner: on,
            overrides: Vec::new(),
        }
    }

    /// Whether queued reviews of `profile`'s PRs are held.
    #[must_use]
    pub fn holds(&self, profile: &str) -> bool {
        self.overrides
            .iter()
            .find(|(name, _)| name == profile)
            .map_or(self.runner, |&(_, on)| on)
    }

    /// Whether `profile` follows `runner.manual_reviews`, so switching that
    /// switches whether its reviews are held.
    #[must_use]
    pub fn follows_runner(&self, profile: &str) -> bool {
        !self.overrides.iter().any(|(name, _)| name == profile)
    }

    /// Whether going from `self` to `after` starts `profile`'s held
    /// reviews.
    #[must_use]
    pub fn releases(&self, after: &Self, profile: &str) -> bool {
        self.holds(profile) && !after.holds(profile)
    }

    /// Whether going from `self` to `after` starts any profile's held
    /// reviews: one that neither names follows `runner.manual_reviews` in
    /// both.
    #[must_use]
    pub fn releases_any(&self, after: &Self) -> bool {
        (self.runner && !after.runner)
            || self
                .overrides
                .iter()
                .chain(&after.overrides)
                .any(|(name, _)| self.releases(after, name))
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
        }
    }

    #[test]
    fn a_profiles_own_wins_either_way_and_others_follow_the_runner() {
        for runner in [false, true] {
            let m = manual(runner, &[("on", true), ("off", false)]);
            assert!(m.holds("on"));
            assert!(!m.holds("off"));
            // Unset, or a profile the config no longer has.
            assert_eq!(m.holds("other"), runner);
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
        assert!(before.releases(&off, "default"));
        // Its own still holds it, and the other was never held.
        assert!(!before.releases(&off, "ring"));
        assert!(!before.releases(&off, "docs"));
        // Turning a profile's override off releases just that profile's.
        let ring_off = manual(false, &[("ring", false)]);
        assert!(off.releases(&ring_off, "ring"));
        assert!(!off.releases(&ring_off, "default"));
        assert!(!ring_off.releases(&before, "ring"));
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
