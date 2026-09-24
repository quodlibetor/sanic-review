# sanic-review

Review things really really fast, if you consider AI reviewing things review.

sanic-review watches GitHub as you: review requests, pushes, replies,
approvals. When someone requests your review, pushes to a PR you've reviewed,
or marks a draft ready, it checks the PR out and runs a read-only `claude -p`
agent over it. The agent drafts review comments. You
read them, edit them, accept or reject them, and then, if you click the
button, they're posted as a normal GitHub review under your name.

Nothing is posted until you click.

<!-- screenshot: the dashboard index -->

## What it isn't

- **It isn't a substitute for reading the code.** It's a way to start a
  review with a pile of opinions already written down. Some of them will be
  wrong, confidently. You're the one posting them.
- **It isn't free.** Every review spends your Claude tokens, and polling
  spends your GitHub rate limit. `serve --manual-reviews` holds every review
  until you start it, if you'd rather look first.
- **It only drafts reviews of other people's PRs, for now.** Replies in
  threads you're in, and comments on your own PRs, show up as things to
  answer, but it doesn't draft answers or fixes for them yet.

## Safety model

As implemented, not as hoped:

- **Nothing is posted or pushed without an explicit action from you**: a
  click in the dashboard, or a command you run. The only code that writes to
  GitHub is the dashboard's submit path, and it shows you every request it
  will send, with its exact body, before you confirm.
- **Agents get read-only tools and no GitHub credentials.** Reviews run
  `claude -p` with only Read, Grep and Glob, `--restricted` to the checkout
  and the directories you configure, and no MCP servers. Environment
  variables that look like GitHub tokens are removed before it starts. PR
  text is fenced as data in the prompt, but prompt injection is still
  possible; the mitigation is that the worst the agent can do is write a bad
  draft, which you then read.
- **The agent can read what you let it read.** That includes the local
  checkouts in your config and their untracked files (`.env`, say). A
  poisoned PR could get it to quote one into a draft. Read drafts before
  posting them.
- **The dashboard is localhost-only.** It binds `127.0.0.1`, checks the
  `Host` header, and requires a token generated at startup on every
  state-changing request. When the browser says where a request came from
  (`Origin`, `Sec-Fetch-Site`), it must be the dashboard itself. Approvals must
  be picked on the PR page, never taken from a URL.

