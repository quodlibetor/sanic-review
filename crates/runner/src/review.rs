//! `review` runs: check out the PR head, ask the agent for a review, and
//! check its inline comments against the diff.
//!
//! Each run keeps its files under `<data>/runs/<id>/`: the prompts, the diff,
//! the `stream-json` transcript and the agent's stderr. Its worktree is
//! `<data>/worktrees/<id>`, so a later resume of the session can recreate
//! the same working directory.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use sanic_core::{
    config::{Effort, Profile, RunnerSettings},
    pr::PrKey,
    run::{
        BaselineDraft, Basis, Carried, Dismissal, DraftComment, DraftOutput, DraftRevision,
        PrContext, ProcedureCheck, QueuedRun, Resume, ReviewOutput, ReviewResult, RevisedOutput,
        Side, Verdict,
    },
};
use serde_json::{Value, json};

use crate::{
    chat, check,
    claude::{Claude, Ended, Invocation, Outcome},
    diff::{DiffIndex, LineMap},
    mirror::{ClaudeTree, Mirrors, Step, Worktree},
    plugin::{self, Plugin},
    prompt,
};

/// What a run needs from its profile.
#[derive(Debug, Clone)]
pub struct AgentProfile {
    pub instructions: Vec<PathBuf>,
    pub procedure: Vec<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub finders: Option<Vec<String>>,
    pub model: Option<String>,
    /// For runs started without an effort of their own.
    pub effort: Effort,
    pub tools: Vec<String>,
}

