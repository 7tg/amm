import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Semaphores } from '../src/semaphores.ts';

function clock() {
  let t = 0;
  return { now: () => t, advance: (ms: number) => { t += ms; } };
}

test('capacity limits holders; re-acquire by the same holder is idempotent', () => {
  const s = new Semaphores(clock().now);
  assert.equal(s.tryAcquire('workers', 'w1', 2, 100), true);
  assert.equal(s.tryAcquire('workers', 'w1', 2, 100), true);
  assert.equal(s.tryAcquire('workers', 'w2', 2, 100), true);
  assert.equal(s.tryAcquire('workers', 'w3', 2, 100), false);
  assert.deepEqual(s.holders('workers'), ['w1', 'w2']);
});

test('release frees the slot under every name', () => {
  const s = new Semaphores(clock().now);
  s.tryAcquire('workers', 'w1', 1, 100);
  s.tryAcquire('merge', 'w1', 1, 100);
  s.release('w1');
  assert.deepEqual(s.holders('workers'), []);
  assert.deepEqual(s.holders('merge'), []);
});

test('expire removes holders whose lease ran out; renew extends it', () => {
  const c = clock();
  const s = new Semaphores(c.now);
  s.tryAcquire('workers', 'w1', 4, 100);
  s.tryAcquire('workers', 'w2', 4, 100);
  c.advance(60);
  s.renew('w2', 100);
  c.advance(50);
  assert.deepEqual(s.expire(), ['w1']);
  assert.deepEqual(s.holders('workers'), ['w2']);
});

test('expired holders do not count toward capacity but are still reported by expire', () => {
  const c = clock();
  const s = new Semaphores(c.now);
  assert.equal(s.tryAcquire('workers', 'w1', 1, 100), true);
  c.advance(100);
  assert.equal(s.tryAcquire('workers', 'w2', 1, 100), true);
  assert.deepEqual(s.expire(), ['w1']);
  assert.deepEqual(s.holders('workers'), ['w2']);
});

test('renew does not revive an expired lease', () => {
  const c = clock();
  const s = new Semaphores(c.now);
  s.tryAcquire('workers', 'w1', 1, 100);
  c.advance(100);
  s.renew('w1', 100);
  assert.deepEqual(s.expire(), ['w1']);
});
