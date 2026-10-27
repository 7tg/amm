import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { runCheck, runChecks } from '../src/checks.ts';
import { changedFiles, git } from '../src/git.ts';

function repo(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'amm-git-'));
  const run = (...a: string[]) => execFileSync('git', a, { cwd: dir, stdio: 'pipe' });
  run('init', '-q', '-b', 'main');
  fs.writeFileSync(path.join(dir, 'a.txt'), 'a\n');
  run('add', '.');
  run('-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-qm', 'init');
  return dir;
}

test('changedFiles lists modified, committed and untracked files since base', async () => {
  const dir = repo();
  const base = (await git(dir, ['rev-parse', 'HEAD'])).stdout.trim();
  fs.writeFileSync(path.join(dir, 'a.txt'), 'changed\n');
  fs.mkdirSync(path.join(dir, 'src'));
  fs.writeFileSync(path.join(dir, 'src', 'new.ts'), 'x\n');
  assert.deepEqual((await changedFiles(dir, base)).sort(), ['a.txt', 'src/new.ts']);
});

test('runCheck reports exit code and output tail', async () => {
  const ok = await runCheck('echo fine', os.tmpdir(), 5_000);
  assert.equal(ok.ok, true);
  assert.match(ok.output, /fine/);
  const bad = await runCheck('echo broken >&2; exit 3', os.tmpdir(), 5_000);
  assert.deepEqual([bad.ok, bad.code], [false, 3]);
  assert.match(bad.output, /broken/);
});

test('runCheck kills commands that exceed the timeout', async () => {
  const r = await runCheck('sleep 5', os.tmpdir(), 200);
  assert.deepEqual([r.ok, r.timedOut], [false, true]);
});

test('runChecks stops at the first failure', async () => {
  const rs = await runChecks(['true', 'false', 'echo never'], os.tmpdir(), 5_000);
  assert.deepEqual(rs.map((r) => r.ok), [true, false]);
});
