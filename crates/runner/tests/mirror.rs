//! Mirrors and PR worktrees, with local bare repos standing in for GitHub.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{git, key, remote};
use sanic_runner::mirror::Mirrors;
use tempfile::TempDir;

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

#[tokio::test]
async fn a_lost_worktree_can_be_removed_by_path() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy();
    let dest = data.path().join("worktrees/1");

    // Simulate a panic: the `Worktree` is dropped without being removed.
    drop(
        mirrors
            .checkout(&url, &key(), &remote.head, &remote.base, &dest)
            .await
            .unwrap(),
    );
    mirrors.remove_worktree(&key().repo, &dest).await;
    assert!(!dest.exists());
    let mirror = data.path().join("mirrors/org/repo.git");
    assert_eq!(git(&mirror, &["worktree", "list"]).lines().count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn mirror_file_locks_exclude_other_holders() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("org/repo.git.lock");
    let held = sanic_runner::mirror::lock_file(&path).await.unwrap();
    // A second open of the file is what another process would do.
    let waiting = tokio::spawn({
        let path = path.clone();
        async move { sanic_runner::mirror::lock_file(&path).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!waiting.is_finished(), "the lock was taken twice");
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .expect("the lock wasn't released")
        .unwrap();
}

#[tokio::test]
async fn files_are_read_at_a_commit_the_mirror_has() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::in_data_dir(data.path());
    let repo = key().repo;
    // Nothing's fetched yet.
    assert!(!mirrors.has_commit(&repo, &remote.head));
    assert_eq!(
        mirrors.file_at(&repo, &remote.head, "lib.rs").unwrap(),
        None
    );

    let url = remote.root.path().to_string_lossy();
    let dest = data.path().join("worktrees/1");
    let worktree = mirrors
        .checkout(&url, &key(), &remote.head, &remote.base, &dest)
        .await
        .unwrap();
    worktree.remove().await;
    // The worktree's gone, but the mirror keeps the commits.
    assert!(mirrors.has_commit(&repo, &remote.head));
    assert_eq!(
        mirrors.file_at(&repo, &remote.head, "lib.rs").unwrap(),
        Some(b"one\n2\nthree\n".to_vec())
    );
    assert_eq!(
        mirrors.file_at(&repo, &remote.head, "absent.rs").unwrap(),
        None
    );
    // Only SHAs name commits.
    assert!(!mirrors.has_commit(&repo, "HEAD"));
    assert!(!mirrors.has_commit(&repo, "--all"));
}
