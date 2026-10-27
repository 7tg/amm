import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
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

test('brief_get from the main checkout is refused', async () => {
  const h = makeAmm();
  const repo = initRepo();
  const run = await h.amm.startRun({ repo, testCmd: 't', baseBranch: 'main' });
  const b = h.amm.addBrief(run.id, { title: 't', goal: 'g', scope: ['src/**'], checks: [], tier: 'light' });
  h.amm.dispatch(run.id);
  const out = await handleHook(h.amm, 'PreToolUse', {
    session_id: 's', cwd: repo, hook_event_name: 'PreToolUse', agent_id: 'ag1', agent_type: 'amm:amm-worker-light',
    tool_name: 'mcp__plugin_amm_amm__amm_brief_get', tool_input: { token: b.id },
  });
  assert.match(denyReason(out)!, /^amm: .*Work from the worktree amm created for you\.$/);
  assert.equal(h.amm.workerByAgent('ag1'), undefined);
});

test('an unbound amm worker may only fetch its brief; other subagents are untouched', async () => {
  const { amm, repo } = await boundWorker();
  const call = (agent_type: string, tool_name: string) =>
    handleHook(amm, 'PreToolUse', { session_id: 's', cwd: repo, hook_event_name: 'PreToolUse', agent_id: 'ag9', agent_type, tool_name, tool_input: { file_path: path.join(repo, 'src/a.ts') } });
  assert.match(denyReason(await call('amm:amm-worker-light', 'Edit'))!, /fetch your brief first with amm_brief_get/);
  assert.equal(denyReason(await call('amm:amm-worker-light', 'ToolSearch')), null);
  assert.deepEqual(await call('general-purpose', 'Edit'), {});
});

test('a bound worker cannot use another brief token', async () => {
  const { hook } = await boundWorker();
  const out = await hook('PreToolUse', { tool_name: 'mcp__plugin_amm_amm__amm_report', tool_input: { token: 'bother' } });
  assert.match(denyReason(out)!, /token/);
  assert.equal(denyReason(await hook('PreToolUse', { tool_name: 'Read', tool_input: {} })), null);
});

test('edits through a symlink that leaves the worktree are denied', async () => {
  const { hook, repo } = await boundWorker();
  const outside = fs.mkdtempSync(path.join(fs.realpathSync(os.tmpdir()), 'amm-out-'));
  fs.symlinkSync(outside, path.join(repo, 'src/l'));
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Edit', tool_input: { file_path: path.join(repo, 'src/l/x') } }))!, /outside your worktree/);
  fs.symlinkSync(path.join(outside, 'nope'), path.join(repo, 'src/dangling'));
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Write', tool_input: { file_path: path.join(repo, 'src/dangling') } }))!, /outside your worktree/);
});

test('git push is denied with options before the subcommand', async () => {
  const { hook } = await boundWorker();
  for (const command of ['git -C . push', 'git -c a=b push origin', 'git send-pack x', 'cd x && git push']) {
    assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Bash', tool_input: { command } }))!, /may not push/, command);
  }
  assert.equal(denyReason(await hook('PreToolUse', { tool_name: 'Bash', tool_input: { command: 'git status; echo push' } })), null);
});

test('the gate fails closed when the worktree is no longer a worktree', async () => {
  const { amm, b, hook, repo } = await boundWorker();
  amm.report(b.id, { status: 'done', files: [], notes: '' });
  fs.rmSync(path.join(repo, '.git'));
  const out = await hook('SubagentStop', {});
  assert.equal(out.decision, 'block');
  assert.match(String(out.reason), /worktree/);
});

test('the gate fails when the branch was switched', async () => {
  const { amm, b, hook, repo } = await boundWorker();
  amm.report(b.id, { status: 'done', files: [], notes: '' });
  execFileSync('git', ['checkout', '-q', '-b', 'other'], { cwd: repo });
  assert.match(String((await hook('SubagentStop', {})).reason), /branch/);
});

test('files touched by the done checks count toward the scope check', async () => {
  const { amm, b, hook } = await boundWorker(['echo x > README.md']);
  amm.report(b.id, { status: 'done', files: [], notes: '' });
  assert.match(String((await hook('SubagentStop', {})).reason), /outside your scope: README.md/);
});

for (const sabotage of ['rm .git', 'git checkout -q --detach HEAD~0']) {
  test(`a check that runs "${sabotage}" cannot hide a committed out-of-scope change`, async () => {
    const { amm, b, hook, repo } = await boundWorker([sabotage]);
    fs.writeFileSync(path.join(repo, 'README.md'), 'evil\n');
    execFileSync('git', ['-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-qam', 'evil'], { cwd: repo });
    amm.report(b.id, { status: 'done', files: [], notes: '' });
    const out = await hook('SubagentStop', {});
    assert.equal(out.decision, 'block');
    assert.notEqual(amm.brief(b.id).state, 'done');
  });
}

test('a committed out-of-scope change is listed even when the checks leave the worktree intact', async () => {
  const { amm, b, hook, repo } = await boundWorker(['true']);
  fs.writeFileSync(path.join(repo, 'README.md'), 'evil\n');
  execFileSync('git', ['-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-qam', 'evil'], { cwd: repo });
  amm.report(b.id, { status: 'done', files: [], notes: '' });
  assert.match(String((await hook('SubagentStop', {})).reason), /outside your scope: README.md/);
});

test('while validation is in progress a bound worker may only call amm_brief_get', async () => {
  const { amm, hook } = await boundWorker();
  const w = amm.workerByAgent('ag1')!;
  amm.store.put('workers', { ...w, baseSha: null });
  assert.match(denyReason(await hook('PreToolUse', { tool_name: 'Read', tool_input: {} }))!, /setting up your brief, retry/);
  assert.equal(denyReason(await hook('PreToolUse', { tool_name: 'mcp__plugin_amm_amm__amm_brief_get', tool_input: { token: w.briefId } })), null);
});

test('only the exact amm_brief_get tool name is allowed before binding', async () => {
  const { amm, repo } = await boundWorker();
  const out = await handleHook(amm, 'PreToolUse', { session_id: 's', cwd: repo, hook_event_name: 'PreToolUse', agent_id: 'ag9', agent_type: 'amm:amm-worker-light', tool_name: 'evil__amm_brief_get', tool_input: { token: 'x' } });
  assert.match(denyReason(out)!, /fetch your brief first/);
});
