//! Token usage and USD cost from Claude Code transcript files (JSONL).
//!
//! Scaffold: the signatures are the contract the dashboard builds against;
//! the bodies are implemented by the "cost" brief.

use serde::Serialize;
use std::path::Path;

/// Token counts summed over the assistant messages of one model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Cache writes with a 5-minute TTL.
    pub cache_write_5m: u64,
    /// Cache writes with a 1-hour TTL.
    pub cache_write_1h: u64,
}

/// Usage of one model inside one transcript.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: String,
    pub usage: Usage,
    pub cost_usd: f64,
}

/// Reads a transcript and returns usage per model, sorted by model id.
/// A missing file is `Ok(vec![])`; unreadable lines are skipped.
pub fn transcript_usage(path: &Path) -> std::io::Result<Vec<ModelUsage>> {
    let _ = path;
    Ok(Vec::new())
}

/// USD cost of `usage` on `model` (a model id, possibly date-suffixed).
/// Unknown models cost 0.0.
pub fn cost_usd(model: &str, usage: &Usage) -> f64 {
    let _ = (model, usage);
    0.0
}
