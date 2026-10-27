//! Shared integration-test helpers. Mirrored in src/testutil.rs for unit tests.
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fresh temp directory that outlives the test process run (not auto-deleted).
pub fn tmp_dir() -> PathBuf {
    tempfile::Builder::new().prefix("amm-test-").tempdir().expect("tmp dir").keep()
}

/// A git repo with one commit, standing in for a worker's worktree. Returns the realpath.
pub fn init_repo() -> PathBuf {
    init_repo_with(&[("README.md", "hi\n")])
}

pub fn init_repo_with(files: &[(&str, &str)]) -> PathBuf {
    let dir = tmp_dir();
    let git = |args: &[&str]| {
        let o = Command::new("git").args(args).current_dir(&dir).output().expect("git");
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join(".gitignore"), ".claude/\n").unwrap();
    for (name, content) in files {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    git(&["add", "."]);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    dir.canonicalize().unwrap()
}

/// A real git worktree of `repo` on branch worktree-agent-<name>, like Claude Code creates for subagents.
pub fn add_worktree(repo: &Path, name: &str) -> PathBuf {
    let dir = repo.join(".claude").join("worktrees").join(format!("agent-{name}"));
    let o = Command::new("git")
        .args(["worktree", "add", "-q", "-b", &format!("worktree-agent-{name}")])
        .arg(&dir)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    dir.canonicalize().unwrap()
}
