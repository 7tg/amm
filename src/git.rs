use std::collections::HashSet;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct GitResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Output cap per stream; a worker could plant a huge file, and the daemon must not buffer it all.
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

/// Default kill timeout for one git call.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs git in `cwd` with repo-controlled code paths neutralised (replace refs, grafts, fsmonitor, hooks) that a worker
/// could plant. Blocking; async callers use `spawn_blocking`. A spawn failure is code 1 with the error as stderr.
/// Errs if stdout or stderr exceeds 16 MiB, or if git runs longer than `GIT_TIMEOUT` (its process group is killed).
pub fn git(cwd: &Path, args: &[&str]) -> Result<GitResult, String> {
    git_timeout(cwd, args, GIT_TIMEOUT)
}

/// `git` with a per-call kill timeout.
pub fn git_timeout(cwd: &Path, args: &[&str], timeout: Duration) -> Result<GitResult, String> {
    run(cwd, args, &[], MAX_OUTPUT, timeout)
}

fn read_capped<R: Read>(r: R, max: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    r.take(max as u64 + 1).read_to_end(&mut buf)?;
    let over = buf.len() > max;
    Ok((buf, over))
}

#[cfg(test)]
fn git_capped(cwd: &Path, args: &[&str], max: usize) -> Result<GitResult, String> {
    run(cwd, args, &[], max, GIT_TIMEOUT)
}

fn killpg(pgid: u32) {
    // SAFETY: plain syscall; the group leader is not reaped yet, so its pid (the pgid) cannot have been reused.
    unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
}

fn run(cwd: &Path, args: &[&str], env: &[(String, String)], max: usize, timeout: Duration) -> Result<GitResult, String> {
    let spawned = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        // Grafts rewrite parents, so they could make any commit look like it descends from a base. The path cannot
        // exist (/dev/null is no directory), and /dev/null itself would make git print a deprecation hint.
        .env("GIT_GRAFT_FILE", "/dev/null/amm-no-grafts")
        .envs(env.iter().map(|(k, v)| (k, v)))
        // Own process group, so a timeout also kills filters or helpers git started.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return Ok(GitResult { code: 1, stdout: String::new(), stderr: e.to_string() }),
    };
    let pid = child.id();
    let timed_out = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = {
        let timed_out = timed_out.clone();
        std::thread::spawn(move || {
            if done_rx.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
                timed_out.store(true, Ordering::SeqCst);
                killpg(pid);
            }
        })
    };
    let err_pipe = child.stderr.take().ok_or("no stderr pipe")?;
    let out_pipe = child.stdout.take().ok_or("no stdout pipe")?;
    let err_thread = std::thread::spawn(move || read_capped(err_pipe, max));
    let out = read_capped(out_pipe, max);
    if matches!(out, Ok((_, true))) {
        killpg(pid);
    }
    // Reading stdout to EOF (or killing) lets stderr finish, then the child can be reaped.
    let err = err_thread.join().map_err(|_| "git stderr reader panicked".to_string());
    // Stop the watchdog before reaping, so it can never signal a reused pid.
    drop(done_tx);
    let _ = watchdog.join();
    let name = args.first().copied().unwrap_or("");
    if timed_out.load(Ordering::SeqCst) {
        let _ = child.wait();
        return Err(format!("git {name} timed out after {} ms", timeout.as_millis()));
    }
    let (stdout, out_over) = out.map_err(|e| e.to_string())?;
    let (stderr, err_over) = err?.map_err(|e| e.to_string())?;
    if out_over || err_over {
        killpg(pid);
        let _ = child.wait();
        return Err(format!("git {name} output exceeds {max} bytes"));
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

/// Where `cwd` sits: its worktree root, the shared git dir, its branch (None if detached) and HEAD. Errors if git
/// fails or the repository is shallow.
pub fn worktree_info(cwd: &Path) -> Result<WorktreeInfo, String> {
    let toplevel = real(Path::new(&must(cwd, &["rev-parse", "--show-toplevel"])?))?;
    let common = must(&toplevel, &["rev-parse", "--git-common-dir"])?;
    let common_dir = real(&toplevel.join(common))?;
    // A shallow file cuts history, so ancestry and merge bases could not be trusted: fail closed.
    if common_dir.join("shallow").exists() {
        return Err(format!("{} is a shallow repository; amm needs its full history", toplevel.display()));
    }
    let b = git(&toplevel, &["symbolic-ref", "--short", "-q", "HEAD"])?;
    let head = must(&toplevel, &["rev-parse", "HEAD"])?;
    let branch = if b.code == 0 && !b.stdout.trim().is_empty() { Some(b.stdout.trim().to_string()) } else { None };
    Ok(WorktreeInfo { toplevel, common_dir, branch, head })
}

/// Env that turns every configured filter driver off (`clean`, `process` empty, `required` false), so hashing the
/// working tree runs no repo-planted command. Attributes can come from the tree, .git/info/attributes or config, so
/// the drivers themselves are disabled; env config outranks every config file and takes any subsection name.
fn no_filters(root: &Path) -> Result<Vec<(String, String)>, String> {
    let r = git(root, &["config", "--name-only", "--get-regexp", r"^filter\."])?;
    if r.code > 1 {
        return Err(format!("git config failed: {}", r.stderr.trim()));
    }
    let mut drivers: Vec<&str> =
        r.stdout.lines().filter_map(|l| l.strip_prefix("filter.")?.rsplit_once('.').map(|(d, _)| d)).collect();
    drivers.sort();
    drivers.dedup();
    let pairs: Vec<(String, &str)> = drivers
        .iter()
        .flat_map(|d| [(format!("filter.{d}.clean"), ""), (format!("filter.{d}.process"), ""), (format!("filter.{d}.required"), "false")])
        .collect();
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), pairs.len().to_string()), ("GIT_ATTR_NOSYSTEM".into(), "1".into())];
    for (i, (k, v)) in pairs.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), k));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), v.to_string()));
    }
    Ok(env)
}

