//! `serve --ui tui`: a read-only summary of tracked PRs, recent activity and
//! the log.
//!
//! It runs on its own thread with its own read-only store connection, and
//! rereads the store on an interval. Its only actions are asking `serve`
//! to rerun a review and to archive or unarchive a PR; nothing in it edits
//! drafts.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
    skip::{Skip, SkipRules},
};
use sanic_store::{Activity, ActivityKind, MyPr, OwedReview, ReviewState, RunCounts, Store};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;

use self::ignore::{IgnoreEditor, Outcome};
use crate::{config_edit, logging::LogLines, schedule::DueTimes};

mod ignore;

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
                let mut app = App::new(shared.no_reviews);
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
    /// `serve --no-reviews`.
    pub no_reviews: bool,
    pub logs: LogLines,
    /// When the scheduler will queue each debounced review.
    pub due: watch::Receiver<DueTimes>,
    /// Which PRs aren't reviewed automatically; follows config reloads.
    pub skips: watch::Receiver<SkipRules>,
    /// `poll.updated_within_days`: PRs quiet for longer are left out.
    pub window: watch::Receiver<Option<u32>>,
    pub clock: Arc<dyn Clock>,
    /// What you ask `serve` to do.
    pub requests: mpsc::UnboundedSender<Request>,
    /// The ignore editor adds `skip_titles` here; `serve` reloads it.
    pub config_path: PathBuf,
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
    while !stop.load(Ordering::Relaxed) {
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
                Some((
                    pr.key.clone(),
                    skips.check(&pr.profile, &pr.title, pr.is_draft)?,
                ))
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
}

pub struct App {
    /// Everything last read from the store.
    loaded: Overview,
    /// What the panes show: `loaded` without archived PRs, unless shown.
    overview: Overview,
    logs: Vec<String>,
    /// Log lines dropped from the front of `logs` so far.
    logs_dropped: u64,
    /// `serve --no-reviews`: queued reviews are held, not waiting their turn.
    no_reviews: bool,
    focus: Pane,
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
    /// Waiting for a yes before asking for a review of this PR. `skipped`
    /// says why it wouldn't be reviewed automatically, if it wouldn't.
    ConfirmRerun {
        key: PrKey,
        skipped: Option<&'static str>,
    },
}

/// What the loop does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Flow {
    Continue,
    Quit,
    /// Something for `serve` to do.
    Request(Request),
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
}

