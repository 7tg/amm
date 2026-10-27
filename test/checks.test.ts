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
  const t = Date.now();
  const r = await runCheck('sleep 5; echo x', os.tmpdir(), 200);
  assert.ok(Date.now() - t < 1500);
  assert.deepEqual([r.ok, r.timedOut], [false, true]);
});

test('runChecks stops at the first failure', async () => {
  const rs = await runChecks(['true', 'false', 'echo never'], os.tmpdir(), 5_000);
  assert.deepEqual(rs.map((r) => r.ok), [true, false]);
});

function gitIn(dir: string, ...a: string[]) {
  return execFileSync('git', ['-c', 'user.email=t@t', '-c', 'user.name=t', ...a], { cwd: dir, stdio: 'pipe' });
}

test('changedFiles sees committed changes, deletions and renames (both paths)', async () => {
  const dir = repo();
  fs.writeFileSync(path.join(dir, 'old.txt'), 'o\n');
  gitIn(dir, 'add', '.');
  gitIn(dir, 'commit', '-qm', 'more');
  const base = (await git(dir, ['rev-parse', 'HEAD'])).stdout.trim();
  fs.writeFileSync(path.join(dir, 'a.txt'), 'changed\n');
  gitIn(dir, 'commit', '-qam', 'edit');
  gitIn(dir, 'mv', 'old.txt', 'new.txt');
  assert.deepEqual((await changedFiles(dir, base)).sort(), ['a.txt', 'new.txt', 'old.txt']);
  fs.rmSync(path.join(dir, 'a.txt'));
  assert.ok((await changedFiles(dir, base)).includes('a.txt'));
});

test('changedFiles returns raw unicode names and root-relative paths from a subdirectory', async () => {
  const dir = repo();
  const base = (await git(dir, ['rev-parse', 'HEAD'])).stdout.trim();
  fs.mkdirSync(path.join(dir, 'sub'));
  fs.writeFileSync(path.join(dir, 'sub', 'ünï\tq".txt'), 'x\n');
  assert.deepEqual(await changedFiles(path.join(dir, 'sub'), base), ['sub/ünï\tq".txt']);
});

test('changedFiles throws on a bad base sha', async () => {
  const dir = repo();
  await assert.rejects(changedFiles(dir, 'deadbeef'), /deadbeef|unknown|bad/i);
});

const hasSetsid = (() => {
  try {
    execFileSync('sh', ['-c', 'command -v setsid'], { stdio: 'pipe' });
    return true;
  } catch {
    return false;
  }
})();

test('runCheck does not wait for background grandchildren', { skip: !hasSetsid }, async () => {
  const t = Date.now();
  const r = await runCheck('setsid sleep 2 & echo hi', os.tmpdir(), 5_000);
  assert.equal(r.ok, true);
  assert.match(r.output, /hi/);
  assert.ok(Date.now() - t < 1000);
});

test('runCheck with a missing cwd resolves ok:false', async () => {
  const r = await runCheck('true', '/nonexistent-amm-dir', 1_000);
  assert.deepEqual([r.ok, r.code, r.timedOut], [false, null, false]);
  assert.ok(r.output.length > 0);
});

test('runCheck truncates output to the last 4000 chars', async () => {
  const r = await runCheck('head -c 9000 /dev/zero | tr "\\0" a; echo END', os.tmpdir(), 5_000);
  assert.equal(r.output.length, 4000);
  assert.match(r.output, /END\n$/);
});
