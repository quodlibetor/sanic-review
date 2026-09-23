//! Remote discovery against real jj and git repositories.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{path::Path, process::Command};

use sanic_core::{
    config::{CheckoutResolver, Vcs},
    repo::RepoName,
};
use sanic_runner::vcs::VcsResolver;
use tempfile::TempDir;

/// Runs a VCS command isolated from the user's global config.
fn vcs(program: &str, dir: &Path, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("JJ_CONFIG", dir.join("no-jj-config.toml"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "{program} {args:?}");
}

fn git_repo(remotes: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    vcs("git", dir.path(), &["init", "-q"]);
    for (name, url) in remotes {
        vcs("git", dir.path(), &["remote", "add", name, url]);
    }
    dir
}

fn jj_repo(remotes: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    vcs("jj", dir.path(), &["git", "init", "--quiet"]);
    for (name, url) in remotes {
        vcs("jj", dir.path(), &["git", "remote", "add", name, url]);
    }
    dir
}

#[test]
fn git_checkout_skips_local_remotes() {
    let dir = git_repo(&[
        ("vuln-dev", "../vuln-dev"),
        ("origin", "git@github.com:Lacework/Services.git"),
    ]);
    let (vcs, repo) = VcsResolver.resolve(dir.path(), None).unwrap();
    assert_eq!(vcs, Vcs::Git);
    assert_eq!(repo, RepoName::new("lacework", "services"));
}

#[test]
fn jj_checkout_prefers_upstream() {
    let dir = jj_repo(&[
        ("origin", "https://github.com/me/fork.git"),
        ("upstream", "https://github.com/org/repo.git"),
    ]);
    let (vcs, repo) = VcsResolver.resolve(dir.path(), None).unwrap();
    assert_eq!(vcs, Vcs::Jj);
    assert_eq!(repo, RepoName::new("org", "repo"));
}

#[test]
fn explicit_remote_overrides_discovery() {
    let dir = jj_repo(&[
        ("origin", "https://github.com/org/repo.git"),
        ("fork", "https://github.com/me/repo.git"),
    ]);
    let (_, repo) = VcsResolver.resolve(dir.path(), Some("fork")).unwrap();
    assert_eq!(repo, RepoName::new("me", "repo"));
}

#[test]
fn non_checkout_is_an_error() {
    let dir = TempDir::new().unwrap();
    let err = VcsResolver.resolve(dir.path(), None).unwrap_err();
    assert!(
        err.to_string().contains("not a jj or git checkout"),
        "{err}"
    );
}

#[test]
fn checkout_without_github_remote_is_an_error() {
    let dir = git_repo(&[("local", "../elsewhere")]);
    let err = VcsResolver.resolve(dir.path(), None).unwrap_err();
    assert!(err.to_string().contains("no github.com remote"), "{err}");
}
