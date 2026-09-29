//! The vi commands a field runs itself, on its one line, rather than
//! half-giving them to edtui: every command that takes more than a key
//! (an operator and its motion or text object, `f`, `t`, `F`, `T`, `r`,
//! `g`, and anything after a count or a register), so edtui only ever
//! gets whole commands of one key; and the motions, `x` and `X` alone,
//! as edtui lacks some and its `x` doesn't yank.

/// What the keys typed in normal mode so far come to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    /// A command begun, waiting for more.
    Waiting,
    /// One key edtui runs as a command of its own, `times` times.
    Edtui {
        key: char,
        times: usize,
    },
    Run(Cmd),
    /// A command the field doesn't run, dropped whole.
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cmd {
    pub count: usize,
    pub op: Option<Op>,
    pub what: What,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    Motion(Motion),
    /// The whole line: `dd`, `cc`, `yy`, and an operator's `_`, or its
    /// `gg` or `G` (`jumps`), which also take a yank's cursor to the
    /// line's first non-blank, as those motions do.
    Line {
        jumps: bool,
    },
    /// `i` or `a` (`around`) and the object: `w`, `W`, a quote or a
    /// bracket.
    Object {
        around: bool,
        kind: char,
    },
    /// `r` and its character.
    Replace(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    /// `w`, or `W` when `big`.
    Word {
        big: bool,
    },
    /// `e`, `E`.
    End {
        big: bool,
    },
    /// `b`, `B`.
    Back {
        big: bool,
    },
    /// `0`.
    Start,
    /// `^`, and `gg` on a line of its own.
    FirstNonBlank,
    /// `$`.
    LineEnd,
    /// `f`, `t` (`till`), and back, `F` and `T`.
    Find {
        back: bool,
        till: bool,
        to: char,
    },
    /// The `n`th of a [`Motion::Find`]'s character along.
    Nth {
        back: bool,
        till: bool,
        to: char,
        n: usize,
    },
}

/// Parses `keys`, typed in normal mode: a count, a register (`"` and its
/// name, which the field's one clipboard stands in for) and a count, any
/// of them left out, then the command.
#[must_use]
pub fn parse(keys: &[char]) -> Parsed {
    let (first, rest) = count(keys);
    let rest = match rest {
        ['"'] => return Parsed::Waiting,
        ['"', _, rest @ ..] => rest,
        _ => rest,
    };
    let (second, rest) = count(rest);
    let n = first.unwrap_or(1) * second.unwrap_or(1);
    let run = |op, what| Parsed::Run(Cmd { count: n, op, what });
    let Some((&key, after)) = rest.split_first() else {
        return Parsed::Waiting;
    };
    let op = match key {
        'd' => Some(Op::Delete),
        'c' => Some(Op::Change),
        'y' => Some(Op::Yank),
        _ => None,
    };
    if let Some(op) = op {
        let (third, after) = count(after);
        let n = n * third.unwrap_or(1);
        let run = |what| {
            Parsed::Run(Cmd {
                count: n,
                op: Some(op),
                what,
            })
        };
        return match after {
            [doubled] if *doubled == key => run(What::Line { jumps: false }),
            ['_'] => run(What::Line { jumps: false }),
            ['G'] | ['g', 'g'] => run(What::Line { jumps: true }),
            [] | ['i' | 'a' | 'g' | 'f' | 't' | 'F' | 'T'] => Parsed::Waiting,
            [ia @ ('i' | 'a'), kind] if is_object(*kind) => run(What::Object {
                around: *ia == 'a',
                kind: *kind,
            }),
            [find @ ('f' | 't' | 'F' | 'T'), to] => run(What::Motion(found(*find, *to))),
            [m] => motion(*m).map_or(Parsed::Drop, |m| run(What::Motion(m))),
            _ => Parsed::Drop,
        };
    }
    match (key, after) {
        ('f' | 't' | 'F' | 'T' | 'r' | 'g', []) => Parsed::Waiting,
        (find @ ('f' | 't' | 'F' | 'T'), [to]) => run(None, What::Motion(found(find, *to))),
        ('r', [with]) => run(None, What::Replace(*with)),
        ('g', ['g']) => run(None, What::Motion(Motion::FirstNonBlank)),
        ('g', _) => Parsed::Drop,
        // edtui's `x` doesn't yank, and it has no `X`, `W`, `B`, `E` or
        // `^`, so the field runs these one-key commands too.
        ('x', []) => run(Some(Op::Delete), What::Motion(Motion::Right)),
        ('X', []) => run(Some(Op::Delete), What::Motion(Motion::Left)),
        (m, []) if motion(m).is_some() => {
            motion(m).map_or(Parsed::Drop, |m| run(None, What::Motion(m)))
        }
        // Once is all a count does to the rest, but for pasting and
        // undoing, which edtui can simply do again.
        (key, []) => Parsed::Edtui {
            key,
            times: if matches!(key, 'p' | 'P' | 'u') { n } else { 1 },
        },
        _ => Parsed::Drop,
    }
}

/// The count `keys` start with, and what's after it.
fn count(keys: &[char]) -> (Option<usize>, &[char]) {
    match keys {
        [first, ..] if ('1'..='9').contains(first) => {
            let digits = keys.iter().take_while(|c| c.is_ascii_digit()).count();
            let n = keys[..digits].iter().fold(0usize, |n, d| {
                n.saturating_mul(10)
                    .saturating_add(d.to_digit(10).map_or(0, |d| d as usize))
            });
            (Some(n), &keys[digits..])
        }
        _ => (None, keys),
    }
}

fn is_object(kind: char) -> bool {
    matches!(
        kind,
        'w' | 'W' | '"' | '\'' | '`' | '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>'
    )
}

fn found(find: char, to: char) -> Motion {
    Motion::Find {
        back: find.is_uppercase(),
        till: matches!(find, 't' | 'T'),
        to,
    }
}

fn motion(key: char) -> Option<Motion> {
    Some(match key {
        'h' => Motion::Left,
        'l' => Motion::Right,
        'w' => Motion::Word { big: false },
        'W' => Motion::Word { big: true },
        'e' => Motion::End { big: false },
        'E' => Motion::End { big: true },
        'b' => Motion::Back { big: false },
        'B' => Motion::Back { big: true },
        '0' => Motion::Start,
        '^' => Motion::FirstNonBlank,
        '$' => Motion::LineEnd,
        _ => return None,
    })
}

/// What a command does to the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// The cursor goes to this column.
    Move(usize),
    /// The operator acts on the columns `from..to`, then the cursor goes
    /// to `cursor`.
    Cut {
        op: Op,
        from: usize,
        to: usize,
        cursor: usize,
    },
    /// `count` characters from `at` become `with`, the cursor on the last.
    Replace { at: usize, with: char, count: usize },
}

