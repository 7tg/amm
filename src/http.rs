//! The daemon's HTTP API: `/health`, `/api/{op}` (the MCP tools without their `amm_` prefix) and `/hook/{event}`.
//! Every request must carry a local Host header (else 403) and the lock file's token (else 401), in that order.
use crate::hooks::{HookInput, try_handle_hook};
use crate::checks::run_check;
use crate::git::worktree_info;
use crate::integration::{create_worktree, integration_branch, remove_worker_worktree, verify_clean};
use crate::service::{
    Amm, AmmError, AmmResult, BriefInput, BriefPatch, StartRun, new_run_id, resolve_base_branch, resolve_repo,
};
use crate::types::{Brief, BriefState, Report, ReportStatus, Tier};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tower_http::catch_panic::CatchPanicLayer;

/// Largest request body accepted.
pub const MAX_BODY: usize = 1_000_000;
/// Largest plan file `plan_import` reads.
pub const MAX_PLAN: u64 = 5_000_000;
/// Longest a request body may take to arrive.
pub const BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct AppState {
    pub amm: Arc<Mutex<Amm>>,
    pub token: Arc<str>,
    /// The port the daemon listens on; Host must name it.
    pub port: u16,
    /// Authenticated requests, which keep the daemon from idling out.
    pub activity: Arc<Activity>,
}

/// When the last authenticated request ended, and how many are still running.
pub struct Activity {
    last_ms: AtomicI64,
    in_flight: AtomicUsize,
}

impl Default for Activity {
    fn default() -> Self {
        Activity { last_ms: AtomicI64::new(now_ms()), in_flight: AtomicUsize::new(0) }
    }
}

impl Activity {
    /// How long the daemon has had no authenticated request running, or None while one is.
    pub fn idle_ms(&self) -> Option<i64> {
        (self.in_flight.load(Ordering::SeqCst) == 0).then(|| now_ms() - self.last_ms.load(Ordering::SeqCst))
    }

    fn touch(&self) {
        self.last_ms.store(now_ms(), Ordering::SeqCst);
    }

    /// Counts a request as running until the guard drops, also when the request is cancelled.
    fn enter(self: &Arc<Self>) -> InFlight {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        InFlight(self.clone())
    }
}

struct InFlight(Arc<Activity>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.touch();
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn lock(amm: &Mutex<Amm>) -> MutexGuard<'_, Amm> {
    // A handler that panicked while holding the lock must not take every later request down with it.
    amm.lock().unwrap_or_else(|e| e.into_inner())
}

fn send(code: StatusCode, body: Value) -> Response {
    (code, axum::Json(body)).into_response()
}

fn error(code: StatusCode, msg: impl Into<String>) -> Response {
    send(code, json!({ "error": msg.into() }))
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

fn too_large() -> Response {
    let mut r = error(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
    r.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
    r
}

/// Constant-time compare (the length is not secret).
fn same(given: &[u8], token: &[u8]) -> bool {
    if given.len() != token.len() {
        return false;
    }
    let diff = given.iter().zip(token).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    std::hint::black_box(diff) == 0
}

/// Constant-time token compare. A missing or repeated header never matches.
fn token_ok(req: &Request, token: &[u8]) -> bool {
    let mut values = req.headers().get_all("x-amm-token").iter();
    let (Some(given), None) = (values.next(), values.next()) else { return false };
    same(given.as_bytes(), token)
}

/// Whether the `amm_token` cookie carries the token. Several `amm_token` cookies never match.
fn cookie_ok(req: &Request, token: &[u8]) -> bool {
    let mut found = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .flat_map(|v| v.as_bytes().split(|&b| b == b';'))
        .filter_map(|c| c.trim_ascii().strip_prefix(b"amm_token="));
    let (Some(given), None) = (found.next(), found.next()) else { return false };
    same(given, token)
}

/// Dashboard paths also accept the cookie; `/api`, `/hook` and `/health` stay header-only.
fn dashboard_path(path: &str) -> bool {
    path == "/" || path.starts_with("/assets/") || path.starts_with("/dash/")
}

/// `GET /?token=<t>`: the token from the query, if one is given.
fn login_token(req: &Request) -> Option<String> {
    if req.uri().path() != "/" || req.method() != Method::GET {
        return None;
    }
    req.uri().query()?.split('&').find_map(|kv| kv.strip_prefix("token=")).map(str::to_string)
}

async fn guard(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok());
    let local = [format!("127.0.0.1:{}", s.port), format!("localhost:{}", s.port)];
    if !host.is_some_and(|h| local.iter().any(|l| l == h)) {
        return error(StatusCode::FORBIDDEN, "bad host");
    }
    if let Some(given) = login_token(&req) {
        if !same(given.as_bytes(), s.token.as_bytes()) {
            return error(StatusCode::UNAUTHORIZED, "bad token");
        }
        s.activity.touch();
        let cookie = format!("amm_token={}; HttpOnly; SameSite=Strict; Path=/", s.token);
        let mut r = StatusCode::SEE_OTHER.into_response();
        r.headers_mut().insert(header::LOCATION, HeaderValue::from_static("/"));
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            r.headers_mut().insert(header::SET_COOKIE, v);
        }
        return r;
    }
    let cookie = dashboard_path(req.uri().path()) && cookie_ok(&req, s.token.as_bytes());
    if !cookie && !token_ok(&req, s.token.as_bytes()) {
        return error(StatusCode::UNAUTHORIZED, "bad token");
    }
    let _busy = s.activity.enter();
    next.run(req).await
}

