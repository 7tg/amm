import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { makeAmm } from './helpers.ts';

async function setup(limits = {}) {
  const h = makeAmm(limits);
  const run = await h.amm.startRun({ repo: '/repo', testCmd: 'npm test', baseBranch: 'main' });
  return { ...h, run };
}
const brief = (title: string, scope: string[], tier: 'light' | 'standard' | 'heavy' = 'standard') => ({
  title,
  goal: `do ${title}`,
  scope,
  checks: ['npm test'],
  tier,
});

test('dispatch returns agent calls with tiered subagent types and tiny prompts', async () => {
  const { amm, run } = await setup();
  const b = amm.addBrief(run.id, brief('parser', ['src/parser.ts'], 'light'));
  const calls = amm.dispatch(run.id);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].subagent_type, 'amm:amm-worker-light');
  assert.match(calls[0].prompt, new RegExp(`token "${b.id}"`));
  assert.ok(calls[0].prompt.length < 220);
  assert.equal(amm.brief(b.id).state, 'running');
  assert.equal(amm.workerForBrief(b.id)!.state, 'starting');
});

test('dispatch respects maxWorkers', async () => {
  const { amm, run } = await setup({ maxWorkers: 1 });
  amm.addBrief(run.id, brief('a', ['src/a.ts']));
  amm.addBrief(run.id, brief('b', ['src/b.ts']));
  assert.equal(amm.dispatch(run.id).length, 1);
  assert.equal(amm.dispatch(run.id).length, 0);
});

test('dispatch holds back briefs whose scope overlaps a running brief', async () => {
  const { amm, run } = await setup();
  amm.addBrief(run.id, brief('a', ['src/**']));
  const b2 = amm.addBrief(run.id, brief('b', ['src/b.ts']));
  amm.addBrief(run.id, brief('c', ['docs/**']));
  assert.deepEqual(amm.dispatch(run.id).map((c) => c.description.split(':')[0]), [
    `amm ${amm.store.all('briefs')[0].id}`,
    `amm ${amm.store.all('briefs')[2].id}`,
  ]);
  assert.equal(amm.brief(b2.id).state, 'queued');
});

test('plan import creates draft briefs that need ready before dispatch', async () => {
  const { amm, run, dir } = await setup();
  const file = path.join(dir, 'plan.md');
  fs.writeFileSync(file, '### Task 1: One\n- Create: `src/one.ts`\n\n### Task 2: Two\nbody\n');
  const briefs = amm.importPlan(run.id, file);
  assert.deepEqual(briefs.map((b) => [b.title, b.state, b.scope]), [
    ['One', 'draft', ['src/one.ts']],
    ['Two', 'draft', []],
  ]);
  assert.equal(amm.dispatch(run.id).length, 0);
  assert.throws(() => amm.updateBrief(briefs[1].id, { ready: true }), /needs a scope/);
  amm.updateBrief(briefs[0].id, { tier: 'heavy', checks: ['npm test'], ready: true });
  assert.equal(amm.brief(briefs[0].id).maxTurns, 120);
  assert.equal(amm.dispatch(run.id).length, 1);
});

test('briefText gives the worker goal, scope, checks and report instructions', async () => {
  const { amm, run } = await setup();
  const b = amm.addBrief(run.id, brief('parser', ['src/parser.ts']));
  const text = amm.briefText(b.id);
  for (const part of ['do parser', '- src/parser.ts', '`npm test`', 'mcp__plugin_amm_amm__amm_report']) {
    assert.ok(text.includes(part), part);
  }
});

test('status reports counts and only new notable events', async () => {
  const { amm, run } = await setup();
  const b = amm.addBrief(run.id, brief('a', ['src/a.ts']));
  amm.dispatch(run.id);
  amm.event('done', run.id, null, { briefId: b.id, title: 'a' });
  const first = amm.status(run.id);
  assert.match(first, /1 briefs · 0 queued · 1 running/);
  assert.match(first, new RegExp(`${b.id} done: a`));
  assert.doesNotMatch(amm.status(run.id), /done: a/);
  assert.match(amm.status(run.id, true), new RegExp(`${b.id} \\[running\\] standard a1 a`));
});