impl From<&Profile> for AgentProfile {
    fn from(profile: &Profile) -> Self {
        Self {
            instructions: profile.instructions.clone(),
            procedure: profile.procedure.clone(),
            skills: profile.skills.clone(),
            finders: profile.finders.clone(),
            model: profile.model.clone(),
            effort: profile.effort,
            tools: profile.tools.clone(),
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
            mirrors: Mirrors::in_data_dir(data_dir),
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
                |_| {},
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

    /// What `key`'s `.claude` check of `head_sha` on `base_sha` reads; see
    /// [`Mirrors::claude_tree`].
    pub async fn claude_tree(
        &self,
        git_url: &str,
        key: &PrKey,
        head_sha: &str,
        base_sha: &str,
    ) -> Result<ClaudeTree> {
        self.mirrors
            .claude_tree(git_url, key, head_sha, base_sha)
            .await
    }

    /// Removes `run`'s worktree when [`ReviewRunner::review`] didn't get to,
    /// because its task panicked.
    pub async fn discard_worktree(&self, run: &QueuedRun) {
        let path = self.worktree_path(run);
        self.mirrors
            .remove_worktree(&run.request.key.repo, &path)
            .await;
    }

    /// Where `run` checks out, for its review or a chat. A run that resumes
    /// a session uses the path of the run it lives under: Claude Code finds
    /// sessions by directory. See [`QueuedRun::worktree`].
    #[must_use]
    pub fn worktree_path(&self, run: &QueuedRun) -> PathBuf {
        chat::worktree_path(&self.data_dir, run.worktree_run())
    }

    /// Checks `run`'s head out again where the review ran, for a chat that
    /// resumes its session; see [`chat::ChatCommand`]. Tells `progress`
    /// each step of the checkout as it starts.
    pub async fn chat_worktree(
        &self,
        run: &QueuedRun,
        git_url: &str,
        progress: impl FnMut(Step),
    ) -> Result<Worktree> {
        let req = &run.request;
        self.mirrors
            .checkout(
                git_url,
                &req.key,
                &req.head_sha,
                &req.base_sha,
                &self.worktree_path(run),
                progress,
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
            plugin_dir: Some(
                self.data_dir
                    .join("runs")
                    .join(run.id.to_string())
                    .join("plugin"),
            )
            .filter(|dir| dir.is_dir()),
            tools: settings.profile.tools.clone(),
            allow_edits,
        }
    }

    /// The directories besides the worktree that `run`'s agent could read,
    /// as a chat should get them again: its run dirs, procedure and skills,
    /// and the reference checkouts that exist.
    fn chat_dirs(&self, run: &QueuedRun, settings: &RunSettings) -> Vec<PathBuf> {
        run_dirs(run, &self.data_dir.join("runs").join(run.id.to_string()))
            .into_iter()
            .chain(settings.profile.procedure.iter().cloned())
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

        let effort = run.effort.unwrap_or(profile.effort);
        let references = existing_dirs(&settings.reference_dirs);
        let plugin = plugin::build(
            &run_dir.join("plugin"),
            &profile.skills,
            worktree.path().parent().unwrap_or(worktree.path()),
            &ctx.url,
        )?;
        let (system_prompt, required) =
            system_prompt(profile, effort, &diff_path, &references, &plugin).await?;
        let args = prompt::finder_args(effort, &diff_path);
        let target = target(run)?;
        let mut brief = match Self::brief(worktree, run_dir, run, ctx, &diff, &diff_path).await? {
            Ok(brief) => brief,
            Err(not_resumed) => return Ok(not_resumed),
        };
        // A resumed session follows its old context, so it's told again.
        if target.is_none() && (run.revision.is_some() || run.resume.is_some()) {
            brief += &prompt::finder_reminder(&required, &args, run.revision.is_none());
        }
        write(&run_dir.join("system.md"), &system_prompt).await?;
        write(&run_dir.join("prompt.md"), &brief).await?;

        // The run dir holds the diff; the procedure's files, skills and
        // reference checkouts are read in place.
        let add_dirs: Vec<PathBuf> = run_dirs(run, run_dir)
            .into_iter()
            .chain(profile.procedure.iter().cloned())
            .chain(profile.skills.iter().cloned())
            .chain(references.iter().map(|p| p.to_path_buf()))
            .collect();
        let schema = schema(run, target, &required);
        let index = DiffIndex::parse(&diff);
        let mut transcripts = vec![run_dir.join("transcript.jsonl")];
        let mut session = run
            .revision
            .as_ref()
            .map(|r| r.session_id.clone())
            .or(run.resume.as_ref().map(|r| r.session_id.clone()));
        let mut answered: Option<Reviewed> = None;
        loop {
            let attempt = transcripts.len();
            let transcript = transcripts.last().cloned().unwrap_or_default();
            let outcome = claude
                .run(&Invocation {
                    cwd: worktree.path(),
                    add_dirs: &add_dirs,
                    plugin_dir: Some(&plugin.dir),
                    tools: &profile.tools,
                    prompt: &brief,
                    system_prompt: &system_prompt,
                    schema: &schema,
                    model: profile.model.as_deref(),
                    effort,
                    transcript: &transcript,
                    stderr: &run_dir.join(if attempt == 1 {
                        "stderr.log".to_owned()
                    } else {
                        format!("stderr.{attempt}.log")
                    }),
                    resume: session.as_deref(),
                })
                .await;
            let parse = |outcome| match target {
                Some(target) => Self::draft_revised(outcome, target, &index, &transcript),
                None => Self::reviewed(outcome, &index, run, &transcript),
            };
            let (mut reviewed, session_id) =
                match settle(outcome, answered.take(), parse, &ctx.url)? {
                    Settled::Answer(reviewed, session_id) => (reviewed, session_id),
                    Settled::First(first) => return resumed(first, run, worktree, &index).await,
                    Settled::NoSession => {
                        return Ok(Reviewed::NotResumed {
                            why: "its agent session is gone".into(),
                        });
                    }
                };
            let misses = procedure_misses(&mut reviewed, &transcripts, &required, &diff_path);
            let retry = misses.iter().any(|m| m.retry);
            if !retry || attempt > 1 || session_id.is_none() {
                return resumed(reviewed, run, worktree, &index).await;
            }
            let texts: Vec<String> = misses.into_iter().map(|m| m.text).collect();
            tracing::warn!(url = %ctx.url, misses = ?texts, "the review failed its procedure check; asking again");
            brief = prompt::corrective(&texts, &required, &args);
            write(&run_dir.join("prompt.2.md"), &brief).await?;
            transcripts.push(run_dir.join("transcript.2.jsonl"));
            session = session_id;
            answered = Some(reviewed);
        }
    }

    /// The prompt for `run`: its brief, or for a resumed or regenerated run,
    /// what it's asked now. `Err` when the session it resumes can't be.
    async fn brief(
        worktree: &Worktree,
        run_dir: &Path,
        run: &QueuedRun,
        ctx: &PrContext,
        diff: &str,
        diff_path: &Path,
    ) -> Result<Result<String, Reviewed>> {
        Ok(Ok(match (&run.revision, target(run)?, &run.resume) {
            (Some(revision), Some(target), _) => {
                prompt::draft_revision(&revision.instruction, target, ctx)
            }
            (Some(revision), None, _) => {
                prompt::revision(&revision.instruction, &revision.baseline, ctx)
            }
            (None, _, Some(resume)) => {
                let Some(interdiff) = worktree
                    .interdiff(&resume.head_sha, &resume.base_sha)
                    .await?
                else {
                    return Ok(Err(Reviewed::NotResumed {
                        why: format!(
                            "the mirror no longer has {} or {}",
                            resume.head_sha, resume.base_sha
                        ),
                    }));
                };
                let interdiff_path = run_dir.join("interdiff.diff");
                write(&interdiff_path, interdiff.text()).await?;
                prompt::update(
                    &run.request,
                    ctx,
                    resume,
                    &interdiff,
                    &interdiff_path,
                    diff_path,
                )
            }
            (None, _, None) => prompt::brief(&run.request, ctx, diff, diff_path),
        }))
    }
}

/// How one session of a run ended.
enum Settled {
    /// An answer to check, and the session to resume to ask again.
    Answer(Reviewed, Option<String>),
    /// A retry that failed, answered badly or answered with no update,
    /// which would drop the first answer's drafts: the first stands.
    First(Reviewed),
    /// The session to resume is gone.
    NoSession,
}

/// Settles `outcome`, a session's, parsed by `parse`; `first` is the
/// answer a retry follows, which stands if the retry's doesn't.
fn settle(
    outcome: Result<Ended>,
    first: Option<Reviewed>,
    parse: impl FnOnce(Outcome) -> Result<Reviewed>,
    url: &str,
) -> Result<Settled> {
    let outcome = match (outcome, first) {
        (Ok(Ended::Answered(outcome)), first) => (outcome, first),
        (Ok(Ended::NoSession), None) => return Ok(Settled::NoSession),
        (Err(err), None) => return Err(err),
        (failed, Some(first)) => {
            if let Err(err) = failed {
                tracing::warn!(url, "the procedure check's retry failed: {err:#}");
            }
            return Ok(Settled::First(retried(first)));
        }
    };
    let (outcome, first) = outcome;
    let session_id = outcome.session_id.clone();
    match (parse(outcome), first) {
        (Ok(reviewed), None) => Ok(Settled::Answer(reviewed, session_id)),
        (Err(err), None) => Err(err),
        (Ok(reviewed @ (Reviewed::Review { .. } | Reviewed::Resumed { .. })), Some(_)) => {
            Ok(Settled::Answer(reviewed, session_id))
        }
        (parsed, Some(first)) => {
            if let Err(err) = parsed {
                tracing::warn!(url, "the procedure check's retry answered badly: {err:#}");
            }
            Ok(Settled::First(retried(first)))
        }
    }
}

/// `reviewed`, whose procedure check's retry got no answer, counting it.
fn retried(mut reviewed: Reviewed) -> Reviewed {
    if let Reviewed::Review {
        check: Some(check), ..
    }
    | Reviewed::Resumed {
        check: Some(check), ..
    } = &mut reviewed
    {
        check.attempts = 2;
    }
    reviewed
}

/// Checks `reviewed`, a whole review answered in `transcripts`, against
/// `required` and records the result on it. Returns its misses.
fn procedure_misses(
    reviewed: &mut Reviewed,
    transcripts: &[PathBuf],
    required: &[String],
    diff: &Path,
) -> Vec<check::Miss> {
    let (Reviewed::Review { result, check, .. } | Reviewed::Resumed { result, check, .. }) =
        reviewed
    else {
        return Vec::new();
    };
    let report = check.as_ref().map(|c| c.report.clone()).unwrap_or_default();
    let comments: Vec<_> = result.comments.iter().map(|c| c.comment.clone()).collect();
    let answer = check::Answer {
        report: &report,
        verdict: result.verdict,
        comments: &comments,
    };
    let misses = check::misses(transcripts, &answer, required, diff);
    *check = (!required.is_empty() || !misses.is_empty()).then(|| ProcedureCheck {
        report,
        misses: misses.iter().map(|m| m.text.clone()).collect(),
        attempts: u8::try_from(transcripts.len()).unwrap_or(u8::MAX),
    });
    misses
}

/// The schema `run`'s answer must match: a whole review's, with the
/// `run_report` `required` calls for, or `target`'s, for a draft revision.
fn schema(run: &QueuedRun, target: Option<&BaselineDraft>, required: &[String]) -> Value {
    let mut schema = match (&run.revision, target) {
        (Some(_), Some(target)) => return draft_revision_schema(&target.kind),
        (Some(_), None) => revision_schema(),
        // It names the drafts it keeps, as a regeneration does.
        (None, _) if run.resume.is_some() => resume_schema(),
        (None, _) => review_schema(),
    };
    with_run_report(&mut schema, required);
    schema
}

/// Adds the required `run_report` to `schema`, a whole review's, when
/// `required` names any skills: one entry per skill.
fn with_run_report(schema: &mut Value, required: &[String]) {
    if required.is_empty() {
        return;
    }
    schema["properties"]["run_report"] = json!({
        "type": "array",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["skill", "status"],
            "properties": {
                "skill": { "enum": required },
                "status": { "enum": ["ran", "failed"] },
                "reason": { "type": ["string", "null"] },
                "findings_count": { "type": ["integer", "null"], "minimum": 0 }
            }
        }
    });
    if let Some(list) = schema["required"].as_array_mut() {
        list.push(json!("run_report"));
    }
}

/// The system prompt for a run of `profile` at `effort` on the diff at
/// `diff_path`, reading its instruction files and procedure, and the
/// skills that procedure requires of `plugin`'s and the built-ins.
async fn system_prompt(
    profile: &AgentProfile,
    effort: Effort,
    diff_path: &Path,
    references: &[&Path],
    plugin: &Plugin,
) -> Result<(String, Vec<String>)> {
    let mut instructions = Vec::new();
    for path in &profile.instructions {
        let text = tokio::fs::read_to_string(path)
            .await
            .wrap_err_with(|| format!("reading instructions {}", path.display()))?;
        instructions.push((path.display().to_string(), text));
    }
    let mut procedure = Vec::new();
    let mut named = Vec::new();
    for dir in &profile.procedure {
        let path = dir.join("SKILL.md");
        let text = tokio::fs::read_to_string(&path)
            .await
            .wrap_err_with(|| format!("reading procedure {}", path.display()))?;
        let own = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        named.push((own, text.clone()));
        procedure.push((path.display().to_string(), text));
    }
    let required = if profile.tools.iter().any(|t| t == "Skill") {
        check::required_skills(&named, &plugin.skills, profile.finders.as_deref())
    } else {
        Vec::new()
    };
    let skills: Vec<&Path> = profile.skills.iter().map(PathBuf::as_path).collect();
    let prompt = prompt::system_prompt(
        &profile.tools,
        effort,
        diff_path,
        &instructions,
        &procedure,
        &required,
        &skills,
        references,
    );
    Ok((prompt, required))
}

/// `reviewed`, `run`'s answer, with, when `run` resumed a session, where
/// the lines of that session's drafts went at the new head: carried, for
/// a review with no update, or to tell which it kept, for one with drafts
/// of its own.
async fn resumed(
    reviewed: Reviewed,
    run: &QueuedRun,
    worktree: &Worktree,
    index: &DiffIndex,
) -> Result<Reviewed> {
    let Some(resume) = &run.resume else {
        return Ok(reviewed);
    };
    let carried = carry(worktree, &run.request.head_sha, resume, index).await?;
    match reviewed {
        Reviewed::NoUpdate {
            result, dismissed, ..
        } => Ok(Reviewed::NoUpdate {
            result,
            carried,
            dismissed,
        }),
        Reviewed::Review {
            result,
            basis: Some(basis),
            check,
        } => Ok(Reviewed::Resumed {
            result,
            basis,
            resume: resume.clone(),
            moved: carried,
            check,
        }),
        reviewed => Ok(reviewed),
    }
}

impl ReviewRunner {
    /// A regeneration of one draft's answer, `target`'s revision checked
    /// against the diff, or its drop.
    fn draft_revised(
        outcome: Outcome,
        target: &BaselineDraft,
        index: &DiffIndex,
        transcript: &Path,
    ) -> Result<Reviewed> {
        let output: DraftOutput = serde_json::from_value(outcome.output)
            .wrap_err("the agent's answer doesn't match the draft revision schema")?;
        let revised = output
            .for_kind(&target.kind, |comment| index.anchors(comment))
            .ok_or_else(|| {
                eyre!(
                    "the agent's answer neither revises nor drops the {}",
                    target.kind
                )
            })?;
        Ok(Reviewed::Draft {
            revised,
            session_id: outcome.session_id,
            transcript_path: transcript.display().to_string(),
        })
    }

