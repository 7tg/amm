import os from 'node:os';
import path from 'node:path';
import type { Tier } from './types.ts';

export const TIERS: Tier[] = ['light', 'standard', 'heavy'];
export const TIER_MAX_TURNS: Record<Tier, number> = { light: 40, standard: 80, heavy: 120 };
export const DEFAULT_PORT = 7878;

export interface Limits {
  maxWorkers: number;
  repeatFailures: number;
  noProgressCalls: number;
  budgetFraction: number;
  maxStopBlocks: number;
  leaseMs: number;
  checkTimeoutMs: number;
  idleShutdownMs: number;
}

export const DEFAULT_LIMITS: Limits = {
  maxWorkers: 4,
  repeatFailures: 3,
  noProgressCalls: 8,
  budgetFraction: 0.8,
  maxStopBlocks: 3,
  leaseMs: 10 * 60_000,
  checkTimeoutMs: 5 * 60_000,
  idleShutdownMs: 30 * 60_000,
};

type Env = Record<string, string | undefined>;

export function loadLimits(env: Env = process.env): Limits {
  const maxWorkers = Number(env.AMM_MAX_WORKERS);
  return { ...DEFAULT_LIMITS, ...(maxWorkers > 0 ? { maxWorkers } : {}) };
}

export function ammHome(env: Env = process.env): string {
  return env.AMM_HOME ?? path.join(os.homedir(), '.amm');
}

export function nextTier(tier: Tier): Tier | null {
  const i = TIERS.indexOf(tier);
  return i < TIERS.length - 1 ? TIERS[i + 1] : null;
}

// Claude Code names plugin agents "<plugin>:<agent>" and plugin MCP tools
// "mcp__plugin_<plugin>_<server>__<tool>". Task 15 verifies both.
export function agentType(tier: Tier, env: Env = process.env): string {
  return `${env.AMM_AGENT_PREFIX ?? 'amm:'}amm-worker-${tier}`;
}

export function toolName(tool: string, env: Env = process.env): string {
  return `${env.AMM_TOOL_PREFIX ?? 'mcp__plugin_amm_amm__'}${tool}`;
}
