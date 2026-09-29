//! The config editor for the README's screenshot (`mise run screenshots`):
//! drawn on an invented config with its counts answered, as an HTML page
//! of the terminal's cells, written to `$CONFIG_EDITOR_HTML` when it's
//! set. `scripts/screenshots.mjs` sets it and captures the page.

use std::fmt::Write as _;

use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};
use sanic_core::{pr::TeamRef, repo::RepoName};

use super::*;

/// You are `quodlibetor`, as in the dashboard's demo; the rest is made up.
const DEMO: &str = r#"# sanic-review config; the format is described in docs/DESIGN.md.

[review_requests]
teams = ["*", "!frobco/docs"]
skip_titles = ["build(deps)*"] # dependabot

[runner]
model = "claude-sonnet-5"
manual_reviews = true

# The frobnicator's reviews follow its style guide.
[profile.frobnicator]
instructions = ["~/.config/sanic-review/frobnicator.md"]
repos = [
  { github = "quodlibetor/frobnicator" },
  { github = "frobco/monorepo", paths = ["/frobnicator/**"] },
]

[profile.default]
repos = [{ github = "quodlibetor" }, { github = "frobco" }]
"#;

/// The editor on [`DEMO`] in `terminal`, counted, on the monorepo entry,
/// whose share of the counts shows beside it.
fn demo(terminal: &mut Terminal<TestBackend>) -> ConfigEditor {
    let doc = ConfigDoc::parse(Some(DEMO)).unwrap();
    let mut editor = ConfigEditor::new(
        Path::new("/home/quodlibetor/.config/sanic-review/config.toml"),
        None,
        doc,
    );
    settle(&mut editor);
    // The view scrolls only as far as keeps the selection in it, which
    // leaves the entry on its last line: one more shows the list's close.
    // Counted first, since the counts' lines take from the view's.
    editor.select(&Row::Entry("frobnicator".into(), 1));
    count(&mut editor);
    terminal.draw(|frame| editor.render(frame)).unwrap();
    editor.top.set(editor.top.get() + 1);
    editor
}

/// Answers everything `editor` wants counted, twice, since your teams,
/// once in, are counted on the next plan.
fn count(editor: &mut ConfigEditor) {
    for _ in 0..2 {
        let answers = editor
            .want(SystemTime::UNIX_EPOCH)
            .into_iter()
            .map(|q| {
                let answer = answer(&q);
                Counted::Answer(q, answer)
            })
            .collect();
        editor.counted(answers);
    }
}

fn answer(q: &Query) -> Answer {
    match q {
        Query::Teams => Answer::Teams(vec![
            TeamRef::new("frobco", "frobnicator"),
            TeamRef::new("frobco", "docs"),
        ]),
        Query::Orgs => Answer::Orgs(vec!["frobco".into()]),
        Query::RepoNames(_) => Answer::RepoNames {
            repos: std::collections::BTreeSet::from([
                RepoName::new("frobco", "monorepo"),
                RepoName::new("frobco", "docs-site"),
                RepoName::new("quodlibetor", "frobnicator"),
                RepoName::new("quodlibetor", "sanic-review"),
            ]),
            complete: true,
        },
        Query::Repos(q) if q.contains("frobco") => Answer::Count(214),
        Query::Repos(_) => Answer::Count(23),
        Query::Prs(q) if q.contains("team-review-requested:frobco/docs") => Answer::Count(17),
        Query::Prs(q) if q.contains("team-review-requested:") => Answer::Count(6),
        // Owed and yours, in each of the profile's repos, else in all.
        Query::Prs(q) => {
            let [owed, yours] = if q.contains("repo:quodlibetor/") {
                [3, 2]
            } else if q.contains("repo:frobco/") {
                [6, 1]
            } else {
                [14, 5]
            };
            Answer::Count(if q.starts_with(counts::OWED) {
                owed
            } else {
                yours
            })
        }
    }
}

#[test]
fn the_readme_screenshot_draws_the_demo_counted() {
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
    let editor = demo(&mut terminal);
    terminal.draw(|frame| editor.render(frame)).unwrap();
    let screen = terminal.backend().to_string();
    for shown in [
        "[profile.frobnicator]",
        "1 repo · up to 7 PRs",
        "What this config does",
        "✓ loads",
    ] {
        assert!(screen.contains(shown), "{screen}");
    }
    let html = html(terminal.backend().buffer());
    if let Some(path) = std::env::var_os("CONFIG_EDITOR_HTML") {
        std::fs::write(path, html).unwrap();
    }
}

