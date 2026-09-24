//! Turning GitHub state into stored snapshots and detected triggers.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

use sanic_core::pr::TeamRef;

use sanic_core::{
    clock::{Clock, SystemClock, rfc3339, window_start},
    config::Config,
    pr::{PrKey, PrSnapshot},
    trigger::{Trigger, detect},
};
use sanic_github::{ApiError, Client, NotificationPoll};
use sanic_store::{List, Store};

const LAST_MODIFIED_KEY: &str = "notifications.last_modified";
const REVIEW_REQUESTED: &str = "review-requested:@me";
const INVOLVES: &str = "involves:@me";

/// Search `qualifiers`, limited to PRs updated from the day of `since` on
/// when there's a window. GitHub's `updated:` works in whole days, so a
/// refresh checks the exact time.
#[must_use]
pub fn recent(qualifiers: &str, since: Option<&str>) -> String {
    match since.and_then(|s| s.get(..10)) {
        Some(day) => format!("{qualifiers} updated:>={day}"),
        None => qualifiers.to_owned(),
    }
}

/// The longest search GitHub takes.
const MAX_SEARCH: usize = 256;

/// Search `qualifiers` for PRs quiet since before `day` (`YYYY-MM-DD`),
/// kept to watched repos by `scope`: as many of its qualifiers as fit in
/// GitHub's longest search.
#[must_use]
pub fn older(qualifiers: &str, day: &str, scope: &[String]) -> String {
    let mut q = format!("{qualifiers} updated:<{day}");
    // With the `is:open is:pr ` the search adds.
    let room = MAX_SEARCH - "is:open is:pr ".len();
    for (i, qualifier) in scope.iter().enumerate() {
        if q.len() + 1 + qualifier.len() > room {
            tracing::debug!(
                "hidden count: {} watched orgs and repos don't fit in a search, so PRs in \
                 them aren't counted",
                scope.len() - i
            );
            break;
        }
        q.push(' ');
        q.push_str(qualifier);
    }
    q
}

/// The GitHub reads the poller needs; a trait so tests can fake GitHub.
pub trait GithubApi {
    fn notifications(
        &self,
        if_modified_since: Option<&str>,
    ) -> impl Future<Output = Result<NotificationPoll, ApiError>>;
    fn search_prs(&self, qualifiers: &str) -> impl Future<Output = Result<Vec<PrKey>, ApiError>>;
    fn count_prs(&self, qualifiers: &str) -> impl Future<Output = Result<u32, ApiError>>;
    fn my_teams(&self) -> impl Future<Output = Result<Vec<TeamRef>, ApiError>>;
    fn pull_request(
        &self,
        key: &PrKey,
        me: &str,
        with_files: bool,
    ) -> impl Future<Output = Result<Option<PrSnapshot>, ApiError>>;
}

impl GithubApi for Client {
    fn notifications(
        &self,
        if_modified_since: Option<&str>,
    ) -> impl Future<Output = Result<NotificationPoll, ApiError>> {
        Client::notifications(self, if_modified_since)
    }

    fn search_prs(&self, qualifiers: &str) -> impl Future<Output = Result<Vec<PrKey>, ApiError>> {
        Client::search_prs(self, qualifiers)
    }

    fn count_prs(&self, qualifiers: &str) -> impl Future<Output = Result<u32, ApiError>> {
        Client::count_prs(self, qualifiers)
    }

    fn my_teams(&self) -> impl Future<Output = Result<Vec<TeamRef>, ApiError>> {
        Client::my_teams(self)
    }

    fn pull_request(
        &self,
        key: &PrKey,
        me: &str,
        with_files: bool,
    ) -> impl Future<Output = Result<Option<PrSnapshot>, ApiError>> {
        Client::pull_request(self, key, me, with_files)
    }
}

/// What a reconcile found, in watched repos.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// PRs that request your review.
    pub requested: Vec<PrKey>,
    /// The other PRs that involve you.
    pub involved: Vec<PrKey>,
}

