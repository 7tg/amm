import assert from 'node:assert/strict';
import net from 'node:net';
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

const raw = (port: number, data: string, end = true) =>
  new Promise<string>((resolve) => {
    const s = net.connect(port, '127.0.0.1', () => (end ? s.end(data) : s.write(data)));
    let out = '';
    s.on('data', (d) => (out += d));
    s.on('close', () => resolve(out));
    s.on('error', () => resolve(out));
    if (!end) setTimeout(() => s.destroy(), 100);
  });

test('malformed requests, aborted bodies and oversized bodies do not kill the daemon', async () => {
  const home = tmpDir();
  const d = (await runDaemon({ home, port: 0 }))!;
  try {
    const lock = readLock(home)!;
    const url = `http://127.0.0.1:${d.port}`;
    const headers = { 'x-amm-token': lock.token };
    const alive = async () => assert.equal((await fetch(`${url}/health`, { headers })).status, 200);
    await raw(d.port, `GET //[ HTTP/1.1\r\nHost: 127.0.0.1:${d.port}\r\n\r\n`);
    await alive();
    await raw(d.port, `POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:${d.port}\r\nx-amm-token: ${lock.token}\r\nContent-Length: 1000\r\n\r\n{"a"`, false);
    await alive();
    const big = await fetch(`${url}/api/status`, { method: 'POST', headers, body: 'x'.repeat(1_100_000) }).then((r) => r.status, () => 413);
    assert.equal(big, 413);
    await alive();
  } finally {
    await d.close();
  }
});

test('health needs the token and foreign Host headers are refused', async () => {
  const home = tmpDir();
  const d = (await runDaemon({ home, port: 0 }))!;
  try {
    const lock = readLock(home)!;
    assert.equal((await fetch(`http://127.0.0.1:${d.port}/health`)).status, 401);
    const out = await raw(d.port, `GET /health HTTP/1.1\r\nHost: evil.example:${d.port}\r\nx-amm-token: ${lock.token}\r\n\r\n`);
    assert.match(out, /^HTTP\/1.1 403/);
  } finally {
    await d.close();
  }
});
