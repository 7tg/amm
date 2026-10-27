import crypto from 'node:crypto';
import fs from 'node:fs';
import { agentType, type Limits, nextTier, TIER_MAX_TURNS, toolName } from './config.ts';
import { git, worktreeInfo } from './git.ts';
import { parsePlan } from './plan-import.ts';
import { scopesOverlap } from './scope.ts';
import { Semaphores } from './semaphores.ts';
import type { Store } from './store.ts';
import type { AmmEvent, Brief, Report, Run, Session, Tier, Worker, WorkerState } from './types.ts';

const newId = (prefix: string) => prefix + crypto.randomBytes(3).toString('hex');
export const ACTIVE: WorkerState[] = ['starting', 'running', 'checking'];
const NOTABLE = new Set(['done', 'escalate', 'failed', 'blocked', 'lost', 'cancel']);

export interface BriefInput {
  title: string;
  goal: string;
  scope: string[];
  checks: string[];
  tier: Tier;
  maxTurns?: number;
}

export type BriefPatch = Partial<Pick<Brief, 'title' | 'goal' | 'scope' | 'checks' | 'tier' | 'maxTurns'>> & { ready?: boolean };

export interface AgentCall {
  briefId: string;
  subagent_type: string;
  description: string;
  prompt: string;
}

export class Amm {
  readonly store: Store;
  readonly sem: Semaphores;
  readonly limits: Limits;
  readonly now: () => number;

  constructor(store: Store, limits: Limits, now: () => number = Date.now) {
    this.store = store;
    this.limits = limits;
    this.now = now;
    this.sem = new Semaphores(now);
    // Rebuild worker slots after a restart; leases keep their remaining time.
    for (const w of store.all('workers')) {
      if (!ACTIVE.includes(w.state)) continue;
      const extra = w.state === 'checking' ? (this.store.get('briefs', w.briefId)?.checks.length ? limits.gateBudgetMs : 0) : 0;
      const remaining = Math.max(1, w.lastEventAt + limits.leaseMs + extra - now());
      this.sem.tryAcquire('workers', w.id, Infinity, remaining);
    }
  }

  event(kind: string, runId: string | null, workerId: string | null, detail: Record<string, unknown> = {}): void {
    this.store.addEvent({ ts: this.now(), runId, workerId, kind, detail });
  }

  run(runId: string): Run {
    const r = this.store.get('runs', runId);
    if (!r) throw new Error(`unknown run ${runId}`);
    return r;
  }

  brief(briefId: string): Brief {
    const b = this.store.get('briefs', briefId);
    if (!b) throw new Error(`unknown brief ${briefId}`);
    return b;
  }

  async startRun(input: { repo: string; testCmd: string; baseBranch?: string }): Promise<Run> {
    const repo = fs.existsSync(input.repo) ? fs.realpathSync(input.repo) : input.repo;
    const baseBranch = input.baseBranch ?? (await git(repo, ['rev-parse', '--abbrev-ref', 'HEAD'])).stdout.trim();
    const run: Run = {
      id: newId('r'),
      repo,
      baseBranch,
      testCmd: input.testCmd,
      sessionId: null,
      state: 'active',
      createdAt: this.now(),
      statusCursor: 0,
    };
    this.store.put('runs', run);
    this.event('run_start', run.id, null, { repo: run.repo, baseBranch });
    return run;
  }

  private activeRun(runId: string): Run {
    const r = this.run(runId);
    if (r.state !== 'active') throw new Error(`run ${runId} is ${r.state}`);
    return r;
  }

  addBrief(runId: string, input: BriefInput, state: Brief['state'] = 'queued'): Brief {
    this.activeRun(runId);
    const b: Brief = {
      id: newId('b'),
      runId,
      seq: this.store.all('briefs').filter((x) => x.runId === runId).length + 1,
      title: input.title,
      goal: input.goal,
      scope: input.scope,
      checks: input.checks,
      tier: input.tier,
      maxTurns: input.maxTurns ?? TIER_MAX_TURNS[input.tier],
      state,
      attempt: 1,
      notes: [],
      report: null,
    };
    this.store.put('briefs', b);
    this.event('brief_add', runId, null, { briefId: b.id, title: b.title, tier: b.tier, state });
    return b;
  }

