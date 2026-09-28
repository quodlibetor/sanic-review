//! Drawing the config editor.

use std::{fmt::Write, path::Path};

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
    counts::{EntryId, EntryPlan, Stopped, Tally},
    rows::{EntryEdit, EntryRow, KINDS, Row, Shown, entries, entry_text, items, kind_label, shown},
    suggest::{Suggest, What},
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
        let tally = Tally(&self.answers);
        let bounded =
            self.watched.is_some() && (self.plan.globs || tally.excludes_a_team(&self.plan));
        let [body, counts, note, help, status] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(u16::from(bounded)),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        self.render_counts(frame, counts);
        frame.render_widget(
            Paragraph::new(
                " ≤: a search can't apply path globs or the team filter, so these can be fewer"
                    .dim(),
            ),
            note,
        );
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
            Some(Popup::Suggest(suggest)) => self.render_suggest(frame, suggest),
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
        let area = area.inner(Margin::new(1, 0));
        let tally = Tally(&self.answers);
        for row in self.rows() {
            let label = match (&row, row.key()) {
                (Row::Name(_), _) => "name",
                (_, Some(key)) if row.labelled() => key.name,
                _ => "",
            };
            let mut spans = vec![Span::raw(format!(" {label:<width$}"))];
            spans.extend(self.row_value(&row).spans);
            let counted = match &row {
                // Each entry's repos and PRs, found by what it covers, so
                // they follow it as it moves; one the config didn't have
                // when it last loaded shows none.
                Row::Entry(profile, n) => self.planned_entry(profile, *n).map(|entry| {
                    let (repos, prs) = tally.entry(entry);
                    format!("{:>6} {:>6} ", repos.text(), prs.text())
                }),
                Row::Scalar(key) if key.name == "updated_within_days" => tally
                    .prs(&self.plan.older)
                    .map(|n| format!("hides {n} older ")),
                _ => None,
            };
            if let Some(counted) = counted {
                // The value gives way to the counts.
                let room = usize::from(area.width).saturating_sub(counted.chars().count() + 1);
                truncate(&mut spans, room);
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                spans.push(Span::raw(" ".repeat(room + 1 - used.min(room + 1))));
                spans.push(counted.dim());
            }
            lines.push(ListItem::new(Line::from(spans)));
        }
        if !self.plan.entries.is_empty() {
            let header = format!("{:>6} {:>6} ", "repos", "PRs");
            let room = usize::from(area.width).saturating_sub(header.chars().count());
            lines[1] = ListItem::new(Line::from(vec![Span::raw(" ".repeat(room)), header.dim()]));
        }
        if let (Table::ReviewRequests, Some(teams)) = (&table, tally.teams()) {
            let filter = sanic_core::config::TeamFilter::new(self.plan.teams.clone()).ok();
            lines.push(ListItem::new(""));
            lines.push(ListItem::new(Line::from(
                format!(
                    " {:<width$}requests, as the window counts them",
                    "your teams"
                )
                .dim(),
            )));
            for team in teams {
                let requests = self
                    .plan
                    .team_requests
                    .iter()
                    .find(|(t, _)| t == team)
                    .and_then(|(_, q)| tally.prs(std::slice::from_ref(q)))
                    .map_or_else(|| "…".to_owned(), |n| n.to_string());
                let excluded = filter.as_ref().is_some_and(|f| !f.allows(team));
                let mut spans = vec![Span::raw(format!(
                    " {:<width$}{team:<32} {requests:>5}",
                    ""
                ))];
                if excluded {
                    spans.push("  excluded".dim());
                }
                lines.push(ListItem::new(Line::from(spans)));
            }
        }
        let mut state = ListState::default().with_selected(Some(self.row + 2));
        frame.render_stateful_widget(
            List::new(lines).highlight_style(highlight(self.focus == Focus::Keys)),
            area,
            &mut state,
        );
    }

    /// The counts planned for a profile's `n`th entry in the file, if the
    /// config had it when it last loaded. Entries that cover the same
    /// thing, such as one checkout read through two remotes, pair up in
    /// order.
    pub(super) fn planned_entry(&self, profile: &str, n: usize) -> Option<&EntryPlan> {
        let base = self.path.parent().unwrap_or(Path::new("."));
        let ids: Vec<Option<EntryId>> = entries(&self.doc, profile)
            .into_iter()
            .take(n + 1)
            .map(|entry| EntryId::of(&entry.ok()?, base))
            .collect();
        let (id, before) = ids.split_last()?;
        let id = id.as_ref()?;
        let nth = before
            .iter()
            .filter(|other| other.as_ref() == Some(id))
            .count();
        self.plan
            .entries
            .iter()
            .filter(|planned| planned.id == *id)
            .nth(nth)
    }

    /// The counts line: what the config as it last loaded watches, and how
    /// counting is going.
    fn render_counts(&self, frame: &mut Frame<'_>, area: Rect) {
        // Nothing's planned until the text has loaded once.
        if self.wanted.is_empty() {
            return;
        }
        let tally = Tally(&self.answers);
        let plan = &self.plan;
        let asked_in = tally.asked_in(plan);
        let mut text = format!(
            " {} repos · you owe {}, in {} repo{} · yours {}",
            tally.repos(plan).text(),
            tally.owed(plan).text(),
            asked_in.text(),
            if asked_in.value == Some(1) { "" } else { "s" },
            tally.yours(plan).text(),
        );
        let window = self.watched.as_ref().and_then(|w| w.window);
        match window {
            Some(days) => {
                let _ = write!(text, " · {days} days");
            }
            None => text.push_str(" · any age"),
        }
        if let Some(super::Serving(serving)) = self.serve_window.filter(|s| s.0 != window) {
            match serving {
                Some(days) => {
                    let _ = write!(text, " (serve: {days})");
                }
                None => text.push_str(" (serve: any age)"),
            }
        }
        let state = match (&self.stopped, &self.check) {
            (Some(Stopped::RateLimited(wait)), _) => Span::styled(
                format!("rate limited for {}s ", wait.as_secs()),
                Style::new().fg(Color::Yellow),
            ),
            (Some(Stopped::Unauthorized), _) => Span::styled(
                "token rejected: gh auth login ",
                Style::new().fg(Color::Red),
            ),
            // A failure stays until it's counted again, but not over a wait
            // that's holding that up.
            (None, _)
                if !self.waiting
                    && let Some(why) = tally.failure(&self.wanted) =>
            {
                Span::styled(
                    format!("counting failed: {why} "),
                    Style::new().fg(Color::Red),
                )
            }
            (None, Check::Fails(_)) => "config doesn't load ".dim(),
            (None, _) if self.waiting => "waiting out serve's rate limit ".dim(),
            (None, _) if !tally.done(&self.wanted) => "counting… ".dim(),
            (None, _) => Span::raw(""),
        };
        let [left, right] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(u16::try_from(state.content.chars().count()).unwrap_or(u16::MAX)),
        ])
        .areas(area);
        let counts = if matches!(self.check, Check::Fails(_)) {
            Span::raw(text).dim()
        } else {
            Span::raw(text)
        };
        frame.render_widget(Paragraph::new(counts), left);
        frame.render_widget(Paragraph::new(state), right);
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
            _ if self.saving => Span::raw(" … saving").dim(),
            Check::Checking => Span::raw(" … checking").dim(),
            Check::Loads => Span::styled(" ✓ loads", Style::new().fg(Color::Green)),
            Check::Fails(_) => Span::styled(" ✗ doesn't load", Style::new().fg(Color::Red)),
        };
        let hint = match (&self.typing, &self.entry, self.focus) {
            (Some(_), _, _) => "Enter set · blank unsets · Esc cancel · Tab complete ",
            (None, Some(_), _) => "Space kind · + glob · - remove · Esc back · ? help ",
            (None, None, Focus::Tables) => "+ profile · - remove · K/J move · ^S save · ? help ",
            (None, None, Focus::Keys) => "+ add · - remove · f find · u unset · ^S save · ? ",
        };
        let [left, right] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(u16::try_from(hint.chars().count()).unwrap_or(u16::MAX)),
        ])
        .areas(status);
        frame.render_widget(Paragraph::new(state), left);
        frame.render_widget(Paragraph::new(hint.dim()), right);
    }

    fn render_suggest(&self, frame: &mut Frame<'_>, suggest: &Suggest) {
        let area = frame.area().inner(Margin::new(3, 2));
        frame.render_widget(Clear, area);
        let title = match &suggest.what {
            What::Teams { .. } => " Count review requests to these teams ".to_owned(),
            What::Repos { profile } => format!(" Watch in [profile.{profile}] "),
            What::Model { key } => format!(" {key} "),
            What::Skills { profile } => format!(" Skills for [profile.{profile}] "),
            What::Instructions { profile } => format!(" Instructions for [profile.{profile}] "),
        };
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let repos = matches!(suggest.what, What::Repos { .. });
        let [head, body, footer] = Layout::vertical([
            Constraint::Length(u16::from(repos) * 2),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        if repos {
            let root = self
                .typed(&Input::ScanRoot)
                .unwrap_or_else(|| Line::raw(suggest.root.clone()));
            let mut spans = vec![Span::raw(" checkouts under ")];
            spans.extend(root.spans);
            spans.push(Span::raw(format!(", {} deep", suggest.depth)));
            if suggest.finding {
                spans.push("  scanning…".dim());
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), head);
        }
        let items: Vec<ListItem<'_>> = suggest
            .rows
            .iter()
            .map(|row| {
                let mark = match (suggest.picks_one(), row.on) {
                    (true, _) => "",
                    (false, true) => "[x] ",
                    (false, false) => "[ ] ",
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {mark}{}  ", row.label)),
                    row.detail.clone().dim(),
                ]))
            })
            .collect();
        let empty = if suggest.finding {
            " looking…"
        } else if repos {
            " s scans for checkouts"
        } else {
            " nothing new to suggest"
        };
        if items.is_empty() {
            frame.render_widget(Paragraph::new(empty.dim()), body);
        } else {
            let mut state = ListState::default().with_selected(Some(suggest.cursor));
            frame.render_stateful_widget(
                List::new(items).highlight_style(highlight(true)),
                body,
                &mut state,
            );
        }
        let hint = match &suggest.what {
            What::Model { .. } => " Enter set · Esc cancel",
            What::Teams { .. } => " Space tick · Enter write · Esc cancel",
            What::Repos { .. } => {
                " Space pick · Enter add · s scan · d directory · </> depth · Esc cancel"
            }
            _ => " Space pick · Enter add · Esc cancel",
        };
        frame.render_widget(Paragraph::new(hint.dim()), footer);
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

/// Cuts `spans` to `width` characters, ending in `…` when cut.
fn truncate(spans: &mut Vec<Span<'static>>, width: usize) {
    let mut left = width;
    for (i, span) in spans.iter_mut().enumerate() {
        let len = span.content.chars().count();
        if len <= left {
            left -= len;
            continue;
        }
        let kept: String = span.content.chars().take(left.saturating_sub(1)).collect();
        span.content = format!("{kept}…").into();
        spans.truncate(i + 1);
        return;
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
    ("f", "suggest teams, repos, models, skills or instructions"),
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
