//! Inspecting local jj and git checkouts.
//!
//! jj commands run with `--ignore-working-copy` so that inspecting a user's
//! checkout never snapshots their working copy.

use std::{path::Path, process::Command};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};
use sanic_core::{
    config::{CheckoutResolver, Vcs},
    repo::{Remote, RepoName, choose_remote},
};

/// Resolves config checkouts against the real filesystem.
#[derive(Debug, Default)]
pub struct VcsResolver;

impl CheckoutResolver for VcsResolver {
    fn resolve(&self, path: &Path, remote: Option<&str>) -> Result<(Vcs, RepoName)> {
        let vcs = detect(path)?;
        let remotes = list_remotes(path, vcs)?;
        let repo = choose_remote(&remotes, remote)?;
        Ok((vcs, repo))
    }
}

/// jj if the checkout has `.jj/` (including colocated repos), otherwise git.
pub fn detect(path: &Path) -> Result<Vcs> {
    if path.join(".jj").is_dir() {
        Ok(Vcs::Jj)
    } else if path.join(".git").exists() {
        Ok(Vcs::Git)
    } else if !path.exists() {
        Err(eyre!("{} does not exist", path.display()))
    } else {
        Err(eyre!("{} is not a jj or git checkout", path.display()))
            .suggestion("point at the repository root, which contains `.jj/` or `.git`")
    }
}

pub fn list_remotes(path: &Path, vcs: Vcs) -> Result<Vec<Remote>> {
    match vcs {
        Vcs::Jj => {
            let out = run(Command::new("jj")
                .args(["--ignore-working-copy", "--no-pager", "--color=never", "-R"])
                .arg(path)
                .args(["git", "remote", "list"]))?;
            Ok(parse_remote_lines(&out, None))
        }
        Vcs::Git => {
            let out = run(Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["remote", "-v"]))?;
            Ok(parse_remote_lines(&out, Some("(fetch)")))
        }
    }
}

/// Parses `name url [suffix]` lines, keeping only lines that end with
/// `suffix` when one is given (git lists each remote for fetch and push).
fn parse_remote_lines(out: &str, suffix: Option<&str>) -> Vec<Remote> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let url = fields.next()?;
            if let Some(suffix) = suffix
                && fields.next() != Some(suffix)
            {
                return None;
            }
            Some(Remote {
                name: name.into(),
                url: url.into(),
            })
        })
        .collect()
}

fn run(cmd: &mut Command) -> Result<String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd
        .output()
        .wrap_err_with(|| format!("running `{program}`"))
        .with_suggestion(|| format!("is `{program}` installed and on PATH?"))?;
    if !output.status.success() {
        return Err(eyre!("`{program}` exited with {}", output.status))
            .section(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    String::from_utf8(output.stdout).wrap_err_with(|| format!("`{program}` printed non-UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_remote_v() {
        let out = "origin\thttps://github.com/a/b.git (fetch)\n\
                   origin\thttps://github.com/a/b.git (push)\n\
                   local\t../b (fetch)\n\
                   local\t../b (push)\n";
        let names: Vec<_> = parse_remote_lines(out, Some("(fetch)"))
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["origin", "local"]);
    }

    #[test]
    fn parses_jj_remote_list() {
        let out = "origin https://github.com/a/b.git\nvuln-dev ../vuln-dev\n";
        assert_eq!(parse_remote_lines(out, None).len(), 2);
    }
}
