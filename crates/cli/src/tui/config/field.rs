//! The text a field's being typed into, with emacs or vi keys.

use std::path::Path;

use edtui::{
    EditorEventHandler, EditorMode, EditorState, Index2, Lines,
    actions::{
        DeleteToFirstCharOfLine, DeleteWordBackward, Execute, InsertChar, RemoveChar, SwitchMode,
        Undo,
    },
    clipboard::ClipboardTrait,
    events::{KeyEventHandler, KeyEventRegister, KeyInput},
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    style::{Style, Stylize},
    text::{Line, Span},
};
use sanic_core::config::Keys;

use super::vi::{self, Edit, Op, Parsed};

/// Which keys to take when the config doesn't say: vi if readline's
/// `editing-mode` is, or else if the editor you'd be dropped into is a vi;
/// emacs otherwise. `inputrc` is the text of readline's init file.
#[must_use]
pub fn guess_keys(inputrc: Option<&str>, visual: Option<&str>, editor: Option<&str>) -> Keys {
    let readline = inputrc.and_then(|text| {
        text.lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                (words.next()? == "set" && words.next()? == "editing-mode")
                    .then(|| words.next())
                    .flatten()
            })
            .next_back()
    });
    match readline {
        Some("vi") => return Keys::Vi,
        Some("emacs") => return Keys::Emacs,
        _ => {}
    }
    // `$VISUAL` wins over `$EDITOR` for a full-screen editor, as it does
    // for everything else that reads them.
    let command = [visual, editor]
        .into_iter()
        .flatten()
        .find(|command| !command.trim().is_empty());
    let program = command
        .and_then(|command| command.split_whitespace().next())
        .and_then(|program| Path::new(program).file_name())
        .and_then(|name| name.to_str());
    if program.is_some_and(|name| name.contains("vi")) {
        Keys::Vi
    } else {
        Keys::Emacs
    }
}

/// [`guess_keys`] from readline's init file, where readline looks for it,
/// and the editor variables.
#[must_use]
pub fn guessed_keys() -> Keys {
    let inputrc = std::env::var_os("INPUTRC")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".inputrc")))
        .and_then(|path| std::fs::read_to_string(path).ok());
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    guess_keys(inputrc.as_deref(), visual.as_deref(), editor.as_deref())
}

/// What a key did to the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Took {
    /// The field's edited or moved, or the key did nothing.
    Edit,
    /// Enter: set what's typed.
    Commit,
    /// Esc, from where Esc goes no further: stop typing, setting nothing.
    Cancel,
}

/// A line of text being typed, and the keys it takes.
#[derive(Clone)]
pub struct TextField {
    state: EditorState,
    handler: EditorEventHandler,
    keys: Keys,
    /// What edtui yanks and pastes, which the field's own commands cut
    /// into too.
    clip: Clip,
    /// The keys of a vi command begun in normal mode and waiting for
    /// more, which edtui keeps to itself.
    pending: Vec<char>,
    /// Whose change vi's `.` repeats.
    last: Last,
}

/// The last change, which `.` repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Last {
    /// edtui's, which it keeps to itself.
    Edtui,
    /// The field's, and the command to run again; none for a `c`, as
    /// what's typed after it is edtui's to keep.
    Field(Option<vi::Cmd>),
}

impl TextField {
    /// `text` to type over, with the cursor after it, inserting.
    #[must_use]
    pub fn new(text: &str, keys: Keys) -> Self {
        let mut state = EditorState::new(Lines::from(text));
        state.set_single_line(true);
        state.mode = EditorMode::Insert;
        state.cursor = Index2::new(0, text.chars().count());
        let clip = Clip::default();
        state.set_clipboard(clip.clone());
        Self {
            state,
            handler: EditorEventHandler::new(key_handler(keys)),
            keys,
            clip,
            pending: Vec::new(),
            last: Last::Edtui,
        }
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.state.lines.flatten(&None).into_iter().collect()
    }

    /// Replaces the text as an edit `u` undoes, keeping what's yanked,
    /// and leaves the cursor after it; or, outside insert mode, in normal
    /// mode on its last character, as vi's cursor can't be past the end
    /// there.
    ///
    /// Only what differs after the part the two share is replaced: a
    /// completion typed mid-insert goes on from what's typed, so vi's `.`
    /// repeats the insert with the rest of it, as edtui records only
    /// what's inserted. One that takes back some of the text leaves `.`
    /// nothing to repeat, as edtui would repeat the insert without that,
    /// and so does one outside insert mode, a change `.` can't make again.
    pub fn set_text(&mut self, text: &str) {
        let inserting = self.inserting();
        self.pending.clear();
        self.state.selection = None;
        let old: Vec<char> = self.text().chars().collect();
        let shared = old
            .iter()
            .zip(text.chars())
            .take_while(|(a, b)| **a == *b)
            .count();
        self.state.cursor = Index2::new(0, shared);
        self.state.execute(RemoveChar(old.len() - shared));
        self.state.cursor = Index2::new(0, shared);
        for c in text.chars().skip(shared) {
            self.state.execute(InsertChar(c));
        }
        // Outside insert mode a take is a change of its own, which `.`
        // can't repeat, and edtui's before it isn't the last any more.
        if !inserting || shared < old.len() {
            self.last = Last::Field(None);
        }
        if !inserting {
            SwitchMode(EditorMode::Normal).execute(&mut self.state);
        }
    }

