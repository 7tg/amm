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

// Launcher tests: a copy of the plugin root with only bin/amm and plugin.json, run under `sh`
// and, when it is installed, under `dash`.

fn target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported {other:?}"),
    }
}

/// Where the launcher expects a cargo build when rustc cannot say.
fn host_guess() -> String {
    target().replace("-musl", "-gnu")
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|p| p.join(tool)).find(|p| p.is_file())
}

fn shells() -> Vec<PathBuf> {
    let mut v = vec![which("sh").expect("sh")];
    v.extend(which("dash"));
    v
}

fn plugin_copy() -> PathBuf {
    let dir = common::tmp_dir();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    std::fs::copy(root().join("bin/amm"), dir.join("bin/amm")).unwrap();
    std::fs::copy(root().join(".claude-plugin/plugin.json"), dir.join(".claude-plugin/plugin.json")).unwrap();
    dir
}

fn write_exe(path: &Path, script: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A PATH holding only the tools the launcher needs, so neither gh, cargo nor rustc can be found.
fn bare_path() -> PathBuf {
    let dir = common::tmp_dir();
    for tool in ["sh", "uname", "sed", "head", "mkdir", "rm", "dirname", "cat", "sleep", "chmod", "cp", "mv", "tar"] {
        std::os::unix::fs::symlink(which(tool).unwrap_or_else(|| panic!("{tool} not on PATH")), dir.join(tool)).unwrap();
    }
    dir
}

fn launch_with(shell: &Path, plugin: &Path, path: &Path, args: &[&str]) -> Output {
    Command::new(shell).arg(plugin.join("bin/amm")).args(args).env("PATH", path).stdin(Stdio::null()).output().unwrap()
}

fn launch(shell: &Path, plugin: &Path, args: &[&str]) -> Output {
    launch_with(shell, plugin, &bare_path(), args)
}

fn fake_script(version: &str) -> String {
    format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'amm {version}'; else echo \"ran: $*\"; fi\n")
}

/// Puts a stand-in binary under `bin/<triple>/` that reports `version` and echoes its arguments.
fn fake_binary(plugin: &Path, triple: &str, version: &str) {
    write_exe(&plugin.join("bin").join(triple).join("amm"), &fake_script(version));
}

fn hold_lock(plugin: &Path, pid: u32) {
    std::fs::create_dir(plugin.join("bin/.lock")).unwrap();
    std::fs::write(plugin.join("bin/.lock/pid"), pid.to_string()).unwrap();
}

#[test]
fn launcher_hook_without_binary_gh_or_cargo_exits_zero() {
    for sh in shells() {
        let plugin = plugin_copy();
        let r = launch(&sh, &plugin, &["hook", "PreToolUse"]);
        assert_eq!(r.status.code(), Some(0), "{sh:?} {}", String::from_utf8_lossy(&r.stderr));
        assert!(r.stdout.is_empty());
        assert!(String::from_utf8_lossy(&r.stderr).contains("amm:"));
        assert!(!plugin.join("bin/.lock").exists(), "{sh:?}: a hook never takes the lock");
        // Anything but a hook does fail.
        assert_eq!(launch(&sh, &plugin, &["shim"]).status.code(), Some(1), "{sh:?}");
        assert!(!plugin.join("bin/.lock").exists(), "{sh:?}: lock released");
    }
}

#[test]
fn launcher_execs_the_binary_at_the_right_version() {
    for sh in shells() {
        for triple in [target().to_string(), host_guess()] {
            let plugin = plugin_copy();
            fake_binary(&plugin, &triple, env!("CARGO_PKG_VERSION"));
            let r = launch(&sh, &plugin, &["hook", "PreToolUse"]);
            assert_eq!(r.status.code(), Some(0));
            assert_eq!(String::from_utf8_lossy(&r.stdout), "ran: hook PreToolUse\n", "{sh:?} {triple}");
        }
    }
}