/// A page of `buffer`'s cells, each a box of one size, in a light or
/// dark palette as the browser prefers. Box-drawing characters are drawn
/// as lines rather than glyphs, so they join whatever the font.
fn html(buffer: &Buffer) -> String {
    let mut styles: Vec<(Color, Color, Modifier)> = Vec::new();
    let mut body = String::new();
    for row in buffer.content().chunks(usize::from(buffer.area.width)) {
        body.push_str("<div>");
        for cell in row {
            let style = (cell.fg, cell.bg, cell.modifier);
            let n = styles.iter().position(|s| *s == style).unwrap_or_else(|| {
                styles.push(style);
                styles.len() - 1
            });
            let symbol = cell.symbol();
            let mut chars = symbol.chars();
            if let (Some(arms), None) = (chars.next().and_then(arms), chars.next()) {
                let _ = write!(body, r#"<i class="s{n} b {arms}"></i>"#);
            } else {
                let _ = write!(body, r#"<i class="s{n}">"#);
                for c in symbol.chars() {
                    match c {
                        '<' => body.push_str("&lt;"),
                        '>' => body.push_str("&gt;"),
                        '&' => body.push_str("&amp;"),
                        c => body.push(c),
                    }
                }
                body.push_str("</i>");
            }
        }
        body.push_str("</div>");
    }
    let mut classes = String::new();
    for (n, style) in styles.into_iter().enumerate() {
        let _ = writeln!(classes, ".s{n} {{ {} }}", css(style));
    }
    format!("{HEAD}{classes}</style>\n<pre>{body}</pre>\n")
}

/// The arms of a light box-drawing character, as classes: up, down, left
/// and right.
fn arms(c: char) -> Option<&'static str> {
    Some(match c {
        '─' => "l r",
        '│' => "u d",
        '┌' | '╭' => "d r",
        '┐' | '╮' => "d l",
        '└' | '╰' => "u r",
        '┘' | '╯' => "u l",
        '├' => "u d r",
        '┤' => "u d l",
        '┬' => "d l r",
        '┴' => "u l r",
        '┼' => "u d l r",
        _ => return None,
    })
}

/// The palette, as the named colours' custom properties, and the cells'
/// grid; [`html`] closes the style with a class for each style it draws.
/// A cell is a whole number of pixels, so its lines land on pixels.
const HEAD: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>Config editor</title>
<style>
:root {
  --fg: #24292f; --bg: #ffffff;
  --black: #24292f; --red: #cf222e; --green: #116329; --yellow: #7d4e00;
  --blue: #0969da; --magenta: #8250df; --cyan: #1b7c83; --gray: #57606a;
  --dark-gray: #8c959f; --light-red: #a40e26; --light-green: #1a7f37;
  --light-yellow: #633c01; --light-blue: #218bff; --light-magenta: #a475f9;
  --light-cyan: #3192aa; --white: #6e7781;
}
@media (prefers-color-scheme: dark) {
  :root {
    --fg: #e6edf3; --bg: #0d1117;
    --black: #484f58; --red: #ff7b72; --green: #3fb950; --yellow: #d29922;
    --blue: #58a6ff; --magenta: #bc8cff; --cyan: #39c5cf; --gray: #b1bac4;
    --dark-gray: #6e7681; --light-red: #ffa198; --light-green: #56d364;
    --light-yellow: #e3b341; --light-blue: #79c0ff; --light-magenta: #d2a8ff;
    --light-cyan: #56d4dd; --white: #f0f6fc;
  }
}
body { margin: 0; background: var(--bg); }
pre {
  display: inline-block; margin: 0; padding: 12px 16px;
  font: 15px "DejaVu Sans Mono", Menlo, Consolas, monospace;
  color: var(--fg); background: var(--bg);
}
pre > div { display: flex; height: 18px; }
i {
  flex: none; width: 9px; height: 18px; line-height: 18px;
  font-style: normal; text-align: center; white-space: pre;
}
.b {
  --u: none; --d: none; --l: none; --r: none;
  background-image: var(--u), var(--d), var(--l), var(--r);
  background-position: center top, center bottom, left center, right center;
  background-size: 1px 9px, 1px 9px, 5px 1px, 5px 1px;
  background-repeat: no-repeat;
}
.u { --u: linear-gradient(currentColor, currentColor); }
.d { --d: linear-gradient(currentColor, currentColor); }
.l { --l: linear-gradient(currentColor, currentColor); }
.r { --r: linear-gradient(currentColor, currentColor); }
"#;

/// `style`'s colours and modifiers as CSS declarations.
fn css((fg, bg, modifier): (Color, Color, Modifier)) -> String {
    let (fg, bg) = if modifier.contains(Modifier::REVERSED) {
        (colour(bg, "bg"), colour(fg, "fg"))
    } else {
        (colour(fg, "fg"), colour(bg, "bg"))
    };
    // Dim is the text's alone, faded towards what's behind it.
    let mut css = if modifier.contains(Modifier::DIM) {
        format!("color:color-mix(in srgb,{fg} 60%,{bg})")
    } else {
        format!("color:{fg}")
    };
    if bg != "var(--bg)" {
        let _ = write!(css, ";background-color:{bg}");
    }
    if modifier.contains(Modifier::BOLD) {
        css.push_str(";font-weight:bold");
    }
    if modifier.contains(Modifier::ITALIC) {
        css.push_str(";font-style:italic");
    }
    if modifier.contains(Modifier::UNDERLINED) {
        css.push_str(";text-decoration:underline");
    }
    css
}

/// `color` as a palette property, or `reset` when it's the default.
fn colour(color: Color, reset: &str) -> String {
    let name = match color {
        Color::Reset => reset,
        Color::Black => "black",
        Color::Red => "red",
        Color::Green => "green",
        Color::Yellow => "yellow",
        Color::Blue => "blue",
        Color::Magenta => "magenta",
        Color::Cyan => "cyan",
        Color::Gray => "gray",
        Color::DarkGray => "dark-gray",
        Color::LightRed => "light-red",
        Color::LightGreen => "light-green",
        Color::LightYellow => "light-yellow",
        Color::LightBlue => "light-blue",
        Color::LightMagenta => "light-magenta",
        Color::LightCyan => "light-cyan",
        Color::White => "white",
        Color::Rgb(r, g, b) => return format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => panic!("no palette entry for colour {n}"),
    };
    format!("var(--{name})")
}
