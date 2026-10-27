//! Claude Code hook handlers. Each handler locks `Amm` only for synchronous reads and guarded transitions; git and
//! checks run on the blocking pool with the lock released (see the plan's Concurrency section).
use crate::checks::run_checks;
use crate::config::{Limits, tool_name};
use crate::git::{changed_files, commit_all, committed_files, git, is_ancestor, worktree_info};
use crate::rules::{Counters, RuleContext, ToolEvent, Verdict, evaluate, is_edit_tool};
use crate::scope::in_scope;
use crate::service::{AmmResult, BindClaim, WorktreeCheck, validate_worktree};
use crate::types::{Brief, ReportStatus, Run, Worker, WorkerState};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use crate::service::Amm;

/// A hook payload as Claude Code sends it. Unknown fields are ignored.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct HookInput {
    pub session_id: String,
    pub cwd: String,
    pub hook_event_name: String,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_response: Option<Value>,
    pub agent_transcript_path: Option<String>,
    pub stop_hook_active: Option<bool>,
}

fn allow() -> Value {
    json!({})
}

fn deny(reason: &str) -> Value {
    json!({ "hookSpecificOutput": {
        "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason,
    } })
}

fn add_context(event: &str, text: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text } })
}

fn block_stop(reason: &str) -> Value {
    json!({ "decision": "block", "reason": reason })
}

fn lock(state: &Arc<Mutex<Amm>>) -> MutexGuard<'_, Amm> {
    // A panic in another handler must not take every later hook down with it.
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// JS `String(v ?? '')` for a JSON value.
fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// JS `a ?? b ?? ...` over fields of an object: the first that is present and not null.
fn first_set<'a>(o: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().filter_map(|k| o.get(*k)).find(|v| !v.is_null())
}

fn first_line(s: &str) -> &str {
    s.split('\n').next().unwrap_or("")
}

/// The last `n` chars of `s` (JS `s.slice(-n)`).
fn tail(s: &str, n: usize) -> String {
    let len = s.chars().count();
    s.chars().skip(len.saturating_sub(n)).collect()
}

/// Logs a failure that must not change a decision already made (the daemon's stderr is its log).
fn log_err(what: &str, e: impl std::fmt::Display) {
    eprintln!("amm: {what}: {e}");
}

type ValidateFn = dyn Fn(&Path, &Path) -> Result<WorktreeCheck, String> + Send + Sync;
type DiffFn = dyn Fn(&Path, &str, &str) -> Result<Vec<String>, String> + Send + Sync;

/// The blocking work the handlers delegate, swappable in tests to land a cancel or a branch move mid-flight.
#[derive(Clone)]
pub(crate) struct Seams {
    /// Bind phase 2: `validate_worktree(run_repo, cwd)`.
    pub validate: Arc<ValidateFn>,
    /// The gate's diff: `changed_files(root, base_sha, sha)`.
    pub diff: Arc<DiffFn>,
}

impl Default for Seams {
    fn default() -> Self {
        Seams { validate: Arc::new(validate_worktree), diff: Arc::new(changed_files) }
    }
}

/// Handles one hook event. On an internal error (unknown run or brief, a store failure) it returns `{"error": ...}`,
/// like the TS handler throwing; use `try_handle_hook` to tell the two apart.
pub async fn handle_hook(state: &Arc<Mutex<Amm>>, event: &str, input: HookInput) -> Value {
    match try_handle_hook(state, event, input).await {
        Ok(v) => v,
        Err(e) => json!({ "error": e.0 }),
    }
}

pub async fn try_handle_hook(state: &Arc<Mutex<Amm>>, event: &str, input: HookInput) -> AmmResult<Value> {
    handle_with(state, event, input, Seams::default()).await
}

pub(crate) async fn handle_with(state: &Arc<Mutex<Amm>>, event: &str, input: HookInput, seams: Seams) -> AmmResult<Value> {
    let Some(agent_id) = input.agent_id.clone().filter(|a| !a.is_empty()) else {
        return main_session(state, event, &input);
    };
    match event {
        "SubagentStart" => {
            if !is_worker(&input) {
                return Ok(allow());
            }
            let brief_get = tool_name("amm_brief_get", &lock(state).env);
            Ok(add_context(
                "SubagentStart",
                &format!(
                    "You are an amm worker. Before anything else, load {brief_get} with ToolSearch and call it with the token from your prompt."
                ),
            ))
        }
        "PreToolUse" => pre_tool(state, &agent_id, &input, &seams).await,
        "PostToolUse" | "PostToolUseFailure" => post_tool(state, &agent_id, &input, event),
        "SubagentStop" => subagent_stop(state, &agent_id, &input, &seams).await,
        _ => Ok(allow()),
    }
}

fn is_worker(input: &HookInput) -> bool {
    input.agent_type.as_deref().is_some_and(|t| t.contains("amm-worker"))
}

/// Main session: only amm tool calls are forwarded here, to track which sessions use amm.
fn main_session(state: &Arc<Mutex<Amm>>, event: &str, input: &HookInput) -> AmmResult<Value> {
    static RUN_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\br[0-9a-f]{6}\b").unwrap());
    let mut amm = lock(state);
    amm.touch_session(&input.session_id, &input.cwd)?;
    if event == "PostToolUse" && input.tool_name.as_deref().is_some_and(|t| t.ends_with("amm_run_start")) {
        let text = serde_json::to_string(input.tool_response.as_ref().unwrap_or(&Value::String(String::new())))
            .unwrap_or_default();
        if let Some(run_id) = RUN_ID.find(&text)
            && let Some(run) = amm.store.get::<Run>(run_id.as_str())
        {
            amm.store.put(&Run { session_id: Some(input.session_id.clone()), ..run })?;
        }
    }
    Ok(allow())
}

/// The three bind phases: claim under the lock, validate the worktree on the blocking pool, finish under the lock.
async fn bind(state: &Arc<Mutex<Amm>>, brief_id: &str, agent_id: &str, cwd: &str, seams: &Seams) -> Result<Worker, String> {
    let (claimed, repo) = {
        let mut amm = lock(state);
        match amm.claim_bind(brief_id, agent_id, Path::new(cwd))? {
            BindClaim::Bound(w) => return Ok(w),
            BindClaim::Claimed(w) => {
                let repo = amm.run(&w.run_id).map(|r| r.repo).map_err(|e| e.0);
                (w, repo)
            }
        }
    };
    let check = match repo {
        Ok(repo) => {
            let (cwd, validate) = (cwd.to_string(), seams.validate.clone());
            tokio::task::spawn_blocking(move || validate(Path::new(&repo), Path::new(&cwd)))
                .await
                .unwrap_or_else(|e| Err(format!("worktree validation failed: {e}")))
        }
        Err(e) => Err(e),
    };
    lock(state).finish_bind(&claimed.id, agent_id, check)
}

/// What an amm worker with no worker record may do: load tools, nothing else.
fn unbound(input: &HookInput, tool: &str) -> Value {
    if !is_worker(input) || tool == "ToolSearch" {
        return allow();
    }
    deny("amm: fetch your brief first with amm_brief_get")
}

