import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DEFAULT_LIMITS } from '../src/config.ts';
import { Amm } from '../src/service.ts';
import { addWorktree, initRepo, makeAmm } from './helpers.ts';

async function started(limits = {}) {
  const h = makeAmm(limits);
  const repo = initRepo();
  const run = await h.amm.startRun({ repo, testCmd: 'npm test', baseBranch: 'main' });
  const b = h.amm.addBrief(run.id, { title: 't', goal: 'g', scope: ['src/**'], checks: ['x'], tier: 'light' });
  h.amm.dispatch(run.id);
  return { ...h, run, b, repo };
}

test('cancel while checking: stopBlocked and complete are no-ops', async () => {
  const { amm, b, repo } = await started();
  const wt = addWorktree(repo);
  await amm.bind(b.id, 'a1', wt);
  const w = amm.workerForBrief(b.id)!;
  assert.ok(amm.markChecking(w, null));
  amm.cancel(b.id);
  assert.equal(amm.stopBlocked(w, ['p']), null);
  assert.equal(amm.complete(w), false);
  assert.equal(amm.store.get('workers', w.id)!.state, 'cancelled');
  assert.equal(amm.brief(b.id).state, 'cancelled');
});

test('sweep while checking, then complete is a no-op', async () => {
  const { amm, clock, b, repo } = await started({ leaseMs: 1000 });
  await amm.bind(b.id, 'a1', addWorktree(repo));
  const w = amm.workerForBrief(b.id)!;
  amm.markChecking(w, null);
  clock.advance(1000 + DEFAULT_LIMITS.gateBudgetMs + 1);
  amm.sweep();
  assert.equal(amm.complete(w), false);
  assert.equal(amm.escalate(w, 'x'), false);
  assert.equal(amm.brief(b.id).state, 'queued');
});

test('status scans only notable events past 1000 noise events', async () => {
  const { amm, run } = await setup();
  for (let i = 0; i < 1200; i++) amm.event('tool', run.id, null, {});
  amm.event('done', run.id, null, { briefId: 'bx', title: 'zz' });
  assert.match(amm.status(run.id), /bx done: zz/);
  assert.doesNotMatch(amm.status(run.id), /bx done/);
});

async function setup() {
  const h = makeAmm();
  const run = await h.amm.startRun({ repo: '/repo', testCmd: 't', baseBranch: 'main' });
  return { ...h, run };
}

test('status() without a run id does not advance cursors', async () => {
  const { amm, run } = await setup();
  amm.event('done', run.id, null, { briefId: 'bx', title: 'zz' });
  assert.doesNotMatch(amm.status(), /bx done/);
  assert.match(amm.status(run.id), /bx done: zz/);
  assert.doesNotMatch(amm.status(run.id), /bx done/);
});

test('concurrent binds bind exactly one agent', async () => {
  const { amm, b, repo } = await started();
  const rs = await Promise.all([amm.bind(b.id, 'a1', addWorktree(repo)), amm.bind(b.id, 'a2', addWorktree(repo))]);
  assert.equal(rs.filter((r) => 'worker' in r).length, 1);
  const w = amm.workerForBrief(b.id)!;
  assert.match(w.baseSha!, /^[0-9a-f]{40}$/);
});

test('restart restores remaining lease so stale workers are swept', async () => {
  const { amm, store, clock, b } = await started({ leaseMs: 1000 });
  clock.advance(1001);
  const amm2 = new Amm(store, { ...DEFAULT_LIMITS, leaseMs: 1000 }, clock.now);
  clock.advance(1);
  amm2.sweep();
  assert.equal(store.all('workers')[0].state, 'lost');
  assert.equal(amm2.brief(b.id).state, 'queued');
});

test('expired but unswept lease is revived by touchWorker', async () => {
  const { amm, clock, b } = await started({ leaseMs: 1000 });
  clock.advance(2000);
  amm.touchWorker(amm.workerForBrief(b.id)!);
  amm.sweep();
  assert.equal(amm.workerForBrief(b.id)!.state, 'starting');
});

test('updateBrief ignores non-patchable fields and rejects empty-scope queued', async () => {
  const { amm, run, b } = await started();
  const d = amm.addBrief(run.id, { title: 'd', goal: 'g', scope: ['q/**'], checks: [], tier: 'light' }, 'draft');
  const u = amm.updateBrief(d.id, { title: 'new', state: 'done', runId: 'zzz' } as never);
  assert.equal(u.title, 'new');
  assert.equal(u.state, 'draft');
  assert.equal(u.runId, run.id);
  const q = amm.addBrief(run.id, { title: 'q', goal: 'g', scope: ['p/**'], checks: [], tier: 'light' });
  assert.throws(() => amm.updateBrief(q.id, { scope: [] }), /needs a scope/);
  void b;
});

test('cancelled runs reject new work', async () => {
  const { amm, run } = await setup();
  amm.cancel(run.id);
  assert.throws(() => amm.addBrief(run.id, { title: 'a', goal: 'g', scope: ['a'], checks: [], tier: 'light' }), /is cancelled/);
  assert.throws(() => amm.dispatch(run.id), /is cancelled/);
  assert.throws(() => amm.importPlan(run.id, '/nonexistent'), /is cancelled/);
});

test('startRun stores the real path when it exists', async () => {
  const { amm, dir } = makeAmm();
  const run = await amm.startRun({ repo: dir, testCmd: 't', baseBranch: 'main' });
  assert.equal(run.repo, (await import('node:fs')).realpathSync(dir));
});

test('status hints dispatch only when something can start', async () => {
  const { amm, run } = await setup();
  amm.addBrief(run.id, { title: 'a', goal: 'g', scope: ['src/**'], checks: [], tier: 'light' });
  amm.dispatch(run.id);
  amm.addBrief(run.id, { title: 'b', goal: 'g', scope: ['src/b.ts'], checks: [], tier: 'light' });
  assert.doesNotMatch(amm.status(run.id), /amm_dispatch/);
  amm.addBrief(run.id, { title: 'c', goal: 'g', scope: ['docs/**'], checks: [], tier: 'light' });
  assert.match(amm.status(run.id), /amm_dispatch/);
});

test('dispatch prompt is minimal', async () => {
  const { amm, b } = await started();
  const calls = amm.store.all('workers').length;
  assert.equal(calls, 1);
  const r = await setup();
  const b2 = r.amm.addBrief(r.run.id, { title: 'a', goal: 'g', scope: ['a'], checks: [], tier: 'light' });
  assert.equal(r.amm.dispatch(r.run.id)[0].prompt, `amm worker. Your brief token is "${b2.id}".`);
  void b;
});

test('status without a run id lists summaries only and keeps the cursor', async () => {
  const { amm, run } = await setup();
  const b = amm.addBrief(run.id, { title: 'a', goal: 'g', scope: ['src/**'], checks: [], tier: 'light' });
  amm.dispatch(run.id);
  amm.cancel(b.id);
  const all = amm.status();
  assert.match(all, /^run /);
  assert.doesNotMatch(all, /cancel/);
  assert.match(amm.status(run.id), /cancel/);
});
