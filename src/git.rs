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
    run_in(cwd, args, env, None, max, timeout)
}

/// `run`, writing `input` to git's stdin (else stdin is /dev/null).
fn run_in(
    cwd: &Path,
    args: &[&str],
    env: &[(String, String)],
    input: Option<Vec<u8>>,
    max: usize,
    timeout: Duration,
) -> Result<GitResult, String> {
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
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return Ok(GitResult { code: 1, stdout: String::new(), stderr: e.to_string() }),
    };
    // Fed from a thread so a full stdout pipe can never deadlock against a full stdin pipe.
    let writer = match (input, child.stdin.take()) {
        (Some(bytes), Some(mut stdin)) => Some(std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(&bytes);
        })),
        _ => None,
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
    if let Some(w) = writer {
        let _ = w.join();
    }
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
    config_env(root, &[], false)
}

/// Config names (`<section>.<sub>`) under `section`, deduplicated, from every config file git reads.
fn subsections(root: &Path, section: &str) -> Result<Vec<String>, String> {
    let r = git(root, &["config", "--name-only", "--get-regexp", &format!(r"^{section}\.")])?;
    if r.code > 1 {
        return Err(format!("git config failed: {}", r.stderr.trim()));
    }
    let prefix = format!("{section}.");
    let mut subs: Vec<String> =
        r.stdout.lines().filter_map(|l| l.strip_prefix(&prefix)?.rsplit_once('.').map(|(d, _)| d.to_string())).collect();
    subs.sort();
    subs.dedup();
    Ok(subs)
}

/// Env config: every filter driver off (and, with `checkout`, also `smudge` and every custom merge driver, which
/// checkouts and merges would run), followed by `extra`.
fn config_env(root: &Path, extra: &[(String, String)], checkout: bool) -> Result<Vec<(String, String)>, String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for d in subsections(root, "filter")? {
        pairs.push((format!("filter.{d}.clean"), String::new()));
        pairs.push((format!("filter.{d}.process"), String::new()));
        pairs.push((format!("filter.{d}.required"), "false".into()));
        if checkout {
            pairs.push((format!("filter.{d}.smudge"), String::new()));
        }
    }
    if checkout {
        for d in subsections(root, "merge")? {
            // `false` is a plain shell builtin: a file with a custom driver always conflicts, and no repo code runs.
            pairs.push((format!("merge.{d}.driver"), "false".into()));
            pairs.push((format!("merge.{d}.recursive"), "text".into()));
        }
    }
    pairs.extend(extra.iter().cloned());
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), pairs.len().to_string()), ("GIT_ATTR_NOSYSTEM".into(), "1".into())];
    for (i, (k, v)) in pairs.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), k));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), v));
    }
    Ok(env)
}

/// Whether a `git --version` line names git 2.40 or newer, the first with `--attr-source`.
fn supports_attr_source(version: &str) -> bool {
    let Some(v) = version.strip_prefix("git version ") else { return false };
    let mut parts = v.split(|c: char| !c.is_ascii_digit()).map(|p| p.parse::<u32>());
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= (2, 40),
        _ => false,
    }
}

/// Errs unless the installed git has `--attr-source`; checked once.
fn require_attr_source() -> Result<(), String> {
    static OK: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    OK.get_or_init(|| {
        let v = git(Path::new("/"), &["--version"])?;
        if supports_attr_source(v.stdout.trim()) {
            Ok(())
        } else {
            Err(format!("amm needs git 2.40 or newer for its integration worktree (found: {})", v.stdout.trim()))
        }
    })
    .clone()
}

