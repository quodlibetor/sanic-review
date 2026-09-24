//! The demo's invented PRs, reviews and drafts: everything the dashboard
//! shows in the README's screenshots. Edit here to change what they show.
//!
//! You are `quodlibetor`; everyone else is made up. Times are hours before
//! [`NOW`], the demo clock's fixed time.

use std::{
    fmt::Write,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, WrapErr, ensure, eyre};
use sanic_core::{
    clock::rfc3339,
    pr::{Comment, Placement, PrKey, PrSnapshot, Review, ReviewState, Thread},
    repo::RepoName,
    run::{
        Confidence, DraftComment, InlineComment, ReviewRequest, ReviewResult, ReviewTrigger,
        Severity, Side, Verdict,
    },
};
use sanic_store::{DraftRow, DraftStatus, List, Store};
use sha2::{Digest, Sha256};

/// The GitHub login the demo dashboard is for.
pub const ME: &str = "quodlibetor";

/// The demo clock's time: 2026-06-12T15:00:00Z.
pub const NOW: u64 = 1_781_276_400;

/// The demo's config: every repo is reviewed, dependency bumps aren't.
pub const CONFIG: &str = r#"
[review_requests]
skip_titles = ["build(deps)*"]

[profile.default]
repos = [{ github = "quodlibetor" }]
"#;

/// The dashboard's recency window, in days. The index says how many PRs it
/// hides only as counted under this same window.
pub const WINDOW_DAYS: u32 = 14;

/// The showcase PR: drafts, a note, an existing thread and a diff.
pub const SHOWCASE: (&str, &str, u32) = ("quodlibetor", "frobnicator", 42);

