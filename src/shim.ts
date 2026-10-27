import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { z } from 'zod';
import { ammHome } from './config.ts';
import { isHealthy, type Lock, readLock } from './daemon.ts';

/** Returns the running daemon's lock, starting a detached daemon first if none is healthy. */
export async function ensureDaemon(home = ammHome()): Promise<Lock> {
  let lock = readLock(home);
  if (lock && (await isHealthy(lock))) return lock;
  fs.mkdirSync(home, { recursive: true, mode: 0o700 });
  const log = fs.openSync(path.join(home, 'daemon.log'), 'a');
  try {
    spawn(process.execPath, [fileURLToPath(import.meta.url), 'daemon'], { detached: true, stdio: ['ignore', log, log], env: process.env }).unref();
  } finally {
    fs.closeSync(log);
  }
  for (let i = 0; i < 50; i++) {
    await new Promise((r) => setTimeout(r, 100));
    lock = readLock(home);
    if (lock && (await isHealthy(lock))) return lock;
  }
  throw new Error(`amm daemon did not start; see ${path.join(home, 'daemon.log')}`);
}

async function callDaemon(op: string, args: Record<string, unknown>): Promise<unknown> {
  for (let attempt = 0; ; attempt++) {
    const lock = await ensureDaemon();
    try {
      const res = await fetch(`http://127.0.0.1:${lock.port}/api/${op}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-amm-token': lock.token },
        body: JSON.stringify(args),
        signal: AbortSignal.timeout(30_000),
      });
      const body = (await res.json()) as { result?: unknown; error?: string };
      if (!res.ok) throw new Error(body.error ?? `http ${res.status}`);
      return body.result;
    } catch (e) {
      // ECONNREFUSED means the request never reached a daemon (it may have just shut down), so one retry is safe.
      if (attempt > 0 || (e as { cause?: { code?: string } }).cause?.code !== 'ECONNREFUSED') throw e;
    }
  }
}

const asText = (v: unknown) => ({ content: [{ type: 'text' as const, text: typeof v === 'string' ? v : JSON.stringify(v) }] });
const tier = z.enum(['light', 'standard', 'heavy']);

export async function runShim(): Promise<void> {
  const server = new McpServer({ name: 'amm', version: '0.1.0' });
  const tool = (name: string, description: string, inputSchema: z.ZodRawShape) =>
    server.registerTool(name, { description, inputSchema }, async (args: Record<string, unknown>) => {
      try {
        return asText(await callDaemon(name.replace(/^amm_/, ''), args));
      } catch (e) {
        return { ...asText(`error: ${(e as Error).message}`), isError: true };
      }
    });

  tool('amm_run_start', 'Start an amm run. Returns run_id.', {
    repo: z.string().describe('Absolute path of the repository (the main session cwd)'),
    test_cmd: z.string().describe('Full test command for the repo'),
    base_branch: z.string().optional(),
  });
  tool('amm_brief_add', 'Add a brief (one isolated unit of work for one worker). Returns brief_id.', {
    run_id: z.string(),
    title: z.string(),
    goal: z.string().describe('What to do and why, self-contained'),
    scope: z.array(z.string()).describe('Globs relative to repo root the worker may change'),
    checks: z.array(z.string()).describe('Commands that must pass for the brief to be done'),
    tier,
    max_turns: z.number().int().optional(),
  });
  tool('amm_plan_import', 'Import "### Task N:" sections of a plan file as draft briefs. Returns id, title, scope per brief.', {
    run_id: z.string(),
    path: z.string().describe('Absolute path of the plan markdown file'),
  });
  tool('amm_brief_update', 'Change a draft/queued/blocked/failed brief. Pass ready=true to queue it.', {
    brief_id: z.string(),
    title: z.string().optional(),
    goal: z.string().optional(),
    scope: z.array(z.string()).optional(),
    checks: z.array(z.string()).optional(),
    tier: tier.optional(),
    max_turns: z.number().int().optional(),
    ready: z.boolean().optional(),
  });
  tool('amm_dispatch', 'Reserve worker slots for queued briefs. Returns the exact Agent calls to make (run each with run_in_background: true).', {
    run_id: z.string(),
  });
  tool('amm_brief_get', 'Worker only: fetch your brief by token.', { token: z.string() });
  tool('amm_report', 'Worker only: report the result of your brief before finishing.', {
    token: z.string(),
    status: z.enum(['done', 'blocked']),
    files: z.array(z.string()).optional(),
    notes: z.string().optional().describe('At most a few lines'),
  });
  tool('amm_status', 'Compact run status: counts plus only what changed since the last call. full=true lists every brief.', {
    run_id: z.string().optional(),
    full: z.boolean().optional(),
  });
  tool('amm_nudge', 'Send a correction to the worker of a brief; delivered on its next tool call.', {
    brief_id: z.string(),
    message: z.string(),
  });
  tool('amm_cancel', 'Cancel a brief, or all unfinished briefs of a run.', { id: z.string().describe('brief id or run id') });

  await server.connect(new StdioServerTransport());
}