    /// A review's answer, or a whole regeneration's, checked against the
    /// diff.
    fn reviewed(
        outcome: Outcome,
        index: &DiffIndex,
        run: &QueuedRun,
        transcript: &Path,
    ) -> Result<Reviewed> {
        let (output, basis) = if run.revision.is_some() || run.resume.is_some() {
            let revised: RevisedOutput = serde_json::from_value(outcome.output)
                .wrap_err("the agent's answer doesn't match the revision schema")?;
            let (output, basis) = revised.split();
            (output, Some(basis))
        } else {
            let output: ReviewOutput = serde_json::from_value(outcome.output)
                .wrap_err("the agent's answer doesn't match the review schema")?;
            (output, None)
        };

        let result = ReviewResult {
            summary: output.summary,
            summary_note: output.summary_note,
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
            resumed_from: run.resume.as_ref().map(|r| r.run),
        };
        // Resumed, `none` with nothing to say is nothing new: the prompt
        // asks for that.
        if run.resume.is_some() && result.verdict == Verdict::None && result.comments.is_empty() {
            return Ok(Reviewed::NoUpdate {
                result,
                carried: Vec::new(),
                dismissed: basis.map(|b| b.dismissed).unwrap_or_default(),
            });
        }
        let check = ProcedureCheck {
            report: output.run_report,
            misses: Vec::new(),
            attempts: 1,
        };
        Ok(Reviewed::Review {
            result,
            basis,
            check: Some(check),
        })
    }
}

/// For a regeneration of one draft, that draft as the agent is shown it.
fn target(run: &QueuedRun) -> Result<Option<&BaselineDraft>> {
    let Some((revision, id)) = run.revision.as_ref().and_then(|r| Some((r, r.draft?))) else {
        return Ok(None);
    };
    let draft = revision
        .baseline
        .iter()
        .find(|d| d.id == id)
        .ok_or_else(|| eyre!("draft {id} isn't one of run {}'s", revision.revises))?;
    Ok(Some(draft))
}

/// What a finished run answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reviewed {
    /// A review, and for a regeneration, which of the drafts it started
    /// from each of its own is based on.
    Review {
        result: ReviewResult,
        basis: Option<Basis>,
        /// How its procedure check came out, if one ran.
        check: Option<ProcedureCheck>,
    },
    /// A regeneration of one draft: that draft revised, or dropped.
    Draft {
        revised: DraftRevision,
        session_id: Option<String>,
        transcript_path: String,
    },
    /// A resumed review that found nothing new since the run it resumed:
    /// `result` has no comments, and its summary says what it checked.
    /// `carried` are that run's drafts, but those posted, on their lines
    /// at the new head.
    NoUpdate {
        result: ReviewResult,
        carried: Vec<Carried>,
        dismissed: Vec<Dismissal>,
    },
    /// A resumed push review, and which of the drafts of the run it
    /// resumed, `resume`'s, each of its own is based on; `moved` says where
    /// their lines went at the new head.
    Resumed {
        result: ReviewResult,
        basis: Basis,
        resume: Resume,
        moved: Vec<Carried>,
        check: Option<ProcedureCheck>,
    },
    /// The session it was to resume couldn't be, and nothing ran: `why`
    /// says why. A push review can start afresh instead.
    NotResumed { why: String },
}

