//! Turning GitHub state into stored snapshots and detected triggers.

use std::{collections::HashSet, future::Future, time::Duration};

use sanic_core::pr::TeamRef;

use sanic_core::{
    config::Config,
    pr::{PrKey, PrSnapshot},
    trigger::{Trigger, detect},
};
use sanic_github::{ApiError, Client, NotificationPoll};
use sanic_store::Store;

const LAST_MODIFIED_KEY: &str = "notifications.last_modified";
const REVIEW_REQUESTED: &str = "review-requested:@me";
const INVOLVES: &str = "involves:@me";

/// The GitHub reads the poller needs; a trait so tests can fake GitHub.
pub trait GithubApi {
    fn notifications(
        &self,
        if_modified_since: Option<&str>,
    ) -> impl Future<Output = Result<NotificationPoll, ApiError>>;
    fn search_prs(&self, qualifiers: &str) -> impl Future<Output = Result<Vec<PrKey>, ApiError>>;
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
}

impl<G: GithubApi> Poller<G> {
    pub fn new(github: G, store: Store, config: Config, me: String) -> Self {
        Self {
            github,
            store,
            config,
            me,
            teams: HashSet::new(),
        }
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
    /// Also refreshes your team memberships.
    pub async fn reconcile(&mut self) -> Result<Vec<PrKey>, ApiError> {
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
        let requested = self.github.search_prs(REVIEW_REQUESTED).await?;
        let involved = self.github.search_prs(INVOLVES).await?;
        let mut keys: Vec<PrKey> = requested
            .into_iter()
            .chain(involved)
            .filter(|k| self.config.watches(&k.repo))
            .collect();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    /// PRs in watched repos with notifications since the last poll, and
    /// GitHub's requested poll interval.
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
                let keys = notifications
                    .into_iter()
                    .filter_map(|n| n.pr)
                    .filter(|k| self.config.watches(&k.repo))
                    .collect();
                Ok((keys, poll_interval))
            }
        }
    }

    /// Fetches `key`, detects triggers against the stored baseline, and
    /// stores the new baseline. `None` if the PR is gone, closed or matches
    /// no profile.
    pub async fn refresh(&mut self, key: &PrKey) -> Result<Option<Refreshed>, ApiError> {
        let with_files = self.config.needs_files(&key.repo);
        let Some(mut snapshot) = self.github.pull_request(key, &self.me, with_files).await? else {
            tracing::debug!("not open or not visible; skipping");
            return Ok(None);
        };
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
        self.store.record(&snapshot, &profile, &triggers)?;
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
        config::{CheckoutResolver, Vcs},
        pr::{CONVERSATION_THREAD, Comment, Thread},
        repo::RepoName,
    };
    use sanic_github::Notification;

    use super::*;

    #[derive(Default)]
    struct FakeGithub {
        searches: HashMap<&'static str, Vec<PrKey>>,
        prs: RefCell<HashMap<PrKey, PrSnapshot>>,
        notifications: Vec<Notification>,
        seen_since: RefCell<Vec<Option<String>>>,
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
            Ok(self.searches.get(qualifiers).cloned().unwrap_or_default())
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
                comments: vec![],
            }],
            files: Some(files.iter().map(|f| (*f).into()).collect()),
        }
    }

    fn poller(github: FakeGithub) -> Poller<FakeGithub> {
        Poller::new(
            github,
            Store::open_in_memory().unwrap(),
            config(),
            "me".into(),
        )
    }

    #[tokio::test]
    async fn reconcile_merges_searches_and_drops_unwatched_repos() {
        let mut github = FakeGithub::default();
        github.searches.insert(
            REVIEW_REQUESTED,
            vec![key("org/repo", 1), key("elsewhere/x", 9)],
        );
        github
            .searches
            .insert(INVOLVES, vec![key("org/repo", 1), key("org/other", 2)]);
        let keys = poller(github).reconcile().await.unwrap();
        assert_eq!(keys, [key("org/other", 2), key("org/repo", 1)]);
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
        });
        poller.github.prs.borrow_mut().insert(pr.clone(), snap);
        let triggers = poller.refresh(&pr).await.unwrap().unwrap().triggers;
        assert!(matches!(triggers.as_slice(), [Trigger::Reply { .. }]));
        assert_eq!(poller.store().events().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn notifications_resume_from_last_modified() {
        let github = FakeGithub {
            notifications: vec![
                Notification {
                    id: "1".into(),
                    reason: "comment".into(),
                    updated_at: "x".into(),
                    pr: Some(key("org/repo", 1)),
                },
                Notification {
                    id: "2".into(),
                    reason: "comment".into(),
                    updated_at: "x".into(),
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
