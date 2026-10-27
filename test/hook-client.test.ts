import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { runDaemon } from '../src/daemon.ts';
import { tmpDir } from './helpers.ts';

const CLIENT = path.resolve(import.meta.dirname, '../bin/amm-hook');
const run = (home: string, event: string, payload: unknown) =>
  spawnSync(process.execPath, [CLIENT, event], { input: JSON.stringify(payload), env: { ...process.env, AMM_HOME: home }, encoding: 'utf8' });

test('without a daemon lock the client exits silently', () => {
  const r = run(tmpDir(), 'PreToolUse', { agent_id: 'a', tool_name: 'Read' });
  assert.deepEqual([r.status, r.stdout], [0, '']);
});

test('main-session events for non-amm tools are not forwarded', () => {
  const home = tmpDir();
  fs.writeFileSync(path.join(home, 'daemon.lock'), JSON.stringify({ pid: process.pid, port: 1, token: 'x' }));
  const r = run(home, 'PreToolUse', { session_id: 's', tool_name: 'Read' });
  assert.deepEqual([r.status, r.stdout], [0, '']);
  assert.equal(fs.existsSync(path.join(home, 'hook-fallback.log')), false);
});

// The daemon lives in this process, so this test must not block the event loop with spawnSync.
function runAsync(home: string, event: string, payload: unknown): Promise<{ status: number | null; stdout: string }> {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [CLIENT, event], { env: { ...process.env, AMM_HOME: home } });
    let stdout = '';
    child.stdout.on('data', (d) => (stdout += d));
    child.on('close', (status) => resolve({ status, stdout }));
    child.stdin.end(JSON.stringify(payload));
  });
}

test('worker events are forwarded and the decision printed', async () => {
  const home = tmpDir();
  const d = (await runDaemon({ home, port: 0 }))!;
  try {
    const r = await runAsync(home, 'PreToolUse', { session_id: 's', cwd: '/', agent_id: 'a', tool_name: 'Read' });
    assert.deepEqual([r.status, r.stdout], [0, '{}']);
  } finally {
    await d.close();
  }
});

test('an unreachable daemon is logged and the action allowed', () => {
  const home = tmpDir();
  fs.writeFileSync(path.join(home, 'daemon.lock'), JSON.stringify({ pid: process.pid, port: 1, token: 'x' }));
  const r = run(home, 'PreToolUse', { agent_id: 'a', tool_name: 'Read' });
  assert.deepEqual([r.status, r.stdout], [0, '']);
  assert.match(fs.readFileSync(path.join(home, 'hook-fallback.log'), 'utf8'), /PreToolUse/);
});

test('a stale lock with a dead pid sends nothing and logs nothing', () => {
  const home = tmpDir();
  fs.writeFileSync(path.join(home, 'daemon.lock'), JSON.stringify({ pid: 2 ** 22 + 12345, port: 1, token: 'x' }));
  const r = run(home, 'PreToolUse', { agent_id: 'a', tool_name: 'Read' });
  assert.deepEqual([r.status, r.stdout], [0, '']);
  assert.equal(fs.existsSync(path.join(home, 'hook-fallback.log')), false);
});
