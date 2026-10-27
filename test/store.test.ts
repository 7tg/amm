import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { Store } from '../src/store.ts';
import type { Session } from '../src/types.ts';

const dbFile = () => path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'amm-store-')), 'amm.db');

test('documents survive reopening the database', () => {
  const file = dbFile();
  const s1 = new Store(file);
  const doc: Session = { id: 's1', cwd: '/repo', firstSeen: 1, lastSeen: 2 };
  s1.put('sessions', doc);
  s1.close();
  const s2 = new Store(file);
  assert.deepEqual(s2.get('sessions', 's1'), doc);
  assert.equal(s2.all('sessions').length, 1);
  s2.close();
});

test('events are filtered by run, worker and cursor', () => {
  const s = new Store(dbFile());
  const a = s.addEvent({ ts: 1, runId: 'r1', workerId: 'w1', kind: 'tool', detail: { n: 1 } });
  s.addEvent({ ts: 2, runId: 'r2', workerId: null, kind: 'run_start', detail: {} });
  s.addEvent({ ts: 3, runId: 'r1', workerId: 'w2', kind: 'done', detail: {} });
  assert.deepEqual(s.events({ runId: 'r1' }).map((e) => e.ts), [1, 3]);
  assert.deepEqual(s.events({ workerId: 'w1' })[0].detail, { n: 1 });
  assert.deepEqual(s.events({ runId: 'r1', afterId: a }).map((e) => e.kind), ['done']);
  s.close();
});
