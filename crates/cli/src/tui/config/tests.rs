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

fn text(editor: &ConfigEditor) -> Option<String> {
    editor.typing.as_ref().map(|t| t.field.text())
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

fn key_of(table: Table, name: &str) -> Key {
    Key::new(table, name).unwrap()
}

/// Selects `table`'s `row`th row: its keys' for a section, and for a
/// profile its header at 0, then its keys.
fn select(editor: &mut ConfigEditor, table: usize, row: usize) {
    let table = editor.tables()[table].clone();
    let profile = matches!(table, Table::Profile(_));
    editor.select(&Row::Header(table));
    let steps = if profile { row } else { row + 1 };
    for _ in 0..steps {
        let _ = press(editor, KeyCode::Char('j'));
    }
}

#[test]
fn shows_every_key_with_defaults_for_the_unset_ones() {
    let mut editor = editor(CONFIG);
    select(&mut editor, 1, 2);
    insta::assert_snapshot!(draw(&editor).backend());
    // A profile's unset keys show what they inherit.
    select(&mut editor, 5, 2);
    insta::assert_snapshot!("profile", draw(&editor).backend());
}

#[test]
fn numbers_are_typed_set_and_unset_then_checked() {
    let mut editor = editor(CONFIG);
    select(&mut editor, 1, 2);
    assert_eq!(press(&mut editor, KeyCode::Enter), Outcome::Open);
    assert_eq!(text(&editor).as_deref(), Some("30"));
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "4x5");
    assert_eq!(text(&editor).as_deref(), Some("45"), "only digits");
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
fn fields_take_vi_keys_when_the_config_or_the_guess_says_so() {
    let screen = |editor: &ConfigEditor| draw(editor).backend().to_string();
    // Unset, the guess counts.
    let mut editor = editor(CONFIG);
    editor.set_guessed_keys(Keys::Vi);
    select(&mut editor, 1, 2);
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(
        screen(&editor).contains("-- INSERT --"),
        "{}",
        screen(&editor)
    );
    assert!(screen(&editor).contains("Esc normal mode"));
    // The first Esc is vi's, to normal mode; `0` then `x` edit there.
    let _ = press(&mut editor, KeyCode::Esc);
    assert!(screen(&editor).contains("-- NORMAL --"));
    assert!(screen(&editor).contains("Esc cancel"));
    typed(&mut editor, "0x");
    assert_eq!(
        text(&editor).as_deref(),
        Some("0"),
        "x deletes, not a digit"
    );
    insta::assert_snapshot!("vi_normal", draw(&editor).backend());
    // The second Esc leaves, setting nothing.
    let _ = press(&mut editor, KeyCode::Esc);
    assert_eq!(text(&editor), None);
    assert!(!editor.doc.is_changed());

    // The config's own setting beats the guess, and takes effect on the
    // next field typed into.
    let mut editor = self::editor(&format!("[tui]\nkeys = \"emacs\"\n{CONFIG}"));
    editor.set_guessed_keys(Keys::Vi);
    select(&mut editor, 1, 2);
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(!screen(&editor).contains("-- INSERT --"));
    let _ = ctrl(&mut editor, 'a');
    typed(&mut editor, "1");
    assert_eq!(text(&editor).as_deref(), Some("130"));
    // A number field refuses letters, not Alt's moves.
    let _ = press(&mut editor, KeyCode::End);
    let _ = editor.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
    typed(&mut editor, "2");
    assert_eq!(text(&editor).as_deref(), Some("2130"));
    assert_eq!(press(&mut editor, KeyCode::Esc), Outcome::Open);
    assert_eq!(text(&editor), None, "Esc cancels at once");
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
    // What's typed shows once, not on the unset list's row as well.
    let screen = draw(&editor).backend().to_string();
    assert_eq!(screen.matches("*▏").count(), 1, "{screen}");
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
    assert_eq!(
        editor.current_row(),
        Some(Row::Item(key_of(Table::ReviewRequests, "teams"), 1)),
        "on what was added"
    );
    insta::assert_snapshot!("lists", draw(&editor).backend());

    // Order matters: the last match wins.
    let _ = press(&mut editor, KeyCode::Char('K'));
    assert_eq!(
        editor.current_row(),
        Some(Row::Item(key_of(Table::ReviewRequests, "teams"), 0))
    );
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
    select(&mut editor, 5, 7);
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
    // `+` on the row after the tables.
    let _ = press(&mut editor, KeyCode::Char('G'));
    let _ = press(&mut editor, KeyCode::Char('+'));
    typed(&mut editor, "extra");
    let _ = press(&mut editor, KeyCode::Enter);
    assert_eq!(editor.doc.profiles(), ["ring", "extra"]);
    assert_eq!(editor.current_table(), Table::Profile("extra".into()));
    let _ = press(&mut editor, KeyCode::Char('K'));
    assert_eq!(editor.doc.profiles(), ["extra", "ring"]);
    assert_eq!(editor.current_table(), Table::Profile("extra".into()));

    // Enter on its header renames it.
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "first");
    let _ = press(&mut editor, KeyCode::Enter);
    assert_eq!(editor.doc.profiles(), ["first", "ring"]);
    assert_eq!(
        editor.current_row(),
        Some(Row::Header(Table::Profile("first".into())))
    );
    // A rename to another profile's name is refused, and stays on it.
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "ring");
    let _ = press(&mut editor, KeyCode::Enter);
    assert_eq!(editor.doc.profiles(), ["first", "ring"]);
    assert_eq!(
        editor.current_row(),
        Some(Row::Header(Table::Profile("first".into())))
    );
    editor.notice = None;
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
    // Tab and ] step to the next table's header, [ back.
    let _ = press(&mut editor, KeyCode::Tab);
    assert_eq!(editor.current_row(), Some(Row::Header(Table::Poll)));
    let _ = press(&mut editor, KeyCode::Char(']'));
    let _ = press(&mut editor, KeyCode::Char('['));
    assert_eq!(editor.current_row(), Some(Row::Header(Table::Poll)));
}

