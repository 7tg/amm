//! Read-only web dashboard served by the daemon. See assets/dashboard/CONTRACT.md.
//!
//! Auth (cookie or header, plus the `/?token=` login redirect) lives in the guard in `http.rs`.
use crate::cost::{ModelUsage, transcript_usage};
use crate::http::AppState;
use crate::service::Amm;
use crate::store::EventFilter;
use crate::types::{AmmEvent, Brief, Run, Session, Worker};
use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

const INDEX_HTML: &str = include_str!("../assets/dashboard/index.html");
const APP_JS: &str = include_str!("../assets/dashboard/app.js");
const STYLE_CSS: &str = include_str!("../assets/dashboard/style.css");

/// How often the events stream polls the store.
const POLL: Duration = Duration::from_millis(500);
/// Keep-alive comment interval on the events stream.
const PING: Duration = Duration::from_secs(15);
/// Most events a worker detail returns (the newest ones).
const DETAIL_EVENTS: usize = 500;

/// Dashboard routes, merged into the daemon router behind the same guard.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/style.css", get(style_css))
        .route("/dash/state", get(state))
        .route("/dash/worker/{id}", get(worker_detail))
        .route("/dash/events", get(events))
}

fn lock(amm: &Mutex<Amm>) -> MutexGuard<'_, Amm> {
    amm.lock().unwrap_or_else(|e| e.into_inner())
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    asset("application/javascript; charset=utf-8", APP_JS)
}

async fn style_css() -> Response {
    asset("text/css; charset=utf-8", STYLE_CSS)
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, axum::Json(json!({ "error": msg }))).into_response()
}

// ---------- cost ----------

type UsageCache = HashMap<PathBuf, (Option<SystemTime>, u64, Arc<Vec<ModelUsage>>)>;

/// Parsed transcript usage, keyed by path and re-read only when (mtime, len) changes.
static USAGE: LazyLock<Mutex<UsageCache>> = LazyLock::new(Default::default);

/// Usage of one transcript; a missing or unreadable file is empty. Blocking.
pub fn cached_usage(path: &Path) -> Arc<Vec<ModelUsage>> {
    let Ok(meta) = std::fs::metadata(path) else { return Arc::new(Vec::new()) };
    let key = (meta.modified().ok(), meta.len());
    {
        let cache = USAGE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((m, l, u)) = cache.get(path)
            && (*m, *l) == key
        {
            return u.clone();
        }
    }
    let usage = Arc::new(transcript_usage(path).unwrap_or_default());
    USAGE.lock().unwrap_or_else(|e| e.into_inner()).insert(path.to_path_buf(), (key.0, key.1, usage.clone()));
    usage
}

/// Where a worker's transcript is: its own recorded path, else beside the main session's transcript
/// (`<dir>/<session>.jsonl` -> `<dir>/<session>/subagents/agent-<agent_id>.jsonl`).
pub fn worker_transcript(w: &Worker, session_transcript: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = w.transcript_path.as_deref().filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let agent = w.agent_id.as_deref().filter(|a| !a.is_empty())?;
    let main = Path::new(session_transcript?);
    let stem = main.file_stem()?.to_str()?;
    Some(main.parent()?.join(stem).join("subagents").join(format!("agent-{agent}.jsonl")))
}

#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    model: Option<String>,
}

fn totals(usage: &[ModelUsage]) -> Totals {
    let mut t = Totals::default();
    let mut best = 0u64;
    for m in usage {
        t.input += m.usage.input;
        t.output += m.usage.output;
        t.cache_read += m.usage.cache_read;
        t.cache_write += m.usage.cache_write_5m + m.usage.cache_write_1h;
        t.cost += m.cost_usd;
        if m.usage.output > best || (t.model.is_none() && m.usage.output > 0) {
            best = m.usage.output;
            t.model = Some(m.model.clone());
        }
    }
    t
}

// ---------- JSON ----------

fn enum_str<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn worker_json(w: &Worker, t: &Totals) -> Value {
    json!({
        "id": w.id, "agentId": w.agent_id, "tier": enum_str(&w.tier), "model": t.model,
        "state": enum_str(&w.state), "branch": w.branch, "toolCalls": w.tool_calls,
        "sinceProgress": w.since_progress, "firedRules": w.fired_rules, "startedAt": w.started_at,
        "lastEventAt": w.last_event_at,
        "tokens": { "input": t.input, "output": t.output, "cacheRead": t.cache_read, "cacheWrite": t.cache_write },
        "costUsd": t.cost,
    })
}

/// A copy of what the dashboard needs, taken under the lock and used without it.
struct Snapshot {
    now: i64,
    runs: Vec<Run>,
    briefs: Vec<Brief>,
    workers: Vec<Worker>,
    sessions: Vec<Session>,
    transcripts: HashMap<String, String>,
}

