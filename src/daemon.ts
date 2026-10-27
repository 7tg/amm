import crypto from 'node:crypto';
import fs from 'node:fs';
import type http from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { ammHome, DEFAULT_PORT, loadLimits } from './config.ts';
import { createServer } from './http.ts';
import { Amm } from './service.ts';
import { Store } from './store.ts';

export interface Lock {
  pid: number;
  port: number;
  token: string;
}

export const lockPath = (home: string) => path.join(home, 'daemon.lock');

export function readLock(home = ammHome()): Lock | null {
  try {
    return JSON.parse(fs.readFileSync(lockPath(home), 'utf8')) as Lock;
  } catch {
    return null;
  }
}

export async function isHealthy(lock: Lock | null): Promise<boolean> {
  if (!lock) return false;
  try {
    const r = await fetch(`http://127.0.0.1:${lock.port}/health`, { headers: { 'x-amm-token': lock.token }, signal: AbortSignal.timeout(1000) });
    return r.ok && ((await r.json()) as { pid: number }).pid === lock.pid;
  } catch {
    return false;
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Binds the first free port from `start`; returns null if another amm daemon turns out to own this home. */
async function listen(server: http.Server, start: number, home: string): Promise<number | null> {
  for (let port = start; port < start + 20; port++) {
    const ok = await new Promise<boolean>((resolve) => {
      const onError = () => resolve(false);
      server.once('error', onError);
      server.listen(port, '127.0.0.1', () => {
        server.off('error', onError);
        resolve(true);
      });
    });
    if (ok) return (server.address() as AddressInfo).port;
    // The port holder may be an amm daemon that has not written its lock yet.
    for (let i = 0; i < 5; i++) {
      if (await isHealthy(readLock(home))) return null;
      await sleep(200);
    }
  }
  throw new Error(`amm: no free port in ${start}-${start + 19}`);
}

export async function runDaemon(opts: { home?: string; port?: number } = {}): Promise<{ port: number; close: () => Promise<void> } | null> {
  const home = opts.home ?? ammHome();
  fs.mkdirSync(home, { recursive: true, mode: 0o700 });
  fs.chmodSync(home, 0o700);
  if (await isHealthy(readLock(home))) return null;
  const limits = loadLimits();
  const dbPath = path.join(home, 'amm.db');
  if (!fs.existsSync(dbPath)) fs.closeSync(fs.openSync(dbPath, 'a', 0o600));
  const store = new Store(dbPath);
  const amm = new Amm(store, limits);
  const token = crypto.randomBytes(16).toString('hex');
  let lastActivity = Date.now();
  const server = createServer(amm, token, () => {
    lastActivity = Date.now();
  });
  const port = await listen(server, opts.port ?? DEFAULT_PORT, home);
  if (port === null) {
    store.close();
    return null;
  }
  const tmp = `${lockPath(home)}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ pid: process.pid, port, token } satisfies Lock), { mode: 0o600 });
  fs.renameSync(tmp, lockPath(home));

  let closed = false;
  const close = async () => {
    if (closed) return;
    closed = true;
    clearInterval(timer);
    const done = new Promise<void>((r) => server.close(() => r()));
    server.closeAllConnections();
    await done;
    store.close();
    if (readLock(home)?.pid === process.pid && readLock(home)?.port === port) fs.rmSync(lockPath(home), { force: true });
  };
  const timer = setInterval(() => {
    try {
      amm.sweep();
      if (Date.now() - lastActivity > limits.idleShutdownMs && amm.activeWorkers().length === 0) void close();
    } catch (e) {
      console.error('amm: sweep failed:', e);
    }
  }, 30_000);
  timer.unref();
  return { port, close };
}
