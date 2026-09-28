//! The index's filter: which rows the lists show, read from the URL's
//! query, and the sidebar that picks it. The server filters, so a filtered
//! index is a link and the lists' refresh keeps it.

use std::{collections::HashMap, fmt::Write};

use maud::{Markup, html};
use sanic_core::pr::is_login;

/// A group of values the sidebar picks from, and its key in the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facet {
    Status,
    Author,
    Reviewer,
    Repo,
    State,
}

impl Facet {
    /// In the sidebar's order, which is also the URL's.
    pub const ALL: [Self; 5] = [
        Self::Status,
        Self::Author,
        Self::Reviewer,
        Self::Repo,
        Self::State,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Author => "author",
            Self::Reviewer => "reviewer",
            Self::Repo => "repo",
            Self::State => "state",
        }
    }

    fn of_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|facet| facet.key() == key)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Author => "Author",
            Self::Reviewer => "Reviewed by",
            Self::Repo => "Repo",
            Self::State => "State",
        }
    }

    /// The values a facet always has, in order, with their words; `None`
    /// for people and repos, which come from the rows.
    fn fixed(self) -> Option<&'static [(&'static str, &'static str)]> {
        match self {
            Self::Status => Some(STATUSES),
            Self::State => Some(STATES),
            Self::Author | Self::Reviewer | Self::Repo => None,
        }
    }

    /// Whether the facet filters `list`: your PRs are all yours, so author
    /// doesn't.
    pub fn applies(self, list: Side) -> bool {
        !(self == Self::Author && list == Side::Mine)
    }
}

/// The groups of both lists, as the Status facet names them. Needs you is
/// both lists' first group.
pub const STATUSES: &[(&str, &str)] = &[
    ("needs-you", "Needs you"),
    ("in-flight", "In flight"),
    ("nothing-to-do", "Nothing to do now"),
    ("ready", "Ready"),
    ("waiting", "Waiting on reviewers"),
    ("archived", "Archived"),
];

/// What the State facet can say of a row.
pub const STATES: &[(&str, &str)] = &[
    ("approved", "approved"),
    ("changes-requested", "changes requested"),
    ("mergeable", "mergeable"),
    ("ci-failing", "ci failing"),
    ("conflicts", "conflicts"),
    ("draft-pr", "draft PR"),
    ("unseen", "unseen review"),
];

/// How many of a facet's values show before the rest fold away.
const SHOWN: usize = 5;

/// Which list a row is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Owed,
    Mine,
}

/// What the filter sees of a row.
#[derive(Debug, Clone)]
pub struct RowValues<'a> {
    pub list: Side,
    /// A [`STATUSES`] value.
    pub status: &'static str,
    /// Someone else's PR's author; `None` for yours.
    pub author: Option<&'a str>,
    /// Whoever the status cell counts as having reviewed it.
    pub reviewers: Vec<&'a str>,
    /// `owner/name`.
    pub repo: String,
    /// [`STATES`] values.
    pub states: Vec<&'static str>,
    pub title: &'a str,
    /// `owner/name#N`.
    pub reference: String,
    pub pending: u32,
}

impl RowValues<'_> {
    fn values(&self, facet: Facet) -> Vec<&str> {
        match facet {
            Facet::Status => vec![self.status],
            Facet::Author => self.author.into_iter().collect(),
            Facet::Reviewer => self.reviewers.clone(),
            Facet::Repo => vec![self.repo.as_str()],
            Facet::State => self.states.clone(),
        }
    }

    /// Logins and repos ignore case, as GitHub's do.
    fn has(&self, facet: Facet, value: &str) -> bool {
        self.values(facet).iter().any(|v| match facet {
            Facet::Author | Facet::Reviewer | Facet::Repo => is_login(v, value),
            Facet::Status | Facet::State => *v == value,
        })
    }
}

/// The values picked in each facet, and the words the title or ref must
/// have. Values in a facet are either-or; facets and words all apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// In the order picked, once each.
    picked: Vec<(Facet, String)>,
    /// As typed.
    text: String,
}