#[test]
fn tab_completes_paths_as_they_are_typed() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("skills/review")).unwrap();
    let mut editor = editor(CONFIG);
    // profile.ring's skills.
    select(&mut editor, 5, 2);
    let _ = press(&mut editor, KeyCode::Char('+'));
    typed(&mut editor, &format!("{}/sk", dir.path().display()));
    let _ = press(&mut editor, KeyCode::Tab);
    let _ = press(&mut editor, KeyCode::Tab);
    assert_eq!(
        text(&editor),
        Some(format!("{}/skills/review/", dir.path().display()))
    );
    // Only paths complete.
    let _ = press(&mut editor, KeyCode::Esc);
    select(&mut editor, 3, 4);
    let _ = press(&mut editor, KeyCode::Enter);
    typed(&mut editor, "/");
    let _ = press(&mut editor, KeyCode::Tab);
    assert_eq!(text(&editor).as_deref(), Some("/"));
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
    select(&mut editor, 5, 7);
    let now = SystemTime::UNIX_EPOCH;
    let wanted = editor.want(now);
    insta::assert_snapshot!("counting", draw(&editor).backend());
    // Answer everything: each count is its search's length, so they differ.
    let answers = wanted
        .iter()
        .map(|q| {
            let answer = match q {
                Query::Teams => Answer::Teams(vec![sanic_core::pr::TeamRef::new("org", "x")]),
                Query::Orgs => Answer::Orgs(vec!["org".into()]),
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
    // Narrower, each bullet wraps under its own text, inside the box.
    let mut narrow = Terminal::new(TestBackend::new(50, 24)).unwrap();
    narrow.draw(|frame| editor.render(frame)).unwrap();
    insta::assert_snapshot!("counted_narrow", narrow.backend());
    // The footer drops whole hints that don't fit, but keeps `?`, which
    // lists them.
    let footer = narrow.backend().to_string();
    let footer = footer.lines().rev().nth(1).unwrap();
    assert!(footer.contains("? keys"), "{footer}");
    assert!(!footer.contains("u uns"), "{footer}");

    // Rate limited, the counts say so.
    editor.counted(vec![Counted::Stopped(Stopped::RateLimited(
        std::time::Duration::from_secs(60),
    ))]);
    let said = effects(&editor);
    assert!(said.contains("GitHub rate limited the counts"), "{said}");
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
    let said = effects(&editor);
    assert!(said.contains("A count failed: 422"), "{said}");
    assert!(
        said.contains("Couldn't count the reviews you're asked for"),
        "{said}"
    );
    assert!(!said.contains("rate limited"), "{said}");
    // Waiting on serve says so over it.
    editor.counted(vec![Counted::Waiting]);
    assert!(effects(&editor).contains("Waiting for serve's rate limit"));
    editor.counted(vec![Counted::Answer(owed, Answer::Count(2))]);
    let said = effects(&editor);
    assert!(!said.contains("count failed"), "{said}");
    assert!(said.contains("· Matches 2 reviews"), "{said}");
}

/// What the Effects block says, as one text.
fn effects(editor: &ConfigEditor) -> String {
    editor
        .effects()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    select(&mut editor, 5, 8);
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
    select(&mut editor, 5, 8);
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

fn answer(editor: &mut ConfigEditor, query: Query, answer: Answer) {
    editor.counted(vec![Counted::Answer(query, answer)]);
}

#[test]
fn f_ticks_your_teams_into_the_filter() {
    let mut editor = editor(CONFIG);
    select(&mut editor, 2, 0);
    let _ = press(&mut editor, KeyCode::Char('f'));
    assert_eq!(editor.notice.as_deref(), Some("your teams aren't in yet"));
    answer(
        &mut editor,
        Query::Teams,
        Answer::Teams(vec![
            sanic_core::pr::TeamRef::new("org", "zone"),
            sanic_core::pr::TeamRef::new("org", "storage"),
        ]),
    );
    let _ = press(&mut editor, KeyCode::Char('f'));
    insta::assert_snapshot!("teams", draw(&editor).backend());
    let _ = press(&mut editor, KeyCode::Char(' '));
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(
        editor
            .doc
            .text()
            .contains("teams = [\"*\", \"!org/storage\"]"),
        "{}",
        editor.doc.text()
    );
}

#[test]
fn f_on_repos_suggests_orgs_and_scans_for_checkouts() {
    let mut editor = editor(CONFIG);
    answer(
        &mut editor,
        Query::Orgs,
        Answer::Orgs(vec!["org".into(), "other".into()]),
    );
    select(&mut editor, 5, 7);
    let _ = press(&mut editor, KeyCode::Char('f'));
    let Some(Popup::Suggest(suggest)) = &editor.popup else {
        panic!("no suggestions");
    };
    let labels: Vec<&str> = suggest.rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["other"], "org is watched already");
    // Type another directory to scan, then scan it.
    let _ = press(&mut editor, KeyCode::Char('d'));
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "/src");
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = press(&mut editor, KeyCode::Char('<'));
    assert_eq!(
        press(&mut editor, KeyCode::Char('s')),
        Outcome::Find(Find::Checkouts {
            root: "/src".into(),
            depth: 2
        })
    );
    editor.found(Found::Checkouts(vec![discover::scan::Found {
        path: PathBuf::from("/src/tool"),
        repo: sanic_core::repo::RepoName::new("Else", "tool"),
    }]));
    insta::assert_snapshot!("repos_found", draw(&editor).backend());
    let _ = press(&mut editor, KeyCode::Char('G'));
    let _ = press(&mut editor, KeyCode::Char('j'));
    let _ = press(&mut editor, KeyCode::Char('j'));
    let _ = press(&mut editor, KeyCode::Char(' '));
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(
        editor
            .doc
            .text()
            .contains("repos = [{ github = \"org\" }, \"/src/tool\"]"),
        "{}",
        editor.doc.text()
    );
}

#[test]
fn models_are_suggested_and_complete_as_typed() {
    let mut editor = editor(CONFIG);
    editor.set_models(vec!["auto".into(), "opus".into(), "claude-sonnet-5".into()]);
    // runner.model
    select(&mut editor, 3, 4);
    let _ = press(&mut editor, KeyCode::Char('f'));
    let _ = press(&mut editor, KeyCode::Char('j'));
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(
        editor.doc.text().contains("model = \"opus\""),
        "{}",
        editor.doc.text()
    );
    let _ = press(&mut editor, KeyCode::Enter);
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "Cl");
    let _ = press(&mut editor, KeyCode::Tab);
    assert_eq!(text(&editor).as_deref(), Some("claude-sonnet-5"));
}

