//! Bare mirrors of GitHub repositories, and throwaway worktrees of PR heads.
//!
//! Review runs never touch your own checkouts. Each repository gets one
//! bare mirror under the data directory; a run fetches the PR into it and
//! checks the head out as a detached worktree, which it removes afterwards.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, PoisonError},
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};
use sanic_core::{pr::PrKey, repo::RepoName};
use tokio::process::Command;

pub struct Mirrors {
    root: PathBuf,
    /// Checkouts (the fetch and `worktree add`) into one mirror run one at a
    /// time. [`Worktree::remove`] doesn't take this lock, so a removal can
    /// run alongside another run's checkout; `worktree add` locks its new
    /// entry while creating it, so the removal's `worktree prune` skips it.
    locks: Mutex<HashMap<RepoName, Arc<tokio::sync::Mutex<()>>>>,
}

/// A PR head checked out for one run.
#[derive(Debug)]
pub struct Worktree {
    mirror: PathBuf,
    path: PathBuf,
    /// Where the PR branched from its base; GitHub diffs against this.
    merge_base: String,
    head_sha: String,
}

impl Mirrors {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            locks: Mutex::default(),
        }
    }

    fn mirror_path(&self, repo: &RepoName) -> PathBuf {
        self.root
            .join(&repo.owner)
            .join(format!("{}.git", repo.name))
    }

    fn lock(&self, repo: &RepoName) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(repo.clone())
            .or_default()
            .clone()
    }

    /// Fetches `key`'s head and base into the mirror from
    /// `<git_url>/<owner>/<name>.git`, then checks `head_sha` out at `dest`,
    /// replacing anything a previous attempt left there.
    pub async fn checkout(
        &self,
        git_url: &str,
        key: &PrKey,
        head_sha: &str,
        base_sha: &str,
        dest: &Path,
    ) -> Result<Worktree> {
        let lock = self.lock(&key.repo);
        let _guard = lock.lock().await;
        let mirror = self.mirror_path(&key.repo);
        tokio::fs::create_dir_all(&mirror)
            .await
            .wrap_err_with(|| format!("creating {}", mirror.display()))?;
        // Every time, not just when the directory is new: `init` is a no-op
        // on a repo, and if an earlier one failed, `git -C` in a non-repo
        // directory would find and fetch into an enclosing repository.
        git(&mirror, &["init", "--bare", "--quiet"]).await?;

        let url = format!(
            "{}/{}/{}.git",
            git_url.trim_end_matches('/'),
            key.repo.owner,
            key.repo.name
        );
        let pull_ref = format!("+refs/pull/{0}/head:refs/pull/{0}/head", key.number);
        git(
            &mirror,
            &["fetch", "--quiet", "--no-tags", &url, &pull_ref, base_sha],
        )
        .await
        .wrap_err_with(|| format!("fetching {} from {url}", key.url()))?;
        if git(
            &mirror,
            &["cat-file", "-e", &format!("{head_sha}^{{commit}}")],
        )
        .await
        .is_err()
        {
            return Err(eyre!(
                "{} head {head_sha} is gone; it was probably force-pushed away",
                key.url()
            ));
        }
        let merge_base = git(&mirror, &["merge-base", base_sha, head_sha])
            .await?
            .trim()
            .to_owned();

        remove_worktree(&mirror, dest).await;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        let dest_arg = dest.to_string_lossy();
        git(
            &mirror,
            &[
                "worktree", "add", "--detach", "--quiet", &dest_arg, head_sha,
            ],
        )
        .await
        .wrap_err_with(|| format!("checking out {} at {head_sha}", key.url()))?;
        Ok(Worktree {
            mirror,
            path: dest.to_owned(),
            merge_base,
            head_sha: head_sha.to_owned(),
        })
    }
}

impl Worktree {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The PR's diff as GitHub shows it: from the merge base to the head.
    pub async fn diff(&self) -> Result<String> {
        git(
            &self.mirror,
            &[
                "-c",
                "core.quotePath=false",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &self.merge_base,
                &self.head_sha,
            ],
        )
        .await
        .wrap_err("diffing the PR")
    }

    /// Removes the worktree, best effort: a later checkout at the same path
    /// clears whatever is left. Doesn't take the mirror's checkout lock.
    pub async fn remove(self) {
        remove_worktree(&self.mirror, &self.path).await;
    }
}

async fn remove_worktree(mirror: &Path, path: &Path) {
    if path.exists() {
        let path_arg = path.to_string_lossy();
        if git(mirror, &["worktree", "remove", "--force", &path_arg])
            .await
            .is_err()
        {
            let _ = tokio::fs::remove_dir_all(path).await;
        }
    }
    let _ = git(mirror, &["worktree", "prune"]).await;
}

/// Runs git in `repo`, returning stdout, which is decoded lossily because
/// diffs can hold any bytes. Never prompts for credentials.
async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .wrap_err("running `git`")
        .suggestion("is `git` installed and on PATH?")?;
    if !output.status.success() {
        return Err(eyre!(
            "`git {}` exited with {}",
            args.join(" "),
            output.status
        ))
        .section(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