impl Filter {
    pub fn is_active(&self) -> bool {
        !self.picked.is_empty() || !self.text.trim().is_empty()
    }

    pub fn picked(&self, facet: Facet) -> impl Iterator<Item = &str> {
        self.picked
            .iter()
            .filter(move |(f, _)| *f == facet)
            .map(|(_, value)| value.as_str())
    }

    fn is_picked(&self, facet: Facet, value: &str) -> bool {
        self.picked(facet).any(|v| same(facet, v, value))
    }

    /// How many facets have a value picked, the words counting as one.
    fn in_use(&self) -> usize {
        let facets = Facet::ALL
            .iter()
            .filter(|facet| self.picked(**facet).next().is_some())
            .count();
        facets + usize::from(!self.text.trim().is_empty())
    }

    pub fn matches(&self, row: &RowValues) -> bool {
        self.matches_but(row, None)
    }

    /// [`Filter::matches`], leaving out what's picked in `skip`.
    fn matches_but(&self, row: &RowValues, skip: Option<Facet>) -> bool {
        let facets = Facet::ALL
            .into_iter()
            .filter(|facet| Some(*facet) != skip && facet.applies(row.list));
        for facet in facets {
            let mut want = self.picked(facet).peekable();
            if want.peek().is_some() && !want.any(|v| row.has(facet, v)) {
                return false;
            }
        }
        let hay = format!("{} {}", row.title, row.reference).to_lowercase();
        self.text
            .split_whitespace()
            .all(|word| hay.contains(&word.to_lowercase()))
    }
}

fn same(facet: Facet, a: &str, b: &str) -> bool {
    match facet {
        Facet::Author | Facet::Reviewer | Facet::Repo => is_login(a, b),
        Facet::Status | Facet::State => a == b,
    }
}

/// The index's URL query: whether archived PRs show, and the filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexQuery {
    pub archived: bool,
    pub filter: Filter,
}

impl IndexQuery {
    /// From a URL's query: `archived=true`, each facet's key once per value
    /// (`author=alice&author=bob`), and `q` for the words. Anything else is
    /// left out.
    pub fn parse(query: Option<&str>) -> Result<Self, String> {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query.unwrap_or(""))
            .map_err(|err| format!("the index's query doesn't parse: {err}"))?;
        let mut out = Self::default();
        let mut words = Vec::new();
        for (key, value) in pairs {
            let value = value.trim();
            match key.as_str() {
                "archived" => match value {
                    "true" => out.archived = true,
                    "false" | "" => out.archived = false,
                    _ => return Err(format!("`archived={value}` isn't true or false")),
                },
                "q" if !value.is_empty() => words.push(value.to_owned()),
                _ if value.is_empty() => {}
                key => {
                    if let Some(facet) = Facet::of_key(key)
                        && !out.filter.is_picked(facet, value)
                    {
                        out.filter.picked.push((facet, value.to_owned()));
                    }
                }
            }
        }
        out.filter.text = words.join(" ");
        // Facets in the sidebar's order, as `href` writes them; a facet's
        // values stay in the order picked, which the URL keeps.
        out.filter
            .picked
            .sort_by_key(|(facet, _)| Facet::ALL.iter().position(|f| f == facet));
        Ok(out)
    }

    /// From an index URL a form sent back, as [`IndexQuery::href`] wrote
    /// it; the unfiltered index for anything else, so the redirect it
    /// makes stays on the index.
    pub fn from_href(href: Option<&str>) -> Self {
        let Some(query) = href.and_then(|h| h.strip_prefix('/')) else {
            return Self::default();
        };
        if query.is_empty() {
            return Self::default();
        }
        query
            .strip_prefix('?')
            .and_then(|q| Self::parse(Some(q)).ok())
            .unwrap_or_default()
    }

    /// The index's URL for this query: `/` when there's nothing to say.
    pub fn href(&self) -> String {
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        if self.archived {
            pairs.push(("archived", "true"));
        }
        for facet in Facet::ALL {
            pairs.extend(self.filter.picked(facet).map(|v| (facet.key(), v)));
        }
        let text = self.filter.text.trim();
        if !text.is_empty() {
            pairs.push(("q", text));
        }
        match serde_urlencoded::to_string(&pairs) {
            Ok(query) if !query.is_empty() => format!("/?{query}"),
            _ => "/".into(),
        }
    }

    pub fn with_archived(&self, archived: bool) -> Self {
        Self {
            archived,
            filter: self.filter.clone(),
        }
    }

    pub fn unfiltered(&self) -> Self {
        Self {
            archived: self.archived,
            filter: Filter::default(),
        }
    }
}

