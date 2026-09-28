//! The `e` editor: every key of the config file, edited in place with its
//! comments and layout kept, checked with the loader after each edit, and
//! written only once it loads and you've seen the diff.

mod check;

use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

use color_eyre::eyre::Result;
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use sanic_core::config::contract_path;

pub use self::check::{Checked, Checker};
use crate::config_doc::{
    ConfigDoc, Key, Op, Saved, Scalar, Setting, Table,
    schema::{Fallback, Kind},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEditor {
    path: PathBuf,
    /// The file `path` links to, when it's a symlink.
    target: Option<PathBuf>,
    doc: ConfigDoc,
    focus: Focus,
    /// Which table, by [`ConfigEditor::tables`].
    table: usize,
    /// Which of its keys.
    row: usize,
    /// What you're typing into the selected key.
    input: Option<String>,
    check: Check,
    /// Counts the texts sent to be checked, so a late answer about an
    /// older one is ignored.
    generation: u64,
    popup: Option<Popup>,
    /// Shown in the status line until the next key.
    notice: Option<String>,
}

/// Whether the edited text loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Checking,
    Loads,
    Fails(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tables,
    Keys,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Popup {
    Help,
    /// The changes the edits make; `saving` asks for a yes to write them.
    Diff {
        scroll: u16,
        saving: bool,
    },
    /// Waiting for a yes before throwing the edits away.
    Discard,
    /// Waiting for a yes before saving an edit that turns manual reviews
    /// off, which starts this many held reviews.
    Held(u32),
}

/// What a key did to the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Outcome {
    Open,
    Close,
    /// Ctrl-C: whatever quitting does where it runs.
    Quit,
    /// Check `text` and hand the answer to [`ConfigEditor::checked`].
    Check {
        generation: u64,
        text: String,
    },
    /// Write the edits, told `told` held reviews start if they turn manual
    /// reviews off; see [`ConfigEditor::ask_held`].
    Save {
        told: u32,
    },
}

impl ConfigEditor {
    /// Edits `doc`, read from `path`. Its first check is
    /// [`ConfigEditor::check_now`]'s to ask for.
    #[must_use]
    pub fn new(path: &Path, target: Option<PathBuf>, doc: ConfigDoc) -> Self {
        Self {
            path: path.to_owned(),
            target,
            doc,
            focus: Focus::Tables,
            table: 0,
            row: 0,
            input: None,
            check: Check::Checking,
            generation: 0,
            popup: None,
            notice: None,
        }
    }

    #[must_use]
    pub fn doc(&self) -> &ConfigDoc {
        &self.doc
    }

    /// Asks for the text as it is now to be checked.
    pub fn check_now(&mut self) -> Outcome {
        self.generation += 1;
        self.check = Check::Checking;
        Outcome::Check {
            generation: self.generation,
            text: self.doc.text(),
        }
    }

    /// The answer to [`Outcome::Check`] for `generation`.
    pub fn checked(&mut self, generation: u64, checked: Checked) {
        if generation == self.generation {
            self.check = match checked {
                Ok(()) => Check::Loads,
                Err(why) => Check::Fails(why),
            };
        }
    }

    /// Saving would start `held` held reviews: asks first, and on a yes
    /// saves again telling so.
    pub fn ask_held(&mut self, held: u32) {
        self.popup = Some(Popup::Held(held));
    }

    /// Whether saving turns manual reviews off, which `serve` has `on`.
    #[must_use]
    pub fn turns_manual_reviews_off(&self, on: bool) -> bool {
        let key = Key::new(Table::Runner, "manual_reviews");
        on && key.is_some_and(|k| self.doc.scalar(&k) == Setting::Set(Scalar::Bool(false)))
    }

    /// How [`Outcome::Save`] went. Once written, editing carries on from
    /// what was written.
    pub fn saved(&mut self, saved: Result<Saved>) {
        let saved = match saved {
            Ok(saved) => saved,
            Err(err) => {
                self.notice = Some(format!("not saved: {err:#}"));
                return;
            }
        };
        let api_url = Key::new(Table::Github, "api_url");
        let restart = api_url.is_some_and(|key| {
            ConfigDoc::parse(self.doc.source())
                .is_ok_and(|before| before.scalar(&key) != self.doc.scalar(&key))
        });
        match ConfigDoc::parse(Some(&saved.text)) {
            Ok(doc) => self.doc = doc,
            Err(err) => {
                self.notice = Some(format!("saved, but can't read it back: {err:#}"));
                return;
            }
        }
        let mut notice = format!("saved {}", contract_path(&self.path));
        if saved.replayed {
            notice.push_str(", on top of changes made to it meanwhile");
        }
        if restart {
            notice.push_str(" · github.api_url applies when serve restarts");
        }
        self.notice = Some(notice);
    }

    /// The tables, in the order they're listed: the sections, then the
    /// profiles.
    fn tables(&self) -> Vec<Table> {
        Table::SECTIONS
            .into_iter()
            .chain(self.doc.profiles().into_iter().map(Table::Profile))
            .collect()
    }

    fn current_table(&self) -> Table {
        let mut tables = self.tables();
        let at = self.table.min(tables.len() - 1);
        tables.swap_remove(at)
    }

    fn current_key(&self) -> Option<Key> {
        let table = self.current_table();
        let field = table.fields().get(self.row)?;
        Key::new(table, field.name)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.notice = None;
        if ctrl && key.code == KeyCode::Char('c') {
            return Outcome::Quit;
        }
        if let Some(popup) = self.popup.take() {
            return self.popup_key(popup, key);
        }
        if let Some(input) = self.input.take() {
            return self.input_key(input, key);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return self.leave(),
            KeyCode::Char('s') if ctrl => self.ask_save(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Tables => Focus::Keys,
                    Focus::Keys => Focus::Tables,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_to(0),
            KeyCode::Char('G') | KeyCode::End => self.move_to(usize::MAX),
            KeyCode::Enter if self.focus == Focus::Tables => self.focus = Focus::Keys,
            KeyCode::Enter => return self.edit(),
            KeyCode::Char(' ') if self.focus == Focus::Keys => return self.toggle(),
            KeyCode::Char('u') if self.focus == Focus::Keys => return self.unset(),
            KeyCode::Char('v') => {
                self.popup = Some(Popup::Diff {
                    scroll: 0,
                    saving: false,
                });
            }
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
            _ => {}
        }
        Outcome::Open
    }

    fn popup_key(&mut self, popup: Popup, key: KeyEvent) -> Outcome {
        match (popup, key.code) {
            (Popup::Diff { scroll, saving }, KeyCode::Char('j') | KeyCode::Down) => {
                self.popup = Some(Popup::Diff {
                    scroll: scroll.saturating_add(1),
                    saving,
                });
            }
            (Popup::Diff { scroll, saving }, KeyCode::Char('k') | KeyCode::Up) => {
                self.popup = Some(Popup::Diff {
                    scroll: scroll.saturating_sub(1),
                    saving,
                });
            }
            (Popup::Diff { saving: true, .. }, KeyCode::Char('y')) => {
                return Outcome::Save { told: 0 };
            }
            (Popup::Discard, KeyCode::Char('y')) => return Outcome::Close,
            (Popup::Held(held), KeyCode::Char('y')) => return Outcome::Save { told: held },
            // Anything else closes it; a stray key neither writes nor
            // throws edits away.
            _ => {}
        }
        Outcome::Open
    }

    fn input_key(&mut self, mut input: String, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let number = self
            .current_key()
            .is_some_and(|k| k.field().kind == Kind::Number);
        match key.code {
            KeyCode::Esc => return Outcome::Open,
            KeyCode::Enter => return self.commit(&input),
            KeyCode::Char('u') if ctrl => input.clear(),
            KeyCode::Char(c) if !ctrl && (!number || c.is_ascii_digit()) => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            _ => {}
        }
        self.input = Some(input);
        Outcome::Open
    }

    /// Enter on a key: starts typing a text or number, or toggles a bool.
    fn edit(&mut self) -> Outcome {
        let Some(key) = self.current_key() else {
            return Outcome::Open;
        };
        match key.field().kind {
            Kind::Bool => self.toggle(),
            Kind::Text | Kind::Number => {
                self.input = Some(match self.doc.scalar(&key) {
                    Setting::Set(value) => scalar_text(&value),
                    Setting::Unset | Setting::Invalid(_) => String::new(),
                });
                Outcome::Open
            }
            Kind::List | Kind::Repos => {
                self.notice = Some(format!("`{key}` is a list; edit it by hand for now"));
                Outcome::Open
            }
        }
    }

    /// Sets the key being typed in; blank unsets it.
    fn commit(&mut self, input: &str) -> Outcome {
        let Some(key) = self.current_key() else {
            return Outcome::Open;
        };
        let input = input.trim();
        let op = if input.is_empty() {
            if self.doc.scalar(&key) == Setting::Unset {
                return Outcome::Open;
            }
            Op::Unset { key }
        } else {
            let value = match key.field().kind {
                Kind::Number => {
                    let Ok(n) = input.parse() else {
                        self.notice = Some(format!("{input} is too big for `{key}`"));
                        self.input = Some(input.to_owned());
                        return Outcome::Open;
                    };
                    Scalar::Number(n)
                }
                _ => Scalar::Text(input.to_owned()),
            };
            if self.doc.scalar(&key) == Setting::Set(value.clone()) {
                return Outcome::Open;
            }
            Op::Set { key, value }
        };
        self.apply(op)
    }

    /// Space on a bool: flips what it means now, set or not.
    fn toggle(&mut self) -> Outcome {
        let Some(key) = self.current_key() else {
            return Outcome::Open;
        };
        if key.field().kind != Kind::Bool {
            return Outcome::Open;
        }
        let now = match self.shown(&key) {
            Shown::Set(value) | Shown::Default(value) => value == "true",
            Shown::Invalid(_) | Shown::Required => false,
        };
        self.apply(Op::Set {
            key,
            value: Scalar::Bool(!now),
        })
    }

    fn unset(&mut self) -> Outcome {
        let Some(key) = self.current_key() else {
            return Outcome::Open;
        };
        if matches!(key.field().fallback, Fallback::Required) {
            self.notice = Some(format!("`{key}` has to be set"));
            return Outcome::Open;
        }
        let set = match key.field().kind {
            Kind::List => self.doc.list(&key) != Setting::Unset,
            Kind::Repos => false,
            Kind::Text | Kind::Number | Kind::Bool => self.doc.scalar(&key) != Setting::Unset,
        };
        if !set {
            self.notice = Some(format!("`{key}` isn't set"));
            return Outcome::Open;
        }
        self.apply(Op::Unset { key })
    }

    fn apply(&mut self, op: Op) -> Outcome {
        match self.doc.apply(op) {
            Ok(()) => self.check_now(),
            Err(err) => {
                self.notice = Some(format!("{err:#}"));
                Outcome::Open
            }
        }
    }

    /// Ctrl-S: shows the diff to confirm, once the edits load.
    fn ask_save(&mut self) {
        if !self.doc.is_changed() {
            self.notice = Some("nothing to save".into());
            return;
        }
        match &self.check {
            Check::Loads => {
                self.popup = Some(Popup::Diff {
                    scroll: 0,
                    saving: true,
                });
            }
            Check::Checking => self.notice = Some("still checking the config".into()),
            Check::Fails(why) => {
                let why = why.clone();
                self.show_where(&why);
                self.notice = Some("fix what's above before saving: it doesn't load".into());
            }
        }
    }

    /// Selects the profile an error names, if it names one.
    fn show_where(&mut self, why: &str) {
        let named = why
            .split_once("in profile `")
            .and_then(|(_, rest)| rest.split_once('`'))
            .map(|(name, _)| Table::Profile(name.to_owned()));
        if let Some(at) = named.and_then(|t| self.tables().iter().position(|x| *x == t)) {
            self.table = at;
            self.row = 0;
            self.focus = Focus::Keys;
        }
    }

    fn leave(&mut self) -> Outcome {
        if self.doc.is_changed() {
            self.popup = Some(Popup::Discard);
            Outcome::Open
        } else {
            Outcome::Close
        }
    }

    fn move_by(&mut self, delta: isize) {
        let now = match self.focus {
            Focus::Tables => self.table,
            Focus::Keys => self.row,
        };
        self.move_to(now.saturating_add_signed(delta));
    }

    fn move_to(&mut self, target: usize) {
        match self.focus {
            Focus::Tables => {
                let last = self.tables().len() - 1;
                let target = target.min(last);
                if target != self.table {
                    self.table = target;
                    self.row = 0;
                }
            }
            Focus::Keys => {
                let last = self.current_table().fields().len() - 1;
                self.row = target.min(last);
            }
        }
    }

    /// What `key` shows: its value, or what it means unset.
    fn shown(&self, key: &Key) -> Shown {
        let field = key.field();
        let written = match field.kind {
            Kind::List => match self.doc.list(key) {
                Setting::Set(items) => Setting::Set(list_text(&items)),
                Setting::Unset => Setting::Unset,
                Setting::Invalid(raw) => Setting::Invalid(raw),
            },
            Kind::Repos => match &key.table {
                Table::Profile(name) => match self.doc.repos(name) {
                    Setting::Set(repos) => Setting::Set(
                        repos
                            .entries
                            .iter()
                            .map(|entry| match entry {
                                Ok(entry) => entry_text(entry),
                                Err(raw) => raw.clone(),
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    Setting::Unset => Setting::Unset,
                    Setting::Invalid(raw) => Setting::Invalid(raw),
                },
                _ => Setting::Unset,
            },
            Kind::Text | Kind::Number | Kind::Bool => match self.doc.scalar(key) {
                Setting::Set(value) => Setting::Set(scalar_text(&value)),
                Setting::Unset => Setting::Unset,
                Setting::Invalid(raw) => Setting::Invalid(raw),
            },
        };
        match written {
            Setting::Set(value) => Shown::Set(value),
            Setting::Invalid(raw) => Shown::Invalid(raw),
            Setting::Unset => match &field.fallback {
                Fallback::Value(value) => Shown::Default(value()),
                Fallback::Nothing if field.kind == Kind::Bool => Shown::Default("false".into()),
                Fallback::Nothing => Shown::Default("none".into()),
                Fallback::Inherits(table, name) => {
                    Key::new(table.clone(), name).map_or(Shown::Required, |inherited| {
                        match self.shown(&inherited) {
                            Shown::Set(value) | Shown::Default(value) => Shown::Default(value),
                            other => other,
                        }
                    })
                }
                Fallback::Required => Shown::Required,
            },
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let changes = self.doc.ops().len();
        let mut title = format!(" Config {}", contract_path(&self.path));
        if let Some(target) = &self.target {
            let _ = write!(title, " → {}", contract_path(target));
        }
        title.push(' ');
        let mut outer = Block::bordered().title(title);
        if self.doc.is_changed() {
            let s = if changes == 1 { "" } else { "s" };
            outer = outer.title_top(Line::from(format!(" {changes} change{s} ")).right_aligned());
        }
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        let [body, help, status] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let [tables, keys] =
            Layout::horizontal([Constraint::Length(20), Constraint::Fill(1)]).areas(body);
        self.render_tables(frame, tables);
        self.render_keys(frame, keys);
        self.render_footer(frame, help, status);
        match &self.popup {
            Some(Popup::Help) => render_help(frame),
            Some(Popup::Diff { scroll, saving }) => self.render_diff(frame, *scroll, *saving),
            Some(Popup::Discard) => render_question(
                frame,
                " Discard? ",
                &format!(
                    " Discard {changes} unsaved change{}?",
                    if changes == 1 { "" } else { "s" }
                ),
                " y discard · any other key keeps editing",
            ),
            Some(Popup::Held(starting)) => render_question(
                frame,
                " Manual reviews off? ",
                &format!(
                    " This turns manual reviews off: {starting} held review{} will start.",
                    if *starting == 1 { "" } else { "s" }
                ),
                " y save · any other key cancels",
            ),
            None => {}
        }
    }

    fn render_tables(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut items = Vec::new();
        let mut selected = None;
        for (i, table) in self.tables().iter().enumerate() {
            if i == Table::SECTIONS.len() {
                items.push(ListItem::new(" profiles").dim());
            }
            if i == self.table {
                selected = Some(items.len());
            }
            items.push(ListItem::new(match table {
                Table::Profile(name) => format!("   {name}"),
                section => format!(" {section}"),
            }));
        }
        if self.doc.profiles().is_empty() {
            items.push(ListItem::new(" profiles").dim());
            items.push(ListItem::new("   none yet").dim());
        }
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::new().borders(Borders::RIGHT))
                .highlight_style(highlight(self.focus == Focus::Tables)),
            area,
            &mut state,
        );
    }

    fn render_keys(&self, frame: &mut Frame<'_>, area: Rect) {
        let table = self.current_table();
        let fields = table.fields();
        let width = fields.iter().map(|f| f.name.len()).max().unwrap_or(0) + 2;
        let mut items = vec![
            ListItem::new(Line::from(format!(" [{table}]").bold())),
            ListItem::new(""),
        ];
        for (i, field) in fields.iter().enumerate() {
            let Some(key) = Key::new(table.clone(), field.name) else {
                continue;
            };
            let value = match (&self.input, i == self.row) {
                (Some(input), true) => Line::from(vec![Span::raw(input.clone()), "▏".slow_blink()]),
                _ => match self.shown(&key) {
                    Shown::Set(value) => Line::raw(value),
                    Shown::Default(value) => {
                        let from = match &field.fallback {
                            Fallback::Inherits(table, name) => format!("{table}.{name}"),
                            _ => "default".into(),
                        };
                        Line::from(format!("‹{from}: {value}›").dim())
                    }
                    Shown::Invalid(raw) => Line::from(Span::styled(
                        format!("{raw}  (not a {})", kind_name(field.kind)),
                        Style::new().fg(Color::Red),
                    )),
                    Shown::Required => {
                        Line::from(Span::styled("required", Style::new().fg(Color::Red)))
                    }
                },
            };
            let mut spans = vec![Span::raw(format!(" {:<width$}", field.name))];
            spans.extend(value.spans);
            items.push(ListItem::new(Line::from(spans)));
        }
        let mut state = ListState::default().with_selected(Some(self.row + 2));
        frame.render_stateful_widget(
            List::new(items).highlight_style(highlight(self.focus == Focus::Keys)),
            area.inner(Margin::new(1, 0)),
            &mut state,
        );
    }

    fn render_footer(&self, frame: &mut Frame<'_>, help: Rect, status: Rect) {
        let about = match &self.check {
            Check::Fails(why) => {
                Line::from(Span::styled(format!(" {why}"), Style::new().fg(Color::Red)))
            }
            _ => match self.current_key() {
                Some(key) if self.focus == Focus::Keys => {
                    Line::from(format!(" {}", key.field().help).dim())
                }
                _ => Line::raw(""),
            },
        };
        frame.render_widget(Paragraph::new(about), help);
        if let Some(notice) = &self.notice {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {notice}"),
                    Style::new().fg(Color::Yellow),
                )),
                status,
            );
            return;
        }
        let state = match &self.check {
            Check::Checking => Span::raw(" … checking").dim(),
            Check::Loads => Span::styled(" ✓ loads", Style::new().fg(Color::Green)),
            Check::Fails(_) => Span::styled(" ✗ doesn't load", Style::new().fg(Color::Red)),
        };
        let hint = if self.input.is_some() {
            "Enter set · blank unsets · Esc cancel · Ctrl-U clear "
        } else {
            "^S save · u unset · v diff · Esc leave · ? help "
        };
        let [left, right] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(u16::try_from(hint.chars().count()).unwrap_or(u16::MAX)),
        ])
        .areas(status);
        frame.render_widget(Paragraph::new(state), left);
        frame.render_widget(Paragraph::new(hint.dim()), right);
    }

    fn render_diff(&self, frame: &mut Frame<'_>, scroll: u16, saving: bool) {
        let area = frame.area().inner(Margin::new(3, 2));
        frame.render_widget(Clear, area);
        let name = contract_path(&self.path);
        let text = self.doc.text();
        let diff = similar::TextDiff::from_lines(self.doc.original(), &text);
        let mut lines: Vec<Line<'_>> = diff
            .unified_diff()
            .header(&name, &name)
            .to_string()
            .lines()
            .map(|line| {
                let color = match line.chars().next() {
                    Some('+') if !line.starts_with("+++") => Some(Color::Green),
                    Some('-') if !line.starts_with("---") => Some(Color::Red),
                    Some('@') => Some(Color::Cyan),
                    _ => None,
                };
                let line = format!(" {line}");
                match color {
                    Some(color) => Line::from(Span::styled(line, Style::new().fg(color))),
                    None => Line::raw(line),
                }
            })
            .collect();
        if lines.is_empty() {
            lines.push(Line::raw(" No changes.").dim());
        }
        let block = Block::bordered().title(if saving { " Save? " } else { " Changes " });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [body, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
        let hint = if saving {
            " y write it · j/k scroll · any other key keeps editing"
        } else {
            " j/k scroll · any other key closes"
        };
        frame.render_widget(Paragraph::new(hint.dim()), footer);
    }
}

