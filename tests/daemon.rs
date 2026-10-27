//! Port of test/daemon.test.ts, plus the crash, singleton, signal and concurrency cases.
mod common;

use amm::daemon::{DaemonHandle, DaemonOpts, Lock, is_healthy, lock_path, read_lock, run_daemon};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn start(home: &Path) -> DaemonHandle {
    run_daemon(DaemonOpts { home: home.to_path_buf(), port: 0, ..Default::default() }).await.unwrap().expect("daemon")
}

async fn healthy(lock: Option<Lock>) -> bool {
    tokio::task::spawn_blocking(move || lock.is_some_and(|l| is_healthy(&l))).await.unwrap()
}

/// Sends raw bytes and returns everything the server writes back until it closes (or 5 s pass).
async fn raw(port: u16, data: &[u8]) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let _ = s.write_all(data).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

/// Writes `data` and hangs up after 100 ms without finishing the request.
async fn raw_abort(port: u16, data: &[u8]) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(data).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(s);
}

struct Resp {
    status: u16,
    body: Value,
}

fn parse(out: &str) -> Resp {
    let status = out.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    Resp { status, body: serde_json::from_str(body).unwrap_or(Value::Null) }
}

async fn request(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> Resp {
    let tok = token.map(|t| format!("x-amm-token: {t}\r\n")).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{tok}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    parse(&raw(port, req.as_bytes()).await)
}

struct Client {
    port: u16,
    token: String,
}

impl Client {
    fn of(home: &Path) -> Client {
        let lock = read_lock(home).expect("lock");
        Client { port: lock.port, token: lock.token }
    }
    async fn call(&self, op: &str, body: Value) -> Resp {
        request(self.port, "POST", &format!("/api/{op}"), Some(&self.token), &body.to_string()).await
    }
    async fn hook(&self, event: &str, body: Value) -> Resp {
        request(self.port, "POST", &format!("/hook/{event}"), Some(&self.token), &body.to_string()).await
    }
    async fn alive(&self) {
        let r = request(self.port, "GET", "/health", Some(&self.token), "").await;
        assert_eq!(r.status, 200, "daemon stopped answering");
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_serves_health_rejects_bad_tokens_and_runs_api_ops() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let lock = read_lock(&home).unwrap();
    assert_eq!(lock.port, d.port());
    assert!(healthy(Some(lock.clone())).await);
    assert_eq!(request(d.port(), "POST", "/api/status", None, "{}").await.status, 401);
    let c = Client::of(&home);
    let run = c.call("run_start", json!({ "repo": home, "test_cmd": "true", "base_branch": "main" })).await;
    let id = run.body["result"]["run_id"].as_str().unwrap().to_string();
    assert!(regex::Regex::new(r"^r[0-9a-f]{6}$").unwrap().is_match(&id), "{id}");
    let bad = c.call("dispatch", json!({ "run_id": "nope" })).await;
    assert_eq!((bad.status, bad.body["error"].as_str()), (400, Some("unknown run nope")));
    let hook = c.hook("PreToolUse", json!({ "session_id": "s", "cwd": "/", "agent_id": "x", "tool_name": "Read" })).await;
    assert_eq!(hook.body, json!({}));
    d.close().await;
    assert!(read_lock(&home).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_for_the_same_home_steps_aside() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    assert!(run_daemon(DaemonOpts { home: home.clone(), port: 0, ..Default::default() }).await.unwrap().is_none());
    // Also when it is pointed at the first daemon's port.
    let again = DaemonOpts { home: home.clone(), port: d.port(), ..Default::default() };
    assert!(run_daemon(again).await.unwrap().is_none());
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_requests_aborted_bodies_and_oversized_bodies_do_not_kill_the_daemon() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let c = Client::of(&home);
    let port = d.port();
    raw(port, format!("GET //[ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).await;
    c.alive().await;
    let partial = format!(
        "POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-amm-token: {}\r\nContent-Length: 1000\r\n\r\n{{\"a\"",
        c.token
    );
    raw_abort(port, partial.as_bytes()).await;
    c.alive().await;
    let big = request(port, "POST", "/api/status", Some(&c.token), &"x".repeat(1_100_000)).await;
    // The server may hang up before reading the whole body; like the TS test, a dropped connection counts.
    assert!(big.status == 413 || big.status == 0, "{}", big.status);
    c.alive().await;
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn health_needs_the_token_and_foreign_host_headers_are_refused() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let lock = read_lock(&home).unwrap();
    assert_eq!(request(d.port(), "GET", "/health", None, "").await.status, 401);
    let port = d.port();
    let out = raw(
        port,
        format!("GET /health HTTP/1.1\r\nHost: evil.example:{port}\r\nx-amm-token: {}\r\nConnection: close\r\n\r\n", lock.token)
            .as_bytes(),
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    d.close().await;
}

// Not in the TS suite: the cases the TS hardening covered, one by one.

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_url_before_auth_does_not_kill_the_daemon() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let port = d.port();
    let out = raw(port, format!("GET //[ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).await;
    assert!(out.is_empty() || out.starts_with("HTTP/1.1 4"), "{out}");
    Client::of(&home).alive().await;
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_aborted_midway_does_not_kill_the_daemon() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let c = Client::of(&home);
    let port = d.port();
    for path in ["/api/status", "/hook/PreToolUse"] {
        let partial = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-amm-token: {}\r\nContent-Length: 1000\r\n\r\n{{\"a\"",
            c.token
        );
        raw_abort(port, partial.as_bytes()).await;
        c.alive().await;
    }
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_two_megabyte_body_gets_413() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let c = Client::of(&home);
    let port = d.port();
    // Declared up front: refused before any of it is read.
    let head = format!(
        "POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-amm-token: {}\r\nContent-Length: 2000000\r\nConnection: close\r\n\r\n",
        c.token
    );
    let r = parse(&raw(port, head.as_bytes()).await);
    assert_eq!((r.status, r.body["error"].as_str()), (413, Some("body too large")));
    // Chunked, so the size is only known while reading.
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let head = format!(
        "POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-amm-token: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        c.token
    );
    let (mut rd, mut wr) = s.split();
    let write = async {
        let _ = wr.write_all(head.as_bytes()).await;
        let chunk = format!("10000\r\n{}\r\n", "x".repeat(0x10000));
        for _ in 0..32 {
            if wr.write_all(chunk.as_bytes()).await.is_err() {
                break;
            }
        }
    };
    let mut out = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), rd.read_to_end(&mut out));
    let _ = tokio::join!(write, read);
    let out = String::from_utf8_lossy(&out);
    assert!(out.is_empty() || out.starts_with("HTTP/1.1 413"), "{out}");
    c.alive().await;
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_host_is_refused_before_the_token_is_checked() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let port = d.port();
    for host in [format!("evil.example:{port}"), "127.0.0.1".to_string(), format!("127.0.0.1:{}", port.wrapping_add(1))] {
        let out = raw(port, format!("GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await;
        assert!(out.starts_with("HTTP/1.1 403"), "{host}: {out}");
    }
    let out = raw(port, b"GET /health HTTP/1.0\r\n\r\n").await;
    assert!(out.starts_with("HTTP/1.1 403") || out.starts_with("HTTP/1.0 403"), "{out}");
    let ok = raw(port, format!("GET /health HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n").as_bytes()).await;
    assert!(ok.starts_with("HTTP/1.1 401"), "{ok}");
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_or_wrong_token_gets_401() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let c = Client::of(&home);
    let same_len: String = c.token.chars().map(|ch| if ch == '0' { '1' } else { '0' }).collect();
    for tok in [None, Some(""), Some("nope"), Some(same_len.as_str()), Some(&c.token[..c.token.len() - 1])] {
        for (m, p) in [("POST", "/api/status"), ("POST", "/hook/PreToolUse"), ("GET", "/health"), ("GET", "/nope")] {
            let r = request(d.port(), m, p, tok, "{}").await;
            assert_eq!((r.status, r.body["error"].as_str()), (401, Some("bad token")), "{m} {p} {tok:?}");
        }
    }
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn health_needs_the_token_and_returns_the_pid() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let c = Client::of(&home);
    assert_eq!(request(d.port(), "GET", "/health", None, "").await.status, 401);
    let r = request(d.port(), "GET", "/health", Some(&c.token), "").await;
    assert_eq!(r.body, json!({ "ok": true, "pid": std::process::id() }));
    let mut wrong = read_lock(&home).unwrap();
    wrong.pid += 1;
    assert!(!healthy(Some(wrong)).await, "a pid mismatch is not healthy");
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_ops_inherited_names_and_bad_json_are_refused() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let c = Client::of(&home);
    for op in ["nope", "constructor", "__proto__", "toString", "hasOwnProperty"] {
        let r = c.call(op, json!({})).await;
        assert_eq!((r.status, r.body["error"].as_str()), (404, Some("not found")), "{op}");
    }
    assert_eq!(request(d.port(), "GET", "/api/status", Some(&c.token), "").await.status, 404);
    assert_eq!(request(d.port(), "POST", "/elsewhere", Some(&c.token), "").await.status, 404);
    let r = request(d.port(), "POST", "/api/status", Some(&c.token), "{not json").await;
    assert_eq!(r.status, 400);
    assert!(r.body["error"].is_string(), "{}", r.body);
    let r = request(d.port(), "POST", "/hook/PreToolUse", Some(&c.token), "[1,").await;
    assert_eq!(r.status, 400);
    // An empty body is `{}`.
    let r = request(d.port(), "POST", "/api/status", Some(&c.token), "").await;
    assert_eq!((r.status, r.body["result"].as_str()), (200, Some("no active runs")));
    c.alive().await;
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn api_ops_map_snake_case_args_and_return_compact_results() {
    let home = common::tmp_dir();
    let repo = common::init_repo();
    let d = start(&home).await;
    let c = Client::of(&home);
    // No base branch: resolved from the repo's checkout.
    let r = c.call("run_start", json!({ "repo": repo, "test_cmd": "true" })).await;
    let run_id = r.body["result"]["run_id"].as_str().unwrap().to_string();
    let st = c.call("status", json!({ "run_id": run_id, "full": true })).await;
    assert!(st.body["result"].as_str().unwrap().starts_with(&format!("run {run_id}: 0 briefs")), "{}", st.body);
    let b = c
        .call(
            "brief_add",
            json!({ "run_id": run_id, "title": "t", "goal": "g", "scope": ["lib/**"], "tier": "light", "max_turns": 7 }),
        )
        .await;
    let brief_id = b.body["result"]["brief_id"].as_str().unwrap().to_string();
    let plan = home.join("plan.md");
    std::fs::write(&plan, "### Task 1: One\n- Modify: `src/a.ts`\n\n### Task 2: Two\n- Create: `src/b.ts`\n").unwrap();
    let imp = c.call("plan_import", json!({ "run_id": run_id, "path": plan })).await;
    let briefs = imp.body["result"].as_array().unwrap_or_else(|| panic!("{}", imp.body));
    assert_eq!(briefs.len(), 2);
    assert_eq!(briefs[0]["title"], "One");
    assert_eq!(briefs[0]["state"], "draft");
    assert_eq!(briefs[0]["tier"], "standard");
    assert_eq!(briefs[0]["scope"], json!(["src/a.ts"]));
    let mut keys: Vec<_> = briefs[0].as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["id", "scope", "state", "tier", "title"]);
    let missing = c.call("plan_import", json!({ "run_id": run_id, "path": home.join("none.md") })).await;
    assert_eq!(missing.status, 400);
    let up = c.call("brief_update", json!({ "brief_id": briefs[1]["id"], "max_turns": 3, "tier": "heavy", "ready": true })).await;
    assert_eq!((up.body["result"]["state"].as_str(), up.body["result"]["tier"].as_str()), (Some("queued"), Some("heavy")));
    let calls = c.call("dispatch", json!({ "run_id": run_id })).await;
    let calls = calls.body["result"].as_array().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["briefId"], brief_id.as_str());
    assert!(calls[0]["prompt"].as_str().unwrap().contains(&brief_id));
    let text = c.call("brief_get", json!({ "token": brief_id })).await;
    assert!(text.body["result"].as_str().unwrap().starts_with(&format!("# amm brief {brief_id}: t")));
    let rep = c.call("report", json!({ "token": brief_id, "status": "done" })).await;
    assert_eq!(rep.body["result"], "recorded");
    let n = c.call("nudge", json!({ "brief_id": brief_id, "message": "hurry" })).await;
    assert_eq!(n.body["result"], "queued; delivered on the worker's next tool call");
    let x = c.call("cancel", json!({ "id": run_id })).await;
    assert_eq!(x.body["result"], "cancelled");
    let st = c.call("status", json!({})).await;
    assert_eq!(st.body["result"], "no active runs");
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_import_refuses_files_over_5mb() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let c = Client::of(&home);
    let r = c.call("run_start", json!({ "repo": home, "test_cmd": "true", "base_branch": "main" })).await;
    let run_id = r.body["result"]["run_id"].as_str().unwrap().to_string();
    let plan = home.join("big.md");
    std::fs::write(&plan, format!("### Task 1: Big\n{}", "x".repeat(5_000_001))).unwrap();
    let r = c.call("plan_import", json!({ "run_id": run_id, "path": plan })).await;
    assert_eq!(r.status, 400);
    assert!(r.body["error"].as_str().unwrap().contains("too large"), "{}", r.body);
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn home_lock_and_db_are_private() {
    let home = common::tmp_dir().join("amm");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
    let d = start(&home).await;
    assert_eq!(mode(&home), 0o700);
    assert_eq!(mode(&lock_path(&home)), 0o600);
    assert_eq!(mode(&home.join("amm.db")), 0o600);
    let leftovers: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
    let lock: Value = serde_json::from_str(&std::fs::read_to_string(lock_path(&home)).unwrap()).unwrap();
    assert_eq!(lock["pid"], std::process::id());
    assert_eq!(lock["port"], d.port());
    assert_eq!(lock["token"].as_str().unwrap().len(), 32);
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_held_by_something_else_falls_back_to_the_next() {
    let home = common::tmp_dir();
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();
    let opts = DaemonOpts { home: home.clone(), port: taken, ..Default::default() };
    match run_daemon(opts).await {
        Ok(Some(d)) => {
            assert!(d.port() > taken && d.port() < taken.saturating_add(20), "{}", d.port());
            Client::of(&home).alive().await;
            d.close().await;
        }
        // The next 19 ports may happen to be taken on a busy machine; that is the only acceptable failure.
        Ok(None) => panic!("stepped aside for a non-amm port holder"),
        Err(e) => assert!(e.to_string().contains("no free port"), "{e}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_has_not_written_its_lock_yet_is_waited_for() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let lock_file = lock_path(&home);
    let saved = std::fs::read(&lock_file).unwrap();
    std::fs::remove_file(&lock_file).unwrap();
    let opts = DaemonOpts { home: home.clone(), port: d.port(), ..Default::default() };
    let second = tokio::spawn(run_daemon(opts));
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(&lock_file, &saved).unwrap();
    assert!(second.await.unwrap().unwrap().is_none());
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_requests_do_not_keep_an_idle_daemon_alive() {
    let home = common::tmp_dir();
    let opts = DaemonOpts { home: home.clone(), port: 0, sweep_ms: Some(50), idle_shutdown_ms: Some(150) };
    let d = run_daemon(opts).await.unwrap().unwrap();
    let port = d.port();
    let poke = tokio::spawn(async move {
        loop {
            let r = request(port, "GET", "/health", None, "").await;
            assert!(r.status == 401 || r.status == 0, "{}", r.status);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let shut = tokio::time::timeout(Duration::from_secs(5), d.closed()).await;
    poke.abort();
    shut.expect("unauthenticated health checks kept the daemon alive");
    assert!(read_lock(&home).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_daemon_shuts_down_unless_workers_are_active() {
    let home = common::tmp_dir();
    let opts = DaemonOpts { home: home.clone(), port: 0, sweep_ms: Some(50), idle_shutdown_ms: Some(150) };
    let d = run_daemon(opts).await.unwrap().unwrap();
    let c = Client::of(&home);
    let r = c.call("run_start", json!({ "repo": home, "test_cmd": "true", "base_branch": "main" })).await;
    let run_id = r.body["result"]["run_id"].as_str().unwrap().to_string();
    let b = json!({ "run_id": run_id, "title": "t", "goal": "g", "scope": ["a"], "checks": [], "tier": "light" });
    c.call("brief_add", b).await;
    assert_eq!(c.call("dispatch", json!({ "run_id": run_id })).await.body["result"].as_array().unwrap().len(), 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(read_lock(&home).is_some(), "shut down with an active worker");
    c.alive().await;
    c.call("cancel", json!({ "id": run_id })).await;
    tokio::time::timeout(Duration::from_secs(5), d.closed()).await.expect("idle shutdown");
    assert!(read_lock(&home).is_none());
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn close_drops_connections_that_are_still_open() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let c = Client::of(&home);
    let port = d.port();
    // A request whose body never arrives keeps its connection open.
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let partial = format!(
        "POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-amm-token: {}\r\nContent-Length: 1000\r\n\r\n{{",
        c.token
    );
    s.write_all(partial.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(2), d.close()).await.expect("close waited for an open request");
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut buf)).await.expect("connection left open");
    assert!(read_lock(&home).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn close_leaves_a_lock_that_is_no_longer_ours() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let other = json!({ "pid": 1, "port": 1, "token": "t" }).to_string();
    std::fs::write(lock_path(&home), &other).unwrap();
    d.close().await;
    assert_eq!(std::fs::read_to_string(lock_path(&home)).unwrap(), other);
}

// The plan's Concurrency test: two hooks race to bind one brief over HTTP.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_http_binds_on_one_brief_bind_exactly_one_agent() {
    let home = common::tmp_dir();
    let main = common::init_repo_with(&[("src/a.ts", "a\n")]);
    let wt = common::add_worktree(&main, &format!("x{:06x}", rand::random::<u32>() & 0xff_ffff));
    let d = start(&home).await;
    let c = std::sync::Arc::new(Client::of(&home));
    let r = c.call("run_start", json!({ "repo": main, "test_cmd": "true", "base_branch": "main" })).await;
    let run_id = r.body["result"]["run_id"].as_str().unwrap().to_string();
    let b = json!({ "run_id": run_id, "title": "t", "goal": "g", "scope": ["src/**"], "checks": [], "tier": "light" });
    let brief_id = c.call("brief_add", b).await.body["result"]["brief_id"].as_str().unwrap().to_string();
    assert_eq!(c.call("dispatch", json!({ "run_id": run_id })).await.body["result"].as_array().unwrap().len(), 1);
    let hook = |agent: &str, tool: &str| {
        let c = c.clone();
        let v = json!({ "session_id": "s", "cwd": wt, "hook_event_name": "PreToolUse", "agent_id": agent,
                        "agent_type": "amm:amm-worker-light", "tool_name": tool, "tool_input": { "token": brief_id } });
        tokio::spawn(async move { c.hook("PreToolUse", v).await.body })
    };
    let denied = |v: &Value| v["hookSpecificOutput"]["permissionDecision"] == "deny";
    let get = "mcp__plugin_amm_amm__amm_brief_get";
    let (a, b) = (hook("agA", get), hook("agB", get));
    let outs = [a.await.unwrap(), b.await.unwrap()];
    assert_eq!(outs.iter().filter(|o| !denied(o)).count(), 1, "{outs:?}");
    let winner = if denied(&outs[0]) { "agB" } else { "agA" };
    let loser = if winner == "agA" { "agB" } else { "agA" };
    assert!(!denied(&hook(winner, "Read").await.unwrap()));
    assert!(denied(&hook(loser, "Read").await.unwrap()));
    d.close().await;
}

fn bin_daemon(home: &Path) -> (std::process::Child, u16) {
    use std::io::BufRead;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_amm"))
        .arg("daemon")
        .env("AMM_HOME", home)
        .env("AMM_PORT", "0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let re = regex::Regex::new(r"^amm daemon pid (\d+) on 127\.0\.0\.1:(\d+)\n$").unwrap();
    let caps = re.captures(&line).unwrap_or_else(|| panic!("{line:?}"));
    assert_eq!(caps[1].parse::<u32>().unwrap(), child.id());
    (child, caps[2].parse().unwrap())
}

fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    for _ in 0..100 {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    panic!("daemon did not exit");
}

#[test]
fn sigterm_and_sigint_stop_the_daemon_binary_and_remove_its_lock() {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let home: PathBuf = common::tmp_dir();
        let (mut child, port) = bin_daemon(&home);
        let lock = read_lock(&home).unwrap();
        assert_eq!((lock.pid, lock.port), (child.id(), port));
        assert!(is_healthy(&lock));
        unsafe { libc::kill(child.id() as i32, sig) };
        let status = wait_exit(&mut child);
        assert_eq!(status.code(), Some(0), "signal {sig}");
        assert!(read_lock(&home).is_none(), "signal {sig}");
    }
}

#[test]
fn the_daemon_binary_exits_quietly_when_another_owns_the_home() {
    let home = common::tmp_dir();
    let (mut first, _) = bin_daemon(&home);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_amm"))
        .arg("daemon")
        .env("AMM_HOME", &home)
        .env("AMM_PORT", "0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    unsafe { libc::kill(first.id() as i32, libc::SIGTERM) };
    assert_eq!(wait_exit(&mut first).code(), Some(0));
}
