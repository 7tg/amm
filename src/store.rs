use crate::types::{AmmEvent, Brief, Run, Session, Worker};
use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

pub type StoreError = rusqlite::Error;

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub run_id: Option<String>,
    pub worker_id: Option<String>,
    pub after_id: Option<i64>,
    pub kinds: Option<Vec<String>>,
    pub limit: Option<i64>,
}

/// A stored document type: lives in its own table, keyed by `id`.
pub trait Doc: Sized + Clone + Serialize + DeserializeOwned {
    const TABLE: &'static str;
    fn doc_id(&self) -> &str;
    fn cache(c: &Cache) -> &HashMap<String, Self>;
    fn cache_mut(c: &mut Cache) -> &mut HashMap<String, Self>;
}

#[derive(Default)]
pub struct Cache {
    runs: HashMap<String, Run>,
    briefs: HashMap<String, Brief>,
    workers: HashMap<String, Worker>,
    sessions: HashMap<String, Session>,
}

macro_rules! doc {
    ($ty:ty, $table:literal, $field:ident) => {
        impl Doc for $ty {
            const TABLE: &'static str = $table;
            fn doc_id(&self) -> &str {
                &self.id
            }
            fn cache(c: &Cache) -> &HashMap<String, Self> {
                &c.$field
            }
            fn cache_mut(c: &mut Cache) -> &mut HashMap<String, Self> {
                &mut c.$field
            }
        }
    };
}
doc!(Run, "runs", runs);
doc!(Brief, "briefs", briefs);
doc!(Worker, "workers", workers);
doc!(Session, "sessions", sessions);

/// In-memory documents, written through to SQLite. Only the daemon process opens this.
/// Nothing here panics: a panic while the daemon holds its mutex would poison it.
pub struct Store {
    db: Connection,
    cache: Cache,
}

fn load<T: Doc>(db: &Connection, cache: &mut Cache) -> Result<(), StoreError> {
    let mut stmt = db.prepare(&format!("SELECT data FROM {}", T::TABLE))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    for row in rows {
        let doc: T = serde_json::from_str(&row?)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        T::cache_mut(cache).insert(doc.doc_id().to_string(), doc);
    }
    Ok(())
}

