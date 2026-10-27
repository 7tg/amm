use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub cmd: String,
    pub ok: bool,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub output: String,
}

const OUTPUT_TAIL: usize = 4000;
/// A char is at most 4 bytes, so this many raw bytes always cover the last OUTPUT_TAIL chars.
const KEEP_BYTES: usize = OUTPUT_TAIL * 4;
const GRACE: Duration = Duration::from_millis(200);

fn kill_group(pid: u32) {
    // SAFETY: plain syscall; a negative pid addresses the process group. ESRCH (already gone) is ignored.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(OUTPUT_TAIL)).collect()
}

fn spawn_reader<R: Read + Send + 'static>(mut r: R, buf: Arc<Mutex<Vec<u8>>>, done: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut b = buf.lock().unwrap();
            b.extend_from_slice(&chunk[..n]);
            if b.len() > KEEP_BYTES {
                let cut = b.len() - KEEP_BYTES;
                b.drain(..cut);
            }
        }
        let _ = done.send(());
    });
}

/// Runs `cmd` with `sh -c` in its own process group. Blocking; async callers use `spawn_blocking`.
/// On exit the whole group is killed (a check must not leave daemons behind) and the pipes get a 200 ms grace
/// before we stop waiting for them (a `setsid` grandchild may hold them open).
pub fn run_check(cmd: &str, cwd: &Path, timeout_ms: u64) -> CheckResult {
    let fail = |output: String| CheckResult { cmd: cmd.to_string(), ok: false, code: None, timed_out: false, output };
    let mut child = match Command::new("sh")
        .args(["-c", cmd])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return fail(e.to_string()),
    };
    let pid = child.id();
    let buf = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    spawn_reader(child.stdout.take().unwrap(), buf.clone(), tx.clone());
    spawn_reader(child.stderr.take().unwrap(), buf.clone(), tx);

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if !timed_out && Instant::now() >= deadline {
            // The flag is set only if the child had not exited by the time the timer fired.
            match child.try_wait() {
                Ok(Some(st)) => break Ok(st),
                Ok(None) => {
                    timed_out = true;
                    kill_group(pid);
                }
                Err(e) => break Err(e),
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    kill_group(pid);
    // Wait for both pipes to close, but no longer than the grace period.
    let grace_end = Instant::now() + GRACE;
    for _ in 0..2 {
        let left = grace_end.saturating_duration_since(Instant::now());
        if rx.recv_timeout(left).is_err() {
            break;
        }
    }
    let output = tail(&buf.lock().unwrap());
    match status {
        Ok(st) => {
            let code = st.code();
            CheckResult { cmd: cmd.to_string(), ok: code == Some(0) && !timed_out, code, timed_out, output }
        }
        Err(e) => fail(e.to_string()),
    }
}

/// Runs checks in order and stops at the first failure. Each check gets at most `timeout_ms` and what is left of
/// `budget_ms` for the whole list (`u64::MAX` for no budget); once the budget is used up the next check counts as
/// timed out without running.
pub fn run_checks(cmds: &[String], cwd: &Path, timeout_ms: u64, budget_ms: u64) -> Vec<CheckResult> {
    let start = Instant::now();
    let mut results = Vec::new();
    for cmd in cmds {
        let used = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let left = budget_ms.saturating_sub(used);
        let r = if left == 0 {
            CheckResult {
                cmd: cmd.clone(),
                ok: false,
                code: None,
                timed_out: true,
                output: "gate time budget used up before this check ran".into(),
            }
        } else {
            run_check(cmd, cwd, timeout_ms.min(left))
        };
        let ok = r.ok;
        results.push(r);
        if !ok {
            break;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> &'static Path {
        Path::new("/tmp")
    }
    fn cmds(c: &[&str]) -> Vec<String> {
        c.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn run_check_reports_exit_code_and_output_tail() {
        let ok = run_check("echo fine", tmp(), 5_000);
        assert!(ok.ok);
        assert!(ok.output.contains("fine"));
        let bad = run_check("echo broken >&2; exit 3", tmp(), 5_000);
        assert_eq!((bad.ok, bad.code), (false, Some(3)));
        assert!(bad.output.contains("broken"));
    }

    #[test]
    fn run_check_kills_commands_that_exceed_the_timeout() {
        let t = Instant::now();
        let r = run_check("sleep 5; echo x", tmp(), 200);
        assert!(t.elapsed() < Duration::from_millis(1500));
        assert_eq!((r.ok, r.timed_out), (false, true));
    }

    #[test]
    fn run_checks_stops_at_the_first_failure() {
        let rs = run_checks(&cmds(&["true", "false", "echo never"]), tmp(), 5_000, u64::MAX);
        assert_eq!(rs.iter().map(|r| r.ok).collect::<Vec<_>>(), [true, false]);
    }

    #[test]
    fn run_check_does_not_wait_for_background_grandchildren() {
        if Command::new("sh").args(["-c", "command -v setsid"]).output().map(|o| !o.status.success()).unwrap_or(true) {
            return; // skip: no setsid
        }
        let t = Instant::now();
        let r = run_check("setsid sleep 2 & echo hi", tmp(), 5_000);
        assert!(r.ok);
        assert!(r.output.contains("hi"));
        assert!(t.elapsed() < Duration::from_millis(1000));
    }

    #[test]
    fn run_check_with_a_missing_cwd_resolves_ok_false() {
        let r = run_check("true", Path::new("/nonexistent-amm-dir"), 1_000);
        assert_eq!((r.ok, r.code, r.timed_out), (false, None, false));
        assert!(!r.output.is_empty());
    }

    #[test]
    fn run_check_truncates_output_to_the_last_4000_chars() {
        let r = run_check("head -c 9000 /dev/zero | tr \"\\0\" a; echo END", tmp(), 5_000);
        assert_eq!(r.output.chars().count(), 4000);
        assert!(r.output.ends_with("END\n"));
    }

    #[test]
    fn run_checks_enforces_an_overall_budget() {
        let t0 = Instant::now();
        let rs = run_checks(&cmds(&["sleep 0.3", "echo second", "echo third"]), tmp(), 5_000, 100);
        assert!(t0.elapsed() < Duration::from_millis(2000));
        assert!(rs[0].timed_out);
        assert!(!rs.last().unwrap().ok);
    }

    #[test]
    fn run_checks_marks_checks_after_the_budget_as_timed_out_without_running_them() {
        let rs = run_checks(&cmds(&["sleep 0.15", "echo second"]), tmp(), 5_000, 100);
        assert!(rs.last().unwrap().timed_out);
        assert!(!rs.last().unwrap().output.contains("second"));
    }

    #[test]
    fn run_checks_with_an_exhausted_budget_does_not_run_the_check() {
        let rs = run_checks(&cmds(&["echo ran"]), tmp(), 5_000, 0);
        assert_eq!(rs.len(), 1);
        assert!(rs[0].timed_out && !rs[0].ok);
        assert!(!rs[0].output.contains("ran\n"));
    }

    #[test]
    fn run_check_kills_the_process_group_on_exit_and_is_not_timed_out() {
        let dir = crate::testutil::tmp_dir();
        let marker = dir.join("late");
        let cmd = format!("(sleep 0.5; touch {}) & exit 0", marker.display());
        let r = run_check(&cmd, tmp(), 5_000);
        assert!(r.ok && !r.timed_out);
        std::thread::sleep(Duration::from_millis(900));
        assert!(!marker.exists(), "background child survived the check");
    }
}
