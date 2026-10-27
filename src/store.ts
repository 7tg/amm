import { DatabaseSync, type StatementSync } from 'node:sqlite';
import type { AmmEvent, Brief, Run, Session, Worker } from './types.ts';

interface Docs {
  runs: Run;
  briefs: Brief;
  workers: Worker;
  sessions: Session;
}
type Table = keyof Docs;
const TABLES: Table[] = ['runs', 'briefs', 'workers', 'sessions'];

export interface EventFilter {
  runId?: string;
  workerId?: string;
  afterId?: number;
  limit?: number;
}

/** In-memory documents, written through to SQLite. Only the daemon process opens this. */
export class Store {
  private db: DatabaseSync;
  private cache: { [K in Table]: Map<string, Docs[K]> } = {
    runs: new Map(),
    briefs: new Map(),
    workers: new Map(),
    sessions: new Map(),
  };

  private putStmts = new Map<Table, StatementSync>();

  constructor(file: string) {
    this.db = new DatabaseSync(file);
    this.db.exec('PRAGMA journal_mode = WAL');
    this.db.exec('PRAGMA busy_timeout = 5000');
    for (const t of TABLES) this.db.exec(`CREATE TABLE IF NOT EXISTS ${t} (id TEXT PRIMARY KEY, data TEXT NOT NULL)`);
    this.db.exec(
      'CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, run_id TEXT, worker_id TEXT, kind TEXT NOT NULL, detail TEXT NOT NULL)',
    );
    this.db.exec('CREATE INDEX IF NOT EXISTS events_run ON events (run_id, id)');
    this.db.exec('CREATE INDEX IF NOT EXISTS events_worker ON events (worker_id, id)');
    for (const t of TABLES) {
      const map = this.cache[t] as Map<string, { id: string }>;
      for (const row of this.db.prepare(`SELECT data FROM ${t}`).all()) {
        const doc = JSON.parse(String(row.data)) as { id: string };
        map.set(doc.id, doc);
      }
    }
  }

  get<T extends Table>(t: T, id: string): Docs[T] | undefined {
    return (this.cache[t] as Map<string, Docs[T]>).get(id);
  }

  all<T extends Table>(t: T): Docs[T][] {
    return [...(this.cache[t] as Map<string, Docs[T]>).values()];
  }

  put<T extends Table>(t: T, doc: Docs[T]): void {
    let stmt = this.putStmts.get(t);
    if (!stmt) {
      stmt = this.db.prepare(`INSERT OR REPLACE INTO ${t} (id, data) VALUES (?, ?)`);
      this.putStmts.set(t, stmt);
    }
    stmt.run(doc.id, JSON.stringify(doc));
    (this.cache[t] as Map<string, Docs[T]>).set(doc.id, structuredClone(doc));
  }

  addEvent(e: Omit<AmmEvent, 'id'>): number {
    const r = this.db
      .prepare('INSERT INTO events (ts, run_id, worker_id, kind, detail) VALUES (?, ?, ?, ?, ?)')
      .run(e.ts, e.runId, e.workerId, e.kind, JSON.stringify(e.detail));
    return Number(r.lastInsertRowid);
  }

  events(f: EventFilter = {}): AmmEvent[] {
    const rows = this.db
      .prepare(
        'SELECT * FROM events WHERE id > ? AND (? IS NULL OR run_id = ?) AND (? IS NULL OR worker_id = ?) ORDER BY id LIMIT ?',
      )
      .all(f.afterId ?? 0, f.runId ?? null, f.runId ?? null, f.workerId ?? null, f.workerId ?? null, Math.min(10000, Math.max(1, f.limit ?? 1000)));
    return rows.map((r) => ({
      id: Number(r.id),
      ts: Number(r.ts),
      runId: (r.run_id as string | null) ?? null,
      workerId: (r.worker_id as string | null) ?? null,
      kind: String(r.kind),
      detail: JSON.parse(String(r.detail)) as Record<string, unknown>,
    }));
  }

  close(): void {
    this.db.close();
  }
}
