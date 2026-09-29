//! Drawing the config editor: the config as a file, commented with what
//! each key does, then what the config does, then the keys.

use std::{fmt::Write, path::Path};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use sanic_core::config::contract_path;

use super::{
    Check, ConfigEditor, Input, Popup, Typing,
    counts::{Bound, EntryId, EntryPlan, Stopped, Tally},
    rows::{EntryEdit, EntryRow, KINDS, Row, Shown, entries, entry_text, kind_label, shown},
    suggest::{Suggest, What},
};
use crate::config_doc::{
    EntryKind, Key, Scalar, Setting, Table,
    schema::{Fallback, Kind},
};

/// The schema's help: dimmer than the file's own comments, and slanted.
const HELP_STYLE: Style = Style::new()
    .fg(Color::DarkGray)
    .add_modifier(Modifier::ITALIC);
/// The file's own comments.
pub(super) const COMMENT: Style = Style::new().fg(Color::Gray);
/// A key the file doesn't set, as it would read commented out.
const UNSET: Style = Style::new().fg(Color::DarkGray);
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);
const ERROR: Style = Style::new().fg(Color::Red);

/// A line of the file view, and the row it shows, if any.
pub(super) struct FileLine {
    pub(super) line: Line<'static>,
    row: Option<usize>,
}

impl ConfigEditor {
    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let mut title = format!(" {}", contract_path(&self.path));
        if let Some(target) = &self.target {
            let _ = write!(title, " → {}", contract_path(target));
        }
        title.push(' ');
        let mut outer = Block::bordered().title(title);
        if self.doc.is_changed() {
            let changes = self.doc.ops().len();
            outer = outer.title_top(Line::from(format!(" {changes} unsaved ")).right_aligned());
        }
        let inner = outer.inner(area);
        frame.render_widget(outer, area);