  importPlan(runId: string, file: string): Brief[] {
    this.activeRun(runId);
    const tasks = parsePlan(fs.readFileSync(file, 'utf8'));
    if (tasks.length === 0) throw new Error(`no "### Task N:" headings found in ${file}`);
    return tasks.map((t) =>
      this.addBrief(runId, { title: t.title, goal: t.body, scope: t.files, checks: [], tier: 'standard' }, 'draft'),
    );
  }

  updateBrief(briefId: string, patch: BriefPatch): Brief {
    const b = this.brief(briefId);
    if (!['draft', 'queued', 'blocked', 'failed'].includes(b.state)) {
      throw new Error(`brief ${briefId} is ${b.state}; only draft, queued, blocked or failed briefs can change`);
    }
    const fields: BriefPatch = {};
    for (const k of ['title', 'goal', 'scope', 'checks', 'tier', 'maxTurns'] as const) {
      if (patch[k] !== undefined) (fields as Record<string, unknown>)[k] = patch[k];
    }
    const ready = patch.ready;
    const next: Brief = { ...b, ...fields };
    if (fields.tier && fields.maxTurns === undefined) next.maxTurns = TIER_MAX_TURNS[fields.tier];
    if (ready) {
      if (b.state === 'blocked' || b.state === 'failed') {
        next.attempt = b.attempt + 1;
        next.report = null;
      }
      next.state = 'queued';
    }
    if (next.state === 'queued' && next.scope.length === 0) throw new Error(`brief ${briefId} needs a scope before it is queued`);
    this.store.put('briefs', next);
    return next;
  }

  activeWorkers(runId?: string): Worker[] {
    return this.store.all('workers').filter((w) => ACTIVE.includes(w.state) && (!runId || w.runId === runId));
  }

  /** The newest active worker for a brief. */
  workerForBrief(briefId: string): Worker | undefined {
    return this.store
      .all('workers')
      .filter((w) => w.briefId === briefId && ACTIVE.includes(w.state))
      .sort((a, b) => b.startedAt - a.startedAt)[0];
  }

  workerByAgent(agentId: string): Worker | undefined {
    return this.store.all('workers').find((w) => w.agentId === agentId);
  }

  /** Queued briefs of a run that could start now, in order, given free slots and scope overlap. */
  private dispatchable(runId: string): Brief[] {
    const repo = this.run(runId).repo;
    const held = this.activeWorkers()
      .map((w) => this.brief(w.briefId))
      .filter((b) => this.run(b.runId).repo === repo);
    let free = this.limits.maxWorkers - this.sem.holders('workers').length;
    const out: Brief[] = [];
    const queued = this.store
      .all('briefs')
      .filter((b) => b.runId === runId && b.state === 'queued')
      .sort((a, b) => a.seq - b.seq);
    for (const b of queued) {
      if (free <= 0) break;
      if (held.some((h) => scopesOverlap(h.scope, b.scope))) continue;
      out.push(b);
      held.push(b);
      free--;
    }
    return out;
  }

  dispatch(runId: string): AgentCall[] {
    this.activeRun(runId);
    const calls: AgentCall[] = [];
    for (const b of this.dispatchable(runId)) {
      const w = this.newWorker(b);
      if (!this.sem.tryAcquire('workers', w.id, this.limits.maxWorkers, this.limits.leaseMs)) break;
      this.store.put('workers', w);
      this.store.put('briefs', { ...b, state: 'running' });
      this.event('dispatch', runId, w.id, { briefId: b.id, tier: b.tier, attempt: b.attempt });
      calls.push({
        briefId: b.id,
        subagent_type: agentType(b.tier),
        description: `amm ${b.id}: ${b.title}`.slice(0, 60),
        prompt: `amm worker. Your brief token is "${b.id}".`,
      });
    }
    return calls;
  }

