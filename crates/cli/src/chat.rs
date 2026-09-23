//! `sanic-review chat`, and the TUI's `c`: talk with the agent that did a
//! review, in a fresh copy of that review's worktree.
//!
//! Claude Code keys sessions by directory, so the worktree is checked out
//! again at the path the review used, `<data-dir>/worktrees/<run id>`. It's
//! removed when the chat ends, however it ends.

use std::path::PathBuf;

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, bail, eyre},
};
use sanic_core::{
    config::{Config, default_config_path, default_data_dir},
    pr::PrKey,
};
use sanic_runner::{
    chat::{ChatCommand, worktree_path},
    mirror::Worktree,
    review::{AgentProfile, ReviewRunner, RunSettings},
    vcs::VcsResolver,
};
use sanic_store::{SessionRun, Store};

#[derive(Debug, clap::Args)]
pub struct ChatArgs {
    /// The PR, as its github.com URL (its latest run with a session), or a
    /// run id.
    target: String,

    /// Let the agent edit files (Edit, Write), in the worktree only. The
    /// worktree, edits included, is thrown away when the chat ends.
    #[arg(long)]
    allow_edits: bool,

    /// Print the command to run instead of running it. The worktree stays
    /// until `sanic-review chat --cleanup <run id>`.
    #[arg(long, conflicts_with = "cleanup")]
    print_command: bool,

    /// Remove the worktree a `--print-command` chat of this run left.
    #[arg(long)]
    cleanup: bool,

    /// Config file; defaults to ~/.config/sanic-review/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Where the database lives; defaults to ~/.local/share/sanic-review.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

/// Where a chat finds its config and data.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub data_dir: PathBuf,
}

/// Which run to chat with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The PR's latest run with a session.
    Pr(PrKey),
    Run(i64),
}

impl Target {
    fn parse(text: &str) -> Result<Self> {
        match text.parse() {
            Ok(id) => Ok(Self::Run(id)),
            Err(_) => Ok(Self::Pr(
                PrKey::parse_url(text).wrap_err("not a run id either")?,
            )),
        }
    }
}

pub async fn run(args: ChatArgs) -> Result<()> {
    // Absolute, as `serve` makes them: git runs in the mirror, and the
    // worktree path has to match the review's exactly.
    let paths = Paths {
        config: std::path::absolute(match args.config {
            Some(path) => path,
            None => default_config_path()?,
        })?,
        data_dir: std::path::absolute(match args.data_dir {
            Some(dir) => dir,
            None => default_data_dir()?,
        })?,
    };
    let target = Target::parse(&args.target)?;
    if args.cleanup {
        let removed = cleanup(&paths, &target).await?;
        println!("removed {}", removed.display());
        return Ok(());
    }
    let chat = Chat::prepare(&paths, &target, args.allow_edits).await?;
    if args.print_command {
        println!("{}", chat.command.shell_line());
        eprintln!(
            "the worktree stays; remove it with `sanic-review chat --cleanup {}`",
            chat.session.run.id
        );
        return Ok(());
    }
    chat.run().await
}

/// A chat ready to start: the worktree is checked out.
pub struct Chat {
    session: SessionRun,
    worktree: Worktree,
    pub command: ChatCommand,
}

impl Chat {
    /// Finds `target`'s session and checks its worktree out again. Refuses
    /// while that worktree exists, since another chat has it. A run gets its
    /// session when it finishes, so it's never still running.
    pub async fn prepare(paths: &Paths, target: &Target, allow_edits: bool) -> Result<Self> {
        let config = Config::load(&paths.config, &VcsResolver)?;
        let session = find(paths, target)?;
        let run = &session.run;
        let dir = worktree_path(&paths.data_dir, run.id);
        if dir.exists() {
            return Err(eyre!(
                "{} exists: another chat of run {} may be open",
                dir.display(),
                run.id
            ))
            .suggestion(format!(
                "if it isn't, `sanic-review chat --cleanup {}` removes it",
                run.id
            ));
        }
        let profile = config
            .profiles
            .iter()
            .find(|p| p.name == run.request.profile)
            .ok_or_else(|| {
                eyre!(
                    "profile `{}`, which ran the review, is no longer configured",
                    run.request.profile
                )
            })?;
        let settings = RunSettings::new(
            AgentProfile::from(profile),
            &config.runner,
            &config.github.git_url,
            config.reference_dirs(),
        );
        let runner = ReviewRunner::new(&paths.data_dir);
        let worktree = runner
            .chat_worktree(run, &settings.git_url)
            .await
            .wrap_err_with(|| format!("checking out run {}'s worktree", run.id))?;
        let command = runner.chat_command(run, &session.session_id, &settings, allow_edits);
        Ok(Self {
            session,
            worktree,
            command,
        })
    }

    /// Runs the chat in the foreground, then removes the worktree. Ctrl-C
    /// belongs to the agent meanwhile.
    pub async fn run(self) -> Result<()> {
        let mut command = tokio::process::Command::from(self.command.command());
        let program = self.command.program.display().to_string();
        let ignore_interrupts =
            tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });
        let status = match command.spawn() {
            Ok(mut child) => child
                .wait()
                .await
                .wrap_err_with(|| format!("waiting for `{program}`")),
            Err(err) => Err(err).wrap_err_with(|| format!("starting `{program}`")),
        };
        ignore_interrupts.abort();
        self.worktree.remove().await;
        let status = status?;
        if !status.success() {
            tracing::debug!(%status, "the chat ended");
        }
        Ok(())
    }
}

fn find(paths: &Paths, target: &Target) -> Result<SessionRun> {
    let store = Store::open(&paths.data_dir.join("state.db"))?;
    match target {
        Target::Pr(key) => store
            .latest_session_run(key)?
            .ok_or_else(|| eyre!("no review of {} has a session to chat with yet", key.url())),
        Target::Run(id) => store
            .session_run(*id)?
            .ok_or_else(|| eyre!("run {id} has no session to chat with")),
    }
}

/// Removes the worktree a printed chat command left. Returns its path.
pub async fn cleanup(paths: &Paths, target: &Target) -> Result<PathBuf> {
    let session = find(paths, target)?;
    let dir = worktree_path(&paths.data_dir, session.run.id);
    ReviewRunner::new(&paths.data_dir)
        .discard_worktree(&session.run)
        .await;
    if dir.exists() {
        bail!("couldn't remove {}", dir.display());
    }
    Ok(dir)
}
