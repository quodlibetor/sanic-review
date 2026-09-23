//! The `i` editor: turn a PR's title into a `skip_titles` glob, previewing
//! which of the reviews you owe it would skip, then pick where it goes.

use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Layout, Margin},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use sanic_core::{
    pr::PrKey,
    skip::{TitleFilter, escape_title},
};
use sanic_store::OwedReview;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreEditor {
    pub key: PrKey,
    title: String,
    body: String,
    pattern: String,
    /// Lines of the description scrolled past.
    scroll: u16,
    step: Step,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Edit,
    /// Choosing where the pattern goes: 0 is `[review_requests]`, then
    /// each profile in order.
    Target {
        selected: usize,
    },
}

/// What a key did to the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Open,
    Cancel,
    Quit,
    /// Add `pattern` to `skip_titles` globally (`None`) or in a profile.
    Save {
        pattern: String,
        profile: Option<String>,
    },
}

impl IgnoreEditor {
    /// Starts with a pattern that matches exactly `pr`'s title, to edit
    /// down.
    #[must_use]
    pub fn new(pr: &OwedReview) -> Self {
        Self {
            key: pr.key.clone(),
            title: pr.title.clone(),
            body: pr.body.clone(),
            pattern: escape_title(&pr.title),
            scroll: 0,
            step: Step::Edit,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, profiles: &[String]) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, self.step) {
            (KeyCode::Char('c'), _) if ctrl => return Outcome::Quit,
            (KeyCode::Esc, _) => return Outcome::Cancel,
            (KeyCode::Enter, Step::Edit) => {
                if TitleFilter::check_pattern(&self.pattern).is_ok() && !self.pattern.is_empty() {
                    self.step = Step::Target { selected: 0 };
                }
            }
            (KeyCode::Char('u'), Step::Edit) if ctrl => self.pattern.clear(),
            (KeyCode::Char(c), Step::Edit) if !ctrl => self.pattern.push(c),
            (KeyCode::Backspace, Step::Edit) => {
                self.pattern.pop();
            }
            (KeyCode::Up | KeyCode::PageUp, Step::Edit) => {
                self.scroll = self.scroll.saturating_sub(page(key.code));
            }
            (KeyCode::Down | KeyCode::PageDown, Step::Edit) => {
                self.scroll = self.scroll.saturating_add(page(key.code));
            }
            (KeyCode::Up | KeyCode::Char('k'), Step::Target { selected }) => {
                self.step = Step::Target {
                    selected: selected.saturating_sub(1),
                };
            }
            (KeyCode::Down | KeyCode::Char('j'), Step::Target { selected }) => {
                self.step = Step::Target {
                    selected: (selected + 1).min(profiles.len()),
                };
            }
            (KeyCode::Enter, Step::Target { selected }) => {
                return Outcome::Save {
                    pattern: self.pattern.clone(),
                    profile: selected
                        .checked_sub(1)
                        .and_then(|i| profiles.get(i))
                        .cloned(),
                };
            }
            _ => {}
        }
        Outcome::Open
    }

    pub fn render(&self, frame: &mut Frame<'_>, owed: &[OwedReview], profiles: &[String]) {
        let area = frame.area().inner(Margin::new(2, 1));
        frame.render_widget(Clear, area);
        let outer = Block::bordered().title(" Ignore by title ");
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        let [title, body, pattern, matches, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new(self.title.as_str()).block(Block::bordered().title(" Title ")),
            title,
        );
        let description = if self.body.trim().is_empty() {
            Paragraph::new("No description.".dim())
        } else {
            Paragraph::new(self.body.as_str())
        };
        frame.render_widget(
            description
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(Block::bordered().title(" Description ")),
            body,
        );

        let preview = preview(&self.pattern, owed);
        let pattern_block = match &preview {
            Ok(_) => Block::bordered().title(" Pattern "),
            Err(err) => Block::bordered()
                .border_style(Style::new().fg(Color::Red))
                .title(format!(" Pattern: {err} ")),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(self.pattern.as_str()),
                "▏".dim(),
            ]))
            .block(pattern_block),
            pattern,
        );

        let (rows, heading) = match &preview {
            Ok(hits) => (
                hits.iter()
                    .map(|pr| {
                        ListItem::new(Line::from(vec![
                            Span::raw(pr.key.url()),
                            Span::raw("  "),
                            Span::raw(pr.title.as_str()),
                        ]))
                    })
                    .collect(),
                format!(" Would skip ({}) ", hits.len()),
            ),
            Err(_) => (Vec::new(), " Would skip ".into()),
        };
        frame.render_widget(
            List::new(rows).block(Block::bordered().title(heading)),
            matches,
        );
        frame.render_widget(
            Paragraph::new(
                " Enter save · Esc cancel · ↑/↓ scroll description · Ctrl-U clear".dim(),
            ),
            footer,
        );

        if let Step::Target { selected } = self.step {
            self.render_target(frame, profiles, selected);
        }
    }

    fn render_target(&self, frame: &mut Frame<'_>, profiles: &[String], selected: usize) {
        let mut options = vec![ListItem::new(" [review_requests]: every profile")];
        options.extend(
            profiles
                .iter()
                .map(|p| ListItem::new(format!(" [profile.{p}]"))),
        );
        let height = u16::try_from(options.len() + 4).unwrap_or(u16::MAX);
        let area = frame
            .area()
            .centered(Constraint::Length(56), Constraint::Length(height));
        frame.render_widget(Clear, area);
        let block = Block::bordered().title(format!(" Add `{}` to ", self.pattern));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [list, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(
            List::new(options).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            list,
            &mut state,
        );
        frame.render_widget(Paragraph::new(" Enter choose · Esc cancel".dim()), footer);
    }
}

fn page(code: KeyCode) -> u16 {
    if matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
        10
    } else {
        1
    }
}

/// The reviews `pattern` would skip, or why it isn't a valid glob.
pub fn preview<'a>(pattern: &str, owed: &'a [OwedReview]) -> Result<Vec<&'a OwedReview>, String> {
    if pattern.is_empty() {
        return Err("empty".into());
    }
    let filter = TitleFilter::new(vec![pattern.to_owned()]).map_err(|err| {
        // The glob crate's own message, without our context line.
        err.root_cause().to_string()
    })?;
    Ok(owed
        .iter()
        .filter(|pr| filter.first_match(&pr.title).is_some())
        .collect())
}

#[cfg(test)]
mod tests {
    use sanic_core::repo::RepoName;

    use super::*;

    fn owed(number: u32, title: &str) -> OwedReview {
        OwedReview {
            key: PrKey {
                repo: RepoName::new("org", "repo"),
                number,
            },
            title: title.into(),
            body: String::new(),
            author: "a".into(),
            profile: "p".into(),
            is_draft: false,
            archived: false,
            latest_run: None,
            pending_drafts: 0,
        }
    }

    #[test]
    fn preview_lists_matching_titles_or_the_glob_error() {
        let owed = [
            owed(1, "build(deps): bump serde"),
            owed(2, "Build(deps): bump tokio"),
            owed(3, "fix: build deps"),
        ];
        let numbers = |pattern: &str| -> Vec<u32> {
            preview(pattern, &owed)
                .unwrap()
                .iter()
                .map(|pr| pr.key.number)
                .collect()
        };
        assert_eq!(numbers("build(deps)*"), [1, 2]);
        assert_eq!(numbers(&escape_title("fix: build deps")), [3]);
        assert_eq!(numbers("nothing"), Vec::<u32>::new());
        assert!(preview("[", &owed).is_err());
        assert!(preview("", &owed).is_err());
    }
}
