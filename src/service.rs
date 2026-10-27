//! The amm state machine: runs, briefs, workers and their leases. Methods are synchronous and take no locks; the
//! daemon wraps `Amm` in a mutex and never holds it across git or check work (see `claim_bind` / `finish_bind`).
use crate::config::{Env, Limits, agent_type, next_tier, tier_max_turns, tier_name, tool_name};
use crate::git::{git, worktree_info};
use crate::plan_import::parse_plan;
use crate::rules::Counters;
use crate::scope::scopes_overlap;
use crate::semaphores::Semaphores;
use crate::store::{EventFilter, Store, StoreError};
use crate::types::{AmmEvent, Brief, BriefState, Clock, Report, Run, RunState, Session, Tier, Worker, WorkerState};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

pub const ACTIVE: [WorkerState; 3] = [WorkerState::Starting, WorkerState::Running, WorkerState::Checking];
const NOTABLE: [&str; 6] = ["done", "escalate", "failed", "blocked", "lost", "cancel"];
/// Worker slots re-acquired outside dispatch never count against capacity (TS: Infinity).
const UNBOUNDED: usize = usize::MAX;

fn new_id(prefix: &str) -> String {
    format!("{prefix}{:06x}", rand::random::<u32>() & 0xff_ffff)
}

fn is_active(s: WorkerState) -> bool {
    ACTIVE.contains(&s)
}

#[derive(Debug, Clone, PartialEq)]
pub struct AmmError(pub String);

impl std::fmt::Display for AmmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AmmError {}

impl From<StoreError> for AmmError {
    fn from(e: StoreError) -> Self {
        AmmError(format!("store: {e}"))
    }
}

impl From<String> for AmmError {
    fn from(e: String) -> Self {
        AmmError(e)
    }
}

pub type AmmResult<T> = Result<T, AmmError>;