/// The dropdown's options, when it's open.
fn menu(editor: &ConfigEditor) -> Vec<String> {
    editor
        .typing
        .as_ref()
        .filter(|t| t.menu.is_open())
        .map(|t| t.menu.options.clone())
        .unwrap_or_default()
}

#[test]
fn a_dropdown_lists_what_a_field_could_hold_as_you_type() {
    let mut editor = editor(CONFIG);
    editor.set_models(
        ["auto", "opus", "claude-opus-5-5", "claude-sonnet-5"]
            .map(String::from)
            .to_vec(),
    );
    // runner.model
    select(&mut editor, 3, 4);
    let _ = press(&mut editor, KeyCode::Enter);
    assert_eq!(menu(&editor).len(), 4, "every model, before typing");
    typed(&mut editor, "op");
    assert_eq!(menu(&editor), ["opus", "claude-opus-5-5"]);
    insta::assert_snapshot!("dropdown", draw(&editor).backend());
    // Enter sets what's typed until ↑ or ↓ chooses; then it takes that.
    let _ = press(&mut editor, KeyCode::Down);
    let _ = press(&mut editor, KeyCode::Down);
    assert_eq!(press(&mut editor, KeyCode::Enter), Outcome::Open);
    assert_eq!(text(&editor).as_deref(), Some("claude-opus-5-5"));
    assert!(menu(&editor).is_empty(), "nothing goes on from it");
    // Esc closes the dropdown and keeps typing; typing opens it again.
    let _ = ctrl(&mut editor, 'u');
    typed(&mut editor, "cl");
    let _ = press(&mut editor, KeyCode::Esc);
    assert!(menu(&editor).is_empty());
    assert_eq!(text(&editor).as_deref(), Some("cl"));
    typed(&mut editor, "a");
    assert_eq!(menu(&editor), ["claude-opus-5-5", "claude-sonnet-5"]);
    // Tab takes the first when none's chosen.
    let _ = press(&mut editor, KeyCode::Tab);
    assert_eq!(text(&editor).as_deref(), Some("claude-opus-5-5"));
    let Outcome::Check { text, .. } = press(&mut editor, KeyCode::Enter) else {
        panic!("not set");
    };
    assert!(text.contains("model = \"claude-opus-5-5\""), "{text}");
}

