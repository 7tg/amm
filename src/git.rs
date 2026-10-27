use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::io::Read;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct GitResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Output cap per stream; a worker could plant a huge file, and the daemon must not buffer it all.
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

/// Runs git in `cwd` with repo-controlled code paths neutralised (replace refs, fsmonitor, hooks) that a worker
/// could plant. Blocking; async callers use `spawn_blocking`. A spawn failure is code 1 with the error as stderr.
/// Errs if stdout or stderr exceeds 16 MiB (the child is killed).
pub fn git(cwd: &Path, args: &[&str]) -> Result<GitResult, String> {
    git_capped(cwd, args, MAX_OUTPUT)
}

fn read_capped<R: Read>(r: R, max: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    r.take(max as u64 + 1).read_to_end(&mut buf)?;
    let over = buf.len() > max;
    Ok((buf, over))
}

fn git_capped(cwd: &Path, args: &[&str], max: usize) -> Result<GitResult, String> {
    let spawned = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return Ok(GitResult { code: 1, stdout: String::new(), stderr: e.to_string() }),
    };
    let err_pipe = child.stderr.take().ok_or("no stderr pipe")?;
    let out_pipe = child.stdout.take().ok_or("no stdout pipe")?;
    let err_thread = std::thread::spawn(move || read_capped(err_pipe, max));
    let out = read_capped(out_pipe, max);
    if matches!(out, Ok((_, true))) {
        let _ = child.kill();
    }
    // Reading stdout to EOF (or killing) lets stderr finish, then the child can be reaped.
    let err = err_thread.join().map_err(|_| "git stderr reader panicked".to_string())?;
    let (stdout, out_over) = out.map_err(|e| e.to_string())?;
    let (stderr, err_over) = err.map_err(|e| e.to_string())?;
    if out_over || err_over {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("git {} output exceeds {max} bytes", args.first().copied().unwrap_or("")));
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    Ok(GitResult {
        code: status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
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
    let r = git(cwd, args)?;
    if r.code != 0 {
        return Err(format!("git {} failed: {}", args.first().copied().unwrap_or(""), r.stderr.trim()));
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
    let b = git(&toplevel, &["symbolic-ref", "--short", "-q", "HEAD"])?;
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
        git(dir, &["rev-parse", "HEAD"]).unwrap().stdout.trim().to_string()
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
    fn git_ignores_planted_fsmonitor_hooks_path_and_replace_refs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo();
        let marker = tmp_dir().join("ran");
        let script_dir = tmp_dir();
        let exe = |name: &str, body: &str| {
            let p = script_dir.join(name);
            fs::write(&p, format!("#!/bin/sh\ntouch {}\n{body}", marker.display())).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let fsmon = exe("fsmon", "");
        let hooks = script_dir.join("hooks");
        fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        git_in(&dir, &["config", "core.fsmonitor", fsmon.to_str().unwrap()]);
        git_in(&dir, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
        // a replace ref turning a.txt's blob into different content
        let orig = git(&dir, &["rev-parse", "HEAD:a.txt"]).unwrap().stdout.trim().to_string();
        let fake_file = script_dir.join("fake");
        fs::write(&fake_file, "fake\n").unwrap();
        let fake = git(&dir, &["hash-object", "-w", fake_file.to_str().unwrap()]).unwrap().stdout.trim().to_string();
        git_in(&dir, &["replace", &orig, &fake]);

        // precondition: unhardened git does fall for all three
        let plain = |a: &[&str]| {
            Command::new("git")
                .args(["-c", "user.email=t@t", "-c", "user.name=t"])
                .args(a)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        assert_eq!(String::from_utf8_lossy(&plain(&["show", "HEAD:a.txt"]).stdout), "fake\n");
        plain(&["status"]);
        assert!(marker.exists(), "fsmonitor precondition");
        fs::remove_file(&marker).unwrap();
        fs::write(dir.join("b.txt"), "b\n").unwrap();
        plain(&["add", "b.txt"]);
        plain(&["commit", "-qm", "x"]);
        assert!(marker.exists(), "hook precondition");
        fs::remove_file(&marker).unwrap();

        // hardened: none of them takes effect
        assert_eq!(git(&dir, &["show", "HEAD:a.txt"]).unwrap().stdout, "a\n");
        git(&dir, &["status"]).unwrap();
        fs::write(dir.join("c.txt"), "c\n").unwrap();
        git(&dir, &["add", "c.txt"]).unwrap();
        let c = git(&dir, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "y"]).unwrap();
        assert_eq!(c.code, 0, "{}", c.stderr);
        assert!(!marker.exists(), "planted fsmonitor or hook ran");
    }

    #[test]
    fn git_errors_when_output_exceeds_the_cap() {
        let dir = repo();
        let big = git_capped(&dir, &["show", "HEAD:a.txt"], 1);
        assert!(big.unwrap_err().contains("exceeds"));
        let ok = git_capped(&dir, &["show", "HEAD:a.txt"], 2).unwrap();
        assert_eq!(ok.stdout, "a\n");
    }
}