/// What a key shows in the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shown {
    Set(String),
    /// Unset: what it means then, from its default or the key it inherits.
    Default(String),
    Invalid(String),
    Required,
}

fn scalar_text(value: &Scalar) -> String {
    match value {
        Scalar::Text(text) => text.clone(),
        Scalar::Number(n) => n.to_string(),
        Scalar::Bool(b) => b.to_string(),
    }
}

fn list_text(items: &[String]) -> String {
    if items.is_empty() {
        "[]".into()
    } else {
        items.join(", ")
    }
}

fn entry_text(entry: &crate::config_doc::RepoEntry) -> String {
    let mut value = entry.to_value();
    value.decor_mut().clear();
    value.to_string()
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Text => "string",
        Kind::Number => "number",
        Kind::Bool => "true or false",
        Kind::List => "list of strings",
        Kind::Repos => "list of repos",
    }
}

/// The selected row stands out more in the focused pane.
fn highlight(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    }
}

const HELP: &[(&str, &str)] = &[
    ("Tab, Shift-Tab", "switch between tables and keys"),
    ("j/k, Down/Up", "move"),
    ("g/G, Home/End", "first, last"),
    ("Enter", "edit the key, or toggle true/false"),
    ("Space", "toggle true/false"),
    ("u", "unset the key, back to its default"),
    ("v", "show the changes"),
    ("Ctrl-S", "save, after showing the changes"),
    ("Esc, q", "leave, asking about unsaved changes"),
    ("Ctrl-C", "quit"),
    ("?", "close this help"),
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
    lines.push(Line::raw(" Typing: Enter sets, a blank unsets, Esc cancels,").dim());
    lines.push(Line::raw(" Ctrl-U clears.").dim());
    render_popup(frame, " Config editor keys ", lines);
}

