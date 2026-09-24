# sanic-review design

Human-guided LLM review of GitHub PRs. A foreground Rust process watches
GitHub as the current user. When something needs attention, it runs a Claude
agent in a read-only checkout and stores the drafted responses locally. A
localhost web dashboard lets you edit, accept or reject each draft before
anything is posted.

**Invariant:** nothing is posted or pushed without an explicit user action,
either a click in the dashboard or a command the user runs. This is permanent,
not a v1 limitation. The agent never has write credentials.

## Process model

One binary, `sanic-review`, running on a tokio runtime:

```
sanic-review serve [--ui logs|tui] [--port N] [--config PATH] [--data-dir PATH] [--manual-reviews]
```

It runs these tasks in one process:

| Task      | Job |
|-----------|-----|
| poller    | Reads the GitHub notifications API and a periodic GraphQL reconcile, and writes `events` |
| scheduler | Turns events into triggers, debounces them, and queues `runs` |
| runner    | Prepares a checkout, invokes `claude -p`, parses structured output into `drafts` |
| web       | axum + server-rendered HTML + htmx. Serves the dashboard and handles edits and submits |
| terminal  | `logs`: tracing output. `tui`: ratatui summary of items you haven't looked at |

Running it as a daemon means wrapping `serve` in a systemd user unit. There is
no separate daemon mode.

`serve --manual-reviews` watches, detects triggers and queues reviews, but
holds them: nothing runs until you start it, so you can look before
spending anything. You start one review at a time, with `r` in the TUI or
`sanic-review review <PR url>`, and just that one runs, now. Held reviews
stay queued and run on the next start without the flag. Under the flag the
log and the TUI's Activity pane say "review held" where they'd otherwise
say "review queued", since nothing will run until you start it.

`sanic-review chat <PR url | run id> [--allow-edits] [--print-command]`
resumes the agent session of a PR's latest run that has one (or of that
run), so you can ask the reviewer about its review. Claude Code keys
sessions by directory, so the run's worktree is checked out again at its
original path, `worktrees/<run id>` in the data dir, at the run's head, and
`claude --resume` runs there in the foreground. It keeps the review's
limits: `--restricted` and `--strict-mcp-config` (the PR's own
`.claude/settings.json` and `.mcp.json` don't load), the read-only tools,
the run's `--add-dir`s, and no GitHub token. `--allow-edits` adds Edit and
Write and drops the `--add-dir`s, so only the worktree can change. The
worktree is removed when the chat ends, however it ends; it's refused while
that path exists (a running review or another chat). `--print-command`
prints the `cd <worktree> && …` line instead and leaves the worktree for it
until `sanic-review chat --cleanup <run id>`.
`sanic_runner::chat::ChatCommand` builds the command, and its
`shell_line()` the line to copy, for the CLI, the TUI and the dashboard.

`sanic-review review <PR url>` asks the running `serve` to review a PR now:
its held review, or else a full review of its current head, subject to the
usual idempotency. It writes a row to a `start_requests` table in the
shared SQLite database, which `serve` checks every couple of seconds and
empties as it goes. There's no network listener. A request made while
`serve` isn't running is handled when it next starts.

`serve` watches its config file, and when it's a symlink, say into a
dotfiles repo, the file it points to as well. Edits from `setup`, the TUI
and the dashboard replace that file, so the link stays a link. A valid
edit applies between poll cycles and
starts a reconcile right away. An invalid one is logged and the previous
config stays in force. `github.api_url` is only read at startup.
Profiles, `[runner]` (including `read_paths`), `github.git_url` and the
set of reference checkouts apply to runs that start after the reload. Lowering `runner.max_concurrent` takes effect as running runs
finish.

The poller has its own SQLite connection; the scheduler and runner share a
second one, so polling never waits on them.

The web server binds to `127.0.0.1` only. Remote access goes through SSH or
port forwarding.

## Configuration

`~/.config/sanic-review/config.toml`. Data lives under
`~/.local/share/sanic-review/`: the SQLite DB, bare repo mirrors, run
transcripts, and `serve.log` when the TUI is in use. Nothing lives under `/tmp`.

Each profile lists the targets it applies to in `repos`. An entry is one of:

- a local checkout path, as a string
- a table with `repo` (local path) and optionally `paths` (globs relative to
  the repo root; a leading `/` is allowed) and `remote` (a remote name that
  overrides discovery)
- a table with `github`, set to `org` or `owner/name`, optionally with `paths`

```toml
[github]
# Token comes from `gh auth token` unless GITHUB_TOKEN is set.
# api_url = "https://api.github.com"   # override for tests or a proxy
# git_url = "https://github.com"       # where repo mirrors fetch from

[poll]
# reconcile_secs = ...                 # GraphQL reconcile interval
# min_notification_secs = ...          # floor under GitHub's X-Poll-Interval
# quiet_secs = ...                     # debounce: how long a PR must go quiet
# updated_within_days = 14             # ignore PRs quiet for longer; 0 = no limit

[review_requests]
teams = ["*", "!storage-platform"]     # which of your teams' requests count
skip_titles = ["build(deps)*"]         # never auto-review PRs with these titles
# skip_drafts = true                   # never auto-review draft PRs

[runner]
# claude = "claude"                    # executable; a bare name uses PATH
# max_concurrent = ...                 # agent runs at once, across all PRs
# timeout_secs = ...                   # kill a run that takes longer
# read_paths = ["~/src/shared-lib"]    # extra dirs the agent may read
# model = "auto"                       # default model; see Model below

[profile.default]
instructions = ["~/.config/sanic-review/instructions/general.md"]
skills = []                            # skill dirs made available to the agent
model = "claude-sonnet-5"
skip_titles = ["wip*"]                 # added to review_requests.skip_titles
# skip_drafts = false                  # overrides review_requests.skip_drafts
repos = [{ github = "my-org" }]

[profile.vuln]
instructions = ["~/.config/sanic-review/instructions/vuln.md"]
skills = ["~/lwcode/services/vulnerability/.claude/skills"]
repos = [
  "~/lwcode/vuln-eval",
  { repo = "~/lwcode/services", paths = ["/vulnerability/**"] },
]
auto_fix = true                        # see Auto-fix
```

**Remote discovery** for local entries runs at startup and on config reload.
It reads remotes with `jj git remote list` if `.jj/` exists, otherwise
`git remote -v`. It keeps only GitHub URLs (https and ssh forms) and ignores
local-path and non-GitHub remotes. From those it takes `upstream` if present,
then `origin`, then the single remaining GitHub remote. If none is found, or
the choice is ambiguous, that's a startup error that names the entry and
suggests `remote = "..."`.

A path-scoped entry selects a PR if any file it changes matches one of the
globs.

**Precedence.** When several entries match a PR, across or within profiles,
the most specific wins: path-scoped, then repo, then org. Between equally
specific matches, the profile that appears first in the config file wins.

**Repo entries claim their repo.** Once any entry names a repo, org entries
stop applying to it. A PR in that repo that misses every path-scoped entry's
globs, with no unscoped entry for the repo, matches nothing and is ignored.

**Local entries have two roles.** They define what's watched, and they
provide the default auto-fix checkout for that repo. The review runner still
uses its own bare mirror and never touches your checkout.

**`sanic-review setup`** writes or updates the config interactively:
- **Teams:** pick which of your teams' review requests count. Unpicked teams
  are written as `!org/slug` after `*`, so teams you join later count until
  you exclude them.
- **Checkouts:** scan a directory for jj and git checkouts with a GitHub
  remote, then pick which to watch, listed by repo name.
- **Orgs:** pick orgs to watch, suggested from your orgs, your teams and the
  checkouts found, plus any others you type.
- **Model:** the default review model (`runner.model`), defaulting to its
  current value or `auto`. Answering `auto` when it's unset leaves the file
  unchanged. Suggests `auto`, the current value, the `opus`/`sonnet`/`haiku`/
  `fable` aliases, the `model` and `availableModels` in your user-level
  Claude settings (`settings.json` in `$CLAUDE_CONFIG_DIR`, else
  `~/.claude`), and the `ANTHROPIC_*MODEL` variables from that file's `env`
  block and your environment; any other id is accepted too.

Current config values are pre-selected. Entries setup doesn't manage
(`owner/name`, path-scoped or remote-override entries) are never removed.
It shows a diff, validates the result, and writes only after you confirm.
Choices that match the current config leave the file unchanged.

## GitHub ingestion

Users can't create webhooks for repos they don't administer, so ingestion
polls:

- **Notifications API** (`GET /notifications`). Sends `If-Modified-Since` and
  honours `X-Poll-Interval`. Reasons used: `review_requested`, `author`,
  `comment`, `mention`, `state_change`. Notifications are hints only; the
  poller never marks them read.
- **GraphQL reconcile.** A slower loop that searches
  `is:open is:pr review-requested:@me` and `is:open is:pr involves:@me`. For
  each hit it fetches head SHA, review threads and comments. This catches
  whatever notifications miss, for example ones read in the browser.

Both loops write normalized rows. Triggers are computed by diffing GitHub state
against stored state, never from notification payloads.

- **Recent PRs only.** Only PRs GitHub saw activity on within
  `poll.updated_within_days` (default 14, 0 for no limit) are looked at.
  The reconcile searches add `updated:>=<date>`, which also keeps the
  first reconcile short, and a refresh skips a PR whose `updatedAt` is
  older, so a notification about it changes nothing. The TUI leaves such
  PRs out. They aren't forgotten: an old PR comes back as soon as it's
  updated again. PRs a reconcile no longer returns are marked no longer
  open, as below, until then. A notification whose own `updated_at` is
  older than the window is dropped before its PR is fetched at all; a
  first poll can return a great many of those.
- **Open PRs only.** A notification can point at a closed or merged PR,
  which still lists its pending review requests. Refreshes skip those PRs,
  so they don't look like new requests.
  A tracked PR is marked no longer open when a refresh finds it closed or
  gone, or when a successful reconcile doesn't return it: merged PRs drop
  out of the `is:open` searches and would otherwise never be refreshed
  again. The same happens to a PR that stops involving you or whose repo
  stops being watched. The next refresh that finds it marks it open
  again.
  Every PR a refresh finds closed or not visible, tracked or not, is
  remembered with the time it was checked, in `closed_prs`. A later
  notification about it is dropped without fetching unless the
  notification was updated after that check, e.g. because the PR was
  reopened. Finding it open again forgets it.
- **Team review requests.** A review requested from a team counts as a
  request to you if you're a member of that team (from `GET /user/teams`,
  refreshed on each reconcile; needs `read:org`) and `review_requests.teams`
  allows it. The filter is an ordered list of globs where the last match
  wins and `!` excludes. A glob with a `/` matches `org/slug`, otherwise it
  matches the slug. The default is `["*"]`. If the team lookup fails, the
  last known teams are kept and discovery carries on.