/// The request body as JSON; an empty body is `{}`.
async fn read_json(req: Request) -> Result<Value, Box<Response>> {
    let declared = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BODY as u64) {
        return Err(Box::new(too_large()));
    }
    // DefaultBodyLimit bounds this read, also for bodies of unknown length.
    // A client that stalls mid-body must not hold its connection (and a slot) forever.
    let read = tokio::time::timeout(BODY_TIMEOUT, Bytes::from_request(req, &())).await;
    let Ok(read) = read else {
        let mut r = error(StatusCode::REQUEST_TIMEOUT, "body timeout");
        r.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
        return Err(Box::new(r));
    };
    let bytes = match read {
        Ok(b) => b,
        Err(r) if r.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(Box::new(too_large())),
        Err(r) => return Err(Box::new(error(StatusCode::BAD_REQUEST, r.body_text()))),
    };
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&String::from_utf8_lossy(&bytes)).map_err(|e| Box::new(error(StatusCode::BAD_REQUEST, e.to_string())))
}

/// The raw (not percent-decoded) second path segment, as the TS server split it.
fn segment(req: &Request) -> String {
    req.uri().path().split('/').nth(2).unwrap_or("").to_string()
}

async fn health(req: Request) -> Response {
    if req.method() == Method::GET {
        return send(StatusCode::OK, json!({ "ok": true, "pid": std::process::id() }));
    }
    not_found()
}

async fn api(State(s): State<AppState>, req: Request) -> Response {
    let (post, op) = (req.method() == Method::POST, segment(&req));
    let body = match read_json(req).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if !post || !OPS.contains(&op.as_str()) {
        return not_found();
    }
    match run_op(&s, &op, body).await {
        Ok(result) => send(StatusCode::OK, json!({ "result": result })),
        Err(e) => error(StatusCode::BAD_REQUEST, e.0),
    }
}

async fn hook(State(s): State<AppState>, req: Request) -> Response {
    let (post, event) = (req.method() == Method::POST, segment(&req));
    let body = match read_json(req).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if !post {
        return not_found();
    }
    let input: HookInput = match serde_json::from_value(body) {
        Ok(i) => i,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    match try_handle_hook(&s.amm, &event, input).await {
        Ok(v) => send(StatusCode::OK, v),
        Err(e) => error(StatusCode::BAD_REQUEST, e.0),
    }
}

async fn fallback() -> Response {
    not_found()
}

fn panic_response(_: Box<dyn std::any::Any + Send + 'static>) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
}

/// Wraps routes in the auth guard, the body limit and the panic catcher.
fn finish(routes: Router<AppState>, state: AppState) -> Router {
    routes
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(CatchPanicLayer::custom(panic_response))
        .with_state(state)
}

pub fn router(state: AppState) -> Router {
    let routes = Router::new().route("/health", any(health)).route("/api/{op}", any(api)).route("/hook/{event}", any(hook)).merge(crate::dashboard::routes());
    finish(routes, state)
}

/// API operations, named after the MCP tools without the "amm_" prefix. Results are kept small on purpose.
pub const OPS: [&str; 12] = [
    "run_start",
    "brief_add",
    "plan_import",
    "brief_update",
    "dispatch",
    "brief_get",
    "report",
    "status",
    "nudge",
    "cancel",
    "resume",
    "integrate",
];

fn args<T: DeserializeOwned>(body: Value) -> AmmResult<T> {
    serde_json::from_value(body).map_err(|e| AmmError(e.to_string()))
}

fn compact(b: &Brief) -> Value {
    json!({ "id": b.id, "title": b.title, "state": b.state, "tier": b.tier, "scope": b.scope })
}

/// Runs `f` on the blocking pool, for git and file reads that must not run under the lock.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> AmmResult<T> + Send + 'static) -> AmmResult<T> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| Err(AmmError(format!("internal: {e}"))))
}