fn render_question(frame: &mut Frame<'_>, title: &str, question: &str, keys: &str) {
    render_popup(
        frame,
        title,
        vec![
            Line::raw(question.to_owned()),
            Line::raw(""),
            Line::raw(keys.to_owned()).dim(),
        ],
    );
}

fn render_popup(frame: &mut Frame<'_>, title: &str, lines: Vec<Line<'_>>) {
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let area = frame
        .area()
        .centered(Constraint::Length(66), Constraint::Length(height));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::poll::tests::NoCheckouts;

    const CONFIG: &str = r#"# Mine.
[poll]
quiet_secs = 30 # short

[runner]
manual_reviews = true

[profile.ring]
repos = [{ github = "org" }]
"#;

    fn editor(text: &str) -> ConfigEditor {
        let doc = ConfigDoc::parse(Some(text)).unwrap();
        let mut editor = ConfigEditor::new(
            Path::new("/home/u/.config/sanic-review/config.toml"),
            Some(PathBuf::from("/home/u/dotfiles/sanic.toml")),
            doc,
        );
        settle(&mut editor);
        editor
    }

    /// Answers the pending check as the loader would.
    fn settle(editor: &mut ConfigEditor) {
        let Outcome::Check { generation, text } = editor.check_now() else {
            unreachable!();
        };
        let path = Path::new("/c/config.toml");
        editor.checked(generation, check::check(&text, path, &NoCheckouts));
    }

    fn press(editor: &mut ConfigEditor, code: KeyCode) -> Outcome {
        editor.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(editor: &mut ConfigEditor, c: char) -> Outcome {
        editor.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn typed(editor: &mut ConfigEditor, text: &str) {
        for c in text.chars() {
            assert_eq!(press(editor, KeyCode::Char(c)), Outcome::Open);
        }
    }

    fn draw(editor: &ConfigEditor) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| editor.render(frame)).unwrap();
        terminal
    }

    /// Selects `table`'s `row`th key.
    fn select(editor: &mut ConfigEditor, table: usize, row: usize) {
        editor.focus = Focus::Tables;
        let _ = press(editor, KeyCode::Char('g'));
        for _ in 0..table {
            let _ = press(editor, KeyCode::Char('j'));
        }
        let _ = press(editor, KeyCode::Enter);
        for _ in 0..row {
            let _ = press(editor, KeyCode::Char('j'));
        }
    }

    #[test]
    fn shows_every_key_with_defaults_for_the_unset_ones() {
        let mut editor = editor(CONFIG);
        select(&mut editor, 1, 2);
        insta::assert_snapshot!(draw(&editor).backend());
        // A profile's unset keys show what they inherit.
        select(&mut editor, 4, 2);
        insta::assert_snapshot!("profile", draw(&editor).backend());
    }

    #[test]
    fn numbers_are_typed_set_and_unset_then_checked() {
        let mut editor = editor(CONFIG);
        select(&mut editor, 1, 2);
        assert_eq!(press(&mut editor, KeyCode::Enter), Outcome::Open);
        assert_eq!(editor.input.as_deref(), Some("30"));
        let _ = ctrl(&mut editor, 'u');
        typed(&mut editor, "4x5");
        assert_eq!(editor.input.as_deref(), Some("45"), "only digits");
        let Outcome::Check { text, .. } = press(&mut editor, KeyCode::Enter) else {
            panic!("no check");
        };
        assert!(text.contains("quiet_secs = 45 # short"), "{text}");
        assert_eq!(editor.check, Check::Checking);
        settle(&mut editor);
        assert_eq!(editor.check, Check::Loads);

        // A blank unsets it.
        let _ = press(&mut editor, KeyCode::Enter);
        let _ = ctrl(&mut editor, 'u');
        let Outcome::Check { text, .. } = press(&mut editor, KeyCode::Enter) else {
            panic!("no check");
        };
        assert!(!text.contains("quiet_secs"), "{text}");
        assert_eq!(editor.doc.ops().len(), 2);

        // Esc drops what was typed.
        let _ = press(&mut editor, KeyCode::Enter);
        typed(&mut editor, "9");
        assert_eq!(press(&mut editor, KeyCode::Esc), Outcome::Open);
        assert_eq!(editor.doc.ops().len(), 2);
    }

    #[test]
    fn bools_toggle_from_what_they_mean_unset_too() {
        let mut editor = editor(CONFIG);
        // review_requests.skip_drafts, unset: true by default.
        select(&mut editor, 2, 2);
        let Outcome::Check { text, .. } = press(&mut editor, KeyCode::Char(' ')) else {
            panic!("no check");
        };
        assert!(
            text.contains("[review_requests]\nskip_drafts = false\n"),
            "{text}"
        );
        let _ = press(&mut editor, KeyCode::Enter);
        assert!(editor.doc.text().contains("skip_drafts = true"));
        let _ = press(&mut editor, KeyCode::Char('u'));
        assert!(!editor.doc.text().contains("review_requests"));
    }

    #[test]
    fn saving_needs_a_config_that_loads_and_a_yes() {
        let mut editor = editor(CONFIG);
        assert_eq!(ctrl(&mut editor, 's'), Outcome::Open);
        assert_eq!(editor.notice.as_deref(), Some("nothing to save"));

        // poll.reconcile_secs = 0 doesn't load.
        select(&mut editor, 1, 0);
        let _ = press(&mut editor, KeyCode::Enter);
        typed(&mut editor, "0");
        let _ = press(&mut editor, KeyCode::Enter);
        settle(&mut editor);
        assert!(matches!(&editor.check, Check::Fails(why) if why.contains("reconcile_secs")));
        assert_eq!(ctrl(&mut editor, 's'), Outcome::Open);
        assert!(editor.popup.is_none());
        insta::assert_snapshot!("doesnt_load", draw(&editor).backend());

        let _ = press(&mut editor, KeyCode::Enter);
        typed(&mut editor, "600");
        let _ = press(&mut editor, KeyCode::Enter);
        settle(&mut editor);
        assert_eq!(ctrl(&mut editor, 's'), Outcome::Open);
        insta::assert_snapshot!("save_diff", draw(&editor).backend());
        // Any other key keeps editing; y writes.
        assert_eq!(press(&mut editor, KeyCode::Char('n')), Outcome::Open);
        assert_eq!(ctrl(&mut editor, 's'), Outcome::Open);
        assert_eq!(
            press(&mut editor, KeyCode::Char('y')),
            Outcome::Save { told: 0 }
        );
    }

    #[test]
    fn a_save_carries_on_from_what_was_written() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let mut editor = editor(CONFIG);
        editor.path.clone_from(&path);
        // github.api_url, which serve reads only at startup.
        select(&mut editor, 0, 0);
        let _ = press(&mut editor, KeyCode::Enter);
        typed(&mut editor, "http://localhost:1");
        let _ = press(&mut editor, KeyCode::Enter);
        settle(&mut editor);
        editor.saved(crate::config_doc::save(&path, &editor.doc, &NoCheckouts));
        let notice = editor.notice.clone().unwrap();
        assert!(notice.contains("applies when serve restarts"), "{notice}");
        assert!(!editor.doc.is_changed());
        assert_eq!(
            editor.doc.source(),
            Some(std::fs::read_to_string(&path).unwrap().as_str())
        );
        assert_eq!(press(&mut editor, KeyCode::Esc), Outcome::Close);
    }

    #[test]
    fn leaving_with_unsaved_edits_asks_first() {
        let mut editor = editor(CONFIG);
        select(&mut editor, 3, 5);
        let _ = press(&mut editor, KeyCode::Char(' '));
        assert!(editor.turns_manual_reviews_off(true));
        assert!(!editor.turns_manual_reviews_off(false));
        assert_eq!(press(&mut editor, KeyCode::Char('q')), Outcome::Open);
        insta::assert_snapshot!("discard", draw(&editor).backend());
        assert_eq!(press(&mut editor, KeyCode::Char('n')), Outcome::Open);
        assert_eq!(press(&mut editor, KeyCode::Esc), Outcome::Open);
        assert_eq!(press(&mut editor, KeyCode::Char('y')), Outcome::Close);
        assert_eq!(ctrl(&mut editor, 'c'), Outcome::Quit);
    }

    #[test]
    fn saving_that_starts_held_reviews_asks_with_the_count() {
        let mut editor = editor(CONFIG);
        editor.ask_held(3);
        insta::assert_snapshot!("held", draw(&editor).backend());
        assert_eq!(
            press(&mut editor, KeyCode::Char('y')),
            Outcome::Save { told: 3 }
        );
        editor.ask_held(3);
        assert_eq!(press(&mut editor, KeyCode::Enter), Outcome::Open);
    }

    #[test]
    fn a_late_check_of_older_text_is_ignored() {
        let mut editor = editor(CONFIG);
        let Outcome::Check { generation, .. } = editor.check_now() else {
            unreachable!();
        };
        let _ = editor.check_now();
        editor.checked(generation, Err("old".into()));
        assert_eq!(editor.check, Check::Checking);
    }

    #[test]
    fn a_failure_in_a_profile_selects_it_on_save() {
        let mut editor = editor(&CONFIG.replace("{ github = \"org\" }", "\"~/github/sanic-cli\""));
        // The checkout can't be resolved without a real repo.
        assert!(matches!(&editor.check, Check::Fails(why) if why.contains("in profile `ring`")));
        select(&mut editor, 1, 2);
        let _ = press(&mut editor, KeyCode::Char('u'));
        settle(&mut editor);
        let _ = ctrl(&mut editor, 's');
        assert_eq!(editor.current_table(), Table::Profile("ring".into()));
    }
}
