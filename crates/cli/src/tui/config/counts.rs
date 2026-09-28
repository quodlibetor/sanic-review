//! Live counts of what the edited config watches: how many repos, and how
//! many PRs on each of the dashboard's lists, from count-only GitHub
//! searches. The editor says what it wants counted; a task on the runtime
//! waits for edits to settle, asks GitHub for what it doesn't already
//! know, one search at a time and within a budget, and hands each answer
//! back.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write,
    future::Future,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, mpsc as std_mpsc},
    time::{Duration, SystemTime},
};

use sanic_core::{
    clock::window_start,
    config::{Config, Scope, expand_path},
    pr::TeamRef,
    repo::RepoName,
};
use sanic_github::{ApiError, Client, Searched};
use tokio::{
    sync::watch,
    time::{Instant, sleep, sleep_until},
};

use crate::{
    config_doc::RepoEntry,
    poll::{chunked, fits, recent},
};

/// The reads the counts need; a trait so tests can fake GitHub.
pub trait EditorGithub: Send + Sync + 'static {
    fn count_prs(&self, qualifiers: &str) -> impl Future<Output = Result<u32, ApiError>> + Send;
    fn count_repos(&self, qualifiers: &str) -> impl Future<Output = Result<u32, ApiError>> + Send;
    fn search_prs_pages(
        &self,
        qualifiers: &str,
        pages: NonZeroUsize,
    ) -> impl Future<Output = Result<Searched, ApiError>> + Send;
    fn my_teams(&self) -> impl Future<Output = Result<Vec<TeamRef>, ApiError>> + Send;
    fn my_orgs(&self) -> impl Future<Output = Result<Vec<String>, ApiError>> + Send;
}

impl EditorGithub for Client {
    fn count_prs(&self, qualifiers: &str) -> impl Future<Output = Result<u32, ApiError>> + Send {
        Client::count_prs(self, qualifiers)
    }

    fn count_repos(&self, qualifiers: &str) -> impl Future<Output = Result<u32, ApiError>> + Send {
        Client::count_repos(self, qualifiers)
    }

    fn search_prs_pages(
        &self,
        qualifiers: &str,
        pages: NonZeroUsize,
    ) -> impl Future<Output = Result<Searched, ApiError>> + Send {
        Client::search_prs_pages(self, qualifiers, Some(pages))
    }

    fn my_teams(&self) -> impl Future<Output = Result<Vec<TeamRef>, ApiError>> + Send {
        Client::my_teams(self)
    }

    fn my_orgs(&self) -> impl Future<Output = Result<Vec<String>, ApiError>> + Send {
        Client::my_orgs(self)
    }
}

/// Review requests for reviews you owe, as the dashboard lists them.
pub const OWED: &str = "review-requested:@me -author:@me";
pub const YOURS: &str = "author:@me";
/// Pages of review requests read for the repos they're in; past them the
/// count is a lower bound.
const PAGES: NonZeroUsize = NonZeroUsize::MIN.saturating_add(3);
/// How long edits have to settle before counting starts.
const SETTLE: Duration = Duration::from_secs(1);
/// Searches in a burst, then one more per [`REFILL`]: well inside GitHub's
/// secondary limits on searches, which `serve` shares.
const BURST: u32 = 10;
const REFILL: Duration = Duration::from_secs(6);

/// One thing to ask GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Query {
    /// How many open PRs match.
    Prs(String),
    /// How many repos match.
    Repos(String),
    /// The repos of the open PRs that match, up to [`PAGES`] pages.
    RepoNames(String),
    /// Your teams.
    Teams,
    /// The orgs you're in, to suggest.
    Orgs,
}

/// GitHub's answer to a [`Query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Count(u32),
    RepoNames {
        repos: BTreeSet<RepoName>,
        /// Whether every PR was read.
        complete: bool,
    },
    Teams(Vec<TeamRef>),
    /// The search failed, for this reason; it's asked again after the
    /// next edit.
    Failed(String),
    Orgs(Vec<String>),
}

/// Why counting stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// GitHub asks to wait this long.
    RateLimited(Duration),
    Unauthorized,
}

/// Why a search wasn't answered: counting stopped, or just this search
/// failed.
enum Refused {
    Stopped(Stopped),
    Failed(String),
}

/// What the counter hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Counted {
    Answer(Query, Answer),
    /// A search that failed for itself; the others go on.
    Failed(Query, String),
    Stopped(Stopped),
    /// Waiting for `serve`'s poller to come out of a rate limit.
    Waiting,
}

/// What the edited config watches, as loaded, for planning its counts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Watched {
    /// [`Config::search_scope`].
    pub scope: Vec<String>,
    /// Whole orgs, each with the repos entries claim in it.
    pub orgs: BTreeMap<String, BTreeSet<RepoName>>,
    /// Repos named by an entry.
    pub repos: BTreeSet<RepoName>,
    /// Whether an entry has path globs, which a search can't apply.
    pub globs: bool,
    /// `review_requests.teams`.
    pub teams: Vec<String>,
    /// `poll.updated_within_days`; `None` is no limit.
    pub window: Option<u32>,
    /// Each profile's entries, in order.
    pub profiles: BTreeMap<String, Vec<Entry>>,
}

/// What one repo entry covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: EntryId,
    pub scope: Scope,
    pub globs: bool,
}

/// What an entry covers, to tell which entry of the file a count is for
/// however the entries have been moved since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryId {
    pub covers: Covers,
    /// Its globs, when it has any.
    pub globs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Covers {
    /// A local checkout, by its expanded path.
    Checkout(PathBuf),
    /// A whole org, lowercased.
    Org(String),
    Repo(RepoName),
}

