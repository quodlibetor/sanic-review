//! End-to-end checks against the built binary.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use sanic_core::{
    pr::{PrKey, PrSnapshot},
    repo::RepoName,
};
use sanic_store::Store;

fn sanic_review() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sanic-review"))
}

#[test]
fn version_prints_package_version() {
    let output = sanic_review().arg("--version").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
}

#[test]
fn unknown_ui_is_rejected() {
    let output = sanic_review()
        .args(["serve", "--ui", "bogus"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn pr_commands_update_the_store() {
    let dir = tempfile::TempDir::new().unwrap();
    let key = PrKey {
        repo: RepoName::new("org", "repo"),
        number: 7,
    };
    let mut store = Store::open(&dir.path().join("state.db")).unwrap();
    store.record(&snapshot(&key), "me", "p", &[]).unwrap();
    let run = |command: &str, url: &str| {
        sanic_review()
            .args([command, url, "--data-dir"])
            .arg(dir.path())
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    };
    let archived = |store: &Store| store.pr_summary(&key).unwrap().unwrap().archived;

    let output = run("archive", &format!("{}/files", key.url()));
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout, "archived https://github.com/org/repo/pull/7\n");
    assert!(archived(&store));

    assert!(run("unarchive", &key.url()).status.success());
    assert!(!archived(&store));

    let output = run("review", &key.url());
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        store.take_start_requests().unwrap(),
        std::slice::from_ref(&key)
    );

    let output = run("archive", "https://github.com/org/repo/pull/8");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("isn't tracked"), "{stderr}");
}

fn snapshot(key: &PrKey) -> PrSnapshot {
    PrSnapshot {
        key: key.clone(),
        title: "t".into(),
        body: String::new(),
        url: key.url(),
        author: "alice".into(),
        head_sha: "h1".into(),
        base_sha: "b1".into(),
        is_draft: false,
        review_requested: true,
        requested_teams: vec![],
        reviews: vec![],
        threads: vec![],
        files: None,
        updated_at: None,
        review_decision: None,
        merge_state: None,
        checks: None,
        in_progress: None,
    }
}
