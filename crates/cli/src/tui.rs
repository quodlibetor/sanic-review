//! `serve --ui tui`: a read-only summary of tracked PRs, recent activity and
//! the log.
//!
//! It runs on its own thread with its own read-only store connection, and
//! rereads the store on an interval. Its only actions are asking `serve`
//! to rerun a review, to archive or unarchive a PR and to save the panes'
//! layout, and opening the dashboard in a browser; nothing in it edits
//! drafts.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr, eyre};
use ratatui::{
    DefaultTerminal, Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        execute,
        terminal::{EnterAlternateScreen, enable_raw_mode},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph},
};
use sanic_core::{
    clock::{Clock, window_start},
    pr::PrKey,
    skip::{PrFacts, Skip, SkipRules},
    start::Why,
    state::{PrState, Urgency},
};
use sanic_store::{Activity, ActivityKind, MyPr, OwedReview, RunCounts, Store};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;

pub use self::layout::Sizes;
use self::{
    ignore::{IgnoreEditor, Outcome},
    layout::Size,
};
use crate::{chat, config_edit, logging::LogLines, poll::Progress, schedule::DueTimes};

mod ignore;
mod layout;

/// How often the store is reread.
const REFRESH: Duration = Duration::from_secs(1);
/// How long to wait for a key before redrawing.
const TICK: Duration = Duration::from_millis(100);
const ACTIVITY_ROWS: u32 = 200;

/// The running UI. Dropping it stops the UI and restores the terminal.
pub struct Tui {
    stop: Arc<AtomicBool>,
    done: oneshot::Receiver<Result<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Tui {
    /// Takes over the terminal. `db` must already be migrated.
    pub fn start(db: &Path, shared: Shared) -> Result<Self> {
        let store = Store::open_read_only(db)?;
        let mut terminal = init_terminal().wrap_err("starting the terminal UI")?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, done) = oneshot::channel();
        let thread = std::thread::Builder::new().name("tui".into()).spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut app = App::new(shared.manual_reviews);
                app.set_sizes(load_layout(&store));
                let result = run(&mut terminal, &mut app, &store, &shared, &stop);
                ratatui::restore();
                let _ = tx.send(result);
            }
        });
        match thread {
            Ok(thread) => Ok(Self {
                stop,
                done,
                thread: Some(thread),
            }),
            Err(err) => {
                ratatui::restore();
                Err(err).wrap_err("starting the terminal UI thread")
            }
        }
    }

    /// Resolves when the user quits or the UI fails.
    pub async fn closed(&mut self) -> Result<()> {
        (&mut self.done)
            .await
            .unwrap_or_else(|_| Err(eyre!("the terminal UI stopped unexpectedly")))
    }
}

/// Like [`ratatui::try_init`], but a panic on a runtime worker thread
/// leaves the terminal alone: the worker records a panicking review as
/// crashed and `serve` carries on.
fn init_terminal() -> std::io::Result<DefaultTerminal> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if matches!(std::thread::current().name(), Some("main" | "tui")) {
            ratatui::restore();
            previous(info);
        } else {
            // Printing it would corrupt the screen; the log pane shows it,
            // under the span of whatever panicked.
            tracing::error!("{info}");
        }
    }));
    let started = enable_raw_mode()
        .and_then(|()| execute!(std::io::stdout(), EnterAlternateScreen))
        .and_then(|()| Terminal::new(CrosstermBackend::new(std::io::stdout())));
    if started.is_err() {
        ratatui::restore();
    }
    started
}

/// Hands the terminal to a chat with `key`'s latest session, and takes it
/// back when the chat ends. `serve` keeps running meanwhile. Returns what
/// to tell you about it.
fn chat(terminal: &mut DefaultTerminal, shared: &Shared, key: &PrKey) -> Result<String> {
    let paths = chat::Paths {
        config: shared.config_path.clone(),
        data_dir: shared.data_dir.clone(),
    };
    shared.chatting.store(true, Ordering::SeqCst);
    ratatui::restore();
    let outcome = shared.runtime.block_on(async {
        let chat = chat::Chat::prepare(&paths, &chat::Target::Pr(key.clone()), false).await?;
        chat.run().await
    });
    shared.chatting.store(false, Ordering::SeqCst);
    enable_raw_mode()
        .and_then(|()| execute!(std::io::stdout(), EnterAlternateScreen))
        .and_then(|()| terminal.clear())
        .wrap_err("taking the terminal back after the chat")?;
    Ok(match outcome {
        Ok(()) => format!("chat about {} ended", key.url()),
        Err(err) => {
            warn!(url = %key.url(), "chat failed: {err:?}");
            format!("chat about {} failed: {err}", key.url())
        }
    })
}

/// Saves the panes' layout for the next start; `serve` does it, since the
/// UI's connection is read-only.
pub fn save_layout(store: &Store, sizes: Sizes) -> Result<()> {
    layout::save(store, sizes)
}

/// The layout [`save_layout`] saved last, or every pane fitting its
/// content.
pub fn load_layout(store: &Store) -> Sizes {
    layout::load(store)
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the rest of `serve` shares with the UI, besides the store.
pub struct Shared {
    /// The GitHub login everything is judged relative to.
    pub me: String,
    /// `serve --manual-reviews`.
    pub manual_reviews: bool,
    pub logs: LogLines,
    /// When the scheduler will queue each debounced review.
    pub due: watch::Receiver<DueTimes>,
    /// Which PRs aren't reviewed automatically; follows config reloads.
    pub skips: watch::Receiver<SkipRules>,
    /// `poll.updated_within_days`: PRs quiet for longer are left out.
    pub window: watch::Receiver<Option<u32>>,
    /// The poller's refresh batch under way, if any.
    pub progress: watch::Receiver<Option<Progress>>,
    pub clock: Arc<dyn Clock>,
    /// What you ask `serve` to do.
    pub requests: mpsc::UnboundedSender<Request>,
    /// The ignore editor adds `skip_titles` here; `serve` reloads it.
    pub config_path: PathBuf,
    /// Where `c`'s chats check worktrees out.
    pub data_dir: PathBuf,
    /// Runs `c`'s chats from the UI thread.
    pub runtime: tokio::runtime::Handle,
    /// Set while a chat has the terminal, so `serve` leaves Ctrl-C to it.
    pub chatting: Arc<AtomicBool>,
    /// The dashboard index's URL, which `o` opens pages under.
    pub dashboard: String,
    /// What `o` opens them with.
    pub opener: Arc<dyn Opener>,
}

/// A dashboard page `o` opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Index,
    Pr(PrKey),
}

impl Page {
    /// Its URL, given the index's.
    fn url(&self, index: &str) -> String {
        match self {
            Self::Index => index.to_owned(),
            Self::Pr(key) => format!("{}{}", index.trim_end_matches('/'), sanic_web::pr_href(key)),
        }
    }
}

/// Opens a URL in a browser.
pub trait Opener: Send + Sync {
    fn open(&self, url: &str) -> Result<()>;
}

/// Your default browser.
pub struct SystemBrowser;

impl Opener for SystemBrowser {
    fn open(&self, url: &str) -> Result<()> {
        // A graphical browser is spawned with stdin, stdout and stderr
        // null, so it can't write over the UI. A text browser named by
        // `$BROWSER` still runs in the terminal, as `webbrowser` runs those.
        let mut options = webbrowser::BrowserOptions::new();
        options.with_suppress_output(true);
        webbrowser::open_browser_with_options(webbrowser::Browser::Default, url, &options)
            .wrap_err_with(|| format!("opening {url} in a browser"))
    }
}

/// Opens pages off the UI thread, since a browser can take a while to
/// start, and hands back what went wrong to show.
struct Launcher {
    opener: Arc<dyn Opener>,
    failures_tx: std_mpsc::Sender<String>,
    failures: std_mpsc::Receiver<String>,
}

impl Launcher {
    fn new(opener: Arc<dyn Opener>) -> Self {
        let (failures_tx, failures) = std_mpsc::channel();
        Self {
            opener,
            failures_tx,
            failures,
        }
    }

    /// Starts opening `page`, and says so.
    fn open(&self, page: &Page, index: &str) -> String {
        let url = page.url(index);
        let opener = Arc::clone(&self.opener);
        let failures = self.failures_tx.clone();
        let spawned = std::thread::Builder::new().name("browser".into()).spawn({
            let url = url.clone();
            let page = page.clone();
            move || {
                if let Err(err) = opener.open(&url) {
                    let failure = open_failed(&page, &url, &err);
                    // Only fails once the UI has stopped.
                    let _ = failures.send(failure);
                }
            }
        });
        match spawned {
            Ok(_) => format!("opening {url}"),
            Err(err) => open_failed(page, &url, &eyre!(err)),
        }
    }

    /// The newest failure since the last call, if any.
    fn failure(&self) -> Option<String> {
        self.failures.try_iter().last()
    }
}

/// Logs a failure to open `page` at `url`, and says what to show.
fn open_failed(page: &Page, url: &str, err: &color_eyre::eyre::Report) -> String {
    match page {
        Page::Pr(key) => {
            warn!(url = %key.url(), dashboard = url, "opening the dashboard failed: {err:?}");
        }
        Page::Index => warn!(dashboard = url, "opening the dashboard failed: {err:?}"),
    }
    format!("couldn't open {url}: {err}")
}

fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    store: &Store,
    shared: &Shared,
    stop: &AtomicBool,
) -> Result<()> {
    let mut loaded_at: Option<Instant> = None;
    let mut load_failed = false;
    let launcher = Launcher::new(Arc::clone(&shared.opener));
    while !stop.load(Ordering::Relaxed) {
        if let Some(failure) = launcher.failure() {
            app.set_notice(failure);
        }
        if loaded_at.is_none_or(|at| at.elapsed() >= REFRESH) {
            loaded_at = Some(Instant::now());
            // A failed read keeps the last data; warn once per failure spell
            // rather than every refresh.
            match Overview::load(store, shared) {
                Ok(overview) => {
                    app.set_overview(overview);
                    load_failed = false;
                }
                Err(err) if !load_failed => {
                    warn!("reading the store for the terminal UI failed: {err:?}");
                    load_failed = true;
                }
                Err(_) => {}
            }
        }
        let (lines, dropped) = shared.logs.snapshot();
        app.set_logs(lines, dropped);
        terminal
            .draw(|frame| render(frame, app))
            .wrap_err("drawing the terminal UI")?;
        if event::poll(TICK).wrap_err("reading terminal input")?
            && let Event::Key(key) = event::read().wrap_err("reading terminal input")?
        {
            match app.handle_key(key) {
                Flow::Continue => {}
                Flow::Quit => break,
                // Only fails once `serve` is stopping.
                Flow::Request(request) => {
                    let _ = shared.requests.send(request);
                }
                Flow::Chat(key) => {
                    let notice = chat(terminal, shared, &key)?;
                    app.set_notice(notice);
                }
                Flow::Open(page) => app.set_notice(launcher.open(&page, &shared.dashboard)),
                Flow::AddSkipTitle { pattern, profile } => {
                    app.set_notice(add_skip_title(
                        &shared.config_path,
                        &pattern,
                        profile.as_deref(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Adds `pattern` to the config file's `skip_titles`, and says how that
/// went.
fn add_skip_title(config: &Path, pattern: &str, profile: Option<&str>) -> String {
    let place = profile.map_or_else(
        || "[review_requests]".to_owned(),
        |p| format!("[profile.{p}]"),
    );
    match config_edit::add_skip_title_to_file(config, profile, pattern) {
        Ok(true) => format!("added `{pattern}` to skip_titles in {place}"),
        Ok(false) => format!("`{pattern}` is already in {place} skip_titles"),
        Err(err) => {
            warn!("adding a skip_titles pattern failed: {err:?}");
            format!("couldn't add `{pattern}`: {err}")
        }
    }
}

/// Everything the panes show from the store.
#[derive(Debug, Default, Clone)]
pub struct Overview {
    pub owed: Vec<OwedReview>,
    pub mine: Vec<MyPr>,
    /// Newest first.
    pub activity: Vec<Activity>,
    pub counts: RunCounts,
    /// The poller's refresh batch under way, if any.
    pub refreshing: Option<Progress>,
    /// PRs with a review running, which quitting cancels.
    pub running: Vec<PrKey>,
    /// How long until each debounced review is queued.
    pub waiting: HashMap<PrKey, Duration>,
    /// Owed reviews that aren't reviewed automatically, and why.
    pub skipped: HashMap<PrKey, Skip>,
    /// The config's profiles, in file order.
    pub profiles: Vec<String>,
}

impl Overview {
    fn load(store: &Store, shared: &Shared) -> Result<Self> {
        let now = Instant::now();
        let waiting = shared
            .due
            .borrow()
            .iter()
            .map(|(key, due)| (key.clone(), due.saturating_duration_since(now)))
            .collect();
        let since = window_start(shared.clock.now(), *shared.window.borrow());
        let owed = store.owed_reviews(&shared.me, since.as_deref())?;
        let skips = shared.skips.borrow();
        let skipped = owed
            .iter()
            .filter_map(|pr| {
                let facts = PrFacts {
                    profile: &pr.profile,
                    title: &pr.title,
                    is_draft: pr.is_draft,
                    archived: pr.archived,
                    head_sha: &pr.head_sha,
                    head_reviewers: &pr.head_reviewers,
                    me: &shared.me,
                };
                Some((pr.key.clone(), skips.decide(&facts)?))
            })
            .collect();
        let profiles = skips.profile_names().to_vec();
        // Not held through the queries below: it blocks a config reload.
        drop(skips);
        Ok(Self {
            owed,
            skipped,
            profiles,
            mine: store.my_prs(&shared.me, since.as_deref())?,
            activity: store.recent_activity(ACTIVITY_ROWS)?,
            counts: store.run_counts()?,
            refreshing: *shared.progress.borrow(),
            running: store.running_reviews()?,
            waiting,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Owed,
    Mine,
    Activity,
    Log,
}

impl Pane {
    const ALL: [Self; 4] = [Self::Owed, Self::Mine, Self::Activity, Self::Log];

    fn index(self) -> usize {
        self as usize
    }

    fn step(self, forward: bool) -> Self {
        let n = Self::ALL.len();
        let i = if forward {
            self.index() + 1
        } else {
            self.index() + n - 1
        };
        Self::ALL[i % n]
    }

    /// Its name, split around the letter that jumps to it.
    fn name(self) -> (&'static str, char, &'static str) {
        match self {
            Self::Owed => ("Revie", 'w', "s you owe"),
            Self::Mine => ("Your ", 'P', "Rs"),
            Self::Activity => ("", 'A', "ctivity"),
            Self::Log => ("", 'L', "og"),
        }
    }

    fn label(self) -> String {
        let (before, letter, after) = self.name();
        format!("{before}{letter}{after}")
    }

    /// The key that jumps to it.
    fn key(self) -> char {
        self.name().1.to_ascii_lowercase()
    }

    fn with_key(key: char) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.key() == key)
    }

    /// Its name and `suffix` for a title or a tab, the jump letter
    /// highlighted.
    fn title(self, suffix: &str, style: Style) -> Vec<Span<'static>> {
        let (before, letter, after) = self.name();
        vec![
            Span::styled(format!(" {before}"), style),
            Span::styled(letter.to_string(), style.patch(JUMP_LETTER)),
            Span::styled(format!("{after}{suffix} "), style),
        ]
    }
}

/// Keys that act on the focused pane's rows, which a collapsed pane
/// doesn't show.
fn acts_on_rows(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char('j' | 'k' | 'g' | 'G' | 'r' | 'x' | 'i' | 'c' | 'o')
            | KeyCode::Down
            | KeyCode::Up
            | KeyCode::Home
            | KeyCode::End
    )
}

/// How a pane's jump letter stands out in its title and its tab.
const JUMP_LETTER: Style = Style::new()
    .fg(Color::Yellow)
    .add_modifier(Modifier::UNDERLINED);

pub struct App {
    /// Everything last read from the store.
    loaded: Overview,
    /// What the panes show: `loaded` without archived PRs, unless shown.
    overview: Overview,
    logs: Vec<String>,
    /// Log lines dropped from the front of `logs` so far.
    logs_dropped: u64,
    /// `serve --manual-reviews`: queued reviews are held, not waiting their turn.
    manual_reviews: bool,
    focus: Pane,
    /// How much room each pane takes; `serve` saves it as it changes.
    sizes: Sizes,
    /// Per pane, by [`Pane::index`].
    lists: [ListState; 4],
    /// The log pane shows the newest lines until you move up in it.
    follow_log: bool,
    show_archived: bool,
    overlay: Option<Overlay>,
    /// Shown in the status bar until the next key.
    notice: Option<String>,
}

/// A popup over the panes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Overlay {
    Help,
    Ignore(Box<IgnoreEditor>),
    /// Waiting for a yes before asking for a review of this PR now.
    ConfirmRerun {
        key: PrKey,
        why: Why,
    },
    /// Waiting for a yes before quitting, which cancels these reviews.
    ConfirmQuit(Vec<PrKey>),
}

/// What the loop does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Flow {
    Continue,
    Quit,
    /// Something for `serve` to do.
    Request(Request),
    /// Hand the terminal to a chat with this PR's agent.
    Chat(PrKey),
    /// Open this dashboard page in a browser.
    Open(Page),
    /// Add a `skip_titles` glob to the config file, globally for `None`.
    AddSkipTitle {
        pattern: String,
        profile: Option<String>,
    },
}

/// What the UI asks `serve` to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// You confirmed a rerun of this PR's review.
    Rerun(PrKey),
    Archive {
        key: PrKey,
        archived: bool,
    },
    /// The panes' layout changed; keep it for the next start.
    SaveLayout(Sizes),
}

impl App {
    #[must_use]
    pub fn new(manual_reviews: bool) -> Self {
        Self {
            loaded: Overview::default(),
            overview: Overview::default(),
            logs: Vec::new(),
            logs_dropped: 0,
            manual_reviews,
            focus: Pane::Owed,
            sizes: Sizes::default(),
            lists: Default::default(),
            follow_log: true,
            show_archived: false,
            overlay: None,
            notice: None,
        }
    }

    pub fn set_overview(&mut self, overview: Overview) {
        self.loaded = overview;
        self.refilter();
    }

    fn refilter(&mut self) {
        let mut overview = self.loaded.clone();
        if !self.show_archived {
            overview.owed.retain(|pr| !pr.archived);
            overview.mine.retain(|pr| !pr.archived);
        }
        self.overview = overview;
        self.clamp();
    }

    /// `dropped` counts lines gone from the front since startup; a log
    /// you've scrolled up in shifts with them to stay on the same line.
    pub fn set_logs(&mut self, logs: Vec<String>, dropped: u64) {
        let shift =
            usize::try_from(dropped.saturating_sub(self.logs_dropped)).unwrap_or(usize::MAX);
        self.logs = logs;
        self.logs_dropped = dropped;
        let state = &mut self.lists[Pane::Log.index()];
        if !self.follow_log
            && let Some(selected) = state.selected()
        {
            state.select(Some(selected.saturating_sub(shift)));
        }
        self.clamp();
    }

    fn len(&self, pane: Pane) -> usize {
        match pane {
            Pane::Owed => self.overview.owed.len(),
            Pane::Mine => self.overview.mine.len(),
            Pane::Activity => self.overview.activity.len(),
            Pane::Log => self.logs.len(),
        }
    }

    /// Keeps selections in range as lists shrink, and pins a following log
    /// to its newest line.
    fn clamp(&mut self) {
        for pane in Pane::ALL {
            let last = self.len(pane).checked_sub(1);
            let state = &mut self.lists[pane.index()];
            if pane == Pane::Log && self.follow_log {
                state.select(last);
            } else if let (Some(selected), Some(last)) = (state.selected(), last) {
                state.select(Some(selected.min(last)));
            } else if last.is_none() {
                state.select(None);
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Flow {
        if key.kind != KeyEventKind::Press {
            return Flow::Continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.notice = None;
        if let Some(Overlay::Ignore(editor)) = &mut self.overlay {
            return match editor.handle_key(key, &self.overview.profiles) {
                Outcome::Open => Flow::Continue,
                Outcome::Quit => {
                    self.overlay = None;
                    self.quit()
                }
                Outcome::Cancel => {
                    self.overlay = None;
                    Flow::Continue
                }
                Outcome::Save { pattern, profile } => {
                    self.overlay = None;
                    Flow::AddSkipTitle { pattern, profile }
                }
            };
        }
        if let Some(Overlay::ConfirmQuit(_)) = &self.overlay {
            self.overlay = None;
            return match key.code {
                // A second Ctrl-C doesn't ask again.
                KeyCode::Char('c') if ctrl => Flow::Quit,
                KeyCode::Char('y') | KeyCode::Enter => Flow::Quit,
                _ => Flow::Continue,
            };
        }
        if let Some(Overlay::ConfirmRerun { key: pr, .. }) = &self.overlay {
            let pr = pr.clone();
            self.overlay = None;
            return match key.code {
                KeyCode::Char('c') if ctrl => self.quit(),
                KeyCode::Char('y') => Flow::Request(Request::Rerun(pr)),
                // Anything else is a no, so a stray key can't spend tokens.
                _ => Flow::Continue,
            };
        }
        if !ctrl && !self.sizes.shown(self.focus) && acts_on_rows(key.code) {
            self.notice = Some(format!(
                "{} is collapsed · z or {} shows it",
                self.focus.label(),
                self.focus.key()
            ));
            return Flow::Continue;
        }
        match key.code {
            KeyCode::Char('q') => return self.quit(),
            KeyCode::Char('c') if ctrl => return self.quit(),
            KeyCode::Char('r') => self.ask_rerun(),
            KeyCode::Char('x') => return self.toggle_archive(),
            KeyCode::Char('i') => self.open_ignore(),
            KeyCode::Char('c') => return self.open_chat(),
            KeyCode::Char('o') => return Flow::Open(self.selected_page()),
            KeyCode::Char(c) if let Some(pane) = Pane::with_key(c) => return self.jump(pane),
            KeyCode::Char('z') => return self.resize(Sizes::cycle),
            KeyCode::Char('Z') => return self.resize(|sizes, _| *sizes = Sizes::default()),
            KeyCode::Char('X') => {
                self.show_archived = !self.show_archived;
                self.notice = Some(if self.show_archived {
                    "showing archived PRs".into()
                } else {
                    "hiding archived PRs".into()
                });
                self.refilter();
            }
            KeyCode::Char('?') => {
                self.overlay = (self.overlay != Some(Overlay::Help)).then_some(Overlay::Help);
            }
            KeyCode::Esc => self.overlay = None,
            KeyCode::Tab => return self.step(true),
            KeyCode::BackTab => return self.step(false),
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_to(0),
            KeyCode::Char('G') | KeyCode::End => self.move_to(usize::MAX),
            _ => {}
        }
        Flow::Continue
    }

    /// Takes a saved layout, focusing the first pane it shows so keys don't
    /// start on one that's hidden.
    fn set_sizes(&mut self, sizes: Sizes) {
        self.sizes = sizes;
        if let Some(pane) = Pane::ALL.into_iter().find(|&p| sizes.shown(p)) {
            self.focus = pane;
        }
    }

    /// Focuses `pane` and shows it: a collapsed pane fits again, and one
    /// behind another's full screen takes it over.
    fn jump(&mut self, pane: Pane) -> Flow {
        self.focus = pane;
        self.resize(|sizes, pane| match sizes.full() {
            Some(full) if full != pane => sizes.set(pane, Size::Full),
            _ if sizes.get(pane) == Size::Collapsed => sizes.set(pane, Size::Fit),
            _ => {}
        })
    }

    /// Tab: the next pane that isn't collapsed. Under a full screen, it
    /// takes the full screen over.
    fn step(&mut self, forward: bool) -> Flow {
        let mut next = self.focus.step(forward);
        while next != self.focus && self.sizes.get(next) == Size::Collapsed {
            next = next.step(forward);
        }
        if next == self.focus {
            return Flow::Continue;
        }
        self.jump(next)
    }

    /// Changes the layout with `change(sizes, focus)`, and asks `serve` to
    /// save it if that changed anything.
    fn resize(&mut self, change: impl FnOnce(&mut Sizes, Pane)) -> Flow {
        let before = self.sizes;
        change(&mut self.sizes, self.focus);
        if self.sizes == before {
            Flow::Continue
        } else {
            Flow::Request(Request::SaveLayout(self.sizes))
        }
    }

    /// Asks to confirm a review of the selected PR you owe, if its latest
    /// run failed or crashed, it's held, or it's skipped or archived.
    fn ask_rerun(&mut self) {
        let selected = self.lists[Pane::Owed.index()]
            .selected()
            .and_then(|i| self.overview.owed.get(i))
            .filter(|_| self.focus == Pane::Owed);
        let Some(pr) = selected else {
            self.notice = Some(RERUN_HINT.into());
            return;
        };
        let skip = self.overview.skipped.get(&pr.key);
        let Some(why) = Why::of(skip, pr.latest_status(), self.manual_reviews) else {
            self.notice = Some(RERUN_HINT.into());
            return;
        };
        self.overlay = Some(Overlay::ConfirmRerun {
            key: pr.key.clone(),
            why,
        });
    }

    /// Quits, unless reviews are running: then it asks first, since
    /// quitting cancels them.
    fn quit(&mut self) -> Flow {
        if self.loaded.running.is_empty() {
            return Flow::Quit;
        }
        self.overlay = Some(Overlay::ConfirmQuit(self.loaded.running.clone()));
        Flow::Continue
    }

    /// Chats with the agent that last reviewed the selected PR, in either
    /// PR pane.
    fn open_chat(&mut self) -> Flow {
        let selected = self.lists[self.focus.index()].selected();
        let target = match self.focus {
            Pane::Owed => selected
                .and_then(|i| self.overview.owed.get(i))
                .map(|pr| (pr.key.clone(), pr.chat_run)),
            Pane::Mine => selected
                .and_then(|i| self.overview.mine.get(i))
                .map(|pr| (pr.key.clone(), pr.chat_run)),
            Pane::Activity | Pane::Log => None,
        };
        match target {
            Some((key, Some(_))) => Flow::Chat(key),
            Some((key, None)) => {
                self.notice = Some(format!("no review of {} to chat with yet", key.url()));
                Flow::Continue
            }
            None => {
                self.notice = Some("c chats with the agent that reviewed the selected PR".into());
                Flow::Continue
            }
        }
    }

    /// The selected row's PR page, in any pane but the log; else the index.
    fn selected_page(&self) -> Page {
        let selected = self.lists[self.focus.index()].selected();
        let key = match self.focus {
            Pane::Owed => selected
                .and_then(|i| self.overview.owed.get(i))
                .map(|pr| &pr.key),
            Pane::Mine => selected
                .and_then(|i| self.overview.mine.get(i))
                .map(|pr| &pr.key),
            Pane::Activity => selected
                .and_then(|i| self.overview.activity.get(i))
                .map(|a| &a.key),
            Pane::Log => None,
        };
        key.map_or(Page::Index, |key| Page::Pr(key.clone()))
    }

    /// Opens the ignore editor on the selected review you owe.
    fn open_ignore(&mut self) {
        let selected = self.lists[Pane::Owed.index()]
            .selected()
            .and_then(|i| self.overview.owed.get(i))
            .filter(|_| self.focus == Pane::Owed);
        match selected {
            Some(pr) => self.overlay = Some(Overlay::Ignore(Box::new(IgnoreEditor::new(pr)))),
            None => self.notice = Some("i skips reviews you owe by title".into()),
        }
    }

    /// Shows `notice` in the status bar until the next key.
    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /// Archives the selected PR in either PR pane, or unarchives it.
    fn toggle_archive(&mut self) -> Flow {
        let selected = self.lists[self.focus.index()].selected();
        let target = match self.focus {
            Pane::Owed => selected
                .and_then(|i| self.overview.owed.get(i))
                .map(|pr| (pr.key.clone(), pr.archived)),
            Pane::Mine => selected
                .and_then(|i| self.overview.mine.get(i))
                .map(|pr| (pr.key.clone(), pr.archived)),
            Pane::Activity | Pane::Log => None,
        };
        let Some((key, was)) = target else {
            self.notice = Some("x archives the selected PR".into());
            return Flow::Continue;
        };
        let archived = !was;
        // Shown right away; the next read of the store agrees.
        for pr in self.loaded.owed.iter_mut().filter(|pr| pr.key == key) {
            pr.archived = archived;
        }
        for pr in self.loaded.mine.iter_mut().filter(|pr| pr.key == key) {
            pr.archived = archived;
        }
        self.notice = Some(if archived {
            format!("archived {} · X shows archived PRs", key.url())
        } else {
            format!("unarchived {}", key.url())
        });
        self.refilter();
        Flow::Request(Request::Archive { key, archived })
    }

    fn move_by(&mut self, delta: isize) {
        let current = self.lists[self.focus.index()].selected();
        let target = match current {
            // Nothing selected yet: moving selects the first row.
            None => 0,
            Some(i) => i.saturating_add_signed(delta),
        };
        self.move_to(target);
    }

    fn move_to(&mut self, target: usize) {
        let Some(last) = self.len(self.focus).checked_sub(1) else {
            return;
        };
        let target = target.min(last);
        if self.focus == Pane::Log {
            self.follow_log = target == last;
        }
        self.lists[self.focus.index()].select(Some(target));
    }
}

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let [panes, status] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let App {
        overview,
        logs,
        manual_reviews,
        focus,
        sizes,
        lists,
        follow_log,
        show_archived,
        overlay,
        notice,
        loaded,
        ..
    } = app;
    // How many archived PRs a pane is hiding, for its title.
    let counted = |shown: usize, archived: usize| {
        if *show_archived || archived == 0 {
            format!(" ({shown})")
        } else {
            format!(" ({shown} · {archived} archived)")
        }
    };

    let owed: Vec<_> = overview
        .owed
        .iter()
        .map(|pr| owed_row(pr, overview, *manual_reviews))
        .collect();
    let archived = loaded.owed.iter().filter(|pr| pr.archived).count();
    let owed_title = counted(owed.len(), archived);

    let mine: Vec<_> = overview.mine.iter().map(my_row).collect();
    let archived = loaded.mine.iter().filter(|pr| pr.archived).count();
    let mine_title = counted(mine.len(), archived);

    let events = &overview.activity;
    let activity: Vec<_> = events
        .iter()
        .enumerate()
        .map(|(i, a)| {
            // Newest first: a held review is one nothing newer for its PR
            // has started or finished, so history from runs that went ahead
            // still reads "queued".
            let held = *manual_reviews
                && !events[..i].iter().any(|newer| {
                    newer.key == a.key
                        && matches!(
                            newer.kind,
                            ActivityKind::RunStarted | ActivityKind::RunFinished { .. }
                        )
                });
            activity_row(a, held)
        })
        .collect();

    let log: Vec<_> = logs.iter().map(|l| ListItem::new(l.as_str())).collect();

    let sections = [
        (owed, owed_title, "No reviews requested."),
        (mine, mine_title, "No open PRs of yours."),
        (activity, String::new(), "Nothing yet."),
        (log, String::new(), ""),
    ];
    let content = sections
        .each_ref()
        .map(|(rows, ..)| rows.iter().map(ListItem::height).sum());
    let (areas, bar) = layout::arrange(panes, *sizes, content);
    let mut tabs = Vec::new();
    for ((pane, (rows, title, empty)), area) in Pane::ALL.into_iter().zip(sections).zip(areas) {
        let Some(area) = area else {
            tabs.push((pane, title));
            continue;
        };
        let mut frame_ = pane.frame(*focus, &title);
        // A following log keeps its newest line selected to stay scrolled
        // to the bottom; that isn't a selection worth highlighting.
        frame_.highlight &= !(pane == Pane::Log && *follow_log);
        frame_.render(frame, area, rows, &mut lists[pane.index()], empty);
    }
    if let Some(bar) = bar {
        render_tabs(frame, bar, &tabs, *focus);
    }

    render_status(frame, status, overview, *manual_reviews, notice.as_deref());
    match overlay {
        Some(Overlay::Help) => render_help(frame),
        Some(Overlay::Ignore(editor)) => editor.render(frame, &loaded.owed, &overview.profiles),
        Some(Overlay::ConfirmRerun { key, why }) => render_confirm(frame, key, why),
        Some(Overlay::ConfirmQuit(running)) => render_confirm_quit(frame, running),
        None => {}
    }
}

/// The panes that aren't shown, as tabs to jump to, with their titles'
/// `suffix`es.
fn render_tabs(frame: &mut Frame<'_>, area: Rect, tabs: &[(Pane, String)], focus: Pane) {
    let mut spans = Vec::new();
    for (i, (pane, suffix)) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("│", Style::new().fg(Color::DarkGray)));
        }
        spans.extend(pane.title(suffix, border_style(*pane == focus)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A pane's border and whether its selection is highlighted.
struct PaneFrame<'a> {
    block: Block<'a>,
    highlight: bool,
}

/// A focused pane's border and tab stand out.
fn border_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

impl Pane {
    /// `suffix` follows its name in the title.
    fn frame(self, focus: Pane, suffix: &str) -> PaneFrame<'static> {
        let focused = focus == self;
        let border = border_style(focused);
        PaneFrame {
            block: Block::bordered()
                .border_style(border)
                .title(Line::from(self.title(suffix, border))),
            highlight: focused,
        }
    }
}

impl PaneFrame<'_> {
    fn render(
        self,
        frame: &mut Frame<'_>,
        area: Rect,
        rows: Vec<ListItem<'_>>,
        state: &mut ListState,
        empty: &str,
    ) {
        if rows.is_empty() {
            frame.render_widget(Paragraph::new(empty.dim()).block(self.block), area);
            return;
        }
        // Scrolled down while the pane was shorter, it would leave rows
        // blank at the bottom now; the list still scrolls to the selection.
        let deepest = deepest_offset(&rows, self.block.inner(area).height);
        let offset = state.offset_mut();
        *offset = (*offset).min(deepest);
        let highlight = if self.highlight {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        let list = List::new(rows).block(self.block).highlight_style(highlight);
        frame.render_stateful_widget(list, area, state);
    }
}

/// The furthest a list of `rows` can scroll in `height` rows and still
/// fill them: the first row of the longest tail that fits.
fn deepest_offset(rows: &[ListItem<'_>], height: u16) -> usize {
    let mut room = usize::from(height);
    let mut offset = rows.len();
    for row in rows.iter().rev() {
        match room.checked_sub(row.height()) {
            Some(left) => room = left,
            None => break,
        }
        offset -= 1;
    }
    offset
}

const RERUN_HINT: &str = "r reviews a failed, held, skipped or archived PR you owe now";

/// Width of the status column in both PR panes, including the space that
/// keeps a label as long as the column off what follows it.
const STATUS_WIDTH: usize = 15;

/// Width of the PR state column left of the status in "Reviews you owe".
const OWED_STATE_WIDTH: usize = 10;

/// Width of the PR state column in "Your PRs", where it's the status.
const MY_STATE_WIDTH: usize = 17;

/// `state` in at most `width - 1` characters and a space, coloured by how
/// much it asks of you.
fn state_cell(state: PrState, width: usize) -> Span<'static> {
    let color = match state.urgency() {
        Urgency::Act => Color::Yellow,
        Urgency::Good => Color::Green,
        Urgency::Quiet => Color::DarkGray,
    };
    let text = state.fitted(width - 1);
    let style = Style::new().fg(color);
    let style = if state.urgency() == Urgency::Act {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    };
    Span::styled(format!("{text:<w$} ", w = width - 1), style)
}

fn owed_row<'a>(pr: &'a OwedReview, overview: &Overview, manual_reviews: bool) -> ListItem<'a> {
    let latest = pr.latest_run.as_ref();
    let waiting = overview.waiting.get(&pr.key).copied();
    // A skip says why nothing will happen; a review waiting out the quiet
    // period is newer news than the last run.
    let (label, color) = match (waiting, pr.latest_status()) {
        _ if pr.archived => ("archived".into(), Color::DarkGray),
        _ if let Some(skip) = overview.skipped.get(&pr.key) => (skip.status(), Color::DarkGray),
        (Some(left), _) => (format!("waiting {}", countdown(left)), Color::DarkGray),
        (None, None) => ("waiting".into(), Color::DarkGray),
        (None, Some("queued")) if manual_reviews => ("held".into(), Color::Yellow),
        (None, Some("queued")) => ("queued".into(), Color::Yellow),
        (None, Some("running")) => ("running".into(), Color::Cyan),
        (None, Some("succeeded")) => ("drafted".into(), Color::Green),
        (None, Some(status @ ("failed" | "crashed"))) => (status.into(), Color::Red),
        (None, Some(other)) => (other.into(), Color::DarkGray),
    };
    let mut lines = vec![Line::from(vec![
        state_cell(pr.state, OWED_STATE_WIDTH),
        Span::styled(
            format!("{label:<w$} ", w = STATUS_WIDTH - 1),
            Style::new().fg(color),
        ),
        drafts(pr.pending_drafts),
        Span::raw(pr.key.url()),
        Span::raw("  "),
        Span::raw(pr.title.as_str()),
        Span::raw(format!(" ({})", pr.author)).dim(),
    ])];
    // Only the latest run's error: an older failure a later run replaced
    // doesn't need attention.
    if let Some(error) = latest.and_then(|run| run.error.as_deref()) {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(OWED_STATE_WIDTH + STATUS_WIDTH)),
            Span::styled(first_line(error), Style::new().fg(Color::Red)),
        ]));
    }
    dim_if_archived(ListItem::new(lines), pr.archived)
}

