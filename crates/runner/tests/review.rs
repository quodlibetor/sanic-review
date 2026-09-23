//! Review runs end to end, with a fake `claude` and a local git remote.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use common::{key, remote};
use sanic_core::{
    config::RunnerSettings,
    run::{PrContext, QueuedRun, ReviewRequest, ReviewTrigger, Verdict},
};
use sanic_runner::review::{AgentProfile, ReviewRunner, RunSettings};
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
        RunSettings::new(profile, &self.settings, &self.git_url)
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
        url: "https://github.com/org/repo/pull/7".into(),
        author: "alice".into(),
        threads: vec![],
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
        .review(&s.run, &context(), &s.settings(profile(vec![instructions])))
        .await
        .unwrap();
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
    assert!(read(fake, "stdin").contains("Tweak lib"));
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
async fn agent_failures_fail_the_run_and_clean_up() {
    let output = transcript(&json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true
    }));
    let s = setup("", &output, Duration::from_secs(30));
    let err = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])))
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
        .review(&s.run, &context(), &s.settings(profile(vec![])))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("review schema"), "{err:?}");
}

#[tokio::test]
async fn slow_agents_are_killed() {
    let s = setup("sleep 30", "", Duration::from_millis(300));
    let err = s
        .runner
        .review(&s.run, &context(), &s.settings(profile(vec![])))
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
        title: "x".repeat(1 << 20),
        ..context()
    };
    let err = s.runner.review(&s.run, &ctx, &settings).await.unwrap_err();
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
        )
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("reading instructions"),
        "{err:?}"
    );
}
