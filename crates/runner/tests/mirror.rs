//! Mirrors and PR worktrees, with local bare repos standing in for GitHub.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{path::Path, process::Command};

use sanic_core::{pr::PrKey, repo::RepoName};
use sanic_runner::mirror::Mirrors;
use tempfile::TempDir;

/// Runs git isolated from the user's config, returning trimmed stdout.
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

fn commit(dir: &Path, file: &str, contents: &str) -> String {
    std::fs::write(dir.join(file), contents).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", file]);
    git(dir, &["rev-parse", "HEAD"])
}

/// A fake GitHub under `root`: `org/repo.git` with PR 7 branched off an
/// older base, and the base branch moved on since.
struct Remote {
    root: TempDir,
    head: String,
    base: String,
}

fn remote() -> Remote {
    let root = TempDir::new().unwrap();
    let github_repo = root.path().join("org/repo.git");
    std::fs::create_dir_all(&github_repo).unwrap();
    git(&github_repo, &["init", "-q", "--bare"]);
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    commit(&work, "lib.rs", "one\ntwo\nthree\n");
    git(&work, &["checkout", "-q", "-b", "pr"]);
    let head = commit(&work, "lib.rs", "one\n2\nthree\n");
    git(&work, &["checkout", "-q", "main"]);
    let base = commit(&work, "unrelated.rs", "base moved on\n");
    let github_arg = github_repo.to_string_lossy();
    git(
        &work,
        &["push", "-q", &github_arg, "main", "pr:refs/pull/7/head"],
    );
    Remote { root, head, base }
}

fn key() -> PrKey {
    PrKey {
        repo: RepoName::new("org", "repo"),
        number: 7,
    }
}

#[tokio::test]
async fn checks_out_the_head_and_diffs_from_the_merge_base() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy();
    let dest = data.path().join("worktrees/1");

    let worktree = mirrors
        .checkout(&url, &key(), &remote.head, &remote.base, &dest)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join("lib.rs")).unwrap(),
        "one\n2\nthree\n"
    );
    let diff = worktree.diff().await.unwrap();
    assert!(diff.contains("+++ b/lib.rs"), "{diff}");
    assert!(!diff.contains("unrelated.rs"), "{diff}");

    worktree.remove().await;
    assert!(!dest.exists());
    let mirror = data.path().join("mirrors/org/repo.git");
    assert_eq!(git(&mirror, &["worktree", "list"]).lines().count(), 1);
}

#[tokio::test]
async fn a_leftover_worktree_is_replaced() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy();
    let dest = data.path().join("worktrees/1");

    // Simulate a crash: the first worktree is never removed.
    let _abandoned = mirrors
        .checkout(&url, &key(), &remote.head, &remote.base, &dest)
        .await
        .unwrap();
    std::fs::write(dest.join("scratch"), "x").unwrap();
    mirrors
        .checkout(&url, &key(), &remote.head, &remote.base, &dest)
        .await
        .unwrap();
    assert!(!dest.join("scratch").exists());
}

#[tokio::test]
async fn a_vanished_head_is_an_error() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy();
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let err = mirrors
        .checkout(&url, &key(), missing, &remote.base, &data.path().join("wt"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("is gone"), "{err:?}");
}