/// `m:ss`, or `h:mm:ss` from an hour up.
fn countdown(left: Duration) -> String {
    let secs = left.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn my_row(pr: &MyPr) -> ListItem<'_> {
    let state = if pr.archived {
        Span::styled(
            format!("{:<w$} ", "archived", w = MY_STATE_WIDTH - 1),
            Style::new().fg(Color::DarkGray),
        )
    } else {
        state_cell(pr.state, MY_STATE_WIDTH)
    };
    let mut spans = vec![
        state,
        drafts(pr.pending_drafts),
        Span::raw(pr.key.url()),
        Span::raw("  "),
    ];
    if pr.is_draft {
        spans.push("[draft] ".dim());
    }
    spans.push(Span::raw(pr.title.as_str()));
    dim_if_archived(ListItem::new(Line::from(spans)), pr.archived)
}

fn dim_if_archived(item: ListItem<'_>, archived: bool) -> ListItem<'_> {
    if archived {
        item.style(Style::new().add_modifier(Modifier::DIM))
    } else {
        item
    }
}

/// A fixed-width pending draft count; blank when there are none.
/// Short, to leave the URL room at 80 columns: `✎3` for three.
fn drafts(n: u32) -> Span<'static> {
    let text = match n {
        0 => String::new(),
        n => format!("✎{n}"),
    };
    Span::styled(format!("{text:<3} "), Style::new().fg(Color::Magenta))
}

