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
    let timedOut = false;
    let exited = false;
    let settled = false;
    let timer: NodeJS.Timeout | undefined;
    const finish = (r: CheckResult) => {
      if (settled) return;
      settled = true;
      if (timer) clearTimeout(timer);
      resolve(r);
    };
    const killGroup = () => {
      try {
        process.kill(-child.pid!, 'SIGKILL');
      } catch {
        // already gone
      }
    };
    const add = (b: string) => {
      output = (output + b).slice(-OUTPUT_TAIL);
    };
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', add);
    child.stderr.on('data', add);
    child.on('error', (err) => {
      finish({ cmd, ok: false, code: null, timedOut: false, output: err.message });
    });
    timer = setTimeout(() => {
      if (!exited) timedOut = true;
      killGroup();
    }, timeoutMs);
    let exitCode: number | null = null;
    let graceTimer: NodeJS.Timeout | undefined;
    const done = () => finish({ cmd, ok: exitCode === 0 && !timedOut, code: exitCode, timedOut, output });
    child.on('exit', (code) => {
      exited = true;
      exitCode = code;
      killGroup();
      // grandchildren may hold the pipes open; give 'close' a moment, then stop waiting
      graceTimer = setTimeout(() => {
        child.stdout.destroy();
        child.stderr.destroy();
        done();
      }, 200);
    });
    child.on('close', (code) => {
      exitCode = code;
      if (graceTimer) clearTimeout(graceTimer);
      done();
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
