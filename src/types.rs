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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefState {
    Draft,
    Queued,
    Running,
    Done,
    Blocked,
    Failed,
    Cancelled,
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
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["baseBranch"], "main");
        assert_eq!(v["sessionId"], serde_json::Value::Null);
        assert_eq!(v["state"], "active");
        let back: Run = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }
}
