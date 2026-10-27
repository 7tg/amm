import fs from 'node:fs';
import path from 'node:path';
import { runChecks } from './checks.ts';
import { toolName } from './config.ts';
import { changedFiles, git, worktreeInfo } from './git.ts';
import { evaluate, isEditTool } from './rules.ts';
import { inScope } from './scope.ts';
import type { Amm } from './service.ts';
import type { Brief, Worker } from './types.ts';

export interface HookInput {
  session_id: string;
  cwd: string;
  hook_event_name: string;
  agent_id?: string;
  agent_type?: string;
  tool_name?: string;
  tool_input?: Record<string, unknown>;
  tool_response?: unknown;
  agent_transcript_path?: string;
  stop_hook_active?: boolean;
}

export type HookOutput = Record<string, unknown>;

const allow = (): HookOutput => ({});
const deny = (reason: string): HookOutput => ({
  hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: reason },
});
const addContext = (event: string, text: string): HookOutput => ({
  hookSpecificOutput: { hookEventName: event, additionalContext: text },
});
const blockStop = (reason: string): HookOutput => ({ decision: 'block', reason });

export async function handleHook(amm: Amm, event: string, input: HookInput): Promise<HookOutput> {
  if (!input.agent_id) {
    // Main session: only amm tool calls are forwarded here, to track which sessions use amm.
    amm.touchSession(input.session_id, input.cwd);
    if (event === 'PostToolUse' && input.tool_name?.endsWith('amm_run_start')) {
      const runId = /\br[0-9a-f]{6}\b/.exec(JSON.stringify(input.tool_response ?? ''))?.[0];
      const run = runId ? amm.store.get('runs', runId) : undefined;
      if (run) amm.store.put('runs', { ...run, sessionId: input.session_id });
    }
    return allow();
  }
  switch (event) {
    case 'SubagentStart':
      return input.agent_type?.includes('amm-worker')
        ? addContext('SubagentStart', `You are an amm worker. Before anything else, load ${toolName('amm_brief_get')} with ToolSearch and call it with the token from your prompt.`)
        : allow();
    case 'PreToolUse':
      return preTool(amm, input);
    case 'PostToolUse':
    case 'PostToolUseFailure':
      return postTool(amm, input, event);
    case 'SubagentStop':
      return subagentStop(amm, input);
    default:
      return allow();
  }
}

async function preTool(amm: Amm, input: HookInput): Promise<HookOutput> {
  const tool = input.tool_name ?? '';
  const ti = input.tool_input ?? {};
  const isWorker = input.agent_type?.includes('amm-worker') ?? false;
  let found = amm.workerByAgent(input.agent_id!);
  if (!found && isWorker) {
    if (tool === toolName('amm_brief_get') && typeof ti.token === 'string') {
      const bound = await amm.bind(ti.token, input.agent_id!, input.cwd);
      if ('error' in bound) return deny(`amm: ${bound.error}. Work from the worktree amm created for you.`);
      found = bound.worker;
    } else if (tool !== 'ToolSearch' && tool !== toolName('amm_brief_get')) {
      return deny('amm: fetch your brief first with amm_brief_get');
    }
  }
  if (!found) return allow();
  if (found.state === 'cancelled') return deny('amm: your manager cancelled this brief. Stop now and reply CANCELLED.');
  if (['escalated', 'lost', 'done', 'blocked'].includes(found.state)) {
    return deny('amm: this brief was reassigned or is finished. Stop now and reply REASSIGNED.');
  }
  if (found.baseSha === null && tool !== toolName('amm_brief_get')) return deny('amm: setting up your brief, retry');
  if (tool.includes('amm_') && typeof ti.token === 'string' && ti.token !== found.briefId) {
    return deny('amm: that token belongs to another brief. Use only your own token.');
  }
  const w = amm.touchWorker(found);
  const correction = amm.takeCorrection(w);
  if (correction) return deny(correction);
  const reason = forbidden(w, amm.brief(w.briefId), tool, ti);
  if (reason) {
    amm.event('deny', w.runId, w.id, { tool, reason });
    return deny(reason);
  }
  return allow();
}

/** realpath of the nearest existing ancestor plus the remaining segments; follows dangling symlinks. */
export function resolveReal(p: string, depth = 0): string {
  try {
    return fs.realpathSync(p);
  } catch {
    // not fully resolvable: fall through
  }
  try {
    if (fs.lstatSync(p).isSymbolicLink() && depth < 40) return resolveReal(path.resolve(path.dirname(p), fs.readlinkSync(p)), depth + 1);
  } catch {
    // does not exist
  }
  const parent = path.dirname(p);
  return parent === p ? p : path.join(resolveReal(parent, depth), path.basename(p));
}

export function forbidden(w: Worker, b: Brief, tool: string, ti: Record<string, unknown>): string | null {
  if (tool === 'Agent' || tool === 'Task') return 'amm: workers may not spawn agents. Do the work yourself or report "blocked".';
  if (tool === 'Bash' && /\bgit\b[^;&|\n]*\b(push|send-pack)\b/.test(String(ti.command ?? ''))) {
    return 'amm: workers may not push. Your manager merges your branch.';
  }
  if (isEditTool(tool) && w.worktree) {
    const file = String(ti.file_path ?? ti.notebook_path ?? '');
    const abs = resolveReal(path.resolve(w.worktree, file));
    const rel = path.relative(w.worktree, abs);
    if (rel === '..' || rel.startsWith('..' + path.sep) || path.isAbsolute(rel)) return `amm: ${file} is outside your worktree (${w.worktree}). Edit only inside it.`;
    if (rel === '.git' || rel.startsWith('.git' + path.sep)) return 'amm: workers may not edit .git.';
    if (!inScope(abs, w.worktree, b.scope)) {
      return `amm: ${rel} is outside your scope (${b.scope.join(', ')}). Do not change it; mention it in your amm_report instead.`;
    }
  }
  return null;
}

