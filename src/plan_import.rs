use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq)]
pub struct PlanTask {
    pub title: String,
    pub body: String,
    pub files: Vec<String>,
}

// `[^\n\r  ]` stands in for JS `.`, which stops at any line terminator.
static TASK_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(#{2,4})\s+Task\s+\d+\s*[:.\-–—]?\s*([^\n\r\u{2028}\u{2029}]*)$").unwrap());
static ANY_HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#{1,6})\s").unwrap());
static FILE_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*[-*]\s*(?:Create|Modify|Test|Update|Delete):\s*([^\n\r\u{2028}\u{2029}]*)$").unwrap()
});
static FENCE_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(`{3,}|~{3,})").unwrap());
static FENCE_CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(`{3,}|~{3,})\s*$").unwrap());
static TICKED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
static LINE_RANGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":\d+(-\d+)?$").unwrap());

/// Splits a plan written as "### Task N: Title" sections (superpowers format) into tasks.
pub fn parse_plan(md: &str) -> Vec<PlanTask> {
    let mut tasks: Vec<PlanTask> = Vec::new();
    let mut cur = false; // whether the last task in `tasks` is the current one
    let mut level = 0usize;
    let mut fence: Option<String> = None;
    for raw in md.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match &fence {
            Some(f) => {
                if let Some(close) = FENCE_CLOSE.captures(line) {
                    let c = &close[1];
                    if c.as_bytes()[0] == f.as_bytes()[0] && c.len() >= f.len() {
                        fence = None;
                    }
                }
            }
            None => {
                if let Some(open) = FENCE_OPEN.captures(line) {
                    fence = Some(open[1].to_string());
                }
            }
        }
        let in_fence = fence.is_some();
        if !in_fence && !FENCE_OPEN.is_match(line) {
            if let Some(h) = TASK_HEADING.captures(line) {
                level = h[1].len();
                tasks.push(PlanTask { title: h[2].trim().to_string(), body: String::new(), files: vec![] });
                cur = true;
                continue;
            }
            if let Some(a) = ANY_HEADING.captures(line)
                && a[1].len() <= level
            {
                cur = false;
            }
        }
        if !cur {
            continue;
        }
        let task = tasks.last_mut().unwrap();
        task.body.push_str(line);
        task.body.push('\n');
        if !in_fence
            && let Some(prefix) = FILE_PREFIX.captures(line)
        {
            for m in TICKED.captures_iter(&prefix[1]) {
                task.files.push(LINE_RANGE.replace(&m[1], "").into_owned());
            }
        }
    }
    for t in &mut tasks {
        t.body = t.body.trim().to_string();
    }
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"# Feature Plan

intro text

### Task 1: Add parser

**Files:**
- Create: `src/parser.ts`
- Test: `test/parser.test.ts`

- [ ] Step 1
```ts
### Task 99: not a heading inside a fence
- Create: `not/a/file.ts`
```

### Task 2 - Wire CLI
- Modify: `src/cli.ts:10-20`
body
"#;

    fn titles(t: &[PlanTask]) -> Vec<&str> {
        t.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn splits_tasks_by_heading_ignoring_fenced_code() {
        assert_eq!(titles(&parse_plan(PLAN)), vec!["Add parser", "Wire CLI"]);
    }

    #[test]
    fn collects_file_paths_and_strips_line_ranges() {
        let tasks = parse_plan(PLAN);
        assert_eq!(tasks[0].files, vec!["src/parser.ts", "test/parser.test.ts"]);
        assert_eq!(tasks[1].files, vec!["src/cli.ts"]);
        assert!(tasks[0].body.contains("Task 99: not a heading"));
    }

    #[test]
    fn returns_no_tasks_for_a_document_without_task_headings() {
        assert!(parse_plan("# Title\n\njust text").is_empty());
    }

    #[test]
    fn collects_every_backticked_path_on_a_file_line() {
        let t = parse_plan("### Task 1: X\n- Modify: `a.ts:1-5`, `b.ts`\n");
        assert_eq!(t[0].files, vec!["a.ts", "b.ts"]);
    }

    #[test]
    fn fences_close_only_on_the_same_marker_at_least_as_long() {
        let md = "### Task 1: X\n````md\n```\n### Task 9: inner\n```\n- Create: `no.ts`\n````\n- Create: `yes.ts`\n\n```\n~~~\n### Task 8: still fenced\n~~~\n```\n";
        let tasks = parse_plan(md);
        assert_eq!(titles(&tasks), vec!["X"]);
        assert_eq!(tasks[0].files, vec!["yes.ts"]);
    }

    #[test]
    fn handles_crlf_and_em_dash_separators() {
        let tasks = parse_plan("### Task 1 — Dash\r\n- Create: `a.ts`\r\n");
        assert_eq!(titles(&tasks), vec!["Dash"]);
        assert_eq!(tasks[0].files, vec!["a.ts"]);
    }

    #[test]
    fn a_heading_of_the_same_or_higher_level_ends_the_task_body() {
        let tasks = parse_plan("### Task 1: X\nin\n#### Sub\nstill in\n## Appendix\n- Create: `out.ts`\nout\n");
        assert_eq!(tasks[0].body, "in\n#### Sub\nstill in");
        assert!(tasks[0].files.is_empty());
    }
}
