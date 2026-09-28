//! The config editor's keys and what it draws.

use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
};

use super::*;
use crate::poll::tests::NoCheckouts;
use std::time::SystemTime;

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

fn text(editor: &ConfigEditor) -> Option<&str> {
    editor.typing.as_ref().map(|t| t.text.as_str())
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
    assert_eq!(text(&editor), Some("30"));
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "4x5");
    assert_eq!(text(&editor), Some("45"), "only digits");
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

#[test]
fn list_items_are_added_edited_moved_and_removed() {
    let mut editor = editor(CONFIG);
    // review_requests.teams, unset: + starts the list.
    select(&mut editor, 2, 0);
    let _ = press(&mut editor, KeyCode::Char('+'));
    typed(&mut editor, "*");
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = press(&mut editor, KeyCode::Char('+'));
    typed(&mut editor, "!org/storage");
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(
        editor
            .doc
            .text()
            .contains("teams = [\"*\", \"!org/storage\"]"),
        "{}",
        editor.doc.text()
    );
    assert_eq!(editor.row, 1, "on what was added");
    insta::assert_snapshot!("lists", draw(&editor).backend());

    // Order matters: the last match wins.
    let _ = press(&mut editor, KeyCode::Char('K'));
    assert_eq!(editor.row, 0);
    assert!(
        editor
            .doc
            .text()
            .contains("teams = [\"!org/storage\", \"*\"]")
    );
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "!org/x");
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = press(&mut editor, KeyCode::Char('-'));
    assert!(
        editor.doc.text().contains("teams = [\"*\"]"),
        "{}",
        editor.doc.text()
    );
    // A blank edit removes the item too.
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = ctrl(&mut editor, 'u');
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(
        editor.doc.text().contains("teams = []"),
        "{}",
        editor.doc.text()
    );
    let _ = press(&mut editor, KeyCode::Char('u'));
    assert!(
        !editor.doc.text().contains("teams"),
        "{}",
        editor.doc.text()
    );
}

#[test]
fn repo_entries_open_switch_kind_and_take_globs() {
    let mut editor = editor(CONFIG);
    // profile.ring's repos, after its name and six keys.
    select(&mut editor, 4, 7);
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(editor.entry.is_some());
    // github → checkout, which needs a path.
    let _ = press(&mut editor, KeyCode::Char(' '));
    assert!(
        editor.doc.text().contains("repos = [\"\"]"),
        "{}",
        editor.doc.text()
    );
    let _ = press(&mut editor, KeyCode::Char(' '));
    let _ = press(&mut editor, KeyCode::Char('j'));
    let _ = press(&mut editor, KeyCode::Enter);
    typed(&mut editor, "/src/services");
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = press(&mut editor, KeyCode::Char('+'));
    typed(&mut editor, "/documentation/**");
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(
        editor
            .doc
            .text()
            .contains("repos = [{ repo = \"/src/services\", paths = [\"/documentation/**\"] }]"),
        "{}",
        editor.doc.text()
    );
    insta::assert_snapshot!("entry", draw(&editor).backend());
    // Round through github and back: the path comes back with it.
    let _ = press(&mut editor, KeyCode::Char('g'));
    for _ in 0..3 {
        let _ = press(&mut editor, KeyCode::Char(' '));
    }
    assert!(
        editor
            .doc
            .text()
            .contains("repos = [{ repo = \"/src/services\""),
        "{}",
        editor.doc.text()
    );
    let _ = press(&mut editor, KeyCode::Esc);
    assert!(editor.entry.is_none());

    // + adds one, once it names something.
    let _ = press(&mut editor, KeyCode::Char('+'));
    let _ = press(&mut editor, KeyCode::Esc);
    assert!(
        editor
            .doc
            .text()
            .contains("paths = [\"/documentation/**\"] }]")
    );
    let _ = press(&mut editor, KeyCode::Char('k'));
    let _ = press(&mut editor, KeyCode::Char(' '));
    let _ = press(&mut editor, KeyCode::Char(' '));
    let _ = press(&mut editor, KeyCode::Char('j'));
    let _ = press(&mut editor, KeyCode::Enter);
    typed(&mut editor, "org");
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = press(&mut editor, KeyCode::Esc);
    assert!(
        editor.doc.text().contains(", { github = \"org\" }]"),
        "{}",
        editor.doc.text()
    );
    assert_eq!(editor.current_row(), Some(Row::Entry("ring".into(), 1)));
    let _ = press(&mut editor, KeyCode::Char('K'));
    let _ = press(&mut editor, KeyCode::Char('-'));
    assert!(
        editor
            .doc
            .text()
            .contains("repos = [{ repo = \"/src/services\""),
        "{}",
        editor.doc.text()
    );
}

