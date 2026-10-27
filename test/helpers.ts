import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DEFAULT_LIMITS, type Limits } from '../src/config.ts';
import { Amm } from '../src/service.ts';
import { Store } from '../src/store.ts';

export function tmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'amm-test-'));
}

export function makeClock(start = 1_000_000) {
  let t = start;
  return { now: () => t, advance: (ms: number) => { t += ms; } };
}

export function makeAmm(limits: Partial<Limits> = {}) {
  const dir = tmpDir();
  const clock = makeClock();
  const store = new Store(path.join(dir, 'amm.db'));
  const amm = new Amm(store, { ...DEFAULT_LIMITS, ...limits }, clock.now);
  return { amm, store, clock, dir };
}

/** A git repo with one commit, standing in for a worker's worktree. */
export function initRepo(files: Record<string, string> = { 'README.md': 'hi\n' }): string {
  const dir = tmpDir();
  const git = (...args: string[]) => execFileSync('git', args, { cwd: dir, stdio: 'pipe' });
  git('init', '-q', '-b', 'main');
  fs.writeFileSync(path.join(dir, '.gitignore'), '.claude/\n');
  for (const [name, content] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(dir, name)), { recursive: true });
    fs.writeFileSync(path.join(dir, name), content);
  }
  git('add', '.');
  git('-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-qm', 'init');
  return fs.realpathSync(dir);
}

/** A real git worktree of `repo` on branch worktree-agent-X, like Claude Code creates for subagents. */
export function addWorktree(repo: string, name = `x${Math.random().toString(16).slice(2, 8)}`): string {
  const dir = path.join(repo, '.claude', 'worktrees', `agent-${name}`);
  execFileSync('git', ['worktree', 'add', '-q', '-b', `worktree-agent-${name}`, dir], { cwd: repo, stdio: 'pipe' });
  return fs.realpathSync(dir);
}