#[test]
fn the_dropdown_offers_teams_choices_and_what_f_finds() {
    let mut editor = editor(CONFIG);
    // Your teams, `!` as typed.
    select(&mut editor, 2, 0);
    let _ = press(&mut editor, KeyCode::Enter);
    answer(
        &mut editor,
        Query::Teams,
        Answer::Teams(vec![sanic_core::pr::TeamRef::new("org", "platform")]),
    );
    assert_eq!(menu(&editor), ["org/platform"], "as the teams come in");
    typed(&mut editor, "!pl");
    assert_eq!(menu(&editor), ["!org/platform"]);
    let _ = press(&mut editor, KeyCode::Esc);
    let _ = press(&mut editor, KeyCode::Esc);

    // A key's few values.
    select(&mut editor, 4, 0);
    let _ = press(&mut editor, KeyCode::Enter);
    assert_eq!(menu(&editor), ["emacs", "vi"]);
    let _ = press(&mut editor, KeyCode::Esc);
    let _ = press(&mut editor, KeyCode::Esc);

    // A profile's skills, found once for the dropdown as `f` finds them.
    select(&mut editor, 5, 2);
    let Outcome::Find(Find::Extras { profile, .. }) = press(&mut editor, KeyCode::Char('+')) else {
        panic!("nothing to find");
    };
    assert!(menu(&editor).is_empty());
    editor.found(Found::Extras {
        profile,
        skills: vec![discover::skills::Skill {
            dir: PathBuf::from("/src/.claude/skills/review"),
            name: "review".into(),
            description: None,
        }],
        instructions: Vec::new(),
    });
    assert_eq!(menu(&editor), ["/src/.claude/skills/review"]);
    let _ = press(&mut editor, KeyCode::Esc);
    let _ = press(&mut editor, KeyCode::Esc);
    assert_eq!(
        press(&mut editor, KeyCode::Char('+')),
        Outcome::Open,
        "found already"
    );
    assert_eq!(menu(&editor), ["/src/.claude/skills/review"]);

    // A github entry's orgs and the repos counted.
    answer(&mut editor, Query::Orgs, Answer::Orgs(vec!["Other".into()]));
    answer(
        &mut editor,
        Query::RepoNames("q".into()),
        Answer::RepoNames {
            repos: std::collections::BTreeSet::from([sanic_core::repo::RepoName::new(
                "org", "api",
            )]),
            complete: true,
        },
    );
    editor.entry = Some(EntryEdit {
        profile: "ring".into(),
        entry: RepoEntry::Github {
            name: String::new(),
            paths: Vec::new(),
        },
        in_doc: false,
        row: 1,
        set_aside: String::new(),
        globs_aside: Vec::new(),
    });
    assert_eq!(
        editor.options(&Input::Entry(EntryRow::Target), "o"),
        ["other", "org/api"],
        "org is watched already"
    );
}

