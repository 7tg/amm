use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct GitResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs git in `cwd` with repo-controlled code paths neutralised (replace refs, fsmonitor, hooks) that a worker
/// could plant. Blocking; async callers use `spawn_blocking`. A spawn failure is code 1 with the error as stderr.
pub fn git(cwd: &Path, args: &[&str]) -> GitResult {
    let out = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output();
    match out {
        Ok(o) => GitResult {
            code: o.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => GitResult { code: 1, stdout: String::new(), stderr: e.to_string() },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub toplevel: PathBuf,
    pub common_dir: PathBuf,
    /// None when HEAD is detached.
    pub branch: Option<String>,
    pub head: String,
}

fn raw(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let r = git(cwd, args);
    if r.code != 0 {
        return Err(format!("git {} failed: {}", args[0], r.stderr.trim()));
    }
    Ok(r.stdout)
}

fn must(cwd: &Path, args: &[&str]) -> Result<String, String> {
    Ok(raw(cwd, args)?.trim().to_string())
}

fn real(p: &Path) -> Result<PathBuf, String> {
    p.canonicalize().map_err(|e| format!("{}: {e}", p.display()))
}

/// Where `cwd` sits: its worktree root, the shared git dir, its branch (None if detached) and HEAD. Errors if git fails.
pub fn worktree_info(cwd: &Path) -> Result<WorktreeInfo, String> {
    let toplevel = real(Path::new(&must(cwd, &["rev-parse", "--show-toplevel"])?))?;
    let common = must(&toplevel, &["rev-parse", "--git-common-dir"])?;
    let common_dir = real(&toplevel.join(common))?;
    let b = git(&toplevel, &["symbolic-ref", "--short", "-q", "HEAD"]);
    let head = must(&toplevel, &["rev-parse", "HEAD"])?;
    let branch = if b.code == 0 && !b.stdout.trim().is_empty() { Some(b.stdout.trim().to_string()) } else { None };
    Ok(WorktreeInfo { toplevel, common_dir, branch, head })
}

/// Files that differ from `base_sha`, relative to the worktree root `root`: changes committed on `branch`, staged and
/// unstaged changes against HEAD, and untracked files. Uses `root` as is (no repo discovery), so a worker cannot
/// redirect it. Renames are a deletion plus an addition. Errors if git fails.
pub fn changed_files(root: &Path, base_sha: &str, branch: &str) -> Result<Vec<String>, String> {
    let branch_ref = format!("refs/heads/{branch}");
    let lists = [
        raw(root, &["diff", "--name-only", "-z", "--no-renames", base_sha, &branch_ref, "--"])?,
        raw(root, &["diff", "--name-only", "-z", "--no-renames", "HEAD", "--"])?,
        raw(root, &["ls-files", "--others", "--exclude-standard", "--full-name", "-z"])?,
    ];
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for name in lists.iter().flat_map(|l| l.split('\0')).filter(|s| !s.is_empty()) {
        if seen.insert(name) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{add_worktree, init_repo, tmp_dir};
    use std::fs;

    fn repo() -> PathBuf {
        let dir = tmp_dir();
        git_in(&dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-qm", "init"]);
        dir
    }

    fn git_in(dir: &Path, a: &[&str]) {
        let o = Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(a)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {a:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    fn head(dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"]).stdout.trim().to_string()
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn changed_files_lists_modified_committed_and_untracked_files_since_base() {
        let dir = repo();
        let base = head(&dir);
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        fs::create_dir(dir.join("src")).unwrap();
        fs::write(dir.join("src").join("new.ts"), "x\n").unwrap();
        assert_eq!(sorted(changed_files(&dir, &base, "main").unwrap()), ["a.txt", "src/new.ts"]);
    }

    #[test]
    fn changed_files_sees_committed_changes_deletions_and_renames_both_paths() {
        let dir = repo();
        fs::write(dir.join("old.txt"), "o\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-qm", "more"]);
        let base = head(&dir);
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        git_in(&dir, &["commit", "-qam", "edit"]);
        git_in(&dir, &["mv", "old.txt", "new.txt"]);
        assert_eq!(sorted(changed_files(&dir, &base, "main").unwrap()), ["a.txt", "new.txt", "old.txt"]);
        fs::remove_file(dir.join("a.txt")).unwrap();
        assert!(changed_files(&dir, &base, "main").unwrap().contains(&"a.txt".to_string()));
    }

    #[test]
    fn changed_files_returns_raw_unicode_names_with_root_relative_paths() {
        let dir = repo();
        let base = head(&dir);
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("ünï\tq\".txt"), "x\n").unwrap();
        assert_eq!(changed_files(&dir, &base, "main").unwrap(), ["sub/ünï\tq\".txt"]);
    }

    #[test]
    fn changed_files_throws_on_a_bad_base_sha() {
        let dir = repo();
        let e = changed_files(&dir, "deadbeef", "main").unwrap_err().to_lowercase();
        assert!(e.contains("deadbeef") || e.contains("unknown") || e.contains("bad"), "{e}");
    }

    #[test]
    fn changed_files_still_lists_a_committed_out_of_scope_change_after_a_working_tree_revert() {
        let dir = repo();
        let base = head(&dir);
        fs::write(dir.join("a.txt"), "evil\n").unwrap();
        git_in(&dir, &["commit", "-qam", "evil"]);
        git_in(&dir, &["revert", "--no-commit", "HEAD"]);
        assert_eq!(changed_files(&dir, &base, "main").unwrap(), ["a.txt"]);
    }

    #[test]
    fn worktree_info_describes_a_worktree() {
        let main = init_repo();
        let wt = add_worktree(&main, "q");
        let info = worktree_info(&wt).unwrap();
        assert_eq!(info.toplevel, wt);
        assert_eq!(info.common_dir, main.join(".git").canonicalize().unwrap());
        assert_eq!(info.branch.as_deref(), Some("worktree-agent-q"));
        assert!(info.head.len() == 40 && info.head.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(worktree_info(&main).unwrap().toplevel, main.canonicalize().unwrap());
    }

    #[test]
    fn worktree_info_reports_detached_head_and_errors_outside_a_repo() {
        let main = init_repo();
        git_in(&main, &["checkout", "-q", "--detach"]);
        assert_eq!(worktree_info(&main).unwrap().branch, None);
        assert!(worktree_info(&tmp_dir()).is_err());
    }

    #[test]
    fn git_neutralises_repo_planted_hooks_path_and_replace_refs() {
        let dir = repo();
        let r = git(&dir, &["config", "--get", "core.hooksPath"]);
        // -c wins over repo config, so the effective value is /dev/null
        assert_eq!(r.stdout.trim(), "/dev/null");
        assert_eq!(git(&dir, &["config", "--get", "core.fsmonitor"]).stdout.trim(), "false");
    }
}
