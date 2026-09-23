//! `review` runs: check out the PR head, ask the agent for a review, and
//! check its inline comments against the diff.
//!
//! Each run keeps its files under `<data>/runs/<id>/`: the prompts, the diff,
//! the `stream-json` transcript and the agent's stderr. Its worktree is
//! `<data>/worktrees/<id>`, so a later resume of the session can recreate
//! the same working directory.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr};
use sanic_core::{
    config::{Profile, RunnerSettings},
    run::{DraftComment, PrContext, QueuedRun, ReviewOutput, ReviewResult},
};
use serde_json::{Value, json};

use crate::{
    claude::{Claude, Invocation},
    diff::DiffIndex,
    mirror::{Mirrors, Worktree},
    prompt,
};

/// What a run needs from its profile.
#[derive(Debug, Clone)]
pub struct AgentProfile {
    pub instructions: Vec<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub model: Option<String>,
}

impl From<&Profile> for AgentProfile {
    fn from(profile: &Profile) -> Self {
        Self {
            instructions: profile.instructions.clone(),
            skills: profile.skills.clone(),
            model: profile.model.clone(),
        }
    }
}

/// The config a run uses, taken when it starts so a reload applies from the
/// next run on.
#[derive(Debug, Clone)]
pub struct RunSettings {
    pub profile: AgentProfile,
    pub claude: PathBuf,
    pub timeout: Duration,
    /// See `github.git_url`.
    pub git_url: String,
    /// Read-only context for the agent; see `Config::reference_dirs`.
    pub reference_dirs: Vec<PathBuf>,
}

impl RunSettings {
    #[must_use]
    pub fn new(
        profile: AgentProfile,
        runner: &RunnerSettings,
        git_url: &str,
        reference_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            profile,
            claude: runner.claude.clone(),
            timeout: runner.timeout,
            git_url: git_url.to_owned(),
            reference_dirs,
        }
    }
}

/// State that outlives config reloads: the mirrors, and the locks that keep
/// two runs from fetching into one mirror at once.
pub struct ReviewRunner {
    mirrors: Mirrors,
    data_dir: PathBuf,
}

impl ReviewRunner {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            mirrors: Mirrors::new(data_dir.join("mirrors")),
            data_dir: data_dir.to_owned(),
        }
    }

    /// Runs `run` to completion. The worktree is removed whether or not the
    /// review succeeds.
    pub async fn review(
        &self,
        run: &QueuedRun,
        ctx: &PrContext,
        settings: &RunSettings,
    ) -> Result<ReviewResult> {
        let req = &run.request;
        let run_dir = self.data_dir.join("runs").join(run.id.to_string());
        tokio::fs::create_dir_all(&run_dir)
            .await
            .wrap_err_with(|| format!("creating {}", run_dir.display()))?;
        let dest = self.data_dir.join("worktrees").join(run.id.to_string());
        let worktree = self
            .mirrors
            .checkout(
                &settings.git_url,
                &req.key,
                &req.head_sha,
                &req.base_sha,
                &dest,
            )
            .await?;
        let claude = Claude::new(settings.claude.clone(), settings.timeout);
        let result = Self::review_in(&claude, &worktree, &run_dir, run, ctx, settings).await;
        worktree.remove().await;
        result
    }

    async fn review_in(
        claude: &Claude,
        worktree: &Worktree,
        run_dir: &Path,
        run: &QueuedRun,
        ctx: &PrContext,
        settings: &RunSettings,
    ) -> Result<ReviewResult> {
        let profile = &settings.profile;
        let diff = worktree.diff().await?;
        let diff_path = run_dir.join("pr.diff");
        write(&diff_path, &diff).await?;

        let mut instructions = Vec::new();
        for path in &profile.instructions {
            let text = tokio::fs::read_to_string(path)
                .await
                .wrap_err_with(|| format!("reading instructions {}", path.display()))?;
            instructions.push((path.display().to_string(), text));
        }
        let skills: Vec<&Path> = profile.skills.iter().map(PathBuf::as_path).collect();
        let references = existing_dirs(&settings.reference_dirs);
        let system_prompt = prompt::system_prompt(&instructions, &skills, &references);
        let brief = prompt::brief(&run.request, ctx, &diff, &diff_path);
        write(&run_dir.join("system.md"), &system_prompt).await?;
        write(&run_dir.join("prompt.md"), &brief).await?;

        // The run dir holds the diff; skills and reference checkouts are
        // read in place.
        let add_dirs: Vec<PathBuf> = std::iter::once(run_dir.to_owned())
            .chain(profile.skills.iter().cloned())
            .chain(references.iter().map(|p| p.to_path_buf()))
            .collect();
        let transcript = run_dir.join("transcript.jsonl");
        let schema = review_schema();
        let outcome = claude
            .run(&Invocation {
                cwd: worktree.path(),
                add_dirs: &add_dirs,
                prompt: &brief,
                system_prompt: &system_prompt,
                schema: &schema,
                model: profile.model.as_deref(),
                transcript: &transcript,
                stderr: &run_dir.join("stderr.log"),
            })
            .await?;
        let output: ReviewOutput = serde_json::from_value(outcome.output)
            .wrap_err("the agent's answer doesn't match the review schema")?;

        let index = DiffIndex::parse(&diff);
        Ok(ReviewResult {
            summary: output.summary,
            verdict: output.suggested_verdict,
            comments: output
                .comments
                .into_iter()
                .map(|comment| DraftComment {
                    unanchored: !index.anchors(&comment),
                    comment,
                })
                .collect(),
            session_id: outcome.session_id,
            transcript_path: transcript.display().to_string(),
        })
    }
}