async fn pre_tool(state: &Arc<Mutex<Amm>>, agent_id: &str, input: &HookInput, seams: &Seams) -> AmmResult<Value> {
    let tool = input.tool_name.clone().unwrap_or_default();
    let ti = input.tool_input.clone().unwrap_or_else(|| json!({}));
    let token = ti.get("token").and_then(Value::as_str).map(str::to_string);
    let (found, brief_get, report) = {
        let amm = lock(state);
        (amm.worker_by_agent(agent_id), tool_name("amm_brief_get", &amm.env), tool_name("amm_report", &amm.env))
    };
    let found = match found {
        Some(w) => w,
        None if is_worker(input) && tool == brief_get && token.is_some() => {
            match bind(state, token.as_deref().unwrap_or_default(), agent_id, &input.cwd, seams).await {
                Ok(w) => w,
                Err(e) => return Ok(deny(&format!("amm: {e}. Work from the worktree amm created for you."))),
            }
        }
        None if is_worker(input) && tool == brief_get => return Ok(allow()),
        None => return Ok(unbound(input, &tool)),
    };
    let (w, b) = {
        let mut amm = lock(state);
        // Re-read: the worker may have moved on, or lost this agent, while the lock was released.
        let Some(found) = amm.store.get::<Worker>(&found.id).filter(|w| w.agent_id.as_deref() == Some(agent_id)) else {
            return Ok(unbound(input, &tool));
        };
        match found.state {
            WorkerState::Cancelled => {
                return Ok(deny("amm: your manager cancelled this brief. Stop now and reply CANCELLED."));
            }
            WorkerState::Escalated | WorkerState::Lost | WorkerState::Done | WorkerState::Blocked => {
                return Ok(deny("amm: this brief was reassigned or is finished. Stop now and reply REASSIGNED."));
            }
            _ => {}
        }
        if found.base_sha.is_none() && tool != brief_get {
            return Ok(deny("amm: setting up your brief, retry"));
        }
        if tool.contains("amm_") && tool != brief_get && tool != report {
            return Ok(deny("amm: workers may only use amm_brief_get and amm_report"));
        }
        if tool.contains("amm_")
            && let Some(token) = &token
            && *token != found.brief_id
        {
            return Ok(deny("amm: that token belongs to another brief. Use only your own token."));
        }
        // A checking worker holds a lease stretched by the gate budget; touching it would cut that back.
        let w = if found.state == WorkerState::Checking {
            found
        } else {
            amm.touch_worker(&found).unwrap_or_else(|e| {
                log_err("touch worker", e);
                found
            })
        };
        let pending = w.pending_correction.clone().filter(|m| !m.is_empty());
        match amm.take_correction(&w) {
            Ok(Some(correction)) => return Ok(deny(&correction)),
            Ok(None) => {}
            // The correction may be cleared already; deliver it rather than lose it.
            Err(e) => {
                log_err("take correction", &e);
                match pending {
                    Some(correction) => return Ok(deny(&correction)),
                    None => return Err(e),
                }
            }
        }
        let b = amm.brief(&w.brief_id)?;
        (w, b)
    };
    // Resolves paths on disk, so it runs on the blocking pool without the lock.
    let reason = {
        let (w, b, tool, ti) = (w.clone(), b, tool.clone(), ti);
        tokio::task::spawn_blocking(move || forbidden(&w, &b, &tool, &ti))
            .await
            .unwrap_or_else(|e| Some(format!("amm: could not check this call ({e}), retry")))
    };
    if let Some(reason) = reason {
        if let Err(e) = lock(state).event("deny", Some(&w.run_id), Some(&w.id), json!({ "tool": tool, "reason": reason })) {
            log_err("deny event", e);
        }
        return Ok(deny(&reason));
    }
    Ok(allow())
}

/// Node's `path.resolve`: lexical, `..` at the root stays at the root.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// realpath of the nearest existing ancestor plus the remaining segments; follows dangling symlinks.
pub fn resolve_real(p: &Path) -> PathBuf {
    resolve_real_at(&normalize(p), 0)
}

fn resolve_real_at(p: &Path, depth: u32) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    if depth < 40
        && std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
        && let Ok(target) = std::fs::read_link(p)
    {
        let dir = p.parent().unwrap_or(Path::new("/"));
        return resolve_real_at(&normalize(&dir.join(target)), depth + 1);
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => resolve_real_at(parent, depth).join(name),
        _ => p.to_path_buf(),
    }
}

/// Why a bound worker may not make this call, if it may not.
pub fn forbidden(w: &Worker, b: &Brief, tool: &str, ti: &Value) -> Option<String> {
    static PUSH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bgit\b[^;&|\n]*\b(push|send-pack)\b").unwrap());
    if tool == "Agent" || tool == "Task" {
        return Some("amm: workers may not spawn agents. Do the work yourself or report \"blocked\".".into());
    }
    if tool == "Bash" && PUSH.is_match(&js_string(ti.get("command"))) {
        return Some("amm: workers may not push. Your manager merges your branch.".into());
    }
    if is_edit_tool(tool)
        && let Some(wt) = w.worktree.as_deref().filter(|s| !s.is_empty())
    {
        let file = js_string(first_set(ti, &["file_path", "notebook_path"]));
        let root = normalize(Path::new(wt));
        let abs = resolve_real(&root.join(&file));
        let Ok(rel) = abs.strip_prefix(&root) else {
            return Some(format!("amm: {file} is outside your worktree ({wt}). Edit only inside it."));
        };
        if rel.components().next() == Some(Component::Normal(".git".as_ref())) {
            return Some("amm: workers may not edit .git.".into());
        }
        if !in_scope(&abs.to_string_lossy(), wt, &b.scope) {
            return Some(format!(
                "amm: {} is outside your scope ({}). Do not change it; mention it in your amm_report instead.",
                rel.to_string_lossy(),
                b.scope.join(", ")
            ));
        }
    }
    None
}

fn bash_failed(response: Option<&Value>) -> bool {
    let Some(r) = response.filter(|r| r.is_object() || r.is_array()) else { return false };
    if r.get("is_error") == Some(&Value::Bool(true)) {
        return true;
    }
    first_set(r, &["exitCode", "exit_code", "returnCode"]).and_then(Value::as_f64).is_some_and(|c| c != 0.0)
}

fn summarize(input: &HookInput, ti: &Value) -> String {
    let s = if input.tool_name.as_deref() == Some("Bash") {
        js_string(ti.get("command"))
    } else {
        js_string(first_set(ti, &["file_path", "pattern", "query"]))
    };
    s.chars().take(160).collect()
}

fn counters(w: &Worker) -> Counters {
    Counters {
        tool_calls: w.tool_calls,
        since_progress: w.since_progress,
        last_cmd: w.last_cmd.clone(),
        last_cmd_fails: w.last_cmd_fails,
        fired_rules: w.fired_rules.clone(),
    }
}

