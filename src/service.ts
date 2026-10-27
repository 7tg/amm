import crypto from 'node:crypto';
import fs from 'node:fs';
import { agentType, type Limits, TIER_MAX_TURNS, toolName } from './config.ts';
import { git } from './git.ts';
import { parsePlan } from './plan-import.ts';
import { scopesOverlap } from './scope.ts';
import { Semaphores } from './semaphores.ts';
import type { Store } from './store.ts';
import type { AmmEvent, Brief, Run, Tier, Worker, WorkerState } from './types.ts';

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
    // Rebuild worker slots after a restart; leases restart from now.
    for (const w of store.all('workers')) {
      if (ACTIVE.includes(w.state)) this.sem.tryAcquire('workers', w.id, Infinity, limits.leaseMs);
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
    const baseBranch = input.baseBranch ?? (await git(input.repo, ['rev-parse', '--abbrev-ref', 'HEAD'])).stdout.trim();
    const run: Run = {
      id: newId('r'),
      repo: input.repo,
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

  addBrief(runId: string, input: BriefInput, state: Brief['state'] = 'queued'): Brief {
    this.run(runId);
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
    const { ready, ...fields } = patch;
    const next: Brief = { ...b, ...fields };
    if (fields.tier && fields.maxTurns === undefined) next.maxTurns = TIER_MAX_TURNS[fields.tier];
    if (ready) {
      if (next.scope.length === 0) throw new Error(`brief ${briefId} needs a scope before it is ready`);
      if (b.state === 'blocked' || b.state === 'failed') {
        next.attempt = b.attempt + 1;
        next.report = null;
      }
      next.state = 'queued';
    }
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

  dispatch(runId: string): AgentCall[] {
    const repo = this.run(runId).repo;
    const held = this.activeWorkers()
      .map((w) => this.brief(w.briefId))
      .filter((b) => this.run(b.runId).repo === repo);
    const queued = this.store
      .all('briefs')
      .filter((b) => b.runId === runId && b.state === 'queued')
      .sort((a, b) => a.seq - b.seq);
    const calls: AgentCall[] = [];
    for (const b of queued) {
      if (held.some((h) => scopesOverlap(h.scope, b.scope))) continue;
      const w = this.newWorker(b);
      if (!this.sem.tryAcquire('workers', w.id, this.limits.maxWorkers, this.limits.leaseMs)) break;
      this.store.put('workers', w);
      this.store.put('briefs', { ...b, state: 'running' });
      held.push(b);
      this.event('dispatch', runId, w.id, { briefId: b.id, tier: b.tier, attempt: b.attempt });
      calls.push({
        briefId: b.id,
        subagent_type: agentType(b.tier),
        description: `amm ${b.id}: ${b.title}`.slice(0, 60),
        prompt: `amm worker. Load the tool with ToolSearch query "select:${toolName('amm_brief_get')}", then call it with token "${b.id}" and follow the brief.`,
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
      const fresh = this.store.events({ runId: r.id, afterId: r.statusCursor });
      for (const e of fresh) if (NOTABLE.has(e.kind)) out.push(`  ${describe(e)}`);
      if (fresh.length) this.store.put('runs', { ...this.run(r.id), statusCursor: fresh.at(-1)!.id });
      if (count('queued') > 0 && this.sem.holders('workers').length < this.limits.maxWorkers) {
        out.push('  → queued briefs and free slots: call amm_dispatch');
      }
      if (briefs.length > 0 && briefs.every((b) => b.state === 'done' || b.state === 'cancelled')) {
        out.push('  → all briefs done');
      }
    }
    return out.join('\n') || 'no active runs';
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