impl EntryId {
    /// An entry of the file, as the loader would read it from a config in
    /// `base`; `None` for one it couldn't.
    #[must_use]
    pub fn of(entry: &RepoEntry, base: &Path) -> Option<Self> {
        let (covers, globs) = match entry {
            RepoEntry::Checkout { path, .. } => {
                (Covers::Checkout(expand_path(path, base).ok()?), None)
            }
            RepoEntry::Scoped { path, paths, .. } => (
                Covers::Checkout(expand_path(path, base).ok()?),
                (!paths.is_empty()).then(|| paths.clone()),
            ),
            RepoEntry::Github { name, paths } => (
                if name.contains('/') {
                    Covers::Repo(RepoName::parse(name).ok()?)
                } else {
                    Covers::Org(name.to_ascii_lowercase())
                },
                (!paths.is_empty()).then(|| paths.clone()),
            ),
        };
        Some(Self { covers, globs })
    }
}

impl Watched {
    #[must_use]
    pub fn of(config: &Config) -> Self {
        let mut watched = Self {
            scope: config.search_scope(),
            teams: config.review_requests.teams.patterns.clone(),
            window: config.poll.updated_within_days,
            ..Self::default()
        };
        for profile in &config.profiles {
            let mut entries = Vec::new();
            for target in &profile.targets {
                match &target.scope {
                    Scope::Org(org) => {
                        watched.orgs.entry(org.clone()).or_default();
                    }
                    Scope::Repo(repo) => {
                        watched.repos.insert(repo.clone());
                    }
                }
                watched.globs |= target.paths.is_some();
                let covers = match (&target.checkout, &target.scope) {
                    (Some(checkout), _) => Covers::Checkout(checkout.path.clone()),
                    (None, Scope::Org(org)) => Covers::Org(org.clone()),
                    (None, Scope::Repo(repo)) => Covers::Repo(repo.clone()),
                };
                entries.push(Entry {
                    id: EntryId {
                        covers,
                        // No globs is no path filter, however it's written.
                        globs: target
                            .paths
                            .as_ref()
                            .map(|p| p.patterns.clone())
                            .filter(|globs| !globs.is_empty()),
                    },
                    scope: target.scope.clone(),
                    globs: target.paths.is_some(),
                });
            }
            watched.profiles.insert(profile.name.clone(), entries);
        }
        let claimed: Vec<RepoName> = watched.repos.iter().cloned().collect();
        for repo in claimed {
            if let Some(claims) = watched.orgs.get_mut(&repo.owner.to_ascii_lowercase()) {
                claims.insert(repo);
            }
        }
        watched
    }

    /// `qualifiers` limited to the window, from `now`.
    #[must_use]
    pub fn within(&self, qualifiers: &str, now: SystemTime) -> String {
        recent(qualifiers, window_start(now, self.window).as_deref())
    }

    /// The headline's searches for a list: in the window, in parts of the
    /// scope.
    #[must_use]
    pub fn searches(&self, list: &str, now: SystemTime) -> Vec<String> {
        chunked(&self.within(list, now), &self.scope)
    }

    /// The searches for a list's PRs the window leaves out, as the
    /// poller's hidden counts; none without a window.
    #[must_use]
    pub fn older(&self, list: &str, now: SystemTime) -> Vec<String> {
        let start = window_start(now, self.window);
        let Some(day) = start.as_deref().and_then(|s| s.get(..10)) else {
            return Vec::new();
        };
        chunked(&format!("{list} updated:<{day}"), &self.scope)
    }

    /// The repository search counting an org's repos.
    #[must_use]
    pub fn org_repos(org: &str) -> String {
        // Repository searches leave forks out and archived repos in.
        format!("user:{org} fork:true archived:false")
    }

    /// What an entry's PR counts search: its repo, or its org without the
    /// repos entries claim.
    #[must_use]
    pub fn entry_scope(&self, entry: &Entry) -> String {
        match &entry.scope {
            Scope::Repo(repo) => format!("repo:{repo}"),
            Scope::Org(org) => {
                let mut q = format!("user:{org}");
                for repo in self.orgs.get(org).into_iter().flatten() {
                    let _ = write!(q, " -repo:{repo}");
                }
                q
            }
        }
    }
}

/// Whether a figure is exact or a bound on the real one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Exact,
    /// Searches can't apply path globs or the team filter, so they can
    /// count PRs the config leaves out.
    AtMost,
    /// Not every result was read.
    AtLeast,
}

/// A count to show: `None` until it's all in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Figure {
    pub value: Option<u32>,
    pub bound: Bound,
    /// A search it needs failed.
    pub failed: bool,
}

impl Figure {
    fn of(value: Option<u32>, bound: Bound) -> Self {
        Self {
            value,
            bound,
            failed: false,
        }
    }

    /// `≤41`, `≥12`, `7`, `—` when a search failed, or `…` while it's
    /// being counted.
    #[must_use]
    pub fn text(&self) -> String {
        let mark = match self.bound {
            Bound::Exact => "",
            Bound::AtMost => "≤",
            Bound::AtLeast => "≥",
        };
        match self.value {
            _ if self.failed => "—".into(),
            Some(n) => format!("{mark}{n}"),
            None => "…".into(),
        }
    }
}

/// How an entry's share of a list's PRs is counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Share {
    /// One search.
    Search(String),
    /// An org's count, less the counts of the repos entries claim in it,
    /// searched in parts: leaving them out of the org's search doesn't fit
    /// in one.
    Less { whole: String, claimed: Claimed },
}

/// The searches counting a list's PRs in claimed repos: at least one, so
/// what's taken off an org's count is never the whole list's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    first: String,
    rest: Vec<String>,
}