/// The directories in `dirs` that exist; a missing one is only a warning,
/// since a checkout can be moved without the config catching up.
fn existing_dirs(dirs: &[PathBuf]) -> Vec<&Path> {
    dirs.iter()
        .map(PathBuf::as_path)
        .filter(|dir| {
            let exists = dir.is_dir();
            if !exists {
                tracing::warn!(dir = %dir.display(), "reference directory is missing; skipping it");
            }
            exists
        })
        .collect()
}

async fn write(path: &Path, contents: &str) -> Result<()> {
    tokio::fs::write(path, contents)
        .await
        .wrap_err_with(|| format!("writing {}", path.display()))
}

/// The JSON Schema for [`ReviewOutput`]. `approve` is deliberately not a
/// verdict.
#[must_use]
pub fn review_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "suggested_verdict", "comments"],
        "properties": {
            "summary": { "type": "string" },
            "suggested_verdict": { "enum": ["comment", "request_changes", "none"] },
            "comments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "line", "side", "body", "severity", "confidence"],
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer", "minimum": 1 },
                        "start_line": { "type": ["integer", "null"], "minimum": 1 },
                        "side": { "enum": ["LEFT", "RIGHT"] },
                        "body": { "type": "string" },
                        "severity": { "enum": ["blocker", "major", "minor", "nit"] },
                        "confidence": { "enum": ["high", "medium", "low"] }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema and the serde types must accept the same shapes.
    #[test]
    fn schema_example_parses() {
        let example = json!({
            "summary": "s",
            "suggested_verdict": "request_changes",
            "comments": [{
                "path": "a.rs", "line": 3, "start_line": null, "side": "RIGHT",
                "body": "b", "severity": "nit", "confidence": "low"
            }]
        });
        let parsed: ReviewOutput = serde_json::from_value(example.clone()).unwrap();
        assert_eq!(parsed.comments[0].line, 3);
        let schema = review_schema();
        let required = schema["properties"]["comments"]["items"]["required"]
            .as_array()
            .unwrap();
        for field in required {
            let field = field.as_str().unwrap();
            assert!(!example["comments"][0][field].is_null(), "{field}");
        }
    }
}
