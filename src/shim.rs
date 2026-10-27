//! `amm shim`: the MCP stdio server. Each tool forwards to the daemon's `/api/<tool minus amm_>`,
//! starting a detached daemon first when none is healthy.
use crate::config::{Env, amm_home};
use crate::daemon::{Lock, is_healthy, read_lock};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use std::fs::{DirBuilder, OpenOptions};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY: u64 = 64 * 1024 * 1024;

/// Returns the running daemon's lock, starting a detached daemon first if none is healthy.
pub fn ensure_daemon(home: &Path) -> Result<Lock, String> {
    if let Some(lock) = read_lock(home).filter(is_healthy) {
        return Ok(lock);
    }
    let log_path = home.join("daemon.log");
    spawn_daemon(home, &log_path).map_err(|e| format!("cannot start amm daemon: {e}"))?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(lock) = read_lock(home).filter(is_healthy) {
            return Ok(lock);
        }
    }
    Err(format!("amm daemon did not start; see {}", log_path.display()))
}

fn spawn_daemon(home: &Path, log_path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(home)?;
    let exe = std::env::current_exe()?;
    let mut child = {
        // Both log handles live in `cmd`, so the parent's copies close when it drops at the end of this block.
        let log = OpenOptions::new().create(true).append(true).open(log_path)?;
        let mut cmd = Command::new(exe);
        cmd.arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        // SAFETY: setsid is async-signal-safe; it detaches the daemon from the shim's session and terminal.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn()?
    };
    // Reap it if it exits while the shim is still running, so it does not linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[derive(Debug)]
enum CallError {
    Refused(String),
    Other(String),
}

fn post_api(lock: &Lock, op: &str, args: &Value) -> Result<Value, CallError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(CALL_TIMEOUT))
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .build()
        .into();
    let url = format!("http://127.0.0.1:{}/api/{op}", lock.port);
    let mut res = match agent
        .post(&url)
        .header("content-type", "application/json")
        .header("x-amm-token", &lock.token)
        .send(args.to_string().as_bytes())
    {
        Ok(r) => r,
        Err(ureq::Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return Err(CallError::Refused(e.to_string()));
        }
        Err(e) => return Err(CallError::Other(e.to_string())),
    };
    let status = res.status().as_u16();
    let text = res.body_mut().with_config().limit(MAX_BODY).read_to_string().map_err(|e| CallError::Other(e.to_string()))?;
    let mut body: Value = serde_json::from_str(&text).map_err(|e| CallError::Other(e.to_string()))?;
    if !(200..300).contains(&status) {
        let msg = body["error"].as_str().map_or_else(|| format!("http {status}"), str::to_string);
        return Err(CallError::Other(msg));
    }
    Ok(body.get_mut("result").map(Value::take).unwrap_or(Value::Null))
}

/// Calls a daemon op. Connection refused means the request never reached a daemon
/// (it may have just shut down), so exactly that case is retried once.
pub fn call_daemon(home: &Path, op: &str, args: &Value) -> Result<Value, String> {
    let mut attempt = 0;
    loop {
        let lock = ensure_daemon(home)?;
        match post_api(&lock, op, args) {
            Ok(v) => return Ok(v),
            Err(CallError::Refused(_)) if attempt == 0 => attempt += 1,
            Err(CallError::Refused(m) | CallError::Other(m)) => return Err(m),
        }
    }
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn schema(properties: Value, required: &[&str]) -> Arc<JsonObject> {
    let mut s = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        s["required"] = json!(required);
    }
    s["additionalProperties"] = json!(false);
    s["$schema"] = json!("http://json-schema.org/draft-07/schema#");
    match s {
        Value::Object(o) => Arc::new(o),
        _ => unreachable!(),
    }
}