- **Newest items only.** PR snapshots fetch the newest reviews, threads and
  comments per connection, and log a warning when older ones were cut off.
  A new reply in an old thread that falls outside the newest threads is
  missed until pagination lands; the warning is the signal. Changed files
  are paginated in full, and only fetched when a path-scoped entry covers
  the repo.
- **Sequential refreshes.** Both loops queue PR keys, and one task refreshes
  the queue in turn, most urgent first: PRs the reconcile found requesting
  your review, then the rest it found involving you, then PRs only a
  notification pointed at. A PR is queued once, at its most urgent
  priority. A rate limit pauses everything and keeps the queue, priorities
  included.
- **Progress.** Each reconcile logs what it found ("reconcile: 21 review
  requests, 29 involving you, 50 queued"), and a notification poll that
  queues PRs logs how many. A big refresh batch logs "refreshed 37/264"
  every so often, and the TUI's status bar shows "refreshing 37/264" until
  the queue is empty.

## Triggers

| Situation | Trigger | Run kind |
|-----------|---------|----------|
| Someone else's PR, you're a requested reviewer, not yet reviewed | review requested | `review` (full) |
| Someone else's PR you're requested on or have reviewed goes from draft to ready | ready for review | `review` (full) |
| Someone else's PR you've reviewed, new head SHA | push | `review` (incremental from last reviewed SHA; full until milestone 4) |
| Any PR you've commented on (requested or not), new non-self comment in a thread you're in | reply | `reply` |
| Your PR, new non-self review or comment | feedback | `respond` (draft replies, optionally a fix; see Auto-fix) |

Rules:

- **Debounce.** A trigger fires only after the PR has been quiet for a
  configurable interval. A newer event resets the timer and replaces the
  queued run.
  Any trigger except `approved` counts as an event. When a newer review
  trigger replaces a pending one, the review runs at the newer head and
  stays full if either trigger asked for a full review. Once the timer
  fires the run is `queued`; queueing another review of the PR marks a run
  that hasn't started `superseded`. A review already running on a different
  head is stopped: the agent is killed, its worktree removed, and the run
  marked `superseded` with no drafts, since they'd be about code that has
  since changed. (A later alternative is to hand the running agent the new
  diff and let it carry on, keeping the work it has done.) Reviews still waiting out the quiet interval are held in memory
  only. To cover a restart during that window, the first time `serve`
  refreshes each PR after starting (the first reconcile covers every open
  one), a standing review request on someone else's PR that matches a
  profile goes to the scheduler like a new one. The idempotency rule then
  skips any head that already has a queued, running or succeeded run.
  Such a head isn't debounced at all, so a review held by
  `--manual-reviews` still shows as held after a restart rather than as
  waiting. A
  reloaded `quiet_secs` applies from the next trigger on.
- **Skips.** These PRs are never reviewed automatically:
  - archived ones (see Archive);
  - drafts, while `skip_drafts` is on (the default; a profile's own
    `skip_drafts` overrides `[review_requests]`);
  - ones already reviewed: someone, you included, left a submitted review
    (approving, requesting changes or commenting) on the current head
    commit. Pending and dismissed reviews don't count, and bot accounts'
    reviews (a `Bot` author, or a login ending in `[bot]`) are ignored
    altogether. A push makes the PR eligible again;
  - ones whose title matches a `skip_titles` glob, from `[review_requests]`
    or from the PR's profile. Globs match the whole title, ignoring case,
    and only glob syntax is special, so `(` and `)` match themselves.

  The check runs when a review comes due, against the PR and config at
  that moment, so edits, archiving and config reloads in the meantime
  count. A skipped review is logged on the PR, e.g. "review skipped: title
  matches `build(deps)*`" or "review skipped: already reviewed by alice at
  0123abcd". When several reasons apply, the first in the list above wins.
  The PR is still tracked and shown everywhere.
- **Archive.** Archiving a PR is yours alone to set and clear: new pushes
  and comments don't clear it. An archived PR is silent: it gets no
  automatic runs of any kind, and archiving supersedes every run of it
  that's queued but not started; a running one finishes. Use the TUI's `a` key,
  or `sanic-review archive <PR url>` and `sanic-review unarchive <PR url>`,
  which write the store directly and work while `serve` runs.
- **Idempotency.** A `review` is keyed by `(pr, head_sha)`. A key that
  already has a queued, running or succeeded run is skipped; one whose run
  failed, crashed or was superseded is queued again. A reply or respond
  run is keyed by `(pr, newest comment id covered)`.
- **Force pushes.** If the last reviewed SHA isn't an ancestor of the new
  head, the incremental review gets a range-diff instead of a plain diff.
- **First sight is a baseline.** The first time a PR is seen, its existing
  comments and reviews are recorded without triggering. Only a pending review
  request triggers then, so starting the tool doesn't replay history.
- **Approvals.** On your PRs, an approval raises an informational
  `approved` trigger that starts no run. It also triggers `respond` if it has
  a body; any review that requests changes does too.
- **Stale drafts.** When a new head SHA arrives, pending drafts anchored on
  lines that changed are marked `stale`. They stay visible, not deleted.

## Runner

1. **Checkout.** There's one bare mirror per repo. Checkouts into a mirror
   (its fetch and `worktree add`) take turns: a lock per mirror within the
   process, and an advisory file lock (`<name>.git.lock` beside it) across
   processes, so `sanic-review chat` and `serve` don't fetch into one mirror
   at once. Fetch `refs/pull/N/head`
   and the base, then `git worktree add` at the head SHA under the data dir.
   Remove the worktree when the run finishes. Your own PRs with auto-fix are
   handled differently; see Auto-fix.
   The base is fetched by SHA, since that's what the PR snapshot records.
   The diff runs from the merge base of base and head, as GitHub's does, and
   fetches never prompt for credentials: they use your git credential
   helper or fail.
2. **Prompt.** Assembled from the profile's instruction files and a generated
   brief: PR metadata, the diff or interdiff, relevant threads, previous drafts
   and how you handled them. PR text goes in as quoted data, never as
   instructions.
   The metadata includes the PR's title and description. Each piece of
   PR-authored text is fenced with more backticks than it contains, so it
   can't close its own block.
   The threads are the PR's existing review threads and its conversation,
   as last polled: each thread's path and lines on the head (or, once
   outdated, its lines in the commit it was left on), its side, whether
   it's resolved or outdated, and each comment's author and body. The
   section says the bodies are other people's text whose instructions
   must not be followed. The agent is told not to comment on a point a
   thread already makes, and, when it agrees with an existing comment, to
   say so in its summary, naming whose and where, instead.