/// What `cmd` does to `line` with the cursor at `at`; `None` when it
/// can't, as when a motion finds nothing, and vi does nothing.
#[must_use]
pub fn edit(cmd: &Cmd, line: &[char], at: usize) -> Option<Edit> {
    let len = line.len();
    match cmd.what {
        What::Replace(with) => (at + cmd.count <= len).then_some(Edit::Replace {
            at,
            with,
            count: cmd.count,
        }),
        // There's no line but this one to count to.
        What::Line { .. } if cmd.count > 1 => None,
        What::Line { jumps } => cmd.op.map(|op| Edit::Cut {
            op,
            from: 0,
            to: len,
            // A yank keeps the cursor where it is, but for `gg`'s and
            // `G`'s.
            cursor: match (op, jumps) {
                (Op::Yank, false) => at,
                (Op::Yank, true) => first_non_blank(line),
                _ => 0,
            },
        }),
        What::Object { around, kind } => {
            let (from, to) = object(line, at, around, kind, cmd.count)?;
            let op = cmd.op?;
            Some(Edit::Cut {
                op,
                from,
                to,
                cursor: from,
            })
        }
        What::Motion(motion) => {
            // A count on `$` goes down lines, and there are none.
            if motion == Motion::LineEnd && cmd.count > 1 {
                return None;
            }
            let (mut to, mut inclusive, mut moved) = (at, false, false);
            let (steps, motion) = match (cmd.op, motion) {
                // `cw` on a word changes to its end, even from its last
                // character, and then on as `ce` does for the count.
                (Some(Op::Change), Motion::Word { big })
                    if line.get(at).is_some_and(|c| !c.is_whitespace()) =>
                {
                    let here = class(line[at], big);
                    while to + 1 < len && class(line[to + 1], big) == here {
                        to += 1;
                    }
                    (inclusive, moved) = (true, true);
                    (cmd.count - 1, Motion::End { big })
                }
                // A count finds that many of the character along.
                (_, Motion::Find { back, till, to }) => (
                    1,
                    Motion::Nth {
                        back,
                        till,
                        to,
                        n: cmd.count,
                    },
                ),
                _ => (cmd.count, motion),
            };
            for _ in 0..steps {
                match step(line, to, motion, cmd.op.is_some()) {
                    Some((next, incl)) => (to, inclusive, moved) = (next, incl, true),
                    // A motion that runs out stops where it got to; one
                    // that can't start fails whole.
                    None if moved => break,
                    None => return None,
                }
            }
            let Some(op) = cmd.op else {
                return Some(Edit::Move(to.min(len.saturating_sub(1))));
            };
            let (from, to) = if to < at {
                (to, at)
            } else {
                (at, (to + usize::from(inclusive)).min(len))
            };
            (from < to).then_some(Edit::Cut {
                op,
                from,
                to,
                cursor: from,
            })
        }
    }
}