#[test]
fn f_on_skills_asks_for_whats_in_the_checkouts_and_adds_what_you_pick() {
    let mut editor = editor(CONFIG);
    select(&mut editor, 5, 2);
    let Outcome::Find(Find::Extras { profile, .. }) = press(&mut editor, KeyCode::Char('f')) else {
        panic!("nothing to find");
    };
    assert_eq!(profile, "ring");
    editor.found(Found::Extras {
        profile,
        skills: vec![discover::skills::Skill {
            dir: PathBuf::from("/src/.claude/skills/review"),
            name: "review".into(),
            description: Some("Reviews.".into()),
        }],
        instructions: vec![PathBuf::from("/src/CLAUDE.md")],
    });
    let _ = press(&mut editor, KeyCode::Char(' '));
    let _ = press(&mut editor, KeyCode::Enter);
    assert!(
        editor
            .doc
            .text()
            .contains("skills = [\"/src/.claude/skills/review\"]"),
        "{}",
        editor.doc.text()
    );
}

#[test]
fn finds_land_once_and_only_on_the_popup_that_asked() {
    let mut editor = editor(CONFIG);
    answer(
        &mut editor,
        Query::Orgs,
        Answer::Orgs(vec!["ORG".into(), "Other".into()]),
    );
    // A profile's repos: orgs you're in are matched without case.
    select(&mut editor, 5, 7);
    let _ = press(&mut editor, KeyCode::Char('f'));
    let Some(Popup::Suggest(suggest)) = &editor.popup else {
        panic!("no suggestions");
    };
    let labels: Vec<&str> = suggest.rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["other"], "org is watched already");
    let checkout = || {
        Found::Checkouts(vec![discover::scan::Found {
            path: PathBuf::from("/src/tool"),
            repo: sanic_core::repo::RepoName::new("else", "tool"),
        }])
    };
    // Nothing was asked for yet.
    editor.found(checkout());
    let Some(Popup::Suggest(suggest)) = &editor.popup else {
        panic!("no suggestions");
    };
    assert_eq!(suggest.rows.len(), 1);
    // Scanning twice offers each checkout once.
    for _ in 0..2 {
        let _ = press(&mut editor, KeyCode::Char('s'));
        editor.found(checkout());
    }
    let Some(Popup::Suggest(suggest)) = &editor.popup else {
        panic!("no suggestions");
    };
    let labels: Vec<&str> = suggest.rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["other", "else", "/src/tool"]);

    // Skills a closed popup asked for don't land on another profile's.
    let _ = press(&mut editor, KeyCode::Esc);
    select(&mut editor, 5, 2);
    let Outcome::Find(Find::Extras { profile, .. }) = press(&mut editor, KeyCode::Char('f')) else {
        panic!("nothing to find");
    };
    editor.found(Found::Extras {
        profile: format!("not-{profile}"),
        skills: vec![discover::skills::Skill {
            dir: PathBuf::from("/elsewhere/review"),
            name: "review".into(),
            description: None,
        }],
        instructions: Vec::new(),
    });
    let Some(Popup::Suggest(suggest)) = &editor.popup else {
        panic!("no suggestions");
    };
    assert!(suggest.rows.is_empty() && suggest.finding);
}