        let (below_title, below) = match self.help_lines() {
            Some((key, lines)) => (format!(" {key} "), lines),
            None => (" What this config does ".to_owned(), self.effects()),
        };
        // Wrapped, each line takes as many rows as its text needs.
        let text_width = usize::from(inner.width.saturating_sub(2)).max(1);
        let below = hang_bullets(below, text_width);
        let wrapped: usize = below
            .iter()
            .map(|line| line.width().div_ceil(text_width).max(1))
            .sum();
        let below_height = u16::try_from(wrapped).unwrap_or(u16::MAX).clamp(1, 8) + 1;
        let [file, below_area, footer] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(below_height),
            Constraint::Length(2),
        ])
        .areas(inner);
        self.render_file(frame, file.inner(Margin::new(1, 0)));
        frame.render_widget(
            // Untrimmed, so a bullet's later rows keep their indent.
            Paragraph::new(below).wrap(Wrap { trim: false }).block(
                Block::new()
                    .borders(Borders::TOP)
                    .title(below_title)
                    .padding(Padding::horizontal(1)),
            ),
            below_area,
        );
        self.render_footer(frame, footer);
        if let Some(edit) = &self.entry {
            self.render_entry(frame, edit);
        }
        let changes = self.doc.ops().len();
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

    /// Whether `key`'s list is being typed into.
    fn typing_into(&self, key: &Key) -> bool {
        matches!(
            &self.typing,
            Some(Typing {
                into: Input::Item(k, _) | Input::NewItem(k),
                ..
            }) if k == key
        )
    }

    /// The config as a file, scrolled to keep the selection in view.
    fn render_file(&self, frame: &mut Frame<'_>, area: Rect) {
        let lines = self.file_lines(usize::from(area.width));
        let height = usize::from(area.height);
        let selected: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.row == Some(self.row))
            .map(|(at, _)| at)
            .collect();
        let mut top = self.top.get();
        if let (Some(&first), Some(&last)) = (selected.first(), selected.last()) {
            // The lines just above a row are its comments: keep them too.
            let from = lines[..first]
                .iter()
                .rposition(|l| l.row.is_some())
                .map_or(0, |at| at + 1);
            if from < top {
                top = from;
            }
            if last >= top + height {
                top = last + 1 - height;
            }
        }
        top = top.min(lines.len().saturating_sub(height));
        self.top.set(top);
        let shown: Vec<Line<'static>> = lines
            .into_iter()
            .skip(top)
            .take(height)
            .map(|l| l.line)
            .collect();
        frame.render_widget(Paragraph::new(shown), area);
    }

    /// Every line of the file view.
    pub(super) fn file_lines(&self, width: usize) -> Vec<FileLine> {
        let rows = self.rows();
        let tally = Tally(&self.answers);
        let mut out: Vec<FileLine> = Vec::new();
        let head = self.doc.head_comments();
        if !head.is_empty() {
            for comment in head {
                out.push(comment_line(&comment, COMMENT));
            }
            out.push(blank());
        }
        let mut at = 0;
        // The table whose rows these are, which its closing comments end.
        let mut shown_table: Option<Table> = None;
        while at < rows.len() {
            let row = &rows[at];
            let selected = at == self.row;
            match row {
                Row::Header(table) => {
                    if let Some(before) = &shown_table {
                        self.unknown_key_lines(before, &mut out);
                        self.closing_lines(before, &mut out);
                    }
                    shown_table = Some(table.clone());
                    self.header_lines(table, at, &mut out);
                    at += 1;
                }
                Row::NewProfile => {
                    if let Some(before) = &shown_table {
                        self.unknown_key_lines(before, &mut out);
                        self.closing_lines(before, &mut out);
                    }
                    self.unknown_lines(&mut out);
                    self.last_lines(at, &mut out);
                    at += 1;
                }
                Row::Scalar(key) => {
                    self.key_comments(key, &mut out);
                    // The window says what it leaves out, once counted.
                    let note = (key.name == "updated_within_days" && !self.plan.older.is_empty())
                        .then(|| tally.prs(&self.plan.older))
                        .flatten()
                        .map(|n| format!("hides {} older", plural_count(n, "PR")));
                    let line = self.scalar_line(key, width, note);
                    out.push(FileLine {
                        line: select(line, selected),
                        row: Some(at),
                    });
                    at += 1;
                }
                Row::Item(key, _) | Row::List(key) | Row::NewItem(key) => {
                    let first = at;
                    while at < rows.len() && rows[at].key().as_ref() == Some(key) {
                        at += 1;
                    }
                    self.key_comments(key, &mut out);
                    self.list_lines(key, first..at, width, &mut out);
                    if key.table == Table::ReviewRequests
                        && key.name == "teams"
                        && self.current_table() == Table::ReviewRequests
                    {
                        self.team_lines(&mut out);
                    }
                }
                Row::Entry(profile, _) | Row::NoEntries(profile) => {
                    let first = at;
                    while at < rows.len()
                        && matches!(&rows[at], Row::Entry(p, _) | Row::NoEntries(p) if p == profile)
                    {
                        at += 1;
                    }
                    if let Some(key) = row.key() {
                        self.key_comments(&key, &mut out);
                    }
                    self.repo_lines(profile, first..at, width, &mut out);
                }
            }
        }
        out
    }

    /// A table's header, with the comments the file has around it.
    fn header_lines(&self, table: &Table, at: usize, out: &mut Vec<FileLine>) {
        let selected = at == self.row;
        if at > 0 {
            out.push(blank());
        }
        let comments = self.doc.table_comments(table);
        for comment in &comments.own {
            out.push(comment_line(comment, COMMENT));
        }
        let mut header = match (table, self.typed_rename(table)) {
            (Table::Profile(_), Some(typed)) => {
                let mut spans = vec![Span::raw("[profile.")];
                spans.extend(typed.spans);
                spans.push(Span::raw("]"));
                Line::from(spans)
            }
            _ => Line::from(format!("[{table}]").bold()),
        };
        if let Some(after) = comments.after {
            header.push_span(Span::styled(format!("  # {after}"), COMMENT));
        }
        out.push(FileLine {
            line: select(header, selected),
            row: Some(at),
        });
    }

    /// The comments that close `table` in the file, set apart by a blank
    /// line as they are there, however far the next table in the file is
    /// from it here.
    fn closing_lines(&self, table: &Table, out: &mut Vec<FileLine>) {
        let closing = self.doc.closing_comments(table);
        if !closing.is_empty() {
            out.push(blank());
            for comment in closing {
                out.push(comment_line(&comment, COMMENT));
            }
        }
    }

    /// The keys a table has that the config doesn't take, as the file
    /// writes them, to edit by hand.
    fn unknown_key_lines(&self, table: &Table, out: &mut Vec<FileLine>) {
        let keys = self.doc.unknown_keys(table);
        if keys.is_empty() {
            return;
        }
        out.push(comment_line(
            "keys this table doesn't take, to fix by hand:",
            HELP_STYLE,
        ));
        for lines in keys {
            out.extend(lines.iter().map(raw_line));
        }
    }

    /// What the file has that the config doesn't take at all, as the file
    /// writes it, to edit by hand, each with the comments that close it.
    fn unknown_lines(&self, out: &mut Vec<FileLine>) {
        let items = self.doc.unknown_items();
        if items.is_empty() {
            return;
        }
        out.push(blank());
        out.push(comment_line(
            "tables and keys the config doesn't take, to fix by hand:",
            HELP_STYLE,
        ));
        for (name, lines) in items {
            out.extend(lines.iter().map(raw_line));
            let closing = self.doc.unknown_closing_comments(&name);
            if !closing.is_empty() {
                out.push(blank());
                out.extend(closing.iter().map(|c| comment_line(c, COMMENT)));
            }
        }
    }

    /// What the file ends with, when it has no tables for it to close,
    /// then the row that adds a profile.
    fn last_lines(&self, at: usize, out: &mut Vec<FileLine>) {
        let selected = at == self.row;
        let end = self.doc.loose_end_comments();
        if !end.is_empty() {
            out.push(blank());
            for comment in end {
                out.push(comment_line(&comment, COMMENT));
            }
        }
        out.push(blank());
        let line = match self.typed(&Input::NewProfile) {
            Some(typed) => {
                let mut spans = vec![Span::raw("[profile.")];
                spans.extend(typed.spans);
                spans.push(Span::raw("]"));
                Line::from(spans)
            }
            None => Line::from(Span::styled("# + adds a profile", HELP_STYLE)),
        };
        out.push(FileLine {
            line: select(line, selected),
            row: Some(at),
        });
    }

    fn typed_rename(&self, table: &Table) -> Option<Line<'static>> {
        match table {
            Table::Profile(name) => self.typed(&Input::Rename(name.clone())),
            _ => None,
        }
    }

    /// A key's help, then the comments the file has above it.
    fn key_comments(&self, key: &Key, out: &mut Vec<FileLine>) {
        out.push(comment_line(key.field().help, HELP_STYLE));
        for comment in self.doc.comments(key).above {
            out.push(comment_line(&comment, COMMENT));
        }
    }

    /// `name = value`, or commented out with what it means unset, and
    /// `note` at the right.
    fn scalar_line(&self, key: &Key, width: usize, note: Option<String>) -> Line<'static> {
        let line = self.scalar_value(key, width, note.as_deref());
        match (note, self.doc.scalar(key)) {
            (Some(note), Setting::Set(_) | Setting::Invalid(_)) => {
                right_align(line, Span::styled(note, UNSET), width)
            }
            _ => line,
        }
    }

    fn scalar_value(&self, key: &Key, width: usize, note: Option<&str>) -> Line<'static> {
        if let Some(typed) = self.typed(&Input::Scalar(key.clone())) {
            let mut spans = vec![Span::raw(format!("{} = ", key.name))];
            spans.extend(typed.spans);
            return Line::from(spans);
        }
        match self.doc.scalar(key) {
            Setting::Set(value) => {
                let mut line = Line::raw(format!("{} = {}", key.name, toml_text(&value)));
                if let Some(after) = self.doc.comments(key).after {
                    line.push_span(Span::styled(format!("  # {after}"), COMMENT));
                }
                line
            }
            Setting::Invalid(raw) => {
                let mut line = Line::from(vec![
                    Span::raw(format!("{} = ", key.name)),
                    Span::styled(
                        format!("{raw}  (not a {})", kind_name(key.field().kind)),
                        ERROR,
                    ),
                ]);
                if let Some(after) = self.doc.comments(key).after {
                    line.push_span(Span::styled(format!("  # {after}"), COMMENT));
                }
                line
            }
            Setting::Unset => self.unset_line(key, width, note),
        }
    }

    /// An unset key, commented out, with what it means and where from,
    /// and `note` after that.
    fn unset_line(&self, key: &Key, width: usize, note: Option<&str>) -> Line<'static> {
        let field = key.field();
        let (value, from) = match shown(&self.doc, key) {
            Shown::Default(value) => (
                quoted(field.kind, &value),
                match &field.fallback {
                    Fallback::Inherits(table, name) => format!("(from {table}.{name})"),
                    _ => "(default)".into(),
                },
            ),
            Shown::Required => {
                return Line::from(Span::styled(
                    format!("# {} = []  required: + adds one", key.name),
                    ERROR,
                ));
            }
            Shown::Set(value) | Shown::Invalid(value) => (value, String::new()),
        };
        let right = match note {
            Some(note) => format!("{from} · {note}"),
            None => from,
        };
        right_align(
            Line::from(Span::styled(format!("# {} = {value}", key.name), UNSET)),
            Span::styled(right, UNSET),
            width,
        )
    }

    /// A list: on one line when it fits and isn't being typed into, else
    /// an item a line. The selected item is marked either way.
    fn list_lines(
        &self,
        key: &Key,
        rows: std::ops::Range<usize>,
        width: usize,
        out: &mut Vec<FileLine>,
    ) {
        let list = match self.doc.list(key) {
            Setting::Set(list) if !list.is_empty() => list,
            Setting::Set(_) if !self.typing_into(key) => {
                self.empty_list_lines(key, None, rows, out);
                return;
            }
            Setting::Invalid(raw) => {
                let mut line = Line::from(vec![
                    Span::raw(format!("{} = ", key.name)),
                    Span::styled(format!("{raw}  (not a list of strings)"), ERROR),
                ]);
                if let Some(after) = self.doc.comments(key).after {
                    line.push_span(Span::styled(format!("  # {after}"), COMMENT));
                }
                out.push(FileLine {
                    line: select(line, rows.contains(&self.row)),
                    row: Some(rows.start),
                });
                return;
            }
            Setting::Unset if !self.typing_into(key) => {
                out.push(FileLine {
                    line: select(self.unset_line(key, width, None), rows.contains(&self.row)),
                    row: Some(rows.start),
                });
                return;
            }
            Setting::Set(_) | Setting::Unset => Vec::new(),
        };
        let quoted: Vec<String> = list.iter().map(|item| toml_string(item)).collect();
        let selected = rows.clone().position(|at| at == self.row);
        let inline = format!("{} = [{}]", key.name, quoted.join(", "));
        // Comments inside it need a line each, so it's never on one then.
        let comments = self.doc.list_comments(key);
        if !self.typing_into(key) && !comments.any() && inline.chars().count() + 2 <= width {
            let mut spans = vec![Span::raw(format!("{} = [", key.name))];
            for (n, item) in quoted.iter().enumerate() {
                if n > 0 {
                    spans.push(Span::raw(", "));
                }
                if selected == Some(n) {
                    spans.push(Span::styled(format!("▸{item}"), SELECTED));
                } else {
                    spans.push(Span::raw(item.clone()));
                }
            }
            spans.push(Span::raw("]"));
            if let Some(after) = self.doc.comments(key).after {
                spans.push(Span::styled(format!("  # {after}"), COMMENT));
            }
            // The one line is the row of whichever of its items is selected.
            out.push(FileLine {
                line: Line::from(spans),
                row: Some(selected.map_or(rows.start, |n| rows.start + n)),
            });
            return;
        }
        out.push(FileLine {
            line: opening(&format!("{} = [", key.name), comments.opening.as_deref()),
            row: None,
        });
        let adding_at = rows.end.saturating_sub(1);
        for (n, at) in rows.enumerate() {
            // An empty list's own row has no item to show: the one being
            // added, last, is its line.
            if n >= list.len() && at != adding_at {
                continue;
            }
            let item = comments.items.get(n);
            for comment in item.into_iter().flat_map(|c| &c.above) {
                out.push(comment_line(comment, COMMENT).indent());
            }
            let mut line = match (
                self.typed(&Input::Item(key.clone(), n)),
                self.typed(&Input::NewItem(key.clone())),
            ) {
                (Some(typed), _) => indented(typed),
                (None, Some(typed)) if n >= list.len() => indented(typed),
                _ => Line::raw(format!("  {},", quoted.get(n).cloned().unwrap_or_default())),
            };
            if let Some(after) = item.and_then(|c| c.after.as_ref()) {
                line.push_span(Span::styled(format!("  # {after}"), COMMENT));
            }
            out.push(FileLine {
                line: select(line, at == self.row),
                row: Some(at),
            });
        }
        for comment in &comments.closing {
            out.push(comment_line(comment, COMMENT).indent());
        }
        let mut close = Line::raw("]");
        if let Some(after) = self.doc.comments(key).after {
            close.push_span(Span::styled(format!("  # {after}"), COMMENT));
        }
        out.push(FileLine {
            line: close,
            row: None,
        });
    }

    /// An empty list, `note` after it: `name = []`, or over lines when the
    /// file has comments inside it.
    fn empty_list_lines(
        &self,
        key: &Key,
        note: Option<Span<'static>>,
        rows: std::ops::Range<usize>,
        out: &mut Vec<FileLine>,
    ) {
        let selected = rows.contains(&self.row);
        let comments = self.doc.list_comments(key);
        let after = self.doc.comments(key).after;
        if comments.opening.is_none() && comments.closing.is_empty() {
            let mut line = opening(&format!("{} = []", key.name), after.as_deref());
            line.spans.extend(note);
            out.push(FileLine {
                line: select(line, selected),
                row: Some(rows.start),
            });
            return;
        }
        let mut line = opening(&format!("{} = [", key.name), comments.opening.as_deref());
        line.spans.extend(note);
        out.push(FileLine {
            line: select(line, selected),
            row: Some(rows.start),
        });
        for comment in &comments.closing {
            out.push(comment_line(comment, COMMENT).indent());
        }
        out.push(FileLine {
            line: opening("]", after.as_deref()),
            row: None,
        });
    }

    /// Your teams under `teams`, with their requests and whether the
    /// filter counts them.
    fn team_lines(&self, out: &mut Vec<FileLine>) {
        let tally = Tally(&self.answers);
        let Some(teams) = tally.teams() else {
            return;
        };
        let filter = sanic_core::config::TeamFilter::new(self.plan.teams.clone()).ok();
        out.push(comment_line(
            "your teams, and their review requests:",
            HELP_STYLE,
        ));
        for team in teams {
            let requests = self
                .plan
                .team_requests
                .iter()
                .find(|(t, _)| t == team)
                .and_then(|(_, q)| tally.prs(std::slice::from_ref(q)))
                .map_or_else(|| "counting".to_owned(), |n| plural(n as usize, "request"));
            let counts = if filter.as_ref().is_some_and(|f| !f.allows(team)) {
                "left out by this filter"
            } else {
                "counted"
            };
            out.push(comment_line(
                &format!("  {team}: {requests}, {counts}"),
                HELP_STYLE,
            ));
        }
    }

    /// `repos = [`, an entry a line with its counts, `]`.
    fn repo_lines(
        &self,
        profile: &str,
        rows: std::ops::Range<usize>,
        width: usize,
        out: &mut Vec<FileLine>,
    ) {
        let all = entries(&self.doc, profile);
        let key = Key::new(Table::Profile(profile.to_owned()), "repos");
        if all.is_empty() {
            let required = Span::styled("  required: + adds one", ERROR);
            match &key {
                Some(key) => self.empty_list_lines(key, Some(required), rows, out),
                None => out.push(FileLine {
                    line: select(
                        Line::from(vec![Span::raw("repos = []"), required]),
                        rows.contains(&self.row),
                    ),
                    row: Some(rows.start),
                }),
            }
            return;
        }
        let tally = Tally(&self.answers);
        let comments = key
            .as_ref()
            .map(|key| self.doc.list_comments(key))
            .unwrap_or_default();
        out.push(FileLine {
            line: opening("repos = [", comments.opening.as_deref()),
            row: None,
        });
        for (n, at) in rows.enumerate() {
            let item = comments.items.get(n);
            for comment in item.into_iter().flat_map(|c| &c.above) {
                out.push(comment_line(comment, COMMENT).indent());
            }
            let text = match all.get(n) {
                Some(Ok(entry)) => Span::raw(format!("  {},", entry_text(entry))),
                Some(Err(raw)) => Span::styled(format!("  {raw},  (edit by hand)"), ERROR),
                None => Span::raw(""),
            };
            let mut line = Line::from(text);
            if let Some(after) = item.and_then(|c| c.after.as_ref()) {
                line.push_span(Span::styled(format!("  # {after}"), COMMENT));
            }
            if let Some(planned) = self.planned_entry(profile, n) {
                let (repos, prs) = tally.entry(planned);
                let counts = format!("{} · {}", repos.words("repo"), prs.words("PR"));
                line = right_align(line, Span::styled(counts, UNSET), width);
            }
            out.push(FileLine {
                line: select(line, at == self.row),
                row: Some(at),
            });
        }
        for comment in &comments.closing {
            out.push(comment_line(comment, COMMENT).indent());
        }
        let after = key.and_then(|key| self.doc.comments(&key).after);
        out.push(FileLine {
            line: opening("]", after.as_deref()),
            row: None,
        });
    }

    /// The counts planned for a profile's `n`th entry in the file, if the
    /// config had it when it last loaded. Entries covering the same thing,
    /// such as a checkout through two remotes, pair up in order.
    pub(super) fn planned_entry(&self, profile: &str, n: usize) -> Option<&EntryPlan> {
        let base = self.path.parent().unwrap_or(Path::new("."));
        let all = entries(&self.doc, profile);
        let id = |entry: &Result<_, String>| entry.as_ref().ok().and_then(|e| EntryId::of(e, base));
        let this = id(all.get(n)?)?;
        let before = all[..n]
            .iter()
            .filter(|e| id(e).as_ref() == Some(&this))
            .count();
        self.plan
            .entries
            .iter()
            .filter(|planned| planned.id == this)
            .nth(before)
    }

    /// While a field's being typed into, what it's for, in full.
    fn help_lines(&self) -> Option<(String, Vec<Line<'static>>)> {
        let typing = self.typing.as_ref()?;
        let (name, lines) = match &typing.into {
            Input::Rename(name) => (
                format!("profile.{name}"),
                vec![Line::raw(
                    "The profile's name. It keeps its place among the profiles, and so its precedence.",
                )],
            ),
            Input::NewProfile => (
                "a new profile".to_owned(),
                vec![Line::raw(
                    "Its name. It goes after the others, with no repos yet: add some with +.",
                )],
            ),
            Input::ScanRoot => return None,
            Input::Entry(_) => (
                "a repo entry".to_owned(),
                vec![Line::raw(
                    "A checkout's path, or an org or owner/name. Globs keep it to PRs changing those paths, from the repo's root.",
                )],
            ),
            Input::Scalar(key) | Input::Item(key, _) | Input::NewItem(key) => {
                (key.to_string(), self.key_help(key))
            }
        };
        Some((name, lines))
    }

    /// A key's help in full: what it does, what it is unset, and how it's
    /// typed.
    fn key_help(&self, key: &Key) -> Vec<Line<'static>> {
        let field = key.field();
        let mut help = capitalised(field.help);
        help.push('.');
        match (&field.fallback, shown(&self.doc, key)) {
            (Fallback::Inherits(table, name), Shown::Default(value)) => {
                let _ = write!(help, " Unset, it's {table}.{name}'s: {value}.");
            }
            (_, Shown::Default(value)) => {
                let _ = write!(help, " Unset, it's {}.", quoted(field.kind, &value));
            }
            _ => {}
        }
        let typing = match (field.kind, field.paths, key.name) {
            (Kind::Number, _, _) => "Type a whole number; a blank unsets it.",
            (_, _, "model") => {
                "Type a model id, or auto; Tab completes the ones known, f lists them."
            }
            (Kind::List, true, _) => "Type a path; Tab completes it, and a blank removes the item.",
            (Kind::List, false, _) => "Type the item; a blank removes it.",
            (_, true, _) => "Type a path; Tab completes it, and a blank unsets it.",
            _ => "Type it; a blank unsets it.",
        };
        vec![Line::raw(help), Line::from(typing.dim())]
    }

    /// What the config does, a bullet each, from the counts so far, and
    /// how counting is going on a line of its own.
    pub(super) fn effects(&self) -> Vec<Line<'static>> {
        if let Some(why) = &self.offline {
            return vec![Line::from(Span::styled(
                format!("{why}."),
                Style::new().fg(Color::Yellow),
            ))];
        }
        let mut lines = Vec::new();
        if let Check::Fails(why) = &self.check {
            lines.push(Line::from(Span::styled(
                format!("This doesn't load: {why}"),
                ERROR,
            )));
        }
        if self.wanted.is_empty() {
            if lines.is_empty() {
                lines.push(Line::from("Counting starts once the config loads.".dim()));
            }
            return lines;
        }
        // While it doesn't load, these are the counts of the last text that
        // did.
        let style = if matches!(self.check, Check::Fails(_)) {
            UNSET
        } else {
            Style::new()
        };
        for bullet in [self.watches(), self.owed(), self.yours(), self.window()] {
            lines.push(Line::from(Span::styled(format!("· {bullet}"), style)));
        }
        let tally = Tally(&self.answers);
        let state = match &self.stopped {
            Some(Stopped::RateLimited(wait)) => Some(format!(
                "GitHub rate limited the counts; they carry on after {}s, once you edit.",
                wait.as_secs()
            )),
            Some(Stopped::Unauthorized) => {
                Some("GitHub rejected the token, so counting stopped: run `gh auth login`.".into())
            }
            None if self.waiting => Some("Waiting for serve's rate limit to end.".into()),
            None => tally
                .failure(&self.wanted)
                .map(|why| format!("A count failed: {why}")),
        };
        if let Some(state) = state {
            lines.push(Line::from(Span::styled(
                state,
                Style::new().fg(Color::Yellow),
            )));
        } else if !tally.done(&self.wanted) {
            lines.push(Line::from("Still counting…".dim()));
        }
        lines
    }

    /// What a search can't apply, making a count an upper bound.
    fn unsearchable(&self) -> Option<&'static str> {
        let tally = Tally(&self.answers);
        match (self.plan.globs, tally.excludes_a_team(&self.plan)) {
            (true, true) => Some("your path globs or team filter"),
            (true, false) => Some("your path globs"),
            (false, true) => Some("your team filter"),
            (false, false) => None,
        }
    }

    /// `Watches 1,059 repos (1,057 in 2 orgs, 2 named directly)`
    fn watches(&self) -> String {
        let repos = Tally(&self.answers).repos(&self.plan);
        match (repos.value, repos.failed) {
            (_, true) => "Couldn't count the repos it watches".into(),
            (None, false) => "Counting the repos it watches…".into(),
            (Some(total), false) => {
                let named = self.plan.repos;
                let orgs = self.plan.orgs.len();
                let watches = format!("Watches {}", plural_count(total, "repo"));
                match (orgs, named) {
                    (0, _) => format!("{watches} (all named directly)"),
                    (_, 0) => format!("{watches} (in {})", plural(orgs, "org")),
                    _ => format!(
                        "{watches} ({} in {}, {} named directly)",
                        thousands(total.saturating_sub(named)),
                        plural(orgs, "org"),
                        thousands(named)
                    ),
                }
            }
        }
    }

    /// `Matches up to 15 reviews in at least 1 repo (search can't apply
    /// your team filter)`: "up to" only when a search can't apply
    /// something, "at least" only when the repos were cut short.
    fn owed(&self) -> String {
        let tally = Tally(&self.answers);
        let owed = tally.owed(&self.plan);
        let Some(n) = owed.value.filter(|_| !owed.failed) else {
            return if owed.failed {
                "Couldn't count the reviews you're asked for".into()
            } else {
                "Counting the reviews you're asked for…".into()
            };
        };
        let bound = self.unsearchable();
        let mut text = format!(
            "Matches {}{}",
            if bound.is_some() { "up to " } else { "" },
            plural_count(n, "review")
        );
        let asked_in = tally.asked_in(&self.plan);
        match (asked_in.value, asked_in.bound) {
            (Some(repos), Bound::AtLeast) => {
                let _ = write!(text, " in at least {}", plural_count(repos, "repo"));
            }
            (Some(repos), _) => {
                let _ = write!(text, " in {}", plural_count(repos, "repo"));
            }
            (None, _) => {}
        }
        if let Some(what) = bound {
            let _ = write!(text, " (search can't apply {what})");
        }
        text
    }

    /// `Matches 2 of your open PRs`, or `up to` when globs apply.
    fn yours(&self) -> String {
        let yours = Tally(&self.answers).yours(&self.plan);
        match (yours.value, yours.failed) {
            (_, true) => "Couldn't count your open PRs".into(),
            (None, false) => "Counting your open PRs…".into(),
            (Some(n), false) if self.plan.globs => {
                format!(
                    "Matches up to {} of your open PRs (search can't apply your path globs)",
                    thousands(n)
                )
            }
            (Some(n), false) => format!("Matches {} of your open PRs", thousands(n)),
        }
    }

    /// Which PRs count, and which `serve` is showing when that differs.
    fn window(&self) -> String {
        let window = self.watched.as_ref().and_then(|w| w.window);
        let mut text = match window {
            Some(days) => format!("Only PRs updated in the last {days} days count"),
            None => "PRs of any age count".into(),
        };
        if let Some(super::Serving(serving)) = self.serve_window.filter(|s| s.0 != window) {
            match serving {
                Some(days) => {
                    let _ = write!(text, " (serve is showing {days} days)");
                }
                None => text.push_str(" (serve is showing any age)"),
            }
        }
        text
    }

    /// The load state, and the main keys, or what the last key said.
    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::new().borders(Borders::TOP);
        let line = block.inner(area);
        frame.render_widget(block, area);
        let state = match &self.check {
            _ if self.saving => Span::raw(" … saving").dim(),
            Check::Checking => Span::raw(" … checking").dim(),
            Check::Loads => Span::styled(" ✓ loads", Style::new().fg(Color::Green)),
            Check::Fails(_) => Span::styled(" ✗ doesn't load", ERROR),
        };
        let [left, right] =
            Layout::horizontal([Constraint::Length(18), Constraint::Fill(1)]).areas(line);
        let room = usize::from(right.width);
        let keys = match (&self.notice, &self.typing, &self.entry) {
            (Some(notice), _, _) => {
                Line::from(Span::styled(notice.clone(), Style::new().fg(Color::Yellow)))
            }
            (None, Some(_), _) => fitted_hints(
                room,
                &[
                    ("↵", "set"),
                    ("blank", "unsets"),
                    ("Esc", "cancel"),
                    ("Tab", "complete"),
                ],
            ),
            (None, None, Some(_)) => fitted_hints(
                room,
                &[
                    ("Esc", "back"),
                    ("Space", "kind"),
                    ("↵", "edit"),
                    ("+", "glob"),
                    ("-", "remove"),
                    ("u", "unset remote"),
                    ("?", "keys"),
                ],
            ),
            (None, None, None) => fitted_hints(
                room,
                &[
                    ("↵", "edit"),
                    ("+", "add"),
                    ("-", "remove"),
                    ("u", "unset"),
                    ("f", "find"),
                    ("^S", "save"),
                    ("?", "keys"),
                ],
            ),
        };
        frame.render_widget(Paragraph::new(state), left);
        frame.render_widget(Paragraph::new(keys.right_aligned()), right);
    }

    /// The open repo entry, over the file.
    fn render_entry(&self, frame: &mut Frame<'_>, edit: &EntryEdit) {
        let place = entries(&self.doc, &edit.profile)
            .iter()
            .position(|e| e.as_ref() == Ok(&edit.entry))
            .filter(|_| edit.in_doc)
            .map_or_else(|| "a new entry".to_owned(), |n| format!("repos[{n}]"));
        let adding = self.adding_glob();
        let github = edit.entry.kind() == EntryKind::Github;
        let mut items = Vec::new();
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
                            Line::from(Span::styled("required", ERROR))
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
                            None => Line::from(Span::styled("# found from the checkout", UNSET)),
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
                    Line::from(Span::styled(
                        if github {
                            "# none: every PR in it"
                        } else {
                            "# none yet: + adds a glob"
                        },
                        UNSET,
                    )),
                ),
                EntryRow::NewGlob => (
                    "",
                    self.typed(&Input::Entry(row))
                        .unwrap_or_else(|| Line::raw("")),
                ),
            };
            let mut spans = vec![Span::raw(format!(" {label:<8}"))];
            spans.extend(value.spans);
            items.push(ListItem::new(Line::from(spans)));
        }
        // Its keys are in the footer; a line of room above and below.
        let height = u16::try_from(items.len() + 4).unwrap_or(u16::MAX);
        let area = frame
            .area()
            .centered(Constraint::Length(72), Constraint::Length(height));
        frame.render_widget(Clear, area);
        let block = Block::bordered().title(format!(" [profile.{}] {place} ", edit.profile));
        let list = block.inner(area).inner(Margin::new(0, 1));
        frame.render_widget(block, area);
        let mut state = ListState::default().with_selected(Some(edit.row));
        frame.render_stateful_widget(List::new(items).highlight_style(SELECTED), list, &mut state);
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
                List::new(items).highlight_style(SELECTED),
                body,
                &mut state,
            );
        }
        let hint: &[(&str, &str)] = match &suggest.what {
            What::Model { .. } => &[("↵", "set"), ("Esc", "cancel")],
            What::Teams { .. } => &[("Space", "tick"), ("↵", "write"), ("Esc", "cancel")],
            What::Repos { .. } => &[
                ("Space", "pick"),
                ("↵", "add"),
                ("s", "scan"),
                ("d", "directory"),
                ("</>", "depth"),
                ("Esc", "cancel"),
            ],
            _ => &[("Space", "pick"), ("↵", "add"), ("Esc", "cancel")],
        };
        let mut hint = key_hints(hint);
        hint.spans.insert(0, Span::raw(" "));
        frame.render_widget(Paragraph::new(hint), footer);
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

