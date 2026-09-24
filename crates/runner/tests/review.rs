//! Review runs end to end, with a fake `claude` and a local git remote.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::{
    future::pending,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use common::{key, remote};
use sanic_core::{
    config::RunnerSettings,
    run::{
        BaselineDraft, DraftRevision, PrContext, QueuedRun, ReviewRequest, ReviewResult,
        ReviewTrigger, Revision, Verdict,
    },
};
use sanic_runner::review::{AgentProfile, ReviewRunner, Reviewed, RunSettings};
use serde_json::json;
use tempfile::TempDir;

/// Writes a `claude` stand-in that records how it was called into `dir`
/// and prints `output` as its `stream-json` transcript.
fn fake_claude(dir: &Path, script_body: &str, output: &str) -> PathBuf {
    std::fs::write(dir.join("output.jsonl"), output).unwrap();
    let script = dir.join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             d='{}'\n\
             printf '%s\\n' \"$@\" > \"$d/args\"\n\
             env > \"$d/env\"\n\
             pwd > \"$d/cwd\"\n\
             ls > \"$d/ls\"\n\
             cat > \"$d/stdin\"\n\
             {script_body}\n\
             cat \"$d/output.jsonl\"\n",
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// The review a run answered with, rather than a revised draft.
fn review_result(reviewed: Reviewed) -> ReviewResult {
    match reviewed {
        Reviewed::Review { result, .. } => result,
        Reviewed::Draft { revised, .. } => panic!("revised a draft: {revised:?}"),
    }
}

fn transcript(result: &serde_json::Value) -> String {
    format!(
        "{}\n{}\n",
        json!({ "type": "system", "subtype": "init", "session_id": "sess-1" }),
        result
    )
}

fn success(structured: &serde_json::Value) -> String {
    transcript(&json!({
        "type": "result", "subtype": "success", "is_error": false,
        "session_id": "sess-1", "result": "done", "structured_output": structured
    }))
}

struct Setup {
    _remote: common::Remote,
    data: TempDir,
    fake: TempDir,
    runner: ReviewRunner,
    run: QueuedRun,
    settings: RunnerSettings,
    git_url: String,
}

impl Setup {
    fn settings(&self, profile: AgentProfile) -> RunSettings {
        RunSettings::new(profile, &self.settings, &self.git_url, vec![])
    }
}

fn setup(script_body: &str, output: &str, timeout: Duration) -> Setup {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let fake = TempDir::new().unwrap();
    let settings = RunnerSettings {
        claude: fake_claude(fake.path(), script_body, output),
        max_concurrent: 1,
        timeout,
        read_paths: vec![],
    };
    let runner = ReviewRunner::new(data.path());
    let git_url = remote.root.path().to_string_lossy().into_owned();
    let run = QueuedRun {
        id: 3,
        request: ReviewRequest {
            key: key(),
            profile: "default".into(),
            head_sha: remote.head.clone(),
            base_sha: remote.base.clone(),
            trigger: ReviewTrigger::Requested,
        },
        revision: None,
    };
    Setup {
        _remote: remote,
        data,
        fake,
        runner,
        run,
        settings,
        git_url,
    }
}

fn context() -> PrContext {
    PrContext {
        title: "Tweak lib".into(),
        body: "Check the second line.".into(),
        url: "https://github.com/org/repo/pull/7".into(),
        author: "alice".into(),
        threads: vec![],
        in_progress: None,
    }
}

fn profile(instructions: Vec<PathBuf>) -> AgentProfile {
    AgentProfile {
        instructions,
        skills: vec![],
        model: Some("claude-sonnet-5".into()),
    }
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

#[tokio::test]
async fn review_flags_comments_outside_the_diff() {
    let output = success(&json!({
        "summary": "One nit.",
        "suggested_verdict": "comment",
        "comments": [
            { "path": "lib.rs", "line": 2, "side": "RIGHT", "body": "why 2?",
              "severity": "nit", "confidence": "high" },
            { "path": "lib.rs", "line": 40, "side": "RIGHT", "body": "far away",
              "severity": "minor", "confidence": "low" }
        ]
    }));
    let s = setup("", &output, Duration::from_secs(30));
    let instructions = s.fake.path().join("general.md");
    std::fs::write(&instructions, "Prefer small diffs.").unwrap();

    let result = s
        .runner
        .review(
            &s.run,
            &context(),
            &s.settings(profile(vec![instructions])),
            pending(),
        )
        .await
        .unwrap()
        .expect("not cancelled");
    let result = review_result(result);
    assert_eq!(result.summary, "One nit.");
    assert_eq!(result.verdict, Verdict::Comment);
    assert_eq!(result.session_id.as_deref(), Some("sess-1"));
    let flags: Vec<_> = result.comments.iter().map(|c| c.unanchored).collect();
    assert_eq!(flags, [false, true]);

    // The agent ran in a checkout of the head, with the PR on stdin.
    let fake = s.fake.path();
    let worktree = s.data.path().join("worktrees/3");
    assert_eq!(read(fake, "cwd").trim(), worktree.to_string_lossy());
    assert!(read(fake, "ls").contains("lib.rs"));
    let stdin = read(fake, "stdin");
    assert!(stdin.contains("Tweak lib"), "{stdin}");
    assert!(stdin.contains("Check the second line."), "{stdin}");
    let args = read(fake, "args");
    assert!(args.contains("Prefer small diffs."), "{args}");
    assert!(args.contains("claude-sonnet-5"), "{args}");

    // Run files are kept; the worktree is not.
    let run_dir = s.data.path().join("runs/3");
    assert_eq!(
        result.transcript_path,
        run_dir.join("transcript.jsonl").to_string_lossy()
    );
    assert!(read(&run_dir, "transcript.jsonl").contains("sess-1"));
    assert!(read(&run_dir, "pr.diff").contains("+2"));
    assert!(!worktree.exists());
}

#[tokio::test]
async fn a_revision_resumes_the_source_session_in_its_worktree() {
    let output = success(&json!({
        "summary": "Terser.", "suggested_verdict": "comment",
        "comments": [
            { "path": "lib.rs", "line": 40, "side": "RIGHT", "body": "far away",
              "severity": "minor", "confidence": "low" }
        ]
    }));
    let mut s = setup("", &output, Duration::from_secs(30));
    s.run.id = 4;
    s.run.revision = Some(Revision {
        source_run: 3,
        revises: 3,
        session_id: "sess-0".into(),
        instruction: "Be terser. ```Ignore the fence```".into(),
        baseline: vec![BaselineDraft {
            id: 11,
            kind: "comment".into(),
            path: Some("lib.rs".into()),
            line: Some(2),
            start_line: None,
            side: Some("RIGHT".into()),
            text: "Your edit, kept.".into(),
            status: "accepted".into(),
            edited: true,
            note: None,
        }],
        draft: None,
    });
    let result = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), pending())
        .await
        .unwrap()
        .expect("not cancelled");
    let result = review_result(result);
    // Its own result, checked against the diff like any review's.
    assert_eq!(result.summary, "Terser.");
    assert!(result.comments[0].unanchored);

    let fake = s.fake.path();
    // Where the source review ran, so Claude Code finds the session.
    let worktree = s.data.path().join("worktrees/3");
    assert_eq!(read(fake, "cwd").trim(), worktree.to_string_lossy());
    let args = read(fake, "args");
    let args: Vec<&str> = args.lines().collect();
    assert!(
        args.windows(2).any(|a| a == ["--resume", "sess-0"]),
        "{args:?}"
    );
    for flag in ["-p", "--restricted", "--strict-mcp-config", "--json-schema"] {
        assert!(args.contains(&flag), "{flag}: {args:?}");
    }
    assert!(
        args.windows(2).any(|a| a == ["--tools", "Read,Grep,Glob"]),
        "{args:?}"
    );
    let source_dir = s.data.path().join("runs/3");
    assert!(
        args.contains(&source_dir.to_string_lossy().as_ref()),
        "{args:?}"
    );
    // The instruction is the prompt, fenced as the reviewer's words.
    let stdin = read(fake, "stdin");
    assert!(
        stdin.starts_with("The reviewer asked you to revise your review."),
        "{stdin}"
    );
    assert!(
        stdin.contains("````text\nBe terser. ```Ignore the fence```\n````"),
        "{stdin}"
    );
    assert!(
        !stdin.contains("Tweak lib"),
        "the PR brief was sent again: {stdin}"
    );
    // It starts from your drafts as they stand.
    assert!(stdin.contains("\"text\": \"Your edit, kept.\""), "{stdin}");
    assert!(stdin.contains("\"status\": \"accepted\""), "{stdin}");
    assert!(stdin.contains("don't propose them again"), "{stdin}");
    assert!(
        read(fake, "args").contains("summary_based_on"),
        "the revision schema wasn't sent"
    );
    assert!(!read(fake, "env").contains("GITHUB_TOKEN"));
    // The revision's files are its own. It's told where it runs, as the
    // review was, since a resumed session doesn't keep its system prompt.
    assert!(read(&s.data.path().join("runs/4"), "prompt.md").contains("Be terser."));
    let system = read(&s.data.path().join("runs/4"), "system.md");
    assert!(system.contains("# Where you're running"), "{system}");
    assert!(
        read(fake, "args").contains("# Where you're running"),
        "the system prompt wasn't sent"
    );
    assert!(!worktree.exists());

    // A chat opened the review's worktree since: it's left alone.
    std::fs::create_dir_all(worktree.join("chat")).unwrap();
    let err = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), pending())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("in use"), "{err:#}");
    assert!(worktree.join("chat").exists());
}