/// What seeding wrote, beyond the store.
pub struct Seeded {
    /// Each run's diff, by run id, for `runs/<id>/pr.diff`.
    pub diffs: Vec<(i64, &'static str)>,
    /// Files at commits, as `(commit, path, text)`, for expanding context.
    pub files: Vec<(String, &'static str, &'static str)>,
    /// The store's own times, which it writes as the real now, to pin.
    pins: Pins,
}

/// Seeds `store` with the demo, and pins its times once it's closed; see
/// [`pin_times`].
pub fn seed(store: &mut Store) -> Result<Seeded> {
    let mut seeded = Seeded {
        diffs: Vec::new(),
        files: Vec::new(),
        pins: Pins::default(),
    };
    owed(store, &mut seeded)?;
    mine_prs(store, &mut seeded)?;
    Ok(seeded)
}

/// Reviews you owe, in each of the index's groups.
fn owed(store: &mut Store, s: &mut Seeded) -> Result<()> {
    // Needs you.
    showcase(store, s)?;
    to_answer(store, s)?;
    to_post(store, s)?;
    failed(store, s)?;
    // In flight.
    let pr = snapshot(
        ("flock", 19),
        "gotta-go-fast",
        "Make flock wait politely instead of tapping its foot",
        0,
    );
    record(store, s, &pr, &[("wait", 1)])?;
    let run = queue(store, &pr)?;
    claim(store, run)?;
    s.pins.run(run, 0.1, Some(0.05), None);
    let pr = snapshot(
        ("jsonlogprint", 23),
        "chilidog-enjoyer",
        "Print each log line in the colour of its mood",
        0,
    );
    record(store, s, &pr, &[("mood", 0)])?;
    let run = queue(store, &pr)?;
    s.pins.run(run, 0.02, None, None);
    // Nothing to do now.
    done(store, s)?;
    let pr = PrSnapshot {
        body: "Bumps hamster-food from 1.9.9 to 2.0.0. Now with 20% more crunch.".into(),
        ..snapshot(
            ("sanic-review", 314),
            "bump-o-tron",
            "build(deps): bump hamster-food from 1.9.9 to 2.0.0",
            6,
        )
    };
    record(store, s, &pr, &[("food", 6)])?;
    let pr = snapshot(
        ("frobnicator", 40),
        "chilidog-enjoyer",
        "Rewrite the frobnicator in a language nobody has heard of",
        30,
    );
    record(store, s, &pr, &[("rewrite", 30)])?;
    store.set_archived(&pr.key, true)?;
    store.set_hidden(List::Owed, WINDOW_DAYS, 3)?;
    Ok(())
}

/// Your PRs: ready, waiting on reviewers, and two that need you.
fn mine_prs(store: &mut Store, s: &mut Seeded) -> Result<()> {
    let pr = PrSnapshot {
        review_decision: Some("APPROVED".into()),
        merge_state: Some("CLEAN".into()),
        checks: Some("SUCCESS".into()),
        ..mine(
            ("sanic-review", 271),
            "Make the dashboard go fast (sonic, not sanic)",
            3,
        )
    };
    let pr = PrSnapshot {
        reviews: vec![review(&pr, "ringcollector", ReviewState::Approved, 1)],
        ..pr
    };
    record(store, s, &pr, &[("sonic", 5)])?;
    let pr = mine(
        ("s3glob", 129),
        "Add --really to confirm you meant --delete",
        4,
    );
    let pr = PrSnapshot {
        review_decision: Some("REVIEW_REQUIRED".into()),
        merge_state: Some("BLOCKED".into()),
        checks: Some("SUCCESS".into()),
        // On the head before the last push.
        reviews: vec![Review {
            commit: Some(sha("really")),
            ..review(&pr, "gotta-go-fast", ReviewState::Commented, 18)
        }],
        ..pr
    };
    record(store, s, &pr, &[("really", 26), ("really-2", 4)])?;
    let pr = PrSnapshot {
        review_decision: Some("APPROVED".into()),
        merge_state: Some("UNSTABLE".into()),
        checks: Some("FAILURE".into()),
        ..mine(
            ("mdserve", 12),
            "Serve Markdown with a little more flair",
            2,
        )
    };
    let pr = PrSnapshot {
        reviews: vec![review(&pr, "sleepy-semicolon", ReviewState::Approved, 2)],
        ..pr
    };
    record(store, s, &pr, &[("flair", 8)])?;
    let pr = mine(
        ("jj-spr", 77),
        "Teach spr to stack pancakes as well as commits",
        1,
    );
    let pr = PrSnapshot {
        review_decision: Some("CHANGES_REQUESTED".into()),
        reviews: vec![review(
            &pr,
            "chilidog-enjoyer",
            ReviewState::ChangesRequested,
            1,
        )],
        threads: vec![thread(
            &pr,
            "spr-1",
            ("src/stack.rs", 88),
            &[(
                "chilidog-enjoyer",
                1,
                "What happens when the pancakes conflict? Syrup everywhere, I assume.",
            )],
        )],
        ..pr
    };
    record(store, s, &pr, &[("pancakes", 9)])?;
    Ok(())
}

/// The PR the README shows in full: a finished review with pending
/// drafts, one accepted, private notes, a Markdown summary with code, a
/// suggestion, and a draft on lines someone already commented on.
fn showcase(store: &mut Store, s: &mut Seeded) -> Result<()> {
    let pr = PrSnapshot {
        body: SHOWCASE_BODY.into(),
        ..snapshot(
            (SHOWCASE.1, SHOWCASE.2),
            "gotta-go-fast",
            "Teach the frobnicator to count past three",
            2,
        )
    };
    let pr = PrSnapshot {
        reviews: vec![review(&pr, "ringcollector", ReviewState::Commented, 3)],
        threads: vec![thread(
            &pr,
            "frob-1",
            ("src/count.rs", 6),
            &[
                (
                    "ringcollector",
                    3,
                    "Is `u32::MAX` really the biggest number anyone needs? Asking for a \
                     friend with 4,294,967,296 widgets.",
                ),
                ("gotta-go-fast", 2, "Your friend can open an issue."),
            ],
        )],
        ..pr
    };
    record(store, s, &pr, &[("count", 4)])?;
    let run = queue(store, &pr)?;
    claim(store, run)?;
    store.finish_review(run, &showcase_review())?;
    // The nit is accepted; the rest wait on you.
    for draft in store.draft_rows(run)? {
        if is_nit(&draft) {
            decide(store, &draft, DraftStatus::Accepted)?;
        }
    }
    s.pins.run(run, 2.0, Some(1.95), Some(1.8));
    s.diffs.push((run, SHOWCASE_DIFF));
    for (path, base, head_text) in SHOWCASE_FILES {
        s.files.push((pr.base_sha.clone(), path, base));
        s.files.push((pr.head_sha.clone(), path, head_text));
    }
    Ok(())
}

/// The showcase's review, as the agent wrote it.
fn showcase_review() -> ReviewResult {
    ReviewResult {
        summary: "Counting past three is overdue, and skipping frobbed things is a nice \
                touch. Two things before it lands:\n\n\
                1. `n = n + 1` wraps in release builds. It's unlikely at `u32`, but \
                `MAX` now promises there's no limit.\n\
                2. Nothing tests the skipping. The loop is also a one-liner:\n\n\
                ```rust\n\
                let n = things.iter().filter(|t| !t.is_frobbed()).count();\n\
                ```\n\n\
                The rest are nits."
            .into(),
        summary_note: Some(
            "I read every caller of `count` and `announce`; none relies on the old cap.".into(),
        ),
        verdict: Verdict::RequestChanges,
        comments: vec![
            draft(
                ("src/count.rs", 15),
                Severity::Major,
                Confidence::High,
                "This wraps silently once there are more than `u32::MAX` things. \
                     `saturating_add` keeps the old \"give up politely\" spirit:\n\n\
                     ```suggestion\n        n = n.saturating_add(1);\n```",
                Some(
                    "Overflow is only a panic in debug builds; release builds wrap to 0, \
                         so the frobnicator would announce \"none\".",
                ),
            ),
            draft(
                ("src/count.rs", 6),
                Severity::Minor,
                Confidence::Medium,
                "`count` no longer reads `MAX`. If nothing else does, it can go, along \
                     with the question of how big a number anyone needs.",
                Some(
                    "Grepped the crate for `MAX`: only the docs link in `count`'s old comment used it.",
                ),
            ),
            draft(
                ("src/count.rs", 17),
                Severity::Nit,
                Confidence::High,
                "A trailing `return` isn't idiomatic Rust:\n\n\
                     ```suggestion\n    n\n```",
                None,
            ),
            draft(
                ("tests/count.rs", 12),
                Severity::Minor,
                Confidence::High,
                "This counts to four, which is the headline, but nothing covers the \
                     frobbed things `count` now skips. A test with one frobbed `Thing` would \
                     pin that down.",
                None,
            ),
        ],
        session_id: Some("demo-session".into()),
        transcript_path: "transcript.jsonl".into(),
    }
}

/// A review you posted, whose author answered your comment.
fn to_answer(store: &mut Store, s: &mut Seeded) -> Result<()> {
    let pr = snapshot(
        ("hamster-wheel", 7),
        "ringcollector",
        "Replace the hamster wheel with a slightly faster hamster wheel",
        3,
    );
    let body = "Does the hamster get a say in this? The old wheel's squeak was load-bearing.";
    let pr = PrSnapshot {
        review_requested: false,
        reviews: vec![review(&pr, ME, ReviewState::Commented, 20)],
        threads: vec![thread(
            &pr,
            "wheel-1",
            ("src/wheel.rs", 31),
            &[
                (ME, 20, body),
                (
                    "ringcollector",
                    3,
                    "The hamster has been consulted and is cautiously optimistic.",
                ),
            ],
        )],
        ..pr
    };
    record(store, s, &pr, &[("wheel", 26)])?;
    let run = finished(
        store,
        &pr,
        "Faster wheel, same hamster. Fine by me once the squeak question is settled.",
        Verdict::Comment,
        vec![draft(
            ("src/wheel.rs", 31),
            Severity::Minor,
            Confidence::Medium,
            body,
            None,
        )],
    )?;
    let drafts = store.draft_rows(run)?;
    let ids: Vec<i64> = drafts.iter().map(|d| d.id).collect();
    store.mark_posted(&ids)?;
    // Posted as your comment that opens the thread.
    let comment = drafts
        .iter()
        .find(|d| d.kind == "comment")
        .ok_or_else(|| eyre!("{} has no comment draft", pr.key.url()))?;
    store.record_posted_comments(&[(comment.id, "wheel-1-c0".into())])?;
    s.pins.run(run, 22.0, Some(21.9), Some(21.5));
    s.pins.posted(run, 20.0);
    s.pins.view(store, &pr.key, 19.0)?;
    Ok(())
}

/// Drafts you've decided on and not yet posted.
fn to_post(store: &mut Store, s: &mut Seeded) -> Result<()> {
    let pr = snapshot(
        ("git-instafix", 88),
        "chilidog-enjoyer",
        "Rename do_the_thing to do_the_other_thing",
        5,
    );
    record(store, s, &pr, &[("rename", 6)])?;
    let run = finished(
        store,
        &pr,
        "A clean rename. Two callers were missed, which is how the other thing gets done twice.",
        Verdict::RequestChanges,
        vec![
            draft(
                ("src/fixup.rs", 140),
                Severity::Major,
                Confidence::High,
                "This still calls `do_the_thing`, which now does the other thing.",
                None,
            ),
            draft(
                ("src/main.rs", 12),
                Severity::Major,
                Confidence::High,
                "Same here: the old name survived the rename.",
                None,
            ),
            draft(
                ("src/fixup.rs", 3),
                Severity::Nit,
                Confidence::Low,
                "Consider `do_the_thing_differently`.",
                None,
            ),
        ],
    )?;
    // The summary and both callers are accepted, the nit rejected.
    for draft in store.draft_rows(run)? {
        let status = if is_nit(&draft) {
            DraftStatus::Rejected
        } else {
            DraftStatus::Accepted
        };
        decide(store, &draft, status)?;
    }
    s.pins.run(run, 5.5, Some(5.4), Some(5.0));
    s.pins.view(store, &pr.key, 4.0)?;
    Ok(())
}

/// A review that failed.
fn failed(store: &mut Store, s: &mut Seeded) -> Result<()> {
    let pr = snapshot(
        ("s3glob", 131),
        "sleepy-semicolon",
        "Glob the globs that glob the buckets",
        1,
    );
    record(store, s, &pr, &[("globs", 1)])?;
    let run = queue(store, &pr)?;
    claim(store, run)?;
    store.fail_run(
        run,
        "claude exited with status 1\nError: the globs went all the way down",
    )?;
    s.pins.run(run, 0.9, Some(0.85), Some(0.6));
    Ok(())
}

/// A review you posted and approved, with nothing left to do.
fn done(store: &mut Store, s: &mut Seeded) -> Result<()> {
    let pr = snapshot(
        ("vcs-status-daemon", 57),
        "ringcollector",
        "Stop the daemon from daydreaming between polls",
        40,
    );
    let pr = PrSnapshot {
        review_requested: false,
        review_decision: Some("APPROVED".into()),
        reviews: vec![
            review(&pr, "gotta-go-fast", ReviewState::Approved, 50),
            review(&pr, ME, ReviewState::Approved, 44),
        ],
        ..pr
    };
    record(store, s, &pr, &[("daydream", 60)])?;
    let run = finished(
        store,
        &pr,
        "Polls on time now. Ship it.",
        Verdict::Comment,
        vec![],
    )?;
    let ids: Vec<i64> = store.draft_rows(run)?.iter().map(|d| d.id).collect();
    store.mark_posted(&ids)?;
    s.pins.run(run, 48.0, Some(47.9), Some(47.5));
    s.pins.posted(run, 44.0);
    s.pins.view(store, &pr.key, 44.0)?;
    Ok(())
}

const SHOWCASE_BODY: &str = "\
The frobnicator used to stop at three, which was fine until someone owned four \
frobbable things.

- widens the count to `u32`
- skips things that are already frobbed
- teaches `announce` some new numbers

```rust
assert_eq!(count(&[a, b, c, d]), 4);
```

Fixes the \"many\" bug, where four and four billion were the same number.";

/// The showcase run's diff, as `git diff` writes it.
const SHOWCASE_DIFF: &str = r#"diff --git a/src/count.rs b/src/count.rs
index 87dc27c..b02beec 100644
--- a/src/count.rs
+++ b/src/count.rs
@@ -2,27 +2,29 @@

