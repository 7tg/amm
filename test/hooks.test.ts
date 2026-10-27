import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { handleHook, type HookInput } from '../src/hooks.ts';
import { addWorktree, initRepo, makeAmm } from './helpers.ts';

async function boundWorker(checks: string[] = []) {
  const h = makeAmm();
  const main = initRepo({ 'src/a.ts': 'a\n', 'README.md': 'r\n' });
  const repo = addWorktree(main);
  const run = await h.amm.startRun({ repo: main, testCmd: 'true', baseBranch: 'main' });
  const b = h.amm.addBrief(run.id, { title: 't', goal: 'g', scope: ['src/**'], checks, tier: 'light' });
  h.amm.dispatch(run.id);
  const base = { session_id: 's', cwd: repo, agent_id: 'ag1', agent_type: 'amm:amm-worker-light' };
  const hook = (event: string, extra: Partial<HookInput>) =>
    handleHook(h.amm, event, { ...base, hook_event_name: event, ...extra } as HookInput);
  await hook('PreToolUse', { tool_name: 'mcp__plugin_amm_amm__amm_brief_get', tool_input: { token: b.id } });
  return { ...h, main, repo, run, b, hook };
}

const denyReason = (out: Record<string, unknown>) =>
  (out.hookSpecificOutput as { permissionDecision?: string; permissionDecisionReason?: string } | undefined)?.permissionDecision === 'deny'
    ? (out.hookSpecificOutput as { permissionDecisionReason: string }).permissionDecisionReason
    : null;

test('brief_get binds the agent to its brief', async () => {
  const { amm, b, repo } = await boundWorker();
  assert.equal(amm.workerByAgent('ag1')!.briefId, b.id);
  assert.equal(amm.workerByAgent('ag1')!.worktree, repo);
});

test('edits outside scope or worktree, pushes and agent spawns are denied', async () => {
  const { hook, repo } = await boundWorker();
  assert.equal(denyReason(await hook('PreToolUse', { tool_name: 'Edit', tool_input: { file_path: path.join(repo, 'src/a.ts') } })), null);
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Write', tool_input: { file_path: path.join(repo, 'README.md') } }))!, /outside your scope/);
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Write', tool_input: { file_path: '/etc/x' } }))!, /outside your worktree/);
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Bash', tool_input: { command: 'git push origin HEAD' } }))!, /may not push/);
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Agent', tool_input: {} }))!, /spawn agents/);
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Write', tool_input: { file_path: path.join(repo, '.git/config') } }))!, /may not edit .git/);
});

test('a manager nudge is delivered once by denying the next call', async () => {
  const { amm, b, hook } = await boundWorker();
  amm.nudge(b.id, 'stop');
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Read', tool_input: {} }))!, /from your manager\): stop/);
  assert.equal(denyReason(await hook('PreToolUse', { tool_name: 'Read', tool_input: {} })), null);
});

test('post-tool rules add context, then escalate and stop the worker', async () => {
  const { amm, b, hook } = await boundWorker();
  const fail = () => hook('PostToolUseFailure', { tool_name: 'Bash', tool_input: { command: 'make' } });
  await fail();
  await fail();
  const third = await fail();
  assert.match(JSON.stringify(third), /failed 3 times/);
  await fail();
  await fail();
  await fail();
  assert.equal(amm.brief(b.id).tier, 'standard');
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Read', tool_input: {} }))!, /reassigned/);
});

test('done gate blocks without a report or with out-of-scope changes, then passes', async () => {
  const { amm, b, hook, repo } = await boundWorker(['test -f src/b.ts']);
  const stop = () => hook('SubagentStop', { agent_transcript_path: '/t.jsonl', stop_hook_active: false });
  let out = await stop();
  assert.equal(out.decision, 'block');
  assert.match(String(out.reason), /not called mcp__plugin_amm_amm__amm_report/);
  assert.match(String(out.reason), /check `test -f src\/b.ts` exited 1/);
  fs.writeFileSync(path.join(repo, 'README.md'), 'changed\n');
  fs.writeFileSync(path.join(repo, 'src/b.ts'), 'b\n');
  amm.report(b.id, { status: 'done', files: ['src/b.ts'], notes: '' });
  out = await stop();
  assert.match(String(out.reason), /outside your scope: README.md/);
  fs.writeFileSync(path.join(repo, 'README.md'), 'r\n');
  out = await stop();
  assert.deepEqual(out, {});
  assert.equal(amm.brief(b.id).state, 'done');
});

test('a blocked report ends the worker without running checks', async () => {
  const { amm, b, hook } = await boundWorker(['false']);
  amm.report(b.id, { status: 'blocked', files: [], notes: 'api missing' });
  assert.deepEqual(await hook('SubagentStop', {}), {});
  assert.equal(amm.brief(b.id).state, 'blocked');
});

test('three failed gates escalate', async () => {
  const { amm, b, hook } = await boundWorker(['false']);
  amm.report(b.id, { status: 'done', files: [], notes: '' });
  await hook('SubagentStop', {});
  await hook('SubagentStop', {});
  assert.deepEqual(await hook('SubagentStop', {}), {});
  assert.equal(amm.brief(b.id).tier, 'standard');
});

test('events from agents amm does not know are allowed untouched', async () => {
  const { amm } = makeAmm();
  const out = await handleHook(amm, 'PreToolUse', { session_id: 's', cwd: '/', hook_event_name: 'PreToolUse', agent_id: 'other', tool_name: 'Bash', tool_input: { command: 'git push' } });
  assert.deepEqual(out, {});
});
