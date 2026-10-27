# amm — agent middle management

A Claude Code plugin that turns the main session into a manager. It writes briefs; cheaper worker
agents (Haiku / Sonnet / Opus) do the work in isolated git worktrees; amm supervises them:

- blocks edits outside a brief's scope, pushes and agent spawns
- watches every tool call (repeated failures, no progress, budget) and corrects or escalates
- refuses "done" until the brief's checks pass, the changes are in scope and a report was filed
- keeps the manager's context small: tiny dispatch prompts, status deltas instead of transcripts

One daemon per machine (`~/.amm`, `127.0.0.1:7878`) holds all state in SQLite.

[![Meet amm: the main Claude Code session writes briefs, cheaper Haiku, Sonnet and Opus workers do the work in isolated git worktrees, amm blocks out-of-scope edits, pushes and agent spawns, corrects or escalates stuck workers, refuses done until the checks pass, merges and tests every brief on an integration branch, shows it all on a live dashboard, and how to install it](https://raw.githubusercontent.com/7tg/amm/master/docs/media/demo.gif)](https://github.com/7tg/amm/blob/master/docs/media/demo.mp4)

<sub>Briefs, workers in worktrees, guardrails, the watchdog, the done gate, integration, the dashboard, then installing it. 54 seconds · 1080p · 60 fps · original soundtrack. [Watch the film](https://github.com/7tg/amm/blob/master/docs/media/demo.mp4).</sub>

## Install

In Claude Code:

```
/plugin marketplace add 7tg/amm
/plugin install amm@amm
```

or from a shell: `claude plugin marketplace add 7tg/amm && claude plugin install amm@amm`.
To try a local checkout instead: `claude --plugin-dir /path/to/amm`.

amm is one Rust binary, `amm` (`daemon` | `shim` | `hook <Event>` | `dashboard`). The plugin runs
it through `bin/amm`, a small sh launcher. On first use, and after a version bump, the launcher
downloads the release binary for your platform (Linux or macOS, x86_64 or arm64) into
`bin/<target>/`, with `gh`, `curl` or `wget`. Without any of them, or without a matching release,
it builds the binary with `cargo build --release` instead (Rust 1.88+) and installs it under
`bin/<host triple>/`. Only `shim`, `daemon`, `dashboard` and `--version` install; a hook never
downloads, builds or waits, and while the binary is missing it fails open, so it never blocks the
agent.

## Use

Ask the main session to delegate a goal or a plan file ("delegate this plan with amm"), or invoke
`/amm:delegate`.

## Dashboard

The daemon also serves a read-only dashboard: runs, briefs as a board, each worker's events,
and token and cost totals for workers and manager sessions. Ask for it with `/amm:dashboard`, or run

```bash
amm dashboard
```

to print `http://127.0.0.1:<port>/?token=<token>` and open it in a browser. The link logs the
browser in with an `HttpOnly` cookie and redirects to `/`; the page then updates live over
Server-Sent Events. With no daemon running, `amm dashboard` prints a hint and exits 1.

## Develop

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release   # target/release/amm; bin/amm installs it on its next shim start
```

Keep the version in `Cargo.toml` equal to the one in `.claude-plugin/plugin.json` (a test checks
this). Pushing a `v<version>` tag publishes the release binaries.

## Limits

Bash commands are not confined to the worktree; amm enforces scope on Edit/Write tools and verifies the branch diff at the done gate.

A worker's Bash can read ~/.amm and call the daemon API as the same OS user; amm is a supervisor for cooperative-but-fallible agents, not a sandbox.

Don't commit to the base branch during a run. A worker's base is the merge base of its worktree with the integration head amm recorded, never a live branch, so commits that reach the base branch after the run started count as the worker's own changes and fail the done gate.

A worker running as the same OS user could edit ~/.amm/amm.db, and the daemon trusts that database when it starts.

amm merges only onto the commit it last recorded for `amm/<run>`. If anything else moves that branch or its worktree (a worker, a test command, a commit by hand), integration stops and the run is marked failed. Put the branch back on the recorded commit (`git reset --hard <sha>` in `.claude/worktrees/amm-<run>`), then call `amm_resume`.

The integration worktree needs git 2.40 or newer (`--attr-source`), and amm refuses a non-empty `.git/info/attributes`.
