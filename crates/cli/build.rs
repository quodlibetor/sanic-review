//! Records the git commit the binary is built from, for `--version`.
//!
//! Emits `SANIC_GIT_SHA`, `SANIC_GIT_TIME` (the commit's Unix time) and
//! `SANIC_GIT_DIRTY`; `src/version.rs` formats them. Anything git can't
//! answer leaves them unset, and the build carries on without them.
//!
//! A build cache that replays build-script results across checkouts (mbx
//! with build-script caching on, say) can make a local `--version` stale or
//! `unknown commit`. Release builds don't use one.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // These pick which repository and index git reads.
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_CEILING_DIRECTORIES",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return;
    };
    let dir = Path::new(&manifest_dir);

    // Only trust a repository that tracks this crate. A jj workspace under
    // `.workspaces/` has no `.git`, so git would find the enclosing repo, whose
    // HEAD and status describe a different working copy; an unpacked crate
    // inside some unrelated repo is the same case.
    if git(dir, &["ls-files", "--error-unmatch", "Cargo.toml"]).is_none() {
        return;
    }
    // A new commit moves HEAD or the branch it names, and staging moves the
    // index. Editing a tracked file touches none of these, so `-dirty` can lag
    // until the next commit or `git status`. `--git-path` resolves each in a
    // linked worktree too. Only existing paths are listed: cargo reruns every
    // build for a missing one.
    let branch = git(dir, &["symbolic-ref", "-q", "HEAD"]);
    for name in ["HEAD", "index", "packed-refs"]
        .into_iter()
        .chain(branch.as_deref())
    {
        let Some(path) = git(dir, &["rev-parse", "--git-path", name]) else {
            continue;
        };
        let mut path = dir.join(path);
        if Some(name) == branch.as_deref() {
            // A branch only in `packed-refs` gets its loose file on its next
            // update, without touching HEAD or the index (`reset --soft`), so
            // watch the directory that file will appear in.
            while !path.exists() && path.pop() {}
        }
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    // One call, so the hash and the time are the same commit's.
    // `log.showSignature` would otherwise put a signature check on stdout.
    let Some(head) = git(
        dir,
        &[
            "log",
            "-1",
            "--no-show-signature",
            "--abbrev=8",
            "--format=%h %ct",
        ],
    ) else {
        return;
    };
    let Some((sha, time)) = head.split_once(' ') else {
        return;
    };
    println!("cargo:rustc-env=SANIC_GIT_SHA={sha}");
    println!("cargo:rustc-env=SANIC_GIT_TIME={time}");
    // Without `--no-optional-locks`, status refreshes the index file this
    // script watches, and the next build would run it again.
    let status = [
        "--no-optional-locks",
        "status",
        "--porcelain",
        "--untracked-files=no",
    ];
    if let Some(status) = git(dir, &status) {
        println!("cargo:rustc-env=SANIC_GIT_DIRTY={}", !status.is_empty());
    }
}

/// Runs git in `dir`, returning its trimmed stdout, or `None` if git is
/// missing or fails.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_owned())
}
