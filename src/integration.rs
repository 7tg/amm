//! The run's integration worktree and its merge queue. The git and test work are plain blocking functions, run on
//! the blocking pool and never under the `Amm` lock; `integrate` drives them and takes the lock only for reads and
//! guarded transitions.
use crate::checks::run_check;
use crate::git::{committed_files, git, git_checkout};
use crate::scope::in_scope;
use crate::service::{Amm, AmmResult, CONFLICT_NOTE};
use crate::types::{Brief, BriefState, Run, RunState, Worker, WorkerState};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// The branch a run integrates into.
pub fn integration_branch(run_id: &str) -> String {
    format!("amm/{run_id}")
}

/// Where a run's integration worktree lives.
pub fn worktree_path(repo: &Path, run_id: &str) -> PathBuf {
    repo.join(".claude").join("worktrees").join(format!("amm-{run_id}"))
}

fn fail(what: &str, stderr: &str) -> String {
    format!("{what}: {}", stderr.lines().next().unwrap_or("").trim())
}

/// Creates `<repo>/.claude/worktrees/amm-<run>` on a new branch `amm/<run>` at `base_branch`, and returns its real
/// path and the base commit, which becomes the run's trusted integration head. The checkout runs no filter (smudge
/// included), hook or fsmonitor, and takes attributes only from the base commit's tree. On any error the half-made
/// worktree and branch are removed.
pub fn create_worktree(repo: &Path, run_id: &str, base_branch: &str) -> Result<(PathBuf, String), String> {
    let base = git(repo, &["rev-parse", "--verify", "--end-of-options", &format!("{base_branch}^{{commit}}")])?;
    let sha = base.stdout.trim().to_string();
    if base.code != 0 || sha.is_empty() {
        return Err(fail(&format!("base branch {base_branch} is not a commit"), &base.stderr));
    }
    let base_tree = tree_of(repo, &sha)?;
    let path = worktree_path(repo, run_id);
    let branch = integration_branch(run_id);
    // Never clean up what amm did not make: refuse a branch or path that already exists.
    let exists = git(repo, &["rev-parse", "--verify", "-q", "--end-of-options", &format!("refs/heads/{branch}")])?;
    if exists.code == 0 || path.exists() {
        return Err(format!("{branch} or {} already exists", path.display()));
    }
    let made = (|| {
        let path_s = path.to_string_lossy();
        let r = git_checkout(repo, &base_tree, &["worktree", "add", "-q", "-b", &branch, "--end-of-options", &path_s, &sha])?;
        if r.code != 0 {
            return Err(fail("could not create the integration worktree", &r.stderr));
        }
        let real = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        verify_clean(&real, &branch, &sha)?;
        Ok(real)
    })();
    match made {
        Ok(real) => Ok((real, sha)),
        Err(e) => {
            let _ = remove_worker_worktree(repo, &path, &branch);
            Err(e)
        }
    }
}

/// The tree of commit `sha`.
fn tree_of(dir: &Path, sha: &str) -> Result<String, String> {
    let r = git(dir, &["rev-parse", "--verify", "--end-of-options", &format!("{sha}^{{tree}}")])?;
    let t = r.stdout.trim();
    if r.code != 0 || t.is_empty() {
        return Err(fail(&format!("{sha} is not a commit"), &r.stderr));
    }
    Ok(t.to_string())
}

/// The worktree is on `refs/heads/<branch>`, the branch is on `sha`, and nothing differs from it but ignored files.
pub fn verify_clean(wt: &Path, branch: &str, sha: &str) -> Result<(), String> {
    let sym = git(wt, &["symbolic-ref", "-q", "HEAD"])?;
    if sym.code != 0 || sym.stdout.trim() != format!("refs/heads/{branch}") {
        return Err(format!("the integration worktree is not on {branch}"));
    }
    let at = git(wt, &["rev-parse", "--verify", "-q", "--end-of-options", &format!("refs/heads/{branch}^{{commit}}")])?;
    if at.code != 0 || at.stdout.trim() != sha {
        return Err(format!("{branch} is not on {sha}"));
    }
    let tree = tree_of(wt, sha)?;
    let st = git_checkout(wt, &tree, &["status", "--porcelain", "-z", "--untracked-files=all", "--ignore-submodules=none"])?;
    if st.code != 0 {
        return Err(fail("git status failed", &st.stderr));
    }
    let dirty: Vec<&str> = st.stdout.split('\0').filter(|e| !e.is_empty()).map(|e| e.get(3..).unwrap_or(e)).collect();
    if !dirty.is_empty() {
        return Err(format!("the integration worktree has changes: {}", dirty.join(", ")));
    }
    Ok(())
}

/// Removes a worktree (`git worktree remove --force`) and deletes its branch (`git branch -D`). A worktree or branch
/// that is already gone is not an error.
pub fn remove_worker_worktree(repo: &Path, worktree: &Path, branch: &str) -> Result<(), String> {
    let wt = worktree.to_string_lossy();
    let r = git(repo, &["worktree", "remove", "--force", "--end-of-options", &wt])?;
    let gone = |e: &str| e.contains("is not a working tree") || e.contains("not found") || e.contains("No such file");
    if r.code != 0 && !gone(&r.stderr) {
        return Err(fail("could not remove the worktree", &r.stderr));
    }
    let r = git(repo, &["branch", "-D", "--end-of-options", branch])?;
    if r.code != 0 && !gone(&r.stderr) {
        return Err(fail("could not delete the branch", &r.stderr));
    }
    Ok(())
}

/// The ref that keeps a brief's gated commit alive (worker branches and worktrees get deleted).
pub fn pin_ref(brief_id: &str) -> String {
    format!("refs/amm/gated/{brief_id}")
}

/// Pins `sha` as the brief's gated commit. Blocking.
pub fn pin_gated(dir: &Path, brief_id: &str, sha: &str) -> Result<(), String> {
    let r = git(dir, &["update-ref", "-m", "amm: gated", "--end-of-options", &pin_ref(brief_id), sha])?;
    if r.code != 0 {
        return Err(fail("could not pin the gated commit", &r.stderr));
    }
    Ok(())
}

/// Drops a brief's pin; a missing pin is fine. Blocking.
pub fn unpin_gated(dir: &Path, brief_id: &str) -> Result<(), String> {
    let r = git(dir, &["update-ref", "-d", "--end-of-options", &pin_ref(brief_id)])?;
    if r.code != 0 {
        return Err(fail("could not drop the pin", &r.stderr));
    }
    Ok(())
}

/// Errors after which a brief fails, and the first back-off (doubling).
const MAX_INTEGRATION_ERRORS: u32 = 3;
const RETRY_MS: i64 = 60_000;

/// Longest test output kept in a brief's notes.
const NOTE_TAIL: usize = 1500;

