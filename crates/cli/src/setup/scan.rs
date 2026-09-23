//! Finding local checkouts to offer in `setup`.

use std::path::{Path, PathBuf};

use sanic_core::{config::CheckoutResolver, repo::RepoName};

/// Directories that never contain checkouts worth offering, and are often
/// huge.
const SKIP: &[&str] = &["node_modules", "target", "vendor", "bazel-out"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub repo: RepoName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// Checkouts under `root`, at most `depth` directories down, split into
/// ones with a GitHub remote and ones without. A checkout's own
/// subdirectories (including nested workspaces) aren't searched.
pub fn scan(
    root: &Path,
    depth: usize,
    resolver: &dyn CheckoutResolver,
) -> (Vec<Found>, Vec<Skipped>) {
    let mut checkouts = Vec::new();
    find_checkouts(root, depth, &mut checkouts);
    checkouts.sort();
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    for path in checkouts {
        match resolver.resolve(&path, None) {
            Ok((_, repo)) => found.push(Found { path, repo }),
            Err(err) => skipped.push(Skipped {
                path,
                reason: err.to_string(),
            }),
        }
    }
    (found, skipped)
}

fn find_checkouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if dir.join(".jj").is_dir() || dir.join(".git").exists() {
        out.push(dir.to_path_buf());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `file_type` doesn't follow symlinks, so links can't cause loops.
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && !name.starts_with('.') && !SKIP.contains(&name.as_ref()) {
            find_checkouts(&entry.path(), depth - 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use color_eyre::eyre::{Result, eyre};
    use sanic_core::config::Vcs;
    use tempfile::TempDir;

    use super::*;

    /// Checkouts named `*-gh` have a GitHub remote; others don't.
    struct ByName;

    impl CheckoutResolver for ByName {
        fn resolve(&self, path: &Path, _: Option<&str>) -> Result<(Vcs, RepoName)> {
            let name = path.file_name().unwrap().to_string_lossy();
            match name.strip_suffix("-gh") {
                Some(repo) => Ok((Vcs::Git, RepoName::new("org", repo))),
                None => Err(eyre!("no github.com remote found")),
            }
        }
    }

    #[test]
    fn finds_checkouts_within_depth_and_skips_their_insides() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for (path, marker) in [
            ("a-gh", ".git"),
            ("group/b-gh", ".jj"),
            ("group/b-gh/.workspaces/w-gh", ".jj"),
            ("group/b-gh/nested-gh", ".git"),
            ("group/local", ".git"),
            ("deep/er/still/c-gh", ".git"),
            ("node_modules/d-gh", ".git"),
            (".hidden/e-gh", ".git"),
        ] {
            std::fs::create_dir_all(root.join(path).join(marker)).unwrap();
        }
        let (found, skipped) = scan(root, 2, &ByName);
        let found: Vec<_> = found
            .iter()
            .map(|f| f.path.strip_prefix(root).unwrap().to_path_buf())
            .collect();
        assert_eq!(found, [PathBuf::from("a-gh"), PathBuf::from("group/b-gh")]);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].path.ends_with("group/local"));
    }
}
