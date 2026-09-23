//! GitHub repository identity and remote-URL discovery.

use std::fmt;

use color_eyre::{
    Section,
    eyre::{Result, bail, eyre},
};

/// An `owner/name` GitHub repository, normalized to lowercase because GitHub
/// treats both parts case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoName {
    pub owner: String,
    pub name: String,
}

impl RepoName {
    #[must_use]
    pub fn new(owner: &str, name: &str) -> Self {
        Self {
            owner: owner.to_ascii_lowercase(),
            name: name.to_ascii_lowercase(),
        }
    }

    /// Parses `owner/name`.
    pub fn parse(full_name: &str) -> Result<Self> {
        match full_name.split_once('/') {
            Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
                Ok(Self::new(owner, name))
            }
            _ => Err(eyre!("`{full_name}` is not an `owner/name` repository")),
        }
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A named VCS remote, as listed by `jj git remote list` or `git remote -v`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// Extracts the repository from a github.com remote URL, in any of the https,
/// scp-style ssh, or `ssh://` forms. Returns `None` for anything else,
/// including local-path remotes.
#[must_use]
pub fn parse_github_url(url: &str) -> Option<RepoName> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    RepoName::parse(path).ok()
}

/// Picks the GitHub remote for a checkout: the `requested` remote if given,
/// otherwise `upstream`, then `origin`, then the only GitHub remote.
pub fn choose_remote(remotes: &[Remote], requested: Option<&str>) -> Result<RepoName> {
    if let Some(name) = requested {
        let Some(remote) = remotes.iter().find(|r| r.name == name) else {
            let names: Vec<_> = remotes.iter().map(|r| r.name.as_str()).collect();
            return Err(eyre!("remote `{name}` does not exist"))
                .with_note(|| format!("remotes present: {}", names.join(", ")));
        };
        return parse_github_url(&remote.url).ok_or_else(|| {
            eyre!(
                "remote `{name}` ({}) is not a github.com repository",
                remote.url
            )
        });
    }

    let github: Vec<(&Remote, RepoName)> = remotes
        .iter()
        .filter_map(|r| parse_github_url(&r.url).map(|repo| (r, repo)))
        .collect();
    for preferred in ["upstream", "origin"] {
        if let Some((_, repo)) = github.iter().find(|(r, _)| r.name == preferred) {
            return Ok(repo.clone());
        }
    }
    match github.as_slice() {
        [(_, repo)] => Ok(repo.clone()),
        [] => Err(eyre!("no github.com remote found"))
            .suggestion("add a GitHub remote, or set `remote = \"...\"` on this entry"),
        many => {
            let names: Vec<_> = many.iter().map(|(r, _)| r.name.as_str()).collect();
            bail!(
                "several GitHub remotes and none is `upstream` or `origin`: {}; \
                 set `remote = \"...\"` on this entry",
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str, url: &str) -> Remote {
        Remote {
            name: name.into(),
            url: url.into(),
        }
    }

    #[test]
    fn parses_github_url_forms() {
        let expected = Some(RepoName::new("Lacework", "services"));
        for url in [
            "https://github.com/lacework/services.git",
            "https://github.com/lacework/services",
            "https://github.com/lacework/services/",
            "git@github.com:lacework/services.git",
            "ssh://git@github.com/Lacework/Services.git",
        ] {
            assert_eq!(parse_github_url(url), expected, "{url}");
        }
    }

    #[test]
    fn rejects_non_github_urls() {
        for url in [
            "../vuln-dev",
            "/abs/path",
            "https://gitlab.com/a/b.git",
            "https://github.com/only-owner",
            "https://github.com/a/b/c",
        ] {
            assert_eq!(parse_github_url(url), None, "{url}");
        }
    }

    #[test]
    fn prefers_upstream_then_origin() {
        let remotes = [
            remote("origin", "https://github.com/me/fork.git"),
            remote("upstream", "https://github.com/org/repo.git"),
        ];
        assert_eq!(
            choose_remote(&remotes, None).unwrap(),
            RepoName::new("org", "repo")
        );
        assert_eq!(
            choose_remote(&remotes[..1], None).unwrap(),
            RepoName::new("me", "fork")
        );
    }

    #[test]
    fn ignores_local_remotes() {
        let remotes = [
            remote("vuln-dev", "../vuln-dev"),
            remote("mine", "git@github.com:org/repo.git"),
        ];
        assert_eq!(
            choose_remote(&remotes, None).unwrap(),
            RepoName::new("org", "repo")
        );
    }

    #[test]
    fn ambiguous_and_missing_remotes_are_errors() {
        let two = [
            remote("a", "https://github.com/org/one.git"),
            remote("b", "https://github.com/org/two.git"),
        ];
        let err = choose_remote(&two, None).unwrap_err().to_string();
        assert!(err.contains("a, b"), "{err}");
        assert!(choose_remote(&[remote("x", "../x")], None).is_err());
    }

    #[test]
    fn requested_remote_wins_or_errors() {
        let remotes = [
            remote("origin", "https://github.com/org/repo.git"),
            remote("fork", "https://github.com/me/repo.git"),
            remote("local", "../repo"),
        ];
        assert_eq!(
            choose_remote(&remotes, Some("fork")).unwrap(),
            RepoName::new("me", "repo")
        );
        assert!(choose_remote(&remotes, Some("local")).is_err());
        assert!(choose_remote(&remotes, Some("nope")).is_err());
    }
}
