//! The run's integration worktree. Plain blocking functions: async callers use `spawn_blocking`, never under the
//! `Amm` lock.
use crate::git::git;
use std::path::{Path, PathBuf};

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
/// path. Uses hardened git (no hooks, no fsmonitor, a timeout). Errors if the base does not resolve to a commit or
/// git fails.
pub fn create_worktree(repo: &Path, run_id: &str, base_branch: &str) -> Result<PathBuf, String> {
    let base = git(repo, &["rev-parse", "--verify", "--end-of-options", &format!("{base_branch}^{{commit}}")])?;
    let sha = base.stdout.trim();
    if base.code != 0 || sha.is_empty() {
        return Err(fail(&format!("base branch {base_branch} is not a commit"), &base.stderr));
    }
    let path = worktree_path(repo, run_id);
    let path_s = path.to_string_lossy();
    let branch = integration_branch(run_id);
    let r = git(repo, &["worktree", "add", "-q", "-b", &branch, "--end-of-options", &path_s, sha])?;
    if r.code != 0 {
        return Err(fail("could not create the integration worktree", &r.stderr));
    }
    path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{add_worktree, init_repo, tmp_dir};

    fn rev(dir: &Path, r: &str) -> Option<String> {
        let o = git(dir, &["rev-parse", "--verify", "-q", r]).unwrap();
        (o.code == 0).then(|| o.stdout.trim().to_string())
    }

    #[test]
    fn create_worktree_branches_amm_run_from_the_base_branch() {
        let repo = init_repo();
        let wt = create_worktree(&repo, "r000001", "main").unwrap();
        assert_eq!(wt, repo.join(".claude/worktrees/amm-r000001"));
        assert_eq!(rev(&repo, "refs/heads/amm/r000001"), rev(&repo, "main"));
        let info = crate::git::worktree_info(&wt).unwrap();
        assert_eq!(info.branch.as_deref(), Some("amm/r000001"));
        assert!(wt.join("README.md").exists());
    }

    #[test]
    fn create_worktree_errors_on_a_missing_base_or_an_option_shaped_base() {
        let repo = init_repo();
        assert!(create_worktree(&repo, "r000002", "nope").is_err());
        assert!(create_worktree(&repo, "r000003", "--help").is_err());
        assert!(create_worktree(&tmp_dir(), "r000004", "main").is_err());
        assert_eq!(rev(&repo, "refs/heads/amm/r000002"), None);
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
}
