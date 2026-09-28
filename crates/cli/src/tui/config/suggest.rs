//! `f`: what the focused key could hold, found for you: your teams to tick,
//! orgs and checkouts to watch, models, skills and instruction files.

use std::path::PathBuf;

use sanic_core::{config::TeamFilter, pr::TeamRef};

use super::discover::{scan, skills::Skill};
use crate::config_doc::{Key, Op, RepoEntry, Table};

/// What the editor asks the host to find, since it reads the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Find {
    /// Checkouts under `root`, as typed, at most `depth` directories down.
    Checkouts { root: String, depth: usize },
    /// A profile's skills and instruction files: what its `entries`
    /// check out, less the `skills` and `instructions` it has, as written.
    Extras {
        profile: String,
        entries: Vec<RepoEntry>,
        skills: Vec<String>,
        instructions: Vec<String>,
    },
}

/// What the host found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Checkouts(Vec<scan::Found>),
    Extras {
        profile: String,
        skills: Vec<Skill>,
        instructions: Vec<PathBuf>,
    },
    Failed(String),
}

/// The suggestions shown for a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggest {
    pub what: What,
    pub rows: Vec<Suggestion>,
    pub cursor: usize,
    /// Where checkouts are scanned for, as typed, and how deep.
    pub root: String,
    pub depth: usize,
    /// A scan or search is under way.
    pub finding: bool,
}

/// What the suggestions are for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    Teams { patterns: Vec<String> },
    Repos { profile: String },
    Model { key: Key },
    Skills { profile: String },
    Instructions { profile: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    /// Shown dimmed after the label.
    pub detail: String,
    pub on: bool,
    pub apply: Apply,
}

/// What picking a suggestion writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Apply {
    Team(TeamRef),
    Entry(RepoEntry),
    Model(String),
    Item(String),
}

/// Where `f` starts scanning, and how deep, as `setup` did.
pub const ROOT: &str = "~";
pub const DEPTH: usize = 3;

impl Suggest {
    pub fn new(what: What, rows: Vec<Suggestion>) -> Self {
        Self {
            what,
            rows,
            cursor: 0,
            root: ROOT.into(),
            depth: DEPTH,
            finding: false,
        }
    }

    /// Your teams, ticked where the filter lets their requests count.
    pub fn teams(teams: &[TeamRef], patterns: Vec<String>) -> Self {
        let filter = TeamFilter::new(patterns.clone()).ok();
        let mut teams = teams.to_vec();
        teams.sort();
        let rows = teams
            .into_iter()
            .map(|team| Suggestion {
                label: team.to_string(),
                detail: String::new(),
                on: filter.as_ref().is_none_or(|f| f.allows(&team)),
                apply: Apply::Team(team),
            })
            .collect();
        Self::new(What::Teams { patterns }, rows)
    }

    /// Only one can be picked: a model.
    pub fn picks_one(&self) -> bool {
        matches!(self.what, What::Model { .. })
    }

    pub fn toggle(&mut self) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.on = !row.on;
        }
    }

    /// The edits picking makes. For teams, `*` then `!org/slug` for each
    /// unticked one, so teams you join later count until you exclude
    /// them; nothing if that's what the filter already says.
    pub fn ops(&self) -> Vec<Op> {
        match &self.what {
            What::Teams { patterns } => {
                let mut wanted = vec!["*".to_owned()];
                wanted.extend(
                    self.rows
                        .iter()
                        .filter(|r| !r.on)
                        .filter_map(|r| match &r.apply {
                            Apply::Team(team) => Some(format!("!{team}")),
                            _ => None,
                        }),
                );
                // The same teams counting as before leaves hand-written
                // patterns alone.
                let before = TeamFilter::new(patterns.clone()).ok();
                let same = self.rows.iter().all(|r| match (&r.apply, &before) {
                    (Apply::Team(team), Some(filter)) => filter.allows(team) == r.on,
                    _ => false,
                });
                let Some(key) = Key::new(Table::ReviewRequests, "teams") else {
                    return Vec::new();
                };
                if same {
                    return Vec::new();
                }
                let mut ops = vec![Op::Unset { key: key.clone() }];
                ops.extend(wanted.into_iter().map(|value| Op::Push {
                    key: key.clone(),
                    value,
                }));
                ops
            }
            What::Model { key } => match self.rows.get(self.cursor).map(|r| &r.apply) {
                Some(Apply::Model(model)) => vec![Op::Set {
                    key: key.clone(),
                    value: crate::config_doc::Scalar::Text(model.clone()),
                }],
                _ => Vec::new(),
            },
            What::Repos { profile } => self
                .picked()
                .filter_map(|apply| match apply {
                    Apply::Entry(entry) => Some(Op::PushEntry {
                        profile: profile.clone(),
                        entry: entry.clone(),
                    }),
                    _ => None,
                })
                .collect(),
            What::Skills { profile } | What::Instructions { profile } => {
                let name = if matches!(self.what, What::Skills { .. }) {
                    "skills"
                } else {
                    "instructions"
                };
                let Some(key) = Key::new(Table::Profile(profile.clone()), name) else {
                    return Vec::new();
                };
                self.picked()
                    .filter_map(|apply| match apply {
                        Apply::Item(value) => Some(Op::Push {
                            key: key.clone(),
                            value: value.clone(),
                        }),
                        _ => None,
                    })
                    .collect()
            }
        }
    }

    fn picked(&self) -> impl Iterator<Item = &Apply> {
        self.rows.iter().filter(|r| r.on).map(|r| &r.apply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unticked_teams_are_written_as_exclusions_after_a_star() {
        let teams = [TeamRef::new("org", "b"), TeamRef::new("org", "a")];
        let mut suggest = Suggest::teams(&teams, vec!["*".into()]);
        assert_eq!(suggest.rows[0].label, "org/a");
        assert!(suggest.ops().is_empty(), "nothing changed");
        suggest.cursor = 1;
        suggest.toggle();
        let ops = suggest.ops();
        let key = Key::new(Table::ReviewRequests, "teams").unwrap();
        assert_eq!(
            ops,
            [
                Op::Unset { key: key.clone() },
                Op::Push {
                    key: key.clone(),
                    value: "*".into()
                },
                Op::Push {
                    key,
                    value: "!org/b".into()
                },
            ]
        );
        // Hand-written patterns that let the same teams count stay.
        let suggest = Suggest::teams(&teams, vec!["org/*".into(), "!org/b".into()]);
        assert!(!suggest.rows[1].on);
        assert!(suggest.ops().is_empty());
    }
}