3. **Invoke.** Headless `claude -p` with JSON output against a schema,
   `--append-system-prompt` for instructions, and skill dirs from the profile.
   Only read-only tools are allowed (Read, Grep, Glob, and read-only Bash if
   required). There's no network access and no `gh`, and the environment has
   no GitHub token.
   Concretely: the brief goes on stdin, the schema via `--json-schema`, and
   output is `stream-json` so the whole session is kept as the transcript.
   `--tools Read,Grep,Glob` removes every other tool, `--restricted`
   confines file access to the worktree and `--add-dir`s (so the agent can't
   read `gh`'s stored token), and `--strict-mcp-config` loads no MCP servers.
   Skill dirs are passed as `--add-dir`s and listed in the system prompt, so
   the agent reads their `SKILL.md` files directly. A run that outlives
   `runner.timeout_secs` is killed and fails. The agent runs in its own
   process group, and whenever it stops, finished or killed, the whole
   group is killed, so nothing it started outlives it.
   So the agent can check changes that span repositories, every local
   checkout named in any profile's `repos` (the PR's own repo's included)
   is also an `--add-dir`, plus any `runner.read_paths`. The system prompt
   lists them as read-only reference checkouts that may be at a different
   revision than the PR. A directory that doesn't exist when the run
   starts is skipped with a warning. The agent still can't write to them,
   since it has no write tools.
4. **Parse.** Validate the result against the schema. Check each inline
   comment's `(path, line, side)` against the PR diff hunks. A draft that
   fails the check is still stored, flagged `unanchored`, so you can re-anchor
   it or post it as a top-level comment.
5. **Record.** Store the transcript path and Claude session id, so
   "regenerate with instruction" can resume the session.
   **Regenerate** revises a finished review with your instruction. It's a
   new run, of kind `regenerate`, recording its source run and your
   instruction, for the source run's head and profile. It resumes the
   source run's session non-interactively, `claude -p --resume <session> --fork-session` (so the source's own session stays as it was),
   with a review's restrictions and schema, in the source run's worktree
   path (Claude Code finds sessions by directory), with its run dir as an
   extra `--add-dir`. The prompt is your instruction, fenced as your words,
   then the drafts of the run being revised as they stand, fenced as your
   material: each one's id, kind, anchor, current text (your edit if you
   made one) and status; then the PR's threads as they stand now, as a
   review's brief has them, since they may have changed since. The agent is told to keep accepted and edited
   drafts word for word unless you ask otherwise, not to propose rejected
   ones again, not to repeat posted ones, and to return the complete revised
   review, naming for each comment (`based_on`) and for the summary
   (`summary_based_on`) the draft it comes from; the regenerate schema adds
   those two optional fields. Its output becomes the new run's drafts,
   checked against the diff as usual. A draft that names a draft of the
   revised run and matches it word for word, kind and anchor too, keeps that
   draft's status and your edit as they stand when the run finishes:
   accepted stays accepted. Word for word means as it stands or as the agent
   was shown it, so an edit you make while it runs isn't lost. Only the first
   to do so keeps them; a repeat is pending. One that changed
   is pending and records `based_on`, so the dashboard can say "revised
   from #N"; one that names nothing (or a draft it wasn't shown) is new and
   pending. The source run and its drafts stay as they were. It starts at once,
   even under `--manual-reviews`, and within `runner.max_concurrent`.
   It's refused when the run has no session, when the PR's head has moved
   since (regenerating reviews the old head, so start a fresh review), when
   a regeneration of it is already queued or running, and when its worktree
   is in use, e.g. by a chat; the worker checks that again before checking
   out, since checkout replaces what's there. Revising a regeneration
   resumes its session and starts from its drafts, but records the
   original review as the source,
   whose worktree path every revision's session lives under; chats with a
   regeneration use that path too. A regeneration cancelled by `serve` exiting,
   or left unfinished by a crash, fails rather than starting again unasked.
   The dashboard asks for one through `sanic_web::Control::regenerate`.
   Each run keeps `system.md`, `prompt.md`, `pr.diff`, `transcript.jsonl`
   and `stderr.log` under `runs/<id>/` in the data dir. Its worktree is
   always `worktrees/<id>/`, since resuming a session needs the same working
   directory.

