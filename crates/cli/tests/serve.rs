//! `sanic-review serve` end to end, against a mock GitHub.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use serde_json::json;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

fn graphql(server_body: serde_json::Value, response: serde_json::Value) -> Mock {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(server_body))
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

async fn mock_github() -> MockServer {
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
            "number": 7, "title": "Add retries", "url": "https://github.com/org/repo/pull/7",
            "isDraft": false, "headRefOid": "aaaabbbbcccc", "baseRefOid": "b",
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
async fn serve_logs_a_review_request() {
    let server = mock_github().await;
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[github]\napi_url = \"{}\"\n\n[profile.p]\nrepos = [{{ github = \"org\" }}]\n",
            server.uri()
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_sanic-review"))
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .env("GITHUB_TOKEN", "t0ken")
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
            if line.contains("review requested at aaaabbbb") {
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
        found.unwrap_or_else(|seen| panic!("no trigger logged; output:\n{}", seen.join("\n")));
    assert!(line.contains("org/repo#7"), "{line}");
    assert!(dir.path().join("data/state.db").exists());
}

#[test]
fn missing_config_explains_itself() {
    let dir = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sanic-review"))
        .arg("serve")
        .arg("--config")
        .arg(dir.path().join("nope.toml"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("reading config"), "{stderr}");
    assert!(stderr.contains("Suggestion"), "{stderr}");
}
