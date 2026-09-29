//! Mirrors and PR worktrees, with local bare repos standing in for GitHub.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use common::{git, key, remote};
use sanic_runner::mirror::{Interdiff, Mirrors, Step, lock_file};
use tempfile::TempDir;

#[tokio::test]
async fn checks_out_the_head_and_diffs_from_the_merge_base() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy();
    let dest = data.path().join("worktrees/1");

    let worktree = mirrors
        .checkout(&url, &key(), &remote.head, &remote.base, &dest, |_| {})
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
        .checkout(&url, &key(), &remote.head, &remote.base, &dest, |_| {})
        .await
        .unwrap();
    std::fs::write(dest.join("scratch"), "x").unwrap();
    mirrors
        .checkout(&url, &key(), &remote.head, &remote.base, &dest, |_| {})
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
        .checkout(
            &url,
            &key(),
            missing,
            &remote.base,
            &data.path().join("wt"),
            |_| {},
        )
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
            .checkout(&url, &key(), &remote.head, &remote.base, &dest, |_| {})
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
    let held = lock_file(&path, || ()).await.unwrap();
    // A second open of the file is what another process would do.
    let waiting = tokio::spawn({
        let path = path.clone();
        async move { lock_file(&path, || ()).await.unwrap() }
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
        .checkout(&url, &key(), &remote.head, &remote.base, &dest, |_| {})
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

#[tokio::test]
async fn a_mirror_with_both_commits_isnt_fetched_into() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy();
    let dest = data.path().join("worktrees/1");

    let mut steps = Vec::new();
    let worktree = mirrors
        .checkout(&url, &key(), &remote.head, &remote.base, &dest, |step| {
            steps.push(step);
        })
        .await
        .unwrap();
    worktree.remove().await;
    assert_eq!(steps, [Step::Fetching, Step::CheckingOut]);

    // Nothing's there to fetch from, so this only works without a fetch.
    let nowhere = data.path().join("nowhere").to_string_lossy().into_owned();
    let mut steps = Vec::new();
    let worktree = mirrors
        .checkout(
            &nowhere,
            &key(),
            &remote.head,
            &remote.base,
            &dest,
            |step| {
                steps.push(step);
            },
        )
        .await
        .unwrap();
    assert_eq!(steps, [Step::CheckingOut]);
    let diff = worktree.diff().await.unwrap();
    assert!(diff.contains("+++ b/lib.rs"), "{diff}");
    assert!(!diff.contains("unrelated.rs"), "{diff}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_says_when_it_waits_for_the_mirror() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy().into_owned();
    // As another process would hold it.
    let other = lock_file(&data.path().join("mirrors/org/repo.git.lock"), || ())
        .await
        .unwrap();

    let steps = Arc::new(Mutex::new(Vec::new()));
    let checkout = tokio::spawn({
        let steps = steps.clone();
        let dest = data.path().join("worktrees/1");
        let head = remote.head.clone();
        let base = remote.base.clone();
        async move {
            mirrors
                .checkout(&url, &key(), &head, &base, &dest, |step| {
                    steps.lock().unwrap().push(step);
                })
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!checkout.is_finished(), "the checkout didn't wait");
    assert_eq!(*steps.lock().unwrap(), [Step::Waiting]);
    drop(other);
    tokio::time::timeout(std::time::Duration::from_secs(10), checkout)
        .await
        .expect("the checkout didn't finish")
        .unwrap();
    assert_eq!(
        *steps.lock().unwrap(),
        [Step::Waiting, Step::Fetching, Step::CheckingOut]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_says_when_it_waits_for_one_in_this_process() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let url = remote.root.path().to_string_lossy().into_owned();
    let other = lock_file(&data.path().join("mirrors/org/repo.git.lock"), || ())
        .await
        .unwrap();
    let start = |n: u32| {
        let mirrors = Mirrors::new(data.path().join("mirrors"));
        let url = url.clone();
        let dest = data.path().join(format!("worktrees/{n}"));
        let head = remote.head.clone();
        let base = remote.base.clone();
        let steps = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let steps = steps.clone();
            async move {
                mirrors
                    .checkout(&url, &key(), &head, &base, &dest, |step| {
                        steps.lock().unwrap().push(step);
                    })
                    .await
                    .unwrap()
            }
        });
        (task, steps)
    };

    // The first holds the in-process lock while it waits for the file lock,
    // so the second waits for the first.
    let (first, first_steps) = start(1);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(*first_steps.lock().unwrap(), [Step::Waiting]);
    let (second, second_steps) = start(2);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!second.is_finished(), "the second checkout didn't wait");
    assert_eq!(*second_steps.lock().unwrap(), [Step::Waiting]);

    drop(other);
    for task in [first, second] {
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("a checkout didn't finish")
            .unwrap();
    }
    // Once: the file lock is free by the time the second gets to it. And
    // the first's fetch left the mirror with both commits.
    assert_eq!(
        *second_steps.lock().unwrap(),
        [Step::Waiting, Step::CheckingOut]
    );
}

#[tokio::test]
async fn an_interdiff_is_a_diff_after_new_commits_and_a_range_diff_after_a_rebase() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy();
    let work = remote.root.path().join("work");
    let github_arg = remote.root.path().join("org/repo.git");
    let github_arg = github_arg.to_string_lossy();
    // The head reviewed first is in the mirror.
    mirrors
        .checkout(
            &url,
            &key(),
            &remote.head,
            &remote.base,
            &data.path().join("wt/1"),
            |_| {},
        )
        .await
        .unwrap()
        .remove()
        .await;

    git(&work, &["checkout", "-q", "pr"]);
    let added = common::commit(&work, "new.rs", "added\n");
    git(
        &work,
        &["push", "-q", "-f", &github_arg, "pr:refs/pull/7/head"],
    );
    let worktree = mirrors
        .checkout(
            &url,
            &key(),
            &added,
            &remote.base,
            &data.path().join("wt/2"),
            |_| {},
        )
        .await
        .unwrap();
    let Some(Interdiff::Diff(diff)) = worktree
        .interdiff(&remote.head, &remote.base)
        .await
        .unwrap()
    else {
        panic!("not a diff");
    };
    assert!(diff.contains("+++ b/new.rs"), "{diff}");
    assert!(!diff.contains("lib.rs"), "{diff}");
    worktree.remove().await;

    // Rebased onto the moved base, the PR's own change unchanged.
    git(&work, &["rebase", "-q", "main"]);
    let rebased = git(&work, &["rev-parse", "HEAD"]);
    git(
        &work,
        &["push", "-q", "-f", &github_arg, "pr:refs/pull/7/head"],
    );
    let worktree = mirrors
        .checkout(
            &url,
            &key(),
            &rebased,
            &remote.base,
            &data.path().join("wt/3"),
            |_| {},
        )
        .await
        .unwrap();
    let Some(Interdiff::RangeDiff(range)) = worktree.interdiff(&added, &remote.base).await.unwrap()
    else {
        panic!("not a range-diff");
    };
    assert_eq!(
        range.lines().filter(|l| l.contains(" = ")).count(),
        2,
        "{range}"
    );
    // Without the earlier head, there's none.
    let gone = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(worktree.interdiff(gone, &remote.base).await.unwrap(), None);
}

#[tokio::test]
async fn an_interdiff_after_merging_the_base_in_is_a_range_diff() {
    let remote = remote();
    let data = TempDir::new().unwrap();
    let mirrors = Mirrors::new(data.path().join("mirrors"));
    let url = remote.root.path().to_string_lossy();
    let work = remote.root.path().join("work");
    let github_arg = remote.root.path().join("org/repo.git");
    let github_arg = github_arg.to_string_lossy();
    mirrors
        .checkout(
            &url,
            &key(),
            &remote.head,
            &remote.base,
            &data.path().join("wt/1"),
            |_| {},
        )
        .await
        .unwrap()
        .remove()
        .await;

    // The old head is an ancestor, but the merge base moved, so the diff
    // between the heads would be the base's.
    git(&work, &["checkout", "-q", "main"]);
    let base = common::commit(&work, "more.rs", "base again\n");
    git(&work, &["checkout", "-q", "pr"]);
    git(&work, &["merge", "-q", "--no-edit", "main"]);
    let merged = git(&work, &["rev-parse", "HEAD"]);
    git(
        &work,
        &[
            "push",
            "-q",
            "-f",
            &github_arg,
            "main",
            "pr:refs/pull/7/head",
        ],
    );
    let worktree = mirrors
        .checkout(
            &url,
            &key(),
            &merged,
            &base,
            &data.path().join("wt/2"),
            |_| {},
        )
        .await
        .unwrap();
    assert!(matches!(
        worktree
            .interdiff(&remote.head, &remote.base)
            .await
            .unwrap(),
        Some(Interdiff::RangeDiff(_))
    ));
    // The PR's own lines are where they were.
    let map = worktree
        .line_map(&remote.head, &merged, "lib.rs")
        .await
        .unwrap();
    assert_eq!(map.map(2), Some(2));
}