#[test]
fn launcher_refuses_a_stale_binary_it_cannot_replace() {
    for sh in shells() {
        let plugin = plugin_copy();
        fake_binary(&plugin, target(), "0.0.0-old");
        let r = launch(&sh, &plugin, &["hook", "PreToolUse"]);
        assert_eq!((r.status.code(), r.stdout.is_empty()), (Some(0), true), "{sh:?}");
        assert_eq!(launch(&sh, &plugin, &["shim"]).status.code(), Some(1), "{sh:?}");
    }
}

#[test]
fn launcher_hook_never_waits_on_the_install_lock() {
    for sh in shells() {
        let plugin = plugin_copy();
        hold_lock(&plugin, std::process::id());
        let t = Instant::now();
        let r = launch(&sh, &plugin, &["hook", "PreToolUse"]);
        assert_eq!((r.status.code(), r.stdout.is_empty()), (Some(0), true), "{sh:?}");
        assert!(t.elapsed() < Duration::from_millis(500), "{sh:?} {:?}", t.elapsed());
        assert!(plugin.join("bin/.lock").exists(), "someone else's lock is left alone");
    }
}

#[test]
fn launcher_install_waits_for_a_live_lock() {
    for sh in shells() {
        let plugin = plugin_copy();
        hold_lock(&plugin, std::process::id());
        let lock = plugin.join("bin/.lock");
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            std::fs::remove_dir_all(lock).unwrap();
        });
        let t = Instant::now();
        let r = launch(&sh, &plugin, &["--version"]);
        release.join().unwrap();
        assert_eq!(r.status.code(), Some(1), "{sh:?}: nothing to install with");
        assert!(t.elapsed() >= Duration::from_millis(450), "{sh:?} {:?}", t.elapsed());
    }
}

#[test]
fn launcher_clears_a_lock_left_by_a_dead_process() {
    for sh in shells() {
        let plugin = plugin_copy();
        hold_lock(&plugin, (1 << 22) + 12345);
        let t = Instant::now();
        let r = launch(&sh, &plugin, &["--version"]);
        assert_eq!(r.status.code(), Some(1), "{sh:?}");
        assert!(t.elapsed() < Duration::from_secs(1), "{sh:?}");
        assert!(!plugin.join("bin/.lock").exists(), "{sh:?}");
    }
}

#[test]
fn launcher_cargo_fallback_installs_under_the_host_triple() {
    for sh in shells() {
        let plugin = plugin_copy();
        let path = bare_path();
        write_exe(&path.join("rustc"), "#!/bin/sh\necho 'rustc 1.99.0'\necho 'host: test-host-triple'\n");
        // Stands in for `cargo build --release --manifest-path <root>/Cargo.toml`.
        let cargo = format!(
            "#!/bin/sh\nfor a; do case \"$a\" in */Cargo.toml) r=$(dirname \"$a\");; esac; done\n\
             mkdir -p \"$r/target/release\"\ncat >\"$r/target/release/amm\" <<'X'\n{}X\nchmod 755 \"$r/target/release/amm\"\necho built\n",
            fake_script(env!("CARGO_PKG_VERSION"))
        );
        write_exe(&path.join("cargo"), &cargo);

        let r = launch_with(&sh, &plugin, &path, &["--version"]);
        assert_eq!(r.status.code(), Some(0), "{sh:?} {}", String::from_utf8_lossy(&r.stderr));
        assert_eq!(String::from_utf8_lossy(&r.stdout), format!("amm {}\n", env!("CARGO_PKG_VERSION")), "{sh:?}");
        assert!(String::from_utf8_lossy(&r.stderr).contains("built"), "cargo output goes to stderr");
        assert!(plugin.join("bin/test-host-triple/amm").is_file(), "{sh:?}");
        assert!(!plugin.join("bin").join(target()).join("amm").exists(), "{sh:?}");
        // A hook finds that build without rustc or cargo.
        let r = launch(&sh, &plugin, &["hook", "PreToolUse"]);
        assert_eq!(String::from_utf8_lossy(&r.stdout), "ran: hook PreToolUse\n", "{sh:?}");
    }
}
