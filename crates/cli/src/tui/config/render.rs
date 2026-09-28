//! Drawing the config editor.

use std::fmt::Write;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use sanic_core::config::contract_path;

use super::{
    Check, ConfigEditor, Focus, Input, Popup, Typing,
    rows::{EntryEdit, EntryRow, KINDS, Row, Shown, entries, entry_text, items, kind_label, shown},
};
use crate::config_doc::{
    EntryKind, Key, Table,
    schema::{Fallback, Kind},
};

impl ConfigEditor {
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
            outer = outer
                .title_top(Line::from(format!(" {} ", plural(changes, "change"))).right_aligned());
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
        match &self.entry {
            Some(edit) => self.render_entry(frame, keys, edit),
            None => self.render_keys(frame, keys),
        }
        self.render_footer(frame, help, status);
        match &self.popup {
            Some(Popup::Help) => render_help(frame),
            Some(Popup::Diff { scroll, saving }) => self.render_diff(frame, *scroll, *saving),
            Some(Popup::Discard) => render_question(
                frame,
                " Discard? ",
                &format!(" Discard {} unsaved?", plural(changes, "change")),
                " y discard · any other key keeps editing",
            ),
            Some(Popup::Held(starting)) => render_question(
                frame,
                " Manual reviews off? ",
                &format!(
                    " This turns manual reviews off: {} will start.",
                    plural(*starting as usize, "held review")
                ),
                " y save · any other key cancels",
            ),
            Some(Popup::RemoveProfile(name)) => render_question(
                frame,
                " Remove profile? ",
                &format!(" Remove [profile.{name}] and everything in it?"),
                " y remove · any other key keeps it",
            ),
            None => {}
        }
    }

    /// What's being typed into `into`, with a cursor, if it is.
    fn typed(&self, into: &Input) -> Option<Line<'static>> {
        match &self.typing {
            Some(Typing { into: now, text }) if now == into => {
                Some(Line::from(vec![Span::raw(text.clone()), "▏".slow_blink()]))
            }
            _ => None,
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
        }
        if let Some(line) = self.typed(&Input::NewProfile) {
            selected = Some(items.len());
            let mut spans = vec![Span::raw("   ")];
            spans.extend(line.spans);
            items.push(ListItem::new(Line::from(spans)));
        } else if self.focus == Focus::Tables {
            items.push(ListItem::new("   + profile").dim());
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
        let width = table
            .fields()
            .iter()
            .map(|f| f.name.len())
            .max()
            .unwrap_or(0)
            + 2;
        let mut lines = vec![
            ListItem::new(Line::from(format!(" [{table}]").bold())),
            ListItem::new(""),
        ];
        for row in self.rows() {
            let label = match (&row, row.key()) {
                (Row::Name(_), _) => "name",
                (_, Some(key)) if row.labelled() => key.name,
                _ => "",
            };
            let mut spans = vec![Span::raw(format!(" {label:<width$}"))];
            spans.extend(self.row_value(&row).spans);
            lines.push(ListItem::new(Line::from(spans)));
        }
        let mut state = ListState::default().with_selected(Some(self.row + 2));
        frame.render_stateful_widget(
            List::new(lines).highlight_style(highlight(self.focus == Focus::Keys)),
            area.inner(Margin::new(1, 0)),
            &mut state,
        );
    }

    fn row_value(&self, row: &Row) -> Line<'static> {
        let typing = match row {
            Row::Name(name) => self.typed(&Input::Rename(name.clone())),
            Row::Scalar(key) => self.typed(&Input::Scalar(key.clone())),
            Row::Item(key, n) => self.typed(&Input::Item(key.clone(), *n)),
            Row::NewItem(key) => self.typed(&Input::NewItem(key.clone())),
            _ => None,
        };
        if let Some(line) = typing {
            return line;
        }
        match row {
            Row::Name(name) => Line::raw(name.clone()),
            Row::Item(key, n) => {
                Line::raw(items(&self.doc, key).get(*n).cloned().unwrap_or_default())
            }
            Row::NewItem(_) => Line::raw(""),
            Row::Entry(profile, n) => match entries(&self.doc, profile).get(*n) {
                Some(Ok(entry)) => Line::raw(entry_text(entry)),
                Some(Err(raw)) => {
                    Line::from(vec![Span::raw(raw.clone()), "  (edit by hand)".dim()])
                }
                None => Line::raw(""),
            },
            Row::Scalar(key) | Row::List(key) => shown_line(&shown(&self.doc, key), key),
            Row::NoEntries(profile) => match Key::new(Table::Profile(profile.clone()), "repos") {
                Some(key) => shown_line(&shown(&self.doc, &key), &key),
                None => Line::raw(""),
            },
        }
    }

    fn render_entry(&self, frame: &mut Frame<'_>, area: Rect, edit: &EntryEdit) {
        let place = entries(&self.doc, &edit.profile)
            .iter()
            .position(|e| e.as_ref() == Ok(&edit.entry))
            .filter(|_| edit.in_doc)
            .map_or_else(|| "new entry".to_owned(), |n| format!("repos[{n}]"));
        let mut lines = vec![
            ListItem::new(Line::from(
                format!(" [profile.{}] {place}", edit.profile).bold(),
            )),
            ListItem::new(""),
        ];
        let adding = self.adding_glob();
        let github = edit.entry.kind() == EntryKind::Github;
        for row in edit.rows(adding) {
            let (label, value) = match row {
                EntryRow::Kind => {
                    let mut spans = Vec::new();
                    for kind in KINDS {
                        let mark = if kind == edit.entry.kind() {
                            "(•)"
                        } else {
                            "( )"
                        };
                        spans.push(Span::raw(format!("{mark} {}  ", kind_label(kind))));
                    }
                    ("kind", Line::from(spans))
                }
                EntryRow::Target => (
                    if github { "github" } else { "repo" },
                    self.typed(&Input::Entry(row)).unwrap_or_else(|| {
                        if edit.target().is_empty() {
                            Line::from(Span::styled("required", Style::new().fg(Color::Red)))
                        } else {
                            Line::raw(edit.target().to_owned())
                        }
                    }),
                ),
                EntryRow::Remote => (
                    "remote",
                    self.typed(&Input::Entry(row))
                        .unwrap_or_else(|| match edit.remote() {
                            Some(remote) => Line::raw(remote.to_owned()),
                            None => Line::from("‹discovered from the checkout›".dim()),
                        }),
                ),
                EntryRow::Glob(n) => (
                    if n == 0 { "paths" } else { "" },
                    self.typed(&Input::Entry(row)).unwrap_or_else(|| {
                        Line::raw(edit.globs().get(n).cloned().unwrap_or_default())
                    }),
                ),
                EntryRow::NoGlobs => (
                    "paths",
                    Line::from(
                        if github {
                            "‹none: every PR in it›"
                        } else {
                            "‹none yet: + adds a glob›"
                        }
                        .dim(),
                    ),
                ),
                EntryRow::NewGlob => (
                    "",
                    self.typed(&Input::Entry(row))
                        .unwrap_or_else(|| Line::raw("")),
                ),
            };
            let mut spans = vec![Span::raw(format!(" {label:<8}"))];
            spans.extend(value.spans);
            lines.push(ListItem::new(Line::from(spans)));
        }
        let mut state = ListState::default().with_selected(Some(edit.row + 2));
        frame.render_stateful_widget(
            List::new(lines).highlight_style(highlight(true)),
            area.inner(Margin::new(1, 0)),
            &mut state,
        );
    }

    fn render_footer(&self, frame: &mut Frame<'_>, help: Rect, status: Rect) {
        let about = match (&self.check, &self.entry, self.current_key()) {
            (Check::Fails(why), _, _) => {
                Line::from(Span::styled(format!(" {why}"), Style::new().fg(Color::Red)))
            }
            (_, Some(_), _) => Line::from(
                " paths are globs from the repo's root; the most specific entry wins".dim(),
            ),
            (_, None, Some(key)) => Line::from(format!(" {}", key.field().help).dim()),
            _ => Line::raw(""),
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
        let hint = match (&self.typing, &self.entry, self.focus) {
            (Some(_), _, _) => "Enter set · blank unsets · Esc cancel · Tab complete ",
            (None, Some(_), _) => "Space kind · + glob · - remove · Esc back · ? help ",
            (None, None, Focus::Tables) => "+ profile · - remove · K/J move · ^S save · ? help ",
            (None, None, Focus::Keys) => "+ add · - remove · u unset · ^S save · ? help ",
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

/// A key's value, or what it means unset, dimmed.
fn shown_line(shown: &Shown, key: &Key) -> Line<'static> {
    let field = key.field();
    match shown {
        Shown::Set(value) => Line::raw(value.clone()),
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
        Shown::Required => Line::from(Span::styled(
            "required: + adds an entry",
            Style::new().fg(Color::Red),
        )),
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("{n} {what}")
    } else {
        format!("{n} {what}s")
    }
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
    ("j/k, g/G", "move, first, last"),
    ("Enter", "edit, toggle true/false, or open a repo entry"),
    ("Space", "toggle true/false, or an entry's kind"),
    ("+", "add a list item, repo entry, glob or profile"),
    ("-", "remove it; a profile asks first"),
    ("K/J", "move an item, entry or profile up or down"),
    ("u", "unset the key, back to its default"),
    ("v", "show the changes"),
    ("Ctrl-S", "save, after showing the changes"),
    ("Esc, q", "back out of an entry, or leave"),
    ("Ctrl-C", "quit"),
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
    lines.push(Line::raw(" Typing: Enter sets, a blank unsets or removes, Esc cancels,").dim());
    lines.push(Line::raw(" Ctrl-U clears, Tab completes a path.").dim());
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