#[test]
fn profiles_are_added_renamed_moved_and_removed_asking_first() {
    let mut editor = editor(CONFIG);
    editor.focus = Focus::Tables;
    let _ = press(&mut editor, KeyCode::Char('+'));
    typed(&mut editor, "extra");
    let _ = press(&mut editor, KeyCode::Enter);
    assert_eq!(editor.doc.profiles(), ["ring", "extra"]);
    assert_eq!(editor.current_table(), Table::Profile("extra".into()));
    let _ = press(&mut editor, KeyCode::Char('K'));
    assert_eq!(editor.doc.profiles(), ["extra", "ring"]);
    assert_eq!(editor.current_table(), Table::Profile("extra".into()));

    // Its name is its first row.
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "first");
    let _ = press(&mut editor, KeyCode::Enter);
    assert_eq!(editor.doc.profiles(), ["first", "ring"]);

    editor.focus = Focus::Tables;
    let _ = press(&mut editor, KeyCode::Char('-'));
    insta::assert_snapshot!("remove_profile", draw(&editor).backend());
    let _ = press(&mut editor, KeyCode::Char('n'));
    assert_eq!(editor.doc.profiles(), ["first", "ring"]);
    let _ = press(&mut editor, KeyCode::Char('-'));
    let _ = press(&mut editor, KeyCode::Char('y'));
    assert_eq!(editor.doc.profiles(), ["ring"]);
    // Sections can't be removed.
    let _ = press(&mut editor, KeyCode::Char('g'));
    let _ = press(&mut editor, KeyCode::Char('-'));
    assert!(editor.popup.is_none());
}

#[test]
fn tab_completes_paths_as_they_are_typed() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("skills/review")).unwrap();
    let mut editor = editor(CONFIG);
    // profile.ring's skills.
    select(&mut editor, 4, 2);
    let _ = press(&mut editor, KeyCode::Char('+'));
    typed(&mut editor, &format!("{}/sk", dir.path().display()));
    let _ = press(&mut editor, KeyCode::Tab);
    let _ = press(&mut editor, KeyCode::Tab);
    assert_eq!(
        text(&editor),
        Some(format!("{}/skills/review/", dir.path().display()).as_str())
    );
    // Only paths complete.
    let _ = press(&mut editor, KeyCode::Esc);
    select(&mut editor, 3, 4);
    let _ = press(&mut editor, KeyCode::Enter);
    typed(&mut editor, "/");
    let _ = press(&mut editor, KeyCode::Tab);
    assert_eq!(text(&editor), Some("/"));
}

#[test]
fn the_selection_stays_on_a_list_it_adds_to_or_removes_from() {
    let mut editor = editor(CONFIG);
    let teams = Key::new(Table::ReviewRequests, "teams").unwrap();
    select(&mut editor, 2, 0);
    for team in ["*", "!org/x"] {
        let _ = press(&mut editor, KeyCode::Char('+'));
        typed(&mut editor, team);
        let _ = press(&mut editor, KeyCode::Enter);
    }
    // Esc drops the row being added, not the selection onto the next key.
    let _ = press(&mut editor, KeyCode::Char('+'));
    let _ = press(&mut editor, KeyCode::Esc);
    assert_eq!(editor.current_row(), Some(Row::Item(teams.clone(), 1)));
    // Removing the last item leaves the one before it selected.
    let _ = press(&mut editor, KeyCode::Char('-'));
    assert_eq!(editor.current_row(), Some(Row::Item(teams.clone(), 0)));
    let _ = press(&mut editor, KeyCode::Char('-'));
    assert_eq!(editor.current_row(), Some(Row::List(teams)));
}

