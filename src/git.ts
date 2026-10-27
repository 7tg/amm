import { execFile } from 'node:child_process';

export interface GitResult {
  code: number;
  stdout: string;
  stderr: string;
}

export function git(cwd: string, args: string[]): Promise<GitResult> {
  return new Promise((resolve) => {
    execFile('git', args, { cwd, maxBuffer: 16 * 1024 * 1024 }, (err, stdout, stderr) => {
      const code = err ? (typeof err.code === 'number' ? err.code : 1) : 0;
      resolve({ code, stdout, stderr });
    });
  });
}

/** Files that differ from `baseSha` (committed or not) plus untracked files, relative to the repo root.
 * Renames are reported as a deletion plus an addition, so both paths are listed. Throws if git fails. */
export async function changedFiles(cwd: string, baseSha: string): Promise<string[]> {
  const top = await git(cwd, ['rev-parse', '--show-toplevel']);
  if (top.code !== 0) throw new Error(`git rev-parse --show-toplevel failed: ${top.stderr.trim()}`);
  const root = top.stdout.trim();
  const diff = await git(root, ['diff', '--name-only', '-z', '--no-renames', baseSha]);
  if (diff.code !== 0) throw new Error(`git diff failed: ${diff.stderr.trim()}`);
  const untracked = await git(root, ['ls-files', '--others', '--exclude-standard', '-z']);
  if (untracked.code !== 0) throw new Error(`git ls-files failed: ${untracked.stderr.trim()}`);
  const all = [...diff.stdout.split('\0'), ...untracked.stdout.split('\0')].filter(Boolean);
  return [...new Set(all)];
}
