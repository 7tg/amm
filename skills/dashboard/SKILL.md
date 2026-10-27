---
name: dashboard
description: Use when the user asks to open, see or get the link to the amm dashboard, the live web page of amm sessions, runs, workers and costs.
---

# Open the amm dashboard

Run the plugin's launcher from this skill's base directory:

```bash
"<this skill's base directory>/../../bin/amm" dashboard
```

It prints a `http://127.0.0.1:<port>/?token=…` URL. Give the user that URL to open in a browser:
it logs them in with a cookie and shows every session and run on this machine.

If it says no daemon is running, call any amm tool (for example `amm_status`) to start it, then
run the command again.