#[test]
fn a_config_reads_as_a_commented_file_with_what_it_does_below() {
    let text = r#"# sanic-review config; the format is described in docs/DESIGN.md.

[review_requests]
teams = ["*", "!sanic-hq/sanic-speedsters"]
skip_titles = ["build(deps)*"] # dependabot

[runner]
model = "claude-sonnet-5"
manual_reviews = true

# The documentation team's.
[profile.ring]
instructions = ["~/.config/sanic-review/instructions/ring.md"]
repos = [
  { github = "sanic-hq/sanic-cli" },
  { github = "sanic-hq/services", paths = ["/documentation/**"] },
]

[profile.default]
repos = [{ github = "sanic-hq" }, { github = "quodlibetor" }]
"#;
    let mut editor = editor(text);
    // On the first team pattern.
    select(&mut editor, 2, 0);
    let wanted = editor.want(SystemTime::UNIX_EPOCH);
    let answers = wanted
        .iter()
        .map(|q| {
            let answer = match q {
                Query::Teams => Answer::Teams(vec![
                    sanic_core::pr::TeamRef::new("sanic-hq", "zone"),
                    sanic_core::pr::TeamRef::new("sanic-hq", "sanic-speedsters"),
                ]),
                Query::Orgs => Answer::Orgs(vec!["sanic-hq".into()]),
                Query::RepoNames(_) => Answer::RepoNames {
                    repos: std::collections::BTreeSet::from([sanic_core::repo::RepoName::new(
                        "sanic-hq", "services",
                    )]),
                    complete: true,
                },
                Query::Repos(q) if q.contains("sanic-hq") => Answer::Count(1040),
                Query::Repos(_) => Answer::Count(19),
                Query::Prs(q) if q.contains("team-review-requested") => Answer::Count(9),
                Query::Prs(q) if q.starts_with("author:@me") => Answer::Count(2),
                Query::Prs(_) => Answer::Count(15),
            };
            Counted::Answer(q.clone(), answer)
        })
        .collect();
    editor.counted(answers);
    // Your teams, now in, are counted on the next plan.
    for q in editor.want(SystemTime::UNIX_EPOCH) {
        if let Query::Prs(team) = &q
            && team.contains("team-review-requested")
        {
            // A count of its own for each team.
            let n = if team.contains("sanic-speedsters") {
                23
            } else {
                9
            };
            editor.counted(vec![Counted::Answer(q.clone(), Answer::Count(n))]);
        }
    }
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
    terminal.draw(|frame| editor.render(frame)).unwrap();
    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn every_comment_the_file_has_shows_where_it_sits() {
    let text = "# About.\n\n[runner] # the runner\nmodel = \"m\" # for now\n# closing runner\n\n# above poll\n[poll]\nquiet_secs = 1\n\n[profile.p]\nrepos = [ # the repos\n  # about org\n  { github = \"org\" }, # on org\n  { github = \"else\" },\n  # more later\n]\nskip_titles = [\n  # about wip\n  \"wip*\",\n]\n# the end\n";
    let editor = editor(text);
    let mut terminal = Terminal::new(TestBackend::new(80, 80)).unwrap();
    terminal.draw(|frame| editor.render(frame)).unwrap();
    let screen = format!("{:?}", terminal.backend());
    for comment in [
        "# About.",
        "[runner]  # the runner",
        "model = \"m\"  # for now",
        "# closing runner",
        "# above poll",
        "repos = [  # the repos",
        "  # about org",
        "{ github = \"org\" },  # on org",
        "  # more later",
        "  # about wip",
        "# the end",
    ] {
        assert!(screen.contains(comment), "{comment:?} missing:\n{screen}");
    }
    // A comment inside a short list puts it an item a line.
    assert!(!screen.contains("skip_titles = [\"wip*\"]"), "{screen}");
}

#[test]
fn comments_in_empty_lists_and_profile_tables_show() {
    let text = "[review_requests]\nteams = [ # none\n  # yet\n]\n\n# about profiles\n[profile]\n# above q\nq = { repos = [] } # after q\n";
    let editor = editor(text);
    let mut terminal = Terminal::new(TestBackend::new(80, 80)).unwrap();
    terminal.draw(|frame| editor.render(frame)).unwrap();
    let screen = format!("{:?}", terminal.backend());
    for comment in [
        "teams = [  # none",
        "  # yet",
        "# about profiles",
        "# above q",
        "[profile.q]  # after q",
    ] {
        assert!(screen.contains(comment), "{comment:?} missing:\n{screen}");
    }
}

#[test]
fn comments_closing_a_table_stay_with_it_when_the_file_orders_tables_otherwise() {
    // The file has runner before poll, which the editor lists first.
    let text =
        "[runner]\nmodel = \"m\"\n# closing runner\n\n[poll]\nquiet_secs = 1\n\n# closing poll\n";
    let editor = editor(text);
    let lines: Vec<String> = editor
        .file_lines(80)
        .into_iter()
        .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let at = |text: &str| {
        lines
            .iter()
            .position(|l| l == text)
            .unwrap_or_else(|| panic!("{text:?} missing: {lines:#?}"))
    };
    // Each after its own table's last key, a blank line before it, and
    // not above the table that follows it here.
    assert!(at("# closing runner") > at("model = \"m\""));
    assert_eq!(lines[at("# closing runner") - 1], "");
    assert!(at("# closing poll") > at("quiet_secs = 1"));
    assert!(at("# closing poll") < at("[review_requests]"));
    assert!(at("# closing runner") > at("[runner]"));
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("# closing")).count(),
        2
    );
}