/// `line`, highlighted when it's the selected row's.
fn select(line: Line<'static>, selected: bool) -> Line<'static> {
    if selected {
        line.patch_style(SELECTED)
    } else {
        line
    }
}

fn blank() -> FileLine {
    FileLine {
        line: Line::raw(""),
        row: None,
    }
}

/// A key in the footer and the pick lists: bright, so it stands out
/// from what it does.
const KEY: Style = Style::new().fg(Color::LightGreen);

/// `↵ edit  + add …`: each key bright, what it does dimmed.
/// As many of `pairs` as fit in `room`, dropping whole hints from the
/// right, but never `?`'s, which lists what's dropped.
fn fitted_hints(room: usize, pairs: &[(&str, &str)]) -> Line<'static> {
    let mut kept = pairs.to_vec();
    loop {
        let line = key_hints(&kept);
        if line.width() <= room || kept.len() <= 1 {
            return line;
        }
        let pinned = kept.last().is_some_and(|(key, _)| *key == "?");
        kept.remove(kept.len() - if pinned { 2 } else { 1 });
    }
}

fn key_hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (n, (key, what)) in pairs.iter().enumerate() {
        let gap = if n + 1 == pairs.len() { " " } else { "  " };
        spans.push(Span::styled((*key).to_owned(), KEY));
        spans.push(Span::raw(format!(" {what}{gap}")).dim());
    }
    Line::from(spans)
}