function bashFailed(response: unknown): boolean {
  if (!response || typeof response !== 'object') return false;
  const r = response as Record<string, unknown>;
  if (r.is_error === true) return true;
  const code = r.exitCode ?? r.exit_code ?? r.returnCode;
  return typeof code === 'number' && code !== 0;
}

function summarize(input: HookInput): string {
  const ti = input.tool_input ?? {};
  if (input.tool_name === 'Bash') return String(ti.command ?? '').slice(0, 160);
  return String(ti.file_path ?? ti.pattern ?? ti.query ?? '').slice(0, 160);
}

function postTool(amm: Amm, input: HookInput, event: string): HookOutput {
  const w = amm.workerByAgent(input.agent_id!);
  if (!w || w.state !== 'running') return allow();
  const b = amm.brief(w.briefId);
  const failed = event === 'PostToolUseFailure' || bashFailed(input.tool_response);
  const { counters, verdict } = evaluate(
    w,
    { tool: input.tool_name ?? '', input: input.tool_input ?? {}, failed },
    { limits: amm.limits, maxTurns: b.maxTurns, checks: b.checks },
  );
  const updated = amm.updateCounters(w, counters);
  amm.event('tool', w.runId, w.id, { tool: input.tool_name, failed, summary: summarize(input) });
  if (verdict.kind === 'correct') {
    amm.event('correction', w.runId, w.id, { rule: verdict.rule, message: verdict.message, via: 'context' });
    return addContext(event, verdict.message);
  }
  if (verdict.kind === 'escalate') {
    amm.escalate(updated, `${verdict.rule} rule fired again after a correction`);
    return addContext(event, 'amm: you are being replaced by a stronger worker. Stop now and reply REASSIGNED.');
  }
  return allow();
}

async function subagentStop(amm: Amm, input: HookInput): Promise<HookOutput> {
  const w = amm.workerByAgent(input.agent_id!);
  if (!w || w.state !== 'running') return allow();
  const b = amm.brief(w.briefId);
  if (b.report?.status === 'blocked') {
    amm.markBlocked(w);
    return allow();
  }
  const checking = amm.markChecking(w, input.agent_transcript_path ?? null);
  if (!checking) return allow();
  const problems = await gateProblems(amm, checking, b);
  if (problems.length === 0) {
    amm.complete(checking);
    return allow();
  }
  // null: the worker was cancelled or swept while its checks ran; let it stop.
  const next = amm.stopBlocked(checking, problems);
  if (!next) return allow();
  if (next.stopBlocks >= amm.limits.maxStopBlocks) {
    amm.escalate(next, `done gate failed ${next.stopBlocks} times: ${problems[0].split('\n')[0]}`);
    return allow();
  }
  return blockStop(`amm done gate failed:\n- ${problems.join('\n- ')}\nFix these, then finish again.`);
}

async function gateProblems(amm: Amm, w: Worker, b: Brief): Promise<string[]> {
  const problems: string[] = [];
  if (!b.report) problems.push(`you have not called ${toolName('amm_report')} with token "${b.id}"`);
  const repoDir = amm.run(w.runId).repo;
  // The worktree must still be what bind validated; checked before and after the checks run.
  const verify = async (): Promise<string | null> => {
    if (!w.worktree || !w.baseSha || !w.branch || !fs.existsSync(w.worktree)) {
      return 'amm could not find your worktree or its base commit, so your changes cannot be scope-checked';
    }
    try {
      const [mine, repo] = await Promise.all([worktreeInfo(w.worktree), worktreeInfo(repoDir)]);
      if (mine.toplevel !== w.worktree || mine.commonDir !== repo.commonDir) {
        return 'your worktree is no longer a worktree of the repository (was .git removed or replaced?)';
      }
      if (mine.branch !== w.branch) return `your worktree is on branch ${mine.branch ?? '(detached)'}, not ${w.branch}. Switch back`;
      return null;
    } catch (e) {
      return `amm could not inspect your worktree: ${(e as Error).message.split('\n')[0]}`;
    }
  };
  const before = await verify();
  if (before) problems.push(before);
  if (w.worktree && fs.existsSync(w.worktree)) {
    for (const r of await runChecks(b.checks, w.worktree, amm.limits.checkTimeoutMs, amm.limits.gateBudgetMs)) {
      if (!r.ok) problems.push(`check \`${r.cmd}\` ${r.timedOut ? 'timed out' : `exited ${r.code}`}:\n${r.output.slice(-1500)}`);
    }
  }
  // Checks run worker-written code, which may have tampered with the worktree: verify again, then diff.
  const after = before ? null : await verify();
  if (after) problems.push(`after the checks: ${after}`);
  if (!before && !after) {
    try {
      const outside = (await changedFiles(w.worktree!, w.baseSha!, w.branch!)).filter((f) => !inScope(path.join(w.worktree!, f), w.worktree!, b.scope));
      if (outside.length) {
        problems.push(`files changed outside your scope: ${outside.join(', ')}. Revert them and describe the needed change in amm_report instead`);
      }
    } catch (e) {
      // Fail closed: an unreadable worktree must not pass the gate.
      problems.push(`amm could not list your changed files: ${(e as Error).message.split('\n')[0]}`);
    }
  }
  return problems;
}