A review whose task panics is recorded as `crashed`, with the panic message
as its error, so it's distinguishable from an ordinary failure. Its worktree
is removed as for a failed run, the panic is logged under the run's span,
and `serve` and the other runs carry on.

A global semaphore bounds concurrency. The per-profile model setting controls
cost.

**Model.** A profile's `model` overrides `runner.model`, which applies to
profiles without one. `"auto"` passes no `--model`, so `claude` uses your own
default model at run time; an unset `runner.model` means the same. A
profile's `"auto"` overrides a named `runner.model`. `auto` ignores case. An
empty `model` is an error.

### Output schema (sketch)

```json
{
  "summary": "string",
  "suggested_verdict": "comment | request_changes | none",
  "comments": [{ "path": "", "line": 0, "start_line": null, "side": "RIGHT",
                 "body": "", "severity": "blocker|major|minor|nit",
                 "confidence": "high|medium|low" }],
  "replies":  [{ "thread_id": "", "body": "" }],
  "fixes":    [{ "thread_id": "", "description": "" }]
}
```

The dashboard never offers `approve` as an agent suggestion. You can still
pick it yourself.

Each run kind gets its own schema with only the fields it uses: a `review`
run's has `summary`, `suggested_verdict` and `comments`, so the agent isn't
invited to draft replies or fixes on someone else's PR.

## Storage (SQLite)

| Table | Contents |
|-------|----------|
| `repos` | owner, name, mirror path |
| `prs` | repo, number, title, description, author, is_mine, open, archived, GitHub updated time, matched profile |
| `revisions` | pr, head_sha, base_sha, seen_at |
| `threads` | GitHub thread id, path, lines and side on the head it was fetched at, resolved, outdated, and its lines in the commit it was left on |
| `comments` | GitHub comment id, thread, author, body, link, created_at |
| `events` | raw normalized events from both poll loops |
| `runs` | pr, kind, trigger, key, status (`queued/running/succeeded/failed/crashed/superseded`), suggested verdict, session id, transcript path, timings |
| `drafts` | run, kind (comment/reply/summary), anchor, original body, edited body, status (`pending/accepted/rejected/stale/posted`), unanchored flag |
| `closed_prs` | PRs a refresh found closed or not visible, and when |
| `start_requests` | PRs `sanic-review review` asked the running `serve` to review now |
| `views` | last time you looked at each PR in the dashboard. Drives "unseen" |

A run's summary is stored as a `summary` draft, so it can be edited like any
other draft. When `serve` exits (Ctrl-C, or quitting the TUI), running
reviews are cancelled rather than waited for: their agents are killed,
their worktrees removed, and their runs queued again, which is logged with
their PRs. The next start runs them, or holds them under
`--manual-reviews`. A second Ctrl-C exits without waiting for that. Runs
left `running` by a previous process are requeued at
startup, unless the same PR also has a run queued after it: that one is for
a newer head, so the older run is marked `superseded` instead.

Keeping both the original and the edited body means the edit history is
available when tuning instruction files.

## Dashboard

`serve` serves it on `http://127.0.0.1:<port>/`. Pages are rendered on the
server with maud, and htmx handles in-place edits. Every script and style is
embedded in the binary, so nothing is fetched at runtime.

- **Index.** The TUI's two lists, "Reviews you owe" and "Your PRs", with
  its statuses, archive toggle and `updated_within_days` window, grouped by
  what they ask of you:
  - reviews you owe: **Needs you** (pending drafts, a review you haven't
    looked at, comments to answer, or a run that failed, crashed or is held
    by `--manual-reviews`), **In flight** (waiting out the quiet period,
    queued or running) and **Nothing to do now**, folded (skipped, archived,
    or reviewed with nothing left);
  - your PRs: **Needs you** (comments to answer, changes requested, or
    drafts), **Ready** (approved or mergeable), **Waiting on reviewers**,
    and, when shown, **Archived**.

  Within a group, PRs with a review you haven't looked at come first, marked
  with a dot. Each row leads with its one next thing (`3 drafts`, `failed`,
  `held`, `2 unanswered`, `mergeable`) and has the title; under the title a
  dim line has the PR as `owner/name#N ↗`, linking to GitHub, its author, its
  other statuses and PR state words, and the first line of a failed run's
  error. A row's actions show on hover and on the selected row. It rereads
  the lists, and the counts in the top bar, every few seconds; selection
  follows the PR, so a refresh that reorders rows keeps it. PR state is the
  TUI's: urgent words highlighted, approved and mergeable ones green, the
  rest muted. The PR page's header shows it too, for an open PR whether or
  not either list has it, and leaves out `—`.
- **Top bar.** Every page's: home, where the page is, and the queued,
  running and pending draft counts.
