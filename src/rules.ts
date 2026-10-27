import type { Limits } from './config.ts';

export interface Counters {
  toolCalls: number;
  sinceProgress: number;
  lastCmd: string | null;
  lastCmdFails: number;
  firedRules: string[];
}

export interface ToolEvent {
  tool: string;
  input: Record<string, unknown>;
  failed: boolean;
}

export type Verdict =
  | { kind: 'ok' }
  | { kind: 'correct'; rule: string; message: string }
  | { kind: 'escalate'; rule: string };

export interface RuleContext {
  limits: Limits;
  maxTurns: number;
  checks: string[];
}

const EDIT_TOOLS = new Set(['Edit', 'Write', 'MultiEdit', 'NotebookEdit']);

export function isEditTool(tool: string): boolean {
  return EDIT_TOOLS.has(tool);
}

export function messageFor(rule: string, ctx: RuleContext): string {
  switch (rule) {
    case 'repeat-failure':
      return `amm watchdog: the same command has failed ${ctx.limits.repeatFailures} times. Stop retrying it. Change approach, or call amm_report with status "blocked" and explain why.`;
    case 'no-progress':
      return `amm watchdog: ${ctx.limits.noProgressCalls} tool calls without changing a file or passing a check. Re-read your brief and make the change, or report "blocked".`;
    case 'budget':
      return `amm watchdog: you have used ${Math.round(ctx.limits.budgetFraction * 100)}% of your tool budget. Finish the current change, run your checks, call amm_report and stop.`;
    default:
      return `amm watchdog: ${rule}`;
  }
}

function forget(c: Counters, rule: string): void {
  c.firedRules = c.firedRules.filter((r) => r !== rule);
}

/** Updates the worker's counters for one tool call and decides whether to correct or escalate. */
export function evaluate(c: Counters, e: ToolEvent, ctx: RuleContext): { counters: Counters; verdict: Verdict } {
  const next: Counters = {
    toolCalls: c.toolCalls + 1,
    sinceProgress: c.sinceProgress,
    lastCmd: c.lastCmd,
    lastCmdFails: c.lastCmdFails,
    firedRules: [...c.firedRules],
  };
  const cmd = e.tool === 'Bash' ? String(e.input.command ?? '') : null;
  const passedCheck = cmd !== null && !e.failed && ctx.checks.includes(cmd);
  const progressed = (isEditTool(e.tool) && !e.failed) || passedCheck;
  next.sinceProgress = progressed ? 0 : c.sinceProgress + 1;
  if (progressed) forget(next, 'no-progress');
  if (cmd !== null) {
    if (cmd !== c.lastCmd || !e.failed) forget(next, 'repeat-failure');
    if (e.failed) {
      next.lastCmdFails = cmd === c.lastCmd ? c.lastCmdFails + 1 : 1;
      next.lastCmd = cmd;
    } else if (cmd === c.lastCmd) {
      next.lastCmdFails = 0;
    }
  }

  const fire = (rule: string) => {
    if (next.firedRules.includes(rule)) return { counters: next, verdict: { kind: 'escalate', rule } as Verdict };
    next.firedRules.push(rule);
    return { counters: next, verdict: { kind: 'correct', rule, message: messageFor(rule, ctx) } as Verdict };
  };

  if (next.lastCmdFails >= ctx.limits.repeatFailures) {
    next.lastCmdFails = 0;
    return fire('repeat-failure');
  }
  if (next.sinceProgress >= ctx.limits.noProgressCalls) {
    next.sinceProgress = 0;
    return fire('no-progress');
  }
  if (next.toolCalls >= Math.ceil(ctx.maxTurns * ctx.limits.budgetFraction) && !next.firedRules.includes('budget')) {
    next.firedRules.push('budget');
    return { counters: next, verdict: { kind: 'correct', rule: 'budget', message: messageFor('budget', ctx) } };
  }
  return { counters: next, verdict: { kind: 'ok' } };
}