/// A line of something the config doesn't take, as the file writes it,
/// with its comment in the file's comment style.
fn raw_line((code, comment): &(String, Option<String>)) -> FileLine {
    let mut line = Line::from(Span::styled(code.clone(), ERROR));
    if let Some(comment) = comment {
        let gap = if code.trim().is_empty() { "" } else { "  " };
        line.push_span(Span::styled(format!("{gap}# {comment}"), COMMENT));
    }
    FileLine { line, row: None }
}

/// `text`, then the comment the file has after it on its line.
fn opening(text: &str, after: Option<&str>) -> Line<'static> {
    let mut line = Line::raw(text.to_owned());
    if let Some(after) = after {
        line.push_span(Span::styled(format!("  # {after}"), COMMENT));
    }
    line
}

impl FileLine {
    /// Indented as a list's items are.
    fn indent(mut self) -> Self {
        self.line = indented(self.line);
        self
    }
}

fn comment_line(text: &str, style: Style) -> FileLine {
    FileLine {
        line: Line::from(Span::styled(format!("# {text}"), style)),
        row: None,
    }
}

fn indented(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(line.spans);
    Line::from(spans)
}

/// `line` with `right` at the end of `width`, cutting `line` short with
/// `…` when they don't both fit.
fn right_align(line: Line<'static>, right: Span<'static>, width: usize) -> Line<'static> {
    let right_width = right.content.chars().count();
    let room = width.saturating_sub(right_width + 2);
    let mut spans = line.spans;
    truncate(&mut spans, room);
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(used + right_width)),
    ));
    spans.push(right);
    Line::from(spans)
}

