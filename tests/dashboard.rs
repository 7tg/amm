//! The dashboard: cookie login, auth on its routes, assets, `/dash/state`, worker detail and the events stream.
mod common;

use amm::daemon::{DaemonHandle, DaemonOpts, read_lock, run_daemon};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn start(home: &Path) -> DaemonHandle {
    run_daemon(DaemonOpts { home: home.to_path_buf(), port: 0, ..Default::default() }).await.unwrap().expect("daemon")
}

/// Sends a request and returns everything the server writes back until it closes (or 5 s pass).
async fn raw(port: u16, data: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let _ = s.write_all(data.as_bytes()).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn get(port: u16, path: &str, extra: &str) -> String {
    raw(port, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Connection: close\r\n\r\n")).await
}

fn status(out: &str) -> u16 {
    out.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn header<'a>(out: &'a str, name: &str) -> Option<&'a str> {
    let head = out.split("\r\n\r\n").next()?;
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn body_json(out: &str) -> Value {
    serde_json::from_str(out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")).unwrap_or(Value::Null)
}

async fn post(port: u16, token: &str, path: &str, body: Value) -> Value {
    let body = body.to_string();
    let out = raw(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-amm-token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert_eq!(status(&out), 200, "{out}");
    body_json(&out)
}

fn cookie(token: &str) -> String {
    format!("Cookie: amm_token={token}\r\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn login_sets_an_http_only_cookie_and_routes_accept_cookie_or_header() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let (port, token) = (d.port(), read_lock(&home).unwrap().token);

    let out = get(port, &format!("/?token={token}"), "").await;
    assert_eq!(status(&out), 303, "{out}");
    assert_eq!(header(&out, "location"), Some("/"));
    let set = header(&out, "set-cookie").unwrap();
    assert_eq!(set, format!("amm_token={token}; HttpOnly; SameSite=Strict; Path=/"));
    assert_eq!(status(&get(port, "/?token=wrong", "").await), 401);

    for path in ["/", "/assets/app.js", "/assets/style.css", "/dash/state", "/dash/worker/nope"] {
        assert_eq!(status(&get(port, path, "").await), 401, "{path}");
        assert_eq!(status(&get(port, path, &cookie("wrong")).await), 401, "{path}");
    }
    for path in ["/", "/assets/app.js", "/assets/style.css", "/dash/state"] {
        assert_eq!(status(&get(port, path, &cookie(&token)).await), 200, "{path}");
        assert_eq!(status(&get(port, path, &format!("x-amm-token: {token}\r\n")).await), 200, "{path}");
    }
    // Cookie among others still works.
    let many = format!("Cookie: a=b; amm_token={token}; c=d\r\n");
    assert_eq!(status(&get(port, "/dash/state", &many).await), 200);

    // The API and health stay header-only.
    let out = raw(
        port,
        &format!("POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}", cookie(&token)),
    )
    .await;
    assert_eq!(status(&out), 401, "{out}");
    assert_eq!(status(&get(port, "/health", &cookie(&token)).await), 401);
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn assets_have_their_content_types() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let (port, token) = (d.port(), read_lock(&home).unwrap().token);
    for (path, ct) in [
        ("/", "text/html; charset=utf-8"),
        ("/assets/app.js", "application/javascript; charset=utf-8"),
        ("/assets/style.css", "text/css; charset=utf-8"),
    ] {
        let out = get(port, path, &cookie(&token)).await;
        assert_eq!(header(&out, "content-type"), Some(ct), "{path}");
        assert!(out.split_once("\r\n\r\n").unwrap().1.len() > 10, "{path}");
    }
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn state_follows_the_contract_and_unknown_workers_are_404() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let (port, token) = (d.port(), read_lock(&home).unwrap().token);
    let r = post(port, &token, "/api/run_start", json!({ "repo": common::init_repo(), "test_cmd": "true", "base_branch": "main" })).await;
    let run_id = r["result"]["run_id"].as_str().unwrap().to_string();
    let b = post(
        port,
        &token,
        "/api/brief_add",
        json!({ "run_id": run_id, "title": "t", "goal": "g", "scope": ["lib/**"], "tier": "light", "max_turns": 7 }),
    )
    .await;
    let brief_id = b["result"]["brief_id"].as_str().unwrap().to_string();

    let st = body_json(&get(port, "/dash/state", &cookie(&token)).await);
    for k in ["now", "sessions", "runs", "totals"] {
        assert!(st.get(k).is_some(), "{k}: {st}");
    }
    assert!(st["totals"]["workersUsd"].is_number() && st["totals"]["managersUsd"].is_number(), "{st}");
    let run = &st["runs"][0];
    for k in ["id", "repo", "baseBranch", "state", "integrationBranch", "merges", "lastTestOk", "createdAt", "costUsd", "briefs"] {
        assert!(run.get(k).is_some(), "{k}: {run}");
    }
    assert_eq!(run["id"], run_id.as_str());
    assert_eq!(run["state"], "active");
    let brief = &run["briefs"][0];
    for k in ["id", "seq", "title", "tier", "state", "attempt", "workers"] {
        assert!(brief.get(k).is_some(), "{k}: {brief}");
    }
    assert_eq!((brief["id"].as_str(), brief["tier"].as_str()), (Some(brief_id.as_str()), Some("light")));
    assert!(brief["workers"].is_array());

    let out = get(port, "/dash/worker/nope", &cookie(&token)).await;
    assert_eq!(status(&out), 404, "{out}");
    d.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_transcript_gives_numeric_costs() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let (port, token) = (d.port(), read_lock(&home).unwrap().token);
    let transcript = home.join("s1.jsonl");
    let line = |id: &str| {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","model":"claude-sonnet-5","usage":{{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":200}}}}}}"#
        )
    };
    std::fs::write(&transcript, format!("{}\n{}\n", line("m1"), line("m2"))).unwrap();
    post(
        port,
        &token,
        "/hook/PostToolUse",
        json!({ "session_id": "s1", "cwd": "/", "tool_name": "mcp__amm__amm_status", "transcript_path": transcript }),
    )
    .await;
    let st = body_json(&get(port, "/dash/state", &cookie(&token)).await);
    let s = &st["sessions"][0];
    assert_eq!((s["id"].as_str(), s["cwd"].as_str()), (Some("s1"), Some("/")), "{st}");
    assert!(s["lastSeen"].is_number());
    let cost = s["costUsd"].as_f64().unwrap();
    assert!(cost > 0.0, "{st}");
    assert!((st["totals"]["managersUsd"].as_f64().unwrap() - cost).abs() < 1e-12);
    assert_eq!(st["totals"]["workersUsd"].as_f64(), Some(0.0));
    d.close().await;
}