/// A `held` queued review (`--manual-reviews`) waits until you start it.
fn activity_row(activity: &Activity, held: bool) -> ListItem<'_> {
    let what = match &activity.kind {
        ActivityKind::Trigger(kind) => kind.replace('_', " "),
        ActivityKind::RunQueued if held => "review held".into(),
        ActivityKind::RunQueued => "review queued".into(),
        ActivityKind::RunStarted => "review started".into(),
        ActivityKind::RunFinished { status, .. } if status == "succeeded" => {
            "review drafted".into()
        }
        ActivityKind::RunFinished { status, .. } => format!("review {status}"),
    };
    // `at` is RFC 3339 UTC; the log pane's clock is UTC too.
    let time = activity.at.get(11..19).unwrap_or(&activity.at);
    let mut spans = vec![
        Span::raw(format!("{time}  ")).dim(),
        Span::raw(format!("{what:<18}")),
        Span::raw(activity.key.url()),
    ];
    if let ActivityKind::RunFinished {
        error: Some(error), ..
    } = &activity.kind
    {
        spans.push(Span::styled(
            format!("  {}", first_line(error)),
            Style::new().fg(Color::Red),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn render_status(
    frame: &mut Frame<'_>,
    area: Rect,
    overview: &Overview,
    manual_reviews: bool,
    notice: Option<&str>,
) {
    let counts = &overview.counts;
    // A notice takes the whole line, so it isn't cut off at 80 columns.
    if let Some(notice) = notice {
        let notice = Span::styled(format!(" {notice}"), Style::new().fg(Color::Yellow));
        frame.render_widget(Paragraph::new(notice), area);
        return;
    }
    let mut right = format!(
        "{} queued · {} running · {} pending drafts ",
        counts.queued, counts.running, counts.pending_drafts
    );
    if manual_reviews {
        right.insert_str(0, "manual reviews · ");
    }
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(u16::try_from(right.chars().count()).unwrap_or(u16::MAX)),
    ])
    .areas(area);
    // The key hint makes room while the poller works through a batch, and
    // leaves `o` out where it would be cut off.
    let left = match overview.refreshing {
        Some(p) => Span::styled(
            format!(" refreshing {}/{}", p.done, p.total),
            Style::new().fg(Color::Cyan),
        ),
        None => [" ? help · o dashboard · q quit", " ? help · q quit"]
            .into_iter()
            .find(|hint| hint.chars().count() < usize::from(left_area.width))
            .unwrap_or(" ? help · q quit")
            .dim(),
    };
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
}

const HELP: &[(&str, &str)] = &[
    ("q, Ctrl-C", "quit serve"),
    ("Tab, Shift-Tab", "next, previous pane"),
    ("w/p/a/l", "jump to owed, yours, activity, log"),
    ("z", "pane size: fit, full screen, collapsed"),
    ("Z", "every pane back to fit"),
    ("j/k, Down/Up", "move in the pane"),
    ("g/G, Home/End", "first, last row"),
    ("r", "review now: failed, held or skipped"),
    ("x", "archive or unarchive the selected PR"),
    ("X", "show or hide archived PRs"),
    ("i", "skip PRs with titles like the selected one"),
    ("c", "chat with the agent that reviewed it"),
    ("o", "open it in the dashboard, or the index"),
    ("?, Esc", "close this help"),
];

fn render_help(frame: &mut Frame<'_>) {
    let mut lines: Vec<Line<'_>> = HELP
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::raw(format!(" {keys:<16}")).bold(),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::raw(" Drafts are edited and submitted in the dashboard.").dim());
    render_popup(frame, " Keys ", lines);
}