The details are in [docs/DESIGN.md](docs/DESIGN.md#security).

## Install

Releases are built by [dist](https://github.com/axodotdev/cargo-dist) when a
version tag is pushed, for macOS and Linux (glibc) on arm64 and x86_64.

Homebrew:

```sh
brew install quodlibetor/tap/sanic-review
```

Installer script, from the latest GitHub release:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/quodlibetor/sanic-review/releases/latest/download/sanic-review-installer.sh | sh
```

Or download a tarball from the
[releases page](https://github.com/quodlibetor/sanic-review/releases). The
macOS binaries aren't signed; a tarball downloaded with a browser gets the
quarantine flag, which you can clear with
`xattr -d com.apple.quarantine sanic-review`.

From source:

```sh
cargo install --locked --git https://github.com/quodlibetor/sanic-review sanic-review
```

## Quick start

You need:

- the `claude` CLI (Claude Code) on your `PATH`, logged in;
- a GitHub token: `GITHUB_TOKEN` if it's set, otherwise whatever
  `gh auth token` prints. Team review requests need the `read:org` scope.
- `git`, and your usual git credential helper for private repos.

Then:

```sh
sanic-review setup
```

This asks which of your teams' review requests count, scans a directory for
local checkouts to watch, suggests orgs, and asks for a default model. It
shows a diff of the config and writes it only if you say yes. You can run it
again later; it keeps what it doesn't manage.

```sh
sanic-review serve --ui tui
```

`serve` runs in the foreground: it polls GitHub, runs reviews and serves the
dashboard, logging its URL on startup (`o` in the TUI opens it). Its options:

```
--ui <logs|tui>        log lines (the default), or a terminal UI
--port <PORT>          dashboard port, bound on 127.0.0.1
--config <CONFIG>      defaults to ~/.config/sanic-review/config.toml
--data-dir <DATA_DIR>  defaults to ~/.local/share/sanic-review
--manual-reviews       queue reviews, but run only the ones you start
```

The first poll records what's already there without reacting to it, so it
won't review your whole backlog at once. Standing review requests do count.

Other commands:

```sh
sanic-review review <PR url>     # ask the running serve to review a PR now
sanic-review archive <PR url>    # stop reviewing it automatically (unarchive undoes it)
sanic-review chat <PR url>       # ask the agent about its review; see --help
```

## A short tour

**The index** lists "Reviews you owe" and "Your PRs", grouped by what they
want from you. Reviews you owe are **Needs you** (drafts to decide on,
comments to answer, a review you haven't looked at, a failed or held run),
**In flight** and **Nothing to do now**. Your PRs are **Needs you**, **Ready** and **Waiting on reviewers**.
Each row leads with its one next thing and says who has reviewed it and how.
Only PRs with recent activity are shown; a line under each list says how many
older ones are hidden and lets you widen the window.

**Drafts.** A PR page shows the latest review's summary and comments, each
with the diff lines around it. Click a draft (or press `e`) to edit it; `y`
accepts, `n` rejects, `u` undoes. Pick a verdict, preview exactly what will
be sent, and confirm. Drafts on lines that aren't in the diff go into the
review body instead of being dropped.

**Regenerate with instructions.** "Agent…" resumes the review's agent session
with your instruction ("drop the nits", "you misread the locking, look
again") and produces a new run with revised drafts. The agent is told to
keep drafts you accepted or edited word for word unless you ask otherwise,
and not to bring back ones you rejected. The original run is left alone. It's
refused once the PR has new commits; start a fresh review then.

**Existing threads.** When a draft lands on lines someone has already
commented on, the draft says so and shows the thread. Instead of posting a
duplicate, you can 👍 a comment in that thread, post the draft as a reply
there, or post it separately anyway.

**Files changed.** `f` switches a PR page to the whole diff the run reviewed,
laid out like GitHub's Files changed tab, with the drafts and existing
threads inline, syntax highlighting, unified or split (`s`), per-file Viewed
boxes, and expandable context read from the local mirror.

**Profiles** decide how PRs get reviewed. Each profile matches repos (by
org, `owner/name` or local checkout, optionally narrowed by path globs) and
brings its own instruction files, skill directories and model. The most
specific match wins.

**Archive and ignore.** `x` archives a PR: it gets no more automatic reviews
and is hidden unless you ask to see archived PRs. `i` opens an editor that turns a PR's
title into a `skip_titles` glob (say, `build(deps)*`), previews which reviews
it would skip, and adds it to your config.

**The TUI** (`serve --ui tui`) is a summary, not an editor: reviews you owe,
your PRs, activity and the log. Its keys:

| Key | Does |
|-----|------|
| `j`/`k`, `g`/`G`, Tab/Shift-Tab | move, jump to first/last, switch pane |
| `w` `p` `a` `l` | jump to a pane |
| `o` | open the selected PR (or the index) in the dashboard |
| `r` | start a failed, skipped, archived or held review, after confirming |
| `x` / `X` | archive or unarchive / show archived |
| `i` | ignore PRs by title |
| `c` | chat with the agent that reviewed the selected PR |
| `z` / `Z` | cycle a pane through fit, full screen and collapsed / reset |
| `?` | help |
| `q` | quit `serve`, after listing any running reviews it would cancel |

The dashboard uses the same keys where they make sense in a browser; `?`
lists them.

For how any of this actually works, see [docs/DESIGN.md](docs/DESIGN.md).

## Configuration

`~/.config/sanic-review/config.toml`. `setup` writes it, and hand edits are
fine too: `serve` reloads it when it changes and keeps comments and layout
when it edits it. A minimal one:

```toml
[review_requests]
skip_titles = ["build(deps)*"]    # never auto-review these

[profile.default]
instructions = ["~/.config/sanic-review/instructions/general.md"]
skills = []                       # directories of SKILL.md skills
model = "auto"                    # your claude default; or name a model
repos = [
  { github = "your-org" },
  { repo = "~/src/your-repo", paths = ["/backend/**"] },
]
```

Instruction files are appended to the agent's system prompt, so that's where
your house style and pet peeves go. Every option, and how matching and
precedence work, is in [docs/DESIGN.md](docs/DESIGN.md#configuration).

## Development

```sh
mise run check
```

That's the gate: fmt, clippy with `-D warnings`, cargo-deny, the dist
workflow check, and the tests. CI runs the same thing. Tests never touch the
network or spend tokens; GitHub, `claude` and the clock sit behind fakes.

- Read [docs/DESIGN.md](docs/DESIGN.md) before changing behaviour, and
  update it when behaviour changes.
- [CLAUDE.md](CLAUDE.md) has the conventions (dependencies, errors, logging).
- Nothing may post to GitHub or push except through an explicit user action.
  Don't add code paths that weaken that.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
