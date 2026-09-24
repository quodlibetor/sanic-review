//! Links to a draft's or thread's lines in GitHub's view of the PR's diff.

use std::fmt::Write;

use sanic_core::{pr::PrKey, run::Side};
use sha2::{Digest, Sha256};

/// GitHub's anchor for `path` in a PR's Files changed view: `diff-` and
/// the SHA-256 of the path, in hex.
#[must_use]
pub fn file_anchor(path: &str) -> String {
    let hash = Sha256::digest(path.as_bytes());
    hash.iter().fold(String::from("diff-"), |mut anchor, byte| {
        let _ = write!(anchor, "{byte:02x}");
        anchor
    })
}

/// GitHub's anchor for `lines` of `path` on `side`: `R12` for a line of
/// the new file, `L12` of the old, and `R10-R12` for a range.
#[must_use]
pub fn lines_anchor(path: &str, side: Side, (start, end): (u32, u32)) -> String {
    let s = match side {
        Side::Left => 'L',
        Side::Right => 'R',
    };
    let file = file_anchor(path);
    if start == end {
        format!("{file}{s}{end}")
    } else {
        format!("{file}{s}{start}-{s}{end}")
    }
}

/// Where links to a PR's lines on GitHub point: `key`, whose drafts and
/// threads are lines of the `reviewed` head, while the PR is at `current`
/// as last polled.
#[derive(Debug, Clone, Copy)]
pub struct At<'a> {
    pub key: &'a PrKey,
    pub reviewed: &'a str,
    pub current: &'a str,
}

impl At<'_> {
    /// `lines` of `path` on `side`, on GitHub. While the reviewed head is
    /// the PR's, that's in the PR's Files changed, which shows all of it;
    /// pinning that view to a commit would show only that commit's own
    /// changes. Once the PR has moved on, it's the lines in the file at the
    /// reviewed head, which has only the new side's.
    #[must_use]
    pub fn lines(self, path: &str, side: Side, lines: (u32, u32)) -> Option<String> {
        if self.reviewed == self.current {
            Some(format!(
                "{}/files#{}",
                self.key.url(),
                lines_anchor(path, side, lines)
            ))
        } else {
            (side == Side::Right).then(|| blob_url(self.key, self.reviewed, path, lines))
        }
    }

    /// `path` on GitHub, as [`At::lines`] links its lines: in the PR's
    /// Files changed, or the file at the reviewed head, which a `deleted`
    /// file isn't in.
    #[must_use]
    pub fn file(self, path: &str, deleted: bool) -> Option<String> {
        if self.reviewed == self.current {
            Some(format!("{}/files#{}", self.key.url(), file_anchor(path)))
        } else {
            (!deleted).then(|| blob_file(self.key, self.reviewed, path))
        }
    }
}

/// `path` at `head` on GitHub, as source even for Markdown (`plain=1`,
/// or GitHub renders it, without its lines).
fn blob_file(key: &PrKey, head: &str, path: &str) -> String {
    let path: Vec<String> = path.split('/').map(percent_encode).collect();
    format!(
        "https://github.com/{}/blob/{head}/{}?plain=1",
        key.repo,
        path.join("/")
    )
}

/// `lines` of `path` in the file at `head` on GitHub; see [`blob_file`].
#[must_use]
pub fn blob_url(key: &PrKey, head: &str, path: &str, (start, end): (u32, u32)) -> String {
    let lines = if start == end {
        format!("L{end}")
    } else {
        format!("L{start}-L{end}")
    };
    format!("{}#{lines}", blob_file(key, head, path))
}

/// `segment` with everything but RFC 3986's unreserved characters
/// percent-encoded, byte by byte.
fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use sanic_core::repo::RepoName;

    use super::*;

    const LIB: &str = "diff-b1a35a68f14e696205874893c07fd24fdb88882b47c23cc0e0c80a30c7d53759";

    #[test]
    fn a_files_anchor_is_the_sha256_of_its_path() {
        // `printf %s src/lib.rs | sha256sum`, and the same for the other.
        assert_eq!(file_anchor("src/lib.rs"), LIB);
        assert_eq!(
            file_anchor("dir/with space é.rs"),
            "diff-ed7a553f7301213fdc043c16bd80d28b3641d0257fd70ff9a78f6fe928133d99"
        );
    }

    #[test]
    fn lines_are_numbered_on_their_side_and_ranges_name_both_ends() {
        assert_eq!(
            lines_anchor("src/lib.rs", Side::Right, (12, 12)),
            format!("{LIB}R12")
        );
        assert_eq!(
            lines_anchor("src/lib.rs", Side::Right, (10, 12)),
            format!("{LIB}R10-R12")
        );
        assert_eq!(
            lines_anchor("src/lib.rs", Side::Left, (3, 4)),
            format!("{LIB}L3-L4")
        );
    }

    #[test]
    fn links_are_to_the_prs_diff_until_it_moves_on_then_to_the_file() {
        let key = PrKey {
            repo: RepoName::new("org", "repo"),
            number: 7,
        };
        let at = |current| At {
            key: &key,
            reviewed: "abc123",
            current,
        };
        let diff = "https://github.com/org/repo/pull/7/files";
        assert_eq!(
            at("abc123").lines("src/lib.rs", Side::Right, (10, 12)),
            Some(format!("{diff}#{LIB}R10-R12"))
        );
        assert_eq!(
            at("abc123").lines("src/lib.rs", Side::Left, (3, 3)),
            Some(format!("{diff}#{LIB}L3"))
        );
        let moved = at("def456");
        assert_eq!(
            moved
                .lines("src/a b#c.rs", Side::Right, (10, 12))
                .as_deref(),
            Some("https://github.com/org/repo/blob/abc123/src/a%20b%23c.rs?plain=1#L10-L12")
        );
        assert_eq!(moved.lines("src/lib.rs", Side::Left, (3, 3)), None);
        assert_eq!(
            at("abc123").file("src/lib.rs", true),
            Some(format!("{diff}#{LIB}"))
        );
        assert_eq!(
            moved.file("src/lib.rs", false).as_deref(),
            Some("https://github.com/org/repo/blob/abc123/src/lib.rs?plain=1")
        );
        // The reviewed head has no deleted file to link to.
        assert_eq!(moved.file("src/lib.rs", true), None);
    }
}
