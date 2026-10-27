//! `amm hook <event>`: forwards a Claude Code hook payload to the daemon and prints its decision.
//!
//! Synchronous and runtime-free so it starts in about a millisecond. It never blocks the agent
//! because of amm itself: every path exits 0, and a failed delivery is only logged.
use crate::config::{Env, amm_home};
use crate::daemon::{Lock, read_lock};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_TOOL_PREFIX: &str = "mcp__plugin_amm_";
const MAX_BODY: u64 = 10 * 1024 * 1024;

/// Runs the client and returns the process exit code, which is always 0.
pub fn run_hook(event: Option<&str>) -> i32 {
    let Some(event) = event.filter(|e| !e.is_empty()) else { return 0 };
    let env: Env = std::env::vars().collect();
    let home = amm_home(&env);

    let mut raw = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut raw);

    // amm is not running on this machine.
    let Some(lock) = read_lock(&home) else { return 0 };
    // Never send the token to whoever now holds a dead daemon's port.
    if !pid_alive(lock.pid) {
        return 0;
    }
    let Ok(input) = serde_json::from_slice::<Value>(&raw) else { return 0 };
    let prefix = env.get("AMM_TOOL_PREFIX").map_or(DEFAULT_TOOL_PREFIX, String::as_str);
    if !relevant(event, &input, prefix) {
        return 0;
    }

    match post(&lock, event, &raw) {
        Ok((200..=299, body)) => {
            if !body.is_empty() {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(body.as_bytes());
                let _ = out.flush();
            }
        }
        Ok((status, body)) => {
            let head: String = body.chars().take(500).collect();
            fallback_log(&home, &format!("{event} http {status} {head}"));
        }
        Err(e) => fallback_log(&home, &format!("{event} {e}")),
    }
    0
}

/// Only worker events, subagent lifecycle, and the main session's amm tool calls matter.
fn relevant(event: &str, input: &Value, prefix: &str) -> bool {
    let agent = match &input["agent_id"] {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    };
    let tool_name = match &input["tool_name"] {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    agent || event.starts_with("Subagent") || tool_name.starts_with(prefix)
}

/// `kill(pid, 0)`; EPERM means the process exists but belongs to someone else.
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence and permission check.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// POSTs the raw payload to `/hook/<event>`; returns the status and body, or a transport error.
fn post(lock: &Lock, event: &str, raw: &[u8]) -> Result<(u16, String), String> {
    let timeout = Duration::from_secs(if event == "SubagentStop" { 590 } else { 5 });
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .build()
        .into();
    let url = format!("http://127.0.0.1:{}/hook/{event}", lock.port);
    let mut res = agent
        .post(&url)
        .header("content-type", "application/json")
        .header("x-amm-token", &lock.token)
        .send(raw)
        .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let body = res.body_mut().with_config().limit(MAX_BODY).read_to_string().map_err(|e| e.to_string())?;
    Ok((status, body))
}

fn fallback_log(home: &Path, line: &str) {
    let line = format!("{} {line}\n", iso_now());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(home.join("hook-fallback.log")) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// The current UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ`, like JS `toISOString`.
fn iso_now() -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    iso(ms)
}

fn iso(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, mi, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_matches_js_to_iso_string() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(951_782_400_123), "2000-02-29T00:00:00.123Z");
        assert_eq!(iso(1_793_087_255_955), "2026-10-27T07:47:35.955Z");
    }

    #[test]
    fn relevance_filter() {
        let p = DEFAULT_TOOL_PREFIX;
        assert!(relevant("PreToolUse", &json!({ "agent_id": "a", "tool_name": "Read" }), p));
        assert!(!relevant("PreToolUse", &json!({ "agent_id": "", "tool_name": "Read" }), p));
        assert!(!relevant("PreToolUse", &json!({ "session_id": "s", "tool_name": "Read" }), p));
        assert!(relevant("SubagentStop", &json!({}), p));
        assert!(relevant("PostToolUse", &json!({ "tool_name": "mcp__plugin_amm_amm__amm_status" }), p));
        assert!(relevant("PostToolUse", &json!({ "tool_name": "x_y" }), "x_"));
        assert!(!relevant("PostToolUse", &json!(null), p));
    }

    #[test]
    fn pid_alive_counts_eperm_as_alive() {
        assert!(pid_alive(std::process::id()));
        assert!(pid_alive(1)); // init: EPERM for an unprivileged user, success for root
        assert!(!pid_alive((1 << 22) + 12345));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
    }
}
