//! Port of test/plugin.test.ts (adjusted for the bin/amm launcher), the version sync, and launcher tests.
mod common;

use regex::Regex;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(p: &str) -> String {
    std::fs::read_to_string(root().join(p)).unwrap_or_else(|e| panic!("{p}: {e}"))
}

fn json_file(p: &str) -> Value {
    serde_json::from_str(&read(p)).unwrap_or_else(|e| panic!("{p}: {e}"))
}

#[test]
fn manifest_mcp_and_hooks_are_valid_json_and_point_at_shipped_files() {
    assert_eq!(json_file(".claude-plugin/plugin.json")["name"], "amm");
    let mcp = &json_file(".mcp.json")["mcpServers"]["amm"];
    assert_eq!(mcp["command"], "${CLAUDE_PLUGIN_ROOT}/bin/amm");
    assert_eq!(mcp["args"], json!(["shim"]));
    let hooks = json_file("hooks/hooks.json")["hooks"].clone();
    let mut events: Vec<&String> = hooks.as_object().unwrap().keys().collect();
    events.sort();
    assert_eq!(events, ["PostToolUse", "PostToolUseFailure", "PreToolUse", "SubagentStart", "SubagentStop"]);
    for (event, entries) in hooks.as_object().unwrap() {
        assert_eq!(entries[0]["hooks"][0]["command"], format!("\"${{CLAUDE_PLUGIN_ROOT}}/bin/amm\" hook {event}"));
    }
    assert_eq!(hooks["SubagentStop"][0]["hooks"][0]["timeout"], 600);
    let mode = std::fs::metadata(root().join("bin/amm")).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "bin/amm must be executable");
}

#[test]
fn each_worker_agent_pins_its_model_runs_in_a_worktree_and_cannot_spawn_agents() {
    let isolation = Regex::new("(?m)^isolation: worktree$").unwrap();
    let no_agents = Regex::new("(?m)^disallowedTools: Agent$").unwrap();
    for (tier, model) in [("light", "haiku"), ("standard", "sonnet"), ("heavy", "opus")] {
        let md = read(&format!("agents/amm-worker-{tier}.md"));
        for pat in [format!("(?m)^name: amm-worker-{tier}$"), format!("(?m)^model: {model}$")] {
            assert!(Regex::new(&pat).unwrap().is_match(&md), "{tier}: {pat}");
        }
        assert!(isolation.is_match(&md), "{tier}");
        assert!(no_agents.is_match(&md), "{tier}");
    }
}

#[test]
fn skills_have_frontmatter_with_name_and_description() {
    for s in ["amm-delegate", "amm-supervise"] {
        let md = read(&format!("skills/{s}/SKILL.md"));
        assert!(Regex::new(&format!("(?m)^---\\nname: {s}\\ndescription: .+\\n---")).unwrap().is_match(&md), "{s}");
    }
}

#[test]
fn cargo_version_matches_plugin_version() {
    assert_eq!(json_file(".claude-plugin/plugin.json")["version"], env!("CARGO_PKG_VERSION"));
}

// Launcher tests: a copy of the plugin root with only bin/amm and plugin.json, run under `sh`.

fn target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported {other:?}"),
    }
}

fn plugin_copy() -> PathBuf {
    let dir = common::tmp_dir();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    std::fs::copy(root().join("bin/amm"), dir.join("bin/amm")).unwrap();
    std::fs::copy(root().join(".claude-plugin/plugin.json"), dir.join(".claude-plugin/plugin.json")).unwrap();
    dir
}

/// A PATH holding only the tools the launcher needs, so neither gh nor cargo can be found.
fn bare_path() -> PathBuf {
    let dir = common::tmp_dir();
    let path = std::env::var_os("PATH").unwrap();
    for tool in ["sh", "uname", "sed", "head", "mkdir", "rm", "dirname", "cat", "sleep", "chmod", "cp", "mv", "tar"] {
        let found = std::env::split_paths(&path).map(|p| p.join(tool)).find(|p| p.is_file());
        std::os::unix::fs::symlink(found.unwrap_or_else(|| panic!("{tool} not on PATH")), dir.join(tool)).unwrap();
    }
    dir
}

fn launch(plugin: &Path, args: &[&str]) -> Output {
    Command::new("sh")
        .arg(plugin.join("bin/amm"))
        .args(args)
        .env("PATH", bare_path())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Puts a stand-in binary that reports `version` and echoes its arguments.
fn fake_binary(plugin: &Path, version: &str) {
    let dir = plugin.join("bin").join(target());
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("amm");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'amm {version}'; else echo \"ran: $*\"; fi\n"
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn launcher_hook_without_binary_gh_or_cargo_exits_zero() {
    let plugin = plugin_copy();
    let r = launch(&plugin, &["hook", "PreToolUse"]);
    assert_eq!(r.status.code(), Some(0), "{}", String::from_utf8_lossy(&r.stderr));
    assert!(r.stdout.is_empty());
    assert!(String::from_utf8_lossy(&r.stderr).contains("amm:"));
    assert!(!plugin.join("bin/.lock").exists(), "lock released");
    // Anything but a hook does fail.
    assert_eq!(launch(&plugin, &["shim"]).status.code(), Some(1));
}

#[test]
fn launcher_execs_the_binary_at_the_right_version() {
    let plugin = plugin_copy();
    fake_binary(&plugin, env!("CARGO_PKG_VERSION"));
    let r = launch(&plugin, &["hook", "PreToolUse"]);
    assert_eq!(r.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&r.stdout), "ran: hook PreToolUse\n");
}

#[test]
fn launcher_refuses_a_stale_binary_it_cannot_replace() {
    let plugin = plugin_copy();
    fake_binary(&plugin, "0.0.0-old");
    let r = launch(&plugin, &["hook", "PreToolUse"]);
    assert_eq!((r.status.code(), r.stdout.is_empty()), (Some(0), true));
    assert_eq!(launch(&plugin, &["shim"]).status.code(), Some(1));
}

#[test]
fn launcher_hook_waits_at_most_2s_for_another_install() {
    let plugin = plugin_copy();
    std::fs::create_dir(plugin.join("bin/.lock")).unwrap();
    std::fs::write(plugin.join("bin/.lock/pid"), std::process::id().to_string()).unwrap();
    let t = Instant::now();
    let r = launch(&plugin, &["hook", "PreToolUse"]);
    let took = t.elapsed();
    assert_eq!(r.status.code(), Some(0));
    assert!(took >= Duration::from_millis(1500) && took < Duration::from_secs(4), "{took:?}");
    assert!(plugin.join("bin/.lock").exists(), "someone else's lock is left alone");
}

#[test]
fn launcher_clears_a_lock_left_by_a_dead_process() {
    let plugin = plugin_copy();
    fake_binary(&plugin, "0.0.0-old");
    std::fs::create_dir(plugin.join("bin/.lock")).unwrap();
    std::fs::write(plugin.join("bin/.lock/pid"), ((1 << 22) + 12345).to_string()).unwrap();
    let t = Instant::now();
    let r = launch(&plugin, &["hook", "PreToolUse"]);
    assert_eq!(r.status.code(), Some(0));
    assert!(t.elapsed() < Duration::from_secs(1));
    assert!(!plugin.join("bin/.lock").exists());
}
