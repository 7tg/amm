import crypto from 'node:crypto';
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

const MAX_BODY = 1_000_000;

function tokenOk(given: string | string[] | undefined, token: string): boolean {
  if (typeof given !== 'string') return false;
  const a = Buffer.from(given);
  const b = Buffer.from(token);
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}

export function createServer(amm: Amm, token: string, onActivity: () => void): http.Server {
  const handle = async (req: http.IncomingMessage, res: http.ServerResponse) => {
    const send = (code: number, body: unknown) => {
      res.writeHead(code, { 'content-type': 'application/json' });
      res.end(JSON.stringify(body));
    };
    try {
      const port = req.socket.localPort;
      if (req.headers.host !== `127.0.0.1:${port}` && req.headers.host !== `localhost:${port}`) return send(403, { error: 'bad host' });
      if (!tokenOk(req.headers['x-amm-token'], token)) return send(401, { error: 'bad token' });
      onActivity();
      const url = new URL(req.url ?? '/', 'http://127.0.0.1');
      if (req.method === 'GET' && url.pathname === '/health') return send(200, { ok: true, pid: process.pid });
      const chunks: Buffer[] = [];
      let size = 0;
      for await (const chunk of req) {
        size += (chunk as Buffer).length;
        if (size > MAX_BODY) {
          res.writeHead(413, { 'content-type': 'application/json', connection: 'close' });
          res.end(JSON.stringify({ error: 'body too large' }), () => req.destroy());
          return;
        }
        chunks.push(chunk as Buffer);
      }
      const raw = Buffer.concat(chunks).toString('utf8');
      const body = raw ? JSON.parse(raw) : {};
      const [, kind, name] = url.pathname.split('/');
      if (req.method === 'POST' && kind === 'api' && Object.hasOwn(OPS, name)) return send(200, { result: await OPS[name](amm, body) });
      if (req.method === 'POST' && kind === 'hook') return send(200, await handleHook(amm, name, body as HookInput));
      send(404, { error: 'not found' });
    } catch (e) {
      if (res.headersSent) res.destroy();
      else send(400, { error: (e as Error).message });
    }
  };
  return http.createServer((req, res) => {
    handle(req, res).catch(() => {
      if (res.headersSent) req.socket.destroy();
      else {
        try {
          res.writeHead(500, { 'content-type': 'application/json' });
          res.end('{"error":"internal"}');
        } catch {
          req.socket.destroy();
        }
      }
    });
  });
}