/// `2 of 8` when the filter hides some of `total`, else `total`.
pub fn count(shown: usize, total: usize) -> String {
    if shown == total {
        total.to_string()
    } else {
        format!("{shown} of {total}")
    }
}

/// Under a list's heading: how many rows the filter hides, and a way to
/// clear it; `note` says what of the filter this list ignores.
pub fn hidden_line(query: &IndexQuery, hidden: usize, note: Option<&str>) -> Markup {
    html! {
        @if hidden > 0 || note.is_some() {
            p.fhidden {
                @if hidden > 0 {
                    b { (hidden) } " hidden by filter"
                    @if let Some(note) = note { " · " (note) }
                    " · " a.fclear href=(query.unfiltered().href()) { "clear" }
                } @else if let Some(note) = note {
                    (note)
                }
            }
        }
    }
}

/// One value of a facet in the sidebar.
struct Choice {
    value: String,
    words: String,
    /// Rows it would leave, under the other facets and the words.
    rows: usize,
    picked: bool,
}

/// The sidebar: a form whose query is the index's, sent as you tick a
/// value or type. The lists' refresh replaces [`facets`] and the count in
/// its heading, never the text box, so typing isn't lost.
pub fn sidebar(query: &IndexQuery, rows: &[RowValues], me: &str) -> Markup {
    html! {
        form #filter .fside method="get" action="/" role="search" aria-label="Filter the lists"
            hx-get="/" hx-trigger="change, submit" hx-target="#panes" hx-select="#panes"
            hx-select-oob=(REFRESHED) hx-swap="outerHTML" hx-sync="#panes:replace" {
            // Open, and always so on a wide window; the script folds it on
            // a narrow one.
            details #filter-fold open {
                summary {
                    span.fsh { "Filter" } " " (in_use(&query.filter, me))
                    // Only where it folds: what a click on it does.
                    span.ftoggle aria-hidden="true" {}
                }
                div.fsbody {
                    input #fq type="search" name="q" value=(query.filter.text)
                        placeholder="title or #number" autocomplete="off"
                        aria-label="words in the title or owner/name#N"
                        hx-get="/" hx-trigger="input changed delay:250ms, search"
                        hx-include="#filter";
                    @if query.archived { input type="hidden" name="archived" value="true"; }
                    // Without the script's htmx, a plain GET.
                    button.btn.fapply type="submit" { "Filter" }
                    (facets(query, rows, me))
                }
            }
        }
    }
}

/// What the lists' refresh replaces besides them: the top bar's counts,
/// and the sidebar's facets and the count in its heading.
pub const REFRESHED: &str = "#counts,#facets,#filter-n";

/// The sidebar heading's word on the filter: how many facets are in use
/// beside the lists, and what's picked when the sidebar is over them,
/// where it folds away.
fn in_use(filter: &Filter, me: &str) -> Markup {
    let n = filter.in_use();
    let text = filter.text.trim();
    let picks: Vec<String> = Facet::ALL
        .into_iter()
        .flat_map(|facet| filter.picked(facet).map(move |v| words(facet, v, me)))
        .chain((!text.is_empty()).then(|| format!("“{text}”")))
        .collect();
    html! {
        span #filter-n .fcount {
            @if n > 0 {
                span.fnum { (n) " in use" }
                span.fpicks { (picks.join(", ")) }
            }
        }
    }
}

