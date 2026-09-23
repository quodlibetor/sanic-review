//! `serve` from review request to stored drafts, against a mock GitHub, a
//! local git repo standing in for github.com, and a fake `claude`.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
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

#[tokio::test(flavor = "multi_thread")]
async fn serve_drafts_a_requested_review() {
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_sanic-review"))
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .env("GITHUB_TOKEN", "t0ken")
        .env("GH_TOKEN", "t0ken")
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
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
            if line.contains("drafted") {
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

    let line =
        found.unwrap_or_else(|seen| panic!("no review drafted; output:\n{}", seen.join("\n")));
    assert!(line.contains("org/repo#7"), "{line}");
    assert!(line.contains("Renames a line."), "{line}");
    assert!(line.contains("unanchored=1"), "{line}");

    let env = std::fs::read_to_string(fake.join("env")).unwrap();
    assert!(
        !env.contains("t0ken"),
        "the agent saw a GitHub token:\n{env}"
    );

    let store = Store::open(&data.join("state.db")).unwrap();
    assert_eq!(store.run_counts().unwrap().pending_drafts, 3);
    let drafts = store.drafts(1).unwrap();
    assert_eq!(drafts[0].kind, "summary");
    assert!(!drafts[1].unanchored);
    assert!(drafts[2].unanchored);
    assert!(!data.join("worktrees/1").exists());
}