/// A value as TOML writes it.
fn toml_text(value: &Scalar) -> String {
    match value {
        Scalar::Text(text) => toml_string(text),
        Scalar::Number(n) => n.to_string(),
        Scalar::Bool(b) => b.to_string(),
    }
}

fn toml_string(text: &str) -> String {
    toml_edit::Value::from(text).to_string()
}

/// A default as TOML would write it: text quoted.
/// `lines` with each bullet wrapped to `width`, its later rows indented
/// under its text rather than under the `·`; other lines are left for the
/// paragraph to wrap.
fn hang_bullets(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    const BULLET: &str = "· ";
    let mut out = Vec::new();
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let Some(rest) = text.strip_prefix(BULLET) else {
            out.push(line);
            continue;
        };
        let style = line.spans.first().map_or_else(Style::new, |s| s.style);
        let indent = BULLET.chars().count();
        let room = width.saturating_sub(indent).max(1);
        let mut rows: Vec<String> = Vec::new();
        for word in rest.split(' ') {
            match rows.last_mut() {
                Some(row) if row.chars().count() + 1 + word.chars().count() <= room => {
                    row.push(' ');
                    row.push_str(word);
                }
                _ => rows.push(word.to_owned()),
            }
        }
        for (n, row) in rows.into_iter().enumerate() {
            let lead = if n == 0 { BULLET } else { "  " };
            out.push(Line::from(Span::styled(format!("{lead}{row}"), style)));
        }
    }
    out
}