fn err<T>(msg: String) -> AmmResult<T> {
    Err(AmmError(msg))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRun {
    pub repo: String,
    pub test_cmd: String,
    pub base_branch: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefInput {
    pub title: String,
    pub goal: String,
    pub scope: Vec<String>,
    pub checks: Vec<String>,
    pub tier: Tier,
    pub max_turns: Option<i64>,
}

/// The fields `update_brief` may change. Anything else on the wire is dropped by serde.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefPatch {
    pub title: Option<String>,
    pub goal: Option<String>,
    pub scope: Option<Vec<String>>,
    pub checks: Option<Vec<String>>,
    pub tier: Option<Tier>,
    pub max_turns: Option<i64>,
    pub ready: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCall {
    #[serde(rename = "briefId")]
    pub brief_id: String,
    pub subagent_type: String,
    pub description: String,
    pub prompt: String,
}

/// What the first bind phase found.
#[derive(Debug, Clone, PartialEq)]
pub enum BindClaim {
    /// The agent was already bound (or being bound): nothing to validate.
    Bound(Worker),
    /// Freshly claimed: run `validate_worktree` without the lock, then `finish_bind`.
    Claimed(Worker),
}

/// A validated worker worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCheck {
    pub toplevel: String,
    pub branch: String,
    pub base_sha: String,
}

/// Bind phase 2, run without `Amm`: checks that `cwd` is an agent worktree of `run_repo` and finds its base commit.
pub fn validate_worktree(run_repo: &Path, cwd: &Path) -> Result<WorktreeCheck, String> {
    let first_line = |e: String| e.lines().next().unwrap_or("").to_string();
    let repo = worktree_info(run_repo).map_err(first_line)?;
    let info = worktree_info(cwd).map_err(first_line)?;
    if info.toplevel == repo.toplevel {
        return Err("this is the main checkout, not a worktree".into());
    }
    if info.common_dir != repo.common_dir {
        return Err("this worktree belongs to a different repository".into());
    }
    let branch = match info.branch {
        Some(b) if b.starts_with("worktree-agent-") => b,
        other => {
            return Err(format!("branch {} is not a worktree-agent-* branch", other.as_deref().unwrap_or("(detached)")));
        }
    };
    let mb = git(&info.toplevel, &["merge-base", "HEAD", &repo.head]).map_err(first_line)?;
    let sha = mb.stdout.trim();
    if mb.code != 0 || sha.is_empty() {
        return Err("could not determine the base commit".into());
    }
    Ok(WorktreeCheck { toplevel: info.toplevel.to_string_lossy().into_owned(), branch, base_sha: sha.to_string() })
}

/// The real path of `path` when it exists, else `path` as given. Touches the filesystem; call it without the lock.
pub fn resolve_repo(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The branch checked out in `repo`, for runs started without a base branch. Runs git; call it without the lock.
pub fn resolve_base_branch(repo: &Path) -> Result<String, String> {
    let r = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let b = r.stdout.trim();
    if r.code != 0 || b.is_empty() {
        return Err(format!("could not resolve the base branch of {}: {}", repo.display(), r.stderr.trim()));
    }
    Ok(b.to_string())
}

/// A JSON object, for event details.
fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

pub struct Amm {
    pub store: Store,
    pub sem: Semaphores,
    pub limits: Limits,
    pub now: Clock,
    /// Process env, for the agent and tool name prefixes.
    pub env: Env,
    /// Most notable events one `status` call reports; the rest wait for the next call.
    pub status_limit: i64,
}

impl Amm {
    pub fn new(store: Store, limits: Limits, now: Clock, env: Env) -> Self {
        let mut sem = Semaphores::new(now.clone());
        // Rebuild worker slots after a restart; leases keep their remaining time.
        let t = now();
        let mut workers = store.all::<Worker>();
        workers.sort_by(by_start);
        for w in workers {
            if !is_active(w.state) {
                continue;
            }
            let has_checks = store.get::<Brief>(&w.brief_id).is_some_and(|b| !b.checks.is_empty());
            let extra = if w.state == WorkerState::Checking && has_checks { limits.gate_budget_ms } else { 0 };
            let remaining = (w.last_event_at + limits.lease_ms + extra - t).max(1);
            sem.try_acquire("workers", &w.id, UNBOUNDED, remaining);
        }
        Amm { store, sem, limits, now, env, status_limit: 10000 }
    }

    fn t(&self) -> i64 {
        (self.now)()
    }

    pub fn event(&self, kind: &str, run_id: Option<&str>, worker_id: Option<&str>, detail: Value) -> AmmResult<()> {
        self.store.add_event(&AmmEvent {
            id: 0,
            ts: self.t(),
            run_id: run_id.map(str::to_string),
            worker_id: worker_id.map(str::to_string),
            kind: kind.to_string(),
            detail: obj(detail),
        })?;
        Ok(())
    }

    pub fn run(&self, run_id: &str) -> AmmResult<Run> {
        self.store.get::<Run>(run_id).ok_or_else(|| AmmError(format!("unknown run {run_id}")))
    }

    pub fn brief(&self, brief_id: &str) -> AmmResult<Brief> {
        self.store.get::<Brief>(brief_id).ok_or_else(|| AmmError(format!("unknown brief {brief_id}")))
    }

    /// Starts a run. No IO: callers pass `repo` through `resolve_repo` and a missing base branch through
    /// `resolve_base_branch` before taking the lock.
    pub fn start_run(&mut self, input: StartRun) -> AmmResult<Run> {
        let repo = input.repo;
        let base_branch = input.base_branch;
        let run = Run {
            id: new_id("r"),
            repo,
            base_branch: base_branch.clone(),
            test_cmd: input.test_cmd,
            session_id: None,
            state: RunState::Active,
            created_at: self.t(),
            status_cursor: 0,
        };
        self.store.put(&run)?;
        self.event("run_start", Some(&run.id), None, serde_json::json!({ "repo": run.repo, "baseBranch": base_branch }))?;
        Ok(run)
    }

    fn active_run(&self, run_id: &str) -> AmmResult<Run> {
        let r = self.run(run_id)?;
        if r.state != RunState::Active {
            return err(format!("run {run_id} is {}", run_state_name(r.state)));
        }
        Ok(r)
    }

    pub fn add_brief(&mut self, run_id: &str, input: BriefInput, state: BriefState) -> AmmResult<Brief> {
        self.active_run(run_id)?;
        let seq = self.store.all::<Brief>().iter().filter(|x| x.run_id == run_id).count() as i64 + 1;
        let b = Brief {
            id: new_id("b"),
            run_id: run_id.to_string(),
            seq,
            title: input.title,
            goal: input.goal,
            scope: input.scope,
            checks: input.checks,
            tier: input.tier,
            max_turns: input.max_turns.unwrap_or_else(|| tier_max_turns(input.tier)),
            state,
            attempt: 1,
            notes: vec![],
            report: None,
        };
        self.store.put(&b)?;
        self.event(
            "brief_add",
            Some(run_id),
            None,
            serde_json::json!({ "briefId": b.id, "title": b.title, "tier": b.tier, "state": state }),
        )?;
        Ok(b)
    }

    /// Imports a plan's markdown (read by the caller, outside the lock); `label` names it in errors.
    pub fn import_plan(&mut self, run_id: &str, md: &str, label: &str) -> AmmResult<Vec<Brief>> {
        self.active_run(run_id)?;
        let tasks = parse_plan(md);
        if tasks.is_empty() {
            return err(format!("no \"### Task N:\" headings found in {label}"));
        }
        tasks
            .into_iter()
            .map(|t| {
                let input =
                    BriefInput { title: t.title, goal: t.body, scope: t.files, checks: vec![], tier: Tier::Standard, max_turns: None };
                self.add_brief(run_id, input, BriefState::Draft)
            })
            .collect()
    }

    pub fn update_brief(&mut self, brief_id: &str, patch: BriefPatch) -> AmmResult<Brief> {
        let b = self.brief(brief_id)?;
        if !matches!(b.state, BriefState::Draft | BriefState::Queued | BriefState::Blocked | BriefState::Failed) {
            return err(format!(
                "brief {brief_id} is {}; only draft, queued, blocked or failed briefs can change",
                brief_state_name(b.state)
            ));
        }
        let mut next = b.clone();
        if let Some(v) = patch.title {
            next.title = v;
        }
        if let Some(v) = patch.goal {
            next.goal = v;
        }
        if let Some(v) = patch.scope {
            next.scope = v;
        }
        if let Some(v) = patch.checks {
            next.checks = v;
        }
        if let Some(v) = patch.tier {
            next.tier = v;
        }
        if let Some(v) = patch.max_turns {
            next.max_turns = v;
        }
        if let (Some(tier), None) = (patch.tier, patch.max_turns) {
            next.max_turns = tier_max_turns(tier);
        }
        if patch.ready == Some(true) {
            if matches!(b.state, BriefState::Blocked | BriefState::Failed) {
                next.attempt = b.attempt + 1;
                next.report = None;
            }
            next.state = BriefState::Queued;
        }
        if next.state == BriefState::Queued && next.scope.is_empty() {
            return err(format!("brief {brief_id} needs a scope before it is queued"));
        }
        self.store.put(&next)?;
        Ok(next)
    }

    pub fn active_workers(&self, run_id: Option<&str>) -> Vec<Worker> {
        self.store
            .all::<Worker>()
            .into_iter()
            .filter(|w| is_active(w.state) && run_id.is_none_or(|r| w.run_id == r))
            .collect()
    }

    /// The newest active worker for a brief.
    pub fn worker_for_brief(&self, brief_id: &str) -> Option<Worker> {
        self.store
            .all::<Worker>()
            .into_iter()
            .filter(|w| w.brief_id == brief_id && is_active(w.state))
            // Newest start wins; on a tie the earliest id, so the answer never depends on map order.
            .max_by(|a, b| a.started_at.cmp(&b.started_at).then_with(|| b.id.cmp(&a.id)))
    }

    /// The earliest-started worker bound to `agent_id` (ties: earliest id).
    pub fn worker_by_agent(&self, agent_id: &str) -> Option<Worker> {
        let mut ws: Vec<Worker> =
            self.store.all::<Worker>().into_iter().filter(|w| w.agent_id.as_deref() == Some(agent_id)).collect();
        ws.sort_by(by_start);
        ws.into_iter().next()
    }

    /// Queued briefs of a run that could start now, in order, given free slots and scope overlap.
    fn dispatchable(&self, run_id: &str) -> AmmResult<Vec<Brief>> {
        let repo = self.run(run_id)?.repo;
        let mut held: Vec<Brief> = Vec::new();
        for w in self.active_workers(None) {
            let b = self.brief(&w.brief_id)?;
            if self.run(&b.run_id)?.repo == repo {
                held.push(b);
            }
        }
        let mut free = self.limits.max_workers as i64 - self.sem.holders("workers").len() as i64;
        let mut queued: Vec<Brief> =
            self.store.all::<Brief>().into_iter().filter(|b| b.run_id == run_id && b.state == BriefState::Queued).collect();
        queued.sort_by_key(|b| b.seq);
        let mut out = Vec::new();
        for b in queued {
            if free <= 0 {
                break;
            }
            if held.iter().any(|h| scopes_overlap(&h.scope, &b.scope)) {
                continue;
            }
            held.push(b.clone());
            out.push(b);
            free -= 1;
        }
        Ok(out)
    }

    pub fn dispatch(&mut self, run_id: &str) -> AmmResult<Vec<AgentCall>> {
        self.active_run(run_id)?;
        let mut calls = Vec::new();
        for b in self.dispatchable(run_id)? {
            let w = self.new_worker(&b);
            if !self.sem.try_acquire("workers", &w.id, self.limits.max_workers, self.limits.lease_ms) {
                break;
            }
            // Store errors are fatal-ish (the daemon reports them), but must not leak the slot just acquired.
            let stored = self.store.put(&w).and_then(|_| self.store.put(&Brief { state: BriefState::Running, ..b.clone() }));
            if let Err(e) = stored {
                self.sem.release(&w.id);
                return Err(e.into());
            }
            self.event(
                "dispatch",
                Some(run_id),
                Some(&w.id),
                serde_json::json!({ "briefId": b.id, "tier": b.tier, "attempt": b.attempt }),
            )?;
            calls.push(AgentCall {
                brief_id: b.id.clone(),
                subagent_type: agent_type(b.tier, &self.env),
                // TS slices UTF-16 units; chars are the nearest safe equivalent.
                description: format!("amm {}: {}", b.id, b.title).chars().take(60).collect(),
                prompt: format!("amm worker. Your brief token is \"{}\".", b.id),
            });
        }
        Ok(calls)
    }

    fn new_worker(&self, b: &Brief) -> Worker {
        Worker {
            id: new_id("w"),
            brief_id: b.id.clone(),
            run_id: b.run_id.clone(),
            agent_id: None,
            tier: b.tier,
            worktree: None,
            branch: None,
            gated_sha: None,
            base_sha: None,
            transcript_path: None,
            state: WorkerState::Starting,
            tool_calls: 0,
            since_progress: 0,
            last_cmd: None,
            last_cmd_fails: 0,
            stop_blocks: 0,
            fired_rules: vec![],
            pending_correction: None,
            started_at: self.t(),
            last_event_at: self.t(),
        }
    }

    pub fn brief_text(&self, brief_id: &str) -> AmmResult<String> {
        let b = self.brief(brief_id)?;
        let report = tool_name("amm_report", &self.env);
        let mut out: Vec<String> = vec![
            format!("# amm brief {}: {}", b.id, b.title),
            String::new(),
            "## Goal".into(),
            b.goal.clone(),
            String::new(),
            "## Scope (you may only create or edit these paths, relative to your worktree root)".into(),
        ];
        out.extend(b.scope.iter().map(|s| format!("- {s}")));
        out.push(String::new());
        out.push("## Done checks (all must pass, run from the worktree root)".into());
        if b.checks.is_empty() {
            out.push("- none".into());
        } else {
            out.extend(b.checks.iter().map(|c| format!("- `{c}`")));
        }
        out.push(String::new());
        if !b.notes.is_empty() {
            out.push("## Notes from earlier attempts".into());
            out.extend(b.notes.iter().map(|n| format!("- {n}")));
            out.push(String::new());
        }
        out.push("## Rules".into());
        out.push("- Work only inside your current worktree. Never push. Never spawn agents.".into());
        out.push("- If something outside your scope must change, do not change it: say so in your report.".into());
        out.push(format!(
            "- When finished, run the done checks, then call {report} with token \"{}\", then reply with at most 10 lines.",
            b.id
        ));
        out.push(format!("- If you are stuck, call {report} with status \"blocked\", explain why, and stop."));
        Ok(out.join("\n"))
    }

    /// Run summaries. With a run id, also the notable events since the last call, and the cursor advances.
    pub fn status(&mut self, run_id: Option<&str>, full: bool) -> AmmResult<String> {
        let runs: Vec<Run> = match run_id {
            Some(id) => vec![self.run(id)?],
            None => {
                let mut rs: Vec<Run> =
                    self.store.all::<Run>().into_iter().filter(|r| r.state == RunState::Active).collect();
                rs.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
                rs
            }
        };
        let mut out: Vec<String> = Vec::new();
        for r in runs {
            let mut briefs: Vec<Brief> = self.store.all::<Brief>().into_iter().filter(|b| b.run_id == r.id).collect();
            briefs.sort_by_key(|b| b.seq);
            let count = |s: BriefState| briefs.iter().filter(|b| b.state == s).count();
            out.push(format!(
                "run {}: {} briefs · {} queued · {} running · {} done · {} blocked · {} failed",
                r.id,
                briefs.len(),
                count(BriefState::Queued),
                count(BriefState::Running),
                count(BriefState::Done),
                count(BriefState::Blocked),
                count(BriefState::Failed)
            ));
            if full {
                for b in &briefs {
                    let calls =
                        self.worker_for_brief(&b.id).map(|w| format!(" · {} calls", w.tool_calls)).unwrap_or_default();
                    out.push(format!(
                        "  {} [{}] {} a{} {}{calls}",
                        b.id,
                        brief_state_name(b.state),
                        tier_name(b.tier),
                        b.attempt,
                        b.title
                    ));
                }
            }
            if run_id.is_some() {
                let fresh = self.store.events(&EventFilter {
                    run_id: Some(r.id.clone()),
                    after_id: Some(r.status_cursor),
                    kinds: Some(NOTABLE.iter().map(|s| s.to_string()).collect()),
                    limit: Some(self.status_limit),
                    ..Default::default()
                })?;
                out.extend(fresh.iter().map(|e| format!("  {}", describe(e))));
                // A full page may have more behind it: stop the cursor at its end so the next call shows the rest.
                let last = match fresh.last() {
                    Some(e) if fresh.len() as i64 >= self.status_limit.clamp(1, 10000) => e.id, // the store clamps the same way
                    _ => self.store.last_event_id(&r.id)?,
                };
                if last > r.status_cursor {
                    let cur = self.run(&r.id)?;
                    self.store.put(&Run { status_cursor: last, ..cur })?;
                }
            }
            if r.state == RunState::Active && !self.dispatchable(&r.id)?.is_empty() {
                out.push("  → queued briefs and free slots: call amm_dispatch".into());
            }
            if !briefs.is_empty() && briefs.iter().all(|b| matches!(b.state, BriefState::Done | BriefState::Cancelled)) {
                out.push("  → all briefs done".into());
            }
        }
        Ok(if out.is_empty() { "no active runs".into() } else { out.join("\n") })
    }

    /// Re-reads a worker so callers never write back a stale copy.
    fn fresh(&self, w: &Worker) -> AmmResult<Worker> {
        self.store.get::<Worker>(&w.id).ok_or_else(|| AmmError(format!("unknown worker {}", w.id)))
    }

    fn put_worker(&mut self, w: &Worker, patch: impl FnOnce(&mut Worker)) -> AmmResult<Worker> {
        let mut next = self.fresh(w)?;
        patch(&mut next);
        self.store.put(&next)?;
        Ok(next)
    }

    /// Writes the patch only if the worker is still in one of the `from` states.
    fn transition(&mut self, w: &Worker, from: &[WorkerState], patch: impl FnOnce(&mut Worker)) -> AmmResult<Option<Worker>> {
        let mut next = self.fresh(w)?;
        if !from.contains(&next.state) {
            return Ok(None);
        }
        patch(&mut next);
        self.store.put(&next)?;
        Ok(Some(next))
    }

    /// Bind phase 1: claims the brief's starting worker for `agent_id` before any git work, so concurrent binds
    /// cannot both win.
    pub fn claim_bind(&mut self, brief_id: &str, agent_id: &str, cwd: &Path) -> Result<BindClaim, String> {
        if let Some(existing) = self.worker_by_agent(agent_id) {
            return Ok(BindClaim::Bound(existing));
        }
        let no_worker = || format!("brief {brief_id} has no unbound worker");
        let w = match self.worker_for_brief(brief_id) {
            Some(w) if w.agent_id.is_none() => w,
            _ => return Err(no_worker()),
        };
        let now = self.t();
        let cwd = cwd.to_string_lossy().into_owned();
        let claimed = self
            .transition(&w, &[WorkerState::Starting], |x| {
                x.agent_id = Some(agent_id.to_string());
                x.worktree = Some(cwd);
                x.state = WorkerState::Running;
                x.last_event_at = now;
            })
            .map_err(|e| e.0)?;
        let Some(claimed) = claimed else { return Err(no_worker()) };
        self.sem.try_acquire("workers", &w.id, UNBOUNDED, self.limits.lease_ms);
        Ok(BindClaim::Claimed(claimed))
    }

    /// Bind phase 3: finalises `agent_id`'s claim with the validated worktree, or releases it. Both are guarded:
    /// only the claimer's own claim, still running, is touched, so a cancel, sweep or later claim that landed while
    /// the worktree was validated wins.
    pub fn finish_bind(
        &mut self,
        worker_id: &str,
        agent_id: &str,
        check: Result<WorktreeCheck, String>,
    ) -> Result<Worker, String> {
        let w = self.store.get::<Worker>(worker_id).ok_or_else(|| format!("unknown worker {worker_id}"))?;
        let ours = w.agent_id.as_deref() == Some(agent_id) && w.state == WorkerState::Running;
        let detail = |extra: Value| {
            let mut d = obj(serde_json::json!({ "briefId": w.brief_id, "agentId": agent_id }));
            d.extend(obj(extra));
            Value::Object(d)
        };
        match check {
            Err(error) => {
                if ours {
                    self.transition(&w, &[WorkerState::Running], |x| {
                        x.agent_id = None;
                        x.worktree = None;
                        x.state = WorkerState::Starting;
                    })
                    .map_err(|e| e.0)?;
                }
                let d = detail(serde_json::json!({ "cwd": w.worktree, "error": error }));
                self.event("bind_refused", Some(&w.run_id), Some(&w.id), d).map_err(|e| e.0)?;
                Err(error)
            }
            Ok(_) if !ours => Err(lost_claim(&w, agent_id)),
            Ok(c) => {
                let toplevel = c.toplevel.clone();
                let bound = self
                    .transition(&w, &[WorkerState::Running], |x| {
                        x.worktree = Some(c.toplevel);
                        x.branch = Some(c.branch);
                        x.base_sha = Some(c.base_sha);
                    })
                    .map_err(|e| e.0)?;
                let Some(bound) = bound else { return Err(lost_claim(&w, agent_id)) };
                let d = detail(serde_json::json!({ "worktree": toplevel }));
                self.event("bind", Some(&w.run_id), Some(&w.id), d).map_err(|e| e.0)?;
                Ok(bound)
            }
        }
    }

    /// All three bind phases in one call. Runs git while holding `&mut self`, so tests only.
    #[cfg(test)]
    pub fn bind_blocking(&mut self, brief_id: &str, agent_id: &str, cwd: &Path) -> Result<Worker, String> {
        match self.claim_bind(brief_id, agent_id, cwd)? {
            BindClaim::Bound(w) => Ok(w),
            BindClaim::Claimed(w) => {
                let check = self.run(&w.run_id).map_err(|e| e.0).and_then(|r| validate_worktree(Path::new(&r.repo), cwd));
                self.finish_bind(&w.id, agent_id, check)
            }
        }
    }

    pub fn touch_worker(&mut self, w: &Worker) -> AmmResult<Worker> {
        if is_active(self.fresh(w)?.state) {
            self.sem.try_acquire("workers", &w.id, UNBOUNDED, self.limits.lease_ms);
        }
        let now = self.t();
        self.put_worker(w, |x| x.last_event_at = now)
    }

    pub fn update_counters(&mut self, w: &Worker, c: Counters) -> AmmResult<Worker> {
        self.put_worker(w, |x| {
            x.tool_calls = c.tool_calls;
            x.since_progress = c.since_progress;
            x.last_cmd = c.last_cmd;
            x.last_cmd_fails = c.last_cmd_fails;
            x.fired_rules = c.fired_rules;
        })
    }

    pub fn take_correction(&mut self, w: &Worker) -> AmmResult<Option<String>> {
        let Some(msg) = self.fresh(w)?.pending_correction.filter(|m| !m.is_empty()) else { return Ok(None) };
        self.put_worker(w, |x| x.pending_correction = None)?;
        self.event(
            "correction",
            Some(&w.run_id),
            Some(&w.id),
            serde_json::json!({ "rule": "manager", "message": msg, "via": "deny" }),
        )?;
        Ok(Some(msg))
    }

    pub fn mark_checking(&mut self, w: &Worker, transcript_path: Option<String>) -> AmmResult<Option<Worker>> {
        let next = self.transition(w, &[WorkerState::Running], |x| {
            x.state = WorkerState::Checking;
            if transcript_path.is_some() {
                x.transcript_path = transcript_path;
            }
        })?;
        let Some(next) = next else { return Ok(None) };
        let has_checks = !self.brief(&w.brief_id)?.checks.is_empty();
        let lease = self.limits.lease_ms + if has_checks { self.limits.gate_budget_ms } else { 0 };
        self.sem.try_acquire("workers", &w.id, UNBOUNDED, lease);
        Ok(Some(next))
    }

    pub fn stop_blocked(&mut self, w: &Worker, problems: &[String]) -> AmmResult<Option<Worker>> {
        let next = self.transition(w, &[WorkerState::Checking], |x| {
            x.state = WorkerState::Running;
            x.stop_blocks += 1;
        })?;
        let Some(next) = next else { return Ok(None) };
        self.sem.try_acquire("workers", &w.id, UNBOUNDED, self.limits.lease_ms);
        self.event("gate_fail", Some(&w.run_id), Some(&w.id), serde_json::json!({ "briefId": w.brief_id, "problems": problems }))?;
        Ok(Some(next))
    }

    pub fn report(&mut self, brief_id: &str, r: Report) -> AmmResult<()> {
        let b = self.brief(brief_id)?;
        self.store.put(&Brief { report: Some(r.clone()), ..b.clone() })?;
        let wid = self.worker_for_brief(&b.id).map(|w| w.id);
        self.event(
            "report",
            Some(&b.run_id),
            wid.as_deref(),
            serde_json::json!({ "briefId": brief_id, "status": r.status, "files": r.files, "notes": r.notes }),
        )
    }

    fn end_worker(
        &mut self,
        w: &Worker,
        from: &[WorkerState],
        state: WorkerState,
        patch: impl FnOnce(&mut Worker),
    ) -> AmmResult<bool> {
        let ended = self.transition(w, from, |x| {
            patch(x);
            x.state = state;
        })?;
        if ended.is_none() {
            return Ok(false);
        }
        self.sem.release(&w.id);
        Ok(true)
    }

    /// Ends a gated worker as done. `gated_sha` is the branch head the gate verified.
    pub fn complete(&mut self, w: &Worker, gated_sha: Option<String>) -> AmmResult<bool> {
        let b = self.brief(&w.brief_id)?;
        if b.state != BriefState::Running {
            return Ok(false);
        }
        if !self.end_worker(w, &[WorkerState::Checking, WorkerState::Running], WorkerState::Done, |x| x.gated_sha = gated_sha)? {
            return Ok(false);
        }
        // The worker has ended and its slot is free; a failed brief write is still reported to the caller.
        self.store.put(&Brief { state: BriefState::Done, ..b.clone() })?;
        self.event("done", Some(&b.run_id), Some(&w.id), serde_json::json!({ "briefId": b.id, "title": b.title }))?;
        Ok(true)
    }

    pub fn mark_blocked(&mut self, w: &Worker) -> AmmResult<bool> {
        if !self.end_worker(w, &[WorkerState::Running, WorkerState::Checking], WorkerState::Blocked, |_| {})? {
            return Ok(false);
        }
        let b = self.brief(&w.brief_id)?;
        self.store.put(&Brief { state: BriefState::Blocked, ..b.clone() })?;
        let notes = b.report.as_ref().map(|r| r.notes.clone()).unwrap_or_default();
        self.event("blocked", Some(&b.run_id), Some(&w.id), serde_json::json!({ "briefId": b.id, "notes": notes }))?;
        Ok(true)
    }

    pub fn escalate(&mut self, w: &Worker, reason: &str) -> AmmResult<bool> {
        if !self.end_worker(w, &ACTIVE, WorkerState::Escalated, |_| {})? {
            return Ok(false);
        }
        let b = self.brief(&w.brief_id)?;
        let mut notes = b.notes.clone();
        notes.push(format!("attempt {} ({}) was stopped: {reason}", b.attempt, tier_name(b.tier)));
        match next_tier(b.tier) {
            Some(tier) => {
                self.store.put(&Brief {
                    tier,
                    max_turns: tier_max_turns(tier),
                    attempt: b.attempt + 1,
                    notes,
                    report: None,
                    state: BriefState::Queued,
                    ..b.clone()
                })?;
                self.event(
                    "escalate",
                    Some(&b.run_id),
                    Some(&w.id),
                    serde_json::json!({ "briefId": b.id, "from": b.tier, "to": tier, "reason": reason }),
                )?;
            }
            None => {
                self.store.put(&Brief { notes, state: BriefState::Failed, ..b.clone() })?;
                self.event("failed", Some(&b.run_id), Some(&w.id), serde_json::json!({ "briefId": b.id, "reason": reason }))?;
            }
        }
        Ok(true)
    }

    pub fn nudge(&mut self, brief_id: &str, message: &str) -> AmmResult<()> {
        let Some(w) = self.worker_for_brief(brief_id) else {
            return err(format!("brief {brief_id} has no active worker"));
        };
        self.put_worker(&w, |x| x.pending_correction = Some(format!("amm (from your manager): {message}")))?;
        self.event("nudge", Some(&w.run_id), Some(&w.id), serde_json::json!({ "briefId": brief_id, "message": message }))
    }

    /// Cancels one brief, or every unfinished brief of a run when given a run id.
    pub fn cancel(&mut self, target: &str) -> AmmResult<()> {
        let run = self.store.get::<Run>(target);
        let mut briefs = match &run {
            Some(_) => self.store.all::<Brief>().into_iter().filter(|b| b.run_id == target).collect(),
            None => vec![self.brief(target)?],
        };
        briefs.sort_by_key(|b| b.seq);
        if let Some(r) = run {
            self.store.put(&Run { state: RunState::Cancelled, ..r })?;
        }
        for b in briefs {
            if matches!(b.state, BriefState::Done | BriefState::Cancelled) {
                continue;
            }
            let w = self.worker_for_brief(&b.id);
            if let Some(w) = &w {
                self.end_worker(w, &ACTIVE, WorkerState::Cancelled, |_| {})?;
            }
            let cur = self.brief(&b.id)?;
            self.store.put(&Brief { state: BriefState::Cancelled, ..cur })?;
            let wid = w.map(|w| w.id);
            self.event("cancel", Some(&b.run_id), wid.as_deref(), serde_json::json!({ "briefId": b.id }))?;
        }
        Ok(())
    }

    /// Marks workers whose lease expired as lost and requeues their briefs.
    pub fn sweep(&mut self) -> AmmResult<()> {
        for id in self.sem.expire() {
            let Some(w) = self.store.get::<Worker>(&id) else { continue };
            if !is_active(w.state) {
                continue;
            }
            self.put_worker(&w, |x| x.state = WorkerState::Lost)?;
            let b = self.brief(&w.brief_id)?;
            if b.state == BriefState::Running {
                self.store.put(&Brief { state: BriefState::Queued, ..b })?;
            }
            self.event("lost", Some(&w.run_id), Some(&w.id), serde_json::json!({ "briefId": w.brief_id }))?;
        }
        Ok(())
    }

    pub fn touch_session(&mut self, id: &str, cwd: &str) -> AmmResult<()> {
        let now = self.t();
        let next = match self.store.get::<Session>(id) {
            Some(s) => Session { last_seen: now, ..s },
            None => Session { id: id.to_string(), cwd: cwd.to_string(), first_seen: now, last_seen: now },
        };
        self.store.put(&next)?;
        Ok(())
    }
}

/// Workers in start order, ties broken by id.
fn by_start(a: &Worker, b: &Worker) -> Ordering {
    a.started_at.cmp(&b.started_at).then_with(|| a.id.cmp(&b.id))
}

/// Why `agent_id` no longer holds the claim on `w`.
fn lost_claim(w: &Worker, agent_id: &str) -> String {
    match w.state {
        WorkerState::Cancelled => "brief was cancelled".into(),
        WorkerState::Lost => "worker lost".into(),
        _ if w.agent_id.as_deref() != Some(agent_id) => "worker was claimed by another agent".into(),
        s => format!("worker is {}", serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()),
    }
}

fn run_state_name(s: RunState) -> &'static str {
    match s {
        RunState::Active => "active",
        RunState::Done => "done",
        RunState::Cancelled => "cancelled",
    }
}

fn brief_state_name(s: BriefState) -> &'static str {
    match s {
        BriefState::Draft => "draft",
        BriefState::Queued => "queued",
        BriefState::Running => "running",
        BriefState::Done => "done",
        BriefState::Blocked => "blocked",
        BriefState::Failed => "failed",
        BriefState::Cancelled => "cancelled",
    }
}