fn lock(state: &Mutex<Amm>) -> MutexGuard<'_, Amm> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// What one merge attempt came to.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    /// Merged and the tests passed; the branch is on `new`.
    Merged { new: String },
    /// The merge conflicted and was aborted; the branch is back on the old head.
    Conflict { files: Vec<String> },
    /// The tests failed and the merge was reset; the branch is back on the old head.
    TestFailed { tail: String },
    /// The merge would change files outside the brief's scope (git's merge base saw history the gate did not); it
    /// was reset and the brief fails closed.
    ScopeViolation { files: Vec<String> },
    /// Something went wrong that may pass on a later try; the branch is back on the old head.
    Error(String),
    /// amm/<run> is not where amm left it: stop integrating this run.
    Diverged(String),
}

/// One merge, as the blocking phase needs it.
#[derive(Clone)]
struct Job {
    /// The run's repository: the worktree must still share its git dir.
    repo: PathBuf,
    wt: PathBuf,
    branch: String,
    /// `Run.integration_head`: where amm left the branch.
    head: String,
    gated: String,
    brief_id: String,
    message: String,
    test_cmd: String,
    budget_ms: u64,
    /// The brief's scope: every file the merge changes must be in it.
    scope: Vec<String>,
}

fn rev(wt: &Path, r: &str) -> Option<String> {
    let o = git(wt, &["rev-parse", "--verify", "-q", "--end-of-options", r]).ok()?;
    let s = o.stdout.trim();
    (o.code == 0 && !s.is_empty()).then(|| s.to_string())
}

/// The integration worktree must still be a worktree of `repo`, on `refs/heads/<branch>`, with the branch and HEAD on
/// `head`.
fn verify_at(repo: &Path, wt: &Path, branch: &str, head: &str) -> Result<(), String> {
    let (mine, theirs) = (crate::git::worktree_info(wt)?, crate::git::worktree_info(repo)?);
    if mine.common_dir != theirs.common_dir || mine.toplevel != wt {
        return Err("the integration worktree is no longer a worktree of the repository".into());
    }
    if mine.branch.as_deref() != Some(branch) {
        return Err(format!("the integration worktree is no longer on {branch}"));
    }
    match rev(wt, &format!("refs/heads/{branch}^{{commit}}")) {
        Some(at) if at == head && mine.head == head => Ok(()),
        Some(at) => Err(format!("{branch} is on {at}, not on {head} where amm left it")),
        None => Err(format!("{branch} cannot be read")),
    }
}

/// Puts the worktree's files back to `sha`: tracked files reset, untracked files removed (ignored ones, such as
/// installed dependencies, stay). Attributes come from `attr_tree`.
fn reset_to(wt: &Path, attr_tree: &str, sha: &str) -> Result<(), String> {
    for args in [&["reset", "-q", "--hard", sha][..], &["clean", "-q", "-f", "-d"][..]] {
        let r = git_checkout(wt, attr_tree, args)?;
        if r.code != 0 {
            return Err(format!("git {} failed: {}", args[0], first_line(&r.stderr)));
        }
    }
    Ok(())
}

/// Moves `refs/heads/<branch>` from `old` to `new`, only if it is still on `old`.
fn cas(wt: &Path, branch: &str, new: &str, old: &str) -> Result<(), String> {
    let r = git(wt, &["update-ref", "-m", "amm: integrate", "--end-of-options", &format!("refs/heads/{branch}"), new, old])?;
    if r.code != 0 {
        return Err(format!("{branch} moved under amm: {}", first_line(&r.stderr)));
    }
    Ok(())
}

/// Takes amm's merge `new` back off the branch: the ref back to `head` (only if it is still on `new`), HEAD back on
/// the branch, the files back to `head`.
fn undo(job: &Job, new: &str) -> Result<(), String> {
    let wt = job.wt.as_path();
    cas(wt, &job.branch, &job.head, new)?;
    let _ = git(wt, &["symbolic-ref", "HEAD", &format!("refs/heads/{}", job.branch)]);
    reset_to(wt, &tree_of(wt, &job.head)?, &job.head)?;
    verify_at(&job.repo, wt, &job.branch, &job.head)
}

/// The blocking phase. The merge is built without the worktree (`merge-tree`), scope-checked, committed and put on
/// the branch with a compare-and-swap; only then is the worktree checked out at it and the tests run. Attributes come
/// from the head's tree, never from the gated commit.
fn merge_one(job: &Job) -> Outcome {
    let wt = job.wt.as_path();
    if let Err(e) = verify_at(&job.repo, wt, &job.branch, &job.head) {
        return Outcome::Diverged(e);
    }
    let head_tree = match tree_of(wt, &job.head) {
        Ok(t) => t,
        Err(e) => return Outcome::Diverged(e),
    };
    if let Err(e) = reset_to(wt, &head_tree, &job.head) {
        return Outcome::Error(e);
    }
    if rev(wt, &format!("{}^{{commit}}", job.gated)).as_deref() != Some(job.gated.as_str()) {
        return Outcome::Error(format!("commit {} is missing", job.gated));
    }
    // The sha amm recorded is what gets merged; the pin only keeps it alive, so a moved pin is put back.
    if rev(wt, &pin_ref(&job.brief_id)).as_deref() != Some(job.gated.as_str())
        && let Err(e) = pin_gated(wt, &job.brief_id, &job.gated)
    {
        return Outcome::Error(e);
    }
    let mt = git_checkout(
        wt,
        &head_tree,
        &["merge-tree", "--write-tree", "-z", "--name-only", "--no-messages", "--end-of-options", &job.head, &job.gated],
    );
    let mt = match mt {
        Ok(m) => m,
        Err(e) => return Outcome::Error(e),
    };
    let mut fields = mt.stdout.split('\0').filter(|f| !f.is_empty());
    let tree = fields.next().unwrap_or("").trim().to_string();
    match mt.code {
        0 if !tree.is_empty() => {}
        1 => {
            let mut files: Vec<String> = fields.map(str::to_string).collect();
            files.dedup();
            return Outcome::Conflict { files };
        }
        _ => return Outcome::Error(format!("git merge-tree failed: {}", first_line(&mt.stderr))),
    }
    // Defence in depth: the gate scope-checked the worker's diff from its own base, but the merge applies the diff
    // from git's merge base, which a crafted history (a criss-cross) can move. Check what the merge changes.
    let changed = match committed_files(wt, &job.head, &tree) {
        Ok(f) => f,
        Err(e) => return Outcome::Error(e),
    };
    let root = wt.to_string_lossy();
    let outside: Vec<String> = changed.into_iter().filter(|f| !in_scope(&wt.join(f).to_string_lossy(), &root, &job.scope)).collect();
    if !outside.is_empty() {
        return Outcome::ScopeViolation { files: outside };
    }
    let commit = git_checkout(
        wt,
        &head_tree,
        &["commit-tree", "--no-gpg-sign", "-p", &job.head, "-p", &job.gated, "-m", &job.message, "--end-of-options", &tree],
    );
    let new = match commit {
        Ok(c) if c.code == 0 && !c.stdout.trim().is_empty() => c.stdout.trim().to_string(),
        Ok(c) => return Outcome::Error(format!("git commit-tree failed: {}", first_line(&c.stderr))),
        Err(e) => return Outcome::Error(e),
    };
    if let Err(e) = cas(wt, &job.branch, &new, &job.head) {
        return Outcome::Diverged(e);
    }
    if let Err(e) = reset_to(wt, &head_tree, &new) {
        return match undo(job, &new) {
            Ok(()) => Outcome::Error(e),
            Err(u) => Outcome::Diverged(u),
        };
    }
    let r = run_check(&job.test_cmd, wt, job.budget_ms);
    // The tests run repo code: HEAD, the branch and the tree must still be amm's merge.
    let intact = verify_at(&job.repo, wt, &job.branch, &new).and_then(|_| match rev(wt, "HEAD^{tree}") {
        Some(t) if t == tree => Ok(()),
        _ => Err("HEAD no longer holds amm's merge".into()),
    });
    if let Err(e) = intact {
        let _ = undo(job, &new);
        return Outcome::Diverged(format!("after the integration tests: {e}"));
    }
    if r.ok {
        let _ = reset_to(wt, &head_tree, &new);
        return Outcome::Merged { new };
    }
    let how = if r.timed_out { "timed out".to_string() } else { format!("exited {}", r.code.map_or("null".into(), |c| c.to_string())) };
    let tail = tail(&format!("`{}` {how}:\n{}", job.test_cmd, r.output), NOTE_TAIL);
    match undo(job, &new) {
        Ok(()) => Outcome::TestFailed { tail },
        Err(e) => Outcome::Diverged(e),
    }
}