fn render_confirm(frame: &mut Frame<'_>, pr: &PrKey, why: &Why) {
    let (head, tail) = match why {
        Why::Skipped(Skip::Reviewed { by, .. }) => (
            format!(" Already reviewed by {by}. Review"),
            " anyway, at its current head? This spends tokens.",
        ),
        Why::Skipped(skip) => (
            format!(" Review this {}-skipped PR anyway,", skip.label()),
            " at its current head? This spends tokens.",
        ),
        Why::Failed => (
            " Rerun the review of".into(),
            " at its current head? This spends tokens.",
        ),
        Why::Held => (
            " Start the held review of".into(),
            " now? This spends tokens.",
        ),
    };
    let lines = vec![
        Line::raw(head),
        Line::raw(format!(" {}", pr.url())),
        Line::raw(tail),
        Line::raw(""),
        Line::from(vec![
            Span::raw(" y").bold(),
            Span::raw(" review now · any other key cancels"),
        ]),
    ];
    render_popup(frame, " Review now ", lines);
}

fn render_confirm_quit(frame: &mut Frame<'_>, running: &[PrKey]) {
    let mut lines = vec![Line::raw(" Exiting will cancel these running tasks:")];
    lines.extend(
        running
            .iter()
            .map(|pr| Line::raw(format!(" · {} — reviewing", pr.url()))),
    );
    lines.push(Line::raw(" They run again on the next start.").dim());
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw(" Enter/y").bold(),
        Span::raw(" exit · any other key stays"),
    ]));
    render_popup(frame, " Exit? ", lines);
}