 use crate::Thing;

-/// The biggest number anyone needs.
-pub const MAX: u8 = 3;
+/// The biggest number anyone needs, for now.
+pub const MAX: u32 = u32::MAX;

-/// Counts `things`, giving up politely after [`MAX`].
-pub fn count(things: &[Thing]) -> u8 {
-    let mut n = 0;
-    for _ in things {
-        if n == MAX {
-            return MAX;
+/// Counts `things`, skipping the ones already frobbed.
+pub fn count(things: &[Thing]) -> u32 {
+    let mut n: u32 = 0;
+    for thing in things {
+        if thing.is_frobbed() {
+            continue;
         }
-        n += 1;
+        n = n + 1;
     }
-    n
+    return n;
 }

 /// Says the count out loud, as the frobnicator likes to.
-pub fn announce(n: u8) -> String {
+pub fn announce(n: u32) -> String {
     match n {
         0 => "none".into(),
         1 => "one".into(),
         2 => "two".into(),
-        _ => "many".into(),
+        3 => "three".into(),
+        4 => "four, which used to be many".into(),
+        _ => format!("{n}, give or take"),
     }
 }
diff --git a/tests/count.rs b/tests/count.rs
index 064ebd7..0f9d527 100644
--- a/tests/count.rs
+++ b/tests/count.rs
@@ -5,3 +5,9 @@ fn counts_a_few() {
     let things = vec![Thing::new(); 2];
     assert_eq!(count(&things), 2);
 }
+
+#[test]
+fn counts_past_three() {
+    let things = vec![Thing::new(); 4];
+    assert_eq!(count(&things), 4);
+}
"#;

/// The showcase's files, as `(path, base, head)`, for expanding context.
const SHOWCASE_FILES: [(&str, &str, &str); 2] = [
    (
        "src/count.rs",
        r#"//! How the frobnicator counts things.

use crate::Thing;

/// The biggest number anyone needs.
pub const MAX: u8 = 3;

/// Counts `things`, giving up politely after [`MAX`].
pub fn count(things: &[Thing]) -> u8 {
    let mut n = 0;
    for _ in things {
        if n == MAX {
            return MAX;
        }
        n += 1;
    }
    n
}

/// Says the count out loud, as the frobnicator likes to.
pub fn announce(n: u8) -> String {
    match n {
        0 => "none".into(),
        1 => "one".into(),
        2 => "two".into(),
        _ => "many".into(),
    }
}
"#,
        r#"//! How the frobnicator counts things.

use crate::Thing;

/// The biggest number anyone needs, for now.
pub const MAX: u32 = u32::MAX;

/// Counts `things`, skipping the ones already frobbed.
pub fn count(things: &[Thing]) -> u32 {
    let mut n: u32 = 0;
    for thing in things {
        if thing.is_frobbed() {
            continue;
        }
        n = n + 1;
    }
    return n;
}

/// Says the count out loud, as the frobnicator likes to.
pub fn announce(n: u32) -> String {
    match n {
        0 => "none".into(),
        1 => "one".into(),
        2 => "two".into(),
        3 => "three".into(),
        4 => "four, which used to be many".into(),
        _ => format!("{n}, give or take"),
    }
}
"#,
    ),
    (
        "tests/count.rs",
        r"use frobnicator::{Thing, count::count};

#[test]
fn counts_a_few() {
    let things = vec![Thing::new(); 2];
    assert_eq!(count(&things), 2);
}
",
        r"use frobnicator::{Thing, count::count};

#[test]
fn counts_a_few() {
    let things = vec![Thing::new(); 2];
    assert_eq!(count(&things), 2);
}

#[test]
fn counts_past_three() {
    let things = vec![Thing::new(); 4];
    assert_eq!(count(&things), 4);
}
",
    ),
];

/// `hours` before [`NOW`], as GitHub writes times.
fn ago(hours: f64) -> String {
    // Whole seconds are plenty, and the demo's hours are small and positive.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let secs = (hours * 3600.0) as u64;
    rfc3339(UNIX_EPOCH + Duration::from_secs(NOW - secs))
}

/// The demo clock's time.
pub fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW)
}

/// A commit hash that looks like one, the same for the same `label`.
fn sha(label: &str) -> String {
    let digest = Sha256::digest(label.as_bytes());
    let mut hex = String::new();
    for byte in &digest[..20] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// `quodlibetor/<name>#<number>` by `author`, requesting your review,
/// updated on GitHub `hours` ago.
fn snapshot((name, number): (&str, u32), author: &str, title: &str, hours: u32) -> PrSnapshot {
    let key = PrKey {
        repo: RepoName::new(ME, name),
        number,
    };
    PrSnapshot {
        url: key.url(),
        title: title.into(),
        body: format!("{title}. It's all in the diff."),
        author: author.into(),
        head_sha: sha(&format!("{name}#{number}")),
        base_sha: sha(&format!("{name}-main")),
        is_draft: false,
        review_requested: true,
        requested_teams: vec![],
        reviews: vec![],
        threads: vec![],
        files: None,
        updated_at: Some(ago(f64::from(hours))),
        review_decision: None,
        merge_state: None,
        checks: None,
        in_progress: None,
        key,
    }
}

/// One of your own PRs.
fn mine(repo: (&str, u32), title: &str, hours: u32) -> PrSnapshot {
    PrSnapshot {
        review_requested: false,
        ..snapshot(repo, ME, title, hours)
    }
}

/// `author`'s review of `pr`'s head, `hours` ago.
fn review(pr: &PrSnapshot, author: &str, state: ReviewState, hours: u32) -> Review {
    Review {
        // Unique across PRs, as GitHub's node ids are: the store keys
        // reviews by id alone.
        id: format!("{}-{author}", pr.url),
        author: author.into(),
        state,
        body: String::new(),
        submitted_at: ago(f64::from(hours)),
        commit: Some(pr.head_sha.clone()),
        by_bot: false,
    }
}

/// An open thread on `line` of `path` at `pr`'s head, of `(author, hours
/// ago, body)` comments, whose ids are `<id>-c<i>`.
fn thread(
    pr: &PrSnapshot,
    id: &str,
    (path, line): (&str, u32),
    comments: &[(&str, u32, &str)],
) -> Thread {
    Thread {
        id: id.into(),
        path: Some(path.into()),
        line: Some(line),
        resolved: false,
        place: Placement {
            start_line: None,
            side: Some(Side::Right),
            head: Some(pr.head_sha.clone()),
            outdated: false,
            original_start_line: None,
            original_line: Some(line),
            original_commit: Some(pr.head_sha.clone()),
        },
        comments: comments
            .iter()
            .enumerate()
            .map(|(i, (author, hours, body))| Comment {
                id: format!("{id}-c{i}"),
                author: (*author).into(),
                body: (*body).into(),
                created_at: ago(f64::from(*hours)),
                url: None,
                by_bot: false,
                reacted_at: None,
                reactions: vec![],
            })
            .collect(),
    }
}

fn draft(
    (path, line): (&str, u32),
    severity: Severity,
    confidence: Confidence,
    body: &str,
    note: Option<&str>,
) -> DraftComment {
    DraftComment {
        comment: InlineComment {
            path: path.into(),
            line,
            start_line: None,
            side: Side::Right,
            body: body.into(),
            severity,
            confidence,
            note: note.map(Into::into),
        },
        unanchored: false,
    }
}

/// Records `pr` once per `(label, hours ago)` head, oldest first: the last
/// is its head.
fn record(store: &mut Store, s: &mut Seeded, pr: &PrSnapshot, heads: &[(&str, u32)]) -> Result<()> {
    for (i, (label, hours)) in heads.iter().enumerate() {
        // The last is the PR's own head, which its reviews and threads name.
        let head = if i + 1 == heads.len() {
            pr.head_sha.clone()
        } else {
            sha(label)
        };
        let snap = PrSnapshot {
            head_sha: head.clone(),
            ..pr.clone()
        };
        store.record(&snap, ME, "default", &[])?;
        s.pins.head(&pr.key, &head, *hours);
    }
    Ok(())
}

/// Queues a review of `pr`'s head.
fn queue(store: &mut Store, pr: &PrSnapshot) -> Result<i64> {
    let request = ReviewRequest {
        key: pr.key.clone(),
        profile: "default".into(),
        head_sha: pr.head_sha.clone(),
        base_sha: pr.base_sha.clone(),
        trigger: ReviewTrigger::Requested,
    };
    Ok(store
        .queue_review(&request)?
        .ok_or_else(|| eyre!("{} was already reviewed", pr.key.url()))?
        .id)
}

/// Starts queued `run`.
fn claim(store: &Store, run: i64) -> Result<()> {
    ensure!(store.claim_run(run)?, "run {run} wasn't queued");
    Ok(())
}

/// Decides on `draft`.
fn decide(store: &Store, draft: &DraftRow, status: DraftStatus) -> Result<()> {
    ensure!(
        store.set_draft_status(draft.id, status)?,
        "draft {} can't be decided on",
        draft.id
    );
    Ok(())
}

fn is_nit(draft: &DraftRow) -> bool {
    draft.severity.as_deref() == Some(Severity::Nit.as_str())
}

/// A review of `pr` that finished with `summary` and `comments`.
fn finished(
    store: &mut Store,
    pr: &PrSnapshot,
    summary: &str,
    verdict: Verdict,
    comments: Vec<DraftComment>,
) -> Result<i64> {
    let run = queue(store, pr)?;
    claim(store, run)?;
    store.finish_review(
        run,
        &ReviewResult {
            summary: summary.into(),
            summary_note: None,
            verdict,
            comments,
            session_id: Some(format!("demo-{}", pr.key.number)),
            transcript_path: "transcript.jsonl".into(),
        },
    )?;
    Ok(run)
}

/// Times the store wrote as the real now, and when the demo says they
/// were.
#[derive(Default)]
struct Pins {
    /// `(run, queued, started, finished)`.
    runs: Vec<(i64, String, Option<String>, Option<String>)>,
    /// When each run's posted drafts were posted.
    posted: Vec<(i64, String)>,
    /// When each head was first seen.
    heads: Vec<(PrKey, String, String)>,
    /// When you last opened each PR's page.
    views: Vec<(PrKey, String)>,
}

impl Pins {
    fn run(&mut self, run: i64, queued: f64, started: Option<f64>, finished: Option<f64>) {
        self.runs
            .push((run, ago(queued), started.map(ago), finished.map(ago)));
    }

    fn posted(&mut self, run: i64, hours: f64) {
        self.posted.push((run, ago(hours)));
    }

    fn head(&mut self, key: &PrKey, head: &str, hours: u32) {
        self.heads
            .push((key.clone(), head.into(), ago(f64::from(hours))));
    }

    /// Records a view of `key`'s page, as opening it does, `hours` ago.
    fn view(&mut self, store: &Store, key: &PrKey, hours: f64) -> Result<()> {
        store.record_view(key)?;
        self.views.push((key.clone(), ago(hours)));
        Ok(())
    }
}

/// Rewrites the times the store wrote as the real now to the demo's, so
/// the screenshots come out the same each time. The store has no API for
/// backdating, so this is SQL on the closed store's file.
pub fn pin_times(db: &Path, seeded: &Seeded) -> Result<()> {
    let conn = rusqlite::Connection::open(db).wrap_err("opening the demo store to pin times")?;
    let pins = &seeded.pins;
    for (run, queued, started, finished) in &pins.runs {
        let changed = conn.execute(
            "UPDATE runs SET queued_at = ?2, started_at = ?3, finished_at = ?4 WHERE id = ?1",
            rusqlite::params![run, queued, started, finished],
        )?;
        pinned(changed, &format!("run {run}"))?;
        let made = finished.as_ref().unwrap_or(queued);
        conn.execute(
            "UPDATE drafts SET created_at = ?2, updated_at = ?2 WHERE run_id = ?1",
            rusqlite::params![run, made],
        )?;
    }
    for (run, at) in &pins.posted {
        conn.execute(
            "UPDATE drafts SET updated_at = ?2 WHERE run_id = ?1 AND status = 'posted'",
            rusqlite::params![run, at],
        )?;
    }
    for (key, head, at) in &pins.heads {
        let changed = conn.execute(
            "UPDATE revisions SET seen_at = ?4 WHERE repo = ?1 AND number = ?2 AND head_sha = ?3",
            rusqlite::params![key.repo.to_string(), key.number, head, at],
        )?;
        pinned(changed, &format!("{}'s head {head}", key.url()))?;
    }
    for (key, at) in &pins.views {
        let changed = conn.execute(
            "UPDATE views SET viewed_at = ?3 WHERE repo = ?1 AND number = ?2",
            rusqlite::params![key.repo.to_string(), key.number, at],
        )?;
        pinned(changed, &format!("the view of {}", key.url()))?;
    }
    Ok(())
}

/// Fails unless pinning `what` changed its one row: a pin that matched
/// nothing would leave the real now in place.
fn pinned(changed: usize, what: &str) -> Result<()> {
    ensure!(changed == 1, "pinning {what} changed {changed} rows");
    Ok(())
}