impl Store {
    pub fn open(file: &Path) -> Result<Store, StoreError> {
        // Create the file private from the start; the db holds briefs and transcripts' paths.
        if !file.exists() {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(file);
        }
        let db = Connection::open(file)?;
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
        db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        db.busy_timeout(Duration::from_millis(5000))?;
        for t in [Run::TABLE, Brief::TABLE, Worker::TABLE, Session::TABLE] {
            db.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS {t} (id TEXT PRIMARY KEY, data TEXT NOT NULL)"
            ))?;
        }
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, run_id TEXT, worker_id TEXT, kind TEXT NOT NULL, detail TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS events_run ON events (run_id, id);
             CREATE INDEX IF NOT EXISTS events_worker ON events (worker_id, id);",
        )?;
        let mut cache = Cache::default();
        load::<Run>(&db, &mut cache)?;
        load::<Brief>(&db, &mut cache)?;
        load::<Worker>(&db, &mut cache)?;
        load::<Session>(&db, &mut cache)?;
        // WAL side files are created lazily with the process umask; tighten them if present.
        for ext in ["-wal", "-shm"] {
            let mut p = file.as_os_str().to_owned();
            p.push(ext);
            let _ = std::fs::set_permissions(Path::new(&p), std::fs::Permissions::from_mode(0o600));
        }
        Ok(Store { db, cache })
    }

    pub fn get<T: Doc>(&self, id: &str) -> Option<T> {
        T::cache(&self.cache).get(id).cloned()
    }

    pub fn all<T: Doc>(&self) -> Vec<T> {
        T::cache(&self.cache).values().cloned().collect()
    }

    /// Writes to the database first; the cache only changes if that succeeded.
    pub fn put<T: Doc>(&mut self, doc: &T) -> Result<(), StoreError> {
        let data = serde_json::to_string(doc)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.db
            .prepare_cached(&format!(
                "INSERT OR REPLACE INTO {} (id, data) VALUES (?1, ?2)",
                T::TABLE
            ))?
            .execute(params![doc.doc_id(), data])?;
        T::cache_mut(&mut self.cache).insert(doc.doc_id().to_string(), doc.clone());
        Ok(())
    }

    /// Appends an event (its `id` is ignored) and returns the new id.
    pub fn add_event(&self, e: &AmmEvent) -> Result<i64, StoreError> {
        let detail = serde_json::to_string(&e.detail)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.db.execute(
            "INSERT INTO events (ts, run_id, worker_id, kind, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![e.ts, e.run_id, e.worker_id, e.kind, detail],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn events(&self, f: &EventFilter) -> Result<Vec<AmmEvent>, StoreError> {
        let mut sql = String::from(
            "SELECT id, ts, run_id, worker_id, kind, detail FROM events WHERE id > ? AND (? IS NULL OR run_id = ?) AND (? IS NULL OR worker_id = ?)",
        );
        let mut p: Vec<Value> = vec![
            f.after_id.unwrap_or(0).into(),
            f.run_id.clone().into(),
            f.run_id.clone().into(),
            f.worker_id.clone().into(),
            f.worker_id.clone().into(),
        ];
        if let Some(kinds) = &f.kinds {
            if kinds.is_empty() {
                sql.push_str(" AND kind IN (NULL)");
            } else {
                sql.push_str(&format!(
                    " AND kind IN ({})",
                    vec!["?"; kinds.len()].join(",")
                ));
                p.extend(kinds.iter().cloned().map(Value::from));
            }
        }
        sql.push_str(" ORDER BY id LIMIT ?");
        p.push(f.limit.unwrap_or(1000).clamp(1, 10000).into());
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(p), |r| {
            let detail: String = r.get(5)?;
            Ok(AmmEvent {
                id: r.get(0)?,
                ts: r.get(1)?,
                run_id: r.get(2)?,
                worker_id: r.get(3)?,
                kind: r.get(4)?,
                detail: serde_json::from_str(&detail).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
                })?,
            })
        })?;
        rows.collect()
    }

    pub fn last_event_id(&self, run_id: &str) -> Result<i64, StoreError> {
        Ok(self.db
            .query_row(
                "SELECT MAX(id) FROM events WHERE run_id = ?1",
                [run_id],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmp_dir;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn db_file() -> PathBuf {
        tmp_dir().join("amm.db")
    }
    fn sess(id: &str, cwd: &str, last: i64) -> Session {
        Session {
            id: id.into(),
            cwd: cwd.into(),
            first_seen: 1,
            last_seen: last,
        }
    }
    fn ev(
        ts: i64,
        run: Option<&str>,
        worker: Option<&str>,
        kind: &str,
        detail: serde_json::Value,
    ) -> AmmEvent {
        AmmEvent {
            id: 0,
            ts,
            run_id: run.map(Into::into),
            worker_id: worker.map(Into::into),
            kind: kind.into(),
            detail: detail.as_object().unwrap().clone(),
        }
    }
    fn json(v: serde_json::Value) -> serde_json::Value {
        v
    }

    #[test]
    fn documents_survive_reopening_the_database() {
        let file = db_file();
        let mut s1 = Store::open(&file).unwrap();
        let doc = sess("s1", "/repo", 2);
        s1.put(&doc).unwrap();
        drop(s1);
        let s2 = Store::open(&file).unwrap();
        assert_eq!(s2.get::<Session>("s1"), Some(doc));
        assert_eq!(s2.all::<Session>().len(), 1);
    }

    #[test]
    fn events_are_filtered_by_run_worker_and_cursor() {
        let s = Store::open(&db_file()).unwrap();
        let a = s
            .add_event(&ev(
                1,
                Some("r1"),
                Some("w1"),
                "tool",
                json(serde_json::json!({"n": 1})),
            ))
            .unwrap();
        s.add_event(&ev(2, Some("r2"), None, "run_start", serde_json::json!({})))
            .unwrap();
        s.add_event(&ev(
            3,
            Some("r1"),
            Some("w2"),
            "done",
            serde_json::json!({}),
        ))
        .unwrap();
        let by_run = s.events(&EventFilter {
            run_id: Some("r1".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(by_run.iter().map(|e| e.ts).collect::<Vec<_>>(), [1, 3]);
        let by_worker = s.events(&EventFilter {
            worker_id: Some("w1".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            serde_json::Value::Object(by_worker[0].detail.clone()),
            serde_json::json!({"n": 1})
        );
        let after = s.events(&EventFilter {
            run_id: Some("r1".into()),
            after_id: Some(a),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            after.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["done"]
        );
    }

    #[test]
    fn put_overwrites_a_document_and_stores_a_copy() {
        let file = db_file();
        let mut s = Store::open(&file).unwrap();
        let mut doc = sess("s1", "/a", 1);
        s.put(&doc).unwrap();
        doc.cwd = "mutated".into();
        assert_eq!(s.get::<Session>("s1").unwrap().cwd, "/a");
        s.put(&sess("s1", "/b", 2)).unwrap();
        assert_eq!(s.all::<Session>().len(), 1);
        drop(s);
        let s2 = Store::open(&file).unwrap();
        assert_eq!(s2.get::<Session>("s1").unwrap().cwd, "/b");
    }

    #[test]
    fn events_respects_limit_and_clamps_it_to_at_least_1() {
        let s = Store::open(&db_file()).unwrap();
        for i in 1..=5 {
            s.add_event(&ev(i, Some("r"), None, "k", serde_json::json!({})))
                .unwrap();
        }
        assert_eq!(
            s.events(&EventFilter {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            s.events(&EventFilter {
                limit: Some(0),
                ..Default::default()
            })
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            s.events(&EventFilter {
                limit: Some(-5),
                ..Default::default()
            })
            .unwrap()
            .len(),
            1
        );
        assert_eq!(s.events(&EventFilter::default()).unwrap().len(), 5);
    }

    #[test]
    fn get_and_all_return_copies_mutating_them_does_not_change_the_store() {
        let file = db_file();
        let mut s = Store::open(&file).unwrap();
        s.put(&sess("s1", "/a", 1)).unwrap();
        s.get::<Session>("s1").unwrap().cwd = "x".into();
        s.all::<Session>()[0].cwd = "y".into();
        assert_eq!(s.get::<Session>("s1").unwrap().cwd, "/a");
        drop(s);
        let s2 = Store::open(&file).unwrap();
        assert_eq!(s2.get::<Session>("s1").unwrap().cwd, "/a");
    }

    #[test]
    fn events_kinds_filter_and_last_event_id() {
        let s = Store::open(&db_file()).unwrap();
        s.add_event(&ev(1, Some("r"), None, "a", serde_json::json!({})))
            .unwrap();
        s.add_event(&ev(2, Some("r"), None, "b", serde_json::json!({})))
            .unwrap();
        let last = s
            .add_event(&ev(3, Some("r"), None, "c", serde_json::json!({})))
            .unwrap();
        s.add_event(&ev(4, Some("other"), None, "a", serde_json::json!({})))
            .unwrap();
        let kinds = |k: &[&str]| {
            let f = EventFilter {
                kinds: Some(k.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            };
            s.events(&f).unwrap().iter().map(|e| e.ts).collect::<Vec<_>>()
        };
        assert_eq!(kinds(&["a", "c"]), [1, 3, 4]);
        assert!(kinds(&[]).is_empty());
        assert_eq!(s.last_event_id("r").unwrap(), last);
        assert_eq!(s.last_event_id("nope").unwrap(), 0);
        let none = s.events(&EventFilter {
            run_id: Some("r".into()),
            after_id: Some(last),
            ..Default::default()
        })
        .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn database_file_is_mode_0600_and_uses_wal() {
        let file = db_file();
        let s = Store::open(&file).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        for ext in ["-wal", "-shm"] {
            let mut p = file.as_os_str().to_owned();
            p.push(ext);
            let m = std::fs::metadata(Path::new(&p)).unwrap().permissions().mode() & 0o777;
            assert_eq!(m, 0o600, "{ext}");
        }
        let mut s = s;
        s.put(&sess("s1", "/a", 1)).unwrap();
        let data: String = s.db.query_row("SELECT data FROM sessions WHERE id = 's1'", [], |r| r.get(0)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["cwd", "firstSeen", "id", "lastSeen"]);
        let jm: String =
            s.db.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
        assert_eq!(jm, "wal");
        let bt: i64 =
            s.db.query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap();
        assert_eq!(bt, 5000);
        let n: i64 = s
            .db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('events_run','events_worker')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn corrupt_event_detail_is_an_error_not_an_empty_object() {
        let s = Store::open(&db_file()).unwrap();
        s.db.execute(
            "INSERT INTO events (ts, run_id, worker_id, kind, detail) VALUES (1, 'r', NULL, 'k', '{not json')",
            [],
        )
        .unwrap();
        assert!(s.events(&EventFilter::default()).is_err());
    }

    #[test]
    fn a_failed_db_write_leaves_the_cache_untouched() {
        let mut s = Store::open(&db_file()).unwrap();
        s.put(&sess("s1", "/a", 1)).unwrap();
        s.db.execute_batch("DROP TABLE sessions").unwrap();
        assert!(s.put(&sess("s1", "/b", 2)).is_err());
        assert_eq!(s.get::<Session>("s1").unwrap().cwd, "/a");
        s.db.execute_batch("DROP TABLE events").unwrap();
        assert!(s.add_event(&ev(1, Some("r"), None, "k", serde_json::json!({}))).is_err());
        assert!(s.events(&EventFilter::default()).is_err());
        assert!(s.last_event_id("r").is_err());
    }
}
