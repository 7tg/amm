---
name: amm-supervise
description: Use while an amm run is active, whenever a background amm worker finishes, or when the user asks how delegated work is going. Keeps the manager's token use minimal by acting only on amm_status deltas.
---

# Supervise an amm run

Each time an amm worker's background task completes, or the user asks for progress:

1. Call `amm_status` once with the run id. Do not read the worker's transcript, output or diff; its
   completion message is not needed.
2. Act only on the lines it returns:
   - `→ call amm_dispatch` or `escalated …`: call `amm_dispatch` and launch every returned Agent call
     exactly as in amm-delegate step 3.
   - `blocked: <notes>`: decide. Either fix the brief (`amm_brief_update` with a better goal or
     scope, `ready: true`, then dispatch) or ask the user if the notes need a decision only they can make.
   - `FAILED at the top tier`: read the brief's notes with `amm_status` `full: true`, then rewrite or
     split the brief, or ask the user.
   - `done:` lines: nothing to do.
   - `→ all briefs done`: tell the user the run is complete, with one line per brief.
3. Otherwise stay silent. Do not narrate waiting.

To correct a running worker yourself, use `amm_nudge` with a one-sentence instruction. To stop work,
use `amm_cancel`.

If you resume in a new session, call `amm_status` with no run id to list active runs.