    /// The character `key` would type: edtui inserts a plain or shifted
    /// character, and one with Alt that isn't a letter (`AltGr`, on some
    /// keyboards). Nothing outside insert mode.
    #[must_use]
    pub fn inserts(&self, key: KeyEvent) -> Option<char> {
        let KeyCode::Char(c) = key.code else {
            return None;
        };
        let held = key.modifiers - KeyModifiers::SHIFT;
        let altgr = held.contains(KeyModifiers::ALT)
            && (held - KeyModifiers::ALT - KeyModifiers::CONTROL).is_empty()
            && !c.is_ascii_alphabetic();
        (self.inserting() && (held.is_empty() || altgr)).then_some(c)
    }

    /// Whether a key typed now would insert, rather than move or edit as
    /// vi's normal mode does.
    #[must_use]
    pub fn inserting(&self) -> bool {
        self.state.mode == EditorMode::Insert
    }

    /// vi's mode, for the status line; nothing for emacs keys.
    #[must_use]
    pub fn mode(&self) -> Option<&'static str> {
        (self.keys == Keys::Vi).then_some(match self.state.mode {
            EditorMode::Insert => "INSERT",
            EditorMode::Normal => "NORMAL",
            EditorMode::Visual => "VISUAL",
            EditorMode::Search => "SEARCH",
        })
    }

    /// Whether Esc stops typing: vi's steps back to normal mode first,
    /// and in normal mode drops a command still waiting for more; emacs
    /// has nowhere to step back to.
    #[must_use]
    pub fn esc_cancels(&self) -> bool {
        self.keys == Keys::Emacs
            || (self.state.mode == EditorMode::Normal && self.pending.is_empty())
    }

    /// What Esc does now, for the footer.
    #[must_use]
    pub fn esc_does(&self) -> &'static str {
        if self.esc_cancels() {
            "cancel"
        } else if self.pending.is_empty() {
            "normal mode"
        } else {
            "drop command"
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Took {
        let searching = self.state.mode == EditorMode::Search;
        match key.code {
            KeyCode::Enter if !searching => return Took::Commit,
            KeyCode::Esc if self.esc_cancels() => return Took::Cancel,
            // edtui drops what it's waiting on at a key that can't go on
            // from it, as Esc can't.
            KeyCode::Esc => self.pending.clear(),
            // Every vi command of more than a key is the field's to run
            // or drop whole, so edtui only ever gets commands of one key.
            _ if self.keys == Keys::Vi && self.state.mode == EditorMode::Normal => {
                let plain = !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                match key.code {
                    KeyCode::Char(c) if plain => {
                        self.pending.push(c);
                        let parsed = vi::parse(&self.pending);
                        if parsed != Parsed::Waiting {
                            self.pending.clear();
                        }
                        match parsed {
                            // edtui would repeat its own change, from
                            // before the field's.
                            Parsed::Edtui { key: '.', .. } if self.last != Last::Edtui => {
                                if let Last::Field(Some(cmd)) = self.last {
                                    self.run(&cmd);
                                }
                            }
                            Parsed::Edtui { times, .. } => {
                                for _ in 0..times {
                                    self.edtui(key);
                                }
                            }
                            Parsed::Run(cmd) => self.run(&cmd),
                            Parsed::Waiting | Parsed::Drop => {}
                        }
                        return Took::Edit;
                    }
                    _ => self.pending.clear(),
                }
            }
            _ => {}
        }
        self.edtui(key);
        Took::Edit
    }

    /// Gives edtui `key`, noting when it makes a change of its own for
    /// `.` to repeat: one outside insert mode, or that goes into it.
    /// Undoing and redoing aren't changes.
    fn edtui(&mut self, key: KeyEvent) {
        let (text, mode) = (self.text(), self.state.mode);
        self.handler.on_key_event(shifted(key), &mut self.state);
        let undoing = matches!(
            (key.code, key.modifiers.contains(KeyModifiers::CONTROL)),
            (KeyCode::Char('u'), false) | (KeyCode::Char('r'), true)
        );
        let changed = self.text() != text
            || (mode != EditorMode::Insert && self.state.mode == EditorMode::Insert);
        if mode != EditorMode::Insert && changed && !undoing {
            self.last = Last::Edtui;
        }
    }

    /// Runs a vi command of more than a key, as edtui can't. What it
    /// changes goes through edtui's own edits, so `u` undoes it, and what
    /// it cuts or yanks into the clipboard edtui pastes from.
    fn run(&mut self, cmd: &vi::Cmd) {
        let chars: Vec<char> = self.text().chars().collect();
        let at = self.state.cursor.col.min(chars.len());
        let Some(edit) = vi::edit(cmd, &chars, at) else {
            return;
        };
        match edit {
            Edit::Move(col) => self.state.cursor.col = col,
            Edit::Cut {
                op,
                from,
                to,
                cursor,
            } => {
                *self.clip.0.borrow_mut() = chars[from..to].iter().collect();
                if op == Op::Yank {
                    self.state.cursor.col = cursor;
                    return;
                }
                self.last = Last::Field((op == Op::Delete).then_some(*cmd));
                self.state.cursor.col = from;
                if op == Op::Delete {
                    // Leaves the cursor on a character, as `d` does.
                    self.state.execute(RemoveChar(to - from));
                    return;
                }
                // Going into insert mode saves the text for `u`, once for
                // the cut and what's typed after it, as vi's `c` is undone.
                SwitchMode(EditorMode::Insert).execute(&mut self.state);
                for _ in from..to {
                    let _ = self.state.lines.remove(Index2::new(0, from));
                }
                self.state.cursor.col = from;
            }
            Edit::Replace { at, with, count } => {
                self.last = Last::Field(Some(*cmd));
                self.state.cursor.col = at;
                self.state.execute(RemoveChar(count));
                self.state.cursor.col = at;
                for _ in 0..count {
                    self.state.execute(InsertChar(with));
                }
                self.state.cursor.col = at + count - 1;
            }
        }
    }

    /// The text with its cursor: a bar where typing inserts, else a block
    /// over the character it's on, and vi's selection shown as the block.
    #[must_use]
    pub fn line(&self) -> Line<'static> {
        let chars: Vec<char> = self.text().chars().collect();
        let at = self.state.cursor.col.min(chars.len());
        if self.inserting() {
            let before: String = chars[..at].iter().collect();
            let after: String = chars[at..].iter().collect();
            return Line::from(vec![Span::raw(before), "▏".slow_blink(), Span::raw(after)]);
        }
        let marked = |col: usize| {
            col == at
                || self
                    .state
                    .selection
                    .as_ref()
                    .is_some_and(|s| s.contains(&Index2::new(0, col)))
        };
        let mut spans = Vec::new();
        let mut run = String::new();
        let mut run_marked = false;
        for (col, c) in chars.iter().enumerate() {
            if marked(col) != run_marked && !run.is_empty() {
                spans.push(styled(std::mem::take(&mut run), run_marked));
            }
            run_marked = marked(col);
            run.push(*c);
        }
        if !run.is_empty() {
            spans.push(styled(run, run_marked));
        }
        if at == chars.len() {
            spans.push(styled(" ".into(), true));
        }
        Line::from(spans)
    }
}

