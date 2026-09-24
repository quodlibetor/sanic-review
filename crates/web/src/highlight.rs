//! Syntax highlighting for the files view, as classes that `syntax.css`
//! colours for the dashboard's light and dark modes.

use std::sync::LazyLock;

use maud::{Markup, PreEscaped, html};
use syntect::{
    highlighting::ThemeSet,
    html::{ClassStyle, css_for_theme_with_class_style, line_tokens_to_classed_spans},
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
};

/// Every class starts with this, so none collides with the dashboard's.
const STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "sy-" };

/// Lines longer than this are shown plain: they're rarely code worth
/// colouring, and the regexes are slowest on them.
pub const MAX_LINE: usize = 1000;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_nonewlines);

/// The colours for the classes: GitHub's light theme, and in dark mode a
/// dark one, as the dashboard's own tokens switch.
static CSS: LazyLock<String> = LazyLock::new(|| {
    let themes = ThemeSet::load_defaults();
    let css = |name: &str| {
        themes
            .themes
            .get(name)
            .and_then(|theme| css_for_theme_with_class_style(theme, STYLE).ok())
            .unwrap_or_default()
    };
    format!(
        "{}\n@media (prefers-color-scheme: dark) {{\n{}\n}}\n",
        css("InspiredGitHub"),
        css("base16-ocean.dark")
    )
});

/// `syntax.css`.
pub fn css() -> &'static str {
    &CSS
}

/// Highlights one file's lines in order, carrying what's open (a string,
/// a comment) from each line to the next.
pub struct Highlighter {
    syntax: &'static SyntaxReference,
    state: ParseState,
    stack: ScopeStack,
}

impl Highlighter {
    /// For `path`, by its extension or else its name (`Makefile`); `None`
    /// for a file syntect doesn't know.
    pub fn for_path(path: &str) -> Option<Self> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let syntax = name
            .rsplit_once('.')
            .and_then(|(_, ext)| SYNTAXES.find_syntax_by_extension(ext))
            .or_else(|| SYNTAXES.find_syntax_by_extension(name))?;
        Some(Self {
            syntax,
            state: ParseState::new(syntax),
            stack: ScopeStack::new(),
        })
    }

    /// Starts over, for lines that don't follow the last ones.
    pub fn restart(&mut self) {
        self.state = ParseState::new(self.syntax);
        self.stack = ScopeStack::new();
    }

    /// `text`, highlighted; plain if it's too long or doesn't parse. Each
    /// line's spans are closed at its end and the ones still open reopened
    /// on the next, so a line stands alone in its cell.
    pub fn line(&mut self, text: &str) -> Markup {
        if text.len() > MAX_LINE {
            // Unparsed, it leaves the state wherever the line before did,
            // which the lines after it may not be in: start them afresh.
            self.restart();
            return html! { (text) };
        }
        let Ok(ops) = self.state.parse_line(text, &SYNTAXES) else {
            self.restart();
            return html! { (text) };
        };
        let mut out = String::new();
        let reopened = self.stack.as_slice().len();
        for scope in self.stack.as_slice() {
            out.push_str("<span class=\"");
            for (i, atom) in scope.build_string().split('.').enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str("sy-");
                out.push_str(&html_escape(atom));
            }
            out.push_str("\">");
        }
        let Ok((spans, delta)) = line_tokens_to_classed_spans(text, &ops, STYLE, &mut self.stack)
        else {
            self.restart();
            return html! { (text) };
        };
        out.push_str(&spans);
        let open = reopened.saturating_add_signed(delta);
        out.push_str(&"</span>".repeat(open));
        PreEscaped(out)
    }
}

/// A scope atom, safe in an attribute; they're only ever letters, digits
/// and a little punctuation.
fn html_escape(atom: &str) -> String {
    maud::html! { (atom) }.into_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_stand_alone_with_whats_open_carried_over() {
        let mut h = Highlighter::for_path("src/lib.rs").unwrap();
        let first = h.line("let s = \"open").into_string();
        let second = h.line("still in the string\";").into_string();
        assert!(first.contains("sy-keyword"), "{first}");
        for line in [&first, &second] {
            assert_eq!(
                line.matches("<span").count(),
                line.matches("</span>").count(),
                "{line}"
            );
        }
        assert!(
            second.starts_with("<span class=\"sy-source sy-rust\">"),
            "{second}"
        );
        assert!(second.contains("sy-string"), "{second}");
    }

    #[test]
    fn unknown_files_and_long_lines_are_plain() {
        assert!(Highlighter::for_path("notes.unknownext").is_none());
        let mut h = Highlighter::for_path("Makefile").unwrap();
        let long = "x".repeat(MAX_LINE + 1);
        assert_eq!(h.line(&long).into_string(), long);
        // What a long line might have closed doesn't stay open after it.
        let mut h = Highlighter::for_path("a.rs").unwrap();
        h.line("let s = \"open");
        h.line(&format!("{long}\";"));
        assert!(!h.line("let t = 1;").into_string().contains("sy-string"));
        let mut h = Highlighter::for_path("a.rs").unwrap();
        assert!(!h.line("<b>").into_string().contains("<b>"));
    }

    #[test]
    fn css_colours_both_modes() {
        let css = css();
        assert!(css.contains(".sy-keyword"), "{css}");
        assert!(css.contains("@media (prefers-color-scheme: dark)"));
    }
}