fn post_tool(state: &Arc<Mutex<Amm>>, agent_id: &str, input: &HookInput, event: &str) -> AmmResult<Value> {
    let mut amm = lock(state);
    let Some(w) = amm.worker_by_agent(agent_id).filter(|w| w.state == WorkerState::Running) else {
        return Ok(allow());
    };
    let b = amm.brief(&w.brief_id)?;
    let failed = event == "PostToolUseFailure" || bash_failed(input.tool_response.as_ref());
    let ti = input.tool_input.clone().unwrap_or_else(|| json!({}));
    let ctx = RuleContext { limits: amm.limits.clone(), max_turns: b.max_turns, checks: b.checks.clone() };
    let tool_event = ToolEvent { tool: input.tool_name.clone().unwrap_or_default(), input: ti.clone(), failed };
    let (c, verdict) = evaluate(&counters(&w), &tool_event, &ctx);
    let updated = amm.update_counters(&w, c)?;
    let mut detail = Map::new();
    if let Some(t) = &input.tool_name {
        detail.insert("tool".into(), json!(t));
    }
    detail.insert("failed".into(), json!(failed));
    detail.insert("summary".into(), json!(summarize(input, &ti)));
    // From here the verdict is decided: event or transition failures are logged, never change it.
    if let Err(e) = amm.event("tool", Some(&w.run_id), Some(&w.id), Value::Object(detail)) {
        log_err("tool event", e);
    }
    match verdict {
        Verdict::Correct { rule, message } => {
            let d = json!({ "rule": rule, "message": message, "via": "context" });
            if let Err(e) = amm.event("correction", Some(&w.run_id), Some(&w.id), d) {
                log_err("correction event", e);
            }
            Ok(add_context(event, &message))
        }
        Verdict::Escalate { rule } => {
            if let Err(e) = amm.escalate(&updated, &format!("{rule} rule fired again after a correction")) {
                log_err("escalate", e);
            }
            Ok(add_context(event, "amm: you are being replaced by a stronger worker. Stop now and reply REASSIGNED."))
        }
        Verdict::Ok => Ok(allow()),
    }
}

async fn subagent_stop(state: &Arc<Mutex<Amm>>, agent_id: &str, input: &HookInput, seams: &Seams) -> AmmResult<Value> {
    let (checking, b, repo, limits, report_tool) = {
        let mut amm = lock(state);
        let Some(w) = amm.worker_by_agent(agent_id).filter(|w| w.state == WorkerState::Running) else {
            return Ok(allow());
        };
        let b = amm.brief(&w.brief_id)?;
        if b.report.as_ref().is_some_and(|r| r.status == ReportStatus::Blocked) {
            if let Err(e) = amm.mark_blocked(&w) {
                log_err("mark blocked", e);
            }
            return Ok(allow());
        }
        let Some(checking) = amm.mark_checking(&w, input.agent_transcript_path.clone())? else {
            return Ok(allow());
        };
        let repo = amm.run(&checking.run_id)?.repo;
        (checking, b, repo, amm.limits.clone(), tool_name("amm_report", &amm.env))
    };
    let (problems, gated_sha) = {
        let (w, b, limits, diff) = (checking.clone(), b.clone(), limits.clone(), seams.diff.clone());
        tokio::task::spawn_blocking(move || gate_problems(&w, &b, Path::new(&repo), &limits, &report_tool, &*diff))
            .await
            .unwrap_or_else(|e| (vec![format!("amm could not run the done gate: {e}")], None))
    };
    let mut amm = lock(state);
    if problems.is_empty() {
        if let Err(e) = amm.complete(&checking, gated_sha) {
            log_err("complete", e);
        }
        return Ok(allow());
    }
    let block = || block_stop(&format!("amm done gate failed:\n- {}\nFix these, then finish again.", problems.join("\n- ")));
    // None: the worker was cancelled or swept while its checks ran; let it stop.
    let next = match amm.stop_blocked(&checking, &problems) {
        Ok(Some(next)) => next,
        Ok(None) => return Ok(allow()),
        Err(e) => {
            log_err("stop blocked", e);
            return Ok(block());
        }
    };
    if next.stop_blocks >= limits.max_stop_blocks {
        let reason = format!("done gate failed {} times: {}", next.stop_blocks, first_line(&problems[0]));
        if let Err(e) = amm.escalate(&next, &reason) {
            log_err("escalate", e);
        }
        return Ok(allow());
    }
    Ok(block())
}

/// The worktree must still be what bind validated; checked before and after the checks run.
fn verify_worktree(w: &Worker, repo: &Path) -> Option<String> {
    let (Some(wt), Some(_), Some(branch)) = (w.worktree.as_deref(), w.base_sha.as_deref(), w.branch.as_deref()) else {
        return Some("amm could not find your worktree or its base commit, so your changes cannot be scope-checked".into());
    };
    if !Path::new(wt).exists() {
        return Some("amm could not find your worktree or its base commit, so your changes cannot be scope-checked".into());
    }
    let infos = worktree_info(Path::new(wt)).and_then(|mine| worktree_info(repo).map(|repo| (mine, repo)));
    match infos {
        Err(e) => Some(format!("amm could not inspect your worktree: {}", first_line(&e))),
        Ok((mine, repo)) => {
            if mine.toplevel != Path::new(wt) || mine.common_dir != repo.common_dir {
                return Some("your worktree is no longer a worktree of the repository (was .git removed or replaced?)".into());
            }
            if mine.branch.as_deref() != Some(branch) {
                let on = mine.branch.as_deref().unwrap_or("(detached)");
                return Some(format!("your worktree is on branch {on}, not {branch}. Switch back"));
            }
            None
        }
    }
}