/// Where one `motion` from `at` lands, and whether an operator takes the
/// character there too; `None` when it can't move.
fn step(line: &[char], at: usize, motion: Motion, operating: bool) -> Option<(usize, bool)> {
    let len = line.len();
    Some(match motion {
        Motion::Left => (at.checked_sub(1)?, false),
        Motion::Right => {
            // Without an operator the cursor stays on a character.
            let last = if operating {
                len
            } else {
                len.saturating_sub(1)
            };
            ((at < last).then_some(at + 1)?, false)
        }
        Motion::Start => (0, false),
        Motion::FirstNonBlank => (first_non_blank(line), false),
        Motion::LineEnd => (len.checked_sub(1)?, true),
        Motion::Word { big } => {
            if at >= len {
                return None;
            }
            let mut to = at;
            let here = class(line[at], big);
            if here != 0 {
                while to < len && class(line[to], big) == here {
                    to += 1;
                }
            }
            while to < len && line[to].is_whitespace() {
                to += 1;
            }
            if to == at {
                return None;
            }
            // Off the end of the line, an operator stops there; the
            // cursor stays on the last character.
            (if operating { to } else { to.min(len - 1) }, false)
        }
        Motion::End { big } => {
            let mut to = at + 1;
            while to < len && line[to].is_whitespace() {
                to += 1;
            }
            if to >= len {
                // An operator still takes the rest of the line.
                return (operating && at < len).then_some((len - 1, true));
            }
            let here = class(line[to], big);
            while to + 1 < len && class(line[to + 1], big) == here {
                to += 1;
            }
            (to, true)
        }
        Motion::Back { big } => {
            let mut to = at.checked_sub(1)?;
            while to > 0 && line[to].is_whitespace() {
                to -= 1;
            }
            let here = class(line[to], big);
            while to > 0 && class(line[to - 1], big) == here && here != 0 {
                to -= 1;
            }
            (to, false)
        }
        Motion::Find { back, till, to } => step(
            line,
            at,
            Motion::Nth {
                back,
                till,
                to,
                n: 1,
            },
            operating,
        )?,
        Motion::Nth { back, till, to, n } => {
            if back {
                let found = (0..at).rev().filter(|&i| line[i] == to).nth(n - 1)?;
                (if till { found + 1 } else { found }, false)
            } else {
                let found = (at + 1..len).filter(|&i| line[i] == to).nth(n - 1)?;
                (if till { found - 1 } else { found }, true)
            }
        }
    })
}

/// A word character's class, as `w` and `iw` see them: blank, a word
/// of letters, digits and `_`, or of other marks; `big` words are of
/// anything but blanks.
fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

fn first_non_blank(line: &[char]) -> usize {
    line.iter()
        .position(|c| !c.is_whitespace())
        .unwrap_or(line.len().saturating_sub(1))
}

/// The columns `count` text objects cover, `from..to`.
fn object(
    line: &[char],
    at: usize,
    around: bool,
    kind: char,
    count: usize,
) -> Option<(usize, usize)> {
    if at >= line.len() {
        return None;
    }
    match kind {
        'w' | 'W' => word_object(line, at, around, kind == 'W', count),
        '"' | '\'' | '`' => quote_object(line, at, around, kind, count),
        _ => bracket_object(line, at, around, kind, count),
    }
}

/// `iw`, `aw`, and `W`'s: the word or blanks under the cursor, and for
/// `a` the blanks after it, or else those before; with a count, as many
/// words and blanks (`i`) or words with their blanks (`a`) along.
fn word_object(
    line: &[char],
    at: usize,
    around: bool,
    big: bool,
    count: usize,
) -> Option<(usize, usize)> {
    let len = line.len();
    let run = |at: usize| {
        let here = class(line[at], big);
        let mut from = at;
        while from > 0 && class(line[from - 1], big) == here {
            from -= 1;
        }
        let mut to = at + 1;
        while to < len && class(line[to], big) == here {
            to += 1;
        }
        (from, to)
    };
    let (mut from, mut to) = run(at);
    if around {
        if line[at].is_whitespace() {
            // Blanks, then the word after them.
            if to < len {
                to = run(to).1;
            }
        } else if to < len && line[to].is_whitespace() {
            to = run(to).1;
        } else if from > 0 && line[from - 1].is_whitespace() {
            from = run(from - 1).0;
        }
    }
    for _ in 1..count {
        if to >= len {
            return None;
        }
        to = run(to).1;
        if around && to < len {
            to = run(to).1;
        }
    }
    Some((from, to))
}

