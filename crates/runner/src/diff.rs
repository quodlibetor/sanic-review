//! Which lines of a PR's diff GitHub accepts inline comments on.
//!
//! GitHub anchors a comment to a line inside a diff hunk, context lines
//! included: `RIGHT` lines are numbered in the new file, `LEFT` lines in the
//! old one. A multi-line comment must stay inside one hunk.

use std::{collections::HashMap, ops::Range};

use sanic_core::run::{InlineComment, Side};

#[derive(Debug, Default)]
pub struct DiffIndex {
    /// Keyed by the path GitHub uses: the new path, or the old one for a
    /// deleted file.
    files: HashMap<String, Vec<Hunk>>,
}

#[derive(Debug)]
struct Hunk {
    old: Range<u32>,
    new: Range<u32>,
}

impl Hunk {
    fn lines(&self, side: Side) -> &Range<u32> {
        match side {
            Side::Left => &self.old,
            Side::Right => &self.new,
        }
    }
}

impl DiffIndex {
    /// Indexes `git diff` output made with `a/` and `b/` prefixes.
    #[must_use]
    pub fn parse(diff: &str) -> Self {
        let mut index = Self::default();
        let mut old_path: Option<String> = None;
        let mut path: Option<String> = None;
        // File headers come between `diff --git` and the first hunk; after
        // that, a `--- ` line is a removed line that began with `-- `.
        let mut in_header = false;
        for line in diff.lines() {
            if line.starts_with("diff --git ") {
                in_header = true;
                old_path = None;
                path = None;
            } else if in_header && let Some(p) = line.strip_prefix("--- ") {
                old_path = header_path(p, "a/");
            } else if in_header && let Some(p) = line.strip_prefix("+++ ") {
                path = header_path(p, "b/").or_else(|| old_path.clone());
            } else if let Some(header) = line.strip_prefix("@@ ") {
                in_header = false;
                if let (Some(path), Some(hunk)) = (&path, parse_hunk_header(header)) {
                    index.files.entry(path.clone()).or_default().push(hunk);
                }
            }
        }
        index
    }

    /// Whether GitHub would accept `comment`'s anchor.
    #[must_use]
    pub fn anchors(&self, comment: &InlineComment) -> bool {
        let Some(hunks) = self.files.get(&comment.path) else {
            return false;
        };
        let start = comment.start_line.unwrap_or(comment.line);
        start <= comment.line
            && hunks.iter().any(|h| {
                let lines = h.lines(comment.side);
                lines.contains(&start) && lines.contains(&comment.line)
            })
    }
}

/// The path on a `---` or `+++` line, without its `a/` or `b/` prefix;
/// `None` for `/dev/null`. Git ends the line with a tab when the name has a
/// space, and C-quotes names with special characters.
fn header_path(raw: &str, prefix: &str) -> Option<String> {
    let raw = raw.strip_suffix('\t').unwrap_or(raw);
    let name = match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(quoted) => unquote(quoted),
        None => raw.to_owned(),
    };
    name.strip_prefix(prefix).map(String::from)
}

/// Undoes git's C-style quoting: `\\`, `\"`, `\t` and friends, and `\ooo`
/// octal bytes.
fn unquote(quoted: &str) -> String {
    let bytes = quoted.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if b != b'\\' || i == bytes.len() {
            out.push(b);
            continue;
        }
        let escaped = bytes[i];
        i += 1;
        out.push(match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'0'..=b'3' if bytes.len() >= i + 2 => {
                let octal = std::str::from_utf8(&bytes[i - 1..i + 2]).ok();
                i += 2;
                octal
                    .and_then(|o| u8::from_str_radix(o, 8).ok())
                    .unwrap_or(escaped)
            }
            other => other,
        });
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `-a[,b] +c[,d] @@...` into line ranges.
fn parse_hunk_header(header: &str) -> Option<Hunk> {
    let mut fields = header.split_whitespace();
    let old = parse_range(fields.next()?.strip_prefix('-')?)?;
    let new = parse_range(fields.next()?.strip_prefix('+')?)?;
    Some(Hunk { old, new })
}

fn parse_range(spec: &str) -> Option<Range<u32>> {
    let (start, len) = match spec.split_once(',') {
        Some((start, len)) => (start.parse().ok()?, len.parse().ok()?),
        None => (spec.parse().ok()?, 1),
    };
    Some(start..start + len)
}

#[cfg(test)]
mod tests {
    use sanic_core::run::{Confidence, Severity};

    use super::*;

    const DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,4 +10,5 @@ fn main() {
 context
--- removed line that looks like a header
+++ added line that looks like a header
+added
 context
@@ -40 +41 @@
-old
+new
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,3 @@
+x
+y
+z
diff --git a/img.png b/img.png
Binary files a/img.png and b/img.png differ
";

    fn comment(path: &str, side: Side, start: Option<u32>, line: u32) -> InlineComment {
        InlineComment {
            path: path.into(),
            line,
            start_line: start,
            side,
            body: String::new(),
            severity: Severity::Nit,
            confidence: Confidence::Low,
        }
    }

    #[test]
    fn right_side_lines_are_numbered_in_the_new_file() {
        let index = DiffIndex::parse(DIFF);
        let right = |line| index.anchors(&comment("src/lib.rs", Side::Right, None, line));
        assert!(!right(9));
        assert!(right(10));
        assert!(right(14));
        assert!(!right(15));
        assert!(right(41));
        assert!(!right(40));
    }

    #[test]
    fn left_side_lines_are_numbered_in_the_old_file() {
        let index = DiffIndex::parse(DIFF);
        assert!(index.anchors(&comment("src/lib.rs", Side::Left, None, 40)));
        assert!(!index.anchors(&comment("src/lib.rs", Side::Left, None, 41)));
        assert!(index.anchors(&comment("gone.txt", Side::Left, None, 2)));
        assert!(!index.anchors(&comment("gone.txt", Side::Right, None, 1)));
        assert!(index.anchors(&comment("new.txt", Side::Right, None, 3)));
    }

    #[test]
    fn multi_line_comments_stay_in_one_hunk() {
        let index = DiffIndex::parse(DIFF);
        let span =
            |start, line| index.anchors(&comment("src/lib.rs", Side::Right, Some(start), line));
        assert!(span(10, 14));
        assert!(!span(14, 41));
        assert!(!span(12, 11));
    }

    #[test]
    fn paths_with_spaces_and_quotes_are_unwrapped() {
        let diff = "\
diff --git a/with space.txt b/with space.txt
--- a/with space.txt\t
+++ b/with space.txt\t
@@ -1 +1 @@
-a
+b
diff --git \"a/tab\\there \\\"q\\\" \\303\\251\" \"b/tab\\there \\\"q\\\" \\303\\251\"
--- \"a/tab\\there \\\"q\\\" \\303\\251\"\t
+++ \"b/tab\\there \\\"q\\\" \\303\\251\"\t
@@ -1 +1 @@
-a
+b
";
        let index = DiffIndex::parse(diff);
        assert!(index.anchors(&comment("with space.txt", Side::Right, None, 1)));
        assert!(index.anchors(&comment("tab\there \"q\" é", Side::Right, None, 1)));
    }

    #[test]
    fn unknown_and_binary_files_have_no_anchors() {
        let index = DiffIndex::parse(DIFF);
        assert!(!index.anchors(&comment("img.png", Side::Right, None, 1)));
        assert!(!index.anchors(&comment("elsewhere.rs", Side::Right, None, 1)));
    }
}
