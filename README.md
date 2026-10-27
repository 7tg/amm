## Limits

Bash commands are not confined to the worktree; amm enforces scope on Edit/Write tools and verifies the branch diff at the done gate.

A worker's Bash can read ~/.amm and call the daemon API as the same OS user; amm is a supervisor for cooperative-but-fallible agents, not a sandbox.