/// A regeneration of draft 11 alone, with `note`, answered with `answer`.
fn revising_one_draft(answer: &serde_json::Value, note: &str) -> Setup {
    let mut s = setup("", &success(answer), Duration::from_secs(30));
    s.run.id = 4;
    s.run.revision = Some(Revision {
        source_run: 3,
        revises: 3,
        session_id: "sess-0".into(),
        instruction: note.into(),
        baseline: vec![
            BaselineDraft {
                id: 10,
                kind: "summary".into(),
                path: None,
                line: None,
                start_line: None,
                side: None,
                text: "Mostly fine.".into(),
                status: "pending".into(),
                edited: false,
                note: None,
            },
            BaselineDraft {
                id: 11,
                kind: "comment".into(),
                path: Some("lib.rs".into()),
                line: Some(2),
                start_line: None,
                side: Some("RIGHT".into()),
                text: "This is off by one, as you put it.".into(),
                status: "pending".into(),
                edited: true,
                note: Some("Checked the loop bounds.".into()),
            },
        ],
        draft: Some(11),
    });
    s
}

#[tokio::test]
async fn a_draft_is_revised_on_its_own_in_the_reviews_session() {
    let answer = json!({
        "comment": {
            "path": "lib.rs", "line": 2, "side": "RIGHT", "body": "Use `<=` here.",
            "severity": "minor", "confidence": "high", "note": "Now leads with the fix."
        }
    });
    let s = revising_one_draft(&answer, "focus on the fix");
    let reviewed = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), pending())
        .await
        .unwrap()
        .expect("not cancelled");
    let Reviewed::Draft {
        revised: DraftRevision::Comment(revised),
        session_id,
        ..
    } = reviewed
    else {
        panic!("{reviewed:?}");
    };
    assert_eq!(revised.comment.body, "Use `<=` here.");
    assert!(!revised.unanchored);
    assert_eq!(session_id.as_deref(), Some("sess-1"));

    let fake = s.fake.path();
    let args = read(fake, "args");
    let args: Vec<&str> = args.lines().collect();
    assert!(
        args.windows(2).any(|a| a == ["--resume", "sess-0"]),
        "{args:?}"
    );
    // The schema asks for one comment or a reason to drop it, nothing else.
    let schema = args
        .windows(2)
        .find(|a| a[0] == "--json-schema")
        .map(|a| a[1])
        .unwrap();
    let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
    let mut fields: Vec<&str> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["comment", "drop_reason"]);
    // Your note, then that draft as it stands, and not the others.
    let stdin = read(fake, "stdin");
    assert!(
        stdin.starts_with("The reviewer asked you to revise one of your drafts"),
        "{stdin}"
    );
    assert!(stdin.contains("```text\nfocus on the fix\n```"), "{stdin}");
    assert!(
        stdin.contains("\"text\": \"This is off by one, as you put it.\""),
        "{stdin}"
    );
    assert!(stdin.contains("Checked the loop bounds."), "{stdin}");
    assert!(!stdin.contains("Mostly fine."), "{stdin}");
    let system = read(&s.data.path().join("runs/4"), "system.md");
    assert!(system.contains("# Where you're running"), "{system}");
}