/// Whether a failed run may resume: its integration worktree is amm's again, on `amm/<run>` at the recorded head,
/// with nothing changed but ignored files. Blocking.
pub fn check_resumable(repo: &Path, wt: &Path, branch: &str, head: &str) -> Result<(), String> {
    verify_at(repo, wt, branch, head)?;
    verify_clean(wt, branch, head)
}

/// After a restart: a brief left in `merging` means a merge may be half done. The branch may be on amm's head, or on
/// a merge commit amm made of that head and one of the `stale` gated commits before it could record it; anything else
/// was not amm's doing. Puts the branch and the worktree back on `head`.
fn recover(repo: &Path, wt: &Path, branch: &str, head: &str, stale: &[String]) -> Result<(), String> {
    let at = rev(wt, &format!("refs/heads/{branch}^{{commit}}")).ok_or_else(|| format!("{branch} cannot be read"))?;
    let ours = at == head || {
        let (p1, p2, p3) = (rev(wt, &format!("{at}^1")), rev(wt, &format!("{at}^2")), rev(wt, &format!("{at}^3")));
        p1.as_deref() == Some(head) && p2.as_ref().is_some_and(|p| stale.contains(p)) && p3.is_none()
    };
    if !ours {
        return Err(format!("{branch} is on {at}, not on {head} where amm left it"));
    }
    let sym = git(wt, &["symbolic-ref", "-q", "HEAD"])?;
    if sym.code != 0 || sym.stdout.trim() != format!("refs/heads/{branch}") {
        return Err(format!("the integration worktree is no longer on {branch}"));
    }
    if at != head {
        cas(wt, branch, head, &at)?;
    }
    reset_to(wt, &tree_of(wt, head)?, head)?;
    verify_at(repo, wt, branch, head)
}

/// Releases a run's merge claim when dropped, panics included. `disarm` once the claim was released under the lock.
struct Claim {
    state: Arc<Mutex<Amm>>,
    run_id: String,
    armed: bool,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.armed {
            lock(&self.state).merging.remove(&self.run_id);
        }
    }
}

/// What `integrate` reads under the lock for one run.
struct RunView {
    wt: PathBuf,
    branch: String,
    head: String,
    test_cmd: String,
    repo: PathBuf,
}

fn view(run: &Run) -> Option<RunView> {
    let wt = run.integration_worktree.clone()?;
    let head = run.integration_head.clone().filter(|h| !h.is_empty())?;
    (run.state == RunState::Active && !run.integration_branch.is_empty()).then(|| RunView {
        wt: PathBuf::from(wt),
        branch: run.integration_branch.clone(),
        head,
        test_cmd: run.test_cmd.clone(),
        repo: PathBuf::from(&run.repo),
    })
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| format!("internal: {e}"))
}

fn log_err(what: &str, e: impl std::fmt::Display) {
    eprintln!("amm: integrate: {what}: {e}");
}

impl Amm {
    /// The worker whose gated commit a done brief waits to merge.
    fn gated_worker(&self, brief_id: &str) -> Option<Worker> {
        self.store
            .all::<Worker>()
            .into_iter()
            .filter(|w| w.brief_id == brief_id && w.state == WorkerState::Done && w.gated_sha.is_some())
            .max_by(|a, b| a.started_at.cmp(&b.started_at).then_with(|| b.id.cmp(&a.id)))
    }

    fn run_briefs(&self, run_id: &str) -> Vec<Brief> {
        let mut bs: Vec<Brief> = self.store.all::<Brief>().into_iter().filter(|b| b.run_id == run_id).collect();
        bs.sort_by_key(|b| b.seq);
        bs
    }

    /// Active runs with briefs waiting to merge (or left merging by a restart) and no merge queue running.
    pub fn runs_to_integrate(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .store
            .all::<Run>()
            .into_iter()
            .filter(|r| r.state == RunState::Active && view(r).is_some() && !self.merging.contains(&r.id))
            .filter(|r| {
                let now = (self.now)();
                self.run_briefs(&r.id).iter().any(|b| {
                    (b.state == BriefState::Done && b.retry_at <= now) || b.state == BriefState::Merging
                })
            })
            .map(|r| r.id)
            .collect();
        ids.sort();
        ids
    }

    /// Picks the lowest-seq done brief with a gated commit and moves it to merging. None when nothing is left.
    fn next_merge(&mut self, run_id: &str) -> AmmResult<Option<(Brief, Worker)>> {
        let now = (self.now)();
        for b in self.run_briefs(run_id).into_iter().filter(|b| b.state == BriefState::Done && b.retry_at <= now) {
            let Some(w) = self.gated_worker(&b.id) else { continue };
            if let Some(b) = self.brief_transition(&b.id, &[BriefState::Done], |x| x.state = BriefState::Merging)? {
                self.event("merging", Some(run_id), Some(&w.id), json!({ "briefId": b.id, "sha": w.gated_sha }))?;
                return Ok(Some((b, w)));
            }
        }
        Ok(None)
    }

