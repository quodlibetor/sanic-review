//! `serve` from review request to stored drafts, against a mock GitHub, a
//! local git repo standing in for github.com, and a fake `claude`.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    ffi::OsStr,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use sanic_store::Store;
use serde_json::json;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Creates `<root>/org/repo.git` with PR 7, returning `(head, base)`.
fn git_remote(root: &Path) -> (String, String) {
    let github_repo = root.join("org/repo.git");
    std::fs::create_dir_all(&github_repo).unwrap();
    git(&github_repo, &["init", "-q", "--bare"]);
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("lib.rs"), "one\ntwo\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "base"]);
    let base = git(&work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("lib.rs"), "one\n2\n").unwrap();
    git(&work, &["commit", "-q", "-am", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    let target = github_repo.to_string_lossy();
    git(
        &work,
        &[
            "push",
            "-q",
            &target,
            &format!("{base}:refs/heads/main"),
            "HEAD:refs/pull/7/head",
        ],
    );
    (head, base)
}

/// A `claude` stand-in that saves its environment and answers with one
/// anchored and one unanchored comment.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let result = json!({
        "type": "result", "subtype": "success", "is_error": false, "session_id": "sess-9",
        "structured_output": {
            "summary": "Renames a line.\nMore detail.",
            "suggested_verdict": "comment",
            "comments": [
                { "path": "lib.rs", "line": 2, "side": "RIGHT", "body": "why?",
                  "severity": "nit", "confidence": "high" },
                { "path": "lib.rs", "line": 99, "side": "RIGHT", "body": "far",
                  "severity": "minor", "confidence": "low" }
            ]
        }
    });
    std::fs::write(dir.join("output.jsonl"), format!("{result}\n")).unwrap();
    let script = dir.join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nd='{}'\nenv > \"$d/env\"\ncat > /dev/null\ncat \"$d/output.jsonl\"\n",
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn graphql(body: serde_json::Value, response: serde_json::Value) -> Mock {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(body))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
}

fn search(q: &str, nodes: &serde_json::Value) -> Mock {
    graphql(
        json!({ "variables": { "q": format!("is:open is:pr {q}") } }),
        json!({ "data": { "search": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": nodes
        }}}),
    )
}

async fn mock_github(head: &str, base: &str) -> MockServer {
    let server = MockServer::start().await;
    graphql(
        json!({ "query": "query { viewer { login } }" }),
        json!({ "data": { "viewer": { "login": "me" } } }),
    )
    .mount(&server)
    .await;
    search(
        "review-requested:@me",
        &json!([{ "number": 7, "repository": { "nameWithOwner": "org/repo" } }]),
    )
    .mount(&server)
    .await;
    search("involves:@me", &json!([])).mount(&server).await;
    graphql(
        json!({ "variables": { "owner": "org", "name": "repo", "number": 7 } }),
        json!({ "data": { "repository": { "pullRequest": {
            "number": 7, "title": "Rename a line", "url": "https://github.com/org/repo/pull/7",
            "isDraft": false, "headRefOid": head, "baseRefOid": base,
            "author": { "login": "alice" },
            "reviewRequests": { "nodes": [
                { "requestedReviewer": { "__typename": "User", "login": "me" } }
            ] },
            "reviews": { "nodes": [] },
            "comments": { "nodes": [] },
            "reviewThreads": { "nodes": [] }
        }}}}),
    )
    .mount(&server)
    .await;
    Mock::given(method("GET"))
        .and(path("/user/teams"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    server
}

/// A mock GitHub, a local "github.com" and a fake `claude`, in one tempdir.
struct World {
    dir: TempDir,
    _server: MockServer,
    config: std::path::PathBuf,
    fake: std::path::PathBuf,
    data: std::path::PathBuf,
}

async fn world() -> World {
    let dir = TempDir::new().unwrap();
    let remote = dir.path().join("github");
    let (head, base) = git_remote(&remote);
    let server = mock_github(&head, &base).await;
    let fake = dir.path().join("fake");
    std::fs::create_dir(&fake).unwrap();
    let claude = fake_claude(&fake);
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[github]\napi_url = \"{}\"\ngit_url = \"{}\"\n\n\
             [poll]\nquiet_secs = 0\n\n\
             [runner]\nclaude = \"{}\"\n\n\
             [profile.p]\nrepos = [{{ github = \"org\" }}]\n",
            server.uri(),
            remote.display(),
            claude.display()
        ),
    )
    .unwrap();
    let data = dir.path().join("data");
    World {
        dir,
        _server: server,
        config,
        fake,
        data,
    }
}

