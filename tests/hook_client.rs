//! Port of test/hook-client.test.ts, driving `amm hook <event>`, plus the startup timing check.
mod common;

use amm::daemon::{DaemonOpts, run_daemon};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const AMM: &str = env!("CARGO_BIN_EXE_amm");

fn run(home: &Path, event: &str, payload: &Value) -> Output {
    let mut child = Command::new(AMM)
        .args(["hook", event])
        .env("AMM_HOME", home)
        .env_remove("AMM_TOOL_PREFIX")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn amm");
    // The client may exit before reading stdin; a broken pipe here is fine.
    let _ = child.stdin.take().unwrap().write_all(payload.to_string().as_bytes());
    child.wait_with_output().unwrap()
}

fn status_stdout(o: &Output) -> (Option<i32>, String) {
    (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned())
}

fn write_lock(home: &Path, pid: u32) {
    std::fs::write(home.join("daemon.lock"), json!({ "pid": pid, "port": 1, "token": "x" }).to_string()).unwrap();
}

#[test]
fn without_a_daemon_lock_the_client_exits_silently() {
    let r = run(&common::tmp_dir(), "PreToolUse", &json!({ "agent_id": "a", "tool_name": "Read" }));
    assert_eq!(status_stdout(&r), (Some(0), String::new()));
}

#[test]
fn main_session_events_for_non_amm_tools_are_not_forwarded() {
    let home = common::tmp_dir();
    write_lock(&home, std::process::id());
    let r = run(&home, "PreToolUse", &json!({ "session_id": "s", "tool_name": "Read" }));
    assert_eq!(status_stdout(&r), (Some(0), String::new()));
    assert!(!home.join("hook-fallback.log").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_events_are_forwarded_and_the_decision_printed() {
    let home = common::tmp_dir();
    let d = run_daemon(DaemonOpts { home: home.clone(), port: 0, ..Default::default() }).await.unwrap().expect("daemon");
    let h = home.clone();
    let r = tokio::task::spawn_blocking(move || {
        run(&h, "PreToolUse", &json!({ "session_id": "s", "cwd": "/", "agent_id": "a", "tool_name": "Read" }))
    })
    .await
    .unwrap();
    d.close().await;
    assert_eq!(status_stdout(&r), (Some(0), "{}".to_string()));
}

#[test]
fn an_unreachable_daemon_is_logged_and_the_action_allowed() {
    let home = common::tmp_dir();
    write_lock(&home, std::process::id());
    let r = run(&home, "PreToolUse", &json!({ "agent_id": "a", "tool_name": "Read" }));
    assert_eq!(status_stdout(&r), (Some(0), String::new()));
    let log = std::fs::read_to_string(home.join("hook-fallback.log")).unwrap();
    assert!(log.contains("PreToolUse"), "{log}");
}

#[test]
fn a_stale_lock_with_a_dead_pid_sends_nothing_and_logs_nothing() {
    let home = common::tmp_dir();
    write_lock(&home, (1 << 22) + 12345);
    let r = run(&home, "PreToolUse", &json!({ "agent_id": "a", "tool_name": "Read" }));
    assert_eq!(status_stdout(&r), (Some(0), String::new()));
    assert!(!home.join("hook-fallback.log").exists());
}

#[test]
fn a_missing_event_exits_zero() {
    let r = Command::new(AMM).arg("hook").stdin(Stdio::null()).output().unwrap();
    assert_eq!(r.status.code(), Some(0));
    assert!(r.stdout.is_empty());
}

#[test]
fn unparseable_stdin_exits_zero_without_sending() {
    let home = common::tmp_dir();
    write_lock(&home, std::process::id());
    let mut child = Command::new(AMM)
        .args(["hook", "PreToolUse"])
        .env("AMM_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(b"{not json");
    let r = child.wait_with_output().unwrap();
    assert_eq!(status_stdout(&r), (Some(0), String::new()));
    assert!(!home.join("hook-fallback.log").exists());
}

#[test]
fn subagent_events_and_amm_tool_prefix_are_forwarded() {
    // Port 1 refuses, so a forwarded event shows up in the fallback log.
    let home = common::tmp_dir();
    write_lock(&home, std::process::id());
    let r = run(&home, "SubagentStart", &json!({ "session_id": "s" }));
    assert_eq!(status_stdout(&r), (Some(0), String::new()));
    let r = run(&home, "PreToolUse", &json!({ "session_id": "s", "tool_name": "mcp__plugin_amm_amm__amm_status" }));
    assert_eq!(status_stdout(&r), (Some(0), String::new()));
    let log = std::fs::read_to_string(home.join("hook-fallback.log")).unwrap();
    assert!(log.contains("SubagentStart") && log.contains("PreToolUse"), "{log}");

    let home = common::tmp_dir();
    write_lock(&home, std::process::id());
    let mut child = Command::new(AMM)
        .args(["hook", "PreToolUse"])
        .env("AMM_HOME", &home)
        .env("AMM_TOOL_PREFIX", "mcp__custom_")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(json!({ "tool_name": "mcp__custom_x" }).to_string().as_bytes());
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(home.join("hook-fallback.log").exists());
}

/// Runs in the debug build `cargo test` produces; it is well inside the budget there.
#[test]
fn hook_without_a_lock_exits_in_under_20ms() {
    let home = common::tmp_dir();
    let mut times: Vec<Duration> = (0..5)
        .map(|_| {
            let t = Instant::now();
            let s = Command::new(AMM).args(["hook", "PreToolUse"]).env("AMM_HOME", &home).stdin(Stdio::null()).status().unwrap();
            assert_eq!(s.code(), Some(0));
            t.elapsed()
        })
        .collect();
    times.sort();
    assert!(times[2] < Duration::from_millis(20), "median {:?} of {times:?}", times[2]);
}

/// Exit code 2 from a PreToolUse hook blocks the tool call, so bad arguments must not produce it.
#[test]
fn bad_hook_arguments_exit_zero() {
    for args in [&["hook", "PreToolUse", "extra"][..], &["hook", "--bogus"][..]] {
        let r = Command::new(AMM).args(args).env("AMM_HOME", common::tmp_dir()).stdin(Stdio::null()).output().unwrap();
        assert_eq!(r.status.code(), Some(0), "{args:?}");
        assert!(r.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn bad_arguments_elsewhere_still_fail() {
    let r = Command::new(AMM).arg("--bogus").output().unwrap();
    assert_eq!(r.status.code(), Some(2));
}