/// The run dirs `run`'s agent may read: its own, `run_dir`, and those of
/// the runs whose sessions it continues, which it knows by path.
fn run_dirs(run: &QueuedRun, run_dir: &Path) -> Vec<PathBuf> {
    std::iter::once(run_dir.to_owned())
        .chain(
            run.lineage
                .iter()
                .map(|id| run_dir.with_file_name(id.to_string())),
        )
        .collect()
}

/// `resume`'s drafts, but those posted, carried to `head`, checked out in
/// `worktree`: each comment on the lines its own are at there, found by
/// diffing its file between the heads (or, on the old side, between the
/// merge bases), and unanchored where any of its lines changed or they
/// aren't in `index`, the new diff.
async fn carry(
    worktree: &Worktree,
    head: &str,
    resume: &Resume,
    index: &DiffIndex,
) -> Result<Vec<Carried>> {
    let from_merge_base = worktree
        .merge_base_of(&resume.base_sha, &resume.head_sha)
        .await?;
    let mut maps: HashMap<(Side, String), LineMap> = HashMap::new();
    let mut carried = Vec::new();
    for draft in resume.drafts.iter().filter(|d| d.status != "posted") {
        let as_it_was = Carried {
            draft: draft.id,
            start_line: draft.start_line,
            line: draft.line,
            unanchored: false,
        };
        let side = match draft.side.as_deref() {
            Some("LEFT") => Side::Left,
            _ => Side::Right,
        };
        let (Some(path), Some(line)) = (&draft.path, draft.line) else {
            carried.push(as_it_was);
            continue;
        };
        let key = (side, path.clone());
        if !maps.contains_key(&key) {
            let map = match side {
                Side::Left => {
                    worktree
                        .line_map(&from_merge_base, worktree.merge_base(), path)
                        .await?
                }
                Side::Right => worktree.line_map(&resume.head_sha, head, path).await?,
            };
            maps.insert(key.clone(), map);
        }
        let map = &maps[&key];
        // A range is unanchored if anything in it changed, not only its ends.
        let moved = map
            .map_range(draft.start_line.unwrap_or(line), line)
            .map(|(start, line)| (draft.start_line.map(|_| start), line));
        carried.push(match moved {
            Some((start_line, line)) => Carried {
                draft: draft.id,
                start_line,
                line: Some(line),
                unanchored: !index.anchors_at(path, start_line, line, side),
            },
            None => Carried {
                unanchored: true,
                ..as_it_was
            },
        });
    }
    Ok(carried)
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

/// The JSON Schema for [`ReviewOutput`].
#[must_use]
pub fn review_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "suggested_verdict", "comments"],
        "properties": {
            "summary": { "type": "string" },
            "summary_note": { "type": ["string", "null"] },
            "suggested_verdict": { "enum": ["comment", "request_changes", "approve", "none"] },
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
                        "confidence": { "enum": ["high", "medium", "low"] },
                        "note": { "type": ["string", "null"] }
                    }
                }
            }
        }
    })
}

