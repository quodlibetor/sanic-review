//! The `e` editor: every key of the config file, edited in place with its
//! comments and layout kept, checked with the loader after each edit, and
//! written only once it loads and you've seen the diff.

mod check;
mod complete;
pub mod counts;
pub mod discover;
pub mod field;
pub mod host;
mod menu;
mod render;
mod rows;
pub mod standalone;
mod suggest;
mod vi;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

use color_eyre::eyre::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sanic_core::{
    config::{DEFAULT_MANUAL_REVIEWS, Keys, contract_path},
    manual::ManualReviews,
};

pub use self::check::{Checked, Checker};
use self::counts::{Answer, Counted, Plan, Query, Showing, Stopped, Tally, Watched};
use self::field::{TextField, Took};
use self::menu::{Menu, matching};
use self::rows::{
    EntryEdit, EntryRow, KINDS, Row, Shown, all_rows, entries, items, scalar_text, shown,
};
use self::suggest::{Apply, Suggest, Suggestion, What};
pub use self::suggest::{Find, Found};
use crate::config_doc::{
    ConfigDoc, Key, Op, RepoEntry, Saved, Scalar, Setting, Table,
    schema::{Fallback, Kind},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEditor {
    path: PathBuf,
    /// The file `path` links to, when it's a symlink.
    target: Option<PathBuf>,
    doc: ConfigDoc,
    /// Which row, of every table's in turn.
    row: usize,
    /// The first line of the file view shown, kept between draws so it
    /// scrolls only as far as the selection needs.
    top: std::cell::Cell<usize>,
    /// The repo entry open in place of its profile's keys.
    entry: Option<EntryEdit>,
    typing: Option<Typing>,
    check: Check,
    /// Counts the texts sent to be checked, so a late answer about an
    /// older one is ignored.
    generation: u64,
    popup: Option<Popup>,
    /// Shown in the status line until the next key.
    notice: Option<String>,
    /// What the text watched when it last loaded, which the counts count.
    watched: Option<Watched>,
    /// What's been counted, by what was asked.
    answers: HashMap<Query, Answer>,
    /// What [`ConfigEditor::want`] last asked for, and why.
    wanted: Vec<Query>,
    plan: Plan,
    /// Why counting stopped, until the next answer.
    stopped: Option<Stopped>,
    /// Counting waits for `serve`'s rate limit to end.
    waiting: bool,
    /// The recency window `serve` is showing, when it's running.
    serve_window: Option<Serving>,
    /// A save is being written.
    saving: bool,
    /// Models to suggest and complete.
    models: Vec<String>,
    /// The keys text fields take when the config doesn't say.
    guessed_keys: Keys,
    /// Each profile's skills and instruction files, found for the
    /// dropdown; `None` while they're being found. Edits forget them.
    extras: HashMap<String, Option<Extras>>,
    /// Why nothing's counted, when that's so from the start.
    offline: Option<String>,
    /// A save has been written.
    wrote: bool,
}

/// The recency window `serve` shows, in days; `None` is any age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Serving(Option<u32>);

/// Whether the edited text loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Checking,
    Loads,
    Fails(String),
}

/// A profile's skills and instruction files, as the config would name
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Extras {
    skills: Vec<String>,
    instructions: Vec<String>,
}

/// What you're typing, and into what.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Typing {
    into: Input,
    field: TextField,
    menu: Menu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Input {
    Scalar(Key),
    Item(Key, usize),
    NewItem(Key),
    /// A new name for this profile.
    Rename(String),
    NewProfile,
    /// A field of the open repo entry.
    Entry(EntryRow),
    /// Where `f` scans for checkouts.
    ScanRoot,
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
    /// Waiting for a yes before removing this profile.
    RemoveProfile(String),
    /// `f`'s suggestions for a key.
    Suggest(Box<Suggest>),
}