- **PR page.** Drafts first. A header with the PR, its state and actions;
  under it the description and the review runs folded away, the runs'
  summary saying which one's drafts are shown ("Run 2 of 3", when it
  finished and the commit it reviewed). The list numbers the runs the same
  way, and a regeneration says which run it revises, linked, and your
  instruction's first line (all of it on hover). Then a bar that stays in
  view: the tally of accepted, pending and rejected drafts, the verdict
  and Preview. Then the drafts of the latest run that succeeded (or of any
  run you pick). Each comment draft shows the lines around its anchor from
  the run's `pr.diff`; one that isn't on a line of the diff is flagged as
  going in the body, and one a regeneration kept links the draft it
  revises.
- **Existing threads.** Over the drafts, a summary of the PR's review
  threads says how many there are, how many overlap your drafts, and how
  many are resolved; the threads no draft overlaps are folded under it.
  A draft overlaps a thread on the same file and side when their lines
  share one: a thread's lines run from its start line, when it has one,
  to its line. Those are the thread's lines on the PR head it was
  fetched at, so they count only when that's the run's head and the
  thread isn't outdated; an outdated thread's lines where it was left
  count only when that commit is the run's head. Otherwise the lines
  might not be the same lines, so it overlaps nothing, and it's listed
  with "on another commit". Resolved threads never overlap and are
  dimmed. A draft that overlaps a thread says so, and the thread is
  shown under the draft's diff lines. Each thread shows where it is,
  whether it's resolved or outdated, its link on GitHub, and each
  comment's author with an excerpt of its body; the diff shows each
  thread on the reviewed head under the line it ends on, with its first
  comment. The conversation isn't on lines, so it isn't listed. For each draft: edit in place (click its text or `e`; htmx
  saves on change, or a Save button without the script; a box you leave
  by clicking another draft folds, and its save swaps in, only after that
  click, so the drafts don't move under it), accept (`y`),
  reject (`n`), and undo back to pending (`u`). The PR's own actions are
  the TUI's: a review now, with a confirm, ignore by title, chat, and
  archive or unarchive, which writes the store as `sanic-review archive`
  does.
- **Agent.** For a run that succeeded with an agent session, "Agent…"
  opens a confirm card with a box for your instruction. It says what
  happens: the run's session is resumed, not interactively; the result is
  a new run with fresh drafts for the same commit; this run, its drafts
  and your edits stay as they are; it spends tokens; and it's refused if
  the PR's head has moved, when you start a fresh review instead. Its
  submit, a POST, has `serve` queue it through
  `sanic_web::Control::regenerate`, then shows the new run, saying it's
  queued or running until it has drafts, or the card again with why it
  was refused.
- **Confirms.** Asking before acting, and saying how a submit went, use one
  card: what it is, the question or outcome, the PR by title with its
  `owner/name#N`, author and head, why (a failed run's whole error, who
  already reviewed it, the `--manual-reviews` hold), what it costs, and
  the buttons with their keys. A posted review's card is green and links
  to it on GitHub; a failed post's is red. Each is its own page, and from
  the index or a PR page, `r`, the Review now link or Agent… opens that
  page's card as a dialog over the page instead, so you keep your place;
  see Security.
- **Chat with the reviewer.** A PR page with a review that has an agent
  session ends with a compact card for the run whose drafts it shows, or,
  if that one has no session, the latest that does:
  `sanic-review chat <run id>`, with `serve`'s `--config` and
  `--data-dir`, on one line with a Copy button, and a line saying it
  checks the review's worktree out again, removes it when the chat ends,
  and keeps the review's limits. "Run claude yourself instead" opens
  numbered steps, each with its own Copy: `--print-command`, the `claude`
  line that chat runs (from `ChatCommand::shell_line()`), and
  `--cleanup`. Commands scroll sideways rather than wrap. The dashboard
  runs none of them. If the run's profile is no longer configured, which
  `sanic-review chat` refuses, it says so in one line instead. Index rows
  with a session link there, as does `c`, which on the PR page scrolls to
  the card and focuses Copy.
- **Ignore by title.** The TUI's ignore editor, as a page, from `i` or a
  button on a review you owe: the PR's title and description, read-only,
  over a pattern prefilled with the title (glob syntax escaped) to edit
  down. You pick where it goes, `[review_requests]` or one profile, and
  a live preview lists the reviews you owe it would skip there, or the
  glob's error. Saving has `serve` add it to that `skip_titles` the way
  the TUI does, unless it's already there; the config reload picks it up.
- **Submit.** You pick the verdict: comment, request changes or approve.
  The agent's suggestion is preselected, and approve never is. An
  approval has to be picked on the PR page's verdict form (see Security),
  so Approve in a URL alone is refused; the preview's button then reads
  "Approve this PR", or "Approve this PR with the above comments" when
  there are any. The preview shows the review as it'll read (body,
  then each inline comment) beside the exact payload, the JSON request
  that will be sent, with the confirm in a footer that stays in view; on a
  narrow window they stack. Above them, "Check before posting" lists what
  to look at: that you picked Approve, that the reviewed commit is behind
  the PR's head, comments moved into the body, what GitHub's Markdown would
  make easy to miss (mentions, hidden comments, images, HTML tags and
  invisible characters), and drafts still pending, which aren't sent. The body
  is the summary if you accepted it. Accepted comments go out as inline
  comments in one GitHub review, anchored to the reviewed run's head, and
  accepted comments that aren't on a line of the diff are added to the
  body. Each one there is headed by a link to its lines in the file at
  that head, shown as source even for Markdown, or by its plain
  `path:line` for lines of the old file. You confirm, then it posts, only
  if the payload is still exactly what the preview showed; otherwise
  nothing is sent and you preview again. It's sent once: a GitHub error
  is shown and nothing is retried or marked. Once GitHub has it, its
  drafts are marked `posted` and can't be changed. Replies aren't posted
  yet: drafts don't record their thread.
- Opening a PR page updates `views`.
- **Keys.** The TUI's, where they make sense in a browser: `?` help, `Tab`
  and Shift-Tab switch list, `j`/`k` or the arrows move, `g`/`G` jump, `r`
  opens the review-now confirm, `a` archives or unarchives, `A` shows
  archived PRs, `i` opens the ignore editor, `c` the chat commands.
  `Enter` opens the selected PR, clicking a row selects it, and `j`/`k`
  skip a folded group. `q` closes the help, or else goes back to
  the index. A confirm, page or dialog, takes `y`, and `Esc` or `q`
  cancels; on a result page `Esc` takes its way back. Keys are
  ignored while you type in a draft; `Esc` leaves it.
- **Settings.** The dashboard can edit settings such as
  `review_requests.teams`. Edits are written back to the config file,
  preserving its comments and layout, and take effect through the same
  reload path as a hand edit.

## Terminal UI