/// Reads an SSE stream until `want` appears (or 5 s pass).
async fn read_until(s: &mut TcpStream, buf: &mut String, want: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut chunk = [0u8; 4096];
    while !buf.contains(want) {
        match tokio::time::timeout_at(deadline, s.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
            _ => return false,
        }
    }
    true
}

async fn open_events(port: u16, token: &str, last: Option<&str>) -> TcpStream {
    let last = last.map(|l| format!("Last-Event-ID: {l}\r\n")).unwrap_or_default();
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!("GET /dash/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{}{last}\r\n", cookie(token));
    s.write_all(req.as_bytes()).await.unwrap();
    s
}

/// The first `id: <n>` line of an `event: amm` message, with its data.
fn first_message(buf: &str) -> Option<(i64, Value)> {
    let at = buf.find("event: amm")?;
    let msg = &buf[at..];
    let id = msg.lines().find_map(|l| l.strip_prefix("id: "))?.trim().parse().ok()?;
    let data = msg.lines().find_map(|l| l.strip_prefix("data: "))?;
    Some((id, serde_json::from_str(data).ok()?))
}

#[tokio::test(flavor = "multi_thread")]
async fn events_stream_delivers_new_events_and_resumes_after_last_event_id() {
    let home = common::tmp_dir();
    let d = start(&home).await;
    let (port, token) = (d.port(), read_lock(&home).unwrap().token);
    let r = post(port, &token, "/api/run_start", json!({ "repo": common::init_repo(), "test_cmd": "true", "base_branch": "main" })).await;
    let run_id = r["result"]["run_id"].as_str().unwrap().to_string();
    let brief = json!({ "run_id": run_id, "title": "t", "goal": "g", "scope": ["lib/**"], "tier": "light", "max_turns": 7 });
    post(port, &token, "/api/brief_add", brief).await;

    let mut s = open_events(port, &token, None).await;
    let mut buf = String::new();
    assert!(read_until(&mut s, &mut buf, "retry:").await, "{buf}");
    assert!(buf.starts_with("HTTP/1.1 200"), "{buf}");
    assert!(header(&buf, "content-type").is_some_and(|c| c.starts_with("text/event-stream")), "{buf}");
    // Started at the newest event: nothing old is replayed.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut probe = [0u8; 4096];
    if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(50), s.read(&mut probe)).await {
        buf.push_str(&String::from_utf8_lossy(&probe[..n]));
    }
    assert!(!buf.contains("event: amm"), "{buf}");

    // Cancelling the run writes a `cancel` event for its brief.
    post(port, &token, "/api/cancel", json!({ "id": run_id })).await;
    assert!(read_until(&mut s, &mut buf, "\ndata: ").await && read_until(&mut s, &mut buf, "}\n").await, "{buf}");
    let (id, data) = first_message(&buf).unwrap_or_else(|| panic!("{buf}"));
    assert_eq!(data["id"], id);
    for k in ["ts", "runId", "workerId", "kind", "detail"] {
        assert!(data.get(k).is_some(), "{k}: {data}");
    }
    drop(s);

    let mut s = open_events(port, &token, Some(&(id - 1).to_string())).await;
    let mut buf = String::new();
    assert!(read_until(&mut s, &mut buf, "\ndata: ").await && read_until(&mut s, &mut buf, "}\n").await, "{buf}");
    assert_eq!(first_message(&buf).map(|m| m.0), Some(id), "{buf}");
    drop(s);
    d.close().await;
}