impl Claimed {
    /// `list`'s searches of `repos`, in parts that fit; `None` for no
    /// repos, which [`chunked`] would search the whole list for.
    #[must_use]
    pub fn new(list: &str, repos: &BTreeSet<RepoName>) -> Option<Self> {
        if repos.is_empty() {
            return None;
        }
        let scope: Vec<String> = repos.iter().map(|repo| format!("repo:{repo}")).collect();
        let mut searches = chunked(list, &scope).into_iter();
        Some(Self {
            first: searches.next()?,
            rest: searches.collect(),
        })
    }

    pub fn searches(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.first).chain(&self.rest)
    }
}

impl Share {
    fn queries(&self) -> Box<dyn Iterator<Item = &String> + '_> {
        match self {
            Self::Search(q) => Box::new(std::iter::once(q)),
            Self::Less { whole, claimed } => {
                Box::new(std::iter::once(whole).chain(claimed.searches()))
            }
        }
    }
}

/// A profile's entry's counts: its repos, and the PRs on either list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPlan {
    pub id: EntryId,
    /// The org's repo count; `None` for a single repo.
    pub repos: Option<String>,
    /// Its PRs you owe and yours.
    pub prs: [Share; 2],
    pub globs: bool,
    /// Repos in the org that entries claim, which the org's count leaves
    /// out.
    pub claimed: u32,
}

/// What to count for the headline and what's on screen, from what the
/// config watches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub owed: Vec<String>,
    pub yours: Vec<String>,
    /// Each watched org's repo search, with how many repos entries claim
    /// in it.
    pub orgs: Vec<(String, u32)>,
    /// Repos named by entries.
    pub repos: u32,
    pub globs: bool,
    pub teams: Vec<String>,
    /// The selected profile's entries.
    pub entries: Vec<EntryPlan>,
    /// Your teams' requests, while `review_requests` is selected.
    pub team_requests: Vec<(TeamRef, String)>,
    /// PRs the window hides, while `poll` is selected.
    pub older: Vec<String>,
    /// Whether your orgs are wanted, to suggest for a profile's repos.
    pub orgs_to_suggest: bool,
}

/// What of the config the plan is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Showing<'a> {
    Profile(&'a str),
    ReviewRequests(&'a [TeamRef]),
    Poll,
    Other,
}

impl Plan {
    #[must_use]
    pub fn new(watched: &Watched, showing: Showing<'_>, now: SystemTime) -> Self {
        let mut plan = Self {
            owed: watched.searches(OWED, now),
            yours: watched.searches(YOURS, now),
            orgs: watched
                .orgs
                .iter()
                .map(|(org, claimed)| {
                    let claimed = u32::try_from(claimed.len()).unwrap_or(u32::MAX);
                    (Watched::org_repos(org), claimed)
                })
                .collect(),
            repos: u32::try_from(watched.repos.len()).unwrap_or(u32::MAX),
            globs: watched.globs,
            teams: watched.teams.clone(),
            ..Self::default()
        };
        match showing {
            Showing::Profile(name) => {
                plan.orgs_to_suggest = true;
                for entry in watched.profiles.get(name).into_iter().flatten() {
                    let scope = watched.entry_scope(entry);
                    let (repos, claimed) = match &entry.scope {
                        Scope::Org(org) => (
                            Some(Watched::org_repos(org)),
                            watched
                                .orgs
                                .get(org)
                                .map_or(0, |c| u32::try_from(c.len()).unwrap_or(u32::MAX)),
                        ),
                        Scope::Repo(_) => (None, 0),
                    };
                    let share = |list: &str| {
                        let direct = watched.within(&format!("{list} {scope}"), now);
                        let claimed = match &entry.scope {
                            Scope::Org(org) if !fits(&direct) => watched
                                .orgs
                                .get(org)
                                .and_then(|repos| Claimed::new(&watched.within(list, now), repos))
                                .map(|claimed| (org, claimed)),
                            _ => None,
                        };
                        match claimed {
                            Some((org, claimed)) => Share::Less {
                                whole: watched.within(&format!("{list} user:{org}"), now),
                                claimed,
                            },
                            None => Share::Search(direct),
                        }
                    };
                    plan.entries.push(EntryPlan {
                        id: entry.id.clone(),
                        repos,
                        prs: [share(OWED), share(YOURS)],
                        globs: entry.globs,
                        claimed,
                    });
                }
            }
            Showing::ReviewRequests(teams) => {
                plan.team_requests = teams
                    .iter()
                    .map(|team| {
                        let q = watched.within(&format!("team-review-requested:{team}"), now);
                        (team.clone(), q)
                    })
                    .collect();
            }
            Showing::Poll => {
                plan.older = [OWED, YOURS]
                    .into_iter()
                    .flat_map(|list| watched.older(list, now))
                    .collect();
            }
            Showing::Other => {}
        }
        plan
    }

    /// Everything to ask, most wanted first, each once.
    #[must_use]
    pub fn queries(&self) -> Vec<Query> {
        let mut wanted = vec![Query::Teams];
        wanted.extend(self.owed.iter().cloned().map(Query::Prs));
        wanted.extend(self.yours.iter().cloned().map(Query::Prs));
        wanted.extend(self.orgs.iter().map(|(q, _)| Query::Repos(q.clone())));
        wanted.extend(self.owed.iter().cloned().map(Query::RepoNames));
        for entry in &self.entries {
            wanted.extend(
                entry
                    .prs
                    .iter()
                    .flat_map(Share::queries)
                    .cloned()
                    .map(Query::Prs),
            );
            wanted.extend(entry.repos.iter().cloned().map(Query::Repos));
        }
        wanted.extend(
            self.team_requests
                .iter()
                .map(|(_, q)| Query::Prs(q.clone())),
        );
        wanted.extend(self.older.iter().cloned().map(Query::Prs));
        if self.orgs_to_suggest {
            wanted.push(Query::Orgs);
        }
        let mut seen = std::collections::HashSet::new();
        wanted.retain(|q| seen.insert(q.clone()));
        wanted
    }
}

/// Reads counts out of the answers so far.
pub struct Tally<'a>(pub &'a HashMap<Query, Answer>);

