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

## Use

Ask the main session to delegate a goal or a plan file ("delegate this plan with amm"), or invoke
`/amm:amm-delegate`.

## Develop

```bash
npm install
npm test
npm run build   # regenerates dist/amm.js, which is committed
```

## Limits

Bash commands are not confined to the worktree; amm enforces scope on Edit/Write tools and verifies the branch diff at the done gate.

A worker's Bash can read ~/.amm and call the daemon API as the same OS user; amm is a supervisor for cooperative-but-fallible agents, not a sandbox.
