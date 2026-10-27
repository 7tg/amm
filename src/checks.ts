import { spawn } from 'node:child_process';

export interface CheckResult {
  cmd: string;
  ok: boolean;
  code: number | null;
  timedOut: boolean;
  output: string;
}

const OUTPUT_TAIL = 4000;

export function runCheck(cmd: string, cwd: string, timeoutMs: number): Promise<CheckResult> {
  return new Promise((resolve) => {
    const child = spawn('sh', ['-c', cmd], { cwd, stdio: ['ignore', 'pipe', 'pipe'], detached: true });
    let output = '';
    const add = (b: Buffer) => {
      output = (output + b.toString()).slice(-OUTPUT_TAIL);
    };
    child.stdout.on('data', add);
    child.stderr.on('data', add);
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        process.kill(-child.pid!, 'SIGKILL');
      } catch {
        // already exited
      }
    }, timeoutMs);
    child.on('close', (code) => {
      clearTimeout(timer);
      resolve({ cmd, ok: code === 0 && !timedOut, code, timedOut, output });
    });
  });
}

/** Runs checks in order and stops at the first failure. */
export async function runChecks(cmds: string[], cwd: string, timeoutMs: number): Promise<CheckResult[]> {
  const results: CheckResult[] = [];
  for (const cmd of cmds) {
    const r = await runCheck(cmd, cwd, timeoutMs);
    results.push(r);
    if (!r.ok) break;
  }
  return results;
}