impl Tally<'_> {
    fn count(&self, query: &Query) -> Option<u32> {
        match self.0.get(query) {
            Some(Answer::Count(n)) => Some(*n),
            _ => None,
        }
    }

    /// The sum of the PR counts of `searches`, once they're all in.
    #[must_use]
    pub fn prs(&self, searches: &[String]) -> Option<u32> {
        searches
            .iter()
            .map(|q| self.count(&Query::Prs(q.clone())))
            .sum()
    }

    /// The orgs you're in, once they're in.
    #[must_use]
    pub fn orgs(&self) -> Option<&[String]> {
        match self.0.get(&Query::Orgs) {
            Some(Answer::Orgs(orgs)) => Some(orgs),
            _ => None,
        }
    }

    #[must_use]
    pub fn teams(&self) -> Option<&[TeamRef]> {
        match self.0.get(&Query::Teams) {
            Some(Answer::Teams(teams)) => Some(teams),
            _ => None,
        }
    }

    /// Whether the team filter leaves out a team you're in, whose
    /// requests the searches still count.
    #[must_use]
    pub fn excludes_a_team(&self, plan: &Plan) -> bool {
        let filter = sanic_core::config::TeamFilter::new(plan.teams.clone());
        match (self.teams(), filter) {
            (Some(teams), Ok(filter)) => teams.iter().any(|team| !filter.allows(team)),
            _ => false,
        }
    }

    #[must_use]
    pub fn owed(&self, plan: &Plan) -> Figure {
        let bound = if plan.globs || self.excludes_a_team(plan) {
            Bound::AtMost
        } else {
            Bound::Exact
        };
        Figure {
            failed: self.failed(plan.owed.iter().cloned().map(Query::Prs)),
            ..Figure::of(self.prs(&plan.owed), bound)
        }
    }

    #[must_use]
    pub fn yours(&self, plan: &Plan) -> Figure {
        let bound = if plan.globs {
            Bound::AtMost
        } else {
            Bound::Exact
        };
        Figure {
            failed: self.failed(plan.yours.iter().cloned().map(Query::Prs)),
            ..Figure::of(self.prs(&plan.yours), bound)
        }
    }

    /// Watched repos: each repo entries name, and each org's repos but
    /// those.
    #[must_use]
    pub fn repos(&self, plan: &Plan) -> Figure {
        let orgs: Option<u32> = plan
            .orgs
            .iter()
            .map(|(q, claimed)| {
                self.count(&Query::Repos(q.clone()))
                    .map(|n| n.saturating_sub(*claimed))
            })
            .sum();
        Figure {
            failed: self.failed(plan.orgs.iter().map(|(q, _)| Query::Repos(q.clone()))),
            ..Figure::of(orgs.map(|n| n + plan.repos), Bound::Exact)
        }
    }

    /// The repos of the PRs you owe a review.
    #[must_use]
    pub fn asked_in(&self, plan: &Plan) -> Figure {
        let mut repos = BTreeSet::new();
        let mut complete = true;
        for q in &plan.owed {
            match self.0.get(&Query::RepoNames(q.clone())) {
                Some(Answer::RepoNames {
                    repos: found,
                    complete: all,
                }) => {
                    repos.extend(found.iter().cloned());
                    complete &= all;
                }
                failed => {
                    return Figure {
                        failed: matches!(failed, Some(Answer::Failed(_))),
                        ..Figure::of(None, Bound::Exact)
                    };
                }
            }
        }
        let bound = if complete {
            Bound::Exact
        } else {
            Bound::AtLeast
        };
        Figure::of(u32::try_from(repos.len()).ok(), bound)
    }

    /// Whether one of `queries` failed.
    fn failed(&self, queries: impl IntoIterator<Item = Query>) -> bool {
        queries
            .into_iter()
            .any(|q| matches!(self.0.get(&q), Some(Answer::Failed(_))))
    }

    /// An entry's share of a list's PRs, once it's all in.
    fn share(&self, share: &Share) -> Option<u32> {
        match share {
            Share::Search(q) => self.prs(std::slice::from_ref(q)),
            Share::Less { whole, claimed } => {
                let whole = self.prs(std::slice::from_ref(whole))?;
                let claimed: Vec<String> = claimed.searches().cloned().collect();
                Some(whole.saturating_sub(self.prs(&claimed)?))
            }
        }
    }

    /// An entry's repos, and its PRs on both lists.
    #[must_use]
    pub fn entry(&self, entry: &EntryPlan) -> (Figure, Figure) {
        let repos = match &entry.repos {
            None => Some(1),
            Some(q) => self
                .count(&Query::Repos(q.clone()))
                .map(|n| n.saturating_sub(entry.claimed)),
        };
        let bound = if entry.globs {
            Bound::AtMost
        } else {
            Bound::Exact
        };
        let prs = entry
            .prs
            .iter()
            .map(|share| self.share(share))
            .sum::<Option<u32>>();
        let prs = Figure {
            failed: self.failed(
                entry
                    .prs
                    .iter()
                    .flat_map(Share::queries)
                    .cloned()
                    .map(Query::Prs),
            ),
            ..Figure::of(prs, bound)
        };
        let repos = Figure {
            failed: self.failed(entry.repos.iter().cloned().map(Query::Repos)),
            ..Figure::of(repos, Bound::Exact)
        };
        (repos, prs)
    }

    /// Why the first of `wanted` to fail failed, while it's still failed:
    /// until it's counted again.
    #[must_use]
    pub fn failure(&self, wanted: &[Query]) -> Option<&str> {
        wanted.iter().find_map(|q| match self.0.get(q) {
            Some(Answer::Failed(why)) => Some(why.as_str()),
            _ => None,
        })
    }

    /// Whether everything asked for is in.
    #[must_use]
    pub fn done(&self, wanted: &[Query]) -> bool {
        wanted.iter().all(|q| self.0.contains_key(q))
    }
}