/// How soon a queued PR is refreshed: earlier variants first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// A reconcile found it requesting your review.
    Requested,
    /// A reconcile found it involving you.
    Involved,
    /// Only a notification pointed at it.
    Notified,
}

/// PRs waiting to be refreshed, most urgent first and each once. A PR
/// queued again keeps its more urgent priority.
#[derive(Debug, Default)]
pub struct RefreshQueue {
    order: BTreeSet<(Priority, PrKey)>,
    queued: HashMap<PrKey, Priority>,
}

impl RefreshQueue {
    pub fn push(&mut self, key: PrKey, priority: Priority) {
        match self.queued.get(&key) {
            Some(&queued) if queued <= priority => return,
            Some(&queued) => {
                self.order.remove(&(queued, key.clone()));
            }
            None => {}
        }
        self.order.insert((priority, key.clone()));
        self.queued.insert(key, priority);
    }

    pub fn extend(&mut self, keys: impl IntoIterator<Item = PrKey>, priority: Priority) {
        for key in keys {
            self.push(key, priority);
        }
    }

    pub fn pop(&mut self) -> Option<(PrKey, Priority)> {
        let (priority, key) = self.order.pop_first()?;
        self.queued.remove(&key);
        Some((key, priority))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// How far through its queue a refresh batch is, for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

/// A refreshed PR that matched the config.
#[derive(Debug)]
pub struct Refreshed {
    pub snapshot: PrSnapshot,
    pub profile: String,
    pub triggers: Vec<Trigger>,
}

pub struct Poller<G> {
    github: G,
    store: Store,
    config: Config,
    me: String,
    /// Your teams, refreshed on each reconcile. A team review request counts
    /// as yours only if you're in the team and the config's filter allows it.
    teams: HashSet<TeamRef>,
    /// Dates `poll.updated_within_days`.
    clock: Arc<dyn Clock>,
}

impl<G: GithubApi> Poller<G> {
    pub fn new(github: G, store: Store, config: Config, me: String) -> Self {
        Self {
            github,
            store,
            config,
            me,
            teams: HashSet::new(),
            clock: Arc::new(SystemClock),
        }
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The recency window in days: the one picked on the dashboard, else
    /// `poll.updated_within_days`. `None` for no limit.
    pub fn window(&self) -> color_eyre::eyre::Result<Option<u32>> {
        Ok(match self.store.window_choice()? {
            Some(choice) => choice.days(),
            None => self.config.poll.updated_within_days,
        })
    }

    /// Where the recency window starts, as GitHub writes times.
    pub fn window_start(&self) -> color_eyre::eyre::Result<Option<String>> {
        Ok(window_start(self.clock.now(), self.window()?))
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The GitHub login everything is judged relative to.
    pub fn me(&self) -> &str {
        &self.me
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Open PRs that request your review or involve you, in watched repos.
    /// Also refreshes your team memberships, and marks tracked PRs missing
    /// from the result as no longer open.
    pub async fn reconcile(&mut self) -> Result<Reconciled, ApiError> {
        // Without teams, team requests just stop counting; discovery still
        // works, so a lookup failure shouldn't stop the reconcile.
        match self.github.my_teams().await {
            Ok(teams) => self.teams = teams.into_iter().collect(),
            Err(ApiError::Other(report)) => tracing::warn!(
                "listing your teams failed; team review requests use the last known teams \
                 (the token needs `read:org`): {report:?}"
            ),
            Err(err) => return Err(err),
        }
        let window = self.window()?;
        let since = window_start(self.clock.now(), window);
        let requested = self
            .github
            .search_prs(&recent(REVIEW_REQUESTED, since.as_deref()))
            .await?;
        let involved = self
            .github
            .search_prs(&recent(INVOLVES, since.as_deref()))
            .await?;
        let watched = |keys: Vec<PrKey>| -> BTreeSet<PrKey> {
            keys.into_iter()
                .filter(|k| self.config.watches(&k.repo))
                .collect()
        };
        let requested = watched(requested);
        let involved: BTreeSet<PrKey> = &watched(involved) - &requested;
        let found: Vec<PrKey> = requested.union(&involved).cloned().collect();
        self.store.keep_open(&found)?;
        if let (Some(days), Some(day)) = (window, since.as_deref().and_then(|s| s.get(..10))) {
            self.count_hidden(days, day).await?;
        }
        Ok(Reconciled {
            requested: requested.into_iter().collect(),
            involved: involved.into_iter().collect(),
        })
    }

    /// Counts each list's open PRs quiet since before `day`, which the
    /// window of `days` leaves out, for the dashboard to say so. Only
    /// review requests count as owed here, and the count can't apply the
    /// team filter or path globs; a count that fails is logged and left
    /// as it was. Only a rejected token fails the reconcile: the PRs it
    /// found still get refreshed when a count is rate limited.
    async fn count_hidden(&mut self, days: u32, day: &str) -> Result<(), ApiError> {
        let scope = self.config.search_scope();
        for (list, qualifiers) in [
            (List::Owed, "review-requested:@me -author:@me"),
            (List::Mine, "author:@me"),
        ] {
            match self.github.count_prs(&older(qualifiers, day, &scope)).await {
                Ok(count) => self.store.set_hidden(list, days, count)?,
                Err(ApiError::Other(report)) => {
                    tracing::warn!("counting PRs older than the window failed: {report:?}");
                }
                Err(ApiError::RateLimited { .. }) => {
                    tracing::warn!("counting PRs older than the window was rate limited");
                    break;
                }
                Err(err @ ApiError::Unauthorized) => return Err(err),
            }
        }
        Ok(())
    }

    /// PRs in watched repos with notifications since the last poll, and
    /// GitHub's requested poll interval. Notifications last updated before
    /// `poll.updated_within_days`, or before their PR was found closed, are
    /// dropped without fetching it: on a first poll that can be most of
    /// them.
    pub async fn poll_notifications(&mut self) -> Result<(Vec<PrKey>, Option<Duration>), ApiError> {
        let since = self.store.poll_state(LAST_MODIFIED_KEY)?;
        match self.github.notifications(since.as_deref()).await? {
            NotificationPoll::NotModified { poll_interval } => Ok((Vec::new(), poll_interval)),
            NotificationPoll::Changed {
                notifications,
                last_modified,
                poll_interval,
            } => {
                if let Some(last_modified) = last_modified {
                    self.store
                        .set_poll_state(LAST_MODIFIED_KEY, &last_modified)?;
                }
                let since = self.window_start()?;
                let mut keys = Vec::new();
                for n in notifications {
                    let Some(key) = n.pr else { continue };
                    if since.as_deref().is_some_and(|s| n.updated_at.as_str() < s)
                        || !self.config.watches(&key.repo)
                    {
                        continue;
                    }
                    // Found closed after this notification's last update:
                    // it can't say anything new.
                    if self
                        .store
                        .closed_at(&key)?
                        .is_some_and(|checked| n.updated_at <= checked)
                    {
                        continue;
                    }
                    keys.push(key);
                }
                Ok((keys, poll_interval))
            }
        }
    }

    /// Fetches `key`, detects triggers against the stored baseline, and
    /// stores the new baseline. `None` if the PR is gone, closed, quiet for
    /// longer than `poll.updated_within_days` or matches no profile; a gone
    /// or closed PR is marked no longer open.
    pub async fn refresh(&mut self, key: &PrKey) -> Result<Option<Refreshed>, ApiError> {
        let with_files = self.config.needs_files(&key.repo);
        let Some(mut snapshot) = self.github.pull_request(key, &self.me, with_files).await? else {
            tracing::debug!("not open or not visible; skipping");
            self.store.mark_closed(key)?;
            self.store
                .remember_closed(key, &rfc3339(self.clock.now()))?;
            return Ok(None);
        };
        // Open, whether or not the checks below skip it.
        self.store.forget_closed(key)?;
        if let (Some(start), Some(updated)) = (self.window_start()?, &snapshot.updated_at)
            && updated.as_str() < start.as_str()
        {
            tracing::debug!(updated = %updated, "no activity within `poll.updated_within_days`; skipping");
            return Ok(None);
        }
        let filter = &self.config.review_requests.teams;
        snapshot.review_requested |= snapshot
            .requested_teams
            .iter()
            .any(|t| self.teams.contains(t) && filter.allows(t));
        let files = snapshot.files.as_deref().unwrap_or_default();
        let Some(matched) = self.config.match_pr(&key.repo, files) else {
            tracing::debug!("matches no profile; skipping");
            return Ok(None);
        };
        let profile = matched.profile.name.clone();
        let known = self.store.known(key)?;
        let triggers = detect(&self.me, known.as_ref(), &snapshot);
        self.store
            .record(&snapshot, &self.me, &profile, &triggers)?;
        Ok(Some(Refreshed {
            snapshot,
            profile,
            triggers,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, path::Path};

    use color_eyre::eyre::{Result, eyre};
    use sanic_core::{
        clock::WindowChoice,
        config::{CheckoutResolver, Vcs},
        pr::{CONVERSATION_THREAD, Comment, Placement, Thread},
        repo::RepoName,
    };
    use sanic_github::Notification;

    use super::*;

    #[derive(Default)]
    struct FakeGithub {
        /// By the exact qualifiers searched for.
        searches: HashMap<String, Vec<PrKey>>,
        /// Counts, by the exact qualifiers counted.
        counts: HashMap<String, u32>,
        /// Every search and count, in order.
        searched: RefCell<Vec<String>>,
        prs: RefCell<HashMap<PrKey, PrSnapshot>>,
        notifications: Vec<Notification>,
        seen_since: RefCell<Vec<Option<String>>>,
        /// Every PR `pull_request` was asked for, in order.
        fetched: RefCell<Vec<PrKey>>,
        teams: Vec<TeamRef>,
    }

    // The fake answers immediately; `async fn` keeps it readable.
    #[allow(clippy::unused_async_trait_impl)]
    impl GithubApi for FakeGithub {
        async fn notifications(
            &self,
            if_modified_since: Option<&str>,
        ) -> Result<NotificationPoll, ApiError> {
            self.seen_since
                .borrow_mut()
                .push(if_modified_since.map(String::from));
            Ok(NotificationPoll::Changed {
                notifications: self.notifications.clone(),
                last_modified: Some("lm-1".into()),
                poll_interval: Some(Duration::from_secs(60)),
            })
        }

        async fn search_prs(&self, qualifiers: &str) -> Result<Vec<PrKey>, ApiError> {
            self.searched.borrow_mut().push(qualifiers.to_owned());
            Ok(self.searches.get(qualifiers).cloned().unwrap_or_default())
        }

        async fn count_prs(&self, qualifiers: &str) -> Result<u32, ApiError> {
            self.searched.borrow_mut().push(qualifiers.to_owned());
            Ok(self.counts.get(qualifiers).copied().unwrap_or_default())
        }

        async fn my_teams(&self) -> Result<Vec<TeamRef>, ApiError> {
            Ok(self.teams.clone())
        }

        async fn pull_request(
            &self,
            key: &PrKey,
            _me: &str,
            with_files: bool,
        ) -> Result<Option<PrSnapshot>, ApiError> {
            self.fetched.borrow_mut().push(key.clone());
            Ok(self.prs.borrow().get(key).cloned().map(|mut s| {
                if !with_files {
                    s.files = None;
                }
                s
            }))
        }
    }

    struct NoCheckouts;

    impl CheckoutResolver for NoCheckouts {
        fn resolve(&self, path: &Path, _: Option<&str>) -> Result<(Vcs, RepoName)> {
            Err(eyre!("unexpected checkout {}", path.display()))
        }
    }

    fn config() -> Config {
        config_with("")
    }

    fn config_with(extra: &str) -> Config {
        Config::parse(
            &format!(
                r#"
            {extra}
            [profile.vuln]
            repos = [{{ github = "org/repo", paths = ["vuln/**"] }}]
            [profile.default]
            repos = [{{ github = "org" }}]
            "#
            ),
            Path::new("/"),
            &NoCheckouts,
        )
        .unwrap()
    }

    fn key(repo: &str, number: u32) -> PrKey {
        PrKey {
            repo: RepoName::parse(repo).unwrap(),
            number,
        }
    }

    fn snapshot(key: &PrKey, author: &str, files: &[&str]) -> PrSnapshot {
        PrSnapshot {
            key: key.clone(),
            title: "t".into(),
            body: String::new(),
            url: key.url(),
            author: author.into(),
            head_sha: "h1".into(),
            base_sha: "b".into(),
            is_draft: false,
            review_requested: false,
            requested_teams: vec![],
            reviews: vec![],
            threads: vec![Thread {
                id: CONVERSATION_THREAD.into(),
                path: None,
                line: None,
                resolved: false,
                place: Placement::default(),
                comments: vec![],
            }],
            files: Some(files.iter().map(|f| (*f).into()).collect()),
            updated_at: None,
            review_decision: None,
            merge_state: None,
            checks: None,
        }
    }

    /// 2026-09-23T16:33:51Z.
    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH + Duration::from_secs(1_790_181_231)
        }
    }

    /// The qualifiers a reconcile at [`FixedClock`] searches for, with the
    /// default two-week window.
    fn searched(qualifiers: &str) -> String {
        format!("{qualifiers} updated:>=2026-09-09")
    }

    fn poller(github: FakeGithub) -> Poller<FakeGithub> {
        Poller::new(
            github,
            Store::open_in_memory().unwrap(),
            config(),
            "me".into(),
        )
        .with_clock(Arc::new(FixedClock))
    }

    #[tokio::test]
    async fn reconcile_merges_searches_and_drops_unwatched_repos() {
        let mut github = FakeGithub::default();
        github.searches.insert(
            searched(REVIEW_REQUESTED),
            vec![key("org/repo", 1), key("elsewhere/x", 9)],
        );
        github.searches.insert(
            searched(INVOLVES),
            vec![key("org/repo", 1), key("org/other", 2)],
        );
        let found = poller(github).reconcile().await.unwrap();
        assert_eq!(
            found,
            Reconciled {
                requested: vec![key("org/repo", 1)],
                involved: vec![key("org/other", 2)],
            }
        );
    }

    #[test]
    fn review_requests_are_refreshed_first_and_each_pr_once() {
        let mut queue = RefreshQueue::default();
        queue.push(key("org/a", 1), Priority::Notified);
        queue.push(key("org/b", 2), Priority::Involved);
        queue.push(key("org/z", 3), Priority::Requested);
        // Queued again: a more urgent priority wins, a less urgent one
        // changes nothing.
        queue.push(key("org/a", 1), Priority::Requested);
        queue.push(key("org/z", 3), Priority::Notified);
        assert_eq!(queue.len(), 3);
        let order: Vec<_> = std::iter::from_fn(|| queue.pop()).collect();
        assert_eq!(
            order,
            [
                (key("org/a", 1), Priority::Requested),
                (key("org/z", 3), Priority::Requested),
                (key("org/b", 2), Priority::Involved),
            ]
        );
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn prs_leave_the_open_set_when_closed_or_no_longer_found() {
        let requested = key("org/a", 1);
        let merged = key("org/b", 2);
        let github = FakeGithub::default();
        for pr in [&requested, &merged] {
            let mut snap = snapshot(pr, "alice", &[]);
            snap.review_requested = true;
            github.prs.borrow_mut().insert(pr.clone(), snap);
        }
        let mut poller = poller(github);
        poller.refresh(&requested).await.unwrap().unwrap();
        poller.refresh(&merged).await.unwrap().unwrap();
        let owed = |poller: &Poller<FakeGithub>| -> Vec<u32> {
            let owed = poller.store().owed_reviews("me", None).unwrap();
            owed.into_iter().map(|pr| pr.key.number).collect()
        };
        assert_eq!(owed(&poller), [1, 2]);

        // GitHub stops returning the merged PR.
        poller.github.prs.borrow_mut().remove(&merged);
        assert!(poller.refresh(&merged).await.unwrap().is_none());
        assert_eq!(owed(&poller), [1]);

        // A reconcile that no longer finds a PR closes it too.
        poller
            .github
            .searches
            .insert(searched(REVIEW_REQUESTED), vec![merged.clone()]);
        poller.reconcile().await.unwrap();
        assert_eq!(owed(&poller), Vec::<u32>::new());
    }

    #[test]
    fn searches_are_limited_to_the_window_by_day() {
        assert_eq!(
            recent(REVIEW_REQUESTED, Some("2026-09-09T16:33:51Z")),
            "review-requested:@me updated:>=2026-09-09"
        );
        assert_eq!(recent(INVOLVES, None), "involves:@me");
    }

    #[tokio::test]
    async fn prs_quiet_for_longer_than_the_window_are_ignored() {
        let old = key("org/a", 1);
        let github = FakeGithub::default();
        let mut snap = snapshot(&old, "alice", &[]);
        snap.review_requested = true;
        // The window starts 2026-09-09T16:33:51Z.
        snap.updated_at = Some("2026-09-09T16:33:50Z".into());
        github.prs.borrow_mut().insert(old.clone(), snap.clone());
        let mut poller = poller(github);
        assert!(poller.refresh(&old).await.unwrap().is_none());
        assert_eq!(poller.store().known(&old).unwrap(), None);

        snap.updated_at = Some("2026-09-09T16:33:51Z".into());
        poller.github.prs.borrow_mut().insert(old.clone(), snap);
        assert!(poller.refresh(&old).await.unwrap().is_some());

        // Without a window, any age counts.
        let mut config = config_with("[poll]\nupdated_within_days = 0");
        std::mem::swap(&mut config, &mut poller.config);
        assert_eq!(poller.window_start().unwrap(), None);
    }

    #[tokio::test]
    async fn a_window_picked_on_the_dashboard_wins_until_reset() {
        let mut poller = poller(FakeGithub::default());
        poller
            .store()
            .set_window_choice(Some(WindowChoice::Days(30)))
            .unwrap();
        assert_eq!(poller.window().unwrap(), Some(30));
        poller.reconcile().await.unwrap();
        poller
            .store()
            .set_window_choice(Some(WindowChoice::All))
            .unwrap();
        assert_eq!(poller.window_start().unwrap(), None);
        poller.reconcile().await.unwrap();
        poller.store().set_window_choice(None).unwrap();
        assert_eq!(poller.window().unwrap(), Some(14));
        assert_eq!(
            poller.github.searched.borrow()[..],
            [
                "review-requested:@me updated:>=2026-08-24",
                "involves:@me updated:>=2026-08-24",
                "review-requested:@me -author:@me updated:<2026-08-24 user:org",
                "author:@me updated:<2026-08-24 user:org",
                "review-requested:@me",
                "involves:@me",
            ]
        );
    }

    #[tokio::test]
    async fn a_reconcile_counts_what_the_window_hides() {
        let github = FakeGithub {
            counts: HashMap::from([
                (
                    "review-requested:@me -author:@me updated:<2026-09-09 user:org".to_owned(),
                    9,
                ),
                ("author:@me updated:<2026-09-09 user:org".to_owned(), 3),
            ]),
            ..FakeGithub::default()
        };
        let mut poller = poller(github);
        poller.reconcile().await.unwrap();
        assert_eq!(
            poller.store().hidden(List::Owed, Some(14)).unwrap(),
            Some(9)
        );
        assert_eq!(
            poller.store().hidden(List::Mine, Some(14)).unwrap(),
            Some(3)
        );
        // No window hides nothing, so nothing is counted.
        poller
            .store()
            .set_window_choice(Some(WindowChoice::All))
            .unwrap();
        let searches = poller.github.searched.borrow().len();
        poller.reconcile().await.unwrap();
        assert_eq!(poller.github.searched.borrow().len(), searches + 2);
    }

    #[test]
    fn hidden_counts_keep_to_what_fits_in_a_search() {
        let scope: Vec<String> = (0..40).map(|i| format!("repo:org/repo-{i}")).collect();
        let q = older("author:@me", "2026-09-09", &scope);
        assert!(q.starts_with("author:@me updated:<2026-09-09 repo:org/repo-0 "));
        assert!(q.len() + "is:open is:pr ".len() <= MAX_SEARCH, "{q}");
        assert_eq!(
            older("author:@me", "2026-09-09", &[]),
            "author:@me updated:<2026-09-09"
        );
    }

    /// Refreshes a PR that requests review from `requested`, as a member of
    /// `mine`, and reports whether that counted as a request to you.
    async fn team_request_counts(requested: &str, mine: &[&str], filter: &str) -> bool {
        let team = |t: &str| {
            let (org, slug) = t.split_once('/').unwrap();
            TeamRef::new(org, slug)
        };
        let pr = key("org/other", 2);
        let mut snap = snapshot(&pr, "alice", &[]);
        snap.requested_teams = vec![team(requested)];
        let github = FakeGithub {
            teams: mine.iter().map(|t| team(t)).collect(),
            ..FakeGithub::default()
        };
        github.prs.borrow_mut().insert(pr.clone(), snap);
        let mut poller = Poller::new(
            github,
            Store::open_in_memory().unwrap(),
            config_with(filter),
            "me".into(),
        );
        poller.reconcile().await.unwrap();
        let refreshed = poller.refresh(&pr).await.unwrap().unwrap();
        matches!(
            refreshed.triggers.as_slice(),
            [Trigger::ReviewRequested { .. }]
        )
    }

    #[tokio::test]
    async fn team_requests_count_for_member_teams_the_filter_allows() {
        assert!(team_request_counts("org/core", &["org/core"], "").await);
        assert!(!team_request_counts("org/core", &["org/other"], "").await);
        let exclude = "[review_requests]\nteams = [\"*\", \"!core\"]";
        assert!(!team_request_counts("org/core", &["org/core"], exclude).await);
    }

    #[tokio::test]
    async fn refresh_uses_files_to_pick_the_profile() {
        let pr = key("org/repo", 1);
        let github = FakeGithub::default();
        github
            .prs
            .borrow_mut()
            .insert(pr.clone(), snapshot(&pr, "alice", &["vuln/a.rs"]));
        let mut poller = poller(github);
        assert_eq!(poller.refresh(&pr).await.unwrap().unwrap().profile, "vuln");
    }

    #[tokio::test]
    async fn second_refresh_sees_new_replies() {
        let pr = key("org/other", 3);
        let mut snap = snapshot(&pr, "alice", &[]);
        snap.threads[0].comments.push(Comment {
            id: "c1".into(),
            author: "me".into(),
            body: "q".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            url: None,
            by_bot: false,
            reacted_at: None,
            reactions: vec![],
        });
        let github = FakeGithub::default();
        github.prs.borrow_mut().insert(pr.clone(), snap.clone());
        let mut poller = poller(github);
        assert_eq!(poller.refresh(&pr).await.unwrap().unwrap().triggers, []);

        snap.threads[0].comments.push(Comment {
            id: "c2".into(),
            author: "alice".into(),
            body: "a".into(),
            created_at: "2026-01-02T00:00:00Z".into(),
            url: None,
            by_bot: false,
            reacted_at: None,
            reactions: vec![],
        });
        poller.github.prs.borrow_mut().insert(pr.clone(), snap);
        let triggers = poller.refresh(&pr).await.unwrap().unwrap().triggers;
        assert!(matches!(triggers.as_slice(), [Trigger::Reply { .. }]));
        assert_eq!(poller.store().events().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn notifications_outside_the_window_fetch_nothing() {
        let old = |i: u32| Notification {
            id: i.to_string(),
            reason: "comment".into(),
            // The window starts 2026-09-09T16:33:51Z.
            updated_at: "2026-09-09T16:33:50Z".into(),
            pr: Some(key("org/repo", i)),
        };
        let mut notifications: Vec<Notification> = (1..=1000).map(old).collect();
        notifications.push(Notification {
            updated_at: "2026-09-20T00:00:00Z".into(),
            ..old(1001)
        });
        let github = FakeGithub {
            notifications,
            ..FakeGithub::default()
        };
        let mut poller = poller(github);
        let (keys, _) = poller.poll_notifications().await.unwrap();
        assert_eq!(keys, [key("org/repo", 1001)]);
        for key in &keys {
            poller.refresh(key).await.unwrap();
        }
        assert_eq!(*poller.github.fetched.borrow(), keys);
        // Last-Modified still moves on, so the old ones aren't seen again.
        assert_eq!(
            poller
                .store()
                .poll_state(LAST_MODIFIED_KEY)
                .unwrap()
                .as_deref(),
            Some("lm-1")
        );
    }

    #[tokio::test]
    async fn a_pr_found_closed_is_only_refetched_for_newer_notifications() {
        let closed = key("org/repo", 5);
        let notified = |updated_at: &str| Notification {
            id: "n".into(),
            reason: "comment".into(),
            updated_at: updated_at.into(),
            pr: Some(closed.clone()),
        };
        // The fake has no such PR, so a refresh finds it closed at the
        // clock's 2026-09-23T16:33:51Z.
        let mut poller = poller(FakeGithub {
            notifications: vec![notified("2026-09-23T16:00:00Z")],
            ..FakeGithub::default()
        });
        let (keys, _) = poller.poll_notifications().await.unwrap();
        assert_eq!(keys, std::slice::from_ref(&closed));
        assert!(poller.refresh(&closed).await.unwrap().is_none());
        assert_eq!(poller.github.fetched.borrow().len(), 1);

        // The same notification again: nothing to fetch.
        let (keys, _) = poller.poll_notifications().await.unwrap();
        assert!(keys.is_empty());
        // Newer activity (it may have been reopened) is looked at.
        poller.github.notifications = vec![notified("2026-09-23T17:00:00Z")];
        let (keys, _) = poller.poll_notifications().await.unwrap();
        assert_eq!(keys, std::slice::from_ref(&closed));

        // Found open, even too quiet to record, it's forgotten.
        let mut snap = snapshot(&closed, "alice", &[]);
        snap.updated_at = Some("2026-09-01T00:00:00Z".into());
        poller.github.prs.borrow_mut().insert(closed.clone(), snap);
        assert!(poller.refresh(&closed).await.unwrap().is_none());
        assert_eq!(poller.store().closed_at(&closed).unwrap(), None);
    }

    #[tokio::test]
    async fn notifications_resume_from_last_modified() {
        let github = FakeGithub {
            notifications: vec![
                Notification {
                    id: "1".into(),
                    reason: "comment".into(),
                    updated_at: "2026-09-20T00:00:00Z".into(),
                    pr: Some(key("org/repo", 1)),
                },
                Notification {
                    id: "2".into(),
                    reason: "comment".into(),
                    updated_at: "2026-09-20T00:00:00Z".into(),
                    pr: Some(key("elsewhere/x", 1)),
                },
            ],
            ..FakeGithub::default()
        };
        let mut poller = poller(github);
        let (keys, _) = poller.poll_notifications().await.unwrap();
        assert_eq!(keys, [key("org/repo", 1)]);
        poller.poll_notifications().await.unwrap();
        assert_eq!(
            *poller.github.seen_since.borrow(),
            [None, Some("lm-1".into())]
        );
    }
}