/// The JSON Schema for [`DraftOutput`], answering a regeneration of one
/// draft of `kind`: a comment's replacement as `comment`, in
/// [`review_schema`]'s form, or the summary's as `summary` and
/// `summary_note`; or else `drop_reason`.
#[must_use]
pub fn draft_revision_schema(kind: &str) -> Value {
    let review = review_schema();
    let mut properties = json!({ "drop_reason": { "type": ["string", "null"] } });
    if kind == "summary" {
        properties["summary"] = json!({ "type": ["string", "null"] });
        properties["summary_note"] = review["properties"]["summary_note"].clone();
    } else {
        let mut comment = review["properties"]["comments"]["items"].clone();
        comment["type"] = json!(["object", "null"]);
        properties["comment"] = comment;
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties
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

/// [`revision_schema`], plus what a resumed push review adds: the earlier
/// drafts it dismisses, each with why.
#[must_use]
pub fn resume_schema() -> Value {
    let mut schema = revision_schema();
    schema["properties"]["dismiss"] = json!({
        "type": "array",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["id", "reason"],
            "properties": {
                "id": { "type": "integer" },
                "reason": { "type": "string" }
            }
        }
    });
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_schema_example_parses() {
        let example = json!({
            "summary": "s", "summary_based_on": 4, "summary_note": "why",
            "suggested_verdict": "comment",
            "comments": [{
                "path": "a.rs", "line": 3, "side": "RIGHT", "body": "b",
                "severity": "nit", "confidence": "high", "based_on": 5, "note": "checked"
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
        assert_eq!(output.summary_note.as_deref(), Some("why"));
        assert_eq!(output.comments[0].note.as_deref(), Some("checked"));
        assert_eq!(output.comments[1].note, None);
    }

    #[test]
    fn resume_schema_example_parses() {
        let example = json!({
            "summary": "s", "suggested_verdict": "none", "comments": [],
            "dismiss": [{ "id": 4, "reason": "Resolved." }]
        });
        let schema = resume_schema();
        assert_eq!(
            schema["properties"]["dismiss"]["items"]["required"],
            json!(["id", "reason"])
        );
        let (_, basis) = serde_json::from_value::<RevisedOutput>(example)
            .unwrap()
            .split();
        assert_eq!(
            basis.dismissed,
            [Dismissal {
                id: 4,
                reason: "Resolved.".into()
            }]
        );
    }

    /// The schema and the serde types must accept the same shapes.
    #[test]
    fn schema_example_parses() {
        let example = json!({
            "summary": "s",
            "suggested_verdict": "request_changes",
            "comments": [{
                "path": "a.rs", "line": 3, "start_line": null, "side": "RIGHT",
                "body": "b", "severity": "nit", "confidence": "low", "note": null
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