#[test]
fn counts_show_what_the_config_watches_and_each_entrys_share() {
    let text = CONFIG.replace(
        "repos = [{ github = \"org\" }]",
        "repos = [{ github = \"org\" }, { github = \"org/api\", paths = [\"v/**\"] }]",
    );
    let mut editor = editor(&text);
    select(&mut editor, 4, 7);
    let now = SystemTime::UNIX_EPOCH;
    let wanted = editor.want(now);
    insta::assert_snapshot!("counting", draw(&editor).backend());
    // Answer everything: each count is its search's length, so they differ.
    let answers = wanted
        .iter()
        .map(|q| {
            let answer = match q {
                Query::Teams => Answer::Teams(vec![sanic_core::pr::TeamRef::new("org", "x")]),
                Query::RepoNames(_) => Answer::RepoNames {
                    repos: std::collections::BTreeSet::from([sanic_core::repo::RepoName::new(
                        "org", "api",
                    )]),
                    complete: true,
                },
                Query::Prs(q) | Query::Repos(q) => {
                    Answer::Count(u32::try_from(q.len()).unwrap() % 50)
                }
            };
            Counted::Answer(q.clone(), answer)
        })
        .collect();
    editor.counted(answers);
    editor.set_serve_window(Some(30));
    insta::assert_snapshot!("counted", draw(&editor).backend());

    // Rate limited, the counts say so.
    editor.counted(vec![Counted::Stopped(Stopped::RateLimited(
        std::time::Duration::from_secs(60),
    ))]);
    let screen = format!("{:?}", draw(&editor).backend());
    assert!(screen.contains("rate limited for 60s"), "{screen}");
}

#[test]
fn a_failed_search_stays_on_the_counts_line_until_its_counted_again() {
    let mut editor = editor(CONFIG);
    let wanted = editor.want(SystemTime::UNIX_EPOCH);
    let owed = wanted
        .iter()
        .find(|q| matches!(q, Query::Prs(_)))
        .unwrap()
        .clone();
    let others = wanted.iter().filter(|q| **q != owed).map(|q| {
        let answer = match q {
            Query::Teams => Answer::Teams(Vec::new()),
            Query::RepoNames(_) => Answer::RepoNames {
                repos: std::collections::BTreeSet::new(),
                complete: true,
            },
            _ => Answer::Count(1),
        };
        Counted::Answer(q.clone(), answer)
    });
    // A rate limit waited out, then a failure: the failure's what shows.
    editor.counted(vec![Counted::Stopped(Stopped::RateLimited(
        std::time::Duration::from_secs(60),
    ))]);
    editor.counted(vec![Counted::Failed(owed.clone(), "422".into())]);
    // Other answers coming in don't clear it.
    editor.counted(others.collect());
    let screen = format!("{:?}", draw(&editor).backend());
    assert!(screen.contains("counting failed: 422"), "{screen}");
    assert!(screen.contains("you owe —"), "{screen}");
    assert!(!screen.contains("rate limited"), "{screen}");
    // Waiting on serve says so over it.
    editor.counted(vec![Counted::Waiting]);
    let screen = format!("{:?}", draw(&editor).backend());
    assert!(screen.contains("waiting out serve"), "{screen}");
    editor.counted(vec![Counted::Answer(owed, Answer::Count(2))]);
    let screen = format!("{:?}", draw(&editor).backend());
    assert!(!screen.contains("counting failed"), "{screen}");
    assert!(screen.contains("you owe 2"), "{screen}");
}