    /// Stops integrating a run whose branch amm no longer controls; briefs left merging go back to done.
    fn fail_run(&mut self, run_id: &str, reason: &str) -> AmmResult<()> {
        for b in self.run_briefs(run_id).into_iter().filter(|b| b.state == BriefState::Merging) {
            self.brief_transition(&b.id, &[BriefState::Merging], |x| x.state = BriefState::Done)?;
        }
        let run = self.run(run_id)?;
        if run.state == RunState::Active {
            self.store.put(&Run { state: RunState::Failed, last_test_ok: None, ..run })?;
        }
        self.event("run_failed", Some(run_id), None, json!({ "reason": reason }))
    }

    /// Records one merge's outcome. Returns true if the merge stays on the branch (only for `Merged` with the brief
    /// still merging); false means the caller must reset the branch to the old head.
    fn record(&mut self, run_id: &str, b: &Brief, w: &Worker, outcome: &Outcome) -> AmmResult<bool> {
        let ids = json!({ "briefId": b.id });
        match outcome {
            Outcome::Merged { new } => {
                let cur = self.brief(&b.id)?;
                if cur.state != BriefState::Merging {
                    return Ok(false);
                }
                let run = self.run(run_id)?;
                // One transaction: a crash or failed write never leaves a merged brief without its head, or the reverse.
                self.store.put_pair(
                    &Brief { state: BriefState::Merged, ..cur },
                    &Run { merges: run.merges + 1, last_test_ok: Some(true), integration_head: Some(new.clone()), ..run },
                )?;
                self.event("merged", Some(run_id), Some(&w.id), json!({ "briefId": b.id, "title": b.title, "sha": new }))?;
                Ok(true)
            }
            Outcome::Conflict { files } => {
                let note = format!("{CONFLICT_NOTE}: {}", files.join(", "));
                let moved = self.brief_transition(&b.id, &[BriefState::Merging], |x| {
                    x.state = BriefState::Queued;
                    x.attempt += 1;
                    x.report = None;
                    x.notes.push(note);
                })?;
                if moved.is_none() {
                    return Ok(false);
                }
                self.event("conflict", Some(run_id), Some(&w.id), json!({ "briefId": b.id, "files": files }))?;
                Ok(false)
            }
            Outcome::TestFailed { tail } => {
                let note = format!("the integration tests failed after merging your work:\n{tail}");
                let Some(cur) = self.brief_transition(&b.id, &[BriefState::Merging], |x| {
                    x.integration_failures += 1;
                    x.notes.push(note);
                })?
                else {
                    return Ok(false);
                };
                let run = self.run(run_id)?;
                self.store.put(&Run { last_test_ok: Some(false), ..run })?;
                let reason = first_line(tail).to_string();
                self.event(
                    "integration_failed",
                    Some(run_id),
                    Some(&w.id),
                    json!({ "briefId": b.id, "reason": reason, "tail": tail }),
                )?;
                if cur.integration_failures >= 2 {
                    self.escalate_brief(&cur, Some(&w.id), &format!("integration tests failed {} times", cur.integration_failures))?;
                } else {
                    self.store.put(&Brief { state: BriefState::Queued, attempt: cur.attempt + 1, report: None, ..cur })?;
                }
                Ok(false)
            }
            Outcome::ScopeViolation { files } => {
                let note = format!(
                    "merging your work would change files outside your scope: {}. amm refused the merge",
                    files.join(", ")
                );
                let moved = self.brief_transition(&b.id, &[BriefState::Merging], |x| {
                    x.state = BriefState::Failed;
                    x.notes.push(note);
                })?;
                if moved.is_none() {
                    return Ok(false);
                }
                self.event("scope_violation", Some(run_id), Some(&w.id), json!({ "briefId": b.id, "files": files }))?;
                Ok(false)
            }
            Outcome::Error(e) => {
                let now = (self.now)();
                let Some(cur) = self.brief_transition(&b.id, &[BriefState::Merging], |x| {
                    x.integration_errors += 1;
                    let n = x.integration_errors.min(10);
                    x.retry_at = now + RETRY_MS * (1 << (n - 1));
                    x.state = if x.integration_errors >= MAX_INTEGRATION_ERRORS { BriefState::Failed } else { BriefState::Done };
                })?
                else {
                    return Ok(false);
                };
                // A repeat of the same error is not news.
                if cur.last_integration_error.as_deref() != Some(e.as_str()) {
                    let mut d = ids;
                    d["error"] = json!(e);
                    self.event("integration_error", Some(run_id), Some(&w.id), d)?;
                }
                let failed = cur.state == BriefState::Failed;
                let mut notes = cur.notes.clone();
                if failed {
                    notes.push(format!("amm could not merge your work {} times: {e}", cur.integration_errors));
                }
                self.store.put(&Brief { last_integration_error: Some(e.clone()), notes, ..cur })?;
                if failed {
                    let reason = format!("merge failed {MAX_INTEGRATION_ERRORS} times: {}", first_line(e));
                    self.event("integration_failed", Some(run_id), Some(&w.id), json!({ "briefId": b.id, "reason": reason, "tail": e }))?;
                }
                Ok(false)
            }
            Outcome::Diverged(reason) => {
                self.fail_run(run_id, reason)?;
                Ok(false)
            }
        }
    }

    /// Ends the run as done when every brief is merged or cancelled, at least one merged, and the last tests passed.
    fn finish_run(&mut self, run_id: &str) -> AmmResult<()> {
        let run = self.run(run_id)?;
        let briefs = self.run_briefs(run_id);
        let all = briefs.iter().all(|b| matches!(b.state, BriefState::Merged | BriefState::Cancelled));
        let any = briefs.iter().any(|b| b.state == BriefState::Merged);
        if run.state == RunState::Active && all && any && run.last_test_ok == Some(true) {
            let merges = run.merges;
            self.store.put(&Run { state: RunState::Done, ..run.clone() })?;
            self.event("run_done", Some(run_id), None, json!({ "branch": run.integration_branch, "merges": merges }))?;
        }
        Ok(())
    }
}

/// The merge queue of one run: merges its done briefs one at a time, in seq order, into the integration worktree,
/// testing after each. Spawn it as a task; it returns at once if the run's queue is already running.
pub async fn integrate(state: Arc<Mutex<Amm>>, run_id: String) {
    {
        let mut amm = lock(&state);
        if !amm.merging.insert(run_id.clone()) {
            return;
        }
    }
    let mut claim = Claim { state: state.clone(), run_id: run_id.clone(), armed: true };
    if let Err(e) = queue(&state, &run_id, &mut claim).await {
        log_err(&run_id, e);
    }
}

