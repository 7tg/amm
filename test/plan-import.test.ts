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