fn describe(e: &AmmEvent) -> String {
    // JS template literals print a missing field as "undefined"; keep that.
    let d = |k: &str| match e.detail.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "undefined".into(),
    };
    match e.kind.as_str() {
        "done" => format!("{} done: {}", d("briefId"), d("title")),
        "escalate" => format!("{} escalated {}→{} ({}) → call amm_dispatch", d("briefId"), d("from"), d("to"), d("reason")),
        "failed" => format!("{} FAILED at the top tier ({}) → fix the brief or ask the user", d("briefId"), d("reason")),
        "blocked" => format!("{} blocked: {}", d("briefId"), d("notes")),
        "lost" => format!("{} worker lost → requeued", d("briefId")),
        kind => {
            let id = if e.detail.contains_key("briefId") { d("briefId") } else { String::new() };
            format!("{id} {kind}").trim().to_string()
        }
    }
}


#[cfg(test)]
mod service_tests {
    use super::*;
    use crate::config::{DEFAULT_LIMITS, Limits};
    use crate::testutil::{TestAmm, make_amm};
    use crate::types::{BriefState, Run, Tier, WorkerState};

    fn setup(limits: Limits) -> (TestAmm, Run) {
        let mut h = make_amm(limits);
        let run = h
            .amm
            .start_run(StartRun { repo: "/repo".into(), test_cmd: "npm test".into(), base_branch: "main".into() })
            .unwrap();
        (h, run)
    }