/// The comments the view shows, from the file itself: each of the file's
/// own is drawn in its comment style.
fn shown_comments(editor: &ConfigEditor) -> Vec<String> {
    let mut shown: Vec<String> = editor
        .file_lines(200)
        .into_iter()
        .flat_map(|l| l.line.spans)
        .filter(|span| span.style == render::COMMENT)
        .map(|span| {
            let text = span.content.trim_start();
            let text = text.strip_prefix('#').unwrap_or(text);
            text.strip_prefix(' ').unwrap_or(text).trim_end().to_owned()
        })
        .collect();
    shown.sort();
    shown
}

/// The comments in `text`, from every decor `toml_edit` parses it into:
/// not from [`crate::config_doc::split_comments`], which the view uses,
/// so a comment it misses can't go missing from both.
fn file_comments(text: &str) -> Vec<String> {
    fn raw(found: &mut Vec<String>, text: Option<&toml_edit::RawString>) {
        let text = text
            .and_then(toml_edit::RawString::as_str)
            .unwrap_or_default();
        found.extend(
            text.lines()
                .filter_map(|line| line.trim().strip_prefix('#'))
                .map(|c| c.strip_prefix(' ').unwrap_or(c).trim_end().to_owned()),
        );
    }
    fn decor(found: &mut Vec<String>, decor: &toml_edit::Decor) {
        raw(found, decor.prefix());
        raw(found, decor.suffix());
    }
    fn key(found: &mut Vec<String>, key: &toml_edit::Key) {
        decor(found, key.leaf_decor());
        decor(found, key.dotted_decor());
    }
    fn in_value(found: &mut Vec<String>, value: &toml_edit::Value) {
        decor(found, value.decor());
        match value {
            toml_edit::Value::Array(list) => {
                list.iter().for_each(|v| in_value(found, v));
                raw(found, Some(list.trailing()));
            }
            toml_edit::Value::InlineTable(table) => {
                for (name, v) in table {
                    table.key(name).into_iter().for_each(|k| key(found, k));
                    in_value(found, v);
                }
                raw(found, Some(table.trailing()));
            }
            _ => {}
        }
    }
    fn in_table(found: &mut Vec<String>, table: &toml_edit::Table) {
        decor(found, table.decor());
        for (name, item) in table {
            table.key(name).into_iter().for_each(|k| key(found, k));
            match item {
                toml_edit::Item::Value(v) => in_value(found, v),
                toml_edit::Item::Table(t) => in_table(found, t),
                toml_edit::Item::ArrayOfTables(ts) => ts.iter().for_each(|t| in_table(found, t)),
                toml_edit::Item::None => {}
            }
        }
    }
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    let mut found = Vec::new();
    in_table(&mut found, doc.as_table());
    raw(&mut found, Some(doc.trailing()));
    found.sort();
    found
}