fn snapshot(amm: &Mutex<Amm>) -> Snapshot {
    let a = lock(amm);
    Snapshot {
        now: (a.now)(),
        runs: a.store.all(),
        briefs: a.store.all(),
        workers: a.store.all(),
        sessions: a.store.all(),
        transcripts: a.session_transcripts.clone(),
    }
}

fn worker_totals(w: &Worker, runs: &HashMap<&str, &Run>, transcripts: &HashMap<String, String>) -> Totals {
    let session = runs.get(w.run_id.as_str()).and_then(|r| r.session_id.as_deref()).and_then(|s| transcripts.get(s));
    match worker_transcript(w, session.map(String::as_str)) {
        Some(p) => totals(&cached_usage(&p)),
        None => Totals::default(),
    }
}

/// The `State` document. Blocking (reads transcripts).
fn build_state(s: Snapshot) -> Value {
    let run_by_id: HashMap<&str, &Run> = s.runs.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut workers_usd = 0.0;
    let mut by_brief: HashMap<&str, Vec<Value>> = HashMap::new();
    let mut run_cost: HashMap<&str, f64> = HashMap::new();
    let mut workers: Vec<&Worker> = s.workers.iter().collect();
    workers.sort_by(|a, b| a.started_at.cmp(&b.started_at).then_with(|| a.id.cmp(&b.id)));
    for w in workers {
        let t = worker_totals(w, &run_by_id, &s.transcripts);
        workers_usd += t.cost;
        *run_cost.entry(w.run_id.as_str()).or_default() += t.cost;
        by_brief.entry(w.brief_id.as_str()).or_default().push(worker_json(w, &t));
    }
    let mut briefs: Vec<&Brief> = s.briefs.iter().collect();
    briefs.sort_by_key(|b| b.seq);
    let mut runs: Vec<&Run> = s.runs.iter().collect();
    runs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
    let runs: Vec<Value> = runs
        .into_iter()
        .map(|r| {
            let briefs: Vec<Value> = briefs
                .iter()
                .filter(|b| b.run_id == r.id)
                .map(|b| {
                    json!({
                        "id": b.id, "seq": b.seq, "title": b.title, "tier": enum_str(&b.tier),
                        "state": enum_str(&b.state), "attempt": b.attempt,
                        "workers": by_brief.remove(b.id.as_str()).unwrap_or_default(),
                    })
                })
                .collect();
            json!({
                "id": r.id, "repo": r.repo, "baseBranch": r.base_branch, "state": enum_str(&r.state),
                "integrationBranch": r.integration_branch, "merges": r.merges, "lastTestOk": r.last_test_ok,
                "createdAt": r.created_at, "costUsd": run_cost.get(r.id.as_str()).copied().unwrap_or(0.0),
                "briefs": briefs,
            })
        })
        .collect();
    let mut managers_usd = 0.0;
    let mut sessions: Vec<&Session> = s.sessions.iter().collect();
    sessions.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.id.cmp(&b.id)));
    let sessions: Vec<Value> = sessions
        .into_iter()
        .map(|se| {
            let cost = s.transcripts.get(&se.id).map(|p| totals(&cached_usage(Path::new(p))).cost).unwrap_or(0.0);
            managers_usd += cost;
            json!({ "id": se.id, "cwd": se.cwd, "lastSeen": se.last_seen, "costUsd": cost })
        })
        .collect();
    json!({
        "now": s.now, "sessions": sessions, "runs": runs,
        "totals": { "workersUsd": workers_usd, "managersUsd": managers_usd },
    })
}

