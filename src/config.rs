use crate::types::Tier;
use std::collections::HashMap;
use std::path::PathBuf;

pub const TIERS: [Tier; 3] = [Tier::Light, Tier::Standard, Tier::Heavy];
pub const DEFAULT_PORT: u16 = 7878;

pub fn tier_max_turns(tier: Tier) -> i64 {
    match tier {
        Tier::Light => 40,
        Tier::Standard => 80,
        Tier::Heavy => 120,
    }
}

pub fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Light => "light",
        Tier::Standard => "standard",
        Tier::Heavy => "heavy",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub max_workers: usize,
    pub repeat_failures: i64,
    pub no_progress_calls: i64,
    pub budget_fraction: f64,
    pub max_stop_blocks: i64,
    pub lease_ms: i64,
    pub check_timeout_ms: i64,
    /// Overall time one done gate may spend running checks.
    pub gate_budget_ms: i64,
    pub idle_shutdown_ms: i64,
}

pub const DEFAULT_LIMITS: Limits = Limits {
    max_workers: 4,
    repeat_failures: 3,
    no_progress_calls: 8,
    budget_fraction: 0.8,
    max_stop_blocks: 3,
    lease_ms: 10 * 60_000,
    check_timeout_ms: 5 * 60_000,
    gate_budget_ms: 540_000,
    idle_shutdown_ms: 30 * 60_000,
};

pub type Env = HashMap<String, String>;

pub fn load_limits(env: &Env) -> Limits {
    // TS: Number(x) > 0. A fractional capacity c admits ceil(c) holders, so ceil matches it.
    let max_workers = env
        .get("AMM_MAX_WORKERS")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0 && *n <= 1e6)
        .map(|n| n.ceil() as usize);
    Limits {
        max_workers: max_workers.unwrap_or(DEFAULT_LIMITS.max_workers),
        ..DEFAULT_LIMITS
    }
}

pub fn amm_home(env: &Env) -> PathBuf {
    if let Some(h) = env.get("AMM_HOME") {
        return PathBuf::from(h);
    }
    let home = env.get("HOME").cloned().or_else(|| std::env::var("HOME").ok()).unwrap_or_default();
    PathBuf::from(home).join(".amm")
}

pub fn next_tier(tier: Tier) -> Option<Tier> {
    let i = TIERS.iter().position(|t| *t == tier)?;
    TIERS.get(i + 1).copied()
}

// Claude Code names plugin agents "<plugin>:<agent>" and plugin MCP tools
// "mcp__plugin_<plugin>_<server>__<tool>". Task 15 verifies both.
pub fn agent_type(tier: Tier, env: &Env) -> String {
    let prefix = env.get("AMM_AGENT_PREFIX").map(String::as_str).unwrap_or("amm:");
    format!("{prefix}amm-worker-{}", tier_name(tier))
}

pub fn tool_name(tool: &str, env: &Env) -> String {
    let prefix = env.get("AMM_TOOL_PREFIX").map(String::as_str).unwrap_or("mcp__plugin_amm_amm__");
    format!("{prefix}{tool}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn next_tier_climbs_the_ladder_and_stops_at_heavy() {
        assert_eq!(next_tier(Tier::Light), Some(Tier::Standard));
        assert_eq!(next_tier(Tier::Standard), Some(Tier::Heavy));
        assert_eq!(next_tier(Tier::Heavy), None);
    }

    #[test]
    fn load_limits_reads_amm_max_workers_and_ignores_junk() {
        assert_eq!(load_limits(&env(&[("AMM_MAX_WORKERS", "2")])).max_workers, 2);
        assert_eq!(load_limits(&env(&[("AMM_MAX_WORKERS", "x")])).max_workers, DEFAULT_LIMITS.max_workers);
    }

    #[test]
    fn names_follow_claude_code_plugin_conventions_and_can_be_overridden() {
        let none = env(&[]);
        assert_eq!(agent_type(Tier::Light, &none), "amm:amm-worker-light");
        assert_eq!(tool_name("amm_report", &none), "mcp__plugin_amm_amm__amm_report");
        assert_eq!(agent_type(Tier::Heavy, &env(&[("AMM_AGENT_PREFIX", "")])), "amm-worker-heavy");
        assert_eq!(amm_home(&env(&[("AMM_HOME", "/x")])), PathBuf::from("/x"));
    }
}