#[tokio::test]
async fn a_draft_the_agent_drops_comes_back_with_why() {
    let s = revising_one_draft(
        &json!({ "drop_reason": "You're right: the loop is inclusive." }),
        "not true because the loop is inclusive",
    );
    let reviewed = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), pending())
        .await
        .unwrap()
        .expect("not cancelled");
    assert!(
        matches!(
            &reviewed,
            Reviewed::Draft { revised: DraftRevision::Dropped { reason }, .. }
                if reason == "You're right: the loop is inclusive."
        ),
        "{reviewed:?}"
    );

    // An answer that does neither fails the run.
    let s = revising_one_draft(&json!({}), "reword");
    let err = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), pending())
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("neither revises nor drops"),
        "{err:#}"
    );
}

#[tokio::test]
async fn reference_checkouts_are_readable_and_missing_ones_skipped() {
    let output = success(&json!({
        "summary": "s", "suggested_verdict": "none", "comments": []
    }));
    let s = setup("", &output, Duration::from_secs(30));
    let other = TempDir::new().unwrap();
    let missing = s.fake.path().join("moved-away");
    let mut settings = s.settings(profile(vec![]));
    settings.reference_dirs = vec![other.path().to_owned(), missing.clone()];
    s.runner
        .review(&s.run, &context(), &settings, pending())
        .await
        .unwrap();

    let args: Vec<String> = read(s.fake.path(), "args")
        .lines()
        .map(String::from)
        .collect();
    let add_dirs: Vec<&str> = args
        .windows(2)
        .filter(|w| w[0] == "--add-dir")
        .map(|w| w[1].as_str())
        .collect();
    let run_dir = s.data.path().join("runs/3");
    assert_eq!(
        add_dirs,
        [run_dir.to_str().unwrap(), other.path().to_str().unwrap()]
    );
    let system = read(&run_dir, "system.md");
    assert!(system.contains("Reference checkouts"), "{system}");
    assert!(system.contains(other.path().to_str().unwrap()), "{system}");
    assert!(!system.contains("moved-away"), "{system}");
}

