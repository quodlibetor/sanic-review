//! The queue overlay: what's running, what's next, and the keys that
//! reorder or cancel it.
//!
//! A move shows at once and gives way to the store's order as soon as the
//! store agrees, so the one-second reload doesn't undo it mid-flight.

use std::time::{Duration, Instant};

use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use sanic_store::QueueEntry;

use crate::tui::render_popup;

/// How long a move is shown before the store's order wins anyway, so a
/// refused move corrects itself rather than sticking.
const HOLD: Duration = Duration::from_millis(1500);

/// What the overlay wants done after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Handled; stay open.
    Open,
    Close,
    Quit,
    Move {
        run: i64,
        up: bool,
    },
    Cancel(i64),
    /// Open the dashboard's queue page.
    Dashboard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueView {
    entries: Vec<QueueEntry>,
    /// The run under the cursor, by id, so a reload that reorders keeps it.
    cursor: Option<i64>,
    /// A cancel waiting for its `y`.
    confirm: Option<i64>,
    /// A move sent but not yet seen: the order it should come back as, and
    /// when it was sent.
    expect: Option<(Vec<i64>, Instant)>,
    notice: Option<String>,
}

impl QueueView {
    #[must_use]
    pub fn new(entries: Vec<QueueEntry>) -> Self {
        let cursor = entries.first().map(|e| e.run_id);
        Self {
            entries,
            cursor,
            confirm: None,
            expect: None,
            notice: None,
        }
    }

    /// The runs shown, in order.
    pub fn ids(&self) -> Vec<i64> {
        self.entries.iter().map(|e| e.run_id).collect()
    }

    fn at(&self) -> Option<usize> {
        let cursor = self.cursor?;
        self.entries.iter().position(|e| e.run_id == cursor)
    }

    fn current(&self) -> Option<&QueueEntry> {
        self.entries.get(self.at()?)
    }

    /// Takes the store's queue, unless a move of ours hasn't landed yet.
    pub fn update(&mut self, fresh: Vec<QueueEntry>) {
        let ids: Vec<i64> = fresh.iter().map(|e| e.run_id).collect();
        let hold = match &self.expect {
            Some((want, _)) if *want == ids => false,
            Some((_, at)) => at.elapsed() < HOLD,
            None => false,
        };
        if hold {
            self.merge(fresh);
        } else {
            self.expect = None;
            self.entries = fresh;
        }
        // A run that started or finished is never left on screen, and the
        // cursor falls to the nearest row.
        if self.at().is_none() {
            self.cursor = self.entries.first().map(|e| e.run_id);
        }
        if self.confirm.is_some_and(|run| !ids_has(&self.entries, run)) {
            self.confirm = None;
        }
    }

    /// Keeps the order shown but takes each run's fresh state, dropping
    /// runs the store no longer has.
    fn merge(&mut self, fresh: Vec<QueueEntry>) {
        let order = self.ids();
        let mut merged: Vec<QueueEntry> = order
            .iter()
            .filter_map(|run| fresh.iter().find(|e| e.run_id == *run).cloned())
            .collect();
        for entry in fresh {
            if !merged.iter().any(|e| e.run_id == entry.run_id) {
                merged.push(entry);
            }
        }
        self.entries = merged;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind != KeyEventKind::Press {
            return Outcome::Open;
        }
        self.notice = None;
        if let Some(run) = self.confirm.take() {
            return match key.code {
                KeyCode::Char('y') => Outcome::Cancel(run),
                _ => Outcome::Open,
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Outcome::Quit;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => Outcome::Close,
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_to(0),
            KeyCode::Char('G') | KeyCode::End => self.move_to(self.entries.len().saturating_sub(1)),
            KeyCode::Char('K') => self.shift(true),
            KeyCode::Char('J') => self.shift(false),
            KeyCode::Char('c') => {
                match self.current() {
                    Some(entry) => self.confirm = Some(entry.run_id),
                    None => self.notice = Some("nothing to cancel".into()),
                }
                Outcome::Open
            }
            KeyCode::Char('o') => Outcome::Dashboard,
            _ => Outcome::Open,
        }
    }

    fn move_cursor(&mut self, by: isize) -> Outcome {
        let Some(at) = self.at() else {
            return Outcome::Open;
        };
        let last = self.entries.len().saturating_sub(1);
        let to = at.saturating_add_signed(by).min(last);
        self.move_to(to)
    }

    fn move_to(&mut self, to: usize) -> Outcome {
        if let Some(entry) = self.entries.get(to) {
            self.cursor = Some(entry.run_id);
        }
        Outcome::Open
    }

    /// Moves the selected queued run itself, showing it at once.
    fn shift(&mut self, up: bool) -> Outcome {
        let Some(at) = self.at() else {
            return Outcome::Open;
        };
        if !self.entries[at].is_movable() {
            self.notice = Some(if self.entries[at].is_running() {
                "a running review can't be moved".into()
            } else {
                "a regeneration isn't in the queue".into()
            });
            return Outcome::Open;
        }
        let to = if up {
            match at.checked_sub(1) {
                Some(to) => to,
                None => return Outcome::Open,
            }
        } else {
            at + 1
        };
        // Only queued reviews have places; running runs sit above them and
        // regenerations below.
        if self.entries.get(to).is_none_or(|e| !e.is_movable()) {
            return Outcome::Open;
        }
        let run = self.entries[at].run_id;
        self.entries.swap(at, to);
        self.expect = Some((self.ids(), Instant::now()));
        Outcome::Move { run, up }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let mut lines: Vec<Line<'_>> = Vec::new();
        if self.entries.is_empty() {
            lines.push(Line::raw(" Nothing is queued or running."));
        }
        let mut place = 0;
        for (i, entry) in self.entries.iter().enumerate() {
            if !entry.is_running() {
                place += 1;
            }
            let here = self.at() == Some(i);
            let mark = if entry.is_running() {
                "  ▶ ".to_owned()
            } else {
                format!(" {place:>2} ")
            };
            let style = if here {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(mark, style),
                Span::styled(entry.key.url(), style),
                Span::styled(
                    format!(
                        "  {}",
                        if entry.is_running() {
                            "running"
                        } else {
                            "queued"
                        }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        lines.push(Line::raw(""));
        match (&self.confirm, &self.notice) {
            (Some(_), _) => lines.push(Line::styled(
                " y cancels this run · any other key keeps it",
                Style::default().fg(Color::Yellow),
            )),
            (None, Some(notice)) => lines.push(Line::styled(
                format!(" {notice}"),
                Style::default().fg(Color::Yellow),
            )),
            (None, None) => lines.push(Line::styled(
                " j/k move · K/J reorder · c cancel · o dashboard · Esc closes",
                Style::default().fg(Color::DarkGray),
            )),
        }
        render_popup(frame, " Queue ", lines);
    }
}

fn ids_has(entries: &[QueueEntry], run: i64) -> bool {
    entries.iter().any(|e| e.run_id == run)
}
