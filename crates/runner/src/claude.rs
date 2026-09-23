//! Headless `claude -p` invocations.
//!
//! The agent is capability-limited rather than trusted: it gets read-only
//! tools, no MCP servers, file access confined to the directories it's
//! given, and an environment with no GitHub token.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, bail, eyre},
};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command};

/// Tools a review agent may use.
pub const READ_ONLY_TOOLS: &str = "Read,Grep,Glob";

/// Environment variables that can carry a GitHub token.
pub const TOKEN_VARS: &[&str] = &[
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_ENTERPRISE_TOKEN",
];

pub struct Claude {
    program: PathBuf,
    timeout: Duration,
}

/// One headless session.
pub struct Invocation<'a> {
    /// The agent's working directory, and the root of its file access.
    pub cwd: &'a Path,
    /// Further directories the agent may read.
    pub add_dirs: &'a [PathBuf],
    /// Sent on stdin, which has no length limit, unlike arguments.
    pub prompt: &'a str,
    pub system_prompt: &'a str,
    /// JSON Schema the final answer must match.
    pub schema: &'a Value,
    pub model: Option<&'a str>,
    /// Receives the `stream-json` transcript.
    pub transcript: &'a Path,
    pub stderr: &'a Path,
    /// Resumes this session rather than starting a new one.
    pub resume: Option<&'a str>,
}

/// A session that ended with an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub session_id: Option<String>,
    /// The structured answer.
    pub output: Value,
}

/// Kills a process group when dropped: when the session ends, or when a
/// cancelled review drops it. `kill_on_drop` only reaches the agent itself,
/// not processes it started.
struct KillGroup(u32);

impl Drop for KillGroup {
    fn drop(&mut self) {
        // Usually the group has already exited, and this fails harmlessly.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.0)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Claude {
    #[must_use]
    pub fn new(program: PathBuf, timeout: Duration) -> Self {
        Self { program, timeout }
    }

    fn command(&self, inv: &Invocation<'_>) -> Result<Command> {
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(inv.cwd)
            .args(["-p", "--output-format", "stream-json", "--verbose"])
            .arg("--json-schema")
            .arg(serde_json::to_string(inv.schema)?)
            .arg("--append-system-prompt")
            .arg(inv.system_prompt)
            // `--tools` limits which tools exist, `--allowedTools` lets them
            // run without a prompt, and nobody is there to answer one.
            .args(["--tools", READ_ONLY_TOOLS])
            .args(["--allowedTools", READ_ONLY_TOOLS])
            .args(["--permission-prompts", "none"])
            // Confines file tools to `cwd` and `--add-dir`s, so the agent
            // can't read credentials elsewhere in your home directory. It
            // also ignores settings files, including a `.claude/settings.json`
            // in the untrusted checkout that could add hooks; likewise
            // `--strict-mcp-config` ignores its `.mcp.json`.
            .arg("--restricted")
            .arg("--strict-mcp-config");
        if let Some(model) = inv.model {
            cmd.args(["--model", model]);
        }
        // A fork, so the resumed session stays as it was: a later chat with
        // that run sees only its own turns.
        if let Some(session) = inv.resume {
            cmd.args(["--resume", session, "--fork-session"]);
        }
        for dir in inv.add_dirs {
            cmd.arg("--add-dir").arg(dir);
        }
        for var in TOKEN_VARS {
            cmd.env_remove(var);
        }
        Ok(cmd)
    }

    /// Runs a session to completion and returns its structured answer.
    pub async fn run(&self, inv: &Invocation<'_>) -> Result<Outcome> {
        let program = self.program.display().to_string();
        let transcript = std::fs::File::create(inv.transcript)
            .wrap_err_with(|| format!("creating {}", inv.transcript.display()))?;
        let stderr = std::fs::File::create(inv.stderr)
            .wrap_err_with(|| format!("creating {}", inv.stderr.display()))?;
        let mut child = self
            .command(inv)?
            .stdin(Stdio::piped())
            .stdout(transcript)
            .stderr(stderr)
            .kill_on_drop(true)
            // Its own process group, so `KillGroup` can end whatever the
            // agent started too.
            .process_group(0)
            .spawn()
            .wrap_err_with(|| format!("starting `{program}`"))
            .with_suggestion(|| {
                format!("is `{program}` installed? `runner.claude` in the config sets the path")
            })?;
        let _group = child.id().map(KillGroup);
        // The timeout covers sending the prompt too: a prompt larger than the
        // pipe buffer blocks until the agent reads it.
        let stdin = child.stdin.take();
        let session = async {
            if let Some(mut stdin) = stdin {
                stdin
                    .write_all(inv.prompt.as_bytes())
                    .await
                    .wrap_err("sending the prompt")
                    .with_section(|| stderr_tail(inv.stderr))?;
            }
            child
                .wait()
                .await
                .wrap_err_with(|| format!("waiting for `{program}`"))
        };
        let Ok(status) = tokio::time::timeout(self.timeout, session).await else {
            let _ = child.kill().await;
            bail!("`{program}` ran longer than {:?}", self.timeout);
        };
        let status = status?;
        let stream = std::fs::read_to_string(inv.transcript)
            .wrap_err_with(|| format!("reading {}", inv.transcript.display()))?;
        let result = stream
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|msg| msg["type"] == "result");
        match result {
            Some(result) => parse_result(&result).with_section(|| stderr_tail(inv.stderr)),
            None => Err(eyre!("`{program}` exited with {status} and no result"))
                .section(stderr_tail(inv.stderr)),
        }
    }
}