/// `git` for amm's own operations on the integration worktree (worktree add, merge-tree, reset, clean, status): on
/// top of the hardening `git` always has, attributes come only from `attr_tree` (`--attr-source`, no
/// core.attributesFile, no system file; a non-empty `info/attributes` is refused), so a worker's .gitattributes in a
/// worktree or a gated commit selects no driver. Known filter and merge drivers are neutralised too, and there is no
/// signing, signature check or automatic gc; commits are `amm <amm@localhost>`. Blocking. Errors if git is older
/// than 2.40.
pub fn git_checkout(root: &Path, attr_tree: &str, args: &[&str]) -> Result<GitResult, String> {
    require_attr_source()?;
    let common = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    if common.code != 0 {
        return Err(format!("git rev-parse failed: {}", common.stderr.trim()));
    }
    let info = Path::new(common.stdout.trim()).join("info").join("attributes");
    if std::fs::metadata(&info).is_ok_and(|m| m.len() > 0) {
        return Err(format!("{} is not empty; amm refuses repository-wide attributes", info.display()));
    }
    let (name, email) = AMM_IDENT;
    let extra: Vec<(String, String)> = [
        ("core.attributesFile", "/dev/null"),
        ("commit.gpgsign", "false"),
        ("merge.verifySignatures", "false"),
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
        ("user.name", name),
        ("user.email", email),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let mut env = config_env(root, &extra, true)?;
    // git-lfs is the usual smudge filter; tell it to stay out even where a filter is not reached through config.
    env.push(("GIT_LFS_SKIP_SMUDGE".into(), "1".into()));
    for who in ["AUTHOR", "COMMITTER"] {
        env.push((format!("GIT_{who}_NAME"), name.into()));
        env.push((format!("GIT_{who}_EMAIL"), email.into()));
    }
    let source = format!("--attr-source={attr_tree}");
    let full: Vec<&str> = std::iter::once(source.as_str()).chain(args.iter().copied()).collect();
    run(root, &full, &env, MAX_OUTPUT, GIT_TIMEOUT)
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

/// A private directory under the temp dir, removed on drop.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn new() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        for _ in 0..8 {
            let p = std::env::temp_dir().join(format!("amm-index-{:016x}", rand::random::<u64>()));
            // create (not create_all) fails if the path exists, so nobody else's directory is ever used.
            match std::fs::DirBuilder::new().mode(0o700).create(&p) {
                Ok(()) => return Ok(PrivateDir(p)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create a private index: {e}")),
            }
        }
        Err("cannot create a private index".into())
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A path C-quoted the way `--stdin-paths` unquotes it, so any byte (newlines, quotes) survives.
fn c_quote(path: &str) -> Vec<u8> {
    let mut out = vec![b'"'];
    for b in path.bytes() {
        match b {
            b'"' | b'\\' => out.extend([b'\\', b]),
            0..0x20 | 0x7f => out.extend(format!("\\{b:03o}").bytes()),
            _ => out.push(b),
        }
    }
    out.push(b'"');
    out
}

/// The NUL-separated names in `out`, refusing any git could not give as UTF-8 (they would be mangled).
fn names(out: &str) -> Result<Vec<&str>, String> {
    let v: Vec<&str> = out.split('\0').filter(|s| !s.is_empty()).collect();
    if v.iter().any(|n| n.contains('\u{FFFD}')) {
        return Err("a file name is not valid UTF-8".into());
    }
    Ok(v)
}

/// Commits the worktree `root` as it is on disk on top of `parent`, as `amm <amm@localhost>`, then moves
/// `refs/heads/<branch>` from `parent` to the commit (compare-and-swap). Returns the commit, or `parent` itself when
/// nothing changed. The worker's index is never read: a private index is seeded from `parent`, every file `parent`
/// tracks plus every untracked, non-ignored file is hashed with `hash-object --no-filters`, with its mode (exec bit,
/// symlink) from `lstat`, and files gone from disk are deleted. So no filter, hook or signer runs, and skip-worktree,
/// assume-unchanged or intent-to-add entries change nothing. Submodules are kept as `parent` has them. Blocking.
/// Errors if any git call fails or times out, or the branch is no longer on `parent`.
pub fn commit_all(root: &Path, branch: &str, parent: &str, message: &str) -> Result<String, String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let dir = PrivateDir::new()?;
    let (name, email) = AMM_IDENT;
    let mut env: Vec<(String, String)> = vec![
        ("GIT_INDEX_FILE".into(), dir.0.join("index").to_string_lossy().into_owned()),
        ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
    ];
    for who in ["AUTHOR", "COMMITTER"] {
        env.push((format!("GIT_{who}_NAME"), name.into()));
        env.push((format!("GIT_{who}_EMAIL"), email.into()));
    }
    let g = |args: &[&str], input: Option<Vec<u8>>| -> Result<String, String> {
        let r = run_in(root, args, &env, input, MAX_OUTPUT, GIT_TIMEOUT)?;
        if r.code != 0 {
            return Err(format!("git {} failed: {}", args.first().copied().unwrap_or(""), r.stderr.trim()));
        }
        Ok(r.stdout)
    };
    let parent_tree = g(&["rev-parse", "--verify", "--end-of-options", &format!("{parent}^{{tree}}")], None)?.trim().to_string();
    g(&["read-tree", "--end-of-options", parent], None)?;
    let listing = g(&["ls-tree", "-r", "-z", "--full-tree", "--end-of-options", parent], None)?;
    let mut paths: Vec<String> = Vec::new();
    for entry in names(&listing)? {
        let (meta, path) = entry.split_once('\t').ok_or("unexpected ls-tree output")?;
        if !meta.starts_with("160000 ") {
            paths.push(path.to_string());
        }
    }
    let others = g(&["ls-files", "--others", "--exclude-standard", "--full-name", "-z"], None)?;
    // A trailing slash is a nested repository: not amm's to commit.
    paths.extend(names(&others)?.into_iter().filter(|p| !p.ends_with('/')).map(str::to_string));
    let mut info: Vec<u8> = Vec::new();
    let mut entry = |mode: &str, sha: &str, path: &str| {
        info.extend_from_slice(format!("{mode} {sha}\t{path}").as_bytes());
        info.push(0);
    };
    let (mut files, mut file_modes) = (Vec::new(), Vec::new());
    for path in &paths {
        match std::fs::symlink_metadata(root.join(path)) {
            Ok(m) if m.file_type().is_symlink() => {
                let target = std::fs::read_link(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                let sha = g(&["hash-object", "-w", "--no-filters", "--stdin"], Some(target.as_os_str().as_bytes().to_vec()))?;
                entry("120000", sha.trim(), path);
            }
            Ok(m) if m.file_type().is_file() => {
                files.push(path.as_str());
                file_modes.push(if m.permissions().mode() & 0o100 != 0 { "100755" } else { "100644" });
            }
            // Gone, or replaced by a directory (whose files are listed on their own): the path is deleted.
            Ok(m) if m.file_type().is_dir() => entry("0", &"0".repeat(40), path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => entry("0", &"0".repeat(40), path),
            Ok(_) => return Err(format!("{path} is not a regular file, symlink or directory")),
            Err(e) => return Err(format!("{path}: {e}")),
        }
    }
    if !files.is_empty() {
        let input: Vec<u8> = files.iter().flat_map(|p| [c_quote(p), vec![b'\n']].concat()).collect();
        let out = g(&["hash-object", "-w", "--no-filters", "--stdin-paths"], Some(input))?;
        let shas: Vec<&str> = out.lines().collect();
        if shas.len() != files.len() {
            return Err("git hash-object returned the wrong number of hashes".into());
        }
        for ((path, mode), sha) in files.iter().zip(&file_modes).zip(shas) {
            entry(mode, sha, path);
        }
    }
    g(&["update-index", "-z", "--index-info"], Some(info))?;
    let tree = g(&["write-tree"], None)?.trim().to_string();
    if tree == parent_tree {
        return Ok(parent.to_string());
    }
    let new = g(
        &["-c", "commit.gpgsign=false", "commit-tree", "--no-gpg-sign", "-p", parent, "-m", message, "--end-of-options", &tree],
        None,
    )?
    .trim()
    .to_string();
    g(&["update-ref", "-m", "amm: gate commit", "--end-of-options", &format!("refs/heads/{branch}"), &new, parent], None)?;
    // Best effort: the worker's own index now matches the commit. read-tree without -u touches no file.
    let _ = git(root, &["read-tree", "-q", "--end-of-options", &new]);
    Ok(new)
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
        let sha = commit_all(&dir, "main", &base, "amm(b1): t").unwrap();
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
        assert_eq!(commit_all(&dir, "main", &base, "m").unwrap(), base);
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
        let sha = commit_all(&dir, "main", &head(&dir), "m").unwrap();
        assert_eq!(sha, head(&dir));
        assert_eq!(show(&dir, "%G?"), "N");
        assert!(!marker.exists(), "a planted hook, signer or filter ran");
    }

    #[test]
    fn commit_all_errors_when_the_branch_moved_or_is_locked() {
        let dir = repo();
        let base = head(&dir);
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        assert!(commit_all(&dir, "main", &"0".repeat(40), "m").is_err());
        assert_eq!(head(&dir), base);
        fs::write(dir.join(".git/refs/heads/main.lock"), "").unwrap();
        assert!(commit_all(&dir, "main", &base, "m").is_err());
        assert_eq!(head(&dir), base);
    }

    #[test]
    fn commit_all_ignores_the_index_and_commits_skip_worktree_and_intent_to_add_files() {
        let dir = repo();
        let base = head(&dir);
        git_in(&dir, &["update-index", "--skip-worktree", "a.txt"]);
        fs::write(dir.join("a.txt"), "hidden change\n").unwrap();
        fs::write(dir.join("ita.txt"), "i\n").unwrap();
        git_in(&dir, &["add", "-N", "ita.txt"]);
        let sha = commit_all(&dir, "main", &base, "m").unwrap();
        let files = git(&dir, &["diff", "--name-only", &base, &sha]).unwrap().stdout;
        assert_eq!(sorted(files.lines().map(str::to_string).collect()), ["a.txt", "ita.txt"]);
        assert_eq!(git(&dir, &["show", &format!("{sha}:a.txt")]).unwrap().stdout, "hidden change\n");
    }

    #[test]
    fn commit_all_commits_deletions_exec_bits_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo();
        fs::write(dir.join("gone.txt"), "g\n").unwrap();
        fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-qm", "more"]);
        let base = head(&dir);
        fs::remove_file(dir.join("gone.txt")).unwrap();
        fs::set_permissions(dir.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
        fs::write(dir.join("we\"ird\nname"), "w\n").unwrap();
        fs::write(dir.join("ünï.txt"), "u\n").unwrap();
        let sha = commit_all(&dir, "main", &base, "m").unwrap();
        let tree = git(&dir, &["ls-tree", "-r", "-z", &sha]).unwrap().stdout;
        let entries: Vec<&str> = tree.split('\0').filter(|e| !e.is_empty()).collect();
        let mode = |p: &str| entries.iter().find(|e| e.ends_with(&format!("\t{p}"))).map(|e| e.split(' ').next().unwrap().to_string());
        assert_eq!(mode("gone.txt"), None);
        assert_eq!(mode("run.sh").as_deref(), Some("100755"));
        assert_eq!(mode("link").as_deref(), Some("120000"));
        assert_eq!(git(&dir, &["cat-file", "-p", &format!("{sha}:link")]).unwrap().stdout, "a.txt");
        assert_eq!(mode("we\"ird\nname").as_deref(), Some("100644"));
        assert_eq!(mode("a.txt").as_deref(), Some("100644"));
        assert_eq!(git(&dir, &["show", &format!("{sha}:ünï.txt")]).unwrap().stdout, "u\n");
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
    fn git_checkout_reads_attributes_only_from_the_trusted_tree() {
        let dir = repo();
        let tree = git(&dir, &["rev-parse", "HEAD^{tree}"]).unwrap().stdout.trim().to_string();
        // Worktree, global and info attributes a worker could plant.
        fs::write(dir.join(".gitattributes"), "a.txt merge=x\n").unwrap();
        let global = tmp_dir().join("attrs");
        fs::write(&global, "a.txt diff=y\n").unwrap();
        git_in(&dir, &["config", "core.attributesFile", global.to_str().unwrap()]);
        let attrs = git_checkout(&dir, &tree, &["check-attr", "merge", "diff", "--", "a.txt"]).unwrap().stdout;
        assert_eq!(attrs, "a.txt: merge: unspecified\na.txt: diff: unspecified\n");
        fs::create_dir_all(dir.join(".git/info")).unwrap();
        fs::write(dir.join(".git/info/attributes"), "* merge=x\n").unwrap();
        assert!(git_checkout(&dir, &tree, &["status"]).unwrap_err().contains("info/attributes"));
    }

    #[test]
    fn attr_source_needs_git_2_40() {
        assert!(supports_attr_source("git version 2.40.0"));
        assert!(supports_attr_source("git version 2.56.0"));
        assert!(supports_attr_source("git version 3.0.1 (Apple Git-1)"));
        assert!(!supports_attr_source("git version 2.39.5"));
        assert!(!supports_attr_source("nonsense"));
    }

    #[test]
    fn git_checkout_runs_no_smudge_filter_or_merge_driver() {
        let dir = repo();
        let marker = tmp_dir().join("ran");
        fs::write(dir.join(".gitattributes"), "* filter=x merge=m\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-qm", "attrs"]);
        let base = head(&dir);
        git_in(&dir, &["checkout", "-qb", "side"]);
        fs::write(dir.join("a.txt"), "side\n").unwrap();
        git_in(&dir, &["commit", "-qam", "side"]);
        git_in(&dir, &["checkout", "-q", "main"]);
        fs::write(dir.join("a.txt"), "main\n").unwrap();
        git_in(&dir, &["commit", "-qam", "main"]);
        let touch = format!("touch {}; cat", marker.display());
        git_in(&dir, &["config", "filter.x.smudge", &touch]);
        git_in(&dir, &["config", "filter.x.clean", &touch]);
        git_in(&dir, &["config", "merge.m.driver", &format!("touch {}; true", marker.display())]);
        // Even with the planted attributes as the trusted tree, the drivers stay neutralised.
        let tree = git(&dir, &["rev-parse", "HEAD^{tree}"]).unwrap().stdout.trim().to_string();
        let m = git_checkout(&dir, &tree, &["merge", "--no-edit", "side"]).unwrap();
        assert_ne!(m.code, 0, "the neutralised driver makes the file conflict");
        git_checkout(&dir, &tree, &["merge", "--abort"]).unwrap();
        git_checkout(&dir, &tree, &["reset", "-q", "--hard", &base]).unwrap();
        assert!(!marker.exists(), "a planted smudge filter or merge driver ran");
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
