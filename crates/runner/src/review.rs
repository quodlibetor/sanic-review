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

use color_eyre::eyre::{Result, WrapErr, bail};
use sanic_core::{
    config::{Profile, RunnerSettings},
    run::{Basis, DraftComment, PrContext, QueuedRun, ReviewOutput, ReviewResult, RevisedOutput},
};
use serde_json::{Value, json};

use crate::{
    chat,
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
    ///
    /// If `cancel` completes while the agent is working, the agent is killed
    /// and this returns `None`. Checkout isn't interrupted, since it's short
    /// and stopping git midway would leave the worktree half made.
    pub async fn review(
        &self,
        run: &QueuedRun,
        ctx: &PrContext,
        settings: &RunSettings,
        cancel: impl Future<Output = ()>,
    ) -> Result<Option<Reviewed>> {
        let req = &run.request;
        let run_dir = self.data_dir.join("runs").join(run.id.to_string());
        tokio::fs::create_dir_all(&run_dir)
            .await
            .wrap_err_with(|| format!("creating {}", run_dir.display()))?;
        let dest = self.worktree_path(run);
        // Checking out removes whatever is there, and a chat may have opened
        // the source review's worktree since the regeneration was queued.
        if run.revision.is_some() && dest.exists() {
            bail!(
                "{} is in use, most likely by a chat with the review's agent; end that and \
                 regenerate again",
                dest.display()
            );
        }
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
        // Dropping the review on cancel drops the agent's process handle,
        // which kills it.
        let result = tokio::select! {
            result = Self::review_in(&claude, &worktree, &run_dir, run, ctx, settings) => Some(result),
            () = cancel => None,
        };
        worktree.remove().await;
        result.transpose()
    }

    /// Removes `run`'s worktree when [`ReviewRunner::review`] didn't get to,
    /// because its task panicked.
    pub async fn discard_worktree(&self, run: &QueuedRun) {
        let path = self.worktree_path(run);
        self.mirrors
            .remove_worktree(&run.request.key.repo, &path)
            .await;
    }

    /// Where `run` checks out, for its review or a chat. A revision uses its
    /// source run's path: Claude Code finds the session it resumes by
    /// directory.
    #[must_use]
    pub fn worktree_path(&self, run: &QueuedRun) -> PathBuf {
        let id = run.revision.as_ref().map_or(run.id, |r| r.source_run);
        chat::worktree_path(&self.data_dir, id)
    }

    /// Checks `run`'s head out again where the review ran, for a chat that
    /// resumes its session; see [`chat::ChatCommand`].
    pub async fn chat_worktree(&self, run: &QueuedRun, git_url: &str) -> Result<Worktree> {
        let req = &run.request;
        self.mirrors
            .checkout(
                git_url,
                &req.key,
                &req.head_sha,
                &req.base_sha,
                &self.worktree_path(run),
            )
            .await
    }

    /// How to resume `run`'s session `session_id` in its worktree, as
    /// `sanic-review chat` runs it and the dashboard shows it. Only names
    /// the worktree; [`ReviewRunner::chat_worktree`] checks it out.
    #[must_use]
    pub fn chat_command(
        &self,
        run: &QueuedRun,
        session_id: &str,
        settings: &RunSettings,
        allow_edits: bool,
    ) -> chat::ChatCommand {
        chat::ChatCommand {
            program: settings.claude.clone(),
            session_id: session_id.to_owned(),
            cwd: self.worktree_path(run),
            add_dirs: self.chat_dirs(run, settings),
            allow_edits,
        }
    }

    /// The directories besides the worktree that `run`'s agent could read,
    /// as a chat should get them again: its run dir, skills and the
    /// reference checkouts that exist.
    fn chat_dirs(&self, run: &QueuedRun, settings: &RunSettings) -> Vec<PathBuf> {
        let runs = self.data_dir.join("runs");
        std::iter::once(runs.join(run.id.to_string()))
            .chain(
                run.revision
                    .as_ref()
                    .map(|r| runs.join(r.source_run.to_string())),
            )
            .chain(settings.profile.skills.iter().cloned())
            .chain(
                existing_dirs(&settings.reference_dirs)
                    .into_iter()
                    .map(Path::to_path_buf),
            )
            .collect()
    }

    async fn review_in(
        claude: &Claude,
        worktree: &Worktree,
        run_dir: &Path,
        run: &QueuedRun,
        ctx: &PrContext,
        settings: &RunSettings,
    ) -> Result<Reviewed> {
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
        let brief = match &run.revision {
            Some(revision) => prompt::revision(&revision.instruction, &revision.baseline),
            None => prompt::brief(&run.request, ctx, &diff, &diff_path),
        };
        write(&run_dir.join("system.md"), &system_prompt).await?;
        write(&run_dir.join("prompt.md"), &brief).await?;

        // The run dir holds the diff; skills and reference checkouts are
        // read in place.
        // A revision's session knows the source run's dir by path.
        let source_dir = run
            .revision
            .as_ref()
            .map(|r| run_dir.with_file_name(r.source_run.to_string()));
        let add_dirs: Vec<PathBuf> = std::iter::once(run_dir.to_owned())
            .chain(source_dir)
            .chain(profile.skills.iter().cloned())
            .chain(references.iter().map(|p| p.to_path_buf()))
            .collect();
        let transcript = run_dir.join("transcript.jsonl");
        let schema = if run.revision.is_some() {
            revision_schema()
        } else {
            review_schema()
        };
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
                resume: run.revision.as_ref().map(|r| r.session_id.as_str()),
            })
            .await?;
        let (output, basis) = if run.revision.is_some() {
            let revised: RevisedOutput = serde_json::from_value(outcome.output)
                .wrap_err("the agent's answer doesn't match the revision schema")?;
            let (output, basis) = revised.split();
            (output, Some(basis))
        } else {
            let output: ReviewOutput = serde_json::from_value(outcome.output)
                .wrap_err("the agent's answer doesn't match the review schema")?;
            (output, None)
        };

        let index = DiffIndex::parse(&diff);
        let result = ReviewResult {
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
        };
        Ok(Reviewed { result, basis })
    }
}

/// A finished review, and for a regeneration, which of the drafts it
/// started from each of its own is based on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewed {
    pub result: ReviewResult,
    pub basis: Option<Basis>,
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

/// [`review_schema`], plus what a regeneration adds: the baseline draft
/// the summary and each comment are based on, if any.
#[must_use]
pub fn revision_schema() -> Value {
    let mut schema = review_schema();
    schema["properties"]["summary_based_on"] = json!({ "type": ["integer", "null"] });
    schema["properties"]["comments"]["items"]["properties"]["based_on"] =
        json!({ "type": ["integer", "null"] });
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_schema_example_parses() {
        let example = json!({
            "summary": "s", "summary_based_on": 4,
            "suggested_verdict": "comment",
            "comments": [{
                "path": "a.rs", "line": 3, "side": "RIGHT", "body": "b",
                "severity": "nit", "confidence": "high", "based_on": 5
            }, {
                "path": "a.rs", "line": 9, "side": "RIGHT", "body": "new",
                "severity": "nit", "confidence": "high"
            }]
        });
        let schema = revision_schema();
        assert_eq!(
            schema["properties"]["comments"]["items"]["properties"]["based_on"]["type"][0],
            "integer"
        );
        let revised: RevisedOutput = serde_json::from_value(example).unwrap();
        let (output, basis) = revised.split();
        assert_eq!(output.comments.len(), 2);
        assert_eq!(basis.summary, Some(4));
        assert_eq!(basis.comments, [Some(5), None]);
    }

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
