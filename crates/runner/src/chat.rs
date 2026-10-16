//! An interactive session with the agent that did a review, resumed in a
//! copy of that review's worktree.
//!
//! It keeps the review's limits: `--restricted` and `--strict-mcp-config`,
//! so the PR's own `.claude/settings.json` and `.mcp.json` don't load, the
//! profile's tools, or a fixed set when edits are allowed, and no GitHub
//! token.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    claude::{TOKEN_VARS, token_vars},
    prompt::CHAT_ENVIRONMENT,
};

/// The tools a chat with `--allow-edits` gets, whatever the profile's are:
/// no Agent or Skill, so nothing it loads can edit beside it.
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
    /// The run's other directories: its run dir, procedure, skills and
    /// reference checkouts.
    pub add_dirs: Vec<PathBuf>,
    /// The run's `sanic` plugin, if it had one.
    pub plugin_dir: Option<PathBuf>,
    /// The profile's tools, unless edits are allowed.
    pub tools: Vec<String>,
    /// [`EDIT_TOOLS`] in place of `tools`. Only the worktree is given
    /// then, so nothing outside it can be edited.
    pub allow_edits: bool,
}

impl ChatCommand {
    fn args(&self) -> Vec<String> {
        let tools = if self.allow_edits {
            EDIT_TOOLS.to_owned()
        } else {
            self.tools.join(",")
        };
        let mut args: Vec<String> = [
            "--resume",
            &self.session_id,
            "--restricted",
            "--strict-mcp-config",
            "--tools",
            &tools,
            "--allowedTools",
            &tools,
            // The review's system prompt isn't kept with its session.
            "--append-system-prompt",
            CHAT_ENVIRONMENT,
        ]
        .map(String::from)
        .into();
        if !self.allow_edits {
            for dir in &self.add_dirs {
                args.push("--add-dir".into());
                args.push(dir.display().to_string());
            }
            if let Some(dir) = &self.plugin_dir {
                args.push("--plugin-dir".into());
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
        for var in token_vars() {
            cmd.env_remove(var);
        }
        cmd
    }

    /// The same, as one shell line to paste: `cd <worktree> && env -u …
    /// claude …`, unsetting [`unset_vars`].
    #[must_use]
    pub fn shell_line(&self) -> String {
        self.shell_line_unsetting(&unset_vars())
    }

    fn shell_line_unsetting(&self, vars: &[String]) -> String {
        let mut words = vec!["env".to_owned()];
        for var in vars {
            words.push("-u".into());
            words.push(quote(var));
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

/// What a pasted chat command unsets: [`TOKEN_VARS`], which the terminal
/// it's pasted in may set, then any other token variable set here.
#[must_use]
pub fn unset_vars() -> Vec<String> {
    let extra: Vec<String> = token_vars()
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| !TOKEN_VARS.contains(&name.as_str()))
        .collect();
    TOKEN_VARS
        .iter()
        .map(|&v| v.to_owned())
        .chain(extra)
        .collect()
}

/// `word` as a single POSIX shell word.
#[must_use]
pub fn quote(word: &str) -> String {
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
            plugin_dir: Some("/data/runs/3/plugin".into()),
            tools: ["Read", "Grep", "Glob", "Agent", "Skill"]
                .map(String::from)
                .into(),
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
                "Read,Grep,Glob,Agent,Skill",
                "--allowedTools",
                "Read,Grep,Glob,Agent,Skill",
                "--append-system-prompt",
                CHAT_ENVIRONMENT,
                "--add-dir",
                "/data/runs/3",
                "--add-dir",
                "/src/lib's",
                "--plugin-dir",
                "/data/runs/3/plugin",
            ]
        );
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/data/worktrees/3")));
        let mut removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_owned())
            .collect();
        removed.sort();
        assert_eq!(removed, token_vars());
    }

    #[test]
    fn a_pasted_chat_unsets_the_known_token_vars_first() {
        let vars = unset_vars();
        assert_eq!(vars[..TOKEN_VARS.len()], *TOKEN_VARS);
        assert!(chat(false).shell_line().starts_with(
            "cd /data/worktrees/3 && env -u GITHUB_TOKEN -u GH_TOKEN -u GITHUB_ENTERPRISE_TOKEN \
             -u GH_ENTERPRISE_TOKEN "
        ));
    }

    #[test]
    fn edits_confine_the_agent_to_the_worktree() {
        let cmd = chat(true).command();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2).any(|a| a == ["--tools", EDIT_TOOLS]),
            "the profile's Agent and Skill aren't kept: {args:?}"
        );
        assert!(!args.contains(&"--add-dir".to_owned()));
        assert!(!args.contains(&"--plugin-dir".to_owned()));
    }

    #[test]
    fn the_shell_line_quotes_what_needs_it() {
        assert_eq!(
            chat(false).shell_line_unsetting(&["GH_TOKEN".into(), "odd name".into()]),
            format!(
                "cd /data/worktrees/3 && env -u GH_TOKEN -u 'odd name' '/opt/my claude' \
                 --resume sess-9 --restricted \
                 --strict-mcp-config --tools Read,Grep,Glob,Agent,Skill \
                 --allowedTools Read,Grep,Glob,Agent,Skill \
                 --append-system-prompt {} \
                 --add-dir /data/runs/3 --add-dir '/src/lib'\\''s' \
                 --plugin-dir /data/runs/3/plugin",
                quote(CHAT_ENVIRONMENT)
            )
        );
        assert!(!CHAT_ENVIRONMENT.contains(['\n', '\'']));
    }
}
