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

/** Files that differ from `baseSha` (committed or not) plus untracked files, relative to the repo root. */
export async function changedFiles(cwd: string, baseSha: string): Promise<string[]> {
  const diff = await git(cwd, ['diff', '--name-only', baseSha]);
  const untracked = await git(cwd, ['ls-files', '--others', '--exclude-standard']);
  const all = [...diff.stdout.split('\n'), ...untracked.stdout.split('\n')].filter(Boolean);
  return [...new Set(all)];
}
