//! Dashboard server; the only caller of GitHub write paths.
//!
//! Server-rendered HTML with htmx, all assets embedded. It binds to
//! `127.0.0.1` only. Every request's `Host` must name the loopback
//! interface, and every state-changing request must carry the process's
//! CSRF token and come from the dashboard's own origin; see [`guard`].
//!
//! GitHub is written to in exactly one place: the submit handler, after
//! you've seen the exact requests and pressed Confirm.

mod assets;
mod cells;
mod chat;
mod diff;
mod files;
mod guard;
mod highlight;
mod ignore;
mod index;
mod links;
mod page;
mod pr;
mod regenerate;
mod submit;
#[cfg(test)]
mod tests;
mod threads;

use std::{
    collections::{HashMap, HashSet},
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Instant,
};

use axum::{
    Router,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use color_eyre::eyre::{self, Result, WrapErr};
use sanic_core::{
    clock::{Clock, RecencyWindow, WindowChoice},
    pr::PrKey,
    repo::RepoName,
    skip::SkipRules,
};
use sanic_github::Client;
use sanic_runner::{mirror::Mirrors, review::RunSettings};
use sanic_store::{Refusal, Store};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::guard::Csrf;

/// What `serve` does on the dashboard's behalf, as it does for the TUI.
/// Archiving needs no help: the dashboard writes the store itself, as
/// `sanic-review archive` does.
pub trait Control: Send + Sync {
    /// Start a review of `key` now: its held run, or a full review of its
    /// head as last polled.
    fn review_now(&self, key: PrKey);

    /// Refreshes `key` from GitHub now, ahead of anything else queued,
    /// rather than at the next reconcile; a rate limit's pause still holds
    /// it back. Asked after a post, so the dashboard shows what GitHub has.
    fn refresh(&self, key: PrKey);

    /// Adds `pattern` to `skip_titles` in the config file, in
    /// `[review_requests]` for `None` or else that profile's table, as the
    /// TUI's ignore editor does; `serve` picks it up by reloading. `false`
    /// if it's already there.
    fn add_skip_title(&self, pattern: &str, profile: Option<&str>) -> Result<bool>;

    /// What a run under `profile` would use now, as the config stands, for
    /// the command that chats with a review's agent.
    fn run_settings(&self, profile: &str) -> Result<RunSettings>;

    /// Revises run `run_id`'s review with `instruction`: a new `regenerate`
    /// run resumes its agent session, and its drafts are the new run's own.
    /// It starts now, even under `--manual-reviews`. Returns the new run's
    /// id, or why it can't; [`Refusal`] says why in words.
    fn regenerate(&self, run_id: i64, instruction: &str) -> Result<Result<i64, Refusal>>;

    /// Picks the recency window for the lists and the poller in place of
    /// `poll.updated_within_days`, or with `None` goes back to it. It's
    /// kept in the store, not the config file, and the next reconcile
    /// fetches what it now takes in.
    fn set_window(&self, choice: Option<WindowChoice>) -> Result<()>;
}

/// Files at a PR's commits, for the files view's expanding context. They
/// come from the mirrors the runner keeps, never from GitHub; both calls
/// block.
pub trait Sources: Send + Sync {
    /// Whether `commit` of `repo` can be read.
    fn has_commit(&self, repo: &RepoName, commit: &str) -> bool;

    /// `path` at `commit` of `repo`; `None` if it can't be read there.
    fn file_at(&self, repo: &RepoName, commit: &str, path: &str) -> Result<Option<Vec<u8>>>;
}

impl Sources for Mirrors {
    fn has_commit(&self, repo: &RepoName, commit: &str) -> bool {
        Mirrors::has_commit(self, repo, commit)
    }

    fn file_at(&self, repo: &RepoName, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        Mirrors::file_at(self, repo, commit, path)
    }
}

/// Everything the dashboard reads and acts through.
pub struct Context {
    /// The GitHub login everything is judged relative to.
    pub me: String,
    /// `serve --manual-reviews`.
    pub manual_reviews: bool,
    /// Where runs keep their files, under `runs/<id>/`.
    pub data_dir: PathBuf,
    /// `serve`'s config file, for the commands the dashboard shows.
    pub config_path: PathBuf,
    /// The dashboard's own connection; it edits drafts and records views.
    pub store: Store,
    /// Posts reviews, their replies and thumbs-ups, and nothing else.
    pub github: Client,
    pub control: Arc<dyn Control>,
    /// The files the diffs are of: the runner's mirrors.
    pub sources: Arc<dyn Sources>,
    /// When the scheduler will queue each debounced review.
    pub due: watch::Receiver<HashMap<PrKey, Instant>>,
    /// Which PRs aren't reviewed automatically; follows config reloads.
    pub skips: watch::Receiver<SkipRules>,
    /// The recency window: PRs quiet for longer are left out.
    pub window: watch::Receiver<RecencyWindow>,
    pub clock: Arc<dyn Clock>,
}

struct App {
    me: String,
    manual_reviews: bool,
    data_dir: PathBuf,
    config_path: PathBuf,
    store: Mutex<Store>,
    github: Client,
    control: Arc<dyn Control>,
    sources: Arc<dyn Sources>,
    due: watch::Receiver<HashMap<PrKey, Instant>>,
    skips: watch::Receiver<SkipRules>,
    window: watch::Receiver<RecencyWindow>,
    clock: Arc<dyn Clock>,
    csrf: Csrf,
    /// Approvals picked on the PR page's verdict form, by pick id; see
    /// [`submit::Pick`].
    picks: Mutex<HashMap<String, submit::Pick>>,
    /// Held while a review is being posted, so two confirms can't both
    /// post before either marks its drafts posted. It holds each run's
    /// posted payloads, as `<run>:<payload>`, so a second confirm of an
    /// approval with no drafts can't post it twice.
    posting: tokio::sync::Mutex<HashSet<String>>,
    /// Drafts GitHub has that the store failed to mark posted, so they're
    /// never sent again while `serve` runs.
    taken: Mutex<HashSet<i64>>,
}

impl App {
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn picks(&self) -> MutexGuard<'_, HashMap<String, submit::Pick>> {
        self.picks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn taken(&self) -> MutexGuard<'_, HashSet<i64>> {
        self.taken.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

type Shared = Arc<App>;

/// The dashboard, ready to serve.
pub struct Dashboard {
    app: Shared,
}

impl Dashboard {
    pub fn new(ctx: Context) -> Result<Self> {
        let Context {
            me,
            manual_reviews,
            data_dir,
            config_path,
            store,
            github,
            control,
            sources,
            due,
            skips,
            window,
            clock,
        } = ctx;
        Ok(Self {
            app: Arc::new(App {
                me,
                manual_reviews,
                data_dir,
                config_path,
                store: Mutex::new(store),
                github,
                control,
                sources,
                due,
                skips,
                window,
                clock,
                csrf: Csrf::generate()?,
                picks: Mutex::new(HashMap::new()),
                posting: tokio::sync::Mutex::new(HashSet::new()),
                taken: Mutex::new(HashSet::new()),
            }),
        })
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/", get(index::index))
            .route("/pr/{owner}/{name}/{number}", get(pr::page))
            .route(
                "/pr/{owner}/{name}/{number}/review-now",
                get(pr::confirm_review_now).post(pr::review_now),
            )
            .route("/pr/{owner}/{name}/{number}/archive", post(pr::archive))
            .route(
                "/pr/{owner}/{name}/{number}/ignore",
                get(ignore::editor).post(ignore::save),
            )
            .route(
                "/pr/{owner}/{name}/{number}/ignore/preview",
                get(ignore::preview),
            )
            .route(
                "/pr/{owner}/{name}/{number}/runs/{run}/preview",
                get(submit::preview).post(submit::pick),
            )
            .route(
                "/pr/{owner}/{name}/{number}/runs/{run}/submit",
                post(submit::submit),
            )
            .route(
                "/pr/{owner}/{name}/{number}/runs/{run}/file",
                get(files::file),
            )
            .route(
                "/pr/{owner}/{name}/{number}/runs/{run}/context",
                get(files::context),
            )
            .route(
                "/pr/{owner}/{name}/{number}/runs/{run}/regenerate",
                get(regenerate::confirm).post(regenerate::regenerate),
            )
            .route("/window", post(index::set_window))
            .route("/drafts/{id}/edit", post(pr::edit_draft))
            .route("/drafts/{id}/status", post(pr::set_draft_status))
            .route("/drafts/{id}/thread", post(pr::choose_thread))
            .route("/assets/{file}", get(assets::asset))
            .route("/favicon.ico", get(assets::favicon_ico))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&self.app),
                guard::guard,
            ))
            .with_state(Arc::clone(&self.app))
    }

    /// Binds `127.0.0.1:port`, so the address is known before serving;
    /// port 0 picks a free one.
    pub async fn bind(self, port: u16) -> Result<Bound> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .wrap_err_with(|| format!("binding the dashboard to {addr}"))?;
        let addr = listener
            .local_addr()
            .wrap_err("reading the dashboard's address")?;
        Ok(Bound {
            dashboard: self,
            listener,
            addr,
        })
    }
}

