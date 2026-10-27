import assert from 'node:assert/strict';
import { test } from 'node:test';
import { initRepo, makeAmm } from './helpers.ts';

async function started(limits = {}) {
  const h = makeAmm(limits);
  const run = await h.amm.startRun({ repo: '/repo', testCmd: 'npm test', baseBranch: 'main' });
  const b = h.amm.addBrief(run.id, { title: 't', goal: 'g', scope: ['src/**'], checks: [], tier: 'light' });
  h.amm.dispatch(run.id);
  return { ...h, run, b };
}

test('bind attaches an agent and worktree to the starting worker', async () => {
  const { amm, b } = await started();
  const repo = initRepo();
  const w = await amm.bind(b.id, 'agent-1', repo);
  assert.equal(w!.state, 'running');
  assert.equal(w!.worktree, repo);
  assert.match(w!.baseSha!, /^[0-9a-f]{40}$/);
  assert.equal(amm.workerByAgent('agent-1')!.id, w!.id);
  assert.equal((await amm.bind(b.id, 'agent-2', repo)), undefined);
});

test('escalate climbs light → standard → heavy, then fails the brief', async () => {
  const { amm, run, b } = await started();
  for (const tier of ['standard', 'heavy']) {
    amm.escalate(amm.workerForBrief(b.id)!, 'stuck');
    assert.equal(amm.brief(b.id).tier, tier);
    assert.equal(amm.brief(b.id).state, 'queued');
    amm.dispatch(run.id);
  }
  amm.escalate(amm.workerForBrief(b.id)!, 'stuck');
  assert.equal(amm.brief(b.id).state, 'failed');
  assert.equal(amm.brief(b.id).attempt, 3);
  assert.equal(amm.brief(b.id).notes.length, 3);
  assert.equal(amm.sem.holders('workers').length, 0);
});

test('complete, block and cancel end the worker and free its slot', async () => {
  const { amm, run, b } = await started();
  await amm.bind(b.id, 'agent-1', initRepo());
  assert.equal(amm.complete(amm.workerForBrief(b.id)!), true);
  assert.equal(amm.brief(b.id).state, 'done');
  const b2 = amm.addBrief(run.id, { title: 'u', goal: 'g', scope: ['lib/**'], checks: [], tier: 'light' });
  amm.dispatch(run.id);
  await amm.bind(b2.id, 'agent-2', initRepo());
  amm.report(b2.id, { status: 'blocked', files: [], notes: 'needs api change' });
  amm.markBlocked(amm.workerForBrief(b2.id)!);
  assert.equal(amm.brief(b2.id).state, 'blocked');
  assert.match(amm.status(run.id), /blocked: needs api change/);
  const b3 = amm.addBrief(run.id, { title: 'v', goal: 'g', scope: ['x/**'], checks: [], tier: 'light' });
  amm.dispatch(run.id);
  const w3 = amm.workerForBrief(b3.id)!;
  amm.cancel(b3.id);
  assert.equal(amm.store.get('workers', w3.id)!.state, 'cancelled');
  assert.equal(amm.sem.holders('workers').length, 0);
});

test('sweep marks workers lost after the lease and requeues their brief', async () => {
  const { amm, clock, b } = await started({ leaseMs: 1000 });
  clock.advance(1001);
  amm.sweep();
  assert.equal(amm.brief(b.id).state, 'queued');
  assert.equal(amm.store.all('workers')[0].state, 'lost');
});

test('nudge queues a correction for the active worker', async () => {
  const { amm, b } = await started();
  amm.nudge(b.id, 'use the existing helper');
  assert.equal(amm.workerForBrief(b.id)!.pendingCorrection, 'amm (from your manager): use the existing helper');
});
