import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DEFAULT_LIMITS } from '../src/config.ts';
import { type Counters, evaluate, type ToolEvent } from '../src/rules.ts';

const fresh = (): Counters => ({ toolCalls: 0, sinceProgress: 0, lastCmd: null, lastCmdFails: 0, firedRules: [] });
const ctx = { limits: DEFAULT_LIMITS, maxTurns: 100, checks: ['npm test'] };

function feed(events: ToolEvent[], start = fresh()) {
  let c = start;
  const verdicts = [];
  for (const e of events) {
    const r = evaluate(c, e, ctx);
    c = r.counters;
    verdicts.push(r.verdict);
  }
  return { counters: c, verdicts };
}

const bash = (command: string, failed = false): ToolEvent => ({ tool: 'Bash', input: { command }, failed });
const read = (): ToolEvent => ({ tool: 'Read', input: { file_path: 'a' }, failed: false });
const edit = (): ToolEvent => ({ tool: 'Edit', input: { file_path: 'a' }, failed: false });

test('same command failing 3 times triggers a correction, then escalation', () => {
  const first = feed([bash('make', true), bash('make', true), bash('make', true)]);
  assert.equal(first.verdicts.at(-1)!.kind, 'correct');
  const again = feed([bash('make', true), bash('make', true), bash('make', true)], first.counters);
  assert.deepEqual(again.verdicts.at(-1), { kind: 'escalate', rule: 'repeat-failure' });
});

test('a different failing command resets the repeat counter', () => {
  const r = feed([bash('a', true), bash('a', true), bash('b', true)]);
  assert.equal(r.counters.lastCmdFails, 1);
  assert.ok(r.verdicts.every((v) => v.kind === 'ok'));
});

test('8 calls without an edit or passing check is no-progress', () => {
  const r = feed(Array.from({ length: 8 }, read));
  assert.equal(r.verdicts.at(-1)!.kind, 'correct');
  assert.equal((r.verdicts.at(-1) as { rule: string }).rule, 'no-progress');
});

test('edits and passing checks count as progress', () => {
  const r = feed([...Array.from({ length: 7 }, read), edit(), ...Array.from({ length: 7 }, read), bash('npm test'), read()]);
  assert.ok(r.verdicts.every((v) => v.kind === 'ok'));
});

test('budget fires once at 80% of maxTurns and never escalates', () => {
  const events = Array.from({ length: 90 }, (_, i) => (i % 2 ? edit() : read()));
  const r = feed(events);
  const budget = r.verdicts.filter((v) => v.kind !== 'ok');
  assert.equal(budget.length, 1);
  assert.equal((budget[0] as { rule: string }).rule, 'budget');
});

test('a success between failures resets the repeat counter', () => {
  const r = feed([bash('make', true), bash('make', true), bash('make'), bash('make', true), bash('make', true)]);
  assert.ok(r.verdicts.every((v) => v.kind === 'ok'));
  const next = evaluate(r.counters, bash('make', true), ctx);
  assert.equal(next.verdict.kind, 'correct');
});

test('no-progress forgets it fired after an edit', () => {
  const r = feed([...Array.from({ length: 8 }, read), edit(), ...Array.from({ length: 8 }, read)]);
  const kinds = r.verdicts.map((v) => v.kind);
  assert.equal(kinds.filter((k) => k === 'correct').length, 2);
  assert.ok(!kinds.includes('escalate'));
});

test('no-progress escalates when it fires again without recovery', () => {
  const r = feed(Array.from({ length: 16 }, read));
  assert.deepEqual(r.verdicts.at(-1), { kind: 'escalate', rule: 'no-progress' });
});

test('a failed edit does not count as progress', () => {
  const failedEdit: ToolEvent = { tool: 'Edit', input: { file_path: 'a' }, failed: true };
  const r = feed([...Array.from({ length: 7 }, read), failedEdit]);
  assert.equal((r.verdicts.at(-1) as { rule: string }).rule, 'no-progress');
});

test('repeat-failure forgets it fired after a different command runs', () => {
  const first = feed([bash('make', true), bash('make', true), bash('make', true), bash('ls')]);
  const again = feed([bash('make', true), bash('make', true), bash('make', true)], first.counters);
  assert.equal(again.verdicts.at(-1)!.kind, 'correct');
});
