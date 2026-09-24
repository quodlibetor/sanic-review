//! Bare mirrors of GitHub repositories, and throwaway worktrees of PR heads.
//!
//! Review runs never touch your own checkouts. Each repository gets one
//! bare mirror under the data directory; a run fetches the PR into it and
//! checks the head out as a detached worktree, which it removes afterwards.

use std::{
    collections::HashMap,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{Arc, LazyLock, Mutex, PoisonError},
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};
use sanic_core::{pr::PrKey, repo::RepoName};
use tokio::process::Command;

use crate::diff::CONTEXT;

pub struct Mirrors {
    root: PathBuf,
}

/// Checkouts (the fetch and `worktree add`) into one mirror run one at a
/// time, by mirror path. Process-wide rather than per [`Mirrors`], since
/// the TUI's chats check out with their own alongside `serve`'s reviews;
/// [`lock_file`] does the same across processes, for `sanic-review chat`.
/// [`Worktree::remove`] doesn't take this lock, so a removal can run
/// alongside another run's checkout; `worktree add` locks its new entry
/// while creating it, so the removal's `worktree prune` skips it.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// Where the file lock for `mirror` lives: beside it, as `<name>.git.lock`.
fn lock_path(mirror: &Path) -> PathBuf {
    let mut path = mirror.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

/// Takes an exclusive advisory lock (`flock`) on `path`, creating it and its
/// directory as needed, and waits for any other holder, in this process or
/// another. The lock is released when the file is dropped.
pub async fn lock_file(path: &Path) -> Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .wrap_err_with(|| format!("creating {}", dir.display()))?;
    }
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .wrap_err_with(|| format!("opening {}", path.display()))?;
        file.lock()
            .wrap_err_with(|| format!("locking {}", path.display()))?;
        Ok(file)
    })
    .await
    .wrap_err("waiting for a mirror lock")?
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
        Self { root }
    }

    fn mirror_path(&self, repo: &RepoName) -> PathBuf {
        self.root
            .join(&repo.owner)
            .join(format!("{}.git", repo.name))
    }

    /// The mirrors a data dir keeps, under `mirrors/`.
    #[must_use]
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join("mirrors"))
    }

    /// Whether `repo`'s mirror has `commit`, a full or short SHA. Blocks
    /// on `git`, and never fetches.
    #[must_use]
    pub fn has_commit(&self, repo: &RepoName, commit: &str) -> bool {
        let mirror = self.mirror_path(repo);
        is_sha(commit)
            && mirror.is_dir()
            && git_blocking(
                &mirror,
                &["cat-file", "-e", &format!("{commit}^{{commit}}")],
            )
            .is_ok()
    }

    /// `path` as it is at `commit` in `repo`'s mirror, or `None` if the
    /// mirror doesn't have that commit, or that commit doesn't have that
    /// file (a submodule's entry isn't one). Blocks on `git`, and never
    /// fetches.
    pub fn file_at(&self, repo: &RepoName, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let mirror = self.mirror_path(repo);
        if !is_sha(commit) || !mirror.is_dir() {
            return Ok(None);
        }
        // One argument, so a path can't pass for an option. Asking its
        // type fails too if the mirror doesn't have the commit.
        let spec = format!("{commit}:{path}");
        let is_blob = git_blocking(&mirror, &["cat-file", "-t", &spec])
            .is_ok_and(|kind| kind.trim_ascii() == b"blob");
        if !is_blob {
            return Ok(None);
        }
        git_blocking(&mirror, &["cat-file", "blob", &spec])
            .map(Some)
            .wrap_err_with(|| format!("reading {path} at {commit} from {}", mirror.display()))
    }

    /// Removes a worktree of `repo`'s mirror whose [`Worktree`] was lost,
    /// e.g. to a panic. Does nothing if `path` doesn't exist.
    pub async fn remove_worktree(&self, repo: &RepoName, path: &Path) {
        remove_worktree(&self.mirror_path(repo), path).await;
    }

    fn lock(&self, repo: &RepoName) -> Arc<tokio::sync::Mutex<()>> {
        LOCKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(self.mirror_path(repo))
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
        let _file_guard = lock_file(&lock_path(&mirror)).await?;
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
                // GitHub's context, whatever `diff.context` says.
                &format!("--unified={CONTEXT}"),
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

/// Whether `commit` looks like a SHA, so it can only ever name one.
fn is_sha(commit: &str) -> bool {
    (4..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `git` in `repo`. Never prompts for credentials. In its own process
/// group, so a Ctrl-C meant for a TUI chat doesn't reach a review's
/// checkout or the dashboard's reads.
fn git_command(repo: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .process_group(0);
    command
}

/// `git args`' stdout, as it is, if it ran and succeeded.
fn git_stdout(args: &[&str], output: std::io::Result<Output>) -> Result<Vec<u8>> {
    let output = output
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
    Ok(output.stdout)
}

/// Runs git in `repo` and waits, returning stdout as it is.
fn git_blocking(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_stdout(args, git_command(repo, args).output())
}

/// Runs git in `repo`, returning stdout, which is decoded lossily because
/// diffs can hold any bytes.
async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::from(git_command(repo, args)).output().await;
    let stdout = git_stdout(args, output)?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}