#[test]
fn saves_are_written_off_the_ui_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let checker = Checker::new(path.clone(), std::sync::Arc::new(NoCheckouts));
    let mut editor = editor(CONFIG);
    select(&mut editor, 1, 2);
    let _ = press(&mut editor, KeyCode::Char('u'));
    settle(&mut editor);
    checker.save(editor.doc().clone());
    editor.saving();
    let screen = format!("{:?}", draw(&editor).backend());
    assert!(screen.contains("… saving"), "{screen}");
    assert_eq!(ctrl(&mut editor, 's'), Outcome::Open);
    assert_eq!(editor.notice.as_deref(), Some("still saving"));
    // Nor edits, nor leaving, until it's written.
    assert_eq!(press(&mut editor, KeyCode::Char('q')), Outcome::Open);
    assert_eq!(editor.notice.as_deref(), Some("still saving"));
    let saved = loop {
        if let Some(saved) = checker.saved() {
            break saved;
        }
        std::thread::yield_now();
    };
    editor.saved(saved);
    assert!(!editor.saving);
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("quiet_secs")
    );
}

#[test]
fn entry_counts_follow_their_entry_and_skip_ones_not_yet_loaded() {
    let text = CONFIG.replace(
        "repos = [{ github = \"org\" }]",
        "repos = [{ github = \"org\" }, { github = \"else/api\" }]",
    );
    let mut editor = editor(&text);
    select(&mut editor, 4, 8);
    let _ = editor.want(SystemTime::UNIX_EPOCH);
    let covers = |editor: &ConfigEditor, n| {
        editor
            .planned_entry("ring", n)
            .map(|entry| entry.id.covers.clone())
    };
    assert_eq!(
        covers(&editor, 1),
        Some(counts::Covers::Repo(sanic_core::repo::RepoName::new(
            "else", "api"
        )))
    );
    // Moved up, before the move is checked, it keeps its own counts.
    let _ = press(&mut editor, KeyCode::Char('K'));
    assert_eq!(editor.check, Check::Checking);
    assert_eq!(
        covers(&editor, 0),
        Some(counts::Covers::Repo(sanic_core::repo::RepoName::new(
            "else", "api"
        )))
    );
    assert_eq!(covers(&editor, 1), Some(counts::Covers::Org("org".into())));
    // One the config didn't have when it loaded shows none.
    let _ = press(&mut editor, KeyCode::Char('+'));
    let _ = press(&mut editor, KeyCode::Esc);
    let _ = press(&mut editor, KeyCode::Char('k'));
    let _ = press(&mut editor, KeyCode::Char(' '));
    let _ = press(&mut editor, KeyCode::Char(' '));
    let _ = press(&mut editor, KeyCode::Char('j'));
    let _ = press(&mut editor, KeyCode::Enter);
    typed(&mut editor, "new-org");
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = press(&mut editor, KeyCode::Esc);
    assert_eq!(covers(&editor, 2), None);
}

#[test]
fn entries_on_one_checkout_through_two_remotes_keep_their_own_counts() {
    struct ByRemote;
    impl sanic_core::config::CheckoutResolver for ByRemote {
        fn resolve(
            &self,
            _: &Path,
            remote: Option<&str>,
        ) -> color_eyre::eyre::Result<(sanic_core::config::Vcs, sanic_core::repo::RepoName)>
        {
            let owner = remote.unwrap_or("origin");
            Ok((
                sanic_core::config::Vcs::Git,
                sanic_core::repo::RepoName::new(owner, "src"),
            ))
        }
    }
    let text = CONFIG.replace(
        "repos = [{ github = \"org\" }]",
        "repos = [{ repo = \"/src\", remote = \"fork\" }, { repo = \"/src\", remote = \"up\" }]",
    );
    let mut editor = editor(&text);
    let Outcome::Check { generation, text } = editor.check_now() else {
        unreachable!();
    };
    let path = Path::new("/c/config.toml");
    editor.checked(generation, check::check(&text, path, &ByRemote));
    select(&mut editor, 4, 8);
    let _ = editor.want(SystemTime::UNIX_EPOCH);
    let scope = |n| {
        editor
            .planned_entry("ring", n)
            .map(|entry| entry.prs[1].clone())
    };
    let (Some(counts::Share::Search(fork)), Some(counts::Share::Search(up))) = (scope(0), scope(1))
    else {
        panic!("{:?} {:?}", scope(0), scope(1));
    };
    assert!(fork.contains("repo:fork/src"), "{fork}");
    assert!(up.contains("repo:up/src"), "{up}");
}