async fn queue(state: &Arc<Mutex<Amm>>, run_id: &str, claim: &mut Claim) -> Result<(), String> {
    // A brief still merging while this process holds no claim was left by a restart: recover first.
    let stale = {
        let amm = lock(state);
        let Some(v) = amm.run(run_id).ok().as_ref().and_then(view) else { return Ok(()) };
        let stale: Vec<Brief> = amm.run_briefs(run_id).into_iter().filter(|b| b.state == BriefState::Merging).collect();
        let shas: Vec<String> = stale.iter().filter_map(|b| amm.gated_worker(&b.id)?.gated_sha).collect();
        (!stale.is_empty()).then_some((v, stale, shas))
    };
    if let Some((v, stale, shas)) = stale {
        let recovered = blocking(move || recover(&v.repo, &v.wt, &v.branch, &v.head, &shas)).await.and_then(|r| r);
        let mut amm = lock(state);
        match recovered {
            Ok(()) => {
                for b in stale {
                    amm.brief_transition(&b.id, &[BriefState::Merging], |x| x.state = BriefState::Done).map_err(|e| e.0)?;
                }
            }
            Err(e) => return amm.fail_run(run_id, &e).map_err(|e| e.0),
        }
    }
    loop {
        let (job, v, b, w) = {
            let mut amm = lock(state);
            let picked = match amm.run(run_id).ok().as_ref().and_then(view) {
                Some(v) => amm.next_merge(run_id).map_err(|e| e.0)?.map(|(b, w)| (v, b, w)),
                None => None,
            };
            let Some((v, b, w)) = picked else {
                // Release under the same lock as the last look, so a brief finishing now is never left waiting.
                amm.merging.remove(run_id);
                claim.armed = false;
                return amm.finish_run(run_id).map_err(|e| e.0);
            };
            let job = Job {
                repo: v.repo.clone(),
                wt: v.wt.clone(),
                branch: v.branch.clone(),
                head: v.head.clone(),
                gated: w.gated_sha.clone().unwrap_or_default(),
                brief_id: b.id.clone(),
                message: format!("amm: {} ({})", b.title, b.id),
                test_cmd: v.test_cmd.clone(),
                budget_ms: amm.limits.gate_budget_ms.max(0) as u64,
                scope: b.scope.clone(),
            };
            (job, v, b, w)
        };
        let undo_job = job.clone();
        let outcome = blocking(move || merge_one(&job)).await.unwrap_or_else(Outcome::Error);
        let kept = lock(state).record(run_id, &b, &w, &outcome).map_err(|e| e.0);
        match (&outcome, kept) {
            (Outcome::Merged { .. }, Ok(true)) => {}
            (Outcome::Merged { new }, Ok(false)) => {
                // The brief was cancelled while it merged: take the merge back off the branch.
                let new = new.clone();
                let undone = blocking(move || undo(&undo_job, &new)).await.and_then(|r| r);
                if let Err(e) = undone {
                    lock(state).fail_run(run_id, &e).map_err(|e| e.0)?;
                    return Ok(());
                }
            }
            (Outcome::Diverged(_), kept) => {
                kept?;
                return Ok(());
            }
            (_, kept) => {
                kept?;
            }
        }
        // Merged, requeued or final: the worker's worktree, branch and pin have served their purpose.
        let settled = lock(state).brief(&b.id).map(|x| x.state != BriefState::Done).unwrap_or(false);
        if settled {
            cleanup(&v, &b.id, &w).await;
        }
    }
}