- `--ui logs`: structured tracing lines. On each state change it also prints a
  one-line summary: unseen PRs, pending drafts, running and queued runs.
  Each finished review logs its PR, suggested verdict, comment and
  unanchored counts, and the first line of its summary.
- `--ui tui`: a ratatui summary with four panes. No editing happens in the
  TUI. Its only actions are rerunning a review, archiving a PR, adding
  a `skip_titles` pattern to the config and opening the dashboard.
  - **Reviews you owe:** open PRs by others that you review: your review
    is requested, or you've left one in any state. Submitting a review
    clears GitHub's request and a push can dismiss the review, so a PR stays
    here through both. The push and ready-for-review triggers use the same
    rule (`PrSnapshot::is_reviewer`). Each row has its PR state (below),
    then the latest run's status (queued, held by `--manual-reviews`,
    running, drafted, failed, crashed) and the pending draft count. A review still waiting
    out the quiet period shows `waiting` with a countdown to when it's
    queued; the scheduler shares those due times with the TUI in memory.
    `waiting` without a countdown means no run and no known due time.
    A PR that isn't reviewed automatically shows why instead: `archived`,
    `skipped: draft`, `reviewed by you, alice` (you first, then others,
    two names at most and `+N` for the rest) or `skipped: title`.
    `sanic_core::skip::SkipRules::decide` makes that decision and
    `Skip::status` words it, for the TUI and the dashboard alike. A failed
    or crashed run also shows the first line of its error.
  - **Your PRs:** every open PR you authored, with its PR state (below)
    and pending drafts.
  - **Activity:** recent triggers and run queues, starts and finishes, with
    the first line of the error for runs that failed or crashed.
  - **Log:** the tracing output.

  **PR state** says where a PR stands, in words that combine:
  - `mergeable`: approved and GitHub's `mergeStateStatus` is `CLEAN` or `HAS_HOOKS`
    (without one, or while it's `UNKNOWN`, approved with passing checks);
  - `approved`, when approved but not mergeable yet, with `ci failing` or
    `ci pending` when that's why;
  - `changes requested`;
  - `N unanswered`: threads, the conversation counting as one, that aren't
    resolved and have someone's comment newer than your latest answer.
    Your own comment answers, and so does your emoji reaction to any comment
    in the thread, as of the reaction's time; review threads only say
    whether you reacted, not when, so the comment's time stands in. Bots'
    comments never need an answer. On your own PRs every thread counts; on
    reviews you owe, only threads you've commented in.

  e.g. `approved · 2 unanswered`, or `—` when there's nothing to say.
  Unanswered comments are highlighted: you need to act. So are changes
  requested on your own PRs; on a review you owe, the author has the
  changes to make, even ones you asked for, so it isn't. Narrow columns
  shorten it (`approved · 2 new`, `2 new`).
  `sanic_core::state::PrState` works it out and words it (`status()` in
  full, `fitted(width)` to fit), for the TUI and the dashboard; the store's
  `pr_state` fills it in for each listed PR.
  "Approved" and "changes requested" are GitHub's `reviewDecision`. A repo
  that doesn't require reviews gives none, so then each reviewer's latest
  approval, change request or dismissal counts, bots' aside: a change
  request by anyone, else an approval by anyone.
  Every PR row shows its github.com URL, so it's clickable. Until the
  dashboard exists there are no dashboard URLs or `views`, so both PR panes
  list every open tracked PR updated within the window. Filtering to unseen items arrives with the
  dashboard.
  The TUI opens its own read-only connection and rereads the store on a
  short interval, so the poller, scheduler and runner don't know it exists.
  Logs never go to stdout while it runs. They go to the log pane and are
  appended to `serve.log` in the data dir.
  Keys: `q` or Ctrl-C quits `serve`; while reviews are running it first
  lists them ("exiting will cancel these running tasks:") and waits for
  Enter or `y`, and any other key stays. A second Ctrl-C quits without
  asking. Tab and Shift-Tab switch pane, `j`/`k`
  or the arrows move, `g`/`G` jump to the first or last row, `?` shows help.
  `a` archives or unarchives the selected PR in either PR pane. Archived
  PRs are hidden, and each pane's title counts them; `A` shows them,
  dimmed and marked `archived`.
  `c` on a PR in either pane chats with the agent that last reviewed it,
  as `sanic-review chat` does: the TUI gives up the terminal for the chat
  and takes it back when the chat ends, and `serve` keeps running
  meanwhile, leaving Ctrl-C to the chat.
  `o` opens the dashboard in your default browser: the selected row's PR
  page in either PR pane or Activity, else (the log, or an empty pane)
  the index. `serve` binds the dashboard before the TUI starts and hands
  it the address actually bound on `127.0.0.1`. The browser is launched
  off the UI thread with its output discarded, so it can't stall or draw
  over the TUI; a text browser named by `$BROWSER` still runs in the
  terminal. A failure to launch is logged and shown in the status bar.
  The status bar's key hint includes `o` where it fits.
  `i` on a review you owe opens an ignore editor: the PR's title and
  description, read-only, over a pattern prefilled with the title (glob
  syntax escaped) to edit down, e.g. to `build(deps)*`. Under it, a live
  preview lists the reviews you owe it would skip; an invalid glob shows
  its error and can't be saved. Enter asks where it goes, `[review_requests]`
  or one profile, and adds it to that `skip_titles` unless it's already
  there. The file is edited in place, keeping comments and layout, and
  written atomically; `serve` picks the change up through its normal
  config reload.
  `r` on a review you owe whose latest run failed or crashed, that's
  skipped or archived, or that `--manual-reviews` is holding, asks for
  confirmation, since it spends tokens, then starts it: the held run, or
  a full review of the PR's head as last polled. For a skipped or
  archived PR that's the way to review it anyway; it stays skipped or
  archived. It goes through the store like any queued review, so
  idempotency applies, and it's logged. It runs right away, even under
  `--manual-reviews`.
  The terminal is restored on exit, and on a panic on the main or TUI
  thread, which also ends `serve`. A panic in a review task leaves the
  terminal alone and shows in the log pane instead.

## Auto-fix (own PRs)

Auto-fix never pushes. A fix is a local edit, shown in the dashboard next to
its drafted "fixed in <change>" reply. You push it yourself, then post the
reply.

When a profile sets `auto_fix = true`, the fix is applied in that repo's
local entry from `repos`. A PR matched only by a `github =` entry has no local
checkout, so it gets no auto-fix and the dashboard says so.

Fixes always run in a temporary workspace or worktree created under the
checkout's `.workspaces/`, never in your working copy. If the checkout has
`.jj/`, jj is used. Otherwise git.

- **jj: the fix amends the PR's change.** Find the local change from the PR
  head SHA (`jj log -r <sha>` resolves hidden predecessors too), then
  `jj workspace add` and `jj edit <change>` in it. The agent's edits rewrite
  that change directly, and descendants rebase automatically. Record the
  pre-fix commit id so the fix's diff and discard work.
  - If the change was rewritten locally after the push, the fix still
    targets its current commit. The dashboard flags it.
  - Any of your workspaces sitting on that change become stale. The
    dashboard reports this. Recovery is `jj workspace update-stale`.
- **git: the fix is a child commit.** Run
  `git worktree add -b sanic-fix/<pr>-<n> <path> <head-sha>` and have the
  agent commit on top of the PR head.
- **Cleanup.** When a run finishes, whether it succeeded or failed, its
  workspace or worktree is removed: `jj workspace forget` plus removing the
  directory, or `git worktree remove`. The fix itself stays, as the amended
  jj change or the `sanic-fix/` branch.
- **Fix agent permissions.** Unlike review runs, the fix agent has write tools
  in its checkout. It still has no network and no GitHub token. Whether it may
  build or test is a per-profile setting.
- **Dashboard actions** for each fix: view the diff (pre-fix commit to
  current), and discard it. For jj, discard is
  `jj restore --from <pre-fix> --into <change>`, and it's refused if the
  change has moved on since the fix. For git, discard deletes the branch.

## Security

- PR content is untrusted and may contain prompt injection. The mitigation is
  capability-based: the agent can't post, push or reach the network, so the
  worst it can do is write a bad draft that you then read.
- The GitHub token lives only in the serve process, and only the web task's
  submit path writes with it.
- Every agent, whether a review, a regeneration or a chat, runs without
  any environment variable whose name looks like a GitHub credential,
  ignoring case: any that names GitHub or starts `GH_` and also names a
  token, PAT, secret, password or key (so `GH_TOKEN`, `MISE_GITHUB_TOKEN`
  and `GITHUB_APP_PRIVATE_KEY`, not `GITHUB_REPOSITORY` or `GH_HOST`).
  `sanic_runner::claude::is_token_var`
  decides, over the names actually set, rather than a fixed list. A chat
  command to paste unsets `gh`'s own token variables, which the terminal
  it's pasted in may set, then the others set where it was printed.
- The dashboard only listens on `127.0.0.1`, but any page open in your
  browser can send it requests. So it answers only when `Host` names the
  loopback interface, which stops DNS rebinding. Every state-changing
  request must carry a random token generated at startup, in a form field
  or an htmx header; a page on another site can't read it. When a request
  says where it came from, through `Origin` or `Sec-Fetch-Site`, it must be
  the dashboard itself. Firefox can send `Origin: null` for the
  dashboard's own form posts; that passes only beside `Sec-Fetch-Site:
  same-origin`, which only the browser sets.
- A page another site sends you to, by a link or `window.open`, shows only
  a link to itself, so that site can't open a confirm page and catch a key
  you're typing. Browsers that send `Sec-Fetch-Site` say where a
  navigation came from. Without it, a `Referer` from anywhere but the
  dashboard counts, and no `Referer` doesn't, since typed URLs and
  bookmarks send none. Keys also do nothing just after a page opens or
  comes to the front, or while held down.
- A confirm can also open as a dialog over the index or a PR page. Only
  the page's own script opens it, on your click or key there: it fetches
  the confirm page and shows that page's card, so another site has no
  way to open it, and its form posts exactly as the page's does, with the
  token and the same-origin checks. Keys and clicks settle again when it
  opens, keys are ignored while held, and it opens with focus on the card,
  not its Confirm, so a stray `Enter` or `Space` presses nothing; the
  Agent card's focus is in its text box, where `y` is only a letter, so
  keys typed there neither settle nor are ignored while held. The
  page behind is inert while it's open, and only the latest ask opens.
  Without the script, the link goes to the confirm page itself.
- An approval is picked, never taken from a URL. The PR page's verdict
  form is a post, behind the token and origin checks, and sent with
  Approve it gets a random pick id, kept in memory with the run and a
  short expiry, which the preview URL and its confirm form carry. The
  preview records the payload it shows under the pick, and the confirm
  uses the pick up, posting only that payload from that run. So a preview
  URL from history, or a second `y`, can't post an approval again: an
  unknown, used or expired pick is refused, before anything is loaded,
  with a note to pick Approve again on the PR page.
- Responses forbid framing, so another page can't trick you into clicking
  Confirm, and the CSP allows only the dashboard's own scripts. The
  referrer policy is `same-origin`: links out to GitHub carry no
  referrer, and the dashboard's own requests keep theirs.
- Worktrees come from untrusted code. The agent never builds or runs that code
  unless a profile explicitly allows it (the default is off).
- The agent can read your configured checkouts and `runner.read_paths`,
  including untracked files such as `.env`. Injected PR text could get it
  to copy their contents into a draft; nothing leaves the machine unless you
  post that draft, so read drafts before posting them.

## Development

### Tooling

- `rust-toolchain.toml` pins Rust (with `clippy` and `rustfmt`). mise reads
  it as Rust's version file, and rustup reads it directly, so local builds,
  `check` CI and release builds use the same toolchain.
- `mise.toml` pins every other tool: `cargo-deny`, `cargo-nextest`,
  `cargo-llvm-cov`, `cargo-insta`, `dist`.
- mise is also the task runner:

  | Task | Runs |
  |------|------|
  | `mise run fmt` | `cargo fmt --check` |
  | `mise run lint` | `cargo clippy --workspace --all-targets -- -D warnings` |
  | `mise run deny` | `cargo deny check` |
  | `mise run dist` | `dist generate --check`: `release.yml` matches `dist-workspace.toml` |
  | `mise run test` | `cargo nextest run --workspace` plus doctests |
  | `mise run coverage` | `cargo llvm-cov nextest`. Informational, not part of `check` |
  | `mise run check` | fmt, lint, deny, dist, test |

- **Every change passes `mise run check`.** Locally this is a rule in
  `CLAUDE.md`, because jj has no commit hooks. In CI, a GitHub Actions
  workflow in `quodlibetor/sanic-review` runs the same `mise run check` on
  each push and PR. Its actions are pinned to full commit SHAs, with the
  release in a trailing comment: `uses: owner/repo@<sha> # vX.Y.Z`.

### Dependency updates

The hosted Renovate app, configured by `.github/renovate.json5`, opens PRs
weekly and never automerges. Minor and patch updates share one PR, apart
from the two groups below; each major gets its own.

- Crates: the requirements in `[workspace.dependencies]`. A requirement is
  raised only when a release falls outside it; otherwise just `Cargo.lock`
  moves. Weekly lock file maintenance refreshes the transitive dependencies.
- Actions in `check.yml`, keeping the `@<sha> # vX.Y.Z` form.
- Tools in `mise.toml`.
- The Rust toolchain in `rust-toolchain.toml`, in a PR of its own, since a
  new clippy can fail the lint step.
- The inputs to `release.yml`: the action SHAs and `cargo-dist-version` in
  `dist-workspace.toml`, plus dist in `mise.toml`, grouped into one
  "release workflow" PR. Renovate can't regenerate `release.yml` (hosted
  Renovate runs no commands), and leaves it alone. Instead, on that PR
  `mise run check` fails at the `dist` step until someone checks out the
  branch, runs `dist generate` and pushes; the PR body says so.