impl World {
    fn set_quiet_secs(&self, secs: u64) {
        let text = std::fs::read_to_string(&self.config).unwrap();
        let text = replace_number(&text, "quiet_secs = ", secs);
        std::fs::write(&self.config, text).unwrap();
    }

    /// Runs `serve` until it prints a line containing `needle`, then kills
    /// it and returns that line.
    async fn serve_until(&self, needle: &'static str) -> String {
        self.serve_with_until(&[], needle).await
    }

    async fn serve_with_until(&self, extra: &[&str], needle: &'static str) -> String {
        self.serve_with_data_dir_until(extra, self.data.as_os_str(), needle)
            .await
    }

    async fn serve_with_data_dir_until(
        &self,
        extra: &[&str],
        data_dir: &OsStr,
        needle: &'static str,
    ) -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sanic-review"))
            .arg("serve")
            .args(extra)
            .arg("--config")
            .arg(&self.config)
            .arg("--data-dir")
            .arg(data_dir)
            .env("GITHUB_TOKEN", "t0ken")
            .env("GH_TOKEN", "t0ken")
            // The scheduler says at debug level when it skips a reviewed head.
            .env("RUST_LOG", "info,sanic_review::schedule=debug")
            .env("NO_COLOR", "1")
            .current_dir(self.dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let found = tokio::task::spawn_blocking(move || {
            let mut seen = Vec::new();
            while let Ok(line) = rx.recv_timeout(Duration::from_secs(30)) {
                if line.contains(needle) {
                    return Ok(line);
                }
                seen.push(line);
            }
            Err(seen)
        })
        .await
        .unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        found.unwrap_or_else(|seen| panic!("no `{needle}` line; output:\n{}", seen.join("\n")))
    }
}

/// Replaces the number after `key` on its line with `value`.
fn replace_number(text: &str, key: &str, value: u64) -> String {
    text.lines()
        .map(|line| match line.strip_prefix(key) {
            Some(_) => format!("{key}{value}"),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_drafts_a_requested_review() {
    let w = world().await;
    let line = w.serve_until("drafted").await;
    assert!(line.contains("org/repo#7"), "{line}");
    assert!(line.contains("Renames a line."), "{line}");
    assert!(line.contains("unanchored=1"), "{line}");

    let env = std::fs::read_to_string(w.fake.join("env")).unwrap();
    assert!(
        !env.contains("t0ken"),
        "the agent saw a GitHub token:\n{env}"
    );

    let store = Store::open(&w.data.join("state.db")).unwrap();
    assert_eq!(store.run_counts().unwrap().pending_drafts, 3);
    let drafts = store.drafts(1).unwrap();
    assert_eq!(drafts[0].kind, "summary");
    assert!(!drafts[1].unanchored);
    assert!(drafts[2].unanchored);
    assert!(!w.data.join("worktrees/1").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_debounced_across_a_restart_is_still_reviewed() {
    let w = world().await;
    // The first process sees the request but stops before the review is due.
    w.set_quiet_secs(3600);
    w.serve_until("review requested at").await;
    let store = Store::open(&w.data.join("state.db")).unwrap();
    assert_eq!(store.run_counts().unwrap().queued, 0);

    // After a restart the request is standing, not new, and still reviewed.
    w.set_quiet_secs(0);
    w.serve_until("drafted").await;

    // Once reviewed, another restart doesn't review the same head again.
    w.serve_until("already reviewed").await;
    assert_eq!(store.run_counts().unwrap().pending_drafts, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_reviews_queues_without_running_until_a_normal_start() {
    let w = world().await;
    let line = w
        .serve_with_until(&["--no-reviews"], "review queued, not run")
        .await;
    assert!(line.contains("org/repo#7"), "{line}");
    assert!(
        !w.fake.join("env").exists(),
        "claude ran despite --no-reviews"
    );
    let store = Store::open(&w.data.join("state.db")).unwrap();
    assert_eq!(store.run_counts().unwrap().queued, 1);

    // Held runs are still queued, so a normal start runs them.
    w.serve_until("drafted").await;
    assert!(w.fake.join("env").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relative_data_dir_still_reviews() {
    let w = world().await;
    // serve runs in the world's directory, so `data` is the same place as
    // `w.data`, but git and claude run elsewhere.
    let line = w
        .serve_with_data_dir_until(&[], OsStr::new("data"), "drafted")
        .await;
    assert!(line.contains("org/repo#7"), "{line}");
    let store = Store::open(&w.data.join("state.db")).unwrap();
    assert_eq!(store.run_counts().unwrap().pending_drafts, 3);
    assert!(!w.data.join("worktrees/1").exists());
}
