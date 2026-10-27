use crate::config::Limits;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Counters {
    pub tool_calls: i64,
    pub since_progress: i64,
    pub last_cmd: Option<String>,
    pub last_cmd_fails: i64,
    pub fired_rules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolEvent {
    pub tool: String,
    pub input: Value,
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Ok,
    Correct { rule: String, message: String },
    Escalate { rule: String },
}

#[derive(Debug, Clone)]
pub struct RuleContext {
    pub limits: Limits,
    pub max_turns: i64,
    pub checks: Vec<String>,
}

pub fn is_edit_tool(tool: &str) -> bool {
    matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
}

pub fn message_for(rule: &str, ctx: &RuleContext) -> String {
    match rule {
        "repeat-failure" => format!(
            "amm watchdog: the same command has failed {} times. Stop retrying it. Change approach, or call amm_report with status \"blocked\" and explain why.",
            ctx.limits.repeat_failures
        ),
        "no-progress" => format!(
            "amm watchdog: {} tool calls without changing a file or passing a check. Re-read your brief and make the change, or report \"blocked\".",
            ctx.limits.no_progress_calls
        ),
        "budget" => format!(
            "amm watchdog: you have used {}% of your tool budget. Finish the current change, run your checks, call amm_report and stop.",
            // JS Math.round rounds halves up.
            (ctx.limits.budget_fraction * 100.0 + 0.5).floor()
        ),
        _ => format!("amm watchdog: {rule}"),
    }
}

fn forget(c: &mut Counters, rule: &str) {
    c.fired_rules.retain(|r| r != rule);
}

/// JS `String(input.command ?? '')`.
fn command_string(input: &Value) -> String {
    match input.get("command") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Updates the worker's counters for one tool call and decides whether to correct or escalate.
pub fn evaluate(c: &Counters, e: &ToolEvent, ctx: &RuleContext) -> (Counters, Verdict) {
    let mut next = c.clone();
    next.tool_calls = c.tool_calls + 1;
    let cmd = (e.tool == "Bash").then(|| command_string(&e.input));
    let passed_check = cmd.as_ref().is_some_and(|c| !e.failed && ctx.checks.contains(c));
    let progressed = (is_edit_tool(&e.tool) && !e.failed) || passed_check;
    next.since_progress = if progressed { 0 } else { c.since_progress + 1 };
    if progressed {
        forget(&mut next, "no-progress");
    }
    if let Some(cmd) = &cmd {
        let same = c.last_cmd.as_deref() == Some(cmd.as_str());
        if if same { !e.failed } else { e.failed } {
            forget(&mut next, "repeat-failure");
        }
        if e.failed {
            next.last_cmd_fails = if same { c.last_cmd_fails + 1 } else { 1 };
            next.last_cmd = Some(cmd.clone());
        } else if same {
            next.last_cmd_fails = 0;
        }
    }

    fn fire(mut next: Counters, rule: &str, ctx: &RuleContext) -> (Counters, Verdict) {
        if next.fired_rules.iter().any(|r| r == rule) {
            return (next, Verdict::Escalate { rule: rule.to_string() });
        }
        next.fired_rules.push(rule.to_string());
        (next, Verdict::Correct { rule: rule.to_string(), message: message_for(rule, ctx) })
    }

    if next.last_cmd_fails >= ctx.limits.repeat_failures {
        next.last_cmd_fails = 0;
        return fire(next, "repeat-failure", ctx);
    }
    if next.since_progress >= ctx.limits.no_progress_calls {
        next.since_progress = 0;
        return fire(next, "no-progress", ctx);
    }
    let budget = (ctx.max_turns as f64 * ctx.limits.budget_fraction).ceil();
    if next.tool_calls as f64 >= budget && !next.fired_rules.iter().any(|r| r == "budget") {
        next.fired_rules.push("budget".to_string());
        let message = message_for("budget", ctx);
        return (next, Verdict::Correct { rule: "budget".to_string(), message });
    }
    (next, Verdict::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_LIMITS;
    use serde_json::json;

    fn fresh() -> Counters {
        Counters { tool_calls: 0, since_progress: 0, last_cmd: None, last_cmd_fails: 0, fired_rules: vec![] }
    }

    fn ctx() -> RuleContext {
        RuleContext { limits: DEFAULT_LIMITS, max_turns: 100, checks: vec!["npm test".to_string()] }
    }

    fn feed(events: Vec<ToolEvent>, start: Counters) -> (Counters, Vec<Verdict>) {
        let ctx = ctx();
        let mut c = start;
        let mut verdicts = vec![];
        for e in &events {
            let (next, v) = evaluate(&c, e, &ctx);
            c = next;
            verdicts.push(v);
        }
        (c, verdicts)
    }

    fn bash(command: &str, failed: bool) -> ToolEvent {
        ToolEvent { tool: "Bash".into(), input: json!({ "command": command }), failed }
    }
    fn read() -> ToolEvent {
        ToolEvent { tool: "Read".into(), input: json!({ "file_path": "a" }), failed: false }
    }
    fn edit() -> ToolEvent {
        ToolEvent { tool: "Edit".into(), input: json!({ "file_path": "a" }), failed: false }
    }
    fn reads(n: usize) -> Vec<ToolEvent> {
        (0..n).map(|_| read()).collect()
    }
    fn is_ok(v: &Verdict) -> bool {
        *v == Verdict::Ok
    }
    fn rule_of(v: &Verdict) -> &str {
        match v {
            Verdict::Correct { rule, .. } | Verdict::Escalate { rule } => rule,
            Verdict::Ok => panic!("ok verdict has no rule"),
        }
    }

    #[test]
    fn same_command_failing_3_times_triggers_a_correction_then_escalation() {
        let (c, v) = feed(vec![bash("make", true), bash("make", true), bash("make", true)], fresh());
        assert!(matches!(v.last().unwrap(), Verdict::Correct { .. }));
        let (_, again) = feed(vec![bash("make", true), bash("make", true), bash("make", true)], c);
        assert_eq!(again.last().unwrap(), &Verdict::Escalate { rule: "repeat-failure".into() });
    }

    #[test]
    fn a_different_failing_command_resets_the_repeat_counter() {
        let (c, v) = feed(vec![bash("a", true), bash("a", true), bash("b", true)], fresh());
        assert_eq!(c.last_cmd_fails, 1);
        assert!(v.iter().all(is_ok));
    }

    #[test]
    fn eight_calls_without_an_edit_or_passing_check_is_no_progress() {
        let (_, v) = feed(reads(8), fresh());
        assert!(matches!(v.last().unwrap(), Verdict::Correct { .. }));
        assert_eq!(rule_of(v.last().unwrap()), "no-progress");
    }

    #[test]
    fn edits_and_passing_checks_count_as_progress() {
        let mut ev = reads(7);
        ev.push(edit());
        ev.extend(reads(7));
        ev.push(bash("npm test", false));
        ev.push(read());
        let (_, v) = feed(ev, fresh());
        assert!(v.iter().all(is_ok));
    }

    #[test]
    fn budget_fires_once_at_80_percent_of_max_turns_and_never_escalates() {
        let ev = (0..90).map(|i| if i % 2 == 1 { edit() } else { read() }).collect();
        let (_, v) = feed(ev, fresh());
        let budget: Vec<_> = v.iter().filter(|v| !is_ok(v)).collect();
        assert_eq!(budget.len(), 1);
        assert_eq!(rule_of(budget[0]), "budget");
    }

    #[test]
    fn a_success_between_failures_resets_the_repeat_counter() {
        let (c, v) = feed(
            vec![bash("make", true), bash("make", true), bash("make", false), bash("make", true), bash("make", true)],
            fresh(),
        );
        assert!(v.iter().all(is_ok));
        let (_, next) = evaluate(&c, &bash("make", true), &ctx());
        assert!(matches!(next, Verdict::Correct { .. }));
    }

    #[test]
    fn no_progress_forgets_it_fired_after_an_edit() {
        let mut ev = reads(8);
        ev.push(edit());
        ev.extend(reads(8));
        let (_, v) = feed(ev, fresh());
        let corrects = v.iter().filter(|v| matches!(v, Verdict::Correct { .. })).count();
        assert_eq!(corrects, 2);
        assert!(!v.iter().any(|v| matches!(v, Verdict::Escalate { .. })));
    }

    #[test]
    fn no_progress_escalates_when_it_fires_again_without_recovery() {
        let (_, v) = feed(reads(16), fresh());
        assert_eq!(v.last().unwrap(), &Verdict::Escalate { rule: "no-progress".into() });
    }

    #[test]
    fn a_failed_edit_does_not_count_as_progress() {
        let failed_edit = ToolEvent { tool: "Edit".into(), input: json!({ "file_path": "a" }), failed: true };
        let mut ev = reads(7);
        ev.push(failed_edit);
        let (_, v) = feed(ev, fresh());
        assert_eq!(rule_of(v.last().unwrap()), "no-progress");
    }

    #[test]
    fn an_unrelated_successful_command_does_not_reset_repeat_failure() {
        let (c, _) = feed(vec![bash("make", true), bash("make", true), bash("make", true), bash("ls", false)], fresh());
        let (_, again) = feed(vec![bash("make", true), bash("make", true), bash("make", true)], c);
        assert_eq!(again.last().unwrap(), &Verdict::Escalate { rule: "repeat-failure".into() });
    }

    #[test]
    fn repeat_failure_is_forgotten_when_a_different_command_fails() {
        let (c, _) = feed(vec![bash("make", true), bash("make", true), bash("make", true), bash("lint", true)], fresh());
        let (_, again) = feed(vec![bash("make", true), bash("make", true), bash("make", true)], c);
        assert!(matches!(again.last().unwrap(), Verdict::Correct { .. }));
    }
}
