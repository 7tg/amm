import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { changedFiles } from '../src/git.ts';
import { addWorktree, initRepo, makeAmm } from './helpers.ts';

async function started(limits = {}) {
  const h = makeAmm(limits);
  const repo = initRepo();
  const run = await h.amm.startRun({ repo, testCmd: 'npm test', baseBranch: 'main' });
  const b = h.amm.addBrief(run.id, { title: 't', goal: 'g', scope: ['src/**'], checks: [], tier: 'light' });
  h.amm.dispatch(run.id);
  return { ...h, run, b, repo };
}

test('bind attaches an agent and worktree to the starting worker', async () => {
  const { amm, b, repo } = await started();
  const wt = addWorktree(repo);
  const r = await amm.bind(b.id, 'agent-1', wt);
  const w = 'worker' in r ? r.worker : undefined;
  assert.equal(w!.branch, path.basename(wt).replace('agent-', 'worktree-agent-'));
  assert.equal(w!.state, 'running');
  assert.equal(w!.worktree, wt);
  assert.match(w!.baseSha!, /^[0-9a-f]{40}$/);
  assert.equal(amm.workerByAgent('agent-1')!.id, w!.id);
  assert.ok('error' in (await amm.bind(b.id, 'agent-2', wt)));
});

test('bind refuses the main checkout, a foreign repo and a non-agent branch, and releases the claim', async () => {
  const { amm, b, repo } = await started();
  const bad = async (cwd: string, re: RegExp) => {
    const r = await amm.bind(b.id, 'agent-1', cwd);
    assert.match('error' in r ? r.error : 'bound', re);
    const w = amm.workerForBrief(b.id)!;
    assert.deepEqual([w.state, w.agentId, w.worktree], ['starting', null, null]);
  };
  await bad(repo, /main checkout/);
  await bad(addWorktree(initRepo()), /different repository/);
  const plain = addWorktree(repo);
  execFileSync('git', ['checkout', '-q', '-b', 'feature'], { cwd: plain });
  await bad(plain, /worktree-agent-/);
});

test('commits made before bind stay in the base..HEAD diff', async () => {
  const { amm, b, repo } = await started();
  const wt = addWorktree(repo);
  fs.writeFileSync(path.join(wt, 'early.txt'), 'x\n');
  execFileSync('git', ['add', '.'], { cwd: wt });
  execFileSync('git', ['-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-qm', 'early'], { cwd: wt });
  const r = await amm.bind(b.id, 'agent-1', wt);
  assert.ok('worker' in r);
  assert.deepEqual(await changedFiles(wt, r.worker.baseSha!, r.worker.branch!), ['early.txt']);
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
  const { amm, run, b, repo } = await started();
  await amm.bind(b.id, 'agent-1', addWorktree(repo));
  assert.equal(amm.complete(amm.workerForBrief(b.id)!), true);
  assert.equal(amm.brief(b.id).state, 'done');
  const b2 = amm.addBrief(run.id, { title: 'u', goal: 'g', scope: ['lib/**'], checks: [], tier: 'light' });
  amm.dispatch(run.id);
  await amm.bind(b2.id, 'agent-2', addWorktree(repo));
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

test('a concurrent cancel is not overwritten when bind refuses', async () => {
  const { amm, b, repo } = await started();
  const p = amm.bind(b.id, 'agent-1', repo);
  amm.cancel(b.id);
  assert.ok('error' in (await p));
  assert.equal(amm.store.all('workers')[0].state, 'cancelled');
});