    fn brief(title: &str, scope: &[&str], tier: Tier) -> BriefInput {
        BriefInput {
            title: title.into(),
            goal: format!("do {title}"),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            checks: vec!["npm test".into()],
            tier,
            max_turns: None,
        }
    }

    #[test]
    fn dispatch_returns_agent_calls_with_tiered_subagent_types_and_tiny_prompts() {
        let (mut h, run) = setup(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let b = amm.add_brief(&run.id, brief("parser", &["src/parser.ts"], Tier::Light), BriefState::Queued).unwrap();
        let calls = amm.dispatch(&run.id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].subagent_type, "amm:amm-worker-light");
        assert!(calls[0].prompt.contains(&format!("token is \"{}\"", b.id)));
        assert!(calls[0].prompt.len() < 220);
        assert_eq!(amm.brief(&b.id).unwrap().state, BriefState::Running);
        assert_eq!(amm.worker_for_brief(&b.id).unwrap().state, WorkerState::Starting);
    }

    #[test]
    fn dispatch_respects_max_workers() {
        let (mut h, run) = setup(Limits { max_workers: 1, ..DEFAULT_LIMITS });
        let amm = &mut h.amm;
        amm.add_brief(&run.id, brief("a", &["src/a.ts"], Tier::Standard), BriefState::Queued).unwrap();
        amm.add_brief(&run.id, brief("b", &["src/b.ts"], Tier::Standard), BriefState::Queued).unwrap();
        assert_eq!(amm.dispatch(&run.id).unwrap().len(), 1);
        assert_eq!(amm.dispatch(&run.id).unwrap().len(), 0);
    }