  private newWorker(b: Brief): Worker {
    return {
      id: newId('w'),
      briefId: b.id,
      runId: b.runId,
      agentId: null,
      tier: b.tier,
      worktree: null,
      branch: null,
      baseSha: null,
      transcriptPath: null,
      state: 'starting',
      toolCalls: 0,
      sinceProgress: 0,
      lastCmd: null,
      lastCmdFails: 0,
      stopBlocks: 0,
      firedRules: [],
      pendingCorrection: null,
      startedAt: this.now(),
      lastEventAt: this.now(),
    };
  }

  briefText(briefId: string): string {
    const b = this.brief(briefId);
    return [
      `# amm brief ${b.id}: ${b.title}`,
      '',
      '## Goal',
      b.goal,
      '',
      '## Scope (you may only create or edit these paths, relative to your worktree root)',
      ...b.scope.map((s) => `- ${s}`),
      '',
      '## Done checks (all must pass, run from the worktree root)',
      ...(b.checks.length ? b.checks.map((c) => `- \`${c}\``) : ['- none']),
      '',
      ...(b.notes.length ? ['## Notes from earlier attempts', ...b.notes.map((n) => `- ${n}`), ''] : []),
      '## Rules',
      '- Work only inside your current worktree. Never push. Never spawn agents.',
      '- If something outside your scope must change, do not change it: say so in your report.',
      `- When finished, run the done checks, then call ${toolName('amm_report')} with token "${b.id}", then reply with at most 10 lines.`,
      `- If you are stuck, call ${toolName('amm_report')} with status "blocked", explain why, and stop.`,
    ].join('\n');
  }

  status(runId?: string, full = false): string {
    const runs = runId ? [this.run(runId)] : this.store.all('runs').filter((r) => r.state === 'active');
    const out: string[] = [];
    for (const r of runs) {
      const briefs = this.store
        .all('briefs')
        .filter((b) => b.runId === r.id)
        .sort((a, b) => a.seq - b.seq);
      const count = (s: Brief['state']) => briefs.filter((b) => b.state === s).length;
      out.push(
        `run ${r.id}: ${briefs.length} briefs · ${count('queued')} queued · ${count('running')} running · ${count('done')} done · ${count('blocked')} blocked · ${count('failed')} failed`,
      );
      if (full) {
        for (const b of briefs) {
          const w = this.workerForBrief(b.id);
          out.push(`  ${b.id} [${b.state}] ${b.tier} a${b.attempt} ${b.title}${w ? ` · ${w.toolCalls} calls` : ''}`);
        }
      }
      if (runId) {
        const fresh = this.store.events({ runId: r.id, afterId: r.statusCursor, kinds: [...NOTABLE], limit: 10000 });
        for (const e of fresh) out.push(`  ${describe(e)}`);
        const last = this.store.lastEventId(r.id);
        if (last > r.statusCursor) this.store.put('runs', { ...this.run(r.id), statusCursor: last });
      }
      if (r.state === 'active' && this.dispatchable(r.id).length > 0) {
        out.push('  → queued briefs and free slots: call amm_dispatch');
      }
      if (briefs.length > 0 && briefs.every((b) => b.state === 'done' || b.state === 'cancelled')) {
        out.push('  → all briefs done');
      }
    }
    return out.join('\n') || 'no active runs';
  }
  /** Re-reads a worker so callers never write back a stale copy. */
  private fresh(w: Worker): Worker {
    return this.store.get('workers', w.id)!;
  }

  private putWorker(w: Worker, patch: Partial<Worker>): Worker {
    const next = { ...this.fresh(w), ...patch };
    this.store.put('workers', next);
    return next;
  }

  /** Writes the patch only if the worker is still in one of the `from` states. */
  private transition(w: Worker, from: WorkerState[], patch: Partial<Worker>): Worker | null {
    const cur = this.fresh(w);
    if (!from.includes(cur.state)) return null;
    const next = { ...cur, ...patch };
    this.store.put('workers', next);
    return next;
  }