#[derive(Deserialize)]
struct RunStartArgs {
    repo: String,
    test_cmd: String,
    base_branch: Option<String>,
    setup_cmd: Option<String>,
}

#[derive(Deserialize)]
struct BriefAddArgs {
    run_id: String,
    title: String,
    goal: String,
    scope: Vec<String>,
    checks: Option<Vec<String>>,
    tier: Tier,
    max_turns: Option<i64>,
}

#[derive(Deserialize)]
struct PlanImportArgs {
    run_id: String,
    path: String,
}

#[derive(Deserialize)]
struct BriefUpdateArgs {
    brief_id: String,
    title: Option<String>,
    goal: Option<String>,
    scope: Option<Vec<String>>,
    checks: Option<Vec<String>>,
    tier: Option<Tier>,
    max_turns: Option<i64>,
    ready: Option<bool>,
}

#[derive(Deserialize)]
struct RunIdArgs {
    run_id: String,
}

#[derive(Deserialize)]
struct TokenArgs {
    token: String,
}

#[derive(Deserialize)]
struct ReportArgs {
    token: String,
    status: ReportStatus,
    files: Option<Vec<String>>,
    notes: Option<String>,
}

#[derive(Deserialize)]
struct StatusArgs {
    run_id: Option<String>,
    full: Option<Value>,
}

#[derive(Deserialize)]
struct NudgeArgs {
    brief_id: String,
    message: String,
}

#[derive(Deserialize)]
struct CancelArgs {
    id: String,
}

