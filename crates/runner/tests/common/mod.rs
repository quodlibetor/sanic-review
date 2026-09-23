//! Helpers shared by the runner's integration tests.

// Each test binary uses a different subset.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::{path::Path, process::Command};

use sanic_core::{pr::PrKey, repo::RepoName};
use tempfile::TempDir;

/// Runs git isolated from the user's config, returning trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
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

pub fn commit(dir: &Path, file: &str, contents: &str) -> String {
    std::fs::write(dir.join(file), contents).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", file]);
    git(dir, &["rev-parse", "HEAD"])
}

/// A fake GitHub under `root`: `org/repo.git` with PR 7 branched off an
/// older base, and the base branch moved on since.
pub struct Remote {
    pub root: TempDir,
    pub head: String,
    pub base: String,
}

pub fn remote() -> Remote {
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

pub fn key() -> PrKey {
    PrKey {
        repo: RepoName::new("org", "repo"),
        number: 7,
    }
}