impl App {
    #[must_use]
    pub fn new(no_reviews: bool) -> Self {
        Self {
            loaded: Overview::default(),
            overview: Overview::default(),
            logs: Vec::new(),
            logs_dropped: 0,
            no_reviews,
            focus: Pane::Owed,
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
                Outcome::Quit => Flow::Quit,
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
        if let Some(Overlay::ConfirmRerun { key: pr, .. }) = &self.overlay {
            let pr = pr.clone();
            self.overlay = None;
            return match key.code {
                KeyCode::Char('c') if ctrl => Flow::Quit,
                KeyCode::Char('y') => Flow::Request(Request::Rerun(pr)),
                // Anything else is a no, so a stray key can't spend tokens.
                _ => Flow::Continue,
            };
        }
        match key.code {
            KeyCode::Char('q') => return Flow::Quit,
            KeyCode::Char('c') if ctrl => return Flow::Quit,
            KeyCode::Char('r') => self.ask_rerun(),
            KeyCode::Char('a') => return self.toggle_archive(),
            KeyCode::Char('i') => self.open_ignore(),
            KeyCode::Char('A') => {
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
            KeyCode::Tab => self.focus = self.focus.step(true),
            KeyCode::BackTab => self.focus = self.focus.step(false),
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_to(0),
            KeyCode::Char('G') | KeyCode::End => self.move_to(usize::MAX),
            _ => {}
        }
        Flow::Continue
    }

    /// Asks to confirm a review of the selected PR you owe, if its latest
    /// run failed or crashed, or it's skipped or archived.
    fn ask_rerun(&mut self) {
        let selected = self.lists[Pane::Owed.index()]
            .selected()
            .and_then(|i| self.overview.owed.get(i))
            .filter(|_| self.focus == Pane::Owed);
        let Some(pr) = selected else {
            self.notice = Some(RERUN_HINT.into());
            return;
        };
        let skipped = if pr.archived {
            Some(Skip::Archived.label())
        } else {
            self.overview.skipped.get(&pr.key).map(Skip::label)
        };
        let failed = pr
            .latest_run
            .as_ref()
            .is_some_and(|run| matches!(run.status.as_str(), "failed" | "crashed"));
        if failed || skipped.is_some() {
            self.overlay = Some(Overlay::ConfirmRerun {
                key: pr.key.clone(),
                skipped,
            });
        } else {
            self.notice = Some(RERUN_HINT.into());
        }
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
            self.notice = Some("a archives the selected PR".into());
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
            format!("archived {} · A shows archived PRs", key.url())
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
    let [owed, mine, activity, log, status] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let App {
        overview,
        logs,
        no_reviews,
        focus,
        lists,
        follow_log,
        show_archived,
        overlay,
        notice,
        loaded,
        ..
    } = app;
    // How many archived PRs a pane is hiding, for its title.
    let title = |name: &str, shown: usize, archived: usize| {
        if *show_archived || archived == 0 {
            format!("{name} ({shown})")
        } else {
            format!("{name} ({shown} · {archived} archived)")
        }
    };
    let [owed_state, mine_state, activity_state, log_state] = lists;

    let rows: Vec<_> = overview
        .owed
        .iter()
        .map(|pr| owed_row(pr, overview, *no_reviews))
        .collect();
    let archived = loaded.owed.iter().filter(|pr| pr.archived).count();
    let pane = Pane::Owed.frame(*focus, &title("Reviews you owe", rows.len(), archived));
    pane.render(frame, owed, rows, owed_state, "No reviews requested.");

    let rows: Vec<_> = overview.mine.iter().map(my_row).collect();
    let archived = loaded.mine.iter().filter(|pr| pr.archived).count();
    let pane = Pane::Mine.frame(*focus, &title("Your PRs", rows.len(), archived));
    pane.render(frame, mine, rows, mine_state, "No open PRs of yours.");

    let rows: Vec<_> = overview.activity.iter().map(activity_row).collect();
    let pane = Pane::Activity.frame(*focus, "Activity");
    pane.render(frame, activity, rows, activity_state, "Nothing yet.");

    let rows: Vec<_> = logs.iter().map(|l| ListItem::new(l.as_str())).collect();
    let mut pane = Pane::Log.frame(*focus, "Log");
    // A following log keeps its newest line selected to stay scrolled to
    // the bottom; that isn't a selection worth highlighting.
    pane.highlight &= !*follow_log;
    pane.render(frame, log, rows, log_state, "");

    render_status(
        frame,
        status,
        &overview.counts,
        *no_reviews,
        notice.as_deref(),
    );
    match overlay {
        Some(Overlay::Help) => render_help(frame),
        Some(Overlay::Ignore(editor)) => editor.render(frame, &loaded.owed, &overview.profiles),
        Some(Overlay::ConfirmRerun { key, skipped }) => render_confirm(frame, key, *skipped),
        None => {}
    }
}

/// A pane's border and whether its selection is highlighted.
struct PaneFrame<'a> {
    block: Block<'a>,
    highlight: bool,
}

impl Pane {
    fn frame(self, focus: Pane, title: &str) -> PaneFrame<'static> {
        let focused = focus == self;
        let border = if focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        PaneFrame {
            block: Block::bordered()
                .border_style(border)
                .title(Span::styled(format!(" {title} "), border)),
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
        let highlight = if self.highlight {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        let list = List::new(rows).block(self.block).highlight_style(highlight);
        frame.render_stateful_widget(list, area, state);
    }
}

const RERUN_HINT: &str = "r reviews a failed, crashed, skipped or archived PR you owe";

/// Width of the status column in both PR panes, including the space that
/// keeps a label as long as the column off what follows it.
const STATUS_WIDTH: usize = 15;

fn owed_row<'a>(pr: &'a OwedReview, overview: &Overview, no_reviews: bool) -> ListItem<'a> {
    let latest = pr.latest_run.as_ref();
    let waiting = overview.waiting.get(&pr.key).copied();
    // A skip says why nothing will happen; a review waiting out the quiet
    // period is newer news than the last run.
    let (label, color) = match (waiting, latest.map(|run| run.status.as_str())) {
        _ if pr.archived => ("archived".into(), Color::DarkGray),
        _ if let Some(skip) = overview.skipped.get(&pr.key) => {
            (format!("skipped: {}", skip.label()), Color::DarkGray)
        }
        (Some(left), _) => (format!("waiting {}", countdown(left)), Color::DarkGray),
        (None, None) => ("waiting".into(), Color::DarkGray),
        (None, Some("queued")) if no_reviews => ("held".into(), Color::Yellow),
        (None, Some("queued")) => ("queued".into(), Color::Yellow),
        (None, Some("running")) => ("running".into(), Color::Cyan),
        (None, Some("succeeded")) => ("drafted".into(), Color::Green),
        (None, Some(status @ ("failed" | "crashed"))) => (status.into(), Color::Red),
        (None, Some(other)) => (other.into(), Color::DarkGray),
    };
    let mut lines = vec![Line::from(vec![
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
            Span::raw(" ".repeat(STATUS_WIDTH)),
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
    let (label, color) = match pr.review_state {
        _ if pr.archived => ("archived", Color::DarkGray),
        ReviewState::Approved => ("approved", Color::Green),
        ReviewState::ChangesRequested => ("changes", Color::Red),
        ReviewState::Waiting => ("waiting", Color::DarkGray),
    };
    let mut spans = vec![
        Span::styled(
            format!("{label:<w$} ", w = STATUS_WIDTH - 1),
            Style::new().fg(color),
        ),
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
fn drafts(n: u32) -> Span<'static> {
    let text = match n {
        0 => String::new(),
        1 => "1 draft".into(),
        n => format!("{n} drafts"),
    };
    Span::styled(format!("{text:<8} "), Style::new().fg(Color::Magenta))
}

fn activity_row(activity: &Activity) -> ListItem<'_> {
    let what = match &activity.kind {
        ActivityKind::Trigger(kind) => kind.replace('_', " "),
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
    counts: &RunCounts,
    no_reviews: bool,
    notice: Option<&str>,
) {
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
    if no_reviews {
        right.insert_str(0, "reviews held · ");
    }
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(u16::try_from(right.chars().count()).unwrap_or(u16::MAX)),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(" ? help · q quit".dim()), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
}

const HELP: &[(&str, &str)] = &[
    ("q, Ctrl-C", "quit serve"),
    ("Tab, Shift-Tab", "next, previous pane"),
    ("j/k, Down/Up", "move in the pane"),
    ("g/G, Home/End", "first, last row"),
    ("r", "review a failed or skipped PR again"),
    ("a", "archive or unarchive the selected PR"),
    ("A", "show or hide archived PRs"),
    ("i", "skip PRs with titles like the selected one"),
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

fn render_confirm(frame: &mut Frame<'_>, pr: &PrKey, skipped: Option<&str>) {
    let head = match skipped {
        Some(reason) => format!(" Review this {reason}-skipped PR anyway,"),
        None => " Rerun the review of".into(),
    };
    let lines = vec![
        Line::raw(head),
        Line::raw(format!(" {}", pr.url())),
        Line::raw(" at its current head? This spends tokens."),
        Line::raw(""),
        Line::from(vec![
            Span::raw(" y").bold(),
            Span::raw(" rerun · any other key cancels"),
        ]),
    ];
    render_popup(frame, " Rerun ", lines);
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
            latest_run: None,
            pending_drafts: 0,
        }
    }

    fn overview() -> Overview {
        Overview {
            owed: vec![
                OwedReview {
                    latest_run: Some(latest("succeeded", None)),
                    pending_drafts: 3,
                    ..owed("org/api", 481, "Retry webhook deliveries", "alice")
                },
                owed("org/api", 490, "build(deps): bump tokio", "dependabot"),
                OwedReview {
                    latest_run: Some(latest("queued", None)),
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
                    key: pr("org/api", 470),
                    title: "Speed up search".into(),
                    is_draft: false,
                    archived: false,
                    review_state: ReviewState::ChangesRequested,
                    pending_drafts: 0,
                },
                MyPr {
                    key: pr("org/web", 80),
                    title: "New settings page".into(),
                    is_draft: true,
                    archived: false,
                    review_state: ReviewState::Waiting,
                    pending_drafts: 0,
                },
                MyPr {
                    key: pr("org/web", 60),
                    title: "Abandoned experiment".into(),
                    is_draft: false,
                    archived: true,
                    review_state: ReviewState::Waiting,
                    pending_drafts: 0,
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

    fn app(no_reviews: bool) -> App {
        let mut app = App::new(no_reviews);
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
    fn held_reviews_and_help_overlay() {
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
                skipped: None,
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
        assert!(row.starts_with("│skipped: draft "), "{row}");
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
            };
            store.record(&snapshot, "p", &[]).unwrap();
        }
        let (window_tx, window) = watch::channel(Some(14));
        let shared = Shared {
            me: "me".into(),
            no_reviews: false,
            logs: LogLines::default(),
            due: watch::channel(DueTimes::new()).1,
            skips: watch::channel(SkipRules::default()).1,
            window,
            clock: Arc::new(FixedClock),
            requests: mpsc::unbounded_channel().0,
            config_path: PathBuf::new(),
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
    fn countdowns_are_minutes_and_seconds_or_hours() {
        assert_eq!(countdown(Duration::from_secs(100)), "1:40");
        assert_eq!(countdown(Duration::from_secs(5)), "0:05");
        assert_eq!(countdown(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn a_archives_and_capital_a_shows_archived_prs() {
        let mut app = app(false);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.overview.mine.len(), 2, "the archived one is hidden");
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
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

        press(&mut app, KeyCode::Char('A'));
        assert_eq!(app.overview.mine.len(), 3);
        insta::assert_snapshot!(draw(&mut app, 80, 24).backend());
        press(&mut app, KeyCode::End);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            Flow::Request(Request::Archive {
                key: pr("org/web", 60),
                archived: false,
            })
        );

        // Nothing to archive in the other panes.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.handle_key(key(KeyCode::Char('a'))), Flow::Continue);
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
                skipped: Some("title"),
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
    fn key_releases_are_ignored() {
        let mut app = App::new(false);
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(app.handle_key(release), Flow::Continue);
    }
}
