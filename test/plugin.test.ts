import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

const root = path.resolve(import.meta.dirname, '..');
const read = (p: string) => fs.readFileSync(path.join(root, p), 'utf8');

test('manifest, mcp and hooks are valid json and point at shipped files', () => {
  assert.equal(JSON.parse(read('.claude-plugin/plugin.json')).name, 'amm');
  assert.deepEqual(JSON.parse(read('.mcp.json')).mcpServers.amm.args, ['${CLAUDE_PLUGIN_ROOT}/dist/amm.js', 'shim']);
  const hooks = JSON.parse(read('hooks/hooks.json')).hooks;
  assert.deepEqual(Object.keys(hooks).sort(), ['PostToolUse', 'PostToolUseFailure', 'PreToolUse', 'SubagentStart', 'SubagentStop']);
  assert.equal(hooks.SubagentStop[0].hooks[0].timeout, 600);
});

test('each worker agent pins its model, runs in a worktree and cannot spawn agents', () => {
  for (const [tier, model] of [['light', 'haiku'], ['standard', 'sonnet'], ['heavy', 'opus']]) {
    const md = read(`agents/amm-worker-${tier}.md`);
    assert.match(md, new RegExp(`^name: amm-worker-${tier}$`, 'm'));
    assert.match(md, new RegExp(`^model: ${model}$`, 'm'));
    assert.match(md, /^isolation: worktree$/m);
    assert.match(md, /^disallowedTools: Agent$/m);
  }
});

test('skills have frontmatter with name and description', () => {
  for (const s of ['amm-delegate', 'amm-supervise']) {
    const md = read(`skills/${s}/SKILL.md`);
    assert.match(md, new RegExp(`^---\\nname: ${s}\\ndescription: .+\\n---`, 'm'));
  }
});