/// The field's clipboard, shared with edtui's state; never the system's.
#[derive(Clone, Default)]
struct Clip(std::rc::Rc<std::cell::RefCell<String>>);

impl ClipboardTrait for Clip {
    fn set_text(&mut self, text: String) {
        *self.0.borrow_mut() = text;
    }

    fn get_text(&mut self) -> String {
        self.0.borrow().clone()
    }
}

/// `key` with Shift as edtui's keys are written: on a capital, which not
/// every terminal says, and off anything else, which some do.
fn shifted(mut key: KeyEvent) -> KeyEvent {
    if let KeyCode::Char(c) = key.code {
        key.modifiers.set(KeyModifiers::SHIFT, c.is_uppercase());
    }
    key
}

fn styled(text: String, marked: bool) -> Span<'static> {
    if marked {
        Span::styled(text, Style::new().reversed())
    } else {
        Span::raw(text)
    }
}

/// edtui's keys, made readline's where they differ: Ctrl-U and Ctrl-W kill
/// back to the start of the line and the word, and undo is Ctrl-X Ctrl-U.
/// Search, which a field of one line has no use for, is gone.
fn key_handler(keys: Keys) -> KeyEventHandler {
    let mut handler = match keys {
        Keys::Emacs => KeyEventHandler::emacs_mode(),
        Keys::Vi => KeyEventHandler::vim_mode(),
    };
    handler.insert(
        KeyEventRegister::i(vec![KeyInput::ctrl('w')]),
        DeleteWordBackward(1),
    );
    if keys == Keys::Emacs {
        handler.insert(
            KeyEventRegister::i(vec![KeyInput::ctrl('u')]),
            DeleteToFirstCharOfLine,
        );
        handler.insert(
            KeyEventRegister::i(vec![KeyInput::ctrl('x'), KeyInput::ctrl('u')]),
            Undo,
        );
        handler.remove(&KeyEventRegister::i(vec![KeyInput::ctrl('s')]));
    } else {
        handler.remove(&KeyEventRegister::n(vec![KeyInput::new('/')]));
    }
    handler
}

impl std::fmt::Debug for TextField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextField")
            .field("text", &self.text())
            .field("cursor", &self.state.cursor)
            .field("mode", &self.state.mode)
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

/// Fields are the same when they'd show the same.
impl PartialEq for TextField {
    fn eq(&self, other: &Self) -> bool {
        (self.text(), self.state.cursor, self.state.mode, self.keys)
            == (
                other.text(),
                other.state.cursor,
                other.state.mode,
                other.keys,
            )
    }
}

