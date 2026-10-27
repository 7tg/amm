# Dashboard contract

The daemon serves a read-only dashboard on its own port (default `127.0.0.1:7878`).
The server (`src/dashboard.rs`) and the page (`assets/dashboard/*`) are built
separately against this file. Change it only together with both sides.

## Auth

- `amm dashboard` prints `http://127.0.0.1:<port>/?token=<token>` (token from the daemon lock).
- `GET /?token=<token>` with a valid token sets the cookie
  `amm_token=<token>; HttpOnly; SameSite=Strict; Path=/` and answers `303 See Other` to `/`.
- Every dashboard route accepts the token from the `amm_token` cookie **or** the
  `x-amm-token` header. The Host check is unchanged. Without a valid token: `401`.
- The page never sees the token: it relies on the cookie being sent by the browser.

## Routes

| Route | Response |
|---|---|
| `GET /` | `index.html` (`text/html; charset=utf-8`) |
| `GET /assets/app.js` | `application/javascript; charset=utf-8` |
| `GET /assets/style.css` | `text/css; charset=utf-8` |
| `GET /dash/state` | JSON `State` below |
| `GET /dash/worker/{id}` | JSON `WorkerDetail` below, `404` for an unknown id |
| `GET /dash/events` | Server-Sent Events, below |

All three asset files are embedded in the binary with `include_str!`; the page loads
nothing from the network (no CDNs, no fonts).

## JSON shapes (camelCase, times are epoch milliseconds)

```ts
type State = {
  now: number;
  sessions: { id: string; cwd: string; lastSeen: number; costUsd: number }[];
  runs: Run[];                       // newest first
  totals: { workersUsd: number; managersUsd: number };
};

type Run = {
  id: string; repo: string; baseBranch: string;
  state: "active" | "done" | "cancelled" | "failed";
  integrationBranch: string; merges: number; lastTestOk: boolean | null;
  createdAt: number; costUsd: number;   // sum of its workers
  briefs: Brief[];                       // by seq
};

type Brief = {
  id: string; seq: number; title: string; tier: "light" | "standard" | "heavy";
  state: "draft" | "queued" | "running" | "done" | "merging" | "merged"
       | "blocked" | "failed" | "cancelled";
  attempt: number;
  workers: Worker[];                     // oldest first; the last one is current
};

type Worker = {
  id: string; agentId: string | null; tier: string; model: string | null;
  state: "starting" | "running" | "checking" | "done" | "blocked" | "lost"
       | "escalated" | "cancelled";
  branch: string | null; toolCalls: number; sinceProgress: number;
  firedRules: string[]; startedAt: number; lastEventAt: number;
  tokens: { input: number; output: number; cacheRead: number; cacheWrite: number };
  costUsd: number;
};

type WorkerDetail = {
  worker: Worker;
  brief: { id: string; title: string; goal: string; scope: string[]; checks: string[];
           notes: string[]; report: { status: string; files: string[]; notes: string } | null };
  events: { id: number; ts: number; kind: string; detail: object }[];   // this worker, oldest first, max 500
};
```

## Events stream

`GET /dash/events` sends `retry: 2000` first, then one SSE message per new store event:

```
event: amm
id: <event id>
data: {"id":..,"ts":..,"runId":..,"workerId":..,"kind":"..","detail":{..}}
```

It honours `Last-Event-ID` (resume after that id); without it, it starts at the
newest event. A `: ping` comment every 15 s keeps proxies open. The page refetches
`/dash/state` (debounced, at most every 500 ms) when an event arrives, and refetches
an open worker panel when the event's `workerId` matches.

## Page

One screen, no build step, vanilla JS, light and dark theme via `prefers-color-scheme`:
- header: totals (workers vs managers USD) and a live/disconnected indicator
- sessions list
- per run: a board with columns queued, running (running/checking), done (done/merging),
  merged, needs attention (blocked/failed/cancelled); one card per brief with tier,
  attempt, current worker's tool calls, time since last event and fired rules as badges
- clicking a card opens a side panel with `WorkerDetail`: goal, scope, checks, notes,
  report, and an event timeline (tool calls, corrections, denies, gate results, merges)
- read-only: no buttons that change state
