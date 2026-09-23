//! An interactive session with the agent that did a review, resumed in a
//! copy of that review's worktree.
//!
//! It keeps the review's limits: `--restricted` and `--strict-mcp-config`,
//! so the PR's own `.claude/settings.json` and `.mcp.json` don't load, the
//! read-only tool set unless edits are allowed, and no GitHub token.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::claude::{READ_ONLY_TOOLS, TOKEN_VARS};

/// Tools a chat with `--allow-edits` may use.
pub const EDIT_TOOLS: &str = "Read,Grep,Glob,Edit,Write";

/// How to resume a run's session. The web dashboard shows
/// [`ChatCommand::shell_line`] to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCommand {
    /// `runner.claude`.
    pub program: PathBuf,
    pub session_id: String,
    /// The run's worktree, which must be where the review ran: Claude Code
    /// keys sessions by directory.
    pub cwd: PathBuf,
    /// The run's other directories: its run dir, skills and reference
    /// checkouts.
    pub add_dirs: Vec<PathBuf>,
    /// Adds Edit and Write. Only the worktree is given then, so nothing
    /// outside it can be edited.
    pub allow_edits: bool,
}

impl ChatCommand {
    fn args(&self) -> Vec<String> {
        let tools = if self.allow_edits {
            EDIT_TOOLS
        } else {
            READ_ONLY_TOOLS
        };
        let mut args: Vec<String> = [
            "--resume",
            &self.session_id,
            "--restricted",
            "--strict-mcp-config",
            "--tools",
            tools,
            "--allowedTools",
            tools,
        ]
        .map(String::from)
        .into();
        if !self.allow_edits {
            for dir in &self.add_dirs {
                args.push("--add-dir".into());
                args.push(dir.display().to_string());
            }
        }
        args
    }

    /// The command, ready to run in the foreground.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(&self.cwd).args(self.args());
        for var in TOKEN_VARS {
            cmd.env_remove(var);
        }
        cmd
    }

    /// The same, as one shell line to paste: `cd <worktree> && env -u …
    /// claude …`.
    #[must_use]
    pub fn shell_line(&self) -> String {
        let mut words = vec!["env".to_owned()];
        for var in TOKEN_VARS {
            words.push("-u".into());
            words.push((*var).into());
        }
        words.push(quote(&self.program.display().to_string()));
        words.extend(self.args().iter().map(|a| quote(a)));
        format!(
            "cd {} && {}",
            quote(&self.cwd.display().to_string()),
            words.join(" ")
        )
    }
}

/// `word` as a single POSIX shell word.
fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Where a run's worktree lives, which a chat must reuse.
#[must_use]
pub fn worktree_path(data_dir: &Path, run_id: i64) -> PathBuf {
    data_dir.join("worktrees").join(run_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(allow_edits: bool) -> ChatCommand {
        ChatCommand {
            program: "/opt/my claude".into(),
            session_id: "sess-9".into(),
            cwd: "/data/worktrees/3".into(),
            add_dirs: vec!["/data/runs/3".into(), "/src/lib's".into()],
            allow_edits,
        }
    }

    #[test]
    fn a_chat_keeps_the_reviews_limits() {
        let cmd = chat(false).command();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--resume",
                "sess-9",
                "--restricted",
                "--strict-mcp-config",
                "--tools",
                "Read,Grep,Glob",
                "--allowedTools",
                "Read,Grep,Glob",
                "--add-dir",
                "/data/runs/3",
                "--add-dir",
                "/src/lib's",
            ]
        );
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/data/worktrees/3")));
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(removed.len(), TOKEN_VARS.len());
        assert!(removed.iter().any(|v| v == "GITHUB_TOKEN"));
    }

    #[test]
    fn edits_confine_the_agent_to_the_worktree() {
        let cmd = chat(true).command();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&EDIT_TOOLS.to_owned()));
        assert!(!args.contains(&"--add-dir".to_owned()));
    }

    #[test]
    fn the_shell_line_quotes_what_needs_it() {
        assert_eq!(
            chat(false).shell_line(),
            "cd /data/worktrees/3 && env -u GITHUB_TOKEN -u GH_TOKEN -u GITHUB_ENTERPRISE_TOKEN \
             -u GH_ENTERPRISE_TOKEN '/opt/my claude' --resume sess-9 --restricted \
             --strict-mcp-config --tools Read,Grep,Glob --allowedTools Read,Grep,Glob \
             --add-dir /data/runs/3 --add-dir '/src/lib'\\''s'"
        );
    }
}
