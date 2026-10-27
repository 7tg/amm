//! Port of test/shim.test.ts, driving `amm shim` over stdio JSON-RPC.
mod common;

use amm::daemon::read_lock;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const AMM: &str = env!("CARGO_BIN_EXE_amm");
const TOOLS: [&str; 10] = [
    "amm_run_start",
    "amm_brief_add",
    "amm_plan_import",
    "amm_brief_update",
    "amm_dispatch",
    "amm_brief_get",
    "amm_report",
    "amm_status",
    "amm_nudge",
    "amm_cancel",
];

struct Shim {
    home: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    id: u64,
}

impl Shim {
    fn start() -> Shim {
        let home = common::tmp_dir();
        let mut child = Command::new(AMM)
            .arg("shim")
            .env("AMM_HOME", &home)
            .env("AMM_PORT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn amm shim");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = Shim { home, child, stdin, stdout, id: 0 };
        let init = s.call(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "0" } }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "amm", "{init}");
        s.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        s
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "shim closed stdout");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        kill_daemon(&self.home);
    }
}

fn kill_daemon(home: &Path) {
    if let Some(lock) = read_lock(home) {
        unsafe { libc::kill(lock.pid as libc::pid_t, libc::SIGTERM) };
    }
}

#[test]
fn shim_starts_the_daemon_and_exposes_amm_tools_over_mcp() {
    let mut s = Shim::start();
    let list = s.call("tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for t in TOOLS {
        assert!(names.contains(&t), "{t}");
    }
    let home = s.home.to_str().unwrap().to_string();
    let res = s.call(
        "tools/call",
        json!({ "name": "amm_run_start", "arguments": { "repo": home, "test_cmd": "true", "base_branch": "main" } }),
    );
    let text = res["result"]["content"][0]["text"].as_str().unwrap_or_default();
    let re = regex::Regex::new(r#""run_id":"r[0-9a-f]{6}""#).unwrap();
    assert!(re.is_match(text), "{res}");
    assert!(read_lock(&s.home).is_some());
}

/// The TS shim's tools/list output (names, descriptions, input schemas), captured from dist/amm.js.
#[test]
fn tool_schemas_match_the_typescript_shim() {
    let want: Value = serde_json::from_str(include_str!("fixtures/ts-tools.json")).unwrap();
    let mut s = Shim::start();
    let list = s.call("tools/list", json!({}));
    let got: Vec<Value> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| json!({ "name": t["name"], "description": t["description"], "inputSchema": t["inputSchema"] }))
        .collect();
    assert_eq!(Value::Array(got), want);
}

#[test]
fn daemon_errors_come_back_as_mcp_tool_errors() {
    let mut s = Shim::start();
    let res = s.call("tools/call", json!({ "name": "amm_brief_get", "arguments": { "token": "nope" } }));
    assert_eq!(res["result"]["isError"], true, "{res}");
    let text = res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("error: "), "{text}");
}

#[test]
fn the_daemon_log_receives_the_daemon_output() {
    let mut s = Shim::start();
    s.call("tools/call", json!({ "name": "amm_status", "arguments": {} }));
    let log = std::fs::read_to_string(s.home.join("daemon.log")).unwrap();
    assert!(log.contains("amm daemon pid"), "{log}");
}
