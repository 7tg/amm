export type Tier = 'light' | 'standard' | 'heavy';
export type RunState = 'active' | 'done' | 'cancelled';
export type BriefState = 'draft' | 'queued' | 'running' | 'done' | 'blocked' | 'failed' | 'cancelled';
export type WorkerState = 'starting' | 'running' | 'checking' | 'done' | 'blocked' | 'lost' | 'escalated' | 'cancelled';

export interface Run {
  id: string;
  repo: string;
  baseBranch: string;
  testCmd: string;
  sessionId: string | null;
  state: RunState;
  createdAt: number;
  /** Id of the last event already reported by amm_status. */
  statusCursor: number;
}

export interface Report {
  status: 'done' | 'blocked';
  files: string[];
  notes: string;
}

export interface Brief {
  id: string;
  runId: string;
  seq: number;
  title: string;
  goal: string;
  scope: string[];
  checks: string[];
  tier: Tier;
  maxTurns: number;
  state: BriefState;
  attempt: number;
  notes: string[];
  report: Report | null;
}

export interface Worker {
  id: string;
  briefId: string;
  runId: string;
  agentId: string | null;
  tier: Tier;
  worktree: string | null;
  branch: string | null;
  baseSha: string | null;
  transcriptPath: string | null;
  state: WorkerState;
  toolCalls: number;
  sinceProgress: number;
  lastCmd: string | null;
  lastCmdFails: number;
  stopBlocks: number;
  firedRules: string[];
  pendingCorrection: string | null;
  startedAt: number;
  lastEventAt: number;
}

export interface Session {
  id: string;
  cwd: string;
  firstSeen: number;
  lastSeen: number;
}

export interface AmmEvent {
  id: number;
  ts: number;
  runId: string | null;
  workerId: string | null;
  kind: string;
  detail: Record<string, unknown>;
}