/// Reads the final `result` message. The structured answer is in
/// `structured_output`; older CLIs only put it in the `result` text.
fn parse_result(result: &Value) -> Result<Outcome> {
    let session_id = result["session_id"].as_str().map(String::from);
    if result["is_error"].as_bool().unwrap_or(false) || result["subtype"] != "success" {
        bail!(
            "the agent failed ({}): {}",
            result["subtype"].as_str().unwrap_or("unknown"),
            result["result"].as_str().unwrap_or("no message")
        );
    }
    let output = if result["structured_output"].is_null() {
        let text = result["result"].as_str().unwrap_or_default();
        serde_json::from_str(strip_fence(text))
            .wrap_err("the agent's answer is not JSON")
            .with_section(|| text.to_owned())?
    } else {
        result["structured_output"].clone()
    };
    Ok(Outcome { session_id, output })
}

/// Unwraps a Markdown code fence around a whole answer, if there is one.
fn strip_fence(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(text, |inner| {
            inner.trim_start_matches(|c: char| c.is_ascii_alphanumeric())
        })
}

fn stderr_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<_> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use serde_json::json;

    use super::*;

    fn invocation<'a>(schema: &'a Value, dirs: &'a [PathBuf]) -> Invocation<'a> {
        Invocation {
            cwd: Path::new("/wt"),
            add_dirs: dirs,
            prompt: "p",
            system_prompt: "s",
            schema,
            model: Some("claude-sonnet-5"),
            transcript: Path::new("/t"),
            stderr: Path::new("/e"),
            resume: None,
        }
    }

    #[test]
    fn resuming_passes_the_session() {
        let schema = json!({});
        let inv = Invocation {
            resume: Some("sess-9"),
            ..invocation(&schema, &[])
        };
        let claude = Claude::new("claude".into(), Duration::from_secs(1));
        let cmd = claude.command(&inv).unwrap();
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert!(
            args.windows(3)
                .any(|a| a == ["--resume", "sess-9", "--fork-session"]),
            "{args:?}"
        );
        assert!(args.contains(&std::ffi::OsStr::new("-p")), "{args:?}");
        assert!(
            args.contains(&std::ffi::OsStr::new("--restricted")),
            "{args:?}"
        );
    }

    #[test]
    fn the_agent_gets_read_only_tools_and_no_github_token() {
        let schema = json!({});
        let dirs = [PathBuf::from("/skills")];
        let claude = Claude::new("claude".into(), Duration::from_secs(1));
        let cmd = claude.command(&invocation(&schema, &dirs)).unwrap();
        let std = cmd.as_std();
        let args: Vec<_> = std.get_args().filter_map(OsStr::to_str).collect();
        let after = |flag: &str| {
            args.iter()
                .position(|a| *a == flag)
                .map(|i| args[i + 1])
                .unwrap()
        };
        assert_eq!(after("--tools"), READ_ONLY_TOOLS);
        assert_eq!(after("--allowedTools"), READ_ONLY_TOOLS);
        assert_eq!(after("--model"), "claude-sonnet-5");
        assert_eq!(after("--add-dir"), "/skills");
        assert!(args.contains(&"--restricted"));
        assert!(args.contains(&"--strict-mcp-config"));
        let removed: Vec<_> = std
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .filter_map(|(k, _)| k.to_str())
            .collect();
        for var in TOKEN_VARS {
            assert!(removed.contains(var), "{var} not removed");
        }
    }

    #[test]
    fn no_model_passes_no_model_flag() {
        let schema = json!({});
        let inv = Invocation {
            model: None,
            ..invocation(&schema, &[])
        };
        let claude = Claude::new("claude".into(), Duration::from_secs(1));
        let cmd = claude.command(&inv).unwrap();
        assert!(!cmd.as_std().get_args().any(|a| a == "--model"));
    }

    #[test]
    fn prefers_structured_output() {
        let outcome = parse_result(&json!({
            "type": "result", "subtype": "success", "is_error": false,
            "session_id": "s", "result": "ignored", "structured_output": { "a": 1 }
        }))
        .unwrap();
        assert_eq!(outcome.session_id.as_deref(), Some("s"));
        assert_eq!(outcome.output, json!({ "a": 1 }));
    }

    #[test]
    fn falls_back_to_json_in_the_result_text() {
        let outcome = parse_result(&json!({
            "type": "result", "subtype": "success", "is_error": false,
            "result": "```json\n{\"a\": 2}\n```"
        }))
        .unwrap();
        assert_eq!(outcome.output, json!({ "a": 2 }));
    }

    #[test]
    fn error_results_fail() {
        let err = parse_result(&json!({
            "type": "result", "subtype": "error_max_turns", "is_error": true
        }))
        .unwrap_err();
        assert!(err.to_string().contains("error_max_turns"), "{err}");
    }
}