/// Counts in the background: [`Counter::want`] says what, and
/// [`Counter::counted`] hands back what's come in.
pub struct Counter {
    wanted: watch::Sender<Vec<Query>>,
    counted: std_mpsc::Receiver<Counted>,
}

impl Counter {
    /// Starts counting on `runtime`, pausing while `paused` is set to a
    /// time still to come: `serve`'s poller, rate limited, shares the
    /// token.
    pub fn start<G: EditorGithub>(
        runtime: &tokio::runtime::Handle,
        github: Arc<G>,
        paused: watch::Receiver<Option<Instant>>,
    ) -> Self {
        let (wanted, wanted_rx) = watch::channel(Vec::new());
        let (counted_tx, counted) = std_mpsc::channel();
        runtime.spawn(count(github, wanted_rx, paused, counted_tx));
        Self { wanted, counted }
    }

    /// Replaces what's wanted, most wanted first.
    pub fn want(&self, queries: Vec<Query>) {
        self.wanted.send_if_modified(|now| {
            let changed = *now != queries;
            *now = queries;
            changed
        });
    }

    /// Everything counted since the last call.
    pub fn counted(&self) -> Vec<Counted> {
        self.counted.try_iter().collect()
    }
}

/// The counting task: ends once the [`Counter`] is dropped.
async fn count<G: EditorGithub>(
    github: Arc<G>,
    mut wanted: watch::Receiver<Vec<Query>>,
    paused: watch::Receiver<Option<Instant>>,
    counted: std_mpsc::Sender<Counted>,
) {
    let mut known: HashMap<Query, Answer> = HashMap::new();
    let mut budget = Budget::new();
    // Until when GitHub asked the counts themselves to wait.
    let mut limited: Option<Instant> = None;
    'edits: while wanted.changed().await.is_ok() {
        // Settles: a newer edit before then starts the wait again.
        loop {
            tokio::select! {
                () = sleep(SETTLE) => break,
                changed = wanted.changed() => if changed.is_err() { return },
            }
        }
        let queries = wanted.borrow_and_update().clone();
        for query in queries {
            if let Some(answer) = known.get(&query) {
                let _ = counted.send(Counted::Answer(query, answer.clone()));
                continue;
            }
            // `serve` can be limited again by the time a wait ends.
            loop {
                let serving = *paused.borrow();
                let Some(until) = serving.max(limited).filter(|until| *until > Instant::now())
                else {
                    break;
                };
                let _ = counted.send(Counted::Waiting);
                tokio::select! {
                    () = sleep_until(until) => {}
                    changed = wanted.changed() => {
                        if changed.is_err() { return }
                        wanted.mark_changed();
                        continue 'edits;
                    }
                }
            }
            tokio::select! {
                () = budget.take() => {}
                // A newer edit drops what's left.
                changed = wanted.changed() => {
                    if changed.is_err() { return }
                    wanted.mark_changed();
                    continue 'edits;
                }
            }
            match ask(&*github, &query).await {
                Ok(answer) => {
                    known.insert(query.clone(), answer.clone());
                    if counted.send(Counted::Answer(query, answer)).is_err() {
                        return;
                    }
                }
                // Not retried until the searches wanted change. A search
                // that fails for itself doesn't stop the others.
                Err(Refused::Failed(why)) => {
                    if counted.send(Counted::Failed(query, why)).is_err() {
                        return;
                    }
                }
                Err(Refused::Stopped(stopped)) => {
                    if let Stopped::RateLimited(wait) = &stopped {
                        limited = Some(Instant::now() + *wait);
                    }
                    if counted.send(Counted::Stopped(stopped)).is_err() {
                        return;
                    }
                    continue 'edits;
                }
            }
            if wanted.has_changed().unwrap_or(false) {
                continue 'edits;
            }
        }
    }
}

async fn ask<G: EditorGithub>(github: &G, query: &Query) -> Result<Answer, Refused> {
    let answer = match query {
        Query::Prs(q) => github.count_prs(q).await.map(Answer::Count),
        Query::Repos(q) => github.count_repos(q).await.map(Answer::Count),
        Query::RepoNames(q) => {
            github
                .search_prs_pages(q, PAGES)
                .await
                .map(|found| Answer::RepoNames {
                    repos: found.keys.into_iter().map(|key| key.repo).collect(),
                    complete: found.complete,
                })
        }
        Query::Teams => github.my_teams().await.map(Answer::Teams),
        Query::Orgs => github.my_orgs().await.map(Answer::Orgs),
    };
    answer.map_err(|err| match err {
        ApiError::RateLimited { retry_after } => {
            Refused::Stopped(Stopped::RateLimited(retry_after))
        }
        ApiError::Unauthorized => Refused::Stopped(Stopped::Unauthorized),
        ApiError::Other(report) => Refused::Failed(format!("{report:#}")),
    })
}

/// A token bucket of searches.
struct Budget {
    tokens: u32,
    refilled: Instant,
}

impl Budget {
    fn new() -> Self {
        Self {
            tokens: BURST,
            refilled: Instant::now(),
        }
    }