    #[test]
    fn dispatch_holds_back_briefs_whose_scope_overlaps_a_running_brief() {
        let (mut h, run) = setup(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let b1 = amm.add_brief(&run.id, brief("a", &["src/**"], Tier::Standard), BriefState::Queued).unwrap();
        let b2 = amm.add_brief(&run.id, brief("b", &["src/b.ts"], Tier::Standard), BriefState::Queued).unwrap();
        let b3 = amm.add_brief(&run.id, brief("c", &["docs/**"], Tier::Standard), BriefState::Queued).unwrap();
        let got: Vec<String> = amm
            .dispatch(&run.id)
            .unwrap()
            .iter()
            .map(|c| c.description.split(':').next().unwrap().to_string())
            .collect();
        assert_eq!(got, vec![format!("amm {}", b1.id), format!("amm {}", b3.id)]);
        assert_eq!(amm.brief(&b2.id).unwrap().state, BriefState::Queued);
    }

    #[test]
    fn plan_import_creates_draft_briefs_that_need_ready_before_dispatch() {
        let (mut h, run) = setup(DEFAULT_LIMITS);
        let file = h.dir.join("plan.md");
        std::fs::write(&file, "### Task 1: One\n- Create: `src/one.ts`\n\n### Task 2: Two\nbody\n").unwrap();
        let amm = &mut h.amm;
        let md = std::fs::read_to_string(&file).unwrap();
        let briefs = amm.import_plan(&run.id, &md, &file.to_string_lossy()).unwrap();
        let got: Vec<(String, BriefState, Vec<String>)> =
            briefs.iter().map(|b| (b.title.clone(), b.state, b.scope.clone())).collect();
        assert_eq!(
            got,
            vec![
                ("One".to_string(), BriefState::Draft, vec!["src/one.ts".to_string()]),
                ("Two".to_string(), BriefState::Draft, vec![]),
            ]
        );
        assert_eq!(amm.dispatch(&run.id).unwrap().len(), 0);
        let err = amm.update_brief(&briefs[1].id, BriefPatch { ready: Some(true), ..Default::default() }).unwrap_err();
        assert!(err.to_string().contains("needs a scope"), "{err}");
        amm.update_brief(
            &briefs[0].id,
            BriefPatch {
                tier: Some(Tier::Heavy),
                checks: Some(vec!["npm test".into()]),
                ready: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(amm.brief(&briefs[0].id).unwrap().max_turns, 120);
        assert_eq!(amm.dispatch(&run.id).unwrap().len(), 1);
    }

    #[test]
    fn brief_text_gives_the_worker_goal_scope_checks_and_report_instructions() {
        let (mut h, run) = setup(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let b = amm.add_brief(&run.id, brief("parser", &["src/parser.ts"], Tier::Standard), BriefState::Queued).unwrap();
        let text = amm.brief_text(&b.id).unwrap();
        for part in ["do parser", "- src/parser.ts", "`npm test`", "mcp__plugin_amm_amm__amm_report"] {
            assert!(text.contains(part), "{part}");
        }
    }

    #[test]
    fn status_reports_counts_and_only_new_notable_events() {
        let (mut h, run) = setup(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let b = amm.add_brief(&run.id, brief("a", &["src/a.ts"], Tier::Standard), BriefState::Queued).unwrap();
        amm.dispatch(&run.id).unwrap();
        amm.event("done", Some(&run.id), None, serde_json::json!({ "briefId": b.id, "title": "a" })).unwrap();
        let first = amm.status(Some(&run.id), false).unwrap();
        assert!(first.contains("1 briefs · 0 queued · 1 running"), "{first}");
        assert!(first.contains(&format!("{} done: a", b.id)), "{first}");
        assert!(!amm.status(Some(&run.id), false).unwrap().contains("done: a"));
        let full = amm.status(Some(&run.id), true).unwrap();
        assert!(full.contains(&format!("{} [running] standard a1 a", b.id)), "{full}");
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::config::{DEFAULT_LIMITS, Limits};
    use crate::git::changed_files;
    use crate::testutil::{TestAmm, add_worktree, init_repo, make_amm};
    use crate::types::{Brief, BriefState, Report, ReportStatus, Run, Tier, Worker, WorkerState};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub(super) fn wt(repo: &Path) -> PathBuf {
        add_worktree(repo, &format!("x{:06x}", rand::random::<u32>() & 0xff_ffff))
    }

    pub(super) fn light(title: &str, scope: &str, checks: &[&str]) -> BriefInput {
        BriefInput {
            title: title.into(),
            goal: "g".into(),
            scope: vec![scope.into()],
            checks: checks.iter().map(|s| s.to_string()).collect(),
            tier: Tier::Light,
            max_turns: None,
        }
    }

    pub(super) fn started_with(limits: Limits, checks: &[&str]) -> (TestAmm, Run, Brief, PathBuf) {
        let mut h = make_amm(limits);
        let repo = init_repo();
        let run = h
            .amm
            .start_run(StartRun {
                repo: repo.to_string_lossy().into_owned(),
                test_cmd: "npm test".into(),
                base_branch: "main".into(),
            })
            .unwrap();
        let b = h.amm.add_brief(&run.id, light("t", "src/**", checks), BriefState::Queued).unwrap();
        h.amm.dispatch(&run.id).unwrap();
        (h, run, b, repo)
    }

    fn started(limits: Limits) -> (TestAmm, Run, Brief, PathBuf) {
        started_with(limits, &[])
    }

    fn git(cwd: &Path, args: &[&str]) {
        let o = Command::new("git").args(args).current_dir(cwd).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    fn active(amm: &Amm, brief_id: &str) -> Worker {
        amm.worker_for_brief(brief_id).unwrap()
    }

    #[test]
    fn bind_attaches_an_agent_and_worktree_to_the_starting_worker() {
        let (mut h, _run, b, repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let wt = wt(&repo);
        let w = amm.bind_blocking(&b.id, "agent-1", &wt).unwrap();
        let base = wt.file_name().unwrap().to_string_lossy().replacen("agent-", "worktree-agent-", 1);
        assert_eq!(w.branch.as_deref(), Some(base.as_str()));
        assert_eq!(w.state, WorkerState::Running);
        assert_eq!(w.worktree.as_deref(), Some(wt.to_str().unwrap()));
        let sha = w.base_sha.clone().unwrap();
        assert!(sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()), "{sha}");
        assert_eq!(amm.worker_by_agent("agent-1").unwrap().id, w.id);
        assert!(amm.bind_blocking(&b.id, "agent-2", &wt).is_err());
    }

    #[test]
    fn bind_refuses_the_main_checkout_a_foreign_repo_and_a_non_agent_branch_and_releases_the_claim() {
        let (mut h, _run, b, repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let mut bad = |cwd: &Path, needle: &str| {
            let err = amm.bind_blocking(&b.id, "agent-1", cwd).unwrap_err();
            assert!(err.contains(needle), "{err}");
            let w = amm.worker_for_brief(&b.id).unwrap();
            assert_eq!((w.state, w.agent_id, w.worktree), (WorkerState::Starting, None, None));
        };
        bad(&repo, "main checkout");
        bad(&wt(&init_repo()), "different repository");
        let plain = wt(&repo);
        git(&plain, &["checkout", "-q", "-b", "feature"]);
        bad(&plain, "worktree-agent-");
    }

    #[test]
    fn commits_made_before_bind_stay_in_the_base_head_diff() {
        let (mut h, _run, b, repo) = started(DEFAULT_LIMITS);
        let wt = wt(&repo);
        std::fs::write(wt.join("early.txt"), "x\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "early"]);
        let w = h.amm.bind_blocking(&b.id, "agent-1", &wt).unwrap();
        assert_eq!(
            changed_files(&wt, w.base_sha.as_deref().unwrap(), w.branch.as_deref().unwrap()).unwrap(),
            vec!["early.txt".to_string()]
        );
    }

    #[test]
    fn escalate_climbs_light_standard_heavy_then_fails_the_brief() {
        let (mut h, run, b, _repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        for tier in [Tier::Standard, Tier::Heavy] {
            let w = active(amm, &b.id);
            amm.escalate(&w, "stuck").unwrap();
            assert_eq!(amm.brief(&b.id).unwrap().tier, tier);
            assert_eq!(amm.brief(&b.id).unwrap().state, BriefState::Queued);
            amm.dispatch(&run.id).unwrap();
        }
        let w = active(amm, &b.id);
        amm.escalate(&w, "stuck").unwrap();
        let fb = amm.brief(&b.id).unwrap();
        assert_eq!(fb.state, BriefState::Failed);
        assert_eq!(fb.attempt, 3);
        assert_eq!(fb.notes.len(), 3);
        assert_eq!(amm.sem.holders("workers").len(), 0);
    }

    #[test]
    fn complete_block_and_cancel_end_the_worker_and_free_its_slot() {
        let (mut h, run, b, repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        amm.bind_blocking(&b.id, "agent-1", &wt(&repo)).unwrap();
        let w = active(amm, &b.id);
        assert!(amm.complete(&w, None).unwrap());
        assert_eq!(amm.brief(&b.id).unwrap().state, BriefState::Done);
        let b2 = amm.add_brief(&run.id, light("u", "lib/**", &[]), BriefState::Queued).unwrap();
        amm.dispatch(&run.id).unwrap();
        amm.bind_blocking(&b2.id, "agent-2", &wt(&repo)).unwrap();
        amm.report(&b2.id, Report { status: ReportStatus::Blocked, files: vec![], notes: "needs api change".into() })
            .unwrap();
        let w2 = active(amm, &b2.id);
        amm.mark_blocked(&w2).unwrap();
        assert_eq!(amm.brief(&b2.id).unwrap().state, BriefState::Blocked);
        assert!(amm.status(Some(&run.id), false).unwrap().contains("blocked: needs api change"));
        let b3 = amm.add_brief(&run.id, light("v", "x/**", &[]), BriefState::Queued).unwrap();
        amm.dispatch(&run.id).unwrap();
        let w3 = active(amm, &b3.id);
        amm.cancel(&b3.id).unwrap();
        assert_eq!(amm.store.get::<Worker>(&w3.id).unwrap().state, WorkerState::Cancelled);
        assert_eq!(amm.sem.holders("workers").len(), 0);
    }

    #[test]
    fn sweep_marks_workers_lost_after_the_lease_and_requeues_their_brief() {
        let (mut h, _run, b, _repo) = started(Limits { lease_ms: 1000, ..DEFAULT_LIMITS });
        h.clock.advance(1001);
        h.amm.sweep().unwrap();
        assert_eq!(h.amm.brief(&b.id).unwrap().state, BriefState::Queued);
        assert_eq!(h.amm.store.all::<Worker>()[0].state, WorkerState::Lost);
    }

    #[test]
    fn nudge_queues_a_correction_for_the_active_worker() {
        let (mut h, _run, b, _repo) = started(DEFAULT_LIMITS);
        h.amm.nudge(&b.id, "use the existing helper").unwrap();
        assert_eq!(
            active(&h.amm, &b.id).pending_correction.as_deref(),
            Some("amm (from your manager): use the existing helper")
        );
    }

    // Not in the TS suite: a successful validation must not write into a worker cancelled meanwhile.
    #[test]
    fn finish_bind_after_a_cancel_writes_nothing_even_when_validation_succeeds() {
        let (mut h, run, b, repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let wt = wt(&repo);
        let BindClaim::Claimed(w) = amm.claim_bind(&b.id, "agent-1", &wt).unwrap() else {
            panic!("expected a fresh claim");
        };
        amm.cancel(&b.id).unwrap();
        let check = validate_worktree(Path::new(&amm.run(&run.id).unwrap().repo), &wt);
        assert!(check.is_ok());
        assert_eq!(amm.finish_bind(&w.id, "agent-1", check).unwrap_err(), "brief was cancelled");
        let after = amm.store.get::<Worker>(&w.id).unwrap();
        assert_eq!(after.state, WorkerState::Cancelled);
        assert_eq!((after.branch, after.base_sha), (None, None));
        // The claim's worktree (cwd) is left as the claim wrote it; finish_bind adds nothing.
        assert_eq!(after.worktree, w.worktree);
    }

    // Not in the TS suite: a validation that finishes after another agent took the worker must not touch it.
    #[test]
    fn finish_bind_refuses_when_another_agent_holds_the_worker() {
        let (mut h, run, b, repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let wt = wt(&repo);
        let BindClaim::Claimed(w) = amm.claim_bind(&b.id, "agent-1", &wt).unwrap() else { panic!("claim") };
        amm.store.put(&Worker { agent_id: Some("agent-2".into()), ..w.clone() }).unwrap();
        let check = validate_worktree(Path::new(&amm.run(&run.id).unwrap().repo), &wt);
        let e = amm.finish_bind(&w.id, "agent-1", check).unwrap_err();
        assert!(e.contains("another agent"), "{e}");
        let after = amm.store.get::<Worker>(&w.id).unwrap();
        assert_eq!((after.agent_id.as_deref(), after.state, after.branch), (Some("agent-2"), WorkerState::Running, None));
    }

    // Not in the TS suite: the refusal names what actually happened to the worker.
    #[test]
    fn finish_bind_after_a_sweep_says_the_worker_was_lost() {
        let (mut h, run, b, repo) = started(Limits { lease_ms: 1000, ..DEFAULT_LIMITS });
        let wt = wt(&repo);
        let BindClaim::Claimed(w) = h.amm.claim_bind(&b.id, "agent-1", &wt).unwrap() else { panic!("claim") };
        h.clock.advance(1001);
        h.amm.sweep().unwrap();
        let check = validate_worktree(Path::new(&h.amm.run(&run.id).unwrap().repo), &wt);
        assert_eq!(h.amm.finish_bind(&w.id, "agent-1", check).unwrap_err(), "worker lost");
        assert_eq!(h.amm.store.get::<Worker>(&w.id).unwrap().base_sha, None);
    }

    // Not in the TS suite: ties on started_at resolve to the earliest id, whatever the store's map order.
    #[test]
    fn worker_lookups_break_started_at_ties_by_the_earliest_id() {
        let (mut h, _run, b, _repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let w = amm.worker_for_brief(&b.id).unwrap();
        for id in ["w1", "w0", "w2"] {
            amm.store.put(&Worker { id: id.into(), agent_id: Some("same".into()), ..w.clone() }).unwrap();
        }
        assert_eq!(amm.worker_for_brief(&b.id).unwrap().id, "w0");
        assert_eq!(amm.worker_by_agent("same").unwrap().id, "w0");
    }

    // Not in the TS suite: the daemon resolves the base branch before it takes the lock.
    #[test]
    fn resolve_base_branch_reads_the_checked_out_branch() {
        assert_eq!(resolve_base_branch(&init_repo()).unwrap(), "main");
        assert!(resolve_base_branch(Path::new("/nonexistent-amm-repo")).is_err());
    }

    #[test]
    fn a_concurrent_cancel_is_not_overwritten_when_bind_refuses() {
        let (mut h, run, b, repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let BindClaim::Claimed(w) = amm.claim_bind(&b.id, "agent-1", &repo).unwrap() else {
            panic!("expected a fresh claim");
        };
        amm.cancel(&b.id).unwrap();
        let check = validate_worktree(Path::new(&amm.run(&run.id).unwrap().repo), &repo);
        assert!(amm.finish_bind(&w.id, "agent-1", check).is_err());
        assert_eq!(amm.store.all::<Worker>()[0].state, WorkerState::Cancelled);
    }
}

#[cfg(test)]
mod review_tests {
    use super::lifecycle_tests::{light, started_with, wt};
    use super::*;
    use crate::config::{DEFAULT_LIMITS, Limits};
    use crate::testutil::{TestAmm, make_amm};
    use crate::types::{Brief, BriefState, Run, Worker, WorkerState};
    use std::path::Path;

    fn started(limits: Limits) -> (TestAmm, Run, Brief, std::path::PathBuf) {
        started_with(limits, &["x"])
    }

    fn setup() -> (TestAmm, Run) {
        let mut h = make_amm(DEFAULT_LIMITS);
        let run = h
            .amm
            .start_run(StartRun { repo: "/repo".into(), test_cmd: "t".into(), base_branch: "main".into() })
            .unwrap();
        (h, run)
    }

    #[test]
    fn cancel_while_checking_stop_blocked_and_complete_are_no_ops() {
        let (mut h, _run, b, repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        amm.bind_blocking(&b.id, "a1", &wt(&repo)).unwrap();
        let w = amm.worker_for_brief(&b.id).unwrap();
        assert!(amm.mark_checking(&w, None).unwrap().is_some());
        amm.cancel(&b.id).unwrap();
        assert_eq!(amm.stop_blocked(&w, &["p".to_string()]).unwrap(), None);
        assert!(!amm.complete(&w, None).unwrap());
        assert_eq!(amm.store.get::<Worker>(&w.id).unwrap().state, WorkerState::Cancelled);
        assert_eq!(amm.brief(&b.id).unwrap().state, BriefState::Cancelled);
    }

    #[test]
    fn sweep_while_checking_then_complete_is_a_no_op() {
        let (mut h, _run, b, repo) = started(Limits { lease_ms: 1000, ..DEFAULT_LIMITS });
        h.amm.bind_blocking(&b.id, "a1", &wt(&repo)).unwrap();
        let w = h.amm.worker_for_brief(&b.id).unwrap();
        h.amm.mark_checking(&w, None).unwrap();
        h.clock.advance(1000 + DEFAULT_LIMITS.gate_budget_ms + 1);
        h.amm.sweep().unwrap();
        assert!(!h.amm.complete(&w, None).unwrap());
        assert!(!h.amm.escalate(&w, "x").unwrap());
        assert_eq!(h.amm.brief(&b.id).unwrap().state, BriefState::Queued);
    }

    #[test]
    fn status_scans_only_notable_events_past_1000_noise_events() {
        let (mut h, run) = setup();
        let amm = &mut h.amm;
        for _ in 0..1200 {
            amm.event("tool", Some(&run.id), None, serde_json::json!({})).unwrap();
        }
        amm.event("done", Some(&run.id), None, serde_json::json!({ "briefId": "bx", "title": "zz" })).unwrap();
        assert!(amm.status(Some(&run.id), false).unwrap().contains("bx done: zz"));
        assert!(!amm.status(Some(&run.id), false).unwrap().contains("bx done"));
    }

    // Not in the TS suite: a full page of notable events leaves the rest for the next call.
    #[test]
    fn status_pages_notable_events_when_the_limit_is_hit() {
        let (mut h, run) = setup();
        let amm = &mut h.amm;
        amm.status_limit = 2;
        for id in ["b1", "b2", "b3"] {
            amm.event("done", Some(&run.id), None, serde_json::json!({ "briefId": id, "title": "t" })).unwrap();
        }
        let first = amm.status(Some(&run.id), false).unwrap();
        assert!(first.contains("b1 done") && first.contains("b2 done") && !first.contains("b3 done"), "{first}");
        let second = amm.status(Some(&run.id), false).unwrap();
        assert!(second.contains("b3 done") && !second.contains("b1 done"), "{second}");
        assert!(!amm.status(Some(&run.id), false).unwrap().contains("done:"));
    }

    #[test]
    fn status_without_a_run_id_does_not_advance_cursors() {
        let (mut h, run) = setup();
        let amm = &mut h.amm;
        amm.event("done", Some(&run.id), None, serde_json::json!({ "briefId": "bx", "title": "zz" })).unwrap();
        assert!(!amm.status(None, false).unwrap().contains("bx done"));
        assert!(amm.status(Some(&run.id), false).unwrap().contains("bx done: zz"));
        assert!(!amm.status(Some(&run.id), false).unwrap().contains("bx done"));
    }

    #[test]
    fn concurrent_binds_bind_exactly_one_agent() {
        let (mut h, run, b, repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let (wt1, wt2) = (wt(&repo), wt(&repo));
        // Both claims land before either validation finishes, as with two in-flight binds.
        let c1 = amm.claim_bind(&b.id, "a1", &wt1);
        let c2 = amm.claim_bind(&b.id, "a2", &wt2);
        let run_repo = amm.run(&run.id).unwrap().repo;
        let mut results = Vec::new();
        for (c, cwd) in [(c1, &wt1), (c2, &wt2)] {
            results.push(match c {
                Ok(BindClaim::Claimed(w)) => {
                    let agent = w.agent_id.clone().unwrap();
                    amm.finish_bind(&w.id, &agent, validate_worktree(Path::new(&run_repo), cwd))
                }
                Ok(BindClaim::Bound(w)) => Ok(w),
                Err(e) => Err(e),
            });
        }
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        let w = amm.worker_for_brief(&b.id).unwrap();
        let sha = w.base_sha.unwrap();
        assert!(sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()), "{sha}");
    }

    #[test]
    fn restart_restores_remaining_lease_so_stale_workers_are_swept() {
        let (h, _run, b, _repo) = started(Limits { lease_ms: 1000, ..DEFAULT_LIMITS });
        h.clock.advance(1001);
        let mut amm2 = Amm::new(h.amm.store, Limits { lease_ms: 1000, ..DEFAULT_LIMITS }, h.clock.clock(), Env::new());
        h.clock.advance(1);
        amm2.sweep().unwrap();
        assert_eq!(amm2.store.all::<Worker>()[0].state, WorkerState::Lost);
        assert_eq!(amm2.brief(&b.id).unwrap().state, BriefState::Queued);
    }

    #[test]
    fn expired_but_unswept_lease_is_revived_by_touch_worker() {
        let (mut h, _run, b, _repo) = started(Limits { lease_ms: 1000, ..DEFAULT_LIMITS });
        h.clock.advance(2000);
        let w = h.amm.worker_for_brief(&b.id).unwrap();
        h.amm.touch_worker(&w).unwrap();
        h.amm.sweep().unwrap();
        assert_eq!(h.amm.worker_for_brief(&b.id).unwrap().state, WorkerState::Starting);
    }

    #[test]
    fn update_brief_ignores_non_patchable_fields_and_rejects_empty_scope_queued() {
        let (mut h, run, _b, _repo) = started(DEFAULT_LIMITS);
        let amm = &mut h.amm;
        let d = amm.add_brief(&run.id, light("d", "q/**", &[]), BriefState::Draft).unwrap();
        // The patch type has no state or runId: unknown fields from the wire are dropped.
        let patch: BriefPatch =
            serde_json::from_value(serde_json::json!({ "title": "new", "state": "done", "runId": "zzz" })).unwrap();
        let u = amm.update_brief(&d.id, patch).unwrap();
        assert_eq!(u.title, "new");
        assert_eq!(u.state, BriefState::Draft);
        assert_eq!(u.run_id, run.id);
        let q = amm.add_brief(&run.id, light("q", "p/**", &[]), BriefState::Queued).unwrap();
        let err = amm.update_brief(&q.id, BriefPatch { scope: Some(vec![]), ..Default::default() }).unwrap_err();
        assert!(err.to_string().contains("needs a scope"), "{err}");
    }

    #[test]
    fn cancelled_runs_reject_new_work() {
        let (mut h, run) = setup();
        let amm = &mut h.amm;
        amm.cancel(&run.id).unwrap();
        let e1 = amm.add_brief(&run.id, light("a", "a", &[]), BriefState::Queued).unwrap_err();
        assert!(e1.to_string().contains("is cancelled"), "{e1}");
        let e2 = amm.dispatch(&run.id).unwrap_err();
        assert!(e2.to_string().contains("is cancelled"), "{e2}");
        let e3 = amm.import_plan(&run.id, "", "/nonexistent").unwrap_err();
        assert!(e3.to_string().contains("is cancelled"), "{e3}");
    }

    #[test]
    fn start_run_stores_the_real_path_when_it_exists() {
        let mut h = make_amm(DEFAULT_LIMITS);
        let repo = resolve_repo(&h.dir);
        assert_eq!(repo, h.dir.canonicalize().unwrap());
        assert_eq!(resolve_repo(Path::new("/nonexistent-amm")), Path::new("/nonexistent-amm"));
        let run = h
            .amm
            .start_run(StartRun {
                repo: repo.to_string_lossy().into_owned(),
                test_cmd: "t".into(),
                base_branch: "main".into(),
            })
            .unwrap();
        assert_eq!(run.repo, h.dir.canonicalize().unwrap().to_string_lossy());
    }

    #[test]
    fn status_hints_dispatch_only_when_something_can_start() {
        let (mut h, run) = setup();
        let amm = &mut h.amm;
        amm.add_brief(&run.id, light("a", "src/**", &[]), BriefState::Queued).unwrap();
        amm.dispatch(&run.id).unwrap();
        amm.add_brief(&run.id, light("b", "src/b.ts", &[]), BriefState::Queued).unwrap();
        assert!(!amm.status(Some(&run.id), false).unwrap().contains("amm_dispatch"));
        amm.add_brief(&run.id, light("c", "docs/**", &[]), BriefState::Queued).unwrap();
        assert!(amm.status(Some(&run.id), false).unwrap().contains("amm_dispatch"));
    }

    #[test]
    fn dispatch_prompt_is_minimal() {
        let (h, _run, _b, _repo) = started(DEFAULT_LIMITS);
        assert_eq!(h.amm.store.all::<Worker>().len(), 1);
        let (mut r, run) = setup();
        let b2 = r.amm.add_brief(&run.id, light("a", "a", &[]), BriefState::Queued).unwrap();
        assert_eq!(r.amm.dispatch(&run.id).unwrap()[0].prompt, format!("amm worker. Your brief token is \"{}\".", b2.id));
    }

    #[test]
    fn status_without_a_run_id_lists_summaries_only_and_keeps_the_cursor() {
        let (mut h, run) = setup();
        let amm = &mut h.amm;
        let b = amm.add_brief(&run.id, light("a", "src/**", &[]), BriefState::Queued).unwrap();
        amm.dispatch(&run.id).unwrap();
        amm.cancel(&b.id).unwrap();
        let all = amm.status(None, false).unwrap();
        assert!(all.starts_with("run "), "{all}");
        assert!(!all.contains("cancel"), "{all}");
        assert!(amm.status(Some(&run.id), false).unwrap().contains("cancel"));
    }
}
