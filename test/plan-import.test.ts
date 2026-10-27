import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parsePlan } from '../src/plan-import.ts';

const PLAN = `# Feature Plan

intro text

### Task 1: Add parser

**Files:**
- Create: \`src/parser.ts\`
- Test: \`test/parser.test.ts\`

- [ ] Step 1
\`\`\`ts
### Task 99: not a heading inside a fence
- Create: \`not/a/file.ts\`
\`\`\`

### Task 2 - Wire CLI
- Modify: \`src/cli.ts:10-20\`
body
`;

test('splits tasks by heading, ignoring fenced code', () => {
  const tasks = parsePlan(PLAN);
  assert.deepEqual(tasks.map((t) => t.title), ['Add parser', 'Wire CLI']);
});

test('collects file paths and strips line ranges', () => {
  const [one, two] = parsePlan(PLAN);
  assert.deepEqual(one.files, ['src/parser.ts', 'test/parser.test.ts']);
  assert.deepEqual(two.files, ['src/cli.ts']);
  assert.match(one.body, /Task 99: not a heading/);
});

test('returns no tasks for a document without task headings', () => {
  assert.deepEqual(parsePlan('# Title\n\njust text'), []);
});

test('collects every backticked path on a file line', () => {
  const [t] = parsePlan('### Task 1: X\n- Modify: `a.ts:1-5`, `b.ts`\n');
  assert.deepEqual(t.files, ['a.ts', 'b.ts']);
});

test('fences close only on the same marker, at least as long', () => {
  const md = '### Task 1: X\n````md\n```\n### Task 9: inner\n```\n- Create: `no.ts`\n````\n- Create: `yes.ts`\n\n```\n~~~\n### Task 8: still fenced\n~~~\n```\n';
  const tasks = parsePlan(md);
  assert.deepEqual(tasks.map((t) => t.title), ['X']);
  assert.deepEqual(tasks[0].files, ['yes.ts']);
});

test('handles CRLF and em dash separators', () => {
  const tasks = parsePlan('### Task 1 — Dash\r\n- Create: `a.ts`\r\n');
  assert.deepEqual(tasks.map((t) => t.title), ['Dash']);
  assert.deepEqual(tasks[0].files, ['a.ts']);
});

test('a heading of the same or higher level ends the task body', () => {
  const tasks = parsePlan('### Task 1: X\nin\n#### Sub\nstill in\n## Appendix\n- Create: `out.ts`\nout\n');
  assert.equal(tasks[0].body, 'in\n#### Sub\nstill in');
  assert.deepEqual(tasks[0].files, []);
});
