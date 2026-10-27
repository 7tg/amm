---
name: delegate
description: Use when the user asks to delegate, parallelize or hand off implementation work to agents, or invokes /amm:delegate. Turns a goal or an existing plan file into amm briefs and dispatches cheaper worker agents in isolated worktrees.
---

# Delegate work through amm

You are the manager. Workers do the work; you write briefs and react to status. Do not read code
or diffs yourself beyond what you need to write good briefs.

## 1. Start a run

Call `amm_run_start` with `repo` = your current working directory (absolute) and `test_cmd` = the
repo's full test command (ask the user only if you cannot find it in package.json, Makefile, etc.).
If a fresh checkout needs dependencies before the tests can run, also pass `setup_cmd` (for example
`npm ci`, `uv sync` or `bundle install`). amm runs it in the run's integration worktree, where every
passed brief is merged into `amm/<run>` and tested. If it fails, fix the command and start again.

## 2. Create briefs

**If a plan file exists** (the user named one, or there is a matching file in `docs/superpowers/plans/`):
use it. Do not re-plan.
1. Call `amm_plan_import` with the absolute path. You get back each task's id, title and scope.
2. For each brief, call `amm_brief_update` with `scope` (fix it if the plan's file list is incomplete),
   `checks` (the test command for that task), `tier`, and `ready: true`.

**Otherwise** split the goal yourself, then call `amm_brief_add` once per brief:
- One brief = one independent unit a worker can finish alone, with disjoint scopes wherever possible
  (briefs with overlapping scopes run one after another).
- `goal`: self-contained. The worker has none of your context: say what to change, why, and any
  conventions it must follow.
- `scope`: globs relative to the repo root, as narrow as possible, including test files.
- `checks`: commands that prove this brief is done, such as the relevant test file.
- `tier`: `light` (Haiku) for mechanical edits, renames, boilerplate; `standard` (Sonnet) for most work;
  `heavy` (Opus) for tricky logic or debugging. amm escalates automatically if a worker struggles.

## 3. Dispatch

Call `amm_dispatch`. For every returned call, invoke the Agent tool with exactly its `subagent_type`,
`description` and `prompt`, and `run_in_background: true`. Send them all in one message. Do not
add anything to the prompt.

Then run `pwd`. If your working directory changed, `cd` back to the repo root.

## 4. Supervise

Follow the supervise skill (/amm:supervise) until the status says `→ run done`.