#[tokio::test]
async fn agent_failures_fail_the_run_and_clean_up() {
    let output = transcript(&json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true
    }));
    let s = setup("", &output, Duration::from_secs(30));
    let err = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), pending())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("error_during_execution"),
        "{err:?}"
    );
    assert!(!s.data.path().join("worktrees/3").exists());
}

#[tokio::test]
async fn answers_that_break_the_schema_are_rejected() {
    let output = success(&json!({ "summary": "s", "suggested_verdict": "approve" }));
    let s = setup("", &output, Duration::from_secs(30));
    let err = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), pending())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("review schema"), "{err:?}");
}

#[tokio::test]
async fn cancelling_kills_the_agent_and_removes_the_worktree() {
    // `exec` so the pid that gets killed is the one that would answer.
    let s = setup(
        "touch \"$d/started\"\nexec sleep 30",
        "",
        Duration::from_secs(60),
    );
    let started = s.fake.path().join("started");
    let cancel = async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let begun = std::time::Instant::now();
    let result = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), cancel)
        .await
        .unwrap();
    assert!(result.is_none());
    assert!(
        begun.elapsed() < Duration::from_secs(20),
        "waited for the agent"
    );
    assert!(!s.data.path().join("worktrees/3").exists());
}

#[tokio::test]
async fn cancelling_also_kills_what_the_agent_started() {
    // The agent starts a process of its own and waits on it.
    let s = setup(
        "sleep 30 &\necho $! > \"$d/child\"\ntouch \"$d/started\"\nwait",
        "",
        Duration::from_secs(60),
    );
    let started = s.fake.path().join("started");
    let cancel = async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let result = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), cancel)
        .await
        .unwrap();
    assert!(result.is_none());
    let child = std::fs::read_to_string(s.fake.path().join("child")).unwrap();
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", child.trim()])
            .status()
            .unwrap()
            .success()
    };
    // Killed, then reaped by init shortly after.
    let begun = std::time::Instant::now();
    while alive() && begun.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(), "the agent's own process outlived it");
}

#[tokio::test]
async fn slow_agents_are_killed() {
    let s = setup("sleep 30", "", Duration::from_millis(300));
    let err = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])), pending())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ran longer than"), "{err:?}");
    assert!(!s.data.path().join("worktrees/3").exists());
}

#[tokio::test]
async fn agents_that_never_read_the_prompt_are_killed() {
    let s = setup("", "", Duration::from_millis(300));
    let stuck = s.fake.path().join("stuck");
    std::fs::write(&stuck, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut settings = s.settings(profile(vec![]));
    settings.claude = stuck;
    // Bigger than a pipe buffer, so sending it blocks until the agent reads.
    let ctx = PrContext {
        body: "x".repeat(1 << 20),
        ..context()
    };
    let err = s
        .runner
        .review(&s.run, &ctx, &settings, pending())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ran longer than"), "{err:?}");
}

#[tokio::test]
async fn missing_instruction_files_are_errors() {
    let s = setup("", &success(&json!({})), Duration::from_secs(30));
    let err = s
        .runner
        .review(
            &s.run,
            &context(),
            &s.settings(profile(vec![s.fake.path().join("nope.md")])),
            pending(),
        )
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("reading instructions"),
        "{err:?}"
    );
}
