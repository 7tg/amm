import assert from 'node:assert/strict';
import { test } from 'node:test';
import { isHealthy, readLock, runDaemon } from '../src/daemon.ts';
import { tmpDir } from './helpers.ts';

test('daemon serves health, rejects bad tokens and runs api ops', async () => {
  const home = tmpDir();
  const d = (await runDaemon({ home, port: 0 }))!;
  try {
    const lock = readLock(home)!;
    assert.equal(lock.port, d.port);
    assert.equal(await isHealthy(lock), true);
    const url = `http://127.0.0.1:${d.port}`;
    assert.equal((await fetch(`${url}/api/status`, { method: 'POST', body: '{}' })).status, 401);
    const call = async (op: string, body: unknown) => {
      const r = await fetch(`${url}/api/${op}`, { method: 'POST', headers: { 'x-amm-token': lock.token }, body: JSON.stringify(body) });
      return { status: r.status, body: await r.json() };
    };
    const run = await call('run_start', { repo: home, test_cmd: 'true', base_branch: 'main' });
    assert.match(run.body.result.run_id, /^r[0-9a-f]{6}$/);
    const bad = await call('dispatch', { run_id: 'nope' });
    assert.deepEqual([bad.status, bad.body.error], [400, 'unknown run nope']);
    const hook = await fetch(`${url}/hook/PreToolUse`, { method: 'POST', headers: { 'x-amm-token': lock.token }, body: JSON.stringify({ session_id: 's', cwd: '/', agent_id: 'x', tool_name: 'Read' }) });
    assert.deepEqual(await hook.json(), {});
  } finally {
    await d.close();
  }
  assert.equal(readLock(home), null);
});

test('a second daemon for the same home steps aside', async () => {
  const home = tmpDir();
  const d = (await runDaemon({ home, port: 0 }))!;
  try {
    assert.equal(await runDaemon({ home, port: 0 }), null);
  } finally {
    await d.close();
  }
});