  async bind(briefId: string, agentId: string, cwd: string): Promise<{ worker: Worker } | { error: string }> {
    const existing = this.workerByAgent(agentId);
    if (existing) return { worker: existing };
    const w = this.workerForBrief(briefId);
    if (!w || w.agentId) return { error: `brief ${briefId} has no unbound worker` };
    // Claim synchronously, before any await, so concurrent binds cannot both win.
    const claimed = this.transition(w, ['starting'], { agentId, worktree: cwd, state: 'running', lastEventAt: this.now() });
    if (!claimed) return { error: `brief ${briefId} has no unbound worker` };
    this.sem.tryAcquire('workers', w.id, Infinity, this.limits.leaseMs);
    const refuse = (error: string) => {
      this.putWorker(w, { agentId: null, worktree: null, state: 'starting' });
      this.event('bind_refused', w.runId, w.id, { briefId, agentId, cwd, error });
      return { error };
    };
    try {
      const repo = await worktreeInfo(this.run(w.runId).repo);
      const info = await worktreeInfo(cwd);
      if (info.toplevel === repo.toplevel) return refuse('this is the main checkout, not a worktree');
      if (info.commonDir !== repo.commonDir) return refuse('this worktree belongs to a different repository');
      if (!info.branch?.startsWith('worktree-agent-')) return refuse(`branch ${info.branch ?? '(detached)'} is not a worktree-agent-* branch`);
      const mb = await git(info.toplevel, ['merge-base', 'HEAD', repo.head]);
      if (mb.code !== 0 || !mb.stdout.trim()) return refuse('could not determine the base commit');
      this.event('bind', w.runId, w.id, { briefId, agentId, worktree: info.toplevel });
      return { worker: this.putWorker(w, { worktree: info.toplevel, branch: info.branch, baseSha: mb.stdout.trim() }) };
    } catch (e) {
      return refuse((e as Error).message.split('\n')[0]);
    }
  }

  touchWorker(w: Worker): Worker {
    if (ACTIVE.includes(this.fresh(w).state)) this.sem.tryAcquire('workers', w.id, Infinity, this.limits.leaseMs);
    return this.putWorker(w, { lastEventAt: this.now() });
  }

  updateCounters(w: Worker, c: Pick<Worker, 'toolCalls' | 'sinceProgress' | 'lastCmd' | 'lastCmdFails' | 'firedRules'>): Worker {
    return this.putWorker(w, {
      toolCalls: c.toolCalls,
      sinceProgress: c.sinceProgress,
      lastCmd: c.lastCmd,
      lastCmdFails: c.lastCmdFails,
      firedRules: c.firedRules,
    });
  }

  takeCorrection(w: Worker): string | null {
    const msg = this.fresh(w).pendingCorrection;
    if (!msg) return null;
    this.putWorker(w, { pendingCorrection: null });
    this.event('correction', w.runId, w.id, { rule: 'manager', message: msg, via: 'deny' });
    return msg;
  }

  markChecking(w: Worker, transcriptPath: string | null): Worker | null {
    const next = this.transition(w, ['running'], { state: 'checking', transcriptPath: transcriptPath ?? this.fresh(w).transcriptPath });
    if (!next) return null;
    const checks = this.brief(w.briefId).checks.length;
    this.sem.tryAcquire('workers', w.id, Infinity, this.limits.leaseMs + (checks ? this.limits.gateBudgetMs : 0));
    return next;
  }

  stopBlocked(w: Worker, problems: string[]): Worker | null {
    const next = this.transition(w, ['checking'], { state: 'running', stopBlocks: this.fresh(w).stopBlocks + 1 });
    if (!next) return null;
    this.sem.tryAcquire('workers', w.id, Infinity, this.limits.leaseMs);
    this.event('gate_fail', w.runId, w.id, { briefId: w.briefId, problems });
    return next;
  }

  report(briefId: string, r: Report): void {
    const b = this.brief(briefId);
    this.store.put('briefs', { ...b, report: r });
    this.event('report', b.runId, this.workerForBrief(b.id)?.id ?? null, { briefId, status: r.status, files: r.files, notes: r.notes });
  }

  private endWorker(w: Worker, from: WorkerState[], state: WorkerState): boolean {
    if (!this.transition(w, from, { state })) return false;
    this.sem.release(w.id);
    return true;
  }