### Releases

Pushing a version tag such as `v0.1.0` runs `.github/workflows/release.yml`,
which builds the `sanic-review` binary, creates a GitHub Release, and pushes
a formula to the `quodlibetor/homebrew-tap` tap.

- [dist](https://github.com/axodotdev/cargo-dist) generates the workflow from
  `dist-workspace.toml`. Don't edit `release.yml` by hand: change the config
  and run `dist generate`. `dist plan` shows what a release would build.
- The version comes from `[workspace.package]` in `Cargo.toml`. Push only a
  `vX.Y.Z` tag equal to it, on a commit that carries that version. The
  trigger is dist's default and runs for anything that looks like a version
  (`v0.1.0`, `0.1.0`, `sanic-review-v0.1.0`); a tag for any other version
  fails the plan job and publishes nothing. A suffix such as `-rc.1` makes a
  prerelease, so the workspace version needs the same suffix. On pull
  requests the workflow runs only the plan job.
- Targets are macOS and Linux (glibc), each on arm64 and x86_64. Linux builds
  run on Ubuntu 22.04 so the binaries run on Debian 12, Ubuntu 22.04 and
  newer. The x86_64 macOS binary is cross-compiled on the arm64 macOS runner.
  Runners are pinned in `dist-workspace.toml` rather than following GitHub's
  `-latest` labels.
- Actions in the workflow are pinned to commit SHAs through
  `github-action-commits`, each with its release in a comment. Binaries
  get GitHub artifact attestations, so `gh attestation verify <file> --repo
  quodlibetor/sanic-review` checks a download. `cargo-auditable` is off,
  since dist installs it unpinned.
- The crates stay unpublished. The binary crate opts back in to dist with
  `[package.metadata.dist] dist = true`, since `publish = false` would
  otherwise exclude it.
- The tap job pushes with the `HOMEBREW_TAP_TOKEN` repository secret: a token
  with write access to `quodlibetor/homebrew-tap`'s contents.
- Each archive ships both license files, and the Homebrew formula declares
  the license from `Cargo.toml`.
- macOS binaries aren't signed or notarized.

Installing:

```sh
brew install quodlibetor/tap/sanic-review
# or
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/quodlibetor/sanic-review/releases/latest/download/sanic-review-installer.sh | sh
# or
cargo binstall --git https://github.com/quodlibetor/sanic-review sanic-review
```

Homebrew and the installer script don't set macOS's quarantine flag. A
tarball downloaded with a browser does, and Gatekeeper then refuses the
unsigned binary; clear it with `xattr -d com.apple.quarantine sanic-review`.

### Cargo workspace

```
crates/
  core/      types, config, repo matching, trigger logic
  github/    REST and GraphQL client. Read paths, plus the write paths `web` calls
  store/     SQLite schema, migrations, queries
  runner/    claude invocation, VCS (jj/git) checkouts, fix workspaces
  web/       dashboard and the only caller of GitHub write paths
  cli/       the `sanic-review` binary, UI modes
```

- The crates only serve this repo. They have no public-API stability concerns
  and aren't published (`publish = false`).
- The root `Cargo.toml` declares every dependency, as a plain semver
  requirement, in
  `[workspace.dependencies]`. Member crates write `foo.workspace = true` (or
  `foo = { workspace = true, features = [...] }`) and never state a version.
  The committed `Cargo.lock` does the exact pinning.
- Lints live in `[workspace.lints]`, which each crate inherits with
  `[lints] workspace = true`: `unsafe_code = "forbid"`, a curated set of
  clippy pedantic lints, and `unwrap_used` / `expect_used` denied outside
  tests.
- `Cargo.lock` is committed. `cargo-deny` checks advisories, a license
  allowlist, duplicate versions, and that crates come only from crates.io.
- The code is licensed `MIT OR Apache-2.0` (`LICENSE-MIT`,
  `LICENSE-APACHE`), set in `[workspace.package]` and inherited by every
  crate with `license.workspace = true`. cargo-deny's license allowlist
  covers dependencies only; it skips the unpublished workspace crates
  (`[licenses.private] ignore`).

### Errors

`color-eyre` is used in every crate. `eyre::Result` plus `.wrap_err(...)` at
each boundary gives a chain of context, a backtrace, and a `tracing` SpanTrace
showing which poll, run or PR the error happened under. `color-eyre`'s
sections add suggestions to errors the user can act on (bad config, missing
remote, expired token). Where the caller has to branch on a failure (rate
limited, not modified, auth rejected), that's a return-type enum, not a
downcast.

### Testing

- GitHub, the `claude` runner and the clock sit behind traits, so tests never
  use the network or spend tokens.
- GitHub client integration tests run against `wiremock` with recorded JSON
  fixtures. Since fixtures are hand-written, every GraphQL query is also
  validated offline against GitHub's published schema, vendored in
  `crates/github/schema/` with the command to refresh it, so a field or
  argument GitHub doesn't have fails a test instead of every poll.
- Runner tests use a fake `claude` executable that returns canned structured
  output.
- Debounce and polling tests use `tokio::time::pause()`.
- VCS tests create real jj and git repos in tempdirs. They cover remote
  discovery, workspace and worktree creation and cleanup, and fix amend and
  discard.
- SQLite tests get a fresh database per test, with migrations applied.
- `insta` snapshots cover prompt assembly, GitHub payload previews and
  dashboard HTML.
- The GitHub token is wrapped in a redacting type, so it can't appear in logs
  or `Debug` output.

## Milestones

1. Config, SQLite, GitHub poller and reconcile, event log. `--ui logs` prints
   detected triggers.
2. Scheduler with debounce, runner for `review` on someone else's PR, drafts
   stored.
3. Dashboard: view, edit, accept or reject, submit with payload preview.
4. `reply` and `respond` runs, stale-draft handling, incremental reviews.
5. TUI.
6. Auto-fix.

## Deferred

- A dashboard button to push code, meaning a local fix, as an explicit user
  action. Until it exists, you push fixes with your own tooling.