/// A quote's `i` and `a`: on a quote, the pair it's in, counting pairs
/// from the start of the line; else the quotes either side of the
/// cursor, or the first pair after it. `a` takes the quotes and the
/// blanks after them, or else those before; `i` after a count takes the
/// quotes and no blanks.
fn quote_object(
    line: &[char],
    at: usize,
    around: bool,
    kind: char,
    count: usize,
) -> Option<(usize, usize)> {
    let len = line.len();
    let next = |from: usize| (from..len).find(|&i| line[i] == kind);
    let (open, close) = if line[at] == kind {
        let quotes: Vec<usize> = (0..len).filter(|&i| line[i] == kind).collect();
        let pair = quotes
            .chunks(2)
            .find(|pair| pair.len() == 2 && pair[1] >= at)?;
        (pair[0], pair[1])
    } else if let Some(open) = (0..at).rev().find(|&i| line[i] == kind) {
        (open, next(at + 1)?)
    } else {
        let open = next(at + 1)?;
        (open, next(open + 1)?)
    };
    if !around {
        return Some(if count > 1 {
            (open, close + 1)
        } else {
            (open + 1, close)
        });
    }
    let mut to = close + 1;
    while to < len && line[to].is_whitespace() {
        to += 1;
    }
    let mut from = open;
    if to == close + 1 {
        while from > 0 && line[from - 1].is_whitespace() {
            from -= 1;
        }
    }
    Some((from, to))
}

/// A bracket's `i` and `a`: the innermost pair around the cursor, or on
/// it, else the first after it; with a count, the pair that many out.
fn bracket_object(
    line: &[char],
    at: usize,
    around: bool,
    kind: char,
    count: usize,
) -> Option<(usize, usize)> {
    let len = line.len();
    let (open, close) = match kind {
        '(' | ')' | 'b' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        '{' | '}' | 'B' => ('{', '}'),
        _ => ('<', '>'),
    };
    // The open bracket of the pair around `start`, or that a close
    // bracket there ends, when the cursor's `on` it.
    let around_it = |start: usize, on: bool| {
        let mut depth = 0usize;
        for i in (0..=start).rev() {
            if line[i] == close && !(on && i == start) {
                depth += 1;
            } else if line[i] == open {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
        }
        None
    };
    let mut from = around_it(at, true);
    for _ in 1..count {
        from = from.and_then(|f| around_it(f.checked_sub(1)?, false));
    }
    let from = from.or_else(|| (count == 1).then(|| (at..len).find(|&i| line[i] == open))?)?;
    let mut depth = 0usize;
    let mut to = None;
    for (i, &c) in line.iter().enumerate().skip(from + 1) {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                to = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let to = to?;
    Some(if around {
        (from, to + 1)
    } else {
        (from + 1, to)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn commands_wait_until_theyre_whole() {
        for waiting in [
            "d", "c", "y", "f", "F", "t", "T", "r", "g", "\"", "\"a", "3", "12", "3\"a", "\"a3",
            "2d", "d2", "df", "dF", "dt", "dT", "di", "ya", "dg", "d2f", "\"ad",
        ] {
            assert_eq!(parse(&keys(waiting)), Parsed::Waiting, "{waiting:?}");
        }
        // One key, or one after a count or register, is edtui's.
        assert_eq!(parse(&keys("D")), Parsed::Edtui { key: 'D', times: 1 });
        assert_eq!(parse(&keys("\"ap")), Parsed::Edtui { key: 'p', times: 1 });
        assert_eq!(parse(&keys("3p")), Parsed::Edtui { key: 'p', times: 3 });
        assert_eq!(parse(&keys("3i")), Parsed::Edtui { key: 'i', times: 1 });
        assert_eq!(parse(&keys("dgx")), Parsed::Drop);
        assert_eq!(parse(&keys("diz")), Parsed::Drop);
        let run = |text| match parse(&keys(text)) {
            Parsed::Run(cmd) => cmd,
            other => panic!("{text:?}: {other:?}"),
        };
        assert_eq!(run("3\"a2dw").count, 6, "counts multiply");
        assert_eq!(run("dgg").what, What::Line { jumps: true });
        assert_eq!(run("10x").what, What::Motion(Motion::Right));
    }
}
