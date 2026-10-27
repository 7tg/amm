import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import path from 'node:path';
import { test } from 'node:test';
import { readLock } from '../src/daemon.ts';
import { tmpDir } from './helpers.ts';

const BUNDLE = path.resolve(import.meta.dirname, '../dist/amm.js');

function rpc(child: ReturnType<typeof spawn>) {
  let buf = '';
  const waiting = new Map<number, (v: any) => void>();
  child.stdout!.on('data', (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const msg = JSON.parse(buf.slice(0, i));
      buf = buf.slice(i + 1);
      waiting.get(msg.id)?.(msg);
    }
  });
  let id = 0;
  return (method: string, params: unknown = {}) =>
    new Promise<any>((resolve) => {
      waiting.set(++id, resolve);
      child.stdin!.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
}

test('shim starts the daemon and exposes amm tools over MCP', async () => {
  const home = tmpDir();
  const env = { ...process.env, AMM_HOME: home, AMM_PORT: '0' };
  const child = spawn(process.execPath, [BUNDLE, 'shim'], { env, stdio: ['pipe', 'pipe', 'inherit'] });
  const call = rpc(child);
  try {
    await call('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 't', version: '0' } });
    child.stdin!.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
    const tools = (await call('tools/list')).result.tools.map((t: { name: string }) => t.name);
    for (const t of ['amm_run_start', 'amm_brief_add', 'amm_plan_import', 'amm_brief_update', 'amm_dispatch', 'amm_brief_get', 'amm_report', 'amm_status', 'amm_nudge', 'amm_cancel']) {
      assert.ok(tools.includes(t), t);
    }
    const res = await call('tools/call', { name: 'amm_run_start', arguments: { repo: home, test_cmd: 'true', base_branch: 'main' } });
    assert.match(res.result.content[0].text, /"run_id":"r[0-9a-f]{6}"/);
    assert.ok(readLock(home));
  } finally {
    child.kill();
    const lock = readLock(home);
    if (lock) process.kill(lock.pid);
  }
});