/// The facets, each value with how many rows it would leave. Everything
/// in them that takes focus has an id, so htmx puts focus back on it when
/// they're replaced.
pub fn facets(query: &IndexQuery, rows: &[RowValues], me: &str) -> Markup {
    let filter = &query.filter;
    html! {
        div #facets {
            @if filter.is_active() {
                a #fclear-all .fsclear.fclear href=(query.unfiltered().href()) { "clear all" }
            }
            @for facet in Facet::ALL {
                @let choices = choices(filter, facet, rows, me);
                @if !choices.is_empty() {
                    @let (shown, more) = fold(&choices);
                    fieldset.facet {
                        legend {
                            (facet.label())
                            @if facet == Facet::Author { " " span.dim { "reviews you owe" } }
                        }
                        @for choice in shown { (option(facet, choice)) }
                        @if !more.is_empty() {
                            details.fmore id={ "facet-more-" (facet.key()) } {
                                summary id={ "facet-more-" (facet.key()) "-open" } {
                                    (more.len()) " more"
                                }
                                @for choice in more { (option(facet, choice)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The values to show, the commonest and every picked one, and the rest,
/// to fold away.
fn fold(choices: &[Choice]) -> (Vec<&Choice>, Vec<&Choice>) {
    let (top, rest) = choices.split_at(choices.len().min(SHOWN));
    let shown = top.iter().chain(rest.iter().filter(|c| c.picked)).collect();
    (shown, rest.iter().filter(|c| !c.picked).collect())
}

fn option(facet: Facet, choice: &Choice) -> Markup {
    let id = format!("f-{}-{}", facet.key(), hex(&choice.value));
    html! {
        label.fopt.zero[choice.rows == 0] for=(id) {
            input id=(id) type="checkbox" name=(facet.key()) value=(choice.value)
                checked[choice.picked];
            span.fv {
                @if facet == Facet::State && choice.value == "unseen" { span.newdot {} " " }
                (choice.words)
            }
            span.fn { (choice.rows) }
        }
    }
}

/// How the sidebar words one of a facet's values.
fn words(facet: Facet, value: &str, me: &str) -> String {
    match facet.fixed() {
        Some(fixed) => fixed
            .iter()
            .find(|(v, _)| *v == value)
            .map_or_else(|| value.to_owned(), |(_, w)| (*w).to_owned()),
        None if facet == Facet::Repo => value.to_owned(),
        None if is_login(value, me) => "you".into(),
        None => format!("@{value}"),
    }
}

/// An id-safe spelling of `value` that's the same on every render, so
/// focus stays on a box the refresh replaced.
fn hex(value: &str) -> String {
    value.bytes().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// A facet's values: the fixed ones in order, else the commonest first,
/// among the rows it filters, and whatever is picked.
fn choices(filter: &Filter, facet: Facet, rows: &[RowValues], me: &str) -> Vec<Choice> {
    let rows: Vec<&RowValues> = rows.iter().filter(|r| facet.applies(r.list)).collect();
    // How often each value comes up, by its lowercase spelling for logins
    // and repos, keeping the first spelling seen.
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut note = |value: &str, n: usize| {
        let fold = match facet {
            Facet::Author | Facet::Reviewer | Facet::Repo => value.to_ascii_lowercase(),
            Facet::Status | Facet::State => value.to_owned(),
        };
        if let Some(i) = at.get(&fold) {
            seen[*i].1 += n;
        } else {
            at.insert(fold, seen.len());
            seen.push((value.to_owned(), n));
        }
    };
    for row in &rows {
        for value in row.values(facet) {
            note(value, 1);
        }
    }
    for value in filter.picked(facet) {
        note(value, 0);
    }
    match facet.fixed() {
        Some(fixed) => seen.sort_by_key(|(value, _)| {
            fixed
                .iter()
                .position(|(v, _)| v == value)
                .unwrap_or(fixed.len())
        }),
        None => seen.sort_by(|(a, m), (b, n)| n.cmp(m).then_with(|| a.cmp(b))),
    }
    seen.into_iter()
        .map(|(value, _)| {
            let rows = rows
                .iter()
                .filter(|r| r.has(facet, &value) && filter.matches_but(r, Some(facet)))
                .count();
            let words = words(facet, &value, me);
            Choice {
                picked: filter.is_picked(facet, &value),
                value,
                words,
                rows,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(list: Side, author: Option<&'a str>, reviewers: Vec<&'a str>) -> RowValues<'a> {
        RowValues {
            list,
            status: "needs-you",
            author,
            reviewers,
            repo: "org/repo".into(),
            states: vec!["approved"],
            title: "Add the retry budget",
            reference: "org/repo#7".into(),
            pending: 0,
        }
    }

    fn query(q: &str) -> IndexQuery {
        IndexQuery::parse(Some(q)).unwrap()
    }

    #[test]
    fn the_query_round_trips_in_the_sidebars_order() {
        let q = query("q=retry&author=bob&archived=true&author=Alice&status=ready&author=alice");
        assert_eq!(
            q.href(),
            "/?archived=true&status=ready&author=bob&author=Alice&q=retry"
        );
        assert_eq!(IndexQuery::from_href(Some(&q.href())), q);
        assert_eq!(query("author=&q=+&archived=false").href(), "/");
        assert!(IndexQuery::parse(Some("archived=yes")).is_err());
    }

    #[test]
    fn a_sent_back_href_stays_on_the_index() {
        for href in [
            "https://elsewhere.example/?author=x",
            "//elsewhere.example/",
            "?author=x",
        ] {
            assert_eq!(IndexQuery::from_href(Some(href)).href(), "/", "{href}");
        }
        assert_eq!(
            IndexQuery::from_href(Some("/?author=x&nope=1")).href(),
            "/?author=x"
        );
    }

    #[test]
    fn values_are_either_or_and_facets_all_apply() {
        let alice = row(Side::Owed, Some("alice"), vec!["carol"]);
        let bob = row(Side::Owed, Some("bob"), vec![]);
        let f = query("author=ALICE&author=bob").filter;
        assert!(f.matches(&alice) && f.matches(&bob));
        let f = query("author=alice&reviewer=dave").filter;
        assert!(!f.matches(&alice));
        let f = query("reviewer=carol&q=RETRY+%237").filter;
        assert!(f.matches(&alice) && !f.matches(&bob));
        assert!(!query("q=nope").filter.matches(&alice));
    }

    #[test]
    fn author_leaves_your_prs_alone() {
        let yours = row(Side::Mine, None, vec!["carol"]);
        assert!(query("author=alice").filter.matches(&yours));
        assert!(!query("author=alice&reviewer=dave").filter.matches(&yours));
    }

    #[test]
    fn a_facets_counts_leave_out_its_own_picks() {
        let rows = [
            row(Side::Owed, Some("alice"), vec![]),
            row(Side::Owed, Some("bob"), vec!["alice"]),
            row(Side::Mine, None, vec!["alice"]),
        ];
        let f = query("author=alice").filter;
        let authors = choices(&f, Facet::Author, &rows, "me");
        let got: Vec<_> = authors
            .iter()
            .map(|c| (c.words.as_str(), c.rows, c.picked))
            .collect();
        assert_eq!(got, [("@alice", 1, true), ("@bob", 1, false)]);
        // Only alice's PR is left among those you owe, and none of yours
        // is filtered by author.
        let reviewers = choices(&f, Facet::Reviewer, &rows, "me");
        assert_eq!(reviewers[0].rows, 1);
        let picked_gone = choices(&query("repo=org/gone").filter, Facet::Repo, &rows, "me");
        let got: Vec<_> = picked_gone
            .iter()
            .map(|c| (c.value.as_str(), c.rows))
            .collect();
        assert_eq!(got, [("org/repo", 3), ("org/gone", 0)]);
    }
}
