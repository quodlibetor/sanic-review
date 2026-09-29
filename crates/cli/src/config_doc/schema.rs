//! Every key the config file takes, with what it means unset, so the
//! editor can list them all and show a default in place of a missing one.

use sanic_core::config::{
    AUTO_MODEL, DEFAULT_API_URL, DEFAULT_CLAUDE, DEFAULT_GIT_URL, DEFAULT_MANUAL_REVIEWS,
    DEFAULT_MAX_RUNS, DEFAULT_MIN_NOTIFICATION_POLL, DEFAULT_QUIET, DEFAULT_RECONCILE,
    DEFAULT_RUN_TIMEOUT, DEFAULT_SKIP_DRAFTS, DEFAULT_TEAMS, DEFAULT_UPDATED_WITHIN_DAYS,
};

use super::Table;

/// What a key holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    Bool,
    /// A list of strings.
    List,
    /// A profile's `repos`: see [`super::RepoEntry`].
    Repos,
}

/// What a key means when the file leaves it unset.
#[derive(Debug, Clone)]
pub enum Fallback {
    /// This value, as the file would write it.
    Value(fn() -> String),
    /// An empty list, or `false`.
    Nothing,
    /// Whatever this key in another table says.
    Inherits(Table, &'static str),
    /// The key has to be set.
    Required,
    /// A guess from outside the file, which the editor makes.
    Guessed,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: &'static str,
    pub kind: Kind,
    pub fallback: Fallback,
    pub help: &'static str,
    /// Holds paths, which typing completes.
    pub paths: bool,
    /// The only values it takes, when it's one of a few.
    pub choices: &'static [&'static str],
}

impl Field {
    const fn new(name: &'static str, kind: Kind, fallback: Fallback, help: &'static str) -> Self {
        Self {
            name,
            kind,
            fallback,
            help,
            paths: false,
            choices: &[],
        }
    }

    const fn paths(mut self) -> Self {
        self.paths = true;
        self
    }

    const fn choices(mut self, choices: &'static [&'static str]) -> Self {
        self.choices = choices;
        self
    }
}

pub const GITHUB: &[Field] = &[
    Field::new(
        "api_url",
        Kind::Text,
        Fallback::Value(|| DEFAULT_API_URL.into()),
        "GitHub's API, e.g. through a proxy; `serve` reads it only at startup",
    ),
    Field::new(
        "git_url",
        Kind::Text,
        Fallback::Value(|| DEFAULT_GIT_URL.into()),
        "where repo mirrors fetch from",
    ),
];

pub const POLL: &[Field] = &[
    Field::new(
        "reconcile_secs",
        Kind::Number,
        Fallback::Value(|| DEFAULT_RECONCILE.as_secs().to_string()),
        "seconds between the GraphQL searches that catch what notifications miss",
    ),
    Field::new(
        "min_notification_secs",
        Kind::Number,
        Fallback::Value(|| DEFAULT_MIN_NOTIFICATION_POLL.as_secs().to_string()),
        "fewest seconds between notification polls; GitHub can ask for more",
    ),
    Field::new(
        "quiet_secs",
        Kind::Number,
        Fallback::Value(|| DEFAULT_QUIET.as_secs().to_string()),
        "seconds a PR must go quiet before its review starts",
    ),
    Field::new(
        "updated_within_days",
        Kind::Number,
        Fallback::Value(|| DEFAULT_UPDATED_WITHIN_DAYS.to_string()),
        "ignore PRs quiet for longer; 0 is no limit",
    ),
];

pub const REVIEW_REQUESTS: &[Field] = &[
    Field::new(
        "teams",
        Kind::List,
        Fallback::Value(|| format!("{DEFAULT_TEAMS:?}")),
        "which of your teams' requests count: the last glob that matches wins, `!` excludes",
    ),
    Field::new(
        "skip_titles",
        Kind::List,
        Fallback::Nothing,
        "never review PRs with these titles automatically",
    ),
    Field::new(
        "skip_drafts",
        Kind::Bool,
        Fallback::Value(|| DEFAULT_SKIP_DRAFTS.to_string()),
        "never review draft PRs automatically",
    ),
];

pub const RUNNER: &[Field] = &[
    Field::new(
        "claude",
        Kind::Text,
        Fallback::Value(|| DEFAULT_CLAUDE.into()),
        "the `claude` executable; a bare name is looked up on PATH",
    )
    .paths(),
    Field::new(
        "max_concurrent",
        Kind::Number,
        Fallback::Value(|| DEFAULT_MAX_RUNS.to_string()),
        "agent runs at once, across all PRs",
    ),
    Field::new(
        "timeout_secs",
        Kind::Number,
        Fallback::Value(|| DEFAULT_RUN_TIMEOUT.as_secs().to_string()),
        "seconds before a run is killed",
    ),
    Field::new(
        "read_paths",
        Kind::List,
        Fallback::Nothing,
        "more directories the agent may read",
    )
    .paths(),
    Field::new(
        "model",
        Kind::Text,
        Fallback::Value(|| AUTO_MODEL.into()),
        "the review model; `auto` is your own Claude default",
    ),
    Field::new(
        "manual_reviews",
        Kind::Bool,
        Fallback::Value(|| DEFAULT_MANUAL_REVIEWS.to_string()),
        "hold queued reviews until you start them",
    ),
];

pub const TUI: &[Field] = &[Field::new(
    "keys",
    Kind::Text,
    Fallback::Guessed,
    "`emacs` or `vi` keys in text fields; unset, guessed from readline and $EDITOR",
)
.choices(&["emacs", "vi"])];

pub const PROFILE: &[Field] = &[
    Field::new(
        "instructions",
        Kind::List,
        Fallback::Nothing,
        "files whose text goes in the agent's system prompt",
    )
    .paths(),
    Field::new(
        "skills",
        Kind::List,
        Fallback::Nothing,
        "skill directories the agent may read",
    )
    .paths(),
    Field::new(
        "model",
        Kind::Text,
        Fallback::Inherits(Table::Runner, "model"),
        "the review model for this profile's PRs",
    ),
    Field::new(
        "manual_reviews",
        Kind::Bool,
        Fallback::Inherits(Table::Runner, "manual_reviews"),
        "overrides `runner.manual_reviews` for this profile's PRs",
    ),
    Field::new(
        "skip_titles",
        Kind::List,
        Fallback::Nothing,
        "added to `review_requests.skip_titles` for this profile's PRs",
    ),
    Field::new(
        "skip_drafts",
        Kind::Bool,
        Fallback::Inherits(Table::ReviewRequests, "skip_drafts"),
        "overrides `review_requests.skip_drafts` for this profile's PRs",
    ),
    Field::new(
        "auto_fix",
        Kind::Bool,
        Fallback::Nothing,
        "draft fixes for comments on your own PRs, in a workspace of their checkout",
    ),
    Field::new(
        "repos",
        Kind::Repos,
        Fallback::Required,
        "what this profile reviews; the most specific entry wins",
    ),
];