fn render_popup(frame: &mut Frame<'_>, title: &str, lines: Vec<Line<'_>>) {
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let area = frame
        .area()
        .centered(Constraint::Length(62), Constraint::Length(height));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};
    use sanic_core::state::{Approval, Checks};
    use sanic_core::{pr::PrKey, repo::RepoName};
    use sanic_store::LatestRun;

    use super::*;

    fn pr(repo: &str, number: u32) -> PrKey {
        PrKey {
            repo: RepoName::parse(repo).unwrap(),
            number,
        }
    }

    fn latest(status: &str, error: Option<&str>) -> LatestRun {
        LatestRun {
            status: status.into(),
            error: error.map(Into::into),
        }
    }

    /// An owed review with no runs.
    fn owed(repo: &str, number: u32, title: &str, author: &str) -> OwedReview {
        OwedReview {
            key: pr(repo, number),
            title: title.into(),
            body: format!("{title}, in detail."),
            author: author.into(),
            profile: "default".into(),
            is_draft: false,
            archived: false,
            head_sha: "h1".into(),
            head_reviewers: vec![],
            chat_run: None,
            state: PrState::default(),
            latest_run: None,
            pending_drafts: 0,
        }
    }

    /// One of your PRs with nothing going on.
    fn my(repo: &str, number: u32, title: &str) -> MyPr {
        MyPr {
            key: pr(repo, number),
            title: title.into(),
            is_draft: false,
            archived: false,
            pending_drafts: 0,
            chat_run: None,
            state: PrState::default(),
        }
    }

    fn overview() -> Overview {
        Overview {
            owed: vec![
                OwedReview {
                    latest_run: Some(latest("succeeded", None)),
                    state: PrState {
                        approval: Approval::Approved(Checks::Pending),
                        ..PrState::default()
                    },
                    pending_drafts: 3,
                    ..owed("org/api", 481, "Retry webhook deliveries", "alice")
                },
                owed("org/api", 490, "build(deps): bump tokio", "dependabot"),
                OwedReview {
                    latest_run: Some(latest("queued", None)),
                    state: PrState {
                        unanswered: 2,
                        ..PrState::default()
                    },
                    ..owed("org/web", 77, "Fix login flake", "bob")
                },
                owed("org/web", 78, "Bump serde", "dana"),
                OwedReview {
                    latest_run: Some(latest(
                        "crashed",
                        Some("index out of bounds\nat src/lib.rs"),
                    )),
                    ..owed("org/web", 79, "Cache avatars", "carol")
                },
            ],
            mine: vec![
                MyPr {
                    state: PrState {
                        approval: Approval::ChangesRequested,
                        unanswered: 1,
                        mine: true,
                    },
                    ..my("org/api", 470, "Speed up search")
                },
                MyPr {
                    is_draft: true,
                    state: PrState {
                        approval: Approval::Mergeable,
                        unanswered: 0,
                        mine: true,
                    },
                    ..my("org/web", 80, "New settings page")
                },
                MyPr {
                    archived: true,
                    ..my("org/web", 60, "Abandoned experiment")
                },
            ],
            activity: vec![
                Activity {
                    at: "2026-09-23T16:36:00.000Z".into(),
                    key: pr("org/web", 79),
                    kind: ActivityKind::RunFinished {
                        status: "crashed".into(),
                        error: Some("index out of bounds\nat src/lib.rs".into()),
                    },
                },
                Activity {
                    at: "2026-09-23T16:35:10.120Z".into(),
                    key: pr("org/api", 481),
                    kind: ActivityKind::RunFinished {
                        status: "succeeded".into(),
                        error: None,
                    },
                },
                Activity {
                    at: "2026-09-23T16:33:02.004Z".into(),
                    key: pr("org/api", 481),
                    kind: ActivityKind::RunStarted,
                },
                Activity {
                    at: "2026-09-23T16:33:01.500Z".into(),
                    key: pr("org/api", 481),
                    kind: ActivityKind::Trigger("review_requested".into()),
                },
            ],
            counts: RunCounts {
                queued: 1,
                running: 0,
                pending_drafts: 3,
            },
            refreshing: None,
            running: Vec::new(),
            waiting: HashMap::from([(pr("org/web", 78), Duration::from_secs(100))]),
            profiles: vec!["default".into(), "vuln".into()],
            skipped: HashMap::from([(
                pr("org/api", 490),
                Skip::Title {
                    pattern: "build(deps)*".into(),
                },
            )]),
        }
    }

    fn app(manual_reviews: bool) -> App {
        let mut app = App::new(manual_reviews);
        app.set_overview(overview());
        app.set_logs(
            vec![
                "16:33:00  INFO watching GitHub user=me".into(),
                "16:35:10  INFO refresh{url=https://github.com/org/api/pull/481}: summary".into(),
            ],
            0,
        );
        app
    }

    fn draw(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Presses a key that isn't meant to quit.
    fn press(app: &mut App, code: KeyCode) {
        assert_eq!(app.handle_key(key(code)), Flow::Continue);
    }

    #[test]
    fn renders_at_80_columns() {
        insta::assert_snapshot!(draw(&mut app(false), 80, 34).backend());
    }

    #[test]
    fn manual_reviews_hold_and_help_overlay() {
        let mut app = app(true);
        press(&mut app, KeyCode::Char('?'));
        insta::assert_snapshot!(draw(&mut app, 80, 24).backend());
    }

    #[test]
    fn empty_panes_say_so() {
        let mut app = App::new(false);
        insta::assert_snapshot!(draw(&mut app, 80, 16).backend());
    }

    #[test]
    fn keys_move_focus_and_selection_and_quit() {
        let mut app = app(false);
        assert_eq!(app.focus, Pane::Owed);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        // Stops at the last row.
        assert_eq!(app.lists[Pane::Owed.index()].selected(), Some(4));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.lists[Pane::Owed.index()].selected(), Some(3));

        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.focus, Pane::Log);
        assert!(app.follow_log);
        press(&mut app, KeyCode::Up);
        assert!(!app.follow_log);
        assert_eq!(app.lists[Pane::Log.index()].selected(), Some(0));
        // New lines don't move a log you've scrolled up in.
        app.set_logs(vec!["a".into(), "b".into(), "c".into()], 0);
        assert_eq!(app.lists[Pane::Log.index()].selected(), Some(0));
        press(&mut app, KeyCode::Down);
        // Dropping old lines keeps you on the line you were reading.
        app.set_logs(vec!["b".into(), "c".into(), "d".into()], 1);
        assert_eq!(app.lists[Pane::Log.index()].selected(), Some(0));
        press(&mut app, KeyCode::End);
        assert!(app.follow_log);
        app.set_logs(vec!["b".into(), "c".into(), "d".into(), "e".into()], 1);
        assert_eq!(app.lists[Pane::Log.index()].selected(), Some(3));

        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus, Pane::Owed);
        // A shrinking list keeps the selection in range.
        let mut fewer = overview();
        fewer.owed.clear();
        app.set_overview(fewer);
        assert_eq!(app.lists[Pane::Owed.index()].selected(), None);

        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.overlay, Some(Overlay::Help));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay, None);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Flow::Quit);
    }

    #[test]
    fn r_asks_before_rerunning_a_failed_or_crashed_review() {
        let mut app = app(false);
        // The first row's latest run succeeded.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(app.overlay, None);
        assert!(app.notice.is_some());

        press(&mut app, KeyCode::End);
        assert_eq!(app.notice, None);
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(
            app.overlay,
            Some(Overlay::ConfirmRerun {
                key: pr("org/web", 79),
                why: Why::Failed,
            })
        );
        insta::assert_snapshot!(draw(&mut app, 80, 24).backend());
        // Anything but `y` cancels.
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.overlay, None);

        press(&mut app, KeyCode::Char('r'));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Flow::Request(Request::Rerun(pr("org/web", 79)))
        );
        assert_eq!(app.overlay, None);

        // Only from the pane of reviews you owe.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn skips_outrank_waiting_and_runs() {
        let mut app = App::new(false);
        let mut overview = Overview::default();
        overview.owed.push(OwedReview {
            is_draft: true,
            latest_run: Some(latest("failed", None)),
            ..owed("org/web", 81, "Try things", "erin")
        });
        overview
            .waiting
            .insert(pr("org/web", 81), Duration::from_secs(30));
        overview.skipped.insert(pr("org/web", 81), Skip::Draft);
        app.set_overview(overview);
        let terminal = draw(&mut app, 80, 12);
        let row: String = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol().to_owned())
            .collect();
        assert!(row.starts_with("│—         skipped: draft "), "{row}");
    }

    #[test]
    fn prs_quiet_for_longer_than_the_window_are_left_out() {
        struct FixedClock;
        impl Clock for FixedClock {
            fn now(&self) -> std::time::SystemTime {
                // 2026-09-23T16:33:51Z.
                std::time::UNIX_EPOCH + Duration::from_secs(1_790_181_231)
            }
        }
        let mut store = Store::open_in_memory().unwrap();
        for (number, updated) in [(1, "2026-09-01T00:00:00Z"), (2, "2026-09-20T00:00:00Z")] {
            let snapshot = sanic_core::pr::PrSnapshot {
                key: pr("org/repo", number),
                title: "t".into(),
                body: String::new(),
                url: String::new(),
                author: "alice".into(),
                head_sha: "h".into(),
                base_sha: "b".into(),
                is_draft: false,
                review_requested: true,
                requested_teams: vec![],
                reviews: vec![],
                threads: vec![],
                files: None,
                updated_at: Some(updated.into()),
                review_decision: None,
                merge_state: None,
                checks: None,
            };
            store.record(&snapshot, "me", "p", &[]).unwrap();
        }
        let (window_tx, window) = watch::channel(Some(14));
        let shared = Shared {
            me: "me".into(),
            manual_reviews: false,
            logs: LogLines::default(),
            due: watch::channel(DueTimes::new()).1,
            skips: watch::channel(SkipRules::default()).1,
            window,
            progress: watch::channel(None).1,
            clock: Arc::new(FixedClock),
            requests: mpsc::unbounded_channel().0,
            config_path: PathBuf::new(),
            data_dir: PathBuf::new(),
            runtime: tokio::runtime::Runtime::new().unwrap().handle().clone(),
            chatting: Arc::default(),
            dashboard: String::new(),
            opener: Arc::new(Opened::default()),
        };
        let owed = |shared: &Shared| -> Vec<u32> {
            let overview = Overview::load(&store, shared).unwrap();
            overview.owed.iter().map(|pr| pr.key.number).collect()
        };
        assert_eq!(owed(&shared), [2]);
        window_tx.send_replace(None);
        assert_eq!(owed(&shared), [1, 2]);
    }

    #[test]
    fn the_status_bar_counts_a_refresh_batch() {
        let mut app = app(false);
        let mut overview = overview();
        overview.refreshing = Some(Progress {
            done: 37,
            total: 264,
        });
        app.set_overview(overview);
        let terminal = draw(&mut app, 80, 24);
        let row: String = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 23)].symbol().to_owned())
            .collect();
        assert!(row.starts_with(" refreshing 37/264 "), "{row}");
    }

    #[test]
    fn queued_reviews_read_as_held_under_manual_reviews() {
        let queued = |manual: bool| {
            let mut app = App::new(manual);
            let mut overview = Overview::default();
            overview.activity.push(Activity {
                at: "2026-09-23T16:36:00.000Z".into(),
                key: pr("org/web", 79),
                kind: ActivityKind::RunQueued,
            });
            app.set_overview(overview);
            draw(&mut app, 80, 16).backend().to_string()
        };
        assert!(queued(true).contains("review held "), "{}", queued(true));
        assert!(
            queued(false).contains("review queued "),
            "{}",
            queued(false)
        );

        // One that has since started isn't held.
        let mut app = App::new(true);
        let mut overview = Overview::default();
        for (at, kind) in [
            ("2026-09-23T16:37:00.000Z", ActivityKind::RunStarted),
            ("2026-09-23T16:36:00.000Z", ActivityKind::RunQueued),
        ] {
            overview.activity.push(Activity {
                at: at.into(),
                key: pr("org/web", 79),
                kind,
            });
        }
        app.set_overview(overview);
        let screen = draw(&mut app, 80, 24).backend().to_string();
        assert!(screen.contains("review queued "), "{screen}");
        assert!(!screen.contains("review held "), "{screen}");
    }

    #[test]
    fn a_reviewed_head_says_who_and_r_still_reviews_it() {
        let mut app = App::new(false);
        let mut overview = Overview::default();
        overview
            .owed
            .push(owed("org/web", 82, "Tidy the logs", "erin"));
        let reviewed = Skip::Reviewed {
            by: sanic_core::skip::ReviewedBy::new("me", &["alice".into(), "me".into()]).unwrap(),
            head: "h1".into(),
        };
        overview.skipped.insert(pr("org/web", 82), reviewed.clone());
        app.set_overview(overview);
        let screen = draw(&mut app, 80, 16).backend().to_string();
        assert!(screen.contains("reviewed by you, alice"), "{screen}");

        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(
            app.overlay,
            Some(Overlay::ConfirmRerun {
                key: pr("org/web", 82),
                why: Why::Skipped(reviewed),
            })
        );
        let screen = draw(&mut app, 80, 16).backend().to_string();
        assert!(
            screen.contains("Already reviewed by you, alice. Review"),
            "{screen}"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Flow::Request(Request::Rerun(pr("org/web", 82)))
        );
    }

    #[test]
    fn countdowns_are_minutes_and_seconds_or_hours() {
        assert_eq!(countdown(Duration::from_secs(100)), "1:40");
        assert_eq!(countdown(Duration::from_secs(5)), "0:05");
        assert_eq!(countdown(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn x_archives_and_capital_x_shows_archived_prs() {
        let mut app = app(false);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.overview.mine.len(), 2, "the archived one is hidden");
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('x'))),
            Flow::Request(Request::Archive {
                key: pr("org/api", 470),
                archived: true,
            })
        );
        assert_eq!(app.overview.mine.len(), 1);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .starts_with("archived https://")
        );

        press(&mut app, KeyCode::Char('X'));
        assert_eq!(app.overview.mine.len(), 3);
        insta::assert_snapshot!(draw(&mut app, 80, 24).backend());
        press(&mut app, KeyCode::End);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('x'))),
            Flow::Request(Request::Archive {
                key: pr("org/web", 60),
                archived: false,
            })
        );

        // Nothing to archive in the other panes.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Flow::Continue);
        assert_eq!(app.notice.as_deref(), Some("x archives the selected PR"));
    }

    #[test]
    fn r_reviews_a_skipped_pr_anyway() {
        let mut app = app(false);
        // The second row is skipped by title.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(
            app.overlay,
            Some(Overlay::ConfirmRerun {
                key: pr("org/api", 490),
                why: Why::Skipped(Skip::Title {
                    pattern: "build(deps)*".into(),
                }),
            })
        );
        insta::assert_snapshot!(draw(&mut app, 80, 24).backend());
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Flow::Request(Request::Rerun(pr("org/api", 490)))
        );
    }

    #[test]
    fn i_edits_a_title_glob_and_picks_where_it_goes() {
        let mut app = app(false);
        // The skipped dependabot PR.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('i'));
        assert!(matches!(app.overlay, Some(Overlay::Ignore(_))));
        // Edit "build(deps): bump tokio" down to "build(deps)*"; typing
        // `q` or `j` goes into the pattern rather than quitting or moving.
        for _ in 0.."): bump tokio".len() {
            press(&mut app, KeyCode::Backspace);
        }
        press(&mut app, KeyCode::Char(')'));
        press(&mut app, KeyCode::Char('*'));
        insta::assert_snapshot!(draw(&mut app, 80, 24).backend());

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('j'));
        insta::assert_snapshot!("ignore_target", draw(&mut app, 80, 24).backend());
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Flow::AddSkipTitle {
                pattern: "build(deps)*".into(),
                profile: Some("default".into()),
            }
        );
        assert_eq!(app.overlay, None);

        // An invalid glob can't be saved, and Esc cancels.
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            Flow::Continue
        );
        press(&mut app, KeyCode::Char('['));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Some(Overlay::Ignore(_))));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn r_starts_a_held_review_under_manual_reviews() {
        let mut manual = app(true);
        // The queued "Fix login flake".
        for _ in 0..3 {
            press(&mut manual, KeyCode::Char('j'));
        }
        press(&mut manual, KeyCode::Char('r'));
        assert_eq!(
            manual.overlay,
            Some(Overlay::ConfirmRerun {
                key: pr("org/web", 77),
                why: Why::Held,
            })
        );
        insta::assert_snapshot!(draw(&mut manual, 80, 24).backend());
        assert_eq!(
            manual.handle_key(key(KeyCode::Char('y'))),
            Flow::Request(Request::Rerun(pr("org/web", 77)))
        );

        // Without the flag, a queued review runs on its own.
        let mut auto = app(false);
        for _ in 0..3 {
            press(&mut auto, KeyCode::Char('j'));
        }
        press(&mut auto, KeyCode::Char('r'));
        assert_eq!(auto.overlay, None);
    }

    #[test]
    fn quitting_while_reviews_run_asks_first() {
        let mut app = app(false);
        let mut busy = overview();
        busy.running = vec![pr("org/api", 481)];
        app.set_overview(busy);

        press(&mut app, KeyCode::Char('q'));
        assert_eq!(
            app.overlay,
            Some(Overlay::ConfirmQuit(vec![pr("org/api", 481)]))
        );
        insta::assert_snapshot!(draw(&mut app, 80, 24).backend());
        // Anything else stays.
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.overlay, None);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Flow::Quit);
        // Ctrl-C asks too, and a second one doesn't.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(ctrl_c), Flow::Continue);
        assert_eq!(app.handle_key(ctrl_c), Flow::Quit);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Flow::Quit);

        // So does Ctrl-C from another overlay.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('i'));
        assert!(matches!(app.overlay, Some(Overlay::Ignore(_))));
        assert_eq!(app.handle_key(ctrl_c), Flow::Continue);
        assert!(matches!(app.overlay, Some(Overlay::ConfirmQuit(_))));
        press(&mut app, KeyCode::Char('n'));

        // With nothing running, it just quits.
        app.set_overview(overview());
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Flow::Quit);
    }

    #[test]
    fn c_hands_a_pr_with_a_session_to_a_chat() {
        let mut app = App::new(false);
        let mut overview = Overview::default();
        overview.owed.push(OwedReview {
            chat_run: Some(4),
            ..owed("org/web", 82, "Tidy the logs", "erin")
        });
        overview.owed.push(owed("org/web", 83, "Not yet", "erin"));
        app.set_overview(overview);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'))),
            Flow::Chat(pr("org/web", 82))
        );
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('c'));
        assert!(app.notice.as_deref().unwrap().starts_with("no review of"));
    }

    /// Records what it's asked to open, or fails to.
    #[derive(Default)]
    struct Opened {
        urls: std::sync::Mutex<Vec<String>>,
        fail: bool,
    }

    impl Opener for Opened {
        fn open(&self, url: &str) -> Result<()> {
            self.urls.lock().unwrap().push(url.to_owned());
            if self.fail {
                Err(eyre!("no browser"))
            } else {
                Ok(())
            }
        }
    }

    const INDEX: &str = "http://127.0.0.1:4000/";

    /// Presses `o`, and returns what the launcher opened and any failure
    /// it handed back.
    fn press_o(app: &mut App, opener: &Arc<Opened>) -> (Vec<String>, Option<String>) {
        let Flow::Open(page) = app.handle_key(key(KeyCode::Char('o'))) else {
            panic!("`o` opens a page");
        };
        let launcher = Launcher::new(Arc::clone(opener) as Arc<dyn Opener>);
        let notice = launcher.open(&page, INDEX);
        assert!(notice.starts_with("opening http://"), "{notice}");
        // The browser thread's `failures` sender outlives the launcher's
        // own only while it runs.
        drop(launcher.failures_tx);
        let failure = launcher.failures.recv().ok();
        (std::mem::take(&mut *opener.urls.lock().unwrap()), failure)
    }

    #[test]
    fn o_opens_the_selected_pr_in_the_dashboard() {
        let opener = Arc::new(Opened::default());
        let mut app = app(false);
        press(&mut app, KeyCode::End);
        assert_eq!(
            press_o(&mut app, &opener),
            (vec![format!("{INDEX}pr/org/web/79")], None)
        );
        // Your PRs, and the PR an activity row is about, too.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            press_o(&mut app, &opener).0,
            [format!("{INDEX}pr/org/api/470")]
        );
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            press_o(&mut app, &opener).0,
            [format!("{INDEX}pr/org/web/79")]
        );
    }

    #[test]
    fn o_opens_the_index_with_no_pr_selected() {
        let opener = Arc::new(Opened::default());
        let mut empty = App::new(false);
        assert_eq!(press_o(&mut empty, &opener), (vec![INDEX.to_owned()], None));
        // The log's lines aren't PRs.
        let mut app = app(false);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(press_o(&mut app, &opener).0, [INDEX]);
    }

    #[test]
    fn a_browser_that_fails_to_open_says_why() {
        let opener = Arc::new(Opened {
            fail: true,
            ..Opened::default()
        });
        let (_, failure) = press_o(&mut App::new(false), &opener);
        assert_eq!(
            failure.as_deref(),
            Some("couldn't open http://127.0.0.1:4000/: no browser")
        );
    }

    /// The screen's rows as text.
    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    /// The row a pane's title is on.
    fn title_row(rows: &[String], title: &str) -> usize {
        rows.iter()
            .position(|row| row.starts_with(&format!("┌ {title}")))
            .unwrap_or_else(|| panic!("no {title} pane in\n{}", rows.join("\n")))
    }

    /// Presses a key that changes the layout, and returns the layout
    /// `serve` is asked to save.
    fn resize(app: &mut App, code: KeyCode) -> Sizes {
        match app.handle_key(key(code)) {
            Flow::Request(Request::SaveLayout(sizes)) => sizes,
            other => panic!("{code} didn't save the layout: {other:?}"),
        }
    }

    #[test]
    fn lists_fit_their_content_and_the_log_fills_the_rest() {
        let mut app = app(false);
        let rows = rows(&draw(&mut app, 80, 40));
        // Five reviews, one with an error line; two PRs; four events.
        let owed = title_row(&rows, "Reviews you owe");
        let mine = title_row(&rows, "Your PRs");
        let activity = title_row(&rows, "Activity");
        let log = title_row(&rows, "Log");
        assert_eq!(
            (owed, mine - owed, activity - mine, log - activity),
            (0, 8, 4, 6)
        );
        // The log runs down to the status bar.
        assert!(rows[38].starts_with('└'), "{}", rows.join("\n"));
        assert!(rows[39].starts_with(" ? help"));
    }

    #[test]
    fn long_lists_scroll_and_leave_activity_and_the_log_a_row() {
        let mut app = app(false);
        let mut many = overview();
        many.owed = (0..60)
            .map(|n| owed("org/api", 1000 + n, "Something", "alice"))
            .collect();
        many.mine = (0..20).map(|n| my("org/web", 2000 + n, "Mine")).collect();
        app.set_overview(many);
        press(&mut app, KeyCode::End);
        let terminal = draw(&mut app, 80, 30);
        let rows = rows(&terminal);
        let mine = title_row(&rows, "Your PRs");
        let activity = title_row(&rows, "Activity");
        let log = title_row(&rows, "Log");
        // The lists split what activity's and the log's single rows leave
        // about 3:1, as their content does.
        assert_eq!((mine, activity - mine, log - activity), (16, 7, 3));
        // The last review you owe, scrolled to.
        assert!(rows[mine - 2].contains("pull/1059 "), "{}", rows.join("\n"));
    }

    #[test]
    fn a_pane_that_grows_scrolls_back_to_fill_itself() {
        let mut app = app(false);
        app.set_logs((0..30).map(|i| format!("line {i}")).collect(), 0);
        // Line 25 selected in the short log pane, which shows the last few.
        press(&mut app, KeyCode::Char('l'));
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('k'));
        }
        let short = rows(&draw(&mut app, 80, 30));
        assert!(!short.iter().any(|r| r.contains("line 3 ")));
        let scrolled = app.lists[Pane::Log.index()].offset();

        // Full screen, it scrolls back so no row is left blank.
        resize(&mut app, KeyCode::Char('z'));
        let full = rows(&draw(&mut app, 80, 30));
        let line = |y: usize| full[y].trim_end_matches(['│', ' ']).to_owned();
        assert_eq!((line(1), line(26)), ("│line 4".into(), "│line 29".into()));
        assert!(app.lists[Pane::Log.index()].offset() < scrolled);
        assert_eq!(app.lists[Pane::Log.index()].selected(), Some(25));
    }

    #[test]
    fn z_cycles_fit_full_screen_collapsed_and_capital_z_resets() {
        let mut app = app(false);
        press(&mut app, KeyCode::Tab);
        let sizes = resize(&mut app, KeyCode::Char('z'));
        assert_eq!(sizes.full(), Some(Pane::Mine));
        insta::assert_snapshot!("full_screen", draw(&mut app, 80, 12).backend());

        let sizes = resize(&mut app, KeyCode::Char('z'));
        assert_eq!(sizes.get(Pane::Mine), Size::Collapsed);
        insta::assert_snapshot!("collapsed", draw(&mut app, 80, 24).backend());
        // Its rows aren't there to act on.
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.notice.as_deref(),
            Some("Your PRs is collapsed · z or p shows it")
        );
        assert_eq!(app.lists[Pane::Mine.index()].selected(), None);
        // Tab passes it by.
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.focus, Pane::Owed);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus, Pane::Activity);

        // p fits it again; from there z goes round the cycle.
        assert_eq!(resize(&mut app, KeyCode::Char('p')), Sizes::default());
        assert_eq!(
            resize(&mut app, KeyCode::Char('z')).full(),
            Some(Pane::Mine)
        );
        let sizes = resize(&mut app, KeyCode::Char('z'));
        assert_eq!(sizes.get(Pane::Mine), Size::Collapsed);
        assert_eq!(resize(&mut app, KeyCode::Char('z')), Sizes::default());

        // Z puts every pane back to fit.
        resize(&mut app, KeyCode::Char('z'));
        // Taking the full screen over, then collapsing.
        resize(&mut app, KeyCode::Char('w'));
        resize(&mut app, KeyCode::Char('z'));
        press(&mut app, KeyCode::Char('l'));
        resize(&mut app, KeyCode::Char('z'));
        assert_eq!(app.sizes.get(Pane::Owed), Size::Collapsed);
        assert_eq!(app.sizes.full(), Some(Pane::Log));
        assert_eq!(resize(&mut app, KeyCode::Char('Z')), Sizes::default());
        // Nothing to reset, nothing to save.
        press(&mut app, KeyCode::Char('Z'));
    }

    #[test]
    fn jump_keys_focus_and_show_a_pane() {
        let mut app = app(false);
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.focus, Pane::Log);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.lists[Pane::Log.index()].selected(), Some(0));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.focus, Pane::Activity);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.lists[Pane::Activity.index()].selected(), Some(3));
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.focus, Pane::Mine);
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.focus, Pane::Owed);

        // A collapsed pane fits again.
        press(&mut app, KeyCode::Char('l'));
        resize(&mut app, KeyCode::Char('z'));
        resize(&mut app, KeyCode::Char('z'));
        press(&mut app, KeyCode::Char('w'));
        let sizes = resize(&mut app, KeyCode::Char('l'));
        assert_eq!((app.focus, sizes), (Pane::Log, Sizes::default()));

        // One behind a full screen takes it over; so does Tab's.
        resize(&mut app, KeyCode::Char('z'));
        let sizes = resize(&mut app, KeyCode::Char('w'));
        assert_eq!(sizes.full(), Some(Pane::Owed));
        assert_eq!(sizes.get(Pane::Log), Size::Fit);
        let sizes = resize(&mut app, KeyCode::Tab);
        assert_eq!((app.focus, sizes.full()), (Pane::Mine, Some(Pane::Mine)));
    }

    #[test]
    fn titles_and_tabs_highlight_their_jump_letters() {
        let mut app = app(false);
        resize(&mut app, KeyCode::Char('z'));
        let terminal = draw(&mut app, 80, 12);
        let buffer = terminal.backend().buffer();
        let underlined = |y: u16| -> String {
            (0..80)
                .filter(|&x| buffer[(x, y)].modifier.contains(Modifier::UNDERLINED))
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        // "Revie[w]s you owe" in the full-screen title; the others' tabs.
        assert_eq!(underlined(0), "w");
        assert_eq!(underlined(10), "PAL");
        let tabs = rows(&terminal).remove(10);
        assert!(
            tabs.starts_with(" Your PRs (2 · 1 archived) │ Activity │ Log "),
            "{tabs}"
        );
    }

    #[test]
    fn the_layout_is_saved_by_serve_and_loaded_at_start() {
        let store = Store::open_in_memory().unwrap();
        let mut app = app(false);
        let sizes = resize(&mut app, KeyCode::Char('z'));
        save_layout(&store, sizes).unwrap();
        assert_eq!(load_layout(&store), sizes);
    }

    #[test]
    fn a_loaded_layout_focuses_a_shown_pane() {
        let mut app = app(false);
        let mut sizes = Sizes::default();
        sizes.set(Pane::Log, Size::Full);
        app.set_sizes(sizes);
        assert_eq!(app.focus, Pane::Log);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.notice, None);
    }

    #[test]
    fn ctrl_c_quits_from_a_collapsed_pane() {
        let mut app = App::new(false);
        resize(&mut app, KeyCode::Char('z'));
        resize(&mut app, KeyCode::Char('z'));
        assert!(!app.sizes.shown(app.focus));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut app = App::new(false);
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(app.handle_key(release), Flow::Continue);
    }
}
