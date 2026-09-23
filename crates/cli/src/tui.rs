//! `serve --ui tui`: a read-only summary of tracked PRs, recent activity and
//! the log.
//!
//! It runs on its own thread with its own read-only store connection, and
//! rereads the store on an interval, so nothing else in `serve` knows it
//! exists. Nothing in it edits anything.

use std::{
    path::Path,
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
use sanic_store::{Activity, ActivityKind, MyPr, OwedReview, ReviewState, RunCounts, Store};
use tokio::sync::oneshot;
use tracing::warn;

use crate::logging::LogLines;

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
    pub fn start(db: &Path, me: String, no_reviews: bool, logs: LogLines) -> Result<Self> {
        let store = Store::open_read_only(db)?;
        let mut terminal = init_terminal().wrap_err("starting the terminal UI")?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, done) = oneshot::channel();
        let thread = std::thread::Builder::new().name("tui".into()).spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut app = App::new(no_reviews);
                let result = run(&mut terminal, &mut app, &store, &me, &logs, &stop);
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

fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    store: &Store,
    me: &str,
    logs: &LogLines,
    stop: &AtomicBool,
) -> Result<()> {
    let mut loaded_at: Option<Instant> = None;
    let mut load_failed = false;
    while !stop.load(Ordering::Relaxed) {
        if loaded_at.is_none_or(|at| at.elapsed() >= REFRESH) {
            loaded_at = Some(Instant::now());
            // A failed read keeps the last data; warn once per failure spell
            // rather than every refresh.
            match Overview::load(store, me) {
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
        let (lines, dropped) = logs.snapshot();
        app.set_logs(lines, dropped);
        terminal
            .draw(|frame| render(frame, app))
            .wrap_err("drawing the terminal UI")?;
        if event::poll(TICK).wrap_err("reading terminal input")?
            && let Event::Key(key) = event::read().wrap_err("reading terminal input")?
            && app.handle_key(key) == Flow::Quit
        {
            break;
        }
    }
    Ok(())
}

/// Everything the panes show from the store.
#[derive(Debug, Default)]
pub struct Overview {
    pub owed: Vec<OwedReview>,
    pub mine: Vec<MyPr>,
    /// Newest first.
    pub activity: Vec<Activity>,
    pub counts: RunCounts,
}

impl Overview {
    fn load(store: &Store, me: &str) -> Result<Self> {
        Ok(Self {
            owed: store.owed_reviews(me)?,
            mine: store.my_prs(me)?,
            activity: store.recent_activity(ACTIVITY_ROWS)?,
            counts: store.run_counts()?,
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
    help: bool,
}

/// What the loop does after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Flow {
    Continue,
    Quit,
}

impl App {
    #[must_use]
    pub fn new(no_reviews: bool) -> Self {
        Self {
            overview: Overview::default(),
            logs: Vec::new(),
            logs_dropped: 0,
            no_reviews,
            focus: Pane::Owed,
            lists: Default::default(),
            follow_log: true,
            help: false,
        }
    }

    pub fn set_overview(&mut self, overview: Overview) {
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
        match key.code {
            KeyCode::Char('q') => return Flow::Quit,
            KeyCode::Char('c') if ctrl => return Flow::Quit,
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Esc => self.help = false,
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
        help,
        ..
    } = app;
    let [owed_state, mine_state, activity_state, log_state] = lists;

    let rows: Vec<_> = overview
        .owed
        .iter()
        .map(|pr| owed_row(pr, *no_reviews))
        .collect();
    let pane = Pane::Owed.frame(*focus, &format!("Reviews you owe ({})", rows.len()));
    pane.render(frame, owed, rows, owed_state, "No reviews requested.");

    let rows: Vec<_> = overview.mine.iter().map(my_row).collect();
    let pane = Pane::Mine.frame(*focus, &format!("Your PRs ({})", rows.len()));
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

    render_status(frame, status, &overview.counts, *no_reviews);
    if *help {
        render_help(frame);
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

fn owed_row(pr: &OwedReview, no_reviews: bool) -> ListItem<'_> {
    let latest = pr.latest_run.as_ref();
    let (label, color) = match latest.map(|run| run.status.as_str()) {
        None => ("no run", Color::DarkGray),
        Some("queued") if no_reviews => ("held", Color::Yellow),
        Some("queued") => ("queued", Color::Yellow),
        Some("running") => ("running", Color::Cyan),
        Some("succeeded") => ("drafted", Color::Green),
        Some("failed") => ("failed", Color::Red),
        Some("crashed") => ("crashed", Color::Red),
        Some(other) => (other, Color::DarkGray),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::new().fg(color)),
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
            Span::raw(" ".repeat(10)),
            Span::styled(first_line(error), Style::new().fg(Color::Red)),
        ]));
    }
    ListItem::new(lines)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn my_row(pr: &MyPr) -> ListItem<'_> {
    let (label, color) = match pr.review_state {
        ReviewState::Approved => ("approved", Color::Green),
        ReviewState::ChangesRequested => ("changes", Color::Red),
        ReviewState::Waiting => ("waiting", Color::DarkGray),
    };
    let mut spans = vec![
        Span::styled(format!("{label:<10}"), Style::new().fg(color)),
        drafts(pr.pending_drafts),
        Span::raw(pr.key.url()),
        Span::raw("  "),
    ];
    if pr.is_draft {
        spans.push("[draft] ".dim());
    }
    spans.push(Span::raw(pr.title.as_str()));
    ListItem::new(Line::from(spans))
}

/// A fixed-width pending draft count; blank when there are none.
fn drafts(n: u32) -> Span<'static> {
    let text = match n {
        0 => String::new(),
        1 => "1 draft".into(),
        n => format!("{n} drafts"),
    };
    Span::styled(format!("{text:<10}"), Style::new().fg(Color::Magenta))
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

fn render_status(frame: &mut Frame<'_>, area: Rect, counts: &RunCounts, no_reviews: bool) {
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
    lines.push(Line::raw(" Read-only: edit and submit drafts in the dashboard.").dim());
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let area = frame
        .area()
        .centered(Constraint::Length(58), Constraint::Length(height));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
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

    fn overview() -> Overview {
        Overview {
            owed: vec![
                OwedReview {
                    key: pr("org/api", 481),
                    title: "Retry webhook deliveries".into(),
                    author: "alice".into(),
                    latest_run: Some(latest("succeeded", None)),
                    pending_drafts: 3,
                },
                OwedReview {
                    key: pr("org/web", 77),
                    title: "Fix login flake".into(),
                    author: "bob".into(),
                    latest_run: Some(latest("queued", None)),
                    pending_drafts: 0,
                },
                OwedReview {
                    key: pr("org/web", 79),
                    title: "Cache avatars".into(),
                    author: "carol".into(),
                    latest_run: Some(latest(
                        "crashed",
                        Some("index out of bounds\nat src/lib.rs"),
                    )),
                    pending_drafts: 0,
                },
            ],
            mine: vec![
                MyPr {
                    key: pr("org/api", 470),
                    title: "Speed up search".into(),
                    is_draft: false,
                    review_state: ReviewState::ChangesRequested,
                    pending_drafts: 0,
                },
                MyPr {
                    key: pr("org/web", 80),
                    title: "New settings page".into(),
                    is_draft: true,
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
        insta::assert_snapshot!(draw(&mut app(false), 80, 24).backend());
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
        // Stops at the last row.
        assert_eq!(app.lists[Pane::Owed.index()].selected(), Some(2));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.lists[Pane::Owed.index()].selected(), Some(1));

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
        assert!(app.help);
        press(&mut app, KeyCode::Esc);
        assert!(!app.help);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Flow::Quit);
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut app = App::new(false);
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(app.handle_key(release), Flow::Continue);
    }
}