impl Eq for TextField {}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(field: &mut TextField, keys: &[KeyEvent]) -> String {
        for key in keys {
            assert_eq!(field.key(*key), Took::Edit, "{key:?}");
        }
        field.text()
    }

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn chars(text: &str) -> Vec<KeyEvent> {
        text.chars().map(|c| plain(KeyCode::Char(c))).collect()
    }

    #[test]
    fn readline_says_vi_first_then_the_editor() {
        let vi = Some("# mine\nset editing-mode vi\n");
        assert_eq!(guess_keys(vi, None, None), Keys::Vi);
        // The last setting wins, and an explicit emacs beats a vi editor.
        let back = Some("set editing-mode vi\nset  editing-mode  emacs\n");
        assert_eq!(guess_keys(back, Some("nvim"), None), Keys::Emacs);
        assert_eq!(
            guess_keys(Some("set bell-style none"), None, Some("vim")),
            Keys::Vi
        );
        assert_eq!(guess_keys(None, Some("/usr/bin/nvim -f"), None), Keys::Vi);
        // `$VISUAL` wins over `$EDITOR`, unless it's blank.
        assert_eq!(
            guess_keys(None, Some("emacsclient"), Some("vi")),
            Keys::Emacs
        );
        assert_eq!(guess_keys(None, Some(" "), Some("vi")), Keys::Vi);
        // Only the program's name counts, not the directory it's in.
        assert_eq!(
            guess_keys(None, None, Some("/opt/vim/bin/nano")),
            Keys::Emacs
        );
        assert_eq!(guess_keys(None, None, None), Keys::Emacs);
    }

    #[test]
    fn emacs_keys_move_and_kill_as_readline_does() {
        let mut field = TextField::new("one two three", Keys::Emacs);
        assert_eq!(typed(&mut field, &[ctrl('w')]), "one two ");
        assert_eq!(typed(&mut field, &[alt('b'), ctrl('k')]), "one ");
        assert_eq!(typed(&mut field, &[ctrl('a'), ctrl('f')]), "one ");
        assert_eq!(typed(&mut field, &chars("X")), "oXne ");
        assert_eq!(typed(&mut field, &[ctrl('e'), ctrl('b')]), "oXne ");
        assert_eq!(typed(&mut field, &chars("Y")), "oXneY ");
        assert_eq!(
            typed(&mut field, &[plain(KeyCode::Home), alt('f')]),
            "oXneY "
        );
        assert_eq!(typed(&mut field, &[ctrl('u')]), " ");
        assert_eq!(typed(&mut field, &[plain(KeyCode::End)]), " ");
        assert_eq!(typed(&mut field, &chars("z")), " z");
        assert_eq!(
            typed(
                &mut field,
                &[plain(KeyCode::Left), plain(KeyCode::Backspace)]
            ),
            "z"
        );
        assert_eq!(field.key(plain(KeyCode::Esc)), Took::Cancel);
        assert_eq!(field.key(plain(KeyCode::Enter)), Took::Commit);
        assert_eq!(field.mode(), None);
    }

    /// Each vi command the field runs, from a cursor on a line, and what
    /// vim makes of it (`nvim --clean` with `startofline`): the text, the
    /// cursor, whether it's inserting, and what's yanked or cut.
    const AS_VIM_DOES: &[(&str, usize, &str, &str, usize, bool, &str)] = &[
        (
            "one two-three four",
            5,
            "dd",
            "",
            0,
            false,
            "one two-three four",
        ),
        (
            "one two-three four",
            5,
            "cc",
            "",
            0,
            true,
            "one two-three four",
        ),
        (
            "one two-three four",
            5,
            "yy",
            "one two-three four",
            5,
            false,
            "one two-three four",
        ),
        (
            "one two-three four",
            5,
            "2dd",
            "one two-three four",
            5,
            false,
            "",
        ),
        (
            "one two-three four",
            5,
            "dgg",
            "",
            0,
            false,
            "one two-three four",
        ),
        (
            "one two-three four",
            5,
            "cgg",
            "",
            0,
            true,
            "one two-three four",
        ),
        (
            "one two-three four",
            5,
            "ygg",
            "one two-three four",
            0,
            false,
            "one two-three four",
        ),
        (
            "one two-three four",
            5,
            "dG",
            "",
            0,
            false,
            "one two-three four",
        ),
        (
            "one two-three four",
            5,
            "d_",
            "",
            0,
            false,
            "one two-three four",
        ),
        (
            "one two-three four",
            0,
            "dw",
            "two-three four",
            0,
            false,
            "one ",
        ),
        (
            "one two-three four",
            4,
            "dW",
            "one four",
            4,
            false,
            "two-three ",
        ),
        (
            "one two-three four",
            4,
            "de",
            "one -three four",
            4,
            false,
            "two",
        ),
        (
            "one two-three four",
            4,
            "dE",
            "one  four",
            4,
            false,
            "two-three",
        ),
        (
            "one two-three four",
            8,
            "db",
            "one twothree four",
            7,
            false,
            "-",
        ),
        (
            "one two-three four",
            8,
            "dB",
            "one three four",
            4,
            false,
            "two-",
        ),
        (
            "one two-three four",
            8,
            "d0",
            "three four",
            0,
            false,
            "one two-",
        ),
        (
            "one two-three four",
            8,
            "d^",
            "three four",
            0,
            false,
            "one two-",
        ),
        (
            "one two-three four",
            8,
            "d$",
            "one two-",
            7,
            false,
            "three four",
        ),
        (
            "one two-three four",
            8,
            "dh",
            "one twothree four",
            7,
            false,
            "-",
        ),
        (
            "one two-three four",
            8,
            "dl",
            "one two-hree four",
            8,
            false,
            "t",
        ),
        (
            "one two-three four",
            0,
            "d2w",
            "-three four",
            0,
            false,
            "one two",
        ),
        (
            "one two-three four",
            0,
            "2dw",
            "-three four",
            0,
            false,
            "one two",
        ),
        (
            "one two-three four",
            16,
            "d3l",
            "one two-three fo",
            15,
            false,
            "ur",
        ),
        (
            "one two-three four",
            0,
            "cw",
            " two-three four",
            0,
            true,
            "one",
        ),
        (
            "one two-three four",
            3,
            "cw",
            "onetwo-three four",
            3,
            true,
            " ",
        ),
        (
            "one two-three four",
            4,
            "cW",
            "one  four",
            4,
            true,
            "two-three",
        ),
        (
            "one two-three four",
            4,
            "ce",
            "one -three four",
            4,
            true,
            "two",
        ),
        (
            "one two-three four",
            8,
            "cb",
            "one twothree four",
            7,
            true,
            "-",
        ),
        (
            "one two-three four",
            8,
            "c$",
            "one two-",
            8,
            true,
            "three four",
        ),
        (
            "one two-three four",
            4,
            "yw",
            "one two-three four",
            4,
            false,
            "two",
        ),
        (
            "one two-three four",
            8,
            "yb",
            "one two-three four",
            7,
            false,
            "-",
        ),
        (
            "one two-three four",
            4,
            "ye",
            "one two-three four",
            4,
            false,
            "two",
        ),
        (
            "one two-three four",
            8,
            "y$",
            "one two-three four",
            8,
            false,
            "three four",
        ),
        (
            "one two-three four",
            8,
            "y0",
            "one two-three four",
            0,
            false,
            "one two-",
        ),
        (
            "one two-three four",
            0,
            "df-",
            "three four",
            0,
            false,
            "one two-",
        ),
        (
            "one two-three four",
            0,
            "dt-",
            "-three four",
            0,
            false,
            "one two",
        ),
        (
            "one two-three four",
            17,
            "dFo",
            "one two-three fr",
            15,
            false,
            "ou",
        ),
        (
            "one two-three four",
            17,
            "dTo",
            "one two-three for",
            16,
            false,
            "u",
        ),
        (
            "one two-three four",
            0,
            "cf-",
            "three four",
            0,
            true,
            "one two-",
        ),
        (
            "one two-three four",
            0,
            "ct-",
            "-three four",
            0,
            true,
            "one two",
        ),
        (
            "one two-three four",
            17,
            "cFo",
            "one two-three fr",
            15,
            true,
            "ou",
        ),
        (
            "one two-three four",
            17,
            "cTo",
            "one two-three for",
            16,
            true,
            "u",
        ),
        (
            "one two-three four",
            0,
            "yf-",
            "one two-three four",
            0,
            false,
            "one two-",
        ),
        (
            "one two-three four",
            0,
            "yt-",
            "one two-three four",
            0,
            false,
            "one two",
        ),
        (
            "one two-three four",
            17,
            "yFo",
            "one two-three four",
            15,
            false,
            "ou",
        ),
        (
            "one two-three four",
            17,
            "yTo",
            "one two-three four",
            16,
            false,
            "u",
        ),
        (
            "one two-three four",
            0,
            "d2fe",
            "e four",
            0,
            false,
            "one two-thre",
        ),
        (
            "one two-three four",
            0,
            "fz",
            "one two-three four",
            0,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "dfz",
            "one two-three four",
            0,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "f-",
            "one two-three four",
            7,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "t-",
            "one two-three four",
            6,
            false,
            "",
        ),
        (
            "one two-three four",
            17,
            "Fo",
            "one two-three four",
            15,
            false,
            "",
        ),
        (
            "one two-three four",
            17,
            "To",
            "one two-three four",
            16,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "2fe",
            "one two-three four",
            11,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "2te",
            "one two-three four",
            10,
            false,
            "",
        ),
        (
            "one two-three four",
            17,
            "2Fe",
            "one two-three four",
            11,
            false,
            "",
        ),
        (
            "one two-three four",
            10,
            "gg",
            "one two-three four",
            0,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "3w",
            "one two-three four",
            8,
            false,
            "",
        ),
        (
            "one two-three four",
            17,
            "2b",
            "one two-three four",
            8,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "3e",
            "one two-three four",
            7,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "3l",
            "one two-three four",
            3,
            false,
            "",
        ),
        (
            "one two-three four",
            5,
            "2h",
            "one two-three four",
            3,
            false,
            "",
        ),
        (
            "one two-three four",
            4,
            "3x",
            "one -three four",
            4,
            false,
            "two",
        ),
        (
            "one two-three four",
            5,
            "2X",
            "onewo-three four",
            3,
            false,
            " t",
        ),
        (
            "one two-three four",
            0,
            "rX",
            "Xne two-three four",
            0,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "3rX",
            "XXX two-three four",
            2,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "20rX",
            "one two-three four",
            0,
            false,
            "",
        ),
        (
            "one two-three four",
            5,
            "\"ayy",
            "one two-three four",
            5,
            false,
            "one two-three four",
        ),
        (
            "one two-three four",
            0,
            "\"adw",
            "two-three four",
            0,
            false,
            "one ",
        ),
        (
            "one two-three four",
            0,
            "3\"adw",
            "three four",
            0,
            false,
            "one two-",
        ),
        (
            "one two-three four",
            0,
            "\"a3dw",
            "three four",
            0,
            false,
            "one two-",
        ),
        (
            "one two-three four",
            5,
            "diw",
            "one -three four",
            4,
            false,
            "two",
        ),
        (
            "one two-three four",
            5,
            "daw",
            "one-three four",
            3,
            false,
            " two",
        ),
        (
            "one two-three four",
            5,
            "diW",
            "one  four",
            4,
            false,
            "two-three",
        ),
        (
            "one two-three four",
            5,
            "daW",
            "one four",
            4,
            false,
            "two-three ",
        ),
        (
            "one two-three four",
            5,
            "ciw",
            "one -three four",
            4,
            true,
            "two",
        ),
        (
            "one two-three four",
            5,
            "yiw",
            "one two-three four",
            4,
            false,
            "two",
        ),
        (
            "one two-three four",
            3,
            "daw",
            "one-three four",
            3,
            false,
            " two",
        ),
        (
            "one two-three four",
            3,
            "diw",
            "onetwo-three four",
            3,
            false,
            " ",
        ),
        (
            "one two-three four",
            15,
            "daw",
            "one two-three",
            12,
            false,
            " four",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            7,
            "di\"",
            "say \"\" (x (y) z) [a] {b} <c>",
            5,
            false,
            "hi there",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            7,
            "da\"",
            "say (x (y) z) [a] {b} <c>",
            4,
            false,
            "\"hi there\" ",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            0,
            "ci\"",
            "say \"\" (x (y) z) [a] {b} <c>",
            5,
            true,
            "hi there",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            0,
            "di\"",
            "say \"\" (x (y) z) [a] {b} <c>",
            5,
            false,
            "hi there",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            19,
            "di(",
            "say \"hi there\" (x () z) [a] {b} <c>",
            19,
            false,
            "y",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            19,
            "da(",
            "say \"hi there\" (x  z) [a] {b} <c>",
            18,
            false,
            "(y)",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            16,
            "dib",
            "say \"hi there\" () [a] {b} <c>",
            16,
            false,
            "x (y) z",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            16,
            "ci)",
            "say \"hi there\" () [a] {b} <c>",
            16,
            true,
            "x (y) z",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            16,
            "yi(",
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            16,
            false,
            "x (y) z",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            27,
            "di[",
            "say \"hi there\" (x (y) z) [] {b} <c>",
            26,
            false,
            "a",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            31,
            "da{",
            "say \"hi there\" (x (y) z) [a]  <c>",
            29,
            false,
            "{b}",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            31,
            "diB",
            "say \"hi there\" (x (y) z) [a] {} <c>",
            30,
            false,
            "b",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            35,
            "di<",
            "say \"hi there\" (x (y) z) [a] {b} <>",
            34,
            false,
            "c",
        ),
        (
            "say \"hi there\" (x (y) z) [a] {b} <c>",
            0,
            "di(",
            "say \"hi there\" () [a] {b} <c>",
            16,
            false,
            "x (y) z",
        ),
        (
            "one two-three four",
            0,
            "9e",
            "one two-three four",
            17,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "d9e",
            "",
            0,
            false,
            "one two-three four",
        ),
        (
            "one two-three four",
            17,
            "9b",
            "one two-three four",
            0,
            false,
            "",
        ),
        (
            "one two-three four",
            17,
            "d9b",
            "r",
            0,
            false,
            "one two-three fou",
        ),
        (
            "one two-three four",
            17,
            "de",
            "one two-three fou",
            16,
            false,
            "r",
        ),
        (
            "one two-three four",
            17,
            "ce",
            "one two-three fou",
            17,
            true,
            "r",
        ),
        (
            "one two-three four",
            17,
            "ye",
            "one two-three four",
            17,
            false,
            "r",
        ),
        (
            "one two-three four",
            0,
            "2$",
            "one two-three four",
            0,
            false,
            "",
        ),
        (
            "one two-three four",
            0,
            "d2$",
            "one two-three four",
            0,
            false,
            "",
        ),
        ("one two", 2, "cw", "on two", 2, true, "e"),
        ("one two", 2, "c2w", "on", 2, true, "e two"),
        ("one two", 6, "cw", "one tw", 6, true, "o"),
        ("a.b c", 0, "cw", ".b c", 0, true, "a"),
        ("a.b c", 1, "cw", "ab c", 1, true, "."),
        ("a.b c d", 1, "cW", "a c d", 1, true, ".b"),
        ("one two", 0, "d3iw", "", 0, false, "one two"),
        ("one two three", 0, "d2aw", "three", 0, false, "one two "),
        ("one two", 0, "y2iw", "one two", 0, false, "one "),
        ("a (b (c) d) e", 7, "d2i(", "a () e", 3, false, "b (c) d"),
        ("a (b) (c)", 0, "2di(", "a (b) (c)", 0, false, ""),
        ("(a)(b (c) d)", 9, "d2i(", "(a)(b (c) d)", 9, false, ""),
        (
            "x \"a\" y \"b\"",
            5,
            "di\"",
            "x \"a\"\"b\"",
            5,
            false,
            " y ",
        ),
        (
            "x \"a\" y \"b\"",
            6,
            "da\"",
            "x \"ab\"",
            4,
            false,
            "\" y \"",
        ),
        (
            "x \"a\" y \"b\"",
            3,
            "2di\"",
            "x  y \"b\"",
            2,
            false,
            "\"a\"",
        ),
        (
            "x \"a\" y \"b\"",
            3,
            "2da\"",
            "x y \"b\"",
            2,
            false,
            "\"a\" ",
        ),
        (
            "x \"a\" y \"b\"",
            5,
            "ya\"",
            "x \"a\" y \"b\"",
            4,
            false,
            "\" y \"",
        ),
        ("x \"a\" y", 6, "di\"", "x \"a\" y", 6, false, ""),
        ("  one", 3, "ygg", "  one", 2, false, "  one"),
        ("  one", 3, "yG", "  one", 2, false, "  one"),
        ("  one", 3, "y_", "  one", 3, false, "  one"),
        ("  one", 3, "W", "  one", 4, false, ""),
        ("  one", 3, "B", "  one", 2, false, ""),
        ("  one", 0, "E", "  one", 4, false, ""),
        ("  one", 4, "^", "  one", 2, false, ""),
        ("one two", 4, "X", "onetwo", 3, false, " "),
        ("one two", 0, "x", "ne two", 0, false, "o"),
        ("one", 2, "5X", "e", 0, false, "on"),
    ];

    #[test]
    fn vi_commands_do_what_vim_does() {
        for &(text, at, keys, want, cursor, inserting, clip) in AS_VIM_DOES {
            let mut field = TextField::new(text, Keys::Vi);
            field.key(plain(KeyCode::Esc));
            field.state.cursor.col = at;
            let got = typed(&mut field, &chars(keys));
            let case = format!("{keys:?} on {text:?} at {at}");
            assert_eq!(got, want, "{case}");
            assert_eq!(field.state.cursor.col, cursor, "{case}: cursor");
            assert_eq!(field.inserting(), inserting, "{case}: mode");
            assert_eq!(field.clip.0.borrow().as_str(), clip, "{case}: clipboard");
            assert!(field.pending.is_empty(), "{case}: still waiting");
            // What changed, `u` puts back whole, once out of insert mode.
            if inserting {
                field.key(plain(KeyCode::Esc));
            }
            if got != text {
                assert_eq!(typed(&mut field, &chars("u")), text, "{case}: undo");
            }
        }
    }

    #[test]
    fn dot_repeats_the_last_change_whoever_ran_it() {
        // As `nvim --clean` does.
        for (keys, want) in [
            ("xdw.", "three four"),
            ("dwx.", "o three four"),
            ("d2w.", ""),
            ("yiwdw.", "three four"),
            ("3rXw.", "XXX XXX three four"),
            ("dwu.", "two three four"),
        ] {
            let mut field = TextField::new("one two three four", Keys::Vi);
            field.key(plain(KeyCode::Esc));
            field.state.cursor.col = 0;
            assert_eq!(typed(&mut field, &chars(keys)), want, "{keys:?}");
        }
        // What's typed after the field's `c` is edtui's, so `.` does
        // nothing rather than repeat an older change.
        let mut field = TextField::new("one two", Keys::Vi);
        field.key(plain(KeyCode::Esc));
        typed(&mut field, &chars("0xcwz"));
        field.key(plain(KeyCode::Esc));
        assert_eq!(typed(&mut field, &chars(".")), "z two");
    }

    #[test]
    fn a_completion_in_normal_mode_leaves_dot_nothing() {
        let mut field = TextField::new("one two", Keys::Vi);
        field.key(plain(KeyCode::Esc));
        typed(&mut field, &chars("0x"));
        field.set_text("three");
        assert_eq!(
            typed(&mut field, &chars(".")),
            "three",
            "not edtui's `x` again"
        );
        assert_eq!(typed(&mut field, &chars("u")), "ne two");
    }

    #[test]
    fn a_completion_mid_insert_repeats_with_the_insert() {
        let mut field = TextField::new("x", Keys::Vi);
        field.key(plain(KeyCode::Esc));
        typed(&mut field, &chars("a o"));
        field.set_text("x org/api");
        field.key(plain(KeyCode::Esc));
        assert_eq!(typed(&mut field, &chars(".")), "x org/api org/api");
        // In chars, not bytes, and the cursor after the text.
        let mut field = TextField::new("é", Keys::Vi);
        field.set_text("éa");
        assert_eq!(typed(&mut field, &chars("b")), "éab");
    }

    #[test]
    fn a_completion_that_takes_back_text_leaves_dot_nothing() {
        let mut field = TextField::new("x", Keys::Vi);
        field.key(plain(KeyCode::Esc));
        typed(&mut field, &chars("a Or"));
        field.set_text("x org/api");
        field.key(plain(KeyCode::Esc));
        assert_eq!(typed(&mut field, &chars(".")), "x org/api");
        assert_eq!(
            typed(&mut field, &chars("u")),
            "x Or",
            "one `u` for the take"
        );
        // An edit after it is `.`'s again.
        assert_eq!(typed(&mut field, &chars("0x.")), "Or");
    }

    #[test]
    fn replacing_the_text_is_an_edit_undo_takes_back() {
        let mut field = TextField::new("one two", Keys::Vi);
        field.key(plain(KeyCode::Esc));
        typed(&mut field, &chars("0yw"));
        field.set_text("three");
        assert_eq!(field.text(), "three");
        assert_eq!(field.mode(), Some("NORMAL"));
        assert_eq!(typed(&mut field, &chars("u")), "one two");
        assert_eq!(
            typed(&mut field, &chars("P")),
            "one one two",
            "the yank's kept"
        );
    }

    #[test]
    fn vi_commands_the_field_doesnt_run_are_dropped_whole() {
        // An operator's other motions and objects, and `g`'s others.
        for keys in [
            "dj", "dk", "dis", "dap", "dit", "yz", "ge", "g_", "d%", "gx",
        ] {
            let mut field = TextField::new("one two", Keys::Vi);
            field.key(plain(KeyCode::Esc));
            assert_eq!(typed(&mut field, &chars(keys)), "one two", "{keys:?}");
            assert_eq!(field.state.cursor.col, 6, "{keys:?}");
            assert_eq!(field.esc_does(), "cancel", "{keys:?} left waiting");
            // edtui got none of it: `x` deletes, finishing nothing.
            assert_eq!(typed(&mut field, &chars("x")), "one tw", "{keys:?}");
        }
    }

    #[test]
    fn vi_r_f_and_t_work_though_edtui_has_none_of_them() {
        let normal = |text: &str| {
            let mut field = TextField::new(text, Keys::Vi);
            field.key(plain(KeyCode::Esc));
            field
        };
        let mut field = normal("one two");
        assert_eq!(typed(&mut field, &chars("rX")), "one twX");
        assert_eq!(field.mode(), Some("NORMAL"));
        // F lands on the character, T just after it; `x` shows where.
        let mut field = normal("one two");
        assert_eq!(typed(&mut field, &chars("Fox")), "ne two");
        let mut field = normal("one two");
        assert_eq!(typed(&mut field, &chars("Tex")), "onetwo");
        let mut field = normal("one two");
        assert_eq!(
            typed(&mut field, &chars("Fzx")),
            "one tw",
            "not found: stays put"
        );
        // An operator runs over what F or T moves across, not the cursor's
        // character, and what it cuts pastes.
        let mut field = normal("one two");
        assert_eq!(typed(&mut field, &chars("dFn")), "oo");
        assert_eq!(typed(&mut field, &chars("P")), "one two");
        let mut field = normal("one two");
        assert_eq!(typed(&mut field, &chars("cTo")), "oo");
        assert!(field.inserting());
        assert_eq!(typed(&mut field, &chars("X")), "oXo");
        let mut field = normal("one two");
        assert_eq!(typed(&mut field, &chars("yFe$p")), "one twoe tw");
        // With nothing to act on, the operator is dropped, not left for
        // the next key to finish.
        let mut field = normal("one two");
        assert_eq!(typed(&mut field, &chars("dFzw")), "one two");
        let mut field = normal("one two");
        assert_eq!(typed(&mut field, &chars("dTww")), "one two");
    }

    #[test]
    fn vi_esc_drops_a_waiting_command_before_it_cancels() {
        for prefix in [
            "d", "c", "y", "f", "F", "t", "T", "r", "g", "\"", "\"a", "3", "12", "3\"a", "\"a3",
            "2d", "d2", "df", "dF", "dt", "dT", "ci", "ya", "dg",
        ] {
            let mut field = TextField::new("one two", Keys::Vi);
            field.key(plain(KeyCode::Esc));
            typed(&mut field, &chars(prefix));
            assert_eq!(field.esc_does(), "drop command", "{prefix:?}");
            assert_eq!(field.key(plain(KeyCode::Esc)), Took::Edit, "{prefix:?}");
            assert_eq!(field.text(), "one two", "{prefix:?} did nothing");
            assert_eq!(field.mode(), Some("NORMAL"), "{prefix:?}");
            // edtui dropped it too: `x` deletes, not finishing the command.
            typed(&mut field, &chars("0x"));
            assert_eq!(field.text(), "ne two", "{prefix:?} left edtui waiting");
            assert_eq!(field.key(plain(KeyCode::Esc)), Took::Cancel, "{prefix:?}");
        }
        // A command that's done leaves nothing waiting.
        let mut field = TextField::new("one two", Keys::Vi);
        field.key(plain(KeyCode::Esc));
        typed(&mut field, &chars("0dw"));
        assert_eq!(field.text(), "two");
        assert_eq!(field.esc_does(), "cancel");
        assert_eq!(field.key(plain(KeyCode::Esc)), Took::Cancel);
    }

    #[test]
    fn vi_keys_that_cant_go_on_drop_a_waiting_command() {
        for key in [
            plain(KeyCode::Left),
            plain(KeyCode::Backspace),
            ctrl('x'),
            alt('x'),
        ] {
            let mut field = TextField::new("one two", Keys::Vi);
            field.key(plain(KeyCode::Esc));
            typed(&mut field, &chars("d"));
            field.key(key);
            assert_eq!(field.esc_does(), "cancel", "{key:?}");
            // edtui dropped it too: `x` deletes, not finishing `d`.
            typed(&mut field, &chars("0x"));
            assert_eq!(field.text(), "ne two", "{key:?} left edtui waiting");
        }
    }

    #[test]
    fn vi_counts_and_registers_arent_edtui_commands() {
        // edtui has neither, so neither reaches it: `"a` isn't append.
        let mut field = TextField::new("one two", Keys::Vi);
        field.key(plain(KeyCode::Esc));
        assert_eq!(typed(&mut field, &chars("\"ayy")), "one two");
        assert_eq!(field.mode(), Some("NORMAL"));
        // `10x`'s `0` isn't a motion to the line's start.
        assert_eq!(typed(&mut field, &chars("10x")), "one tw");
    }

    #[test]
    fn vi_esc_goes_to_normal_mode_then_cancels() {
        let mut field = TextField::new("one two", Keys::Vi);
        assert_eq!(field.mode(), Some("INSERT"));
        assert_eq!(field.key(plain(KeyCode::Esc)), Took::Edit);
        assert_eq!(field.mode(), Some("NORMAL"));
        assert!(!field.inserting());
        assert_eq!(typed(&mut field, &chars("0dw")), "two");
        assert_eq!(typed(&mut field, &chars("A")), "two");
        assert_eq!(typed(&mut field, &chars("s")), "twos");
        assert_eq!(typed(&mut field, &[ctrl('w')]), "");
        assert_eq!(field.key(plain(KeyCode::Esc)), Took::Edit);
        assert_eq!(field.key(plain(KeyCode::Esc)), Took::Cancel);
        assert_eq!(field.key(plain(KeyCode::Enter)), Took::Commit);
    }

    #[test]
    fn only_what_would_be_typed_inserts() {
        let field = TextField::new("", Keys::Emacs);
        assert_eq!(field.inserts(plain(KeyCode::Char('x'))), Some('x'));
        assert_eq!(
            field.inserts(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT)),
            Some('X')
        );
        assert_eq!(field.inserts(alt('@')), Some('@'), "AltGr");
        assert_eq!(field.inserts(alt('b')), None);
        assert_eq!(field.inserts(ctrl('a')), None);
        assert_eq!(field.inserts(plain(KeyCode::Left)), None);
        let mut vi = TextField::new("", Keys::Vi);
        vi.key(plain(KeyCode::Esc));
        assert_eq!(vi.inserts(plain(KeyCode::Char('x'))), None);
    }

    #[test]
    fn replaced_text_keeps_the_cursor_on_it_in_normal_mode() {
        let mut field = TextField::new("ab", Keys::Vi);
        typed(&mut field, &[plain(KeyCode::Esc)]);
        typed(&mut field, &chars("v"));
        field.set_text("abcd");
        assert_eq!(field.mode(), Some("NORMAL"));
        assert_eq!(typed(&mut field, &chars("x")), "abc");
        let mut field = TextField::new("ab", Keys::Vi);
        field.set_text("abcd");
        assert_eq!(typed(&mut field, &chars("e")), "abcde");
    }

    #[test]
    fn the_cursor_is_a_bar_inserting_and_a_block_in_normal_mode() {
        let shown = |field: &TextField| {
            field
                .line()
                .spans
                .iter()
                .map(|span| {
                    if span
                        .style
                        .add_modifier
                        .contains(ratatui::style::Modifier::REVERSED)
                    {
                        format!("[{}]", span.content)
                    } else {
                        span.content.to_string()
                    }
                })
                .collect::<String>()
        };
        let mut field = TextField::new("abc", Keys::Vi);
        assert_eq!(shown(&field), "abc▏");
        field.key(plain(KeyCode::Esc));
        assert_eq!(shown(&field), "ab[c]");
        typed(&mut field, &chars("0"));
        assert_eq!(shown(&field), "[a]bc");
        typed(&mut field, &chars("vl"));
        assert_eq!(shown(&field), "[ab]c");
        let empty = TextField::new("", Keys::Vi);
        assert_eq!(shown(&empty), "▏");
    }
}