/// The commit `refs/heads/<branch>` points at, if git can read it.
fn branch_head(wt: &str, branch: &str) -> Option<String> {
    git(Path::new(wt), &["rev-parse", "--verify", "--end-of-options", &format!("refs/heads/{branch}^{{commit}}")])
        .ok()
        .filter(|r| r.code == 0)
        .map(|r| r.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The done gate's git and check work. Blocking: run it on the blocking pool, without the lock.
fn gate_problems(
    w: &Worker,
    b: &Brief,
    repo: &Path,
    limits: &Limits,
    report_tool: &str,
    diff: &DiffFn,
) -> (Vec<String>, Option<String>) {
    let mut problems = Vec::new();
    if b.report.is_none() {
        problems.push(format!("you have not called {report_tool} with token \"{}\"", b.id));
    }
    let before = verify_worktree(w, repo);
    if let Some(p) = &before {
        problems.push(p.clone());
    }
    if let Some(wt) = w.worktree.as_deref().filter(|wt| Path::new(wt).exists()) {
        let timeout = limits.check_timeout_ms.max(0) as u64;
        let budget = limits.gate_budget_ms.max(0) as u64;
        for r in run_checks(&b.checks, Path::new(wt), timeout, budget) {
            if !r.ok {
                let how = if r.timed_out {
                    "timed out".to_string()
                } else {
                    format!("exited {}", r.code.map_or("null".to_string(), |c| c.to_string()))
                };
                problems.push(format!("check `{}` {how}:\n{}", r.cmd, tail(&r.output, 1500)));
            }
        }
    }
    // Checks run worker-written code, which may have tampered with the worktree: verify again, then diff.
    let after = if before.is_some() { None } else { verify_worktree(w, repo) };
    if let Some(a) = &after {
        problems.push(format!("after the checks: {a}"));
    }
    if before.is_some() || after.is_some() {
        return (problems, None);
    }
    // verify_worktree passed, so all three are set.
    let (wt, base, branch) = (w.worktree.as_deref().unwrap(), w.base_sha.as_deref().unwrap(), w.branch.as_deref().unwrap());
    // Read the head once and diff exactly that commit: the gated sha is what was scope-checked.
    let Some(sha) = branch_head(wt, branch) else {
        problems.push("amm could not read your branch head".into());
        return (problems, None);
    };
    match diff(Path::new(wt), base, &sha) {
        Ok(files) => {
            let outside: Vec<String> =
                files.into_iter().filter(|f| !in_scope(&Path::new(wt).join(f).to_string_lossy(), wt, &b.scope)).collect();
            if !outside.is_empty() {
                problems.push(format!(
                    "files changed outside your scope: {}. Revert them and describe the needed change in amm_report instead",
                    outside.join(", ")
                ));
            }
        }
        // Fail closed: an unreadable worktree must not pass the gate.
        Err(e) => problems.push(format!("amm could not list your changed files: {}", first_line(&e))),
    }
    // Final identity check: the branch must still point at the commit that was diffed.
    if branch_head(wt, branch).as_deref() != Some(sha.as_str()) {
        problems.push(format!("after the checks: your branch {branch} moved while amm checked it. Finish again"));
    }
    if !problems.is_empty() {
        return (problems, None);
    }
    match commit_work(w, b, wt, base, branch, &sha, repo) {
        Ok(gated) => (problems, Some(gated)),
        Err(p) => {
            problems.push(p);
            (problems, None)
        }
    }
}

/// Commits the scope-checked worktree as amm and returns the commit to gate. Everything is verified again on the
/// commit itself, since a process that outlived the checks may have written after the scope check: the branch must
/// sit on the new commit, which descends from both `sha` (the commit that was scope-checked) and `base`, and every
/// file it changes since `base` must be in scope. A commit that fails these checks is taken back off the branch.
fn commit_work(w: &Worker, b: &Brief, wt: &str, base: &str, branch: &str, sha: &str, repo: &Path) -> Result<String, String> {
    let root = Path::new(wt);
    let new = commit_all(root, &format!("amm({}): {}", b.id, b.title))
        .map_err(|e| format!("amm could not commit your changes: {}", first_line(&e)))?;
    if new == sha {
        return Ok(new);
    }
    let verified = (|| {
        if let Some(p) = verify_worktree(w, repo) {
            return Err(format!("after the commit: {p}"));
        }
        if branch_head(wt, branch).as_deref() != Some(new.as_str()) {
            return Err(format!("after the commit: your branch {branch} moved while amm committed it. Finish again"));
        }
        let descends = is_ancestor(root, sha, &new).and_then(|a| Ok(a && is_ancestor(root, base, &new)?));
        match descends {
            Ok(true) => {}
            Ok(false) => return Err("after the commit: amm's commit does not descend from your base commit".into()),
            Err(e) => return Err(format!("amm could not verify its commit: {}", first_line(&e))),
        }
        let files = committed_files(root, base, &new).map_err(|e| format!("amm could not verify its commit: {}", first_line(&e)))?;
        let outside: Vec<String> =
            files.into_iter().filter(|f| !in_scope(&root.join(f).to_string_lossy(), wt, &b.scope)).collect();
        if !outside.is_empty() {
            return Err(format!(
                "files changed outside your scope: {}. Revert them and describe the needed change in amm_report instead",
                outside.join(", ")
            ));
        }
        Ok(())
    })();
    if let Err(p) = verified {
        // Undo amm's commit only if the branch still points at it; the changes stay staged in the worktree.
        let _ = git(root, &["update-ref", "-m", "amm: undo gate commit", &format!("refs/heads/{branch}"), sha, &new]);
        return Err(p);
    }
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_LIMITS;
    use crate::service::{BriefInput, StartRun};
    use crate::testutil::{add_worktree, init_repo_with, make_amm};
    use crate::types::{Brief, BriefState, Report, ReportStatus, Run, Tier, Worker, WorkerState};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    type State = Arc<Mutex<Amm>>;

    struct Bound {
        state: State,
        repo: PathBuf,
        b: Brief,
        dir: PathBuf,
    }

    impl Bound {
        async fn hook(&self, event: &str, extra: Value) -> Value {
            let mut base = json!({
                "session_id": "s", "cwd": self.repo, "agent_id": "ag1", "agent_type": "amm:amm-worker-light",
                "hook_event_name": event,
            });
            for (k, v) in extra.as_object().unwrap() {
                base[k] = v.clone();
            }
            handle_hook(&self.state, event, serde_json::from_value(base).unwrap()).await
        }
        fn amm(&self) -> std::sync::MutexGuard<'_, Amm> {
            self.state.lock().unwrap()
        }
        fn report(&self, status: ReportStatus) {
            let r = Report { status, files: vec![], notes: String::new() };
            self.amm().report(&self.b.id, r).unwrap();
        }
        fn worker(&self) -> Worker {
            self.amm().worker_by_agent("ag1").unwrap()
        }
    }

    fn brief_input(scope: &[&str], checks: &[&str]) -> BriefInput {
        BriefInput {
            title: "t".into(),
            goal: "g".into(),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            checks: checks.iter().map(|s| s.to_string()).collect(),
            tier: Tier::Light,
            max_turns: None,
        }
    }

    fn rand_name() -> String {
        format!("x{:06x}", rand::random::<u32>() & 0xff_ffff)
    }

    /// An Amm with a run, one dispatched brief and its worktree, not yet bound.
    fn dispatched(checks: &[&str]) -> (State, PathBuf, PathBuf, Run, Brief) {
        let (state, main, repo, run, b, _dir) = dispatched_in(checks);
        (state, main, repo, run, b)
    }

    fn dispatched_in(checks: &[&str]) -> (State, PathBuf, PathBuf, Run, Brief, PathBuf) {
        let mut h = make_amm(DEFAULT_LIMITS);
        let main = init_repo_with(&[("src/a.ts", "a\n"), ("README.md", "r\n")]);
        let repo = add_worktree(&main, &rand_name());
        let run = h
            .amm
            .start_run(StartRun {
                repo: main.to_string_lossy().into(),
                test_cmd: "true".into(),
                base_branch: "main".into(),
            })
            .unwrap();
        let b = h.amm.add_brief(&run.id, brief_input(&["src/**"], checks), BriefState::Queued).unwrap();
        h.amm.dispatch(&run.id).unwrap();
        (Arc::new(Mutex::new(h.amm)), main, repo, run, b, h.dir)
    }

    async fn bound_worker(checks: &[&str]) -> Bound {
        let (state, _main, repo, _run, b, dir) = dispatched_in(checks);
        let h = Bound { state, repo, b, dir };
        h.hook("PreToolUse", json!({ "tool_name": "mcp__plugin_amm_amm__amm_brief_get", "tool_input": { "token": h.b.id } }))
            .await;
        h
    }

    fn deny_reason(out: &Value) -> Option<String> {
        let h = out.get("hookSpecificOutput")?;
        if h.get("permissionDecision")? != "deny" {
            return None;
        }
        Some(h["permissionDecisionReason"].as_str().unwrap().to_string())
    }

    fn deny_of(out: &Value) -> String {
        deny_reason(out).unwrap_or_else(|| panic!("expected a deny, got {out}"))
    }

    fn reason(out: &Value) -> String {
        out.get("reason").and_then(Value::as_str).unwrap_or("").to_string()
    }

    fn p(repo: &Path, rel: &str) -> String {
        repo.join(rel).to_string_lossy().into_owned()
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn commit_all(dir: &Path, msg: &str) {
        git_in(dir, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qam", msg]);
    }

    async fn raw_hook(state: &State, event: &str, v: Value) -> Value {
        handle_hook(state, event, serde_json::from_value(v).unwrap()).await
    }

    #[tokio::test]
    async fn brief_get_binds_the_agent_to_its_brief() {
        let h = bound_worker(&[]).await;
        let w = h.worker();
        assert_eq!(w.brief_id, h.b.id);
        assert_eq!(w.worktree.as_deref(), Some(h.repo.to_str().unwrap()));
    }

    #[tokio::test]
    async fn edits_outside_scope_or_worktree_pushes_and_agent_spawns_are_denied() {
        let h = bound_worker(&[]).await;
        let pre = |tool: &str, ti: Value| h.hook("PreToolUse", json!({ "tool_name": tool, "tool_input": ti }));
        assert_eq!(deny_reason(&pre("Edit", json!({ "file_path": p(&h.repo, "src/a.ts") })).await), None);
        assert!(deny_of(&pre("Write", json!({ "file_path": p(&h.repo, "README.md") })).await).contains("outside your scope"));
        assert!(deny_of(&pre("Write", json!({ "file_path": "/etc/x" })).await).contains("outside your worktree"));
        assert!(deny_of(&pre("Bash", json!({ "command": "git push origin HEAD" })).await).contains("may not push"));
        assert!(deny_of(&pre("Agent", json!({})).await).contains("spawn agents"));
        assert!(deny_of(&pre("Write", json!({ "file_path": p(&h.repo, ".git/config") })).await).contains("may not edit .git"));
    }

    #[tokio::test]
    async fn a_manager_nudge_is_delivered_once_by_denying_the_next_call() {
        let h = bound_worker(&[]).await;
        h.amm().nudge(&h.b.id, "stop").unwrap();
        let read = || h.hook("PreToolUse", json!({ "tool_name": "Read", "tool_input": {} }));
        assert!(deny_of(&read().await).contains("from your manager): stop"));
        assert_eq!(deny_reason(&read().await), None);
    }

    #[tokio::test]
    async fn post_tool_rules_add_context_then_escalate_and_stop_the_worker() {
        let h = bound_worker(&[]).await;
        let fail = || h.hook("PostToolUseFailure", json!({ "tool_name": "Bash", "tool_input": { "command": "make" } }));
        fail().await;
        fail().await;
        let third = fail().await;
        assert!(third.to_string().contains("failed 3 times"), "{third}");
        assert_eq!(third["hookSpecificOutput"]["hookEventName"], "PostToolUseFailure");
        fail().await;
        fail().await;
        fail().await;
        assert_eq!(h.amm().brief(&h.b.id).unwrap().tier, Tier::Standard);
        let out = h.hook("PreToolUse", json!({ "tool_name": "Read", "tool_input": {} })).await;
        assert!(deny_of(&out).contains("reassigned"));
    }

    #[tokio::test]
    async fn done_gate_blocks_without_a_report_or_with_out_of_scope_changes_then_passes() {
        let h = bound_worker(&["test -f src/b.ts"]).await;
        let stop = || h.hook("SubagentStop", json!({ "agent_transcript_path": "/t.jsonl", "stop_hook_active": false }));
        let out = stop().await;
        assert_eq!(out["decision"], "block");
        assert!(reason(&out).contains("not called mcp__plugin_amm_amm__amm_report"), "{out}");
        assert!(reason(&out).contains("check `test -f src/b.ts` exited 1"), "{out}");
        std::fs::write(h.repo.join("README.md"), "changed\n").unwrap();
        std::fs::write(h.repo.join("src/b.ts"), "b\n").unwrap();
        let r = Report { status: ReportStatus::Done, files: vec!["src/b.ts".into()], notes: String::new() };
        h.amm().report(&h.b.id, r).unwrap();
        let out = stop().await;
        assert!(reason(&out).contains("outside your scope: README.md"), "{out}");
        std::fs::write(h.repo.join("README.md"), "r\n").unwrap();
        assert_eq!(stop().await, json!({}));
        assert_eq!(h.amm().brief(&h.b.id).unwrap().state, BriefState::Done);
    }

    #[tokio::test]
    async fn a_blocked_report_ends_the_worker_without_running_checks() {
        let h = bound_worker(&["false"]).await;
        let r = Report { status: ReportStatus::Blocked, files: vec![], notes: "api missing".into() };
        h.amm().report(&h.b.id, r).unwrap();
        assert_eq!(h.hook("SubagentStop", json!({})).await, json!({}));
        assert_eq!(h.amm().brief(&h.b.id).unwrap().state, BriefState::Blocked);
    }

    #[tokio::test]
    async fn three_failed_gates_escalate() {
        let h = bound_worker(&["false"]).await;
        h.report(ReportStatus::Done);
        assert_eq!(h.hook("SubagentStop", json!({})).await["decision"], "block");
        h.hook("SubagentStop", json!({})).await;
        assert_eq!(h.hook("SubagentStop", json!({})).await, json!({}));
        assert_eq!(h.amm().brief(&h.b.id).unwrap().tier, Tier::Standard);
    }

    #[tokio::test]
    async fn events_from_agents_amm_does_not_know_are_allowed_untouched() {
        let state = Arc::new(Mutex::new(make_amm(DEFAULT_LIMITS).amm));
        let out = raw_hook(
            &state,
            "PreToolUse",
            json!({ "session_id": "s", "cwd": "/", "hook_event_name": "PreToolUse", "agent_id": "other",
                    "tool_name": "Bash", "tool_input": { "command": "git push" } }),
        )
        .await;
        assert_eq!(out, json!({}));
    }

    #[tokio::test]
    async fn brief_get_from_the_main_checkout_is_refused() {
        let (state, main, _repo, _run, b) = dispatched(&[]);
        let out = raw_hook(
            &state,
            "PreToolUse",
            json!({ "session_id": "s", "cwd": main, "hook_event_name": "PreToolUse", "agent_id": "ag1",
                    "agent_type": "amm:amm-worker-light", "tool_name": "mcp__plugin_amm_amm__amm_brief_get",
                    "tool_input": { "token": b.id } }),
        )
        .await;
        let r = deny_of(&out);
        assert!(r.starts_with("amm: ") && r.ends_with("Work from the worktree amm created for you."), "{r}");
        assert_eq!(state.lock().unwrap().worker_by_agent("ag1"), None);
    }

    #[tokio::test]
    async fn an_unbound_amm_worker_may_only_fetch_its_brief_other_subagents_are_untouched() {
        let h = bound_worker(&[]).await;
        let call = |agent_type: &str, tool_name: &str| {
            raw_hook(
                &h.state,
                "PreToolUse",
                json!({ "session_id": "s", "cwd": h.repo, "hook_event_name": "PreToolUse", "agent_id": "ag9",
                        "agent_type": agent_type, "tool_name": tool_name,
                        "tool_input": { "file_path": p(&h.repo, "src/a.ts") } }),
            )
        };
        assert!(deny_of(&call("amm:amm-worker-light", "Edit").await).contains("fetch your brief first with amm_brief_get"));
        assert_eq!(deny_reason(&call("amm:amm-worker-light", "ToolSearch").await), None);
        assert_eq!(call("general-purpose", "Edit").await, json!({}));
    }

    #[tokio::test]
    async fn a_bound_worker_cannot_use_another_brief_token() {
        let h = bound_worker(&[]).await;
        let out = h
            .hook("PreToolUse", json!({ "tool_name": "mcp__plugin_amm_amm__amm_report", "tool_input": { "token": "bother" } }))
            .await;
        assert!(deny_of(&out).contains("token"));
        assert_eq!(deny_reason(&h.hook("PreToolUse", json!({ "tool_name": "Read", "tool_input": {} })).await), None);
    }

    #[tokio::test]
    async fn edits_through_a_symlink_that_leaves_the_worktree_are_denied() {
        let h = bound_worker(&[]).await;
        let outside = crate::testutil::tmp_dir().canonicalize().unwrap();
        std::os::unix::fs::symlink(&outside, h.repo.join("src/l")).unwrap();
        let out = h.hook("PreToolUse", json!({ "tool_name": "Edit", "tool_input": { "file_path": p(&h.repo, "src/l/x") } })).await;
        assert!(deny_of(&out).contains("outside your worktree"));
        std::os::unix::fs::symlink(outside.join("nope"), h.repo.join("src/dangling")).unwrap();
        let out =
            h.hook("PreToolUse", json!({ "tool_name": "Write", "tool_input": { "file_path": p(&h.repo, "src/dangling") } })).await;
        assert!(deny_of(&out).contains("outside your worktree"));
    }

    #[tokio::test]
    async fn git_push_is_denied_with_options_before_the_subcommand() {
        let h = bound_worker(&[]).await;
        for command in ["git -C . push", "git -c a=b push origin", "git send-pack x", "cd x && git push"] {
            let out = h.hook("PreToolUse", json!({ "tool_name": "Bash", "tool_input": { "command": command } })).await;
            assert!(deny_of(&out).contains("may not push"), "{command}");
        }
        let out =
            h.hook("PreToolUse", json!({ "tool_name": "Bash", "tool_input": { "command": "git status; echo push" } })).await;
        assert_eq!(deny_reason(&out), None);
    }

    #[tokio::test]
    async fn the_gate_fails_closed_when_the_worktree_is_no_longer_a_worktree() {
        let h = bound_worker(&[]).await;
        h.report(ReportStatus::Done);
        std::fs::remove_file(h.repo.join(".git")).unwrap();
        let out = h.hook("SubagentStop", json!({})).await;
        assert_eq!(out["decision"], "block");
        assert!(reason(&out).contains("worktree"), "{out}");
    }

    #[tokio::test]
    async fn the_gate_fails_when_the_branch_was_switched() {
        let h = bound_worker(&[]).await;
        h.report(ReportStatus::Done);
        git_in(&h.repo, &["checkout", "-q", "-b", "other"]);
        let out = h.hook("SubagentStop", json!({})).await;
        assert!(reason(&out).contains("branch"), "{out}");
    }

    #[tokio::test]
    async fn files_touched_by_the_done_checks_count_toward_the_scope_check() {
        let h = bound_worker(&["echo x > README.md"]).await;
        h.report(ReportStatus::Done);
        let out = h.hook("SubagentStop", json!({})).await;
        assert!(reason(&out).contains("outside your scope: README.md"), "{out}");
    }

    async fn sabotage(kind: &str) {
        let h = bound_worker(&[]).await;
        let base_sha = h.worker().base_sha.unwrap();
        let check = if kind == "rm .git" { "rm .git".to_string() } else { format!("git checkout -q --detach {base_sha}") };
        {
            let mut amm = h.amm();
            let b = amm.brief(&h.b.id).unwrap();
            amm.store.put(&Brief { checks: vec![check], ..b }).unwrap();
        }
        std::fs::write(h.repo.join("README.md"), "evil\n").unwrap();
        commit_all(&h.repo, "evil");
        h.report(ReportStatus::Done);
        let out = h.hook("SubagentStop", json!({})).await;
        assert_eq!(out["decision"], "block");
        assert!(reason(&out).contains("after the checks"), "{out}");
        assert_ne!(h.amm().brief(&h.b.id).unwrap().state, BriefState::Done);
    }

    #[tokio::test]
    async fn a_check_that_runs_rm_git_cannot_hide_a_committed_out_of_scope_change() {
        sabotage("rm .git").await;
    }

    #[tokio::test]
    async fn a_check_that_runs_detach_cannot_hide_a_committed_out_of_scope_change() {
        sabotage("detach").await;
    }

    #[tokio::test]
    async fn a_committed_out_of_scope_change_is_listed_even_when_the_checks_leave_the_worktree_intact() {
        let h = bound_worker(&["true"]).await;
        std::fs::write(h.repo.join("README.md"), "evil\n").unwrap();
        commit_all(&h.repo, "evil");
        h.report(ReportStatus::Done);
        let out = h.hook("SubagentStop", json!({})).await;
        assert!(reason(&out).contains("outside your scope: README.md"), "{out}");
    }

    #[tokio::test]
    async fn while_validation_is_in_progress_a_bound_worker_may_only_call_amm_brief_get() {
        let h = bound_worker(&[]).await;
        let w = h.worker();
        h.amm().store.put(&Worker { base_sha: None, ..w.clone() }).unwrap();
        let out = h.hook("PreToolUse", json!({ "tool_name": "Read", "tool_input": {} })).await;
        assert!(deny_of(&out).contains("setting up your brief, retry"));
        let out = h
            .hook(
                "PreToolUse",
                json!({ "tool_name": "mcp__plugin_amm_amm__amm_brief_get", "tool_input": { "token": w.brief_id } }),
            )
            .await;
        assert_eq!(deny_reason(&out), None);
    }

    #[tokio::test]
    async fn only_the_exact_amm_brief_get_tool_name_is_allowed_before_binding() {
        let h = bound_worker(&[]).await;
        let out = raw_hook(
            &h.state,
            "PreToolUse",
            json!({ "session_id": "s", "cwd": h.repo, "hook_event_name": "PreToolUse", "agent_id": "ag9",
                    "agent_type": "amm:amm-worker-light", "tool_name": "evil__amm_brief_get", "tool_input": { "token": "x" } }),
        )
        .await;
        assert!(deny_of(&out).contains("fetch your brief first"));
    }

    #[tokio::test]
    async fn completion_records_the_verified_branch_head_as_gated_sha() {
        let h = bound_worker(&[]).await;
        std::fs::write(h.repo.join("src/b.ts"), "b\n").unwrap();
        git_in(&h.repo, &["add", "."]);
        git_in(&h.repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "work"]);
        let r = Report { status: ReportStatus::Done, files: vec!["src/b.ts".into()], notes: String::new() };
        h.amm().report(&h.b.id, r).unwrap();
        assert_eq!(h.hook("SubagentStop", json!({})).await, json!({}));
        let w = h.worker();
        assert_eq!(w.state, WorkerState::Done);
        assert_eq!(w.gated_sha, Some(git_in(&h.repo, &["rev-parse", w.branch.as_deref().unwrap()])));
    }


    #[tokio::test]
    async fn the_gate_commits_the_in_scope_work_as_amm_and_gates_that_commit() {
        let h = bound_worker(&[]).await;
        let base = h.worker().base_sha.unwrap();
        std::fs::write(h.repo.join("src/a.ts"), "changed\n").unwrap();
        std::fs::write(h.repo.join("src/b.ts"), "b\n").unwrap();
        h.report(ReportStatus::Done);
        assert_eq!(h.hook("SubagentStop", json!({})).await, json!({}));
        let w = h.worker();
        let head = git_in(&h.repo, &["rev-parse", &format!("refs/heads/{}", w.branch.as_deref().unwrap())]);
        assert_eq!(w.gated_sha.as_deref(), Some(head.as_str()));
        assert_ne!(head, base);
        let meta = git_in(&h.repo, &["log", "-1", "--format=%an <%ae>|%cn <%ce>|%P|%s", &head]);
        assert_eq!(meta, format!("amm <amm@localhost>|amm <amm@localhost>|{base}|amm({}): t", h.b.id));
        let files = git_in(&h.repo, &["show", "--name-only", "--format=", &head]);
        assert_eq!(files.lines().collect::<Vec<_>>(), ["src/a.ts", "src/b.ts"]);
        assert_eq!(git_in(&h.repo, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn a_gate_on_a_clean_tree_makes_no_empty_commit() {
        let h = bound_worker(&[]).await;
        let base = h.worker().base_sha.unwrap();
        h.report(ReportStatus::Done);
        assert_eq!(h.hook("SubagentStop", json!({})).await, json!({}));
        assert_eq!(git_in(&h.repo, &["rev-parse", "HEAD"]), base);
        assert_eq!(h.worker().gated_sha, Some(base));
    }

    #[tokio::test]
    async fn the_gate_commit_runs_no_planted_hook_or_signer() {
        use std::os::unix::fs::PermissionsExt;
        let h = bound_worker(&[]).await;
        let marker = crate::testutil::tmp_dir().join("ran");
        let hooks = crate::testutil::tmp_dir();
        for name in ["pre-commit", "commit-msg", "post-commit"] {
            let f = hooks.join(name);
            std::fs::write(&f, format!("#!/bin/sh\ntouch {}\nexit 1\n", marker.display())).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git_in(&h.repo, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
        git_in(&h.repo, &["config", "commit.gpgsign", "true"]);
        git_in(&h.repo, &["config", "gpg.program", hooks.join("pre-commit").to_str().unwrap()]);
        std::fs::write(h.repo.join("src/b.ts"), "b\n").unwrap();
        h.report(ReportStatus::Done);
        assert_eq!(h.hook("SubagentStop", json!({})).await, json!({}));
        assert!(!marker.exists(), "a planted hook or signer ran");
        let head = git_in(&h.repo, &["rev-parse", "HEAD"]);
        assert_eq!(h.worker().gated_sha, Some(head.clone()));
        assert_eq!(git_in(&h.repo, &["log", "-1", "--format=%an|%G?", &head]), "amm|N");
    }

    #[tokio::test]
    async fn a_failed_commit_fails_the_gate() {
        let h = bound_worker(&[]).await;
        std::fs::write(h.repo.join("src/b.ts"), "b\n").unwrap();
        let gitdir = git_in(&h.repo, &["rev-parse", "--absolute-git-dir"]);
        std::fs::write(Path::new(&gitdir).join("index.lock"), "").unwrap();
        h.report(ReportStatus::Done);
        let out = h.hook("SubagentStop", json!({})).await;
        assert_eq!(out["decision"], "block");
        assert!(reason(&out).contains("amm could not commit your changes"), "{out}");
        assert_eq!(h.amm().brief(&h.b.id).unwrap().state, BriefState::Running);
    }

    #[tokio::test]
    async fn an_out_of_scope_file_written_after_the_scope_check_is_never_gated() {
        let h = bound_worker(&[]).await;
        std::fs::write(h.repo.join("src/b.ts"), "b\n").unwrap();
        h.report(ReportStatus::Done);
        let seams = with_diff(|root, base, sha| {
            let files = crate::git::changed_files(root, base, sha);
            // A worker process that outlived the checks writes outside its scope once the scope check has read the tree.
            std::fs::write(root.join("README.md"), "evil\n").unwrap();
            files
        });
        let out = handle_with(&h.state, "SubagentStop", stop_input(), seams).await.unwrap();
        assert_eq!(out["decision"], "block");
        assert!(reason(&out).contains("outside your scope: README.md"), "{out}");
        assert_eq!(h.worker().gated_sha, None);
        assert_ne!(h.amm().brief(&h.b.id).unwrap().state, BriefState::Done);
        // amm's commit is taken back off the branch; the changes stay staged.
        assert_eq!(git_in(&h.repo, &["rev-parse", "HEAD"]), h.worker().base_sha.unwrap());
        assert!(git_in(&h.repo, &["diff", "--cached", "--name-only"]).contains("README.md"));
    }

    fn stop_input() -> HookInput {
        serde_json::from_value(json!({ "session_id": "s", "cwd": "/", "hook_event_name": "SubagentStop", "agent_id": "ag1",
                                       "agent_type": "amm:amm-worker-light" }))
        .unwrap()
    }

    fn with_diff(diff: impl Fn(&Path, &str, &str) -> Result<Vec<String>, String> + Send + Sync + 'static) -> Seams {
        Seams { diff: Arc::new(diff), ..Seams::default() }
    }

    #[tokio::test]
    async fn the_gate_diffs_the_exact_sha_it_read_and_fails_if_the_branch_moves() {
        let h = bound_worker(&[]).await;
        h.report(ReportStatus::Done);
        let read_head = git_in(&h.repo, &["rev-parse", "HEAD"]);
        let seen = Arc::new(Mutex::new(None));
        let seams = {
            let seen = seen.clone();
            with_diff(move |root, base, sha| {
                *seen.lock().unwrap() = Some(sha.to_string());
                let files = crate::git::changed_files(root, base, sha);
                // A worker process that outlived the checks moves the branch after amm read it.
                std::fs::write(root.join("README.md"), "evil\n").unwrap();
                commit_all(root, "evil");
                files
            })
        };
        let out = handle_with(&h.state, "SubagentStop", stop_input(), seams).await.unwrap();
        assert_eq!(seen.lock().unwrap().as_deref(), Some(read_head.as_str()));
        assert_eq!(out["decision"], "block");
        assert!(reason(&out).contains("after the checks") && reason(&out).contains("moved"), "{out}");
        assert_ne!(h.amm().brief(&h.b.id).unwrap().state, BriefState::Done);
    }

    #[tokio::test]
    async fn a_decided_deny_survives_a_failed_event_write() {
        let h = bound_worker(&[]).await;
        h.amm().nudge(&h.b.id, "stop").unwrap();
        rusqlite::Connection::open(h.dir.join("amm.db")).unwrap().execute_batch("DROP TABLE events").unwrap();
        let read = h.hook("PreToolUse", json!({ "tool_name": "Read", "tool_input": {} })).await;
        assert!(deny_of(&read).contains("from your manager): stop"), "{read}");
        let push = h.hook("PreToolUse", json!({ "tool_name": "Bash", "tool_input": { "command": "git push" } })).await;
        assert!(deny_of(&push).contains("may not push"), "{push}");
    }

    #[tokio::test]
    async fn a_bound_worker_may_only_use_amm_brief_get_and_amm_report() {
        let h = bound_worker(&[]).await;
        for tool in ["amm_cancel", "amm_status", "amm_nudge"] {
            let out = h
                .hook("PreToolUse", json!({ "tool_name": format!("mcp__plugin_amm_amm__{tool}"), "tool_input": { "token": h.b.id } }))
                .await;
            assert!(deny_of(&out).contains("workers may only use amm_brief_get and amm_report"), "{tool}: {out}");
        }
        for tool in ["amm_brief_get", "amm_report"] {
            let out = h
                .hook("PreToolUse", json!({ "tool_name": format!("mcp__plugin_amm_amm__{tool}"), "tool_input": { "token": h.b.id } }))
                .await;
            assert_eq!(deny_reason(&out), None, "{tool}");
        }
    }

    #[tokio::test]
    async fn a_cancel_during_the_gate_stays_cancelled() {
        let h = bound_worker(&[]).await;
        h.report(ReportStatus::Done);
        let seams = {
            let (state, id) = (h.state.clone(), h.b.id.clone());
            with_diff(move |root, base, sha| {
                state.lock().unwrap().cancel(&id).unwrap();
                crate::git::changed_files(root, base, sha)
            })
        };
        assert_eq!(handle_with(&h.state, "SubagentStop", stop_input(), seams).await.unwrap(), json!({}));
        assert_eq!(h.amm().brief(&h.b.id).unwrap().state, BriefState::Cancelled);
        assert_eq!(h.worker().state, WorkerState::Cancelled);
    }

    #[tokio::test]
    async fn an_escalation_during_the_gate_is_not_overwritten_by_completion() {
        let h = bound_worker(&[]).await;
        h.report(ReportStatus::Done);
        let seams = {
            let state = h.state.clone();
            with_diff(move |root, base, sha| {
                let mut amm = state.lock().unwrap();
                let w = amm.worker_by_agent("ag1").unwrap();
                assert!(amm.escalate(&w, "manager").unwrap());
                drop(amm);
                crate::git::changed_files(root, base, sha)
            })
        };
        assert_eq!(handle_with(&h.state, "SubagentStop", stop_input(), seams).await.unwrap(), json!({}));
        let b = h.amm().brief(&h.b.id).unwrap();
        assert_eq!((b.state, b.tier), (BriefState::Queued, Tier::Standard));
        let w = h.worker();
        assert_eq!((w.state, w.gated_sha), (WorkerState::Escalated, None));
    }

    #[tokio::test]
    async fn a_cancel_during_bind_stays_cancelled_and_binds_nothing() {
        let (state, _main, repo, _run, b) = dispatched(&[]);
        let seams = {
            let (st, id) = (state.clone(), b.id.clone());
            Seams {
                validate: Arc::new(move |run_repo, cwd| {
                    st.lock().unwrap().cancel(&id).unwrap();
                    validate_worktree(run_repo, cwd)
                }),
                ..Seams::default()
            }
        };
        let input = serde_json::from_value(json!({ "session_id": "s", "cwd": repo, "hook_event_name": "PreToolUse",
            "agent_id": "ag1", "agent_type": "amm:amm-worker-light", "tool_name": "mcp__plugin_amm_amm__amm_brief_get",
            "tool_input": { "token": b.id } }))
        .unwrap();
        let out = handle_with(&state, "PreToolUse", input, seams).await.unwrap();
        assert!(deny_reason(&out).is_some(), "{out}");
        let amm = state.lock().unwrap();
        assert_eq!(amm.brief(&b.id).unwrap().state, BriefState::Cancelled);
        let w = amm.store.all::<Worker>().into_iter().find(|w| w.brief_id == b.id).unwrap();
        assert_eq!((w.state, w.base_sha), (WorkerState::Cancelled, None));
    }

    // Not in the TS suite: the TS event loop serialised binds; here two hooks race on a multi-threaded runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_concurrent_binds_on_one_brief_bind_exactly_one_agent() {
        let (state, _main, repo, _run, b) = dispatched(&[]);
        let bind = |agent: &str| {
            let state = state.clone();
            let v = json!({ "session_id": "s", "cwd": repo, "hook_event_name": "PreToolUse", "agent_id": agent,
                            "agent_type": "amm:amm-worker-light", "tool_name": "mcp__plugin_amm_amm__amm_brief_get",
                            "tool_input": { "token": b.id } });
            tokio::spawn(async move { handle_hook(&state, "PreToolUse", serde_json::from_value(v).unwrap()).await })
        };
        let (a, c) = (bind("agA"), bind("agB"));
        let outs = [a.await.unwrap(), c.await.unwrap()];
        assert_eq!(outs.iter().filter(|o| deny_reason(o).is_none()).count(), 1, "{outs:?}");
        let amm = state.lock().unwrap();
        let bound: Vec<_> = ["agA", "agB"].iter().filter_map(|a| amm.worker_by_agent(a)).collect();
        assert_eq!(bound.len(), 1);
        assert!(bound[0].base_sha.is_some());
    }

    // Not in the TS suite: the main session touches its session and links the run it started.
    #[tokio::test]
    async fn the_main_session_is_touched_and_linked_to_the_run_it_starts() {
        let (state, main, _repo, run, _b) = dispatched(&[]);
        let out = raw_hook(
            &state,
            "PostToolUse",
            json!({ "session_id": "sess1", "cwd": main, "hook_event_name": "PostToolUse",
                    "tool_name": "mcp__plugin_amm_amm__amm_run_start", "tool_response": [{ "type": "text", "text": format!("run {} started", run.id) }] }),
        )
        .await;
        assert_eq!(out, json!({}));
        let amm = state.lock().unwrap();
        assert!(amm.store.get::<crate::types::Session>("sess1").is_some());
        assert_eq!(amm.run(&run.id).unwrap().session_id.as_deref(), Some("sess1"));
    }

    // Not in the TS suite: SubagentStart context goes only to amm workers.
    #[tokio::test]
    async fn subagent_start_gives_amm_workers_their_first_step() {
        let state = Arc::new(Mutex::new(make_amm(DEFAULT_LIMITS).amm));
        let start = |agent_type: &str| {
            raw_hook(
                &state,
                "SubagentStart",
                json!({ "session_id": "s", "cwd": "/", "hook_event_name": "SubagentStart", "agent_id": "a", "agent_type": agent_type }),
            )
        };
        let out = start("amm:amm-worker-light").await;
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SubagentStart");
        assert!(out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("mcp__plugin_amm_amm__amm_brief_get"));
        assert_eq!(start("general-purpose").await, json!({}));
    }

    #[test]
    fn handle_hook_futures_are_send() {
        fn send<T: Send>(_: &T) {}
        let state = Arc::new(Mutex::new(make_amm(DEFAULT_LIMITS).amm));
        let f = handle_hook(&state, "PreToolUse", HookInput::default());
        send(&f);
    }
}