fn quoted(kind: Kind, value: &str) -> String {
    match kind {
        Kind::Text => toml_string(value),
        Kind::List if value == "none" => "[]".into(),
        _ => value.to_owned(),
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("{n} {what}")
    } else {
        format!(
            "{} {what}s",
            thousands(u32::try_from(n).unwrap_or(u32::MAX))
        )
    }
}

fn plural_count(n: u32, what: &str) -> String {
    plural(usize::try_from(n).unwrap_or(usize::MAX), what)
}

/// `1,061`.
fn thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
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

const HELP: &[(&str, &str)] = &[
    ("j/k, g/G", "move a row, to the first, the last"),
    ("Tab, ], Shift-Tab, [", "the next table, the one before"),
    (
        "Enter",
        "edit, flip a bool, open a repo entry, rename a profile",
    ),
    ("Space", "flip a bool, or an entry's kind"),
    (
        "+",
        "add a list item, repo entry, glob or, on a header, a profile",
    ),
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
                Span::raw(format!(" {keys:<22}")).bold(),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::raw(" Typing: Enter sets, a blank unsets or removes, Esc cancels,").dim());
    lines.push(Line::raw(" Ctrl-U clears, Tab completes a path or a model.").dim());
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
        .centered(Constraint::Length(76), Constraint::Length(height));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}