/// Shown on arriving at a profile's header: `Config::match_pr`'s rule.
const PRECEDENCE_HINT: &str = "most specific match wins, then first in the file";

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
    /// Find this and hand it to [`ConfigEditor::found`].
    Find(Find),
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
            row: 0,
            top: std::cell::Cell::new(0),
            entry: None,
            typing: None,
            check: Check::Checking,
            generation: 0,
            popup: None,
            notice: None,
            watched: None,
            answers: HashMap::new(),
            wanted: Vec::new(),
            plan: Plan::default(),
            stopped: None,
            waiting: false,
            serve_window: None,
            saving: false,
            models: Vec::new(),
            guessed_keys: Keys::Emacs,
            extras: HashMap::new(),
            offline: None,
            wrote: false,
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

    /// The answer to [`Outcome::Check`] for `generation`. While the text
    /// doesn't load, the counts stay those of the last that did.
    pub fn checked(&mut self, generation: u64, checked: Checked) {
        if generation == self.generation {
            self.check = match checked {
                Ok(watched) => {
                    self.watched = Some(watched);
                    Check::Loads
                }
                Err(why) => Check::Fails(why),
            };
        }
    }

    /// What to count now, most wanted first: the headline's, then what's
    /// on screen. Nothing until the text has loaded once.
    pub fn want(&mut self, now: SystemTime) -> Vec<Query> {
        let Some(watched) = &self.watched else {
            return Vec::new();
        };
        let table = self.current_table();
        let teams = Tally(&self.answers)
            .teams()
            .map(<[_]>::to_vec)
            .unwrap_or_default();
        let showing = match &table {
            Table::Profile(name) => Showing::Profile(name),
            Table::ReviewRequests => Showing::ReviewRequests(&teams),
            Table::Poll => Showing::Poll,
            Table::Github | Table::Runner | Table::Tui => Showing::Other,
        };
        self.plan = Plan::new(watched, showing, now);
        self.wanted = self.plan.queries();
        self.wanted.clone()
    }

    /// Answers from the counter.
    pub fn counted(&mut self, counted: Vec<Counted>) {
        // Teams, orgs and repos come in with the answers.
        let answered = counted.iter().any(|c| matches!(c, Counted::Answer(..)));
        for counted in counted {
            match counted {
                Counted::Answer(query, answer) => {
                    self.answers.insert(query, answer);
                    self.stopped = None;
                    self.waiting = false;
                }
                // Shown until that search is counted again, whatever else
                // comes in meanwhile. GitHub answered, so counting isn't
                // stopped.
                Counted::Failed(query, why) => {
                    self.answers.insert(query, Answer::Failed(why));
                    self.stopped = None;
                    self.waiting = false;
                }
                Counted::Stopped(stopped) => {
                    self.stopped = Some(stopped);
                    self.waiting = false;
                }
                Counted::Waiting => self.waiting = true,
            }
        }
        if answered {
            self.refresh_menu();
        }
    }

    /// The window `serve` is showing, to say so when the edited one
    /// differs.
    pub fn set_serve_window(&mut self, days: Option<u32>) {
        self.serve_window = Some(Serving(days));
    }

    /// Nothing will be counted, and GitHub suggests nothing, for `why`.
    pub fn set_offline(&mut self, why: &str) {
        self.offline = Some(why.to_owned());
    }

    /// Whether a save has been written.
    #[must_use]
    pub fn wrote(&self) -> bool {
        self.wrote
    }

    /// Whether a save has been sent and not yet come back.
    #[must_use]
    pub fn is_saving(&self) -> bool {
        self.saving
    }

    /// The models `f` suggests and the dropdown offers for a `model`.
    pub fn set_models(&mut self, models: Vec<String>) {
        self.models = models;
    }

    /// The keys text fields take when the config doesn't say; see
    /// [`guess_keys`].
    pub fn set_guessed_keys(&mut self, keys: Keys) {
        self.guessed_keys = keys;
    }

    /// What [`Outcome::Find`] found.
    pub fn found(&mut self, found: Found) {
        if let Found::Extras {
            profile,
            skills,
            instructions,
        } = &found
            && self.extras.get(profile) == Some(&None)
        {
            let extras = Extras {
                skills: skills.iter().map(|s| contract_path(&s.dir)).collect(),
                instructions: instructions.iter().map(|p| contract_path(p)).collect(),
            };
            self.extras.insert(profile.clone(), Some(extras));
            self.refresh_menu();
        }
        let Some(Popup::Suggest(suggest)) = &mut self.popup else {
            if let Found::Failed(why) = found {
                self.notice = Some(why);
            }
            return;
        };
        // A find outlives the popup that asked for it: what a closed one
        // asked for isn't this one's to show.
        let asked = suggest.finding
            && match (&found, &suggest.what) {
                (Found::Checkouts(_), What::Repos { .. }) | (Found::Failed(_), _) => true,
                (
                    Found::Extras { profile, .. },
                    What::Skills { profile: for_ } | What::Instructions { profile: for_ },
                ) => profile == for_,
                _ => false,
            };
        if !asked {
            return;
        }
        suggest.finding = false;
        let present = present_entries(&self.doc, &self.path);
        match found {
            Found::Checkouts(checkouts) => {
                for found in checkouts {
                    let label = contract_path(&found.path);
                    let org = found.repo.owner.to_ascii_lowercase();
                    if !present.orgs.contains(&org) && !suggest.rows.iter().any(|r| r.label == org)
                    {
                        suggest
                            .rows
                            .push(org_row(&org, "an owner of checkouts found"));
                    }
                    // A scan again, deeper, finds what the last one did.
                    if !present.checkouts.contains(&found.path)
                        && !suggest.rows.iter().any(|r| r.label == label)
                    {
                        suggest.rows.push(Suggestion {
                            label: label.clone(),
                            detail: found.repo.to_string(),
                            on: false,
                            apply: Apply::Entry(RepoEntry::Checkout {
                                path: label,
                                remote: None,
                            }),
                        });
                    }
                }
            }
            Found::Extras {
                skills,
                instructions,
                ..
            } => match &suggest.what {
                What::Skills { .. } => {
                    suggest.rows = skills
                        .into_iter()
                        .map(|skill| Suggestion {
                            label: skill.name,
                            detail: format!(
                                "{}  {}",
                                skill.description.unwrap_or_default(),
                                contract_path(&skill.dir)
                            ),
                            on: false,
                            apply: Apply::Item(contract_path(&skill.dir)),
                        })
                        .collect();
                }
                _ => {
                    suggest.rows = instructions
                        .into_iter()
                        .map(|file| Suggestion {
                            label: contract_path(&file),
                            detail: String::new(),
                            on: false,
                            apply: Apply::Item(contract_path(&file)),
                        })
                        .collect();
                }
            },
            Found::Failed(why) => self.notice = Some(why),
        }
    }

    /// What to find for `profile`'s skills and instruction files: what
    /// its entries check out, less what it lists.
    fn find_extras(&self, profile: String) -> Find {
        let listed = |name| {
            Key::new(Table::Profile(profile.clone()), name)
                .map(|k| items(&self.doc, &k))
                .unwrap_or_default()
        };
        Find::Extras {
            entries: entries(&self.doc, &profile)
                .into_iter()
                .filter_map(Result::ok)
                .collect(),
            skills: listed("skills"),
            instructions: listed("instructions"),
            profile,
        }
    }

    /// The find the dropdown needs for the skills or instructions being
    /// typed, once a profile.
    fn dropdown_find(&mut self) -> Option<Find> {
        let Some(Typing {
            into: Input::Item(key, _) | Input::NewItem(key),
            ..
        }) = &self.typing
        else {
            return None;
        };
        let Table::Profile(profile) = &key.table else {
            return None;
        };
        if !matches!(key.name, "skills" | "instructions") || self.extras.contains_key(profile) {
            return None;
        }
        let profile = profile.clone();
        self.extras.insert(profile.clone(), None);
        Some(self.find_extras(profile))
    }

    /// `f`: suggestions for the selected key.
    fn suggest(&mut self) -> Outcome {
        let row = self.current_row();
        let key = row.as_ref().and_then(Row::key);
        let tally = Tally(&self.answers);
        match (&row, key) {
            (_, Some(key)) if key.table == Table::ReviewRequests && key.name == "teams" => {
                let Some(teams) = tally.teams() else {
                    self.notice = Some(
                        self.offline
                            .clone()
                            .unwrap_or_else(|| "your teams aren't in yet".into()),
                    );
                    return Outcome::Open;
                };
                let patterns = match self.doc.list(&key) {
                    Setting::Set(patterns) => patterns,
                    Setting::Unset | Setting::Invalid(_) => vec!["*".into()],
                };
                self.popup = Some(Popup::Suggest(Box::new(Suggest::teams(teams, patterns))));
            }
            (Some(Row::Entry(profile, _) | Row::NoEntries(profile)), _) => {
                let present = present_entries(&self.doc, &self.path);
                let mut orgs: Vec<(String, &str)> = Vec::new();
                for org in tally.orgs().into_iter().flatten() {
                    orgs.push((org.to_ascii_lowercase(), "one of your orgs"));
                }
                for team in tally.teams().into_iter().flatten() {
                    orgs.push((team.org.to_ascii_lowercase(), "one of your teams' orgs"));
                }
                let mut rows: Vec<Suggestion> = Vec::new();
                for (org, why) in orgs {
                    if !present.orgs.contains(&org) && !rows.iter().any(|r| r.label == org) {
                        rows.push(org_row(&org, why));
                    }
                }
                let what = What::Repos {
                    profile: profile.clone(),
                };
                self.popup = Some(Popup::Suggest(Box::new(Suggest::new(what, rows))));
            }
            (_, Some(key)) if key.name == "model" => {
                let rows = self
                    .models
                    .iter()
                    .map(|model| Suggestion {
                        label: model.clone(),
                        detail: String::new(),
                        on: false,
                        apply: Apply::Model(model.clone()),
                    })
                    .collect();
                self.popup = Some(Popup::Suggest(Box::new(Suggest::new(
                    What::Model { key },
                    rows,
                ))));
            }
            (_, Some(key)) if matches!(key.name, "skills" | "instructions") => {
                let Table::Profile(profile) = key.table.clone() else {
                    return Outcome::Open;
                };
                let what = if key.name == "skills" {
                    What::Skills {
                        profile: profile.clone(),
                    }
                } else {
                    What::Instructions {
                        profile: profile.clone(),
                    }
                };
                let mut suggest = Suggest::new(what, Vec::new());
                suggest.finding = true;
                self.popup = Some(Popup::Suggest(Box::new(suggest)));
                return Outcome::Find(self.find_extras(profile));
            }
            _ => {
                self.notice = Some(
                    "f suggests teams, repos, models, skills and instructions for those keys"
                        .into(),
                );
            }
        }
        Outcome::Open
    }

    /// Keys on `f`'s suggestions.
    fn suggest_key(&mut self, mut suggest: Box<Suggest>, key: KeyEvent) -> Outcome {
        let repos = matches!(suggest.what, What::Repos { .. });
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                suggest.cursor = (suggest.cursor + 1).min(suggest.rows.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => suggest.cursor = suggest.cursor.saturating_sub(1),
            KeyCode::Char(' ') if !suggest.picks_one() => suggest.toggle(),
            KeyCode::Char('s') if repos && !suggest.finding => {
                suggest.finding = true;
                let find = Find::Checkouts {
                    root: suggest.root.clone(),
                    depth: suggest.depth,
                };
                self.popup = Some(Popup::Suggest(suggest));
                return Outcome::Find(find);
            }
            KeyCode::Char('d') if repos => {
                let root = suggest.root.clone();
                self.popup = Some(Popup::Suggest(suggest));
                self.start_typing(Input::ScanRoot, &root);
                return Outcome::Open;
            }
            KeyCode::Char('<') if repos => suggest.depth = suggest.depth.saturating_sub(1),
            KeyCode::Char('>') if repos => suggest.depth += 1,
            KeyCode::Enter => {
                let ops = suggest.ops();
                if ops.is_empty() {
                    self.notice = Some(
                        if suggest.picks_one() || matches!(suggest.what, What::Teams { .. }) {
                            "nothing changed".into()
                        } else {
                            "Space picks what to add".into()
                        },
                    );
                    return Outcome::Open;
                }
                let mut outcome = Outcome::Open;
                for op in ops {
                    outcome = self.apply(op);
                }
                return outcome;
            }
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Open,
            _ => {}
        }
        self.popup = Some(Popup::Suggest(suggest));
        Outcome::Open
    }

    /// The save [`Outcome::Save`] asked for is being written.
    pub fn saving(&mut self) {
        self.saving = true;
    }

    /// Saving would start `held` held reviews: asks first, and on a yes
    /// saves again telling so.
    pub fn ask_held(&mut self, held: u32) {
        self.popup = Some(Popup::Held(held));
    }

    /// What manual reviews would hold once the edits are saved, which
    /// starts the held reviews of profiles they no longer hold.
    #[must_use]
    pub fn manual_reviews(&self) -> ManualReviews {
        let set = |table: Table| {
            let key = Key::new(table, "manual_reviews")?;
            match self.doc.scalar(&key) {
                Setting::Set(Scalar::Bool(on)) => Some(on),
                _ => None,
            }
        };
        ManualReviews {
            runner: set(Table::Runner).unwrap_or(DEFAULT_MANUAL_REVIEWS),
            overrides: self
                .doc
                .profiles()
                .into_iter()
                .filter_map(|name| Some((name.clone(), set(Table::Profile(name))?)))
                .collect(),
        }
    }

    /// How [`Outcome::Save`] went. Once written, editing carries on from
    /// what was written.
    pub fn saved(&mut self, saved: Result<Saved>) {
        self.saving = false;
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
        self.wrote = true;
        match ConfigDoc::parse(Some(&saved.text)) {
            Ok(doc) => self.doc = doc,
            Err(err) => {
                self.notice = Some(format!("saved, but can't read it back: {err:#}"));
                return;
            }
        }
        // Changes made to it meanwhile can drop the profile, or the entry
        // that's open, from under the selection.
        self.clamp();
        let gone = self.entry.as_ref().is_some_and(|edit| {
            edit.in_doc
                && !entries(&self.doc, &edit.profile)
                    .iter()
                    .any(|e| e.as_ref() == Ok(&edit.entry))
        });
        if gone {
            self.entry = None;
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

    /// The table the selected row is in; the last for the row after them.
    fn current_table(&self) -> Table {
        let rows = self.rows();
        rows[..=self.row.min(rows.len() - 1)]
            .iter()
            .rev()
            .find_map(|row| match row {
                Row::Header(table) => Some(table.clone()),
                _ => None,
            })
            .unwrap_or(Table::Runner)
    }

    /// The whole config's rows, with the one being added to a list.
    fn rows(&self) -> Vec<Row> {
        let adding = match &self.typing {
            Some(Typing {
                into: Input::NewItem(key),
                ..
            }) => Some(key),
            _ => None,
        };
        all_rows(&self.doc, &self.tables(), adding)
    }

    fn current_row(&self) -> Option<Row> {
        let mut rows = self.rows();
        (self.row < rows.len()).then(|| rows.swap_remove(self.row))
    }

    /// Selects `row`, if it's there.
    fn select(&mut self, row: &Row) {
        if let Some(at) = self.rows().iter().position(|r| r == row) {
            self.row = at;
        }
    }

    /// Selects the next table's header, or the one before's.
    fn step_table(&mut self, forward: bool) {
        let rows = self.rows();
        let headers = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, Row::Header(_) | Row::NewProfile))
            .map(|(at, _)| at);
        let target = if forward {
            headers.filter(|at| *at > self.row).min()
        } else {
            headers.filter(|at| *at < self.row).max()
        };
        if let Some(at) = target {
            self.row = at;
        }
    }

    /// Whether a new glob is being typed into the open entry.
    fn adding_glob(&self) -> bool {
        matches!(
            self.typing,
            Some(Typing {
                into: Input::Entry(EntryRow::NewGlob),
                ..
            })
        )
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let before = (self.row, self.current_row());
        let outcome = match self.key(key) {
            Outcome::Open => self.dropdown_find().map_or(Outcome::Open, Outcome::Find),
            outcome => outcome,
        };
        self.hint_precedence(before.0, before.1.as_ref());
        outcome
    }

    /// Moving onto a profile's header, says why the profiles' order
    /// matters, unless the key left a notice of its own. Like any notice,
    /// the next key clears it and the footer's keys come back. Only a move
    /// counts: the cursor on another row, and that row another header. An
    /// edit that keeps the selection in place, as a rename does, or that
    /// carries it along, as `K`/`J` do, changes only one of the two.
    fn hint_precedence(&mut self, at: usize, before: Option<&Row>) {
        let row = self.current_row();
        let moved = self.row != at && row.as_ref() != before;
        let header = matches!(row, Some(Row::Header(Table::Profile(_))));
        let idle = self.popup.is_none() && self.typing.is_none() && self.entry.is_none();
        if moved && header && idle && self.notice.is_none() {
            self.notice = Some(PRECEDENCE_HINT.into());
        }
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.notice = None;
        if ctrl && key.code == KeyCode::Char('c') {
            return Outcome::Quit;
        }
        // The save lands on the text as it was sent, and replaces the
        // edits with what it wrote: edits made meanwhile, or leaving to
        // discard them, would be lost to it.
        if self.saving {
            self.notice = Some("still saving".into());
            return Outcome::Open;
        }
        // Typing first: a scan's directory is typed over its suggestions.
        if let Some(typing) = self.typing.take() {
            return self.typing_key(typing, key);
        }
        if let Some(popup) = self.popup.take() {
            return self.popup_key(popup, key);
        }
        match key.code {
            KeyCode::Char('s') if ctrl => self.ask_save(),
            KeyCode::Char('v') => {
                self.popup = Some(Popup::Diff {
                    scroll: 0,
                    saving: false,
                });
            }
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
            _ if self.entry.is_some() => return self.entry_key(key),
            KeyCode::Esc | KeyCode::Char('q') => return self.leave(),
            KeyCode::Tab | KeyCode::Char(']') => self.step_table(true),
            KeyCode::BackTab | KeyCode::Char('[') => self.step_table(false),
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_to(0),
            KeyCode::Char('G') | KeyCode::End => self.move_to(usize::MAX),
            KeyCode::Enter => return self.edit(),
            KeyCode::Char(' ') => return self.toggle(),
            KeyCode::Char('u') => return self.unset(),
            KeyCode::Char('+') => self.add(),
            KeyCode::Char('-') => return self.remove(),
            KeyCode::Char('f') => return self.suggest(),
            KeyCode::Char('K') => return self.shift(-1),
            KeyCode::Char('J') => return self.shift(1),
            _ => {}
        }
        Outcome::Open
    }

    fn popup_key(&mut self, popup: Popup, key: KeyEvent) -> Outcome {
        match (popup, key.code) {
            (Popup::Suggest(suggest), _) => return self.suggest_key(suggest, key),
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
            (Popup::RemoveProfile(name), KeyCode::Char('y')) => {
                return self.apply(Op::RemoveProfile { name });
            }
            // Anything else closes it; a stray key neither writes nor
            // throws edits away.
            _ => {}
        }
        Outcome::Open
    }

    fn start_typing(&mut self, into: Input, text: &str) {
        let field = TextField::new(text, self.keys());
        let menu = Menu::new(self.options(&into, text));
        self.typing = Some(Typing { into, field, menu });
    }

    /// The keys text fields take: the config's, as edited, or the guess.
    fn keys(&self) -> Keys {
        let set = Key::new(Table::Tui, "keys").and_then(|key| match self.doc.scalar(&key) {
            Setting::Set(Scalar::Text(keys)) => match keys.as_str() {
                "emacs" => Some(Keys::Emacs),
                "vi" => Some(Keys::Vi),
                _ => None,
            },
            _ => None,
        });
        set.unwrap_or(self.guessed_keys)
    }

    fn typing_key(&mut self, mut typing: Typing, key: KeyEvent) -> Outcome {
        let number = matches!(&typing.into, Input::Scalar(k) if k.field().kind == Kind::Number);
        let open = typing.menu.is_open();
        let before = typing.field.text();
        match key.code {
            // Esc closes the dropdown first, and ↑ and ↓ choose in it.
            KeyCode::Esc if open => typing.menu.closed = true,
            KeyCode::Up if open => typing.menu.step(-1),
            KeyCode::Down if open => typing.menu.step(1),
            // Enter takes an option only once one's chosen, so it still
            // sets what's typed as it is.
            KeyCode::Tab if open => self.take(&mut typing),
            KeyCode::Enter if open && typing.menu.chosen.is_some() => self.take(&mut typing),
            // A tab is never part of a value.
            KeyCode::Tab => {}
            _ if number
                && typing
                    .field
                    .inserts(key)
                    .is_some_and(|c| !c.is_ascii_digit()) => {}
            _ => match typing.field.key(key) {
                Took::Edit if typing.field.text() != before => {
                    typing.menu = Menu::new(self.options(&typing.into, &typing.field.text()));
                }
                Took::Edit => {}
                Took::Commit => return self.commit(&typing.into, typing.field.text().trim()),
                Took::Cancel => {
                    // The row being added goes, so step back onto the one
                    // before.
                    match typing.into {
                        Input::NewItem(_) => self.row = self.row.saturating_sub(1),
                        Input::Entry(EntryRow::NewGlob) => {
                            self.with_entry(|edit| edit.row = edit.row.saturating_sub(1));
                        }
                        _ => {}
                    }
                    return Outcome::Open;
                }
            },
        }
        self.typing = Some(typing);
        Outcome::Open
    }

    /// Types the dropdown's option over what's typed, and lists what goes
    /// on from it, as a directory's files.
    fn take(&self, typing: &mut Typing) {
        if let Some(pick) = typing.menu.pick().map(str::to_owned) {
            typing.field.set_text(&pick);
            typing.menu = Menu::new(self.options(&typing.into, &pick));
        }
    }

    /// Lists the dropdown's options again, as what they come from changes.
    fn refresh_menu(&mut self) {
        let Some(mut typing) = self.typing.take() else {
            return;
        };
        let options = self.options(&typing.into, &typing.field.text());
        if options != typing.menu.options {
            typing.menu = Menu {
                options,
                chosen: None,
                closed: typing.menu.closed,
            };
        }
        self.typing = Some(typing);
    }

    /// What `into` could hold, going on from `typed`: paths on disk, and
    /// otherwise what `f` suggests, from what's been found and counted
    /// already.
    fn options(&self, into: &Input, typed: &str) -> Vec<String> {
        let mut options = Vec::new();
        let key = match into {
            Input::Scalar(key) | Input::Item(key, _) | Input::NewItem(key) => Some(key),
            _ => None,
        };
        let tally = Tally(&self.answers);
        match (key, into) {
            (Some(key), _) if !key.field().choices.is_empty() => {
                options.extend(key.field().choices.iter().map(|&c| c.to_owned()));
            }
            (Some(key), _) if key.name == "model" => options.extend(self.models.iter().cloned()),
            (Some(key), _) if key.table == Table::ReviewRequests && key.name == "teams" => {
                // `!` excludes a team, so it goes on as the team would.
                let (not, team) = match typed.strip_prefix('!') {
                    Some(team) => ("!", team),
                    None => ("", typed),
                };
                let teams = tally.teams().into_iter().flatten();
                let teams = teams.map(|team| format!("{}/{}", team.org, team.slug));
                return matching(teams, team)
                    .into_iter()
                    .map(|team| format!("{not}{team}"))
                    .collect();
            }
            (Some(key), _) if matches!(key.name, "skills" | "instructions") => {
                if let Table::Profile(profile) = &key.table
                    && let Some(Some(found)) = self.extras.get(profile)
                {
                    let found = if key.name == "skills" {
                        &found.skills
                    } else {
                        &found.instructions
                    };
                    // A find that raced an edit may name what's listed now.
                    let listed = items(&self.doc, key);
                    options.extend(found.iter().filter(|f| !listed.contains(f)).cloned());
                }
            }
            (None, Input::Entry(EntryRow::Target)) if !self.completes_paths(into) => {
                // As `f` does, what a profile watches already isn't offered.
                let watched = present_entries(&self.doc, &self.path).orgs;
                let orgs = tally.orgs().into_iter().flatten().cloned();
                let team_orgs = tally.teams().into_iter().flatten().map(|t| t.org.clone());
                options.extend(
                    orgs.chain(team_orgs)
                        .map(|org| org.to_ascii_lowercase())
                        .filter(|org| !watched.contains(org)),
                );
                for answer in self.answers.values() {
                    if let Answer::RepoNames { repos, .. } = answer {
                        options.extend(repos.iter().map(ToString::to_string));
                    }
                }
            }
            _ => {}
        }
        let mut options = matching(options, typed);
        // Paths go on from what's typed, so there's nothing to list until
        // something is.
        if self.completes_paths(into) && !typed.trim().is_empty() {
            // The scan's directory resolves against the working
            // directory, as a path typed at a shell does; the config's
            // paths against its own.
            let base = if *into == Input::ScanRoot {
                std::env::current_dir().unwrap_or_default()
            } else {
                self.path.parent().unwrap_or(Path::new(".")).to_owned()
            };
            let home = std::env::home_dir();
            let typed = typed.trim();
            for path in complete::path_matches(typed, &base, home.as_deref()) {
                // A file taken lists only itself: nothing goes on from it.
                if path != typed && !options.contains(&path) {
                    options.push(path);
                }
            }
        }
        options
    }

    fn completes_paths(&self, into: &Input) -> bool {
        match into {
            Input::Scalar(key) | Input::Item(key, _) | Input::NewItem(key) => key.field().paths,
            Input::Entry(EntryRow::Target) => self
                .entry
                .as_ref()
                .is_some_and(|e| e.entry.kind() != crate::config_doc::EntryKind::Github),
            Input::ScanRoot => true,
            _ => false,
        }
    }

    /// Enter on a row: types into it, flips a bool, or opens a repo entry.
    fn edit(&mut self) -> Outcome {
        match self.current_row() {
            Some(Row::Header(Table::Profile(name))) => {
                self.start_typing(Input::Rename(name.clone()), &name);
            }
            Some(Row::Header(_)) => self.move_by(1),
            Some(Row::NewProfile) => self.start_typing(Input::NewProfile, ""),
            Some(Row::Scalar(key)) if key.field().kind == Kind::Bool => return self.toggle(),
            Some(Row::Scalar(key)) => {
                let text = match self.doc.scalar(&key) {
                    Setting::Set(value) => scalar_text(&value),
                    Setting::Unset | Setting::Invalid(_) => String::new(),
                };
                self.start_typing(Input::Scalar(key), &text);
            }
            Some(Row::Item(key, n)) => {
                let text = items(&self.doc, &key).get(n).cloned().unwrap_or_default();
                self.start_typing(Input::Item(key, n), &text);
            }
            Some(Row::List(_) | Row::NewItem(_) | Row::NoEntries(_)) => self.add(),
            Some(Row::Entry(profile, n)) => match entries(&self.doc, &profile).get(n) {
                Some(Ok(entry)) => {
                    self.entry = Some(EntryEdit {
                        profile,
                        entry: entry.clone(),
                        in_doc: true,
                        row: 0,
                        set_aside: String::new(),
                        globs_aside: Vec::new(),
                    });
                }
                _ => {
                    self.notice =
                        Some("the editor doesn't know this entry's shape; edit it by hand".into());
                }
            },
            None => {}
        }
        Outcome::Open
    }

    /// `+`: adds to the selected list, or a repo entry to the profile.
    fn add(&mut self) {
        match self.current_row() {
            Some(Row::Item(key, _) | Row::List(key) | Row::NewItem(key)) => {
                self.start_typing(Input::NewItem(key.clone()), "");
                if let Some(at) = self
                    .rows()
                    .iter()
                    .position(|r| *r == Row::NewItem(key.clone()))
                {
                    self.row = at;
                }
            }
            Some(Row::Entry(profile, _) | Row::NoEntries(profile)) => {
                self.entry = Some(EntryEdit {
                    profile,
                    entry: RepoEntry::Checkout {
                        path: String::new(),
                        remote: None,
                    },
                    in_doc: false,
                    row: 1,
                    set_aside: String::new(),
                    globs_aside: Vec::new(),
                });
                self.start_typing(Input::Entry(EntryRow::Target), "");
            }
            Some(Row::Header(Table::Profile(_)) | Row::NewProfile) => {
                self.start_typing(Input::NewProfile, "");
                self.select(&Row::NewProfile);
            }
            _ => {
                self.notice =
                    Some("+ adds to a list, a repo entry to a profile, or a profile".into());
            }
        }
    }

    /// `-`: removes the selected list item or repo entry.
    fn remove(&mut self) -> Outcome {
        match self.current_row() {
            Some(Row::Item(key, n)) => {
                let Some(value) = items(&self.doc, &key).get(n).cloned() else {
                    return Outcome::Open;
                };
                self.remove_row(Op::Remove { key, value })
            }
            Some(Row::Entry(profile, n)) => {
                if let Some(Ok(entry)) = entries(&self.doc, &profile).get(n) {
                    let entry = entry.clone();
                    return self.remove_row(Op::RemoveEntry { profile, entry });
                }
                self.notice =
                    Some("the editor doesn't know this entry's shape; edit it by hand".into());
                Outcome::Open
            }
            Some(Row::Header(Table::Profile(name))) => {
                self.popup = Some(Popup::RemoveProfile(name));
                Outcome::Open
            }
            _ => {
                self.notice = Some("- removes a list item, a repo entry or a profile".into());
                Outcome::Open
            }
        }
    }

    /// Applies `op`, which removes the selected row, and stays on its key:
    /// on the row that took its place, or the one before when it was last.
    fn remove_row(&mut self, op: Op) -> Outcome {
        let first = self.row - self.row_in_key();
        let key = self.current_row().and_then(|row| row.key());
        let outcome = self.apply(op);
        let left = self
            .rows()
            .iter()
            .rposition(|row| row.key().is_some() && row.key() == key);
        if let Some(last) = left {
            self.row = self.row.min(last).max(first);
        }
        outcome
    }

    /// K and J: moves the selected list item or repo entry up or down,
    /// since order matters: the last team glob to match wins, and the
    /// first of equally specific entries.
    fn shift(&mut self, by: isize) -> Outcome {
        let (op, at) = match self.current_row() {
            Some(Row::Item(key, n)) => {
                let list = items(&self.doc, &key);
                let Some(to) = n.checked_add_signed(by).filter(|to| *to < list.len()) else {
                    return Outcome::Open;
                };
                let value = list[n].clone();
                (Op::Move { key, value, to }, to)
            }
            Some(Row::Entry(profile, n)) => {
                let all = entries(&self.doc, &profile);
                let Some(to) = n.checked_add_signed(by).filter(|to| *to < all.len()) else {
                    return Outcome::Open;
                };
                let Some(Ok(entry)) = all.get(n).cloned() else {
                    return Outcome::Open;
                };
                (Op::MoveEntry { profile, entry, to }, to)
            }
            Some(Row::Header(Table::Profile(name))) => return self.shift_profile(name, by),
            _ => {
                self.notice = Some("K and J move a list item, a repo entry or a profile".into());
                return Outcome::Open;
            }
        };
        let first = self.row - self.row_in_key();
        let applied = self.doc.ops().len();
        let outcome = self.apply(op);
        if self.doc.ops().len() > applied {
            self.row = first + at;
        }
        outcome
    }

    /// Moves a profile up or down among the profiles, keeping it selected.
    fn shift_profile(&mut self, name: String, by: isize) -> Outcome {
        let profiles = self.doc.profiles();
        let Some(to) = profiles
            .iter()
            .position(|p| *p == name)
            .and_then(|at| at.checked_add_signed(by))
            .filter(|to| *to < profiles.len())
        else {
            return Outcome::Open;
        };
        let outcome = self.apply(Op::MoveProfile {
            name: name.clone(),
            to,
        });
        self.select(&Row::Header(Table::Profile(name)));
        outcome
    }

    /// Where the selected row is among its key's rows.
    fn row_in_key(&self) -> usize {
        match self.current_row() {
            Some(Row::Item(_, n) | Row::Entry(_, n)) => n,
            _ => 0,
        }
    }

    /// Makes what was typed.
    fn commit(&mut self, into: &Input, text: &str) -> Outcome {
        let op = match into.clone() {
            Input::Scalar(key) => return self.commit_scalar(key, text),
            Input::Item(key, n) => {
                let Some(old) = items(&self.doc, &key).get(n).cloned() else {
                    return Outcome::Open;
                };
                match text {
                    "" => return self.remove_row(Op::Remove { key, value: old }),
                    _ if text == old => return Outcome::Open,
                    _ => Op::Replace {
                        key,
                        old,
                        new: text.to_owned(),
                    },
                }
            }
            Input::NewItem(key) => {
                // Back on the list's last row, then on what was added.
                self.row = self.row.saturating_sub(1);
                if text.is_empty() {
                    return Outcome::Open;
                }
                let outcome = self.apply(Op::Push {
                    key: key.clone(),
                    value: text.to_owned(),
                });
                let last = items(&self.doc, &key).len().saturating_sub(1);
                if let Some(at) = self
                    .rows()
                    .iter()
                    .position(|r| *r == Row::Item(key.clone(), last))
                {
                    self.row = at;
                }
                return outcome;
            }
            Input::Rename(from) => {
                if text.is_empty() || text == from {
                    return Outcome::Open;
                }
                // A refused rename, as to a name another profile has,
                // stays on the profile being renamed.
                let applied = self.doc.ops().len();
                let outcome = self.apply(Op::RenameProfile {
                    from,
                    to: text.to_owned(),
                });
                if self.doc.ops().len() > applied {
                    self.select(&Row::Header(Table::Profile(text.to_owned())));
                }
                return outcome;
            }
            Input::NewProfile => {
                if text.is_empty() {
                    return Outcome::Open;
                }
                let outcome = self.apply(Op::AddProfile {
                    name: text.to_owned(),
                });
                self.select(&Row::Header(Table::Profile(text.to_owned())));
                return outcome;
            }
            Input::Entry(row) => return self.commit_entry(row, text),
            Input::ScanRoot => {
                if let Some(Popup::Suggest(suggest)) = &mut self.popup {
                    suggest.root = if text.is_empty() {
                        suggest::ROOT.into()
                    } else {
                        text.to_owned()
                    };
                }
                return Outcome::Open;
            }
        };
        self.apply(op)
    }

    /// Sets a text or number key; blank unsets it.
    fn commit_scalar(&mut self, key: Key, text: &str) -> Outcome {
        let op = if text.is_empty() {
            if self.doc.scalar(&key) == Setting::Unset {
                return Outcome::Open;
            }
            Op::Unset { key }
        } else {
            let value = match key.field().kind {
                Kind::Number => {
                    let Ok(n) = text.parse() else {
                        self.notice = Some(format!("{text} is too big for `{key}`"));
                        self.start_typing(Input::Scalar(key), text);
                        return Outcome::Open;
                    };
                    Scalar::Number(n)
                }
                _ => Scalar::Text(text.to_owned()),
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
        let Some(Row::Scalar(key)) = self.current_row() else {
            return Outcome::Open;
        };
        if key.field().kind != Kind::Bool {
            return Outcome::Open;
        }
        let now = match shown(&self.doc, &key) {
            Shown::Set(value) | Shown::Default(value) => value == "true",
            Shown::Guessed | Shown::Invalid(_) | Shown::Required => false,
        };
        self.apply(Op::Set {
            key,
            value: Scalar::Bool(!now),
        })
    }

    /// `u`: unsets the selected key, a whole list at once.
    fn unset(&mut self) -> Outcome {
        let Some(key) = self.current_row().and_then(|row| row.key()) else {
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
        let first = self.row - self.row_in_key();
        let outcome = self.apply(Op::Unset { key });
        self.row = first;
        outcome
    }

    /// Keys while a repo entry is open.
    fn entry_key(&mut self, key: KeyEvent) -> Outcome {
        let Some(edit) = self.entry.clone() else {
            return Outcome::Open;
        };
        let rows = edit.rows(false);
        let current = edit.current(false);
        let step = |edit: &mut EntryEdit, to: usize| edit.row = to.min(rows.len() - 1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_entry(),
            KeyCode::Char('j') | KeyCode::Down => {
                self.with_entry(|edit| step(edit, edit.row + 1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.with_entry(|edit| step(edit, edit.row.saturating_sub(1)));
            }
            KeyCode::Char('g') | KeyCode::Home => self.with_entry(|edit| step(edit, 0)),
            KeyCode::Char('G') | KeyCode::End => self.with_entry(|edit| step(edit, usize::MAX)),
            KeyCode::Char(' ' | 'l') | KeyCode::Right | KeyCode::Enter
                if current == EntryRow::Kind =>
            {
                return self.step_kind(&edit, 1);
            }
            KeyCode::Char('h') | KeyCode::Left if current == EntryRow::Kind => {
                return self.step_kind(&edit, KINDS.len() - 1);
            }
            KeyCode::Enter => match current {
                EntryRow::Target => {
                    self.start_typing(Input::Entry(current), edit.target());
                }
                EntryRow::Remote => {
                    self.start_typing(Input::Entry(current), edit.remote().unwrap_or_default());
                }
                EntryRow::Glob(n) => self.start_typing(
                    Input::Entry(current),
                    edit.globs().get(n).map_or("", String::as_str),
                ),
                EntryRow::NoGlobs | EntryRow::NewGlob => self.add_glob(&edit),
                EntryRow::Kind => {}
            },
            KeyCode::Char('+') => self.add_glob(&edit),
            KeyCode::Char('-') => match current {
                EntryRow::Glob(n) => {
                    let new = edit.changed(|_, _, globs| {
                        globs.remove(n);
                    });
                    return self.update_entry(new);
                }
                _ => self.notice = Some("- removes a glob".into()),
            },
            KeyCode::Char('u') if current == EntryRow::Remote && edit.remote().is_some() => {
                let new = edit.changed(|_, remote, _| *remote = None);
                return self.update_entry(new);
            }
            _ => {}
        }
        Outcome::Open
    }

    fn with_entry(&mut self, change: impl FnOnce(&mut EntryEdit)) {
        if let Some(edit) = &mut self.entry {
            change(edit);
        }
    }

    fn add_glob(&mut self, edit: &EntryEdit) {
        if edit.entry.kind() == crate::config_doc::EntryKind::Checkout {
            self.notice =
                Some("a plain checkout has no globs: switch it to checkout + paths".into());
            return;
        }
        self.start_typing(Input::Entry(EntryRow::NewGlob), "");
        let rows = edit.rows(true);
        self.with_entry(|edit| edit.row = rows.len() - 1);
    }

    /// Switches the open entry `by` kinds along [`KINDS`].
    fn step_kind(&mut self, edit: &EntryEdit, by: usize) -> Outcome {
        let at = KINDS
            .iter()
            .position(|k| *k == edit.entry.kind())
            .unwrap_or(0);
        let kind = KINDS[(at + by) % KINDS.len()];
        let github = |kind| kind == crate::config_doc::EntryKind::Github;
        let crossing = github(kind) != github(edit.entry.kind());
        let switched = EntryEdit {
            entry: edit.entry.clone().with_kind(kind, None),
            ..edit.clone()
        };
        // What the new kind drops is set aside, and what it takes back
        // comes back.
        let new = switched.changed(|target, _, globs| {
            if crossing {
                edit.set_aside.clone_into(target);
            }
            if globs.is_empty() {
                globs.clone_from(&edit.globs_aside);
            }
        });
        let outcome = self.update_entry(new.clone());
        self.with_entry(|e| {
            if e.entry == new {
                if crossing {
                    edit.target().clone_into(&mut e.set_aside);
                }
                if !edit.globs().is_empty() {
                    e.globs_aside = edit.globs().to_vec();
                }
            }
        });
        outcome
    }

    fn close_entry(&mut self) {
        let Some(edit) = self.entry.take() else {
            return;
        };
        if !edit.in_doc {
            self.notice = Some("nothing added: the entry names no checkout or repo".into());
        }
        let found = entries(&self.doc, &edit.profile)
            .iter()
            .position(|e| e.as_ref() == Ok(&edit.entry));
        if let Some(at) = found.and_then(|n| {
            self.rows()
                .iter()
                .position(|r| *r == Row::Entry(edit.profile.clone(), n))
        }) {
            self.row = at;
        }
    }

    /// Makes what was typed into the open entry.
    fn commit_entry(&mut self, row: EntryRow, text: &str) -> Outcome {
        let Some(edit) = self.entry.clone() else {
            return Outcome::Open;
        };
        let new = match row {
            EntryRow::Target => edit.changed(|target, _, _| text.clone_into(target)),
            EntryRow::Remote => {
                edit.changed(|_, remote, _| *remote = (!text.is_empty()).then(|| text.to_owned()))
            }
            EntryRow::Glob(n) => edit.changed(|_, _, globs| {
                if text.is_empty() {
                    globs.remove(n);
                } else if let Some(glob) = globs.get_mut(n) {
                    text.clone_into(glob);
                }
            }),
            EntryRow::NewGlob => {
                if text.is_empty() {
                    self.with_entry(|edit| edit.row = edit.row.saturating_sub(1));
                    return Outcome::Open;
                }
                edit.changed(|_, _, globs| globs.push(text.to_owned()))
            }
            EntryRow::Kind | EntryRow::NoGlobs => return Outcome::Open,
        };
        self.update_entry(new)
    }

    /// Replaces the open entry with `new`: in the file once it's there,
    /// or put there once it names a checkout or a repo.
    fn update_entry(&mut self, new: RepoEntry) -> Outcome {
        let Some(edit) = self.entry.clone() else {
            return Outcome::Open;
        };
        if new == edit.entry {
            return Outcome::Open;
        }
        let named = |entry: &RepoEntry| match entry {
            RepoEntry::Checkout { path, .. } | RepoEntry::Scoped { path, .. } => !path.is_empty(),
            RepoEntry::Github { name, .. } => !name.is_empty(),
        };
        let op = if edit.in_doc {
            Op::ReplaceEntry {
                profile: edit.profile.clone(),
                old: edit.entry.clone(),
                new: new.clone(),
            }
        } else if named(&new) {
            Op::PushEntry {
                profile: edit.profile.clone(),
                entry: new.clone(),
            }
        } else {
            self.with_entry(|e| e.entry = new);
            return Outcome::Open;
        };
        let applied = self.doc.ops().len();
        let outcome = self.apply(op);
        if self.doc.ops().len() > applied {
            self.with_entry(|e| {
                e.entry = new;
                e.in_doc = true;
            });
        }
        let rows = self.entry.as_ref().map_or(0, |e| e.rows(false).len());
        self.with_entry(|e| e.row = e.row.min(rows.saturating_sub(1)));
        outcome
    }

    fn apply(&mut self, op: Op) -> Outcome {
        match self.doc.apply(op) {
            Ok(()) => {
                self.extras.clear();
                self.clamp();
                self.check_now()
            }
            Err(err) => {
                self.notice = Some(format!("{err:#}"));
                Outcome::Open
            }
        }
    }

    /// Keeps the selection on a table and row that exist.
    fn clamp(&mut self) {
        self.row = self.row.min(self.rows().len().saturating_sub(1));
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
                self.notice = Some("it doesn't load: fix what it says below, then save".into());
            }
        }
    }

    /// Selects the profile an error names, if it names one.
    fn show_where(&mut self, why: &str) {
        let named = why
            .split_once("in profile `")
            .and_then(|(_, rest)| rest.split_once('`'))
            .map(|(name, _)| Table::Profile(name.to_owned()));
        if let Some(table) = named {
            self.select(&Row::Header(table));
            self.entry = None;
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
        self.move_to(self.row.saturating_add_signed(delta));
    }

    fn move_to(&mut self, target: usize) {
        self.row = target.min(self.rows().len().saturating_sub(1));
    }
}

/// What every profile's entries already name: whole orgs, lowercased,
/// and checkouts, expanded against the config's directory.
struct Present {
    orgs: Vec<String>,
    checkouts: Vec<PathBuf>,
}

fn present_entries(doc: &ConfigDoc, path: &Path) -> Present {
    let base = path.parent().unwrap_or(Path::new("."));
    let mut present = Present {
        orgs: Vec::new(),
        checkouts: Vec::new(),
    };
    for profile in doc.profiles() {
        for entry in entries(doc, &profile).into_iter().flatten() {
            match entry {
                RepoEntry::Github { name, .. } if !name.contains('/') => {
                    present.orgs.push(name.to_ascii_lowercase());
                }
                RepoEntry::Checkout { path, .. } | RepoEntry::Scoped { path, .. } => {
                    if let Ok(path) = sanic_core::config::expand_path(&path, base) {
                        present.checkouts.push(path);
                    }
                }
                RepoEntry::Github { .. } => {}
            }
        }
    }
    present
}

fn org_row(org: &str, why: &str) -> Suggestion {
    Suggestion {
        label: org.to_owned(),
        detail: format!("every repo in it · {why}"),
        on: false,
        apply: Apply::Entry(RepoEntry::Github {
            name: org.to_owned(),
            paths: Vec::new(),
        }),
    }
}

#[cfg(test)]
mod tests;