async fn state(State(s): State<AppState>) -> Response {
    let snap = snapshot(&s.amm);
    match tokio::task::spawn_blocking(move || build_state(snap)).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

async fn worker_detail(State(s): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let taken = {
        let a = lock(&s.amm);
        match a.store.get::<Worker>(&id) {
            None => None,
            Some(w) => {
                let brief = a.store.get::<Brief>(&w.brief_id);
                let session = a
                    .store
                    .get::<Run>(&w.run_id)
                    .and_then(|r| r.session_id)
                    .and_then(|sid| a.session_transcripts.get(&sid).cloned());
                let filter = EventFilter { worker_id: Some(w.id.clone()), limit: Some(10_000), ..Default::default() };
                let events = a.store.events(&filter);
                Some((w, brief, session, events))
            }
        }
    };
    let Some((w, brief, session, events)) = taken else { return err(StatusCode::NOT_FOUND, "not found") };
    let Ok(mut events) = events else { return err(StatusCode::INTERNAL_SERVER_ERROR, "store error") };
    if events.len() > DETAIL_EVENTS {
        events.drain(..events.len() - DETAIL_EVENTS);
    }
    let built = tokio::task::spawn_blocking(move || {
        let t = match worker_transcript(&w, session.as_deref()) {
            Some(p) => totals(&cached_usage(&p)),
            None => Totals::default(),
        };
        let brief = brief.map(|b| {
            json!({
                "id": b.id, "title": b.title, "goal": b.goal, "scope": b.scope, "checks": b.checks, "notes": b.notes,
                "report": b.report.map(|r| json!({ "status": enum_str(&r.status), "files": r.files, "notes": r.notes })),
            })
        });
        let events: Vec<Value> =
            events.into_iter().map(|e| json!({ "id": e.id, "ts": e.ts, "kind": e.kind, "detail": e.detail })).collect();
        json!({ "worker": worker_json(&w, &t), "brief": brief, "events": events })
    })
    .await;
    match built {
        Ok(v) => axum::Json(v).into_response(),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

// ---------- events stream ----------

/// The newest event id in the store.
fn newest_event_id(a: &Amm) -> i64 {
    let mut cursor = a.store.all::<Run>().iter().filter_map(|r| a.store.last_event_id(&r.id).ok()).max().unwrap_or(0);
    // Events without a run (or newer than every run's last) still count.
    loop {
        let page = a.store.events(&EventFilter { after_id: Some(cursor), limit: Some(10_000), ..Default::default() });
        match page.ok().and_then(|p| p.last().map(|e| e.id)) {
            Some(id) => cursor = id,
            None => return cursor,
        }
    }
}

struct Stream {
    amm: Arc<Mutex<Amm>>,
    cursor: i64,
    pending: VecDeque<AmmEvent>,
    started: bool,
}

fn sse_event(e: &AmmEvent) -> Event {
    let data = serde_json::to_string(e).unwrap_or_else(|_| "{}".into());
    Event::default().event("amm").id(e.id.to_string()).data(data)
}

async fn events(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let resume = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok());
    let cursor = match resume {
        Some(id) => id.max(0),
        None => newest_event_id(&lock(&s.amm)),
    };
    let init = Stream { amm: s.amm.clone(), cursor, pending: VecDeque::new(), started: false };
    let stream = futures_util::stream::unfold(init, |mut st| async move {
        if !st.started {
            st.started = true;
            return Some((Ok::<_, Infallible>(Event::default().retry(Duration::from_millis(2000))), st));
        }
        loop {
            if let Some(e) = st.pending.pop_front() {
                return Some((Ok(sse_event(&e)), st));
            }
            tokio::time::sleep(POLL).await;
            let page = {
                let a = lock(&st.amm);
                a.store.events(&EventFilter { after_id: Some(st.cursor), limit: Some(1000), ..Default::default() })
            };
            if let Ok(page) = page {
                if let Some(last) = page.last() {
                    st.cursor = last.id;
                }
                st.pending.extend(page);
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(PING).text("ping")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::Usage;
    use crate::types::{Tier, WorkerState};

    fn worker(agent: Option<&str>, transcript: Option<&str>) -> Worker {
        Worker {
            id: "w1".into(),
            brief_id: "b1".into(),
            run_id: "r1".into(),
            agent_id: agent.map(Into::into),
            tier: Tier::Light,
            worktree: None,
            branch: None,
            gated_sha: None,
            base_sha: None,
            transcript_path: transcript.map(Into::into),
            state: WorkerState::Running,
            tool_calls: 0,
            since_progress: 0,
            last_cmd: None,
            last_cmd_fails: 0,
            stop_blocks: 0,
            fired_rules: vec![],
            pending_correction: None,
            started_at: 0,
            last_event_at: 0,
        }
    }

    #[test]
    fn worker_transcript_prefers_own_then_derives_from_session() {
        let w = worker(Some("abc"), Some("/x/own.jsonl"));
        assert_eq!(worker_transcript(&w, Some("/p/s1.jsonl")), Some(PathBuf::from("/x/own.jsonl")));
        let w = worker(Some("abc"), None);
        assert_eq!(worker_transcript(&w, Some("/p/s1.jsonl")), Some(PathBuf::from("/p/s1/subagents/agent-abc.jsonl")));
        assert_eq!(worker_transcript(&w, None), None);
        assert_eq!(worker_transcript(&worker(None, None), Some("/p/s1.jsonl")), None);
    }

    #[test]
    fn totals_pick_model_with_most_output() {
        let mu = |m: &str, out: u64, cost: f64| ModelUsage {
            model: m.into(),
            usage: Usage { input: 1, output: out, cache_read: 2, cache_write_5m: 3, cache_write_1h: 4 },
            cost_usd: cost,
        };
        let t = totals(&[mu("a", 5, 1.0), mu("b", 9, 2.0)]);
        assert_eq!((t.input, t.output, t.cache_read, t.cache_write), (2, 14, 4, 14));
        assert_eq!(t.model.as_deref(), Some("b"));
        assert!((t.cost - 3.0).abs() < 1e-9);
        assert_eq!(totals(&[]).model, None);
    }
}
