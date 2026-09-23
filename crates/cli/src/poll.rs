//! Turning GitHub state into stored snapshots and detected triggers.

use std::{collections::HashSet, future::Future, time::Duration};

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
    /// PRs from the last `review-requested:@me` search. The PR query only
    /// sees direct user requests, so team requests come from here.
    requested: HashSet<PrKey>,
}

impl<G: GithubApi> Poller<G> {
    pub fn new(github: G, store: Store, config: Config, me: String) -> Self {
        Self {
            github,
            store,
            config,
            me,
            requested: HashSet::new(),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Open PRs that request your review or involve you, in watched repos.
    pub async fn reconcile(&mut self) -> Result<Vec<PrKey>, ApiError> {
        let requested = self.github.search_prs(REVIEW_REQUESTED).await?;
        let involved = self.github.search_prs(INVOLVES).await?;
        self.requested = requested.iter().cloned().collect();
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
    /// stores the new baseline. `None` if the PR is gone or matches no
    /// profile.
    pub async fn refresh(&mut self, key: &PrKey) -> Result<Option<Refreshed>, ApiError> {
        let with_files = self.config.needs_files(&key.repo);
        let Some(mut snapshot) = self.github.pull_request(key, &self.me, with_files).await? else {
            tracing::debug!(pr = %key, "not visible; skipping");
            return Ok(None);
        };
        snapshot.review_requested |= self.requested.contains(key);
        let files = snapshot.files.as_deref().unwrap_or_default();
        let Some(matched) = self.config.match_pr(&key.repo, files) else {
            tracing::debug!(pr = %key, "matches no profile; skipping");
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
        Config::parse(
            r#"
            [profile.vuln]
            repos = [{ github = "org/repo", paths = ["vuln/**"] }]
            [profile.default]
            repos = [{ github = "org" }]
            "#,
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
            url: format!("https://github.com/{}/pull/{}", key.repo, key.number),
            author: author.into(),
            head_sha: "h1".into(),
            base_sha: "b".into(),
            is_draft: false,
            review_requested: false,
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

    #[tokio::test]
    async fn team_review_requests_come_from_search() {
        let pr = key("org/other", 2);
        let mut github = FakeGithub::default();
        github.searches.insert(REVIEW_REQUESTED, vec![pr.clone()]);
        github
            .prs
            .borrow_mut()
            .insert(pr.clone(), snapshot(&pr, "alice", &[]));
        let mut poller = poller(github);
        poller.reconcile().await.unwrap();
        let refreshed = poller.refresh(&pr).await.unwrap().unwrap();
        assert!(matches!(
            refreshed.triggers.as_slice(),
            [Trigger::ReviewRequested { .. }]
        ));
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
