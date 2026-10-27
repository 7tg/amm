use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Injectable clock, in ms since the epoch.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Light,
    Standard,
    Heavy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Active,
    Done,
    Cancelled,
    /// Integration stopped: amm/<run> no longer matches what amm left there.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefState {
    Draft,
    Queued,
    Running,
    /// Passed the gate; waiting in the merge queue.
    Done,
    Blocked,
    Failed,
    Cancelled,
    /// Being merged into the integration branch.
    Merging,
    /// Merged, and the integration tests passed.
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerState {
    Starting,
    Running,
    Checking,
    Done,
    Blocked,
    Lost,
    Escalated,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub repo: String,
    pub base_branch: String,
    pub test_cmd: String,
    pub session_id: Option<String>,
    pub state: RunState,
    pub created_at: i64,
    /// Id of the last event already reported by amm_status.
    pub status_cursor: i64,
    /// `amm/<run>`; empty for runs started before integration existed.
    #[serde(default)]
    pub integration_branch: String,
    /// Where `integration_branch` is checked out.
    #[serde(default)]
    pub integration_worktree: Option<String>,
    /// Prepares a fresh checkout (e.g. `npm ci`); run in the integration worktree at run start.
    #[serde(default)]
    pub setup_cmd: Option<String>,
    /// Whether the last integration test run passed.
    #[serde(default)]
    pub last_test_ok: Option<bool>,
    /// Briefs merged into `integration_branch`.
    #[serde(default)]
    pub merges: u32,
    /// The commit amm last left `integration_branch` on (its creation, then each merge). The gate trusts this, never
    /// the branch ref, which a worker could move.
    #[serde(default)]
    pub integration_head: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportStatus {
    Done,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub status: ReportStatus,
    pub files: Vec<String>,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Brief {
    pub id: String,
    pub run_id: String,
    pub seq: i64,
    pub title: String,
    pub goal: String,
    pub scope: Vec<String>,
    pub checks: Vec<String>,
    pub tier: Tier,
    pub max_turns: i64,
    pub state: BriefState,
    pub attempt: i64,
    pub notes: Vec<String>,
    pub report: Option<Report>,
    /// Times this brief's merge failed the integration tests.
    #[serde(default)]
    pub integration_failures: u32,
    /// Times the merge queue could not even try this brief's merge (a git error); 3 fail it.
    #[serde(default)]
    pub integration_errors: u32,
    /// The last such error, so a repeat is not reported again.
    #[serde(default)]
    pub last_integration_error: Option<String>,
    /// The merge queue skips the brief until then (ms since the epoch).
    #[serde(default)]
    pub retry_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Worker {
    pub id: String,
    pub brief_id: String,
    pub run_id: String,
    pub agent_id: Option<String>,
    pub tier: Tier,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// Branch head the done gate verified; the merge queue merges this sha, never the branch name.
    pub gated_sha: Option<String>,
    pub base_sha: Option<String>,
    pub transcript_path: Option<String>,
    pub state: WorkerState,
    pub tool_calls: i64,
    pub since_progress: i64,
    pub last_cmd: Option<String>,
    pub last_cmd_fails: i64,
    pub stop_blocks: i64,
    pub fired_rules: Vec<String>,
    pub pending_correction: Option<String>,
    pub started_at: i64,
    pub last_event_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub first_seen: i64,
    pub last_seen: i64,
    #[serde(default)]
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmmEvent {
    pub id: i64,
    pub ts: i64,
    pub run_id: Option<String>,
    pub worker_id: Option<String>,
    pub kind: String,
    pub detail: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not in the TS suite: guards the stored JSON shape that the port must keep.
    #[test]
    fn stored_json_uses_camel_case_and_lowercase_states() {
        let r = Run {
            id: "r1".into(),
            repo: "/r".into(),
            base_branch: "main".into(),
            test_cmd: "t".into(),
            session_id: None,
            state: RunState::Active,
            created_at: 1,
            status_cursor: 0,
            integration_branch: "amm/r1".into(),
            integration_worktree: Some("/r/.claude/worktrees/amm-r1".into()),
            setup_cmd: None,
            last_test_ok: None,
            merges: 0,
            integration_head: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["baseBranch"], "main");
        assert_eq!(v["sessionId"], serde_json::Value::Null);
        assert_eq!(v["state"], "active");
        assert_eq!(v["integrationBranch"], "amm/r1");
        let back: Run = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn runs_stored_before_integration_still_load() {
        let old = serde_json::json!({ "id": "r1", "repo": "/r", "baseBranch": "main", "testCmd": "t", "sessionId": null,
                                      "state": "active", "createdAt": 1, "statusCursor": 0 });
        let r: Run = serde_json::from_value(old).unwrap();
        assert_eq!((r.integration_branch.as_str(), r.integration_worktree, r.merges), ("", None, 0));
    }
}