/// Reads a plan file, refusing anything over `MAX_PLAN`. Invalid UTF-8 is replaced, like Node's `utf8` decoding.
fn read_plan(path: &str) -> AmmResult<String> {
    let fail = |e: std::io::Error| AmmError(format!("cannot read {path}: {e}"));
    let f = std::fs::File::open(path).map_err(fail)?;
    let mut buf = Vec::new();
    f.take(MAX_PLAN + 1).read_to_end(&mut buf).map_err(fail)?;
    if buf.len() as u64 > MAX_PLAN {
        return Err(AmmError(format!("plan {path} is too large (over {MAX_PLAN} bytes)")));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn run_op(s: &AppState, op: &str, body: Value) -> AmmResult<Value> {
    let amm = &s.amm;
    Ok(match op {
        "run_start" => {
            let a: RunStartArgs = args(body)?;
            let budget = lock(amm).limits.gate_budget_ms.max(0) as u64;
            let id = new_run_id();
            let (setup_cmd, run_id) = (a.setup_cmd.clone().filter(|c| !c.trim().is_empty()), id.clone());
            // Resolve, create the integration worktree and run the setup, all outside the lock.
            let (repo, base_branch, wt, head) = blocking(move || {
                let given = resolve_repo(Path::new(&a.repo));
                let repo = worktree_info(&given)
                    .map_err(|e| AmmError(format!("{} is not a git repository: {}", given.display(), e.lines().next().unwrap_or(""))))?
                    .toplevel;
                let base = match a.base_branch {
                    Some(b) => b,
                    None => resolve_base_branch(&repo)?,
                };
                let (wt, head) = create_worktree(&repo, &run_id, &base)?;
                let branch = integration_branch(&run_id);
                // From here every failure removes the worktree and its branch again.
                let set_up = (|| {
                    let Some(cmd) = &setup_cmd else { return Ok(()) };
                    let r = run_check(cmd, &wt, budget);
                    if !r.ok {
                        let how = if r.timed_out { "timed out".to_string() } else { format!("exited {}", r.code.unwrap_or(-1)) };
                        let n = r.output.chars().count();
                        let tail: String = r.output.chars().skip(n.saturating_sub(1500)).collect();
                        return Err(format!("setup_cmd `{cmd}` {how}:\n{tail}"));
                    }
                    // setup_cmd runs repo code: the worktree must still be the base commit, changed only in ignored files.
                    verify_clean(&wt, &branch, &head).map_err(|e| format!("setup_cmd `{cmd}` changed the integration worktree: {e}"))
                })();
                if let Err(e) = set_up {
                    let _ = remove_worker_worktree(&repo, &wt, &branch);
                    return Err(AmmError(e));
                }
                Ok((repo, base, wt, head))
            })
            .await?;
            let input = StartRun {
                repo: repo.to_string_lossy().into_owned(),
                test_cmd: a.test_cmd,
                base_branch,
                id: Some(id.clone()),
                integration_worktree: Some(wt.to_string_lossy().into_owned()),
                integration_head: Some(head),
                setup_cmd: a.setup_cmd,
            };
            let started = lock(amm).start_run(input);
            if let Err(e) = started {
                let branch = integration_branch(&id);
                let _ = blocking(move || Ok(remove_worker_worktree(&repo, &wt, &branch))).await;
                return Err(e);
            }
            json!({ "run_id": id })
        }
        "brief_add" => {
            let a: BriefAddArgs = args(body)?;
            let input = BriefInput {
                title: a.title,
                goal: a.goal,
                scope: a.scope,
                checks: a.checks.unwrap_or_default(),
                tier: a.tier,
                max_turns: a.max_turns,
            };
            json!({ "brief_id": lock(amm).add_brief(&a.run_id, input, BriefState::Queued)?.id })
        }
        "plan_import" => {
            let a: PlanImportArgs = args(body)?;
            lock(amm).run(&a.run_id)?;
            let path = a.path.clone();
            let md = blocking(move || read_plan(&path)).await?;
            let briefs = lock(amm).import_plan(&a.run_id, &md, &a.path)?;
            Value::Array(briefs.iter().map(compact).collect())
        }
        "brief_update" => {
            let a: BriefUpdateArgs = args(body)?;
            let patch = BriefPatch {
                title: a.title,
                goal: a.goal,
                scope: a.scope,
                checks: a.checks,
                tier: a.tier,
                max_turns: a.max_turns,
                ready: a.ready,
            };
            compact(&lock(amm).update_brief(&a.brief_id, patch)?)
        }
        "dispatch" => {
            let a: RunIdArgs = args(body)?;
            let calls = lock(amm).dispatch(&a.run_id)?;
            serde_json::to_value(calls).map_err(|e| AmmError(e.to_string()))?
        }
        "brief_get" => {
            let a: TokenArgs = args(body)?;
            Value::String(lock(amm).brief_text(&a.token)?)
        }
        "report" => {
            let a: ReportArgs = args(body)?;
            let r = Report { status: a.status, files: a.files.unwrap_or_default(), notes: a.notes.unwrap_or_default() };
            lock(amm).report(&a.token, r)?;
            json!("recorded")
        }
        "status" => {
            let a: StatusArgs = args(body)?;
            let full = a.full == Some(Value::Bool(true));
            Value::String(lock(amm).status(a.run_id.as_deref(), full)?)
        }
        "nudge" => {
            let a: NudgeArgs = args(body)?;
            lock(amm).nudge(&a.brief_id, &a.message)?;
            json!("queued; delivered on the worker's next tool call")
        }
        "resume" => {
            let a: RunIdArgs = args(body)?;
            let run = lock(amm).run(&a.run_id)?;
            if run.state != crate::types::RunState::Failed {
                // Same message the guarded transition gives, before any git work.
                lock(amm).resume_run(&a.run_id)?;
            }
            let (Some(wt), Some(head)) = (run.integration_worktree.clone(), run.integration_head.clone()) else {
                return Err(AmmError(format!("run {} has no integration worktree", run.id)));
            };
            let (repo, branch) = (run.repo.clone(), run.integration_branch.clone());
            blocking(move || {
                crate::integration::check_resumable(Path::new(&repo), Path::new(&wt), &branch, &head).map_err(|e| {
                    AmmError(format!("cannot resume: {e}. Put {branch} back on {head} (git reset --hard {head} in {wt})"))
                })
            })
            .await?;
            lock(amm).resume_run(&a.run_id)?;
            tokio::spawn(crate::integration::integrate(amm.clone(), a.run_id.clone()));
            json!("resumed")
        }
        "integrate" => {
            let a: RunIdArgs = args(body)?;
            let line = lock(amm).run_summary(&a.run_id)?;
            // Returns at once if the run's queue is already running.
            tokio::spawn(crate::integration::integrate(amm.clone(), a.run_id.clone()));
            Value::String(line)
        }
        "cancel" => {
            let a: CancelArgs = args(body)?;
            lock(amm).cancel(&a.id)?;
            json!("cancelled")
        }
        _ => return Err(AmmError(format!("unknown op {op}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_LIMITS;
    use crate::testutil::make_amm;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn raw(port: u16, path: &str, token: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-amm-token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    fn poison(m: &Mutex<Amm>) {
        let _guard = m.lock();
        panic!("boom");
    }

    async fn boom(State(s): State<AppState>) -> Response {
        poison(&s.amm);
        not_found()
    }

    fn app(amm: Amm) -> AppState {
        AppState { amm: Arc::new(Mutex::new(amm)), token: Arc::from("tok"), port: 0, activity: Arc::default() }
    }

    fn rev(dir: &Path, r: &str) -> Option<String> {
        let o = crate::git::git(dir, &["rev-parse", "--verify", "-q", r]).unwrap();
        (o.code == 0).then(|| o.stdout.trim().to_string())
    }

    #[tokio::test]
    async fn run_start_creates_the_integration_worktree_at_the_base_commit_and_runs_setup_there() {
        let repo = crate::testutil::init_repo();
        let s = app(make_amm(DEFAULT_LIMITS).amm);
        let args = json!({ "repo": repo, "test_cmd": "true", "setup_cmd": "mkdir -p .claude && touch .claude/ready" });
        let out = run_op(&s, "run_start", args).await.unwrap();
        let id = out["run_id"].as_str().unwrap().to_string();
        let run = lock(&s.amm).run(&id).unwrap();
        let wt = repo.join(".claude/worktrees").join(format!("amm-{id}"));
        assert_eq!(run.integration_branch, format!("amm/{id}"));
        assert_eq!(run.integration_worktree.as_deref(), Some(wt.to_string_lossy().as_ref()));
        assert_eq!(run.setup_cmd.as_deref(), Some("mkdir -p .claude && touch .claude/ready"));
        assert!(regex::Regex::new(r"^r[0-9a-f]{16}$").unwrap().is_match(&id), "{id}");
        assert_eq!(run.integration_head, rev(&repo, "main"));
        assert_eq!(rev(&repo, &format!("refs/heads/amm/{id}")), rev(&repo, "main"));
        assert_eq!(crate::git::worktree_info(&wt).unwrap().branch, Some(format!("amm/{id}")));
        assert!(wt.join(".claude/ready").exists(), "setup_cmd ran outside the integration worktree");
        assert!(!repo.join(".claude/ready").exists());
        // Cancelling the run leaves the integration worktree for the user.
        run_op(&s, "cancel", json!({ "id": id })).await.unwrap();
        assert!(wt.exists());
    }

    #[tokio::test]
    async fn a_failing_setup_cmd_removes_the_worktree_and_errors_with_its_output() {
        let repo = crate::testutil::init_repo();
        let s = app(make_amm(DEFAULT_LIMITS).amm);
        let args = json!({ "repo": repo, "test_cmd": "true", "setup_cmd": "echo setup-broke; exit 3" });
        let e = run_op(&s, "run_start", args).await.unwrap_err().0;
        assert!(e.contains("setup") && e.contains("setup-broke"), "{e}");
        let branches = crate::git::git(&repo, &["branch", "--list", "amm/*"]).unwrap().stdout;
        assert_eq!(branches.trim(), "");
        let left = std::fs::read_dir(repo.join(".claude/worktrees")).map(|d| d.count()).unwrap_or(0);
        assert_eq!(left, 0);
        assert!(lock(&s.amm).store.all::<crate::types::Run>().is_empty());
    }

    #[tokio::test]
    async fn a_setup_cmd_that_moves_head_or_leaves_untracked_files_fails_and_cleans_up() {
        for setup in ["git -c user.email=a@b -c user.name=a commit -q --allow-empty -m x", "touch stray.txt", "git checkout -q -b elsewhere"] {
            let repo = crate::testutil::init_repo();
            let s = app(make_amm(DEFAULT_LIMITS).amm);
            let args = json!({ "repo": repo, "test_cmd": "true", "setup_cmd": setup });
            let e = run_op(&s, "run_start", args).await.unwrap_err().0;
            assert!(e.contains("setup_cmd"), "{setup}: {e}");
            let branches = crate::git::git(&repo, &["branch", "--list", "amm/*"]).unwrap().stdout;
            assert_eq!(branches.trim(), "", "{setup}");
            let left = std::fs::read_dir(repo.join(".claude/worktrees")).map(|d| d.count()).unwrap_or(0);
            assert_eq!(left, 0, "{setup}");
            assert!(lock(&s.amm).store.all::<crate::types::Run>().is_empty());
        }
    }

    #[tokio::test]
    async fn resume_restarts_a_failed_run_only_once_the_branch_is_back_where_amm_left_it() {
        let repo = crate::testutil::init_repo();
        let s = app(make_amm(DEFAULT_LIMITS).amm);
        let id = run_op(&s, "run_start", json!({ "repo": repo, "test_cmd": "true" })).await.unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(run_op(&s, "resume", json!({ "run_id": id })).await.unwrap_err().0.contains("not failed"));
        let (wt, head) = {
            let mut amm = lock(&s.amm);
            let r = amm.run(&id).unwrap();
            amm.store.put(&crate::types::Run { state: crate::types::RunState::Failed, ..r.clone() }).unwrap();
            (std::path::PathBuf::from(r.integration_worktree.unwrap()), r.integration_head.unwrap())
        };
        let g = |a: &[&str]| crate::git::git(&wt, a).unwrap();
        g(&["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "user"]);
        let e = run_op(&s, "resume", json!({ "run_id": id })).await.unwrap_err().0;
        assert!(e.contains("not on"), "{e}");
        assert_eq!(lock(&s.amm).run(&id).unwrap().state, crate::types::RunState::Failed);
        g(&["reset", "-q", "--hard", &head]);
        run_op(&s, "resume", json!({ "run_id": id })).await.unwrap();
        assert_eq!(lock(&s.amm).run(&id).unwrap().state, crate::types::RunState::Active);
    }

    #[tokio::test]
    async fn integrate_kicks_the_queue_and_returns_the_run_status_line() {
        let repo = crate::testutil::init_repo();
        let s = app(make_amm(DEFAULT_LIMITS).amm);
        let id = run_op(&s, "run_start", json!({ "repo": repo, "test_cmd": "true" })).await.unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let out = run_op(&s, "integrate", json!({ "run_id": id })).await.unwrap();
        let line = out.as_str().unwrap();
        assert!(line.starts_with(&format!("run {id}: 0 briefs")), "{line}");
        assert!(run_op(&s, "integrate", json!({ "run_id": "nope" })).await.is_err());
    }

    #[tokio::test]
    async fn run_start_outside_a_git_repo_fails() {
        let s = app(make_amm(DEFAULT_LIMITS).amm);
        let args = json!({ "repo": crate::testutil::tmp_dir(), "test_cmd": "true", "base_branch": "main" });
        let e = run_op(&s, "run_start", args).await.unwrap_err().0;
        assert!(e.contains("not a git repository"), "{e}");
        assert!(lock(&s.amm).store.all::<crate::types::Run>().is_empty());
    }

    // Not in the TS suite: a panicking handler answers 500, and the mutex it poisoned keeps working.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_handler_gets_500_and_a_poisoned_lock_recovers() {
        let h = make_amm(DEFAULT_LIMITS);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = AppState {
            amm: Arc::new(Mutex::new(h.amm)),
            token: Arc::from("tok"),
            port,
            activity: Arc::default(),
        };
        let routes = Router::new().route("/boom", any(boom)).route("/api/{op}", any(api));
        let app = finish(routes, state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let out = raw(port, "/boom", "tok").await;
        assert!(out.starts_with("HTTP/1.1 500"), "{out}");
        assert!(out.ends_with("{\"error\":\"internal\"}"), "{out}");
        assert!(state.amm.is_poisoned());
        let out = raw(port, "/api/status", "tok").await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.ends_with("{\"result\":\"no active runs\"}"), "{out}");
    }
}
