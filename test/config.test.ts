import assert from 'node:assert/strict';
import { test } from 'node:test';
import { agentType, ammHome, DEFAULT_LIMITS, loadLimits, nextTier, toolName } from '../src/config.ts';

test('nextTier climbs the ladder and stops at heavy', () => {
  assert.equal(nextTier('light'), 'standard');
  assert.equal(nextTier('standard'), 'heavy');
  assert.equal(nextTier('heavy'), null);
});

test('loadLimits reads AMM_MAX_WORKERS and ignores junk', () => {
  assert.equal(loadLimits({ AMM_MAX_WORKERS: '2' }).maxWorkers, 2);
  assert.equal(loadLimits({ AMM_MAX_WORKERS: 'x' }).maxWorkers, DEFAULT_LIMITS.maxWorkers);
});

test('names follow Claude Code plugin conventions and can be overridden', () => {
  assert.equal(agentType('light', {}), 'amm:amm-worker-light');
  assert.equal(toolName('amm_report', {}), 'mcp__plugin_amm_amm__amm_report');
  assert.equal(agentType('heavy', { AMM_AGENT_PREFIX: '' }), 'amm-worker-heavy');
  assert.equal(ammHome({ AMM_HOME: '/x' }), '/x');
});