/// The ten amm tools, with the TS shim's names, descriptions and input schemas.
pub fn tools() -> Vec<Tool> {
    let s = || json!({ "type": "string" });
    let sd = |d: &str| json!({ "type": "string", "description": d });
    let arr = || json!({ "type": "array", "items": { "type": "string" } });
    let arrd = |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
    let int = || json!({ "type": "integer" });
    let boolean = || json!({ "type": "boolean" });
    let tier = || json!({ "type": "string", "enum": ["light", "standard", "heavy"] });
    vec![
        Tool::new(
            "amm_run_start",
            "Start an amm run. Returns run_id.",
            schema(
                json!({
                    "repo": sd("Absolute path of the repository (the main session cwd)"),
                    "test_cmd": sd("Full test command for the repo"),
                    "base_branch": s(),
                    "setup_cmd": sd("command to prepare a fresh checkout, e.g. npm ci"),
                }),
                &["repo", "test_cmd"],
            ),
        ),
        Tool::new(
            "amm_brief_add",
            "Add a brief (one isolated unit of work for one worker). Returns brief_id.",
            schema(
                json!({
                    "run_id": s(),
                    "title": s(),
                    "goal": sd("What to do and why, self-contained"),
                    "scope": arrd("Globs relative to repo root the worker may change"),
                    "checks": arrd("Commands that must pass for the brief to be done"),
                    "tier": tier(),
                    "max_turns": int(),
                }),
                &["run_id", "title", "goal", "scope", "checks", "tier"],
            ),
        ),
        Tool::new(
            "amm_plan_import",
            "Import \"### Task N:\" sections of a plan file as draft briefs. Returns id, title, scope per brief.",
            schema(json!({ "run_id": s(), "path": sd("Absolute path of the plan markdown file") }), &["run_id", "path"]),
        ),
        Tool::new(
            "amm_brief_update",
            "Change a draft/queued/blocked/failed brief. Pass ready=true to queue it.",
            schema(
                json!({
                    "brief_id": s(),
                    "title": s(),
                    "goal": s(),
                    "scope": arr(),
                    "checks": arr(),
                    "tier": tier(),
                    "max_turns": int(),
                    "ready": boolean(),
                }),
                &["brief_id"],
            ),
        ),
        Tool::new(
            "amm_dispatch",
            "Reserve worker slots for queued briefs. Returns the exact Agent calls to make (run each with run_in_background: true).",
            schema(json!({ "run_id": s() }), &["run_id"]),
        ),
        Tool::new(
            "amm_brief_get",
            "Worker only: fetch your brief by token.",
            schema(json!({ "token": s() }), &["token"]),
        ),
        Tool::new(
            "amm_report",
            "Worker only: report the result of your brief before finishing.",
            schema(
                json!({
                    "token": s(),
                    "status": { "type": "string", "enum": ["done", "blocked"] },
                    "files": arr(),
                    "notes": sd("At most a few lines"),
                }),
                &["token", "status"],
            ),
        ),
        Tool::new(
            "amm_status",
            "Compact run status: counts plus only what changed since the last call. full=true lists every brief.",
            schema(json!({ "run_id": s(), "full": boolean() }), &[]),
        ),
        Tool::new(
            "amm_nudge",
            "Send a correction to the worker of a brief; delivered on its next tool call.",
            schema(json!({ "brief_id": s(), "message": s() }), &["brief_id", "message"]),
        ),
        Tool::new(
            "amm_cancel",
            "Cancel a brief, or all unfinished briefs of a run.",
            schema(json!({ "id": sd("brief id or run id") }), &["id"]),
        ),
        Tool::new(
            "amm_resume",
            "Resume a run whose integration stopped, once amm/<run> is back on the commit amm recorded.",
            schema(json!({ "run_id": s() }), &["run_id"]),
        ),
    ]
}

struct Shim {
    home: Arc<Path>,
    tools: Arc<Vec<Tool>>,
}

impl Shim {
    async fn call(&self, name: &str, args: Option<JsonObject>) -> CallToolResult {
        let Some(tool) = self.tools.iter().find(|t| t.name == name) else {
            return error_result(format!("unknown tool {name}"));
        };
        // Like the TS shim's zod objects, undeclared arguments are dropped.
        let declared = tool.input_schema.get("properties").and_then(Value::as_object);
        let args: JsonObject =
            args.unwrap_or_default().into_iter().filter(|(k, _)| declared.is_some_and(|p| p.contains_key(k))).collect();
        let op = name.strip_prefix("amm_").unwrap_or(name).to_string();
        let home = self.home.clone();
        let res = tokio::task::spawn_blocking(move || call_daemon(&home, &op, &Value::Object(args)))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        match res {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(as_text(&v))]),
            Err(m) => error_result(m),
        }
    }
}

fn error_result(msg: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("error: {msg}"))])
}

impl ServerHandler for Shim {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("amm", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult { tools: self.tools.as_ref().clone(), ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(self.call(&request.name, request.arguments).await.into())
    }
}

/// Serves MCP over stdin/stdout until the client hangs up.
pub async fn run_shim() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let env: Env = std::env::vars().collect();
    let shim = Shim { home: amm_home(&env).into(), tools: Arc::new(tools()) };
    let service = shim.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