/// Drops a settled brief's pin and removes its worker's worktree and branch (never the integration worktree or the
/// main checkout, and only `worktree-agent-*` branches). Failures are logged: they leave clutter, not risk.
async fn cleanup(v: &RunView, brief_id: &str, w: &Worker) {
    let (repo, iwt, brief_id) = (v.repo.clone(), v.wt.clone(), brief_id.to_string());
    let (wt, branch) = (w.worktree.clone(), w.branch.clone());
    let done = blocking(move || {
        let mut errs = Vec::new();
        if let Err(e) = unpin_gated(&repo, &brief_id) {
            errs.push(e);
        }
        if let (Some(wt), Some(branch)) = (wt, branch.filter(|b| b.starts_with("worktree-agent-")))
            && Path::new(&wt) != iwt
            && Path::new(&wt) != repo
            && let Err(e) = remove_worker_worktree(&repo, Path::new(&wt), &branch)
        {
            errs.push(e);
        }
        errs
    })
    .await;
    match done {
        Ok(errs) => errs.into_iter().for_each(|e| log_err("cleanup", e)),
        Err(e) => log_err("cleanup", e),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_LIMITS;
    use crate::service::{BriefInput, StartRun, new_run_id};
    use crate::testutil::{add_worktree, init_repo, init_repo_with, make_amm, tmp_dir};
    use crate::types::{AmmEvent, Tier};

    fn rev(dir: &Path, r: &str) -> Option<String> {
        let o = git(dir, &["rev-parse", "--verify", "-q", r]).unwrap();
        (o.code == 0).then(|| o.stdout.trim().to_string())
    }

    #[test]
    fn create_worktree_branches_amm_run_from_the_base_branch() {
        let repo = init_repo();
        let (wt, sha) = create_worktree(&repo, "r000001", "main").unwrap();
        assert_eq!(Some(sha), rev(&repo, "main"));
        assert_eq!(wt, repo.join(".claude/worktrees/amm-r000001"));
        assert_eq!(rev(&repo, "refs/heads/amm/r000001"), rev(&repo, "main"));
        let info = crate::git::worktree_info(&wt).unwrap();
        assert_eq!(info.branch.as_deref(), Some("amm/r000001"));
        assert!(wt.join("README.md").exists());
    }

    #[test]
    fn create_worktree_runs_no_planted_smudge_filter() {
        let repo = init_repo();
        let marker = tmp_dir().join("ran");
        std::fs::write(repo.join(".gitattributes"), "* filter=x\n").unwrap();
        let g = |a: &[&str]| assert_eq!(git(&repo, a).unwrap().code, 0, "{a:?}");
        g(&["add", ".gitattributes"]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "attrs"]);
        g(&["config", "filter.x.smudge", &format!("touch {}; cat", marker.display())]);
        g(&["config", "filter.x.required", "true"]);
        let (wt, _) = create_worktree(&repo, "r000005", "main").unwrap();
        assert!(wt.join("README.md").exists());
        assert!(!marker.exists(), "a planted smudge filter ran");
    }

    #[test]
    fn create_worktree_errors_on_a_missing_base_or_an_option_shaped_base() {
        let repo = init_repo();
        assert!(create_worktree(&repo, "r000002", "nope").is_err());
        assert!(create_worktree(&repo, "r000003", "--help").is_err());
        assert!(create_worktree(&tmp_dir(), "r000004", "main").is_err());
        assert_eq!(rev(&repo, "refs/heads/amm/r000002"), None);
        // An existing branch is refused and left alone.
        create_worktree(&repo, "r000006", "main").unwrap();
        assert!(create_worktree(&repo, "r000006", "main").is_err());
        assert!(rev(&repo, "refs/heads/amm/r000006").is_some());
        assert!(repo.join(".claude/worktrees/amm-r000006").exists());
    }

    #[test]
    fn remove_worker_worktree_removes_both_and_ignores_what_is_gone() {
        let repo = init_repo();
        let wt = add_worktree(&repo, "gone");
        std::fs::write(wt.join("dirty.txt"), "x").unwrap();
        remove_worker_worktree(&repo, &wt, "worktree-agent-gone").unwrap();
        assert!(!wt.exists());
        assert_eq!(rev(&repo, "refs/heads/worktree-agent-gone"), None);
        remove_worker_worktree(&repo, &wt, "worktree-agent-gone").unwrap();
    }

    struct Q {
        db: PathBuf,
        clock: crate::testutil::ManualClock,
        state: Arc<Mutex<Amm>>,
        repo: PathBuf,
        run: Run,
        iwt: PathBuf,
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn queue_with(test_cmd: &str) -> Q {
        let h = make_amm(DEFAULT_LIMITS);
        let repo = init_repo_with(&[("src/a.ts", "a\n"), ("c.txt", "1\n2\n3\n4\n5\n")]);
        let id = new_run_id();
        let (iwt, head) = create_worktree(&repo, &id, "main").unwrap();
        let mut amm = h.amm;
        let run = amm
            .start_run(StartRun {
                repo: repo.to_string_lossy().into_owned(),
                test_cmd: test_cmd.into(),
                base_branch: "main".into(),
                id: Some(id),
                integration_worktree: Some(iwt.to_string_lossy().into_owned()),
                integration_head: Some(head),
                setup_cmd: None,
            })
            .unwrap();
        Q { db: h.dir.join("amm.db"), clock: h.clock, state: Arc::new(Mutex::new(amm)), repo, run, iwt }
    }

    impl Q {
        fn amm(&self) -> MutexGuard<'_, Amm> {
            lock(&self.state)
        }
        fn add(&self, title: &str, scope: &str) -> Brief {
            self.add_scoped(title, &[scope])
        }
        fn add_scoped(&self, title: &str, scope: &[&str]) -> Brief {
            let input = BriefInput {
                title: title.into(),
                goal: "g".into(),
                scope: scope.iter().map(|s| s.to_string()).collect(),
                checks: vec![],
                tier: Tier::Light,
                max_turns: None,
            };
            self.amm().add_brief(&self.run.id, input, BriefState::Queued).unwrap()
        }
        /// Dispatches `b`, binds a fresh worker worktree, lets `work` change it, commits and passes the gate.
        fn run_worker(&self, b: &Brief, work: impl FnOnce(&Path)) -> String {
            self.amm().dispatch(&self.run.id).unwrap();
            let wt = add_worktree(&self.repo, &format!("q{:08x}", rand::random::<u32>()));
            let agent = format!("ag{:08x}", rand::random::<u32>());
            let w = self.amm().bind_blocking(&b.id, &agent, &wt).unwrap();
            work(&wt);
            let parent = sh(&wt, &["rev-parse", "HEAD"]);
            let sha = crate::git::commit_all(&wt, w.branch.as_deref().unwrap(), &parent, "work").unwrap();
            pin_gated(&wt, &b.id, &sha).unwrap();
            let mut amm = self.amm();
            let w = amm.mark_checking(&w, None).unwrap().unwrap();
            assert!(amm.complete(&w, Some(sha.clone())).unwrap());
            sha
        }
        fn brief(&self, id: &str) -> Brief {
            self.amm().brief(id).unwrap()
        }
        fn fresh_run(&self) -> Run {
            self.amm().run(&self.run.id).unwrap()
        }
        fn tip(&self) -> String {
            sh(&self.repo, &["rev-parse", &format!("refs/heads/{}", self.run.integration_branch)])
        }
        fn events(&self, kind: &str) -> Vec<AmmEvent> {
            self.amm().store.events(&Default::default()).unwrap().into_iter().filter(|e| e.kind == kind).collect()
        }
        async fn integrate(&self) {
            integrate(self.state.clone(), self.run.id.clone()).await;
        }
    }

    fn write(wt: &Path, file: &str, body: &str) {
        let p = wt.join(file);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    const SUITE: &str = "if [ -e broken ]; then echo suite-broke; exit 1; fi";

    #[tokio::test]
    async fn two_briefs_merge_in_seq_order_and_the_run_ends_done() {
        let q = queue_with(SUITE);
        let (b1, b2) = (q.add("one", "a/**"), q.add("two", "b/**"));
        let g1 = q.run_worker(&b1, |wt| write(wt, "a/x.ts", "1\n"));
        let wt1 = q.amm().store.all::<Worker>().into_iter().find(|w| w.brief_id == b1.id).unwrap().worktree.unwrap();
        let g2 = q.run_worker(&b2, |wt| write(wt, "b/y.ts", "2\n"));
        q.integrate().await;
        let log = sh(&q.repo, &["log", "--first-parent", "--format=%s|%an", &q.run.integration_branch]);
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[..2], [format!("amm: two ({})|amm", b2.id), format!("amm: one ({})|amm", b1.id)], "{log}");
        let tip = q.tip();
        assert_eq!(sh(&q.repo, &["rev-parse", &format!("{tip}^2")]), g2);
        assert_eq!(sh(&q.repo, &["rev-parse", &format!("{tip}^1^2")]), g1);
        let run = q.fresh_run();
        assert_eq!((run.state, run.merges, run.last_test_ok), (RunState::Done, 2, Some(true)));
        assert_eq!(run.integration_head, Some(tip));
        assert_eq!((q.brief(&b1.id).state, q.brief(&b2.id).state), (BriefState::Merged, BriefState::Merged));
        assert_eq!(q.events("run_done").len(), 1);
        assert!(!Path::new(&wt1).exists(), "the worker worktree was not removed");
        assert!(q.amm().merging.is_empty());
        assert_eq!(sh(&q.iwt, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn a_brief_that_breaks_the_tests_is_reset_requeued_then_escalated() {
        let q = queue_with(SUITE);
        let b = q.add("breaks", "broken");
        let before = q.tip();
        q.run_worker(&b, |wt| write(wt, "broken", "x\n"));
        q.integrate().await;
        assert_eq!(q.tip(), before);
        assert_eq!(q.fresh_run().integration_head, Some(before.clone()));
        assert!(!q.iwt.join("broken").exists());
        let b1 = q.brief(&b.id);
        assert_eq!((b1.state, b1.tier, b1.attempt, b1.integration_failures), (BriefState::Queued, Tier::Light, 2, 1));
        assert!(b1.notes.iter().any(|n| n.contains("suite-broke")), "{:?}", b1.notes);
        assert_eq!(q.fresh_run().last_test_ok, Some(false));
        assert_eq!(q.events("integration_failed").len(), 1);
        q.run_worker(&b, |wt| write(wt, "broken", "y\n"));
        q.integrate().await;
        let b2 = q.brief(&b.id);
        assert_eq!((b2.state, b2.tier, b2.integration_failures), (BriefState::Queued, Tier::Standard, 2));
        assert_eq!(q.events("escalate").len(), 1);
        assert_eq!(q.tip(), before);
    }

    #[tokio::test]
    async fn a_conflict_is_aborted_and_requeued_with_the_files() {
        let q = queue_with("true");
        let (b1, b2) = (q.add("one", "c.txt"), q.add("two", "c.txt"));
        q.run_worker(&b1, |wt| write(wt, "c.txt", "one\n"));
        q.run_worker(&b2, |wt| write(wt, "c.txt", "two\n"));
        let wt2 = q.amm().store.all::<Worker>().into_iter().find(|w| w.brief_id == b2.id).unwrap().worktree.unwrap();
        q.integrate().await;
        assert!(!Path::new(&wt2).exists(), "a requeued brief's old worktree must go");
        assert_eq!(rev(&q.repo, &pin_ref(&b2.id)), None);
        assert_eq!(q.brief(&b1.id).state, BriefState::Merged);
        let after = q.brief(&b2.id);
        assert_eq!(after.state, BriefState::Queued);
        assert!(after.notes.iter().any(|n| n.starts_with(CONFLICT_NOTE) && n.contains("c.txt")), "{:?}", after.notes);
        assert_eq!(q.events("conflict").len(), 1);
        assert_eq!(q.fresh_run().integration_head, Some(q.tip()));
        assert_eq!(sh(&q.iwt, &["status", "--porcelain"]), "");
        let merge_head = sh(&q.iwt, &["rev-parse", "--path-format=absolute", "--git-path", "MERGE_HEAD"]);
        assert!(!Path::new(&merge_head).exists(), "the merge was not aborted");
        assert_eq!(std::fs::read_to_string(q.iwt.join("c.txt")).unwrap(), "one\n");
        assert_ne!(q.fresh_run().state, RunState::Done);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_racing_integrations_merge_each_brief_once() {
        let q = queue_with("true");
        let (b1, b2) = (q.add("one", "a/**"), q.add("two", "b/**"));
        q.run_worker(&b1, |wt| write(wt, "a/x.ts", "1\n"));
        q.run_worker(&b2, |wt| write(wt, "b/y.ts", "2\n"));
        let (s1, s2) = (q.state.clone(), q.state.clone());
        let (r1, r2) = (q.run.id.clone(), q.run.id.clone());
        let (x, y) = tokio::join!(tokio::spawn(integrate(s1, r1)), tokio::spawn(integrate(s2, r2)));
        x.unwrap();
        y.unwrap();
        // A loser that saw the claim may leave work for the winner, never twice.
        q.integrate().await;
        let merges = sh(&q.repo, &["rev-list", "--merges", "--count", &q.run.integration_branch]);
        assert_eq!(merges, "2");
        assert_eq!(q.fresh_run().merges, 2);
        assert_eq!(q.events("merged").len(), 2);
    }

    #[tokio::test]
    async fn a_restart_with_a_brief_stuck_in_merging_recovers() {
        let q = queue_with("true");
        let b = q.add("one", "a/**");
        let g = q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        let head = q.fresh_run().integration_head.unwrap();
        // amm made the merge commit, then died before recording it.
        q.amm().brief_transition(&b.id, &[BriefState::Done], |x| x.state = BriefState::Merging).unwrap();
        sh(&q.iwt, &["merge", "-q", "--no-ff", "--no-edit", &g]);
        write(&q.iwt, "half.txt", "x\n");
        q.integrate().await;
        assert_eq!(q.brief(&b.id).state, BriefState::Merged);
        let tip = q.tip();
        assert_eq!(sh(&q.repo, &["rev-parse", &format!("{tip}^1")]), head);
        assert_eq!(sh(&q.repo, &["rev-list", "--merges", "--count", &q.run.integration_branch]), "1");
        assert_eq!(q.fresh_run().integration_head, Some(tip));
    }

    #[tokio::test]
    async fn a_user_commit_on_the_integration_branch_fails_the_run_closed() {
        let q = queue_with("true");
        let b = q.add("one", "a/**");
        q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        write(&q.iwt, "user.txt", "u\n");
        sh(&q.iwt, &["add", "."]);
        sh(&q.iwt, &["commit", "-qm", "user"]);
        let user = q.tip();
        q.integrate().await;
        assert_eq!(q.fresh_run().state, RunState::Failed);
        assert_eq!(q.events("run_failed").len(), 1);
        assert_eq!(q.brief(&b.id).state, BriefState::Done);
        assert_eq!(q.tip(), user, "the user's commit must stay");
        // A failed run never merges again.
        q.integrate().await;
        assert_eq!(q.tip(), user);
    }

    #[tokio::test]
    async fn tests_that_move_the_integration_branch_fail_the_run() {
        let q = queue_with("git -c user.email=a@b -c user.name=a commit -q --allow-empty -m sneaky");
        let b = q.add("one", "a/**");
        q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        q.integrate().await;
        assert_eq!(q.fresh_run().state, RunState::Failed);
        assert_ne!(q.brief(&b.id).state, BriefState::Merged);
    }

    #[tokio::test]
    async fn a_criss_cross_merge_cannot_change_another_brief_file() {
        let q = queue_with("true");
        let (b1, b2, evil) = (q.add("one", "lib/a.ts"), q.add("two", "lib/b.ts"), q.add("evil", "src/**"));
        let g1 = q.run_worker(&b1, |wt| write(wt, "lib/a.ts", "a\n"));
        let g2 = q.run_worker(&b2, |wt| write(wt, "lib/b.ts", "b\n"));
        q.integrate().await;
        assert_eq!(q.fresh_run().merges, 2);
        q.run_worker(&evil, |wt| {
            // Both gated commits become ancestors, with none of their changes: git's virtual merge base then
            // holds lib/a.ts and lib/b.ts, so merging would delete them.
            sh(wt, &["merge", "-q", "--no-edit", "-s", "ours", &g1]);
            sh(wt, &["merge", "-q", "--no-edit", "-s", "ours", &g2]);
            write(wt, "src/a.ts", "evil\n");
        });
        let before = q.tip();
        q.integrate().await;
        assert_eq!(q.tip(), before);
        assert_eq!(sh(&q.repo, &["show", &format!("{}:lib/a.ts", q.run.integration_branch)]), "a");
        assert_eq!(q.brief(&evil.id).state, BriefState::Failed);
        let v = q.events("scope_violation");
        assert_eq!(v.len(), 1);
        assert!(v[0].detail["files"].to_string().contains("lib/a.ts"), "{:?}", v[0].detail);
    }

    #[tokio::test]
    async fn a_failed_record_writes_neither_the_brief_nor_the_run() {
        let q = queue_with("true");
        let b = q.add("one", "a/**");
        q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        let head = q.fresh_run().integration_head;
        rusqlite::Connection::open(&q.db).unwrap().execute_batch("DROP TABLE runs").unwrap();
        q.integrate().await;
        assert_eq!(q.brief(&b.id).state, BriefState::Merging);
        assert_eq!((q.fresh_run().integration_head, q.fresh_run().merges), (head, 0));
    }

    #[tokio::test]
    async fn a_merge_driver_named_in_a_gated_commit_never_runs() {
        let q = queue_with("true");
        let marker = tmp_dir().join("ran");
        sh(&q.repo, &["config", "merge.x.driver", &format!("touch {}; false", marker.display())]);
        let (b1, b2) = (q.add("one", "c.txt"), q.add_scoped("two", &["c.txt", ".gitattributes"]));
        q.run_worker(&b1, |wt| write(wt, "c.txt", "one\n2\n3\n4\n5\n"));
        q.run_worker(&b2, |wt| {
            write(wt, ".gitattributes", "c.txt merge=x\n");
            write(wt, "c.txt", "1\n2\n3\n4\ntwo\n");
        });
        q.integrate().await;
        assert!(!marker.exists(), "the merge driver from the gated commit ran");
        assert_eq!(q.brief(&b2.id).state, BriefState::Merged);
        assert_eq!(sh(&q.repo, &["show", &format!("{}:c.txt", q.run.integration_branch)]), "one\n2\n3\n4\ntwo");
    }

    #[tokio::test]
    async fn tests_that_detach_head_fail_the_run_closed() {
        let q = queue_with("git checkout -q --detach");
        let b = q.add("one", "a/**");
        let before = q.tip();
        q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        q.integrate().await;
        assert_eq!(q.fresh_run().state, RunState::Failed);
        assert_eq!(q.tip(), before, "amm's merge must come back off the branch");
        assert_eq!(q.fresh_run().integration_head, Some(before));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancel_during_the_merge_takes_the_merge_back_off() {
        let q = queue_with("sleep 1");
        let b = q.add("one", "a/**");
        let before = q.tip();
        q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        let task = tokio::spawn(integrate(q.state.clone(), q.run.id.clone()));
        while q.brief(&b.id).state != BriefState::Merging {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        q.amm().cancel(&b.id).unwrap();
        task.await.unwrap();
        assert_eq!(q.brief(&b.id).state, BriefState::Cancelled);
        assert_eq!(q.tip(), before);
        assert_eq!(q.fresh_run().integration_head, Some(before));
        assert!(!q.iwt.join("a/x.ts").exists());
        assert_eq!(q.fresh_run().state, RunState::Active);
    }

    #[tokio::test]
    async fn a_brief_that_cannot_merge_backs_off_without_blocking_later_briefs_then_fails() {
        let q = queue_with("true");
        let (stuck, ok) = (q.add("stuck", "a/**"), q.add("ok", "b/**"));
        q.run_worker(&stuck, |wt| write(wt, "a/x.ts", "1\n"));
        q.run_worker(&ok, |wt| write(wt, "b/y.ts", "2\n"));
        // The gated commit is gone (no object, no pin).
        {
            let mut amm = q.amm();
            let w = amm.store.all::<Worker>().into_iter().find(|w| w.brief_id == stuck.id).unwrap();
            amm.store.put(&Worker { gated_sha: Some("0".repeat(40)), ..w }).unwrap();
        }
        q.integrate().await;
        assert_eq!(q.brief(&ok.id).state, BriefState::Merged, "a stuck brief must not block the queue");
        let s = q.brief(&stuck.id);
        assert_eq!((s.state, s.integration_errors), (BriefState::Done, 1));
        assert_eq!(q.events("integration_error").len(), 1);
        // Backing off: an immediate retry does nothing, and the sweep does not even start the queue.
        q.integrate().await;
        assert_eq!(q.brief(&stuck.id).integration_errors, 1);
        assert!(q.amm().runs_to_integrate().is_empty());
        q.clock.advance(10 * 60 * 1000);
        assert_eq!(q.amm().runs_to_integrate(), std::slice::from_ref(&q.run.id));
        q.integrate().await;
        assert_eq!(q.brief(&stuck.id).integration_errors, 2);
        assert_eq!(q.events("integration_error").len(), 1, "the same error is reported once");
        q.clock.advance(10 * 60 * 1000);
        q.integrate().await;
        let s = q.brief(&stuck.id);
        assert_eq!((s.state, s.integration_errors), (BriefState::Failed, 3));
        assert_eq!(q.events("integration_failed").len(), 1);
    }

    #[tokio::test]
    async fn the_gated_commit_is_pinned_until_the_brief_is_merged() {
        let q = queue_with("true");
        let b = q.add("one", "a/**");
        let g = q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        let pin = format!("refs/amm/gated/{}", b.id);
        assert_eq!(rev(&q.repo, &pin), Some(g.clone()));
        // A worker that moves the pin cannot change what is merged.
        sh(&q.repo, &["update-ref", &pin, &q.tip()]);
        q.integrate().await;
        assert_eq!(rev(&q.repo, &format!("{}^2", q.tip())), Some(g));
        assert_eq!(rev(&q.repo, &pin), None, "the pin outlived the merge");
    }

    #[tokio::test]
    async fn outcomes_for_a_brief_no_longer_merging_record_nothing() {
        let q = queue_with("true");
        let b = q.add("one", "a/**");
        q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        let w = q.amm().store.all::<Worker>().into_iter().find(|w| w.brief_id == b.id).unwrap();
        // Done briefs are not cancellable; force a settled state as a cancel during the merge leaves it.
        q.amm().brief_transition(&b.id, &[BriefState::Done], |x| x.state = BriefState::Cancelled).unwrap();
        let before = q.amm().store.events(&Default::default()).unwrap().len();
        for o in [
            Outcome::Conflict { files: vec!["c.txt".into()] },
            Outcome::ScopeViolation { files: vec!["x".into()] },
            Outcome::TestFailed { tail: "t".into() },
            Outcome::Error("e".into()),
        ] {
            assert!(!q.amm().record(&q.run.id, &b, &w, &o).unwrap());
        }
        assert_eq!(q.amm().store.events(&Default::default()).unwrap().len(), before);
        assert_eq!(q.brief(&b.id).state, BriefState::Cancelled);
    }

    #[test]
    fn the_merge_claim_is_released_when_the_queue_panics() {
        let state = Arc::new(Mutex::new(make_amm(DEFAULT_LIMITS).amm));
        lock(&state).merging.insert("r1".into());
        let st = state.clone();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _claim = Claim { state: st.clone(), run_id: "r1".into(), armed: true };
            let _held = lock(&st);
            panic!("boom");
        }));
        assert!(r.is_err());
        assert!(lock(&state).merging.is_empty());
    }

    #[tokio::test]
    async fn the_sweep_lists_runs_with_waiting_briefs_and_no_running_queue() {
        let q = queue_with("true");
        assert!(q.amm().runs_to_integrate().is_empty());
        let b = q.add("one", "a/**");
        q.run_worker(&b, |wt| write(wt, "a/x.ts", "1\n"));
        assert_eq!(q.amm().runs_to_integrate(), std::slice::from_ref(&q.run.id));
        q.amm().merging.insert(q.run.id.clone());
        assert!(q.amm().runs_to_integrate().is_empty());
    }

}
