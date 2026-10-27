import { execFile } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

export interface GitResult {
  code: number;
  stdout: string;
  stderr: string;
}

export function git(cwd: string, args: string[]): Promise<GitResult> {
  return new Promise((resolve) => {
    // Neutralise repo-controlled code paths (replace refs, fsmonitor, hooks) a worker could plant.
    const safe = ['-c', 'core.fsmonitor=false', '-c', 'core.hooksPath=/dev/null', ...args];
    const env = { ...process.env, GIT_NO_REPLACE_OBJECTS: '1' };
    execFile('git', safe, { cwd, env, maxBuffer: 16 * 1024 * 1024 }, (err, stdout, stderr) => {
      const code = err ? (typeof err.code === 'number' ? err.code : 1) : 0;
      resolve({ code, stdout, stderr });
    });
  });
}

export interface WorktreeInfo {
  toplevel: string;
  commonDir: string;
  branch: string | null;
  head: string;
}

async function raw(cwd: string, args: string[]): Promise<string> {
  const r = await git(cwd, args);
  if (r.code !== 0) throw new Error(`git ${args[0]} failed: ${r.stderr.trim()}`);
  return r.stdout;
}

const must = async (cwd: string, args: string[]) => (await raw(cwd, args)).trim();

/** Where `cwd` sits: its worktree root, the shared git dir, its branch (null if detached) and HEAD. Throws if git fails. */
export async function worktreeInfo(cwd: string): Promise<WorktreeInfo> {
  const toplevel = fs.realpathSync(await must(cwd, ['rev-parse', '--show-toplevel']));
  const common = await must(toplevel, ['rev-parse', '--git-common-dir']);
  const commonDir = fs.realpathSync(path.resolve(toplevel, common));
  const b = await git(toplevel, ['symbolic-ref', '--short', '-q', 'HEAD']);
  const head = await must(toplevel, ['rev-parse', 'HEAD']);
  return { toplevel, commonDir, branch: b.code === 0 && b.stdout.trim() ? b.stdout.trim() : null, head };
}

/** Files that differ from `baseSha`, relative to the worktree root: committed changes, staged and unstaged
 * changes, and untracked files. Renames are reported as a deletion plus an addition. Throws if git fails. */
export async function changedFiles(cwd: string, baseSha: string): Promise<string[]> {
  const root = await must(cwd, ['rev-parse', '--show-toplevel']);
  const lists = await Promise.all([
    raw(root, ['diff', '--name-only', '-z', '--no-renames', baseSha, 'HEAD']),
    raw(root, ['diff', '--name-only', '-z', '--no-renames', 'HEAD']),
    raw(root, ['ls-files', '--others', '--exclude-standard', '-z']),
  ]);
  return [...new Set(lists.flatMap((l) => l.split('\0')).filter(Boolean))];
}