/// Configs in every shape the comment reviews turned up.
const CORPUS: &[&str] = &[
    // Header profiles, comments everywhere, tables in another order.
    r#"# opens the file

# the runner's own
[runner] # on runner
# above model
model = "m" # on model
# closes runner

# the poll's own
[poll]
quiet_secs = 1

[profile.a] # on a
repos = [ # opening
  # above x
  { github = "o/x" }, # on x
  { github = "o/y" } # on y, no comma
  # closing
]
# closes a

# the end
"#,
    // Dotted and inline profiles under an explicit [profile].
    r#"[runner]
model = "m"

# closes runner
# own of profile
[profile] # on profile
# above a
a.repos = [{ github = "o" }] # after a
# above b
b = { repos = [{ github = "p" }] } # after b
# closes the profiles

# end
"#,
    // Arrays of tables, with and without their profile's header.
    r#"[profile.a]
# above entry
[[profile.a.repos]] # on entry
# above github
github = "o" # on github

# between entries
[[profile.a.repos]]
github = "p"

# closes a
[[profile.b.repos]]
github = "q"
# end of b
"#,
    // Dotted keys before any header, empty lists, no trailing commas.
    r#"# about the dotted ones
profile.a.repos = [ # empty
  # nothing yet
]
review_requests.teams = [] # none
# closes the dotted ones

[runner] # last
read_paths = ["x" # on x
  ,"y" # on y
] # after
"#,
    // A table the config doesn't take, and a key it doesn't either.
    r#"[runner]
model = "m"
# above bogus
bogus = 1 # on bogus

# above foo
[foo] # on foo
# above bar
bar = [1, # on 1
  2]
# closes foo

[profile.a]
repos = [{ github = "o" }]
"#,
    // Only comments.
    "# just a comment\n\n# and another\n",
    // A `#` in a string isn't a comment; no newline at the end.
    "[runner]\nclaude = \"c # not a comment\" # real\nmodel = '''m\n# still the string\n''' # after\n# the end",
    // An unknown table with one under it, a key before any header, and a
    // profile key the config doesn't take.
    r#"# about x
x = 1 # on x

[foo.sub] # on sub
# above k
k = 2

# closes sub
[profile.a]
repos = [{ github = "o" }]
# above odd
odd = true # on odd
"#,
    // Comments set apart between two parts of a table the config doesn't
    // take, or of one inside a table or profile: they're its own.
    r#"[runner]
model = "m"
# apart in runner

[runner.extra]
k = 1

[foo]
a = 1
# apart in foo

[foo.sub]
b = 2

[[bar]]
c = 1
# between bars

[[bar]]
c = 2

[profile.a]
repos = [{ github = "o" }]
# apart in a

[profile.a.extra]
x = 1
"#,
    // Multi-line strings that end with quotes of their own.
    r#"[foo]
a = """x"""" # after four
b = '''y''''' # after five
c = """""" # after an empty one
"#,
];

#[test]
fn every_comment_in_the_file_shows_once() {
    for text in CORPUS {
        let editor = editor(text);
        assert_eq!(
            shown_comments(&editor),
            file_comments(text),
            "in\n{text}\nthe view has\n{:#?}",
            editor
                .file_lines(200)
                .into_iter()
                .map(|l| l
                    .line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn every_comment_still_shows_once_after_edits() {
    for text in CORPUS {
        let mut editor = editor(text);
        let edits = [
            Op::AddProfile { name: "new".into() },
            Op::Set {
                key: key_of(Table::Github, "api_url"),
                value: Scalar::Text("http://x".into()),
            },
            Op::MoveProfile {
                name: "new".into(),
                to: 0,
            },
        ];
        for op in edits {
            // Some shapes refuse some edits; the rest must still hold.
            let _ = editor.doc.apply(op);
            let written = editor.doc.text();
            assert_eq!(
                shown_comments(&editor),
                file_comments(&written),
                "after edits, in\n{written}"
            );
        }
    }
}
