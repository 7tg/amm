# amm — agent middle management

A Claude Code plugin that turns the main session into a manager. It writes briefs; cheaper worker
agents (Haiku / Sonnet / Opus) do the work in isolated git worktrees; amm supervises them:

- blocks edits outside a brief's scope, pushes and agent spawns
- watches every tool call (repeated failures, no progress, budget) and corrects or escalates
- refuses "done" until the brief's checks pass, the changes are in scope and a report was filed
- keeps the manager's context small: tiny dispatch prompts, status deltas instead of transcripts

One daemon per machine (`~/.amm`, `127.0.0.1:7878`) holds all state in SQLite.

## Install (local)

```bash
claude --plugin-dir /path/to/amm
```

amm is one Rust binary, `amm` (`daemon` | `shim` | `hook <Event>`). The plugin runs it through
`bin/amm`, a small sh launcher. On first use, and after a version bump, the launcher fetches the
release binary for your platform (Linux or macOS, x86_64 or arm64) into `bin/<target>/` with
`gh release download`. The repo is private, so this needs `gh auth login`. Without `gh`, or
without a matching release, it builds the binary with `cargo build --release` instead (Rust 1.88+) and installs it under
`bin/<host triple>/`. Only `shim`, `daemon` and `--version` install; a hook never downloads, builds
or waits, and while the binary is missing it fails open, so it never blocks the agent.

## Use

Ask the main session to delegate a goal or a plan file ("delegate this plan with amm"), or invoke
`/amm:amm-delegate`.

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
