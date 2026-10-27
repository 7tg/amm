---
name: supervise
description: Use while an amm run is active, whenever a background amm worker finishes, or when the user asks how delegated work is going. Keeps the manager's token use minimal by acting only on amm_status deltas.
---

# Supervise an amm run

Each time an amm worker's background task completes, or the user asks for progress:

1. Call `amm_status` once with the run id. Do not read the worker's transcript, output or diff; its
   completion message is not needed.
2. Act only on the lines it returns:
   - `→ call amm_dispatch` or `escalated …`: call `amm_dispatch` and launch every returned Agent call
     exactly as in the delegate skill, step 3.
   - `blocked: <notes>`: decide. Either fix the brief (`amm_brief_update` with a better goal or
     scope, `ready: true`, then dispatch) or ask the user if the notes need a decision only they can make.
   - `FAILED at the top tier`: read the brief's notes with `amm_status` `full: true`, then rewrite or
     split the brief, or ask the user.
   - `done:` and `merged:` lines, `→ integrating (k waiting)`, `could not be merged yet`: nothing to do;
     amm merges passed briefs into `amm/<run>` in the background and tests after each merge.
   - `integration failed (…) → call amm_dispatch` or `conflict on <files> → call amm_dispatch`: the
     brief was requeued with the failure in its notes. Call `amm_dispatch` and launch the calls.
   - `→ fix the brief or ask the user` (an integration failure amm gave up on, or a merge that would
     change files `outside its scope`): read the brief with `amm_status` `full: true`, then fix its
     scope or goal (`amm_brief_update` with `ready: true`) or ask the user.
   - `integration stopped (…)`: something other than amm moved `amm/<run>`. Tell the user. Once the
     branch is back on the commit amm recorded (`git reset --hard <sha>` in
     `.claude/worktrees/amm-<run>`, the user's call), call `amm_resume`.
   - `→ run done: amm/<run> (N merges, tests pass)`: tell the user the branch name and the number of
     merges, and that amm never pushes and never touches their base branch: merging or pushing
     `amm/<run>` is up to them.
   - If integration seems stalled (briefs stay done with no news), call `amm_integrate` with the run id.
3. Otherwise stay silent. Do not narrate waiting.

To correct a running worker yourself, use `amm_nudge` with a one-sentence instruction. To stop work,
use `amm_cancel`.

If you resume in a new session, call `amm_status` with no run id to list active runs.
