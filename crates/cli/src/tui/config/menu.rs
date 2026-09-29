//! The dropdown under a field as it's typed: what the field could hold,
//! from what the editor knows already, so typing asks GitHub nothing.

/// How many options show at once; the rest scroll into view.
pub const SHOWN: usize = 6;

/// The options for what's typed, and which one's chosen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Menu {
    pub options: Vec<String>,
    /// Nothing until ↑ or ↓ chooses, so Enter sets what's typed.
    pub chosen: Option<usize>,
    /// Esc closed it; typing opens it again.
    pub closed: bool,
}

impl Menu {
    #[must_use]
    pub fn new(options: Vec<String>) -> Self {
        Self {
            options,
            chosen: None,
            closed: false,
        }
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.closed && !self.options.is_empty()
    }

    /// Chooses the option `by` away, round from either end.
    pub fn step(&mut self, by: isize) {
        let n = self.options.len();
        if n == 0 {
            return;
        }
        self.chosen = Some(match self.chosen {
            None if by < 0 => n - 1,
            None => 0,
            Some(at) => (at.cast_signed() + by)
                .rem_euclid(n.cast_signed())
                .cast_unsigned(),
        });
    }

    /// What Tab takes: the chosen option, else the first.
    #[must_use]
    pub fn pick(&self) -> Option<&str> {
        self.options
            .get(self.chosen.unwrap_or(0))
            .map(String::as_str)
    }

    /// The options in view, keeping the chosen one there, and where they
    /// start.
    #[must_use]
    pub fn window(&self) -> (usize, &[String]) {
        let first = self
            .chosen
            .map_or(0, |at| (at + 1).saturating_sub(SHOWN))
            .min(self.options.len().saturating_sub(SHOWN));
        let last = (first + SHOWN).min(self.options.len());
        (first, &self.options[first..last])
    }
}

/// The `options` that go with `typed`, ignoring case: those starting with
/// it, then those with it anywhere, each once and without `typed` itself.
#[must_use]
pub fn matching(options: impl IntoIterator<Item = String>, typed: &str) -> Vec<String> {
    let typed = typed.trim();
    let lower = typed.to_lowercase();
    let mut starting = Vec::new();
    let mut within = Vec::new();
    for option in options {
        let folded = option.to_lowercase();
        if option == typed || starting.contains(&option) || within.contains(&option) {
            continue;
        }
        if folded.starts_with(&lower) {
            starting.push(option);
        } else if folded.contains(&lower) {
            within.push(option);
        }
    }
    starting.extend(within);
    starting
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(n: usize) -> Menu {
        Menu::new((0..n).map(|i| format!("o{i}")).collect())
    }

    #[test]
    fn options_that_start_with_it_come_first() {
        let options = ["claude-opus-5-5", "opus", "Sonnet", "opus", "haiku"].map(String::from);
        assert_eq!(matching(options.clone(), "op"), ["opus", "claude-opus-5-5"]);
        assert_eq!(matching(options.clone(), "SON"), ["Sonnet"]);
        assert_eq!(
            matching(options.clone(), "opus"),
            ["claude-opus-5-5"],
            "not itself"
        );
        assert_eq!(matching(options, "").len(), 4, "all, once each");
    }

    #[test]
    fn arrows_choose_round_the_ends_and_the_view_follows() {
        let mut m = menu(SHOWN + 3);
        assert_eq!(m.pick(), Some("o0"), "Tab takes the first unchosen");
        m.step(-1);
        assert_eq!(m.chosen, Some(SHOWN + 2));
        assert_eq!(m.window().0, 3);
        m.step(1);
        assert_eq!(m.chosen, Some(0));
        assert_eq!(m.window(), (0, &m.options[..SHOWN]));
        let mut none = Menu::default();
        none.step(1);
        assert_eq!((none.chosen, none.is_open()), (None, false));
        m.closed = true;
        assert!(!m.is_open());
    }
}