/// Files that differ from `base_sha`, relative to the worktree root `root`: changes committed in `base_sha..sha`,
/// staged and unstaged changes against HEAD, and untracked files. `sha` is the exact commit being gated, not a branch
/// name, so a branch that moves afterwards cannot change what was diffed. Uses `root` as is (no repo discovery), so a
/// worker cannot redirect it, and runs no filter, textconv or external diff. Renames are a deletion plus an
/// addition. Errors if git fails or times out.
pub fn changed_files(root: &Path, base_sha: &str, sha: &str) -> Result<Vec<String>, String> {
    let env = no_filters(root)?;
    let diff = ["diff", "--name-only", "-z", "--no-renames", "--no-textconv", "--no-ext-diff"];
    let lists = [
        raw_env(root, &[&diff[..], &[base_sha, sha, "--"]].concat(), &env)?,
        raw_env(root, &[&diff[..], &["HEAD", "--"]].concat(), &env)?,
        raw_env(root, &["ls-files", "--others", "--exclude-standard", "--full-name", "-z"], &env)?,
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

/// Name and email amm commits under.
pub const AMM_IDENT: (&str, &str) = ("amm", "amm@localhost");

/// Stages every change in the worktree `root` (`git add -A`) and commits it as `amm <amm@localhost>`, with no hook,
/// signing, filter or automatic gc. Returns the new HEAD; on a clean tree it returns HEAD and makes no commit.
/// Blocking. Errors if any git call fails or times out.
pub fn commit_all(root: &Path, message: &str) -> Result<String, String> {
    let mut env = no_filters(root)?;
    let (name, email) = AMM_IDENT;
    for who in ["AUTHOR", "COMMITTER"] {
        env.push((format!("GIT_{who}_NAME"), name.into()));
        env.push((format!("GIT_{who}_EMAIL"), email.into()));
    }
    raw_env(root, &["add", "-A", "--", "."], &env)?;
    let staged = run(root, &["diff", "--cached", "--quiet", "--no-ext-diff", "--no-textconv"], &env, MAX_OUTPUT, GIT_TIMEOUT)?;
    if staged.code > 1 {
        return Err(format!("git diff failed: {}", staged.stderr.trim()));
    }
    if staged.code == 1 {
        let cfg = ["-c", "commit.gpgsign=false", "-c", "gc.auto=0", "-c", "maintenance.auto=false"];
        let ident = [format!("user.name={name}"), format!("user.email={email}")];
        let args: Vec<&str> = cfg
            .iter()
            .copied()
            .chain(ident.iter().flat_map(|i| ["-c", i.as_str()]))
            .chain(["commit", "-q", "--no-verify", "--no-gpg-sign", "-m", message])
            .collect();
        raw_env(root, &args, &env)?;
    }
    Ok(raw_env(root, &["rev-parse", "--verify", "HEAD^{commit}"], &env)?.trim().to_string())
}

/// Files that differ between the commits `base_sha` and `sha` (trees only, no working tree). Errors if git fails.
pub fn committed_files(root: &Path, base_sha: &str, sha: &str) -> Result<Vec<String>, String> {
    let out = raw(root, &["diff", "--name-only", "-z", "--no-renames", "--no-textconv", "--no-ext-diff", base_sha, sha, "--"])?;
    Ok(out.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect())
}

/// Whether commit `a` is an ancestor of (or equal to) commit `b`. Errors if git cannot tell.
pub fn is_ancestor(root: &Path, a: &str, b: &str) -> Result<bool, String> {
    let r = git(root, &["merge-base", "--is-ancestor", "--end-of-options", a, b])?;
    match r.code {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(format!("git merge-base failed: {}", r.stderr.trim())),
    }
}

fn raw_env(cwd: &Path, args: &[&str], env: &[(String, String)]) -> Result<String, String> {
    let r = run(cwd, args, env, MAX_OUTPUT, GIT_TIMEOUT)?;
    if r.code != 0 {
        return Err(format!("git {} failed: {}", args.first().copied().unwrap_or(""), r.stderr.trim()));
    }
    Ok(r.stdout)
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

    /// Plants a filter driver `x` whose clean and process commands touch `marker`, applied to every file through
    /// both an in-tree .gitattributes and .git/info/attributes, then dirties a.txt's stat so git must hash it.
    fn plant_filter(dir: &Path, marker: &Path, clean: &str) {
        fs::write(dir.join(".gitattributes"), "* filter=x\n").unwrap();
        fs::create_dir_all(dir.join(".git/info")).unwrap();
        fs::write(dir.join(".git/info/attributes"), "* filter=x\n").unwrap();
        git_in(dir, &["config", "filter.x.clean", clean]);
        git_in(dir, &["config", "filter.x.process", &format!("sh -c 'touch {}'", marker.display())]);
        git_in(dir, &["config", "filter.x.required", "true"]);
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
    }

    #[test]
    fn changed_files_never_runs_planted_clean_or_process_filters() {
        let dir = repo();
        let base = head(&dir);
        let marker = tmp_dir().join("ran");
        plant_filter(&dir, &marker, &format!("touch {}; cat", marker.display()));
        // precondition: an unhardened working-tree diff runs the filter
        Command::new("git").args(["diff", "--name-only", "HEAD"]).current_dir(&dir).output().unwrap();
        assert!(marker.exists(), "filter precondition");
        fs::remove_file(&marker).unwrap();
        fs::write(dir.join("a.txt"), "changed again\n").unwrap();
        let files = sorted(changed_files(&dir, &base, &base).unwrap());
        assert_eq!(files, [".gitattributes", "a.txt"]);
        assert!(!marker.exists(), "planted filter ran");
    }

    #[test]
    fn git_times_out_and_kills_a_hung_command() {
        let dir = repo();
        plant_filter(&dir, &tmp_dir().join("ran"), "sleep 30");
        git_in(&dir, &["config", "filter.x.process", "sleep 30"]);
        let t = std::time::Instant::now();
        let e = git_timeout(&dir, &["diff", "--name-only", "HEAD"], std::time::Duration::from_millis(300)).unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }

    fn show(dir: &Path, fmt: &str) -> String {
        git(dir, &["log", "-1", &format!("--format={fmt}")]).unwrap().stdout.trim().to_string()
    }

    #[test]
    fn commit_all_commits_every_change_as_amm_and_returns_the_new_head() {
        let dir = repo();
        let base = head(&dir);
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        fs::write(dir.join("new.txt"), "n\n").unwrap();
        let sha = commit_all(&dir, "amm(b1): t").unwrap();
        assert_eq!(sha, head(&dir));
        assert_ne!(sha, base);
        assert_eq!(show(&dir, "%an <%ae>|%cn <%ce>|%s|%P"), format!("amm <amm@localhost>|amm <amm@localhost>|amm(b1): t|{base}"));
        let files = git(&dir, &["show", "--name-only", "--format=", &sha]).unwrap().stdout;
        assert_eq!(sorted(files.lines().map(str::to_string).collect()), ["a.txt", "new.txt"]);
        assert_eq!(git(&dir, &["status", "--porcelain"]).unwrap().stdout, "");
    }

    #[test]
    fn commit_all_on_a_clean_tree_returns_head_without_a_commit() {
        let dir = repo();
        let base = head(&dir);
        assert_eq!(commit_all(&dir, "m").unwrap(), base);
        assert_eq!(head(&dir), base);
    }

    #[test]
    fn commit_all_runs_no_planted_hook_signer_or_filter() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo();
        let marker = tmp_dir().join("ran");
        let hooks = tmp_dir();
        for name in ["pre-commit", "commit-msg", "post-commit", "prepare-commit-msg"] {
            let p = hooks.join(name);
            fs::write(&p, format!("#!/bin/sh\ntouch {}\nexit 1\n", marker.display())).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::copy(hooks.join("pre-commit"), dir.join(".git/hooks/pre-commit")).unwrap();
        git_in(&dir, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
        git_in(&dir, &["config", "commit.gpgsign", "true"]);
        git_in(&dir, &["config", "gpg.program", hooks.join("pre-commit").to_str().unwrap()]);
        plant_filter(&dir, &marker, &format!("touch {}; cat", marker.display()));
        let sha = commit_all(&dir, "m").unwrap();
        assert_eq!(sha, head(&dir));
        assert_eq!(show(&dir, "%G?"), "N");
        assert!(!marker.exists(), "a planted hook, signer or filter ran");
    }

    #[test]
    fn commit_all_errors_when_git_cannot_commit() {
        let dir = repo();
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        fs::write(dir.join(".git/index.lock"), "").unwrap();
        assert!(commit_all(&dir, "m").is_err());
    }

    #[test]
    fn git_ignores_a_planted_grafts_file() {
        let dir = repo();
        let root = head(&dir);
        fs::write(dir.join("a.txt"), "1\n").unwrap();
        git_in(&dir, &["commit", "-qam", "one"]);
        let one = head(&dir);
        // An unrelated root commit, grafted to look like it descends from `one`.
        git_in(&dir, &["checkout", "-q", "--orphan", "other"]);
        git_in(&dir, &["commit", "-qm", "other"]);
        let other = head(&dir);
        fs::write(dir.join(".git/info/grafts"), format!("{other} {one}\n")).unwrap();
        let plain = Command::new("git").args(["merge-base", "--is-ancestor", &root, &other]).current_dir(&dir).status().unwrap();
        assert!(plain.success(), "graft precondition");
        assert!(!is_ancestor(&dir, &root, &other).unwrap());
    }

    #[test]
    fn worktree_info_refuses_a_shallow_repository() {
        let dir = repo();
        fs::write(dir.join(".git/shallow"), format!("{}\n", head(&dir))).unwrap();
        assert!(worktree_info(&dir).unwrap_err().contains("shallow"));
    }

    #[test]
    fn changed_files_diffs_the_given_sha_not_the_branch_head() {
        let dir = repo();
        let base = head(&dir);
        fs::write(dir.join("a.txt"), "1\n").unwrap();
        git_in(&dir, &["commit", "-qam", "one"]);
        let one = head(&dir);
        fs::write(dir.join("z.txt"), "z\n").unwrap();
        git_in(&dir, &["add", "z.txt"]);
        git_in(&dir, &["commit", "-qm", "two"]);
        assert_eq!(changed_files(&dir, &base, &one).unwrap(), ["a.txt"]);
    }
}
