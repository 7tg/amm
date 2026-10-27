import http from 'node:http';
import { handleHook, type HookInput } from './hooks.ts';
import type { Amm } from './service.ts';
import type { Brief } from './types.ts';

type Args = Record<string, any>;
const compact = (b: Brief) => ({ id: b.id, title: b.title, state: b.state, tier: b.tier, scope: b.scope });

/** API operations, named after the MCP tools without the "amm_" prefix. Results are kept small on purpose. */
export const OPS: Record<string, (amm: Amm, a: Args) => unknown> = {
  run_start: async (amm, a) => ({ run_id: (await amm.startRun({ repo: a.repo, testCmd: a.test_cmd, baseBranch: a.base_branch })).id }),
  brief_add: (amm, a) =>
    ({ brief_id: amm.addBrief(a.run_id, { title: a.title, goal: a.goal, scope: a.scope, checks: a.checks ?? [], tier: a.tier, maxTurns: a.max_turns }).id }),
  plan_import: (amm, a) => amm.importPlan(a.run_id, a.path).map(compact),
  brief_update: (amm, a) => {
    const { brief_id, max_turns, ...rest } = a;
    return compact(amm.updateBrief(brief_id, { ...rest, ...(max_turns !== undefined ? { maxTurns: max_turns } : {}) }));
  },
  dispatch: (amm, a) => amm.dispatch(a.run_id),
  brief_get: (amm, a) => amm.briefText(a.token),
  report: (amm, a) => {
    amm.report(a.token, { status: a.status, files: a.files ?? [], notes: a.notes ?? '' });
    return 'recorded';
  },
  status: (amm, a) => amm.status(a.run_id, a.full === true),
  nudge: (amm, a) => {
    amm.nudge(a.brief_id, a.message);
    return 'queued; delivered on the worker\'s next tool call';
  },
  cancel: (amm, a) => {
    amm.cancel(a.id);
    return 'cancelled';
  },
};

export function createServer(amm: Amm, token: string, onActivity: () => void): http.Server {
  return http.createServer(async (req, res) => {
    onActivity();
    const send = (code: number, body: unknown) => {
      res.writeHead(code, { 'content-type': 'application/json' });
      res.end(JSON.stringify(body));
    };
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    if (req.method === 'GET' && url.pathname === '/health') return send(200, { ok: true, pid: process.pid });
    if (req.headers['x-amm-token'] !== token) return send(401, { error: 'bad token' });
    let raw = '';
    for await (const chunk of req) raw += chunk;
    try {
      const body = raw ? JSON.parse(raw) : {};
      const [, kind, name] = url.pathname.split('/');
      if (req.method === 'POST' && kind === 'api' && name in OPS) return send(200, { result: await OPS[name](amm, body) });
      if (req.method === 'POST' && kind === 'hook') return send(200, await handleHook(amm, name, body as HookInput));
      send(404, { error: 'not found' });
    } catch (e) {
      send(400, { error: (e as Error).message });
    }
  });
}