    /// Waits for a search's worth, and spends it.
    async fn take(&mut self) {
        loop {
            let now = Instant::now();
            let earned = now.duration_since(self.refilled).as_secs() / REFILL.as_secs();
            if earned > 0 {
                let earned = u32::try_from(earned).unwrap_or(u32::MAX);
                self.tokens = self.tokens.saturating_add(earned).min(BURST);
                self.refilled += REFILL * earned;
            }
            if self.tokens > 0 {
                self.tokens -= 1;
                return;
            }
            sleep_until(self.refilled + REFILL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Mutex};

    use sanic_core::pr::PrKey;

    use super::*;
    use crate::poll::tests::NoCheckouts;

    #[derive(Default)]
    struct FakeGithub {
        asked: Mutex<Vec<String>>,
        /// Queries answered with a rate limit.
        limited: Vec<String>,
    }

    impl FakeGithub {
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }

        fn answer(&self, q: &str) -> Result<u32, ApiError> {
            self.asked.lock().unwrap().push(q.to_owned());
            if self.limited.iter().any(|l| l == q) {
                return Err(ApiError::RateLimited {
                    retry_after: Duration::from_secs(60),
                });
            }
            Ok(u32::try_from(q.len()).unwrap())
        }
    }

    // The fake answers immediately; `async fn` keeps it readable.
    #[allow(clippy::unused_async_trait_impl)]
    impl EditorGithub for FakeGithub {
        async fn count_prs(&self, q: &str) -> Result<u32, ApiError> {
            self.answer(q)
        }

        async fn count_repos(&self, q: &str) -> Result<u32, ApiError> {
            self.answer(q)
        }

        async fn search_prs_pages(&self, q: &str, _: NonZeroUsize) -> Result<Searched, ApiError> {
            self.answer(q)?;
            Ok(Searched {
                keys: vec![PrKey {
                    repo: RepoName::new("org", "a"),
                    number: 1,
                }],
                complete: false,
            })
        }

        async fn my_teams(&self) -> Result<Vec<TeamRef>, ApiError> {
            self.answer("teams")?;
            Ok(vec![TeamRef::new("org", "x")])
        }

        async fn my_orgs(&self) -> Result<Vec<String>, ApiError> {
            self.answer("orgs")?;
            Ok(vec!["org".into()])
        }
    }

    fn prs(qs: &[&str]) -> Vec<Query> {
        qs.iter().map(|q| Query::Prs((*q).to_owned())).collect()
    }

    fn start(github: &Arc<FakeGithub>) -> (Counter, watch::Sender<Option<Instant>>) {
        let (paused_tx, paused) = watch::channel(None);
        let counter = Counter::start(
            &tokio::runtime::Handle::current(),
            Arc::clone(github),
            paused,
        );
        (counter, paused_tx)
    }

    #[tokio::test(start_paused = true)]
    async fn counting_waits_for_edits_to_settle_and_asks_once() {
        let github = Arc::new(FakeGithub::default());
        let (counter, _paused) = start(&github);
        counter.want(prs(&["a"]));
        sleep(SETTLE / 2).await;
        counter.want(prs(&["a", "bb"]));
        sleep(SETTLE / 2).await;
        assert!(github.asked().is_empty(), "still settling");
        sleep(SETTLE).await;
        assert_eq!(github.asked(), ["a", "bb"]);
        assert_eq!(
            counter.counted(),
            [
                Counted::Answer(Query::Prs("a".into()), Answer::Count(1)),
                Counted::Answer(Query::Prs("bb".into()), Answer::Count(2)),
            ]
        );
        // What's known isn't asked again, but is handed back.
        counter.want(prs(&["bb"]));
        sleep(SETTLE * 2).await;
        assert_eq!(github.asked(), ["a", "bb"]);
        assert_eq!(
            counter.counted(),
            [Counted::Answer(Query::Prs("bb".into()), Answer::Count(2))]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_spaces_searches_and_a_newer_edit_drops_the_rest() {
        let github = Arc::new(FakeGithub::default());
        let (counter, _paused) = start(&github);
        let many: Vec<String> = (0..15).map(|i| format!("q{i}")).collect();
        counter.want(many.iter().cloned().map(Query::Prs).collect());
        sleep(SETTLE + Duration::from_millis(1)).await;
        assert_eq!(github.asked().len(), usize::try_from(BURST).unwrap());
        sleep(REFILL).await;
        assert_eq!(github.asked().len(), usize::try_from(BURST).unwrap() + 1);
        // An edit drops the queued ones.
        counter.want(prs(&["new"]));
        sleep(SETTLE + REFILL * 20).await;
        let asked = github.asked();
        assert_eq!(asked.last().map(String::as_str), Some("new"));
        assert_eq!(asked.len(), usize::try_from(BURST).unwrap() + 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_stops_counting_until_an_edit_and_its_wait() {
        let github = Arc::new(FakeGithub {
            limited: vec!["b".into()],
            ..FakeGithub::default()
        });
        let (counter, _paused) = start(&github);
        counter.want(prs(&["a", "b", "c"]));
        sleep(SETTLE * 2).await;
        assert_eq!(github.asked(), ["a", "b"]);
        assert_eq!(
            counter.counted().last(),
            Some(&Counted::Stopped(Stopped::RateLimited(
                Duration::from_secs(60)
            )))
        );
        counter.want(prs(&["c"]));
        sleep(SETTLE * 2).await;
        assert_eq!(github.asked(), ["a", "b"], "waiting out the limit");
        sleep(Duration::from_secs(60)).await;
        assert_eq!(github.asked(), ["a", "b", "c"]);
    }

    #[tokio::test(start_paused = true)]
    async fn counting_waits_while_serve_is_rate_limited() {
        let github = Arc::new(FakeGithub::default());
        let (counter, paused) = start(&github);
        paused.send_replace(Some(Instant::now() + Duration::from_secs(30)));
        counter.want(prs(&["a"]));
        sleep(SETTLE * 2).await;
        assert!(github.asked().is_empty());
        assert_eq!(counter.counted(), [Counted::Waiting]);
        // Limited again as the wait ends: it waits again.
        paused.send_replace(Some(Instant::now() + Duration::from_secs(60)));
        sleep(Duration::from_secs(30)).await;
        assert!(github.asked().is_empty());
        sleep(Duration::from_secs(31)).await;
        assert_eq!(github.asked(), ["a"]);
    }

    fn watched(text: &str) -> Watched {
        let config = Config::parse(text, Path::new("/"), &NoCheckouts).unwrap();
        Watched::of(&config)
    }

    const NOW: SystemTime = SystemTime::UNIX_EPOCH;

    #[test]
    fn plans_count_the_headline_then_whats_shown() {
        let watched = watched(
            r#"
            [poll]
            updated_within_days = 0
            [profile.p]
            repos = [{ github = "org" }, { github = "org/a", paths = ["x/**"] }, { github = "else/b" }]
            "#,
        );
        assert_eq!(
            watched.orgs,
            BTreeMap::from([(
                "org".to_owned(),
                BTreeSet::from([RepoName::new("org", "a")])
            )])
        );
        let plan = Plan::new(&watched, Showing::Profile("p"), NOW);
        assert_eq!(plan.owed, [format!("{OWED} user:org repo:else/b")]);
        assert_eq!(
            plan.entries[0].prs,
            [
                Share::Search(format!("{OWED} user:org -repo:org/a")),
                Share::Search(format!("{YOURS} user:org -repo:org/a")),
            ]
        );
        assert_eq!(
            plan.entries[1].prs[1],
            Share::Search(format!("{YOURS} repo:org/a"))
        );
        assert_eq!(
            plan.entries[1].id,
            EntryId {
                covers: Covers::Repo(RepoName::new("org", "a")),
                globs: Some(vec!["x/**".into()]),
            }
        );
        let queries = plan.queries();
        assert_eq!(queries[0], Query::Teams);
        assert_eq!(queries[1], Query::Prs(plan.owed[0].clone()));
        assert!(queries.contains(&Query::Repos("user:org fork:true archived:false".into())));
        assert_eq!(
            queries.len(),
            queries
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        );

        let mut answers = HashMap::new();
        let tally = Tally(&answers);
        assert_eq!(tally.owed(&plan).text(), "…");
        answers.insert(Query::Prs(plan.owed[0].clone()), Answer::Count(41));
        answers.insert(Query::Prs(plan.yours[0].clone()), Answer::Count(7));
        answers.insert(
            Query::Repos("user:org fork:true archived:false".into()),
            Answer::Count(10),
        );
        answers.insert(
            Query::RepoNames(plan.owed[0].clone()),
            Answer::RepoNames {
                repos: BTreeSet::from([RepoName::new("org", "a")]),
                complete: false,
            },
        );
        let tally = Tally(&answers);
        // Globs make the PR counts upper bounds.
        assert_eq!(tally.owed(&plan).text(), "≤41");
        assert_eq!(tally.yours(&plan).text(), "≤7");
        // The org's ten, less the one an entry claims, and two named.
        assert_eq!(tally.repos(&plan).text(), "11");
        assert_eq!(tally.asked_in(&plan).text(), "≥1");
        assert_eq!(tally.entry(&plan.entries[0]).0.text(), "9");
        assert_eq!(tally.entry(&plan.entries[2]).0.text(), "1");
    }

    #[test]
    fn an_org_whose_claimed_repos_dont_fit_in_a_search_is_counted_less_them() {
        let repos: Vec<String> = (0..20)
            .map(|i| format!("{{ github = \"org/a-long-repository-name-{i}\" }}"))
            .collect();
        let watched = watched(&format!(
            "[profile.p]\nrepos = [{{ github = \"org\" }}, {}]\n",
            repos.join(", ")
        ));
        let plan = Plan::new(&watched, Showing::Profile("p"), NOW);
        let Share::Less { whole, claimed } = &plan.entries[0].prs[0] else {
            panic!("{:?}", plan.entries[0].prs[0]);
        };
        assert!(
            whole.starts_with(&format!("{OWED} user:org updated:>=")),
            "{whole}"
        );
        assert!(
            claimed.searches().count() > 1,
            "the claimed repos take several searches"
        );
        let claimed: Vec<String> = claimed.searches().cloned().collect();
        assert!(matches!(plan.entries[1].prs[0], Share::Search(_)));
        assert!(plan.queries().iter().all(|q| match q {
            Query::Prs(q) => fits(q),
            _ => true,
        }));

        // The org's 50, less the 5 in each claimed part, on each list.
        let mut answers: HashMap<Query, Answer> = plan
            .queries()
            .into_iter()
            .map(|q| {
                let n = match &q {
                    Query::Prs(s) if s.contains("user:org") => 50,
                    _ => 5,
                };
                (q, Answer::Count(n))
            })
            .collect();
        let parts = u32::try_from(claimed.len()).unwrap();
        let (_, prs) = Tally(&answers).entry(&plan.entries[0]);
        assert_eq!(prs.text(), (2 * (50 - 5 * parts)).to_string());
        // A failed search shows in the headline figure it's part of.
        let mut failed_headline = answers.clone();
        failed_headline.insert(
            Query::Prs(plan.owed[0].clone()),
            Answer::Failed("422".into()),
        );
        assert_eq!(Tally(&failed_headline).owed(&plan).text(), "—");
        // A failed part blanks the row, and only it.
        answers.insert(Query::Prs(claimed[0].clone()), Answer::Failed("422".into()));
        let tally = Tally(&answers);
        assert_eq!(tally.entry(&plan.entries[0]).1.text(), "—");
        assert_eq!(tally.entry(&plan.entries[1]).1.text(), "10");
        // So does a failed count of the org's repos, in its repos column.
        answers.insert(
            Query::Repos(Watched::org_repos("org")),
            Answer::Failed("422".into()),
        );
        assert_eq!(Tally(&answers).entry(&plan.entries[0]).0.text(), "—");
    }

    #[test]
    fn claimed_repos_are_never_counted_as_the_whole_list() {
        assert_eq!(Claimed::new(OWED, &BTreeSet::new()), None);
        let one = Claimed::new(OWED, &BTreeSet::from([RepoName::new("org", "a")])).unwrap();
        assert_eq!(
            one.searches().collect::<Vec<_>>(),
            [&format!("{OWED} repo:org/a")]
        );
    }

    #[test]
    fn a_headline_failure_shows_until_its_figure_is_counted_again() {
        let watched = watched("[profile.p]\nrepos = [{ github = \"org\" }]\n");
        let plan = Plan::new(&watched, Showing::Other, NOW);
        let wanted = plan.queries();
        let owed = Query::Prs(plan.owed[0].clone());
        let mut answers = HashMap::from([(owed.clone(), Answer::Failed("422".into()))]);
        // Other answers coming in don't clear it.
        answers.insert(Query::Prs(plan.yours[0].clone()), Answer::Count(3));
        let tally = Tally(&answers);
        assert_eq!(tally.owed(&plan).text(), "—");
        assert_eq!(tally.failure(&wanted), Some("422"));
        answers.insert(owed, Answer::Count(4));
        let tally = Tally(&answers);
        assert_eq!(tally.owed(&plan).text(), "4");
        assert_eq!(tally.failure(&wanted), None);
    }

    #[test]
    fn entries_of_the_file_are_found_by_what_they_cover() {
        let base = Path::new("/c");
        let scoped = RepoEntry::Scoped {
            path: "src".into(),
            paths: vec!["v/**".into()],
            remote: None,
        };
        assert_eq!(
            EntryId::of(&scoped, base),
            Some(EntryId {
                covers: Covers::Checkout(PathBuf::from("/c/src")),
                globs: Some(vec!["v/**".into()]),
            })
        );
        // No globs is no path filter, however it's written.
        let unscoped = RepoEntry::Scoped {
            path: "src".into(),
            paths: Vec::new(),
            remote: None,
        };
        assert_eq!(EntryId::of(&unscoped, base).unwrap().globs, None);
        let org = RepoEntry::Github {
            name: "Org".into(),
            paths: Vec::new(),
        };
        assert_eq!(
            EntryId::of(&org, base),
            Some(EntryId {
                covers: Covers::Org("org".into()),
                globs: None,
            })
        );
        let loaded = watched(
            "[profile.p]\nrepos = [{ github = \"org\" }, { github = \"o/r\", paths = [\"x\"] }]\n",
        );
        let ids: Vec<&EntryId> = loaded.profiles["p"].iter().map(|e| &e.id).collect();
        assert_eq!(ids[0], &EntryId::of(&org, base).unwrap());
        let repo = RepoEntry::Github {
            name: "o/r".into(),
            paths: vec!["x".into()],
        };
        assert_eq!(ids[1], &EntryId::of(&repo, base).unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_search_doesnt_stop_the_others() {
        struct Failing;
        #[allow(clippy::unused_async_trait_impl)]
        impl EditorGithub for Failing {
            async fn count_prs(&self, q: &str) -> Result<u32, ApiError> {
                match q {
                    "bad" => Err(ApiError::Other(color_eyre::eyre::eyre!("422"))),
                    _ => Ok(1),
                }
            }
            async fn count_repos(&self, _: &str) -> Result<u32, ApiError> {
                Ok(1)
            }
            async fn search_prs_pages(
                &self,
                _: &str,
                _: NonZeroUsize,
            ) -> Result<Searched, ApiError> {
                Ok(Searched {
                    keys: Vec::new(),
                    complete: true,
                })
            }
            async fn my_teams(&self) -> Result<Vec<TeamRef>, ApiError> {
                Ok(Vec::new())
            }
            async fn my_orgs(&self) -> Result<Vec<String>, ApiError> {
                Ok(Vec::new())
            }
        }
        let (_paused_tx, paused) = watch::channel(None);
        let counter = Counter::start(
            &tokio::runtime::Handle::current(),
            Arc::new(Failing),
            paused,
        );
        counter.want(prs(&["bad", "good"]));
        sleep(SETTLE * 2).await;
        let got = counter.counted();
        assert!(
            matches!(&got[0], Counted::Failed(Query::Prs(q), _) if q == "bad"),
            "{got:?}"
        );
        assert_eq!(
            got[1],
            Counted::Answer(Query::Prs("good".into()), Answer::Count(1))
        );
    }

    #[test]
    fn a_team_the_filter_leaves_out_makes_what_you_owe_a_bound() {
        let watched = watched(
            "[review_requests]\nteams = [\"*\", \"!org/x\"]\n[profile.p]\nrepos = [{ github = \"org\" }]\n",
        );
        let teams = [TeamRef::new("org", "x")];
        let plan = Plan::new(&watched, Showing::ReviewRequests(&teams), NOW);
        assert_eq!(plan.team_requests[0].0, teams[0]);
        assert!(
            plan.team_requests[0]
                .1
                .starts_with("team-review-requested:org/x updated:>=")
        );
        let mut answers = HashMap::from([(Query::Prs(plan.owed[0].clone()), Answer::Count(3))]);
        assert_eq!(Tally(&answers).owed(&plan).text(), "3");
        answers.insert(Query::Teams, Answer::Teams(teams.to_vec()));
        assert_eq!(Tally(&answers).owed(&plan).text(), "≤3");
    }
}