/// The dashboard, bound and ready to serve.
pub struct Bound {
    dashboard: Dashboard,
    listener: tokio::net::TcpListener,
    addr: SocketAddr,
}

impl Bound {
    /// The index page's URL, with a trailing `/`.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// Serves until the process ends.
    pub async fn serve(self) -> Result<()> {
        info!(url = %self.url(), "dashboard listening");
        axum::serve(self.listener, self.dashboard.router())
            .await
            .wrap_err("serving the dashboard")
    }
}

/// A PR as the dashboard's URLs name it.
#[derive(Debug, serde::Deserialize)]
struct PrPath {
    owner: String,
    name: String,
    number: u32,
}

impl PrPath {
    /// The PR, if the path could name one: GitHub owners and repo names
    /// are letters, digits, `-`, `_` and `.`, so nothing else ever reaches
    /// a URL or header this builds.
    fn key(&self) -> Result<PrKey, Error> {
        let valid = |part: &str| {
            !matches!(part, "" | "." | "..")
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if !valid(&self.owner) || !valid(&self.name) {
            return Err(Error::NotFound(format!(
                "`{}/{}` isn't a GitHub repository name",
                self.owner, self.name
            )));
        }
        Ok(PrKey {
            repo: RepoName::new(&self.owner, &self.name),
            number: self.number,
        })
    }
}

/// The dashboard page for `key`, as a path from the dashboard's root.
#[must_use]
pub fn pr_href(key: &PrKey) -> String {
    format!("/pr/{}/{}/{}", key.repo.owner, key.repo.name, key.number)
}

/// A handler failure, shown as an error page.
enum Error {
    NotFound(String),
    /// A request the dashboard won't carry out, with why.
    Refused(String),
    /// Something broke; logged with the PR's URL when there is one.
    Internal {
        url: Option<String>,
        report: eyre::Report,
    },
}

impl Error {
    /// Wraps a failure while handling `key`.
    fn pr(key: &PrKey) -> impl FnOnce(eyre::Report) -> Self {
        let url = key.url();
        move |report| Self::Internal {
            url: Some(url),
            report,
        }
    }
}

impl From<eyre::Report> for Error {
    fn from(report: eyre::Report) -> Self {
        Self::Internal { url: None, report }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound(what) => (StatusCode::NOT_FOUND, what),
            Self::Refused(why) => (StatusCode::CONFLICT, why),
            Self::Internal { url, report } => {
                if let Some(url) = url {
                    warn!(url = %url, "dashboard request failed: {report:?}");
                } else {
                    warn!("dashboard request failed: {report:?}");
                }
                (StatusCode::INTERNAL_SERVER_ERROR, format!("{report:#}"))
            }
        };
        (status, page::error(status, &message)).into_response()
    }
}