  complete(w: Worker): boolean {
    const b = this.brief(w.briefId);
    if (b.state !== 'running') return false;
    if (!this.endWorker(w, ['checking', 'running'], 'done')) return false;
    this.store.put('briefs', { ...b, state: 'done' });
    this.event('done', b.runId, w.id, { briefId: b.id, title: b.title });
    return true;
  }

  markBlocked(w: Worker): boolean {
    if (!this.endWorker(w, ['running', 'checking'], 'blocked')) return false;
    const b = this.brief(w.briefId);
    this.store.put('briefs', { ...b, state: 'blocked' });
    this.event('blocked', b.runId, w.id, { briefId: b.id, notes: b.report?.notes ?? '' });
    return true;
  }

  escalate(w: Worker, reason: string): boolean {
    if (!this.endWorker(w, ACTIVE, 'escalated')) return false;
    const b = this.brief(w.briefId);
    const tier = nextTier(b.tier);
    const notes = [...b.notes, `attempt ${b.attempt} (${b.tier}) was stopped: ${reason}`];
    if (tier) {
      this.store.put('briefs', { ...b, tier, maxTurns: TIER_MAX_TURNS[tier], attempt: b.attempt + 1, notes, report: null, state: 'queued' });
      this.event('escalate', b.runId, w.id, { briefId: b.id, from: b.tier, to: tier, reason });
    } else {
      this.store.put('briefs', { ...b, notes, state: 'failed' });
      this.event('failed', b.runId, w.id, { briefId: b.id, reason });
    }
    return true;
  }

  nudge(briefId: string, message: string): void {
    const w = this.workerForBrief(briefId);
    if (!w) throw new Error(`brief ${briefId} has no active worker`);
    this.putWorker(w, { pendingCorrection: `amm (from your manager): ${message}` });
    this.event('nudge', w.runId, w.id, { briefId, message });
  }

  /** Cancels one brief, or every unfinished brief of a run when given a run id. */
  cancel(target: string): void {
    const isRun = this.store.get('runs', target) !== undefined;
    const briefs = isRun ? this.store.all('briefs').filter((b) => b.runId === target) : [this.brief(target)];
    if (isRun) this.store.put('runs', { ...this.run(target), state: 'cancelled' });
    for (const b of briefs) {
      if (b.state === 'done' || b.state === 'cancelled') continue;
      const w = this.workerForBrief(b.id);
      if (w) this.endWorker(w, ACTIVE, 'cancelled');
      this.store.put('briefs', { ...this.brief(b.id), state: 'cancelled' });
      this.event('cancel', b.runId, w?.id ?? null, { briefId: b.id });
    }
  }

  /** Marks workers whose lease expired as lost and requeues their briefs. */
  sweep(): void {
    for (const id of this.sem.expire()) {
      const w = this.store.get('workers', id);
      if (!w || !ACTIVE.includes(w.state)) continue;
      this.putWorker(w, { state: 'lost' });
      const b = this.brief(w.briefId);
      if (b.state === 'running') this.store.put('briefs', { ...b, state: 'queued' });
      this.event('lost', w.runId, w.id, { briefId: w.briefId });
    }
  }

  touchSession(id: string, cwd: string): void {
    const s = this.store.get('sessions', id);
    const next: Session = s ? { ...s, lastSeen: this.now() } : { id, cwd, firstSeen: this.now(), lastSeen: this.now() };
    this.store.put('sessions', next);
  }
}

function describe(e: AmmEvent): string {
  const d = e.detail as Record<string, string>;
  switch (e.kind) {
    case 'done':
      return `${d.briefId} done: ${d.title}`;
    case 'escalate':
      return `${d.briefId} escalated ${d.from}→${d.to} (${d.reason}) → call amm_dispatch`;
    case 'failed':
      return `${d.briefId} FAILED at the top tier (${d.reason}) → fix the brief or ask the user`;
    case 'blocked':
      return `${d.briefId} blocked: ${d.notes}`;
    case 'lost':
      return `${d.briefId} worker lost → requeued`;
    default:
      return `${d.briefId ?? ''} ${e.kind}`.trim();
  }
}
