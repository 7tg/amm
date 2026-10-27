use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
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
const GRACE: Duration = Duration::from_millis(200);
const POLL_SLICE_MS: i32 = 20;
/// After a timeout kill, how long we wait for the leader to die before giving up on it.
const KILL_WAIT: Duration = Duration::from_secs(2);

fn kill_group(pid: u32) {
    // SAFETY: plain syscall; a negative pid addresses the process group. ESRCH (already gone) is ignored.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn kill_pid(pid: u32) {
    // SAFETY: plain syscall on our own child; it is not reaped yet, so the pid cannot have been reused.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// True once the child has exited, without reaping it (WNOWAIT), so its pid and pgid stay ours until we kill the group.
fn has_exited(pid: u32) -> bool {
    // SAFETY: waitid fills a zeroed siginfo_t; with WNOHANG it leaves si_pid 0 while the child still runs.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT);
        // On error (e.g. already reaped) fall through to try_wait, which reports the real state.
        r != 0 || info.si_pid() != 0
    }
}

/// One output pipe: its fd (None once closed) and bytes of an incomplete trailing UTF-8 sequence.
struct Stream {
    fd: Option<OwnedFd>,
    pending: Vec<u8>,
}

impl Stream {
    fn new(fd: OwnedFd) -> Stream {
        // SAFETY: fcntl on a valid owned fd.
        unsafe {
            let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        Stream { fd: Some(fd), pending: Vec::new() }
    }

    /// Reads whatever is available into `tail`; closes the fd on EOF or a hard error.
    fn drain(&mut self, tail: &mut String) {
        let Some(fd) = &self.fd else { return };
        let mut chunk = [0u8; 8192];
        loop {
            // SAFETY: reading into a local buffer of the stated length.
            let n = unsafe { libc::read(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                self.pending.extend_from_slice(&chunk[..n as usize]);
                decode(&mut self.pending, tail, false);
            } else if n == 0 {
                break;
            } else {
                match std::io::Error::last_os_error().kind() {
                    std::io::ErrorKind::WouldBlock => return,
                    std::io::ErrorKind::Interrupted => continue,
                    _ => break,
                }
            }
        }
        decode(&mut self.pending, tail, true);
        self.fd = None;
    }
}

/// Moves valid UTF-8 from `pending` to `tail`, keeping an incomplete trailing sequence unless `flush`.
/// Invalid bytes become U+FFFD. `tail` is then capped to its last OUTPUT_TAIL chars.
fn decode(pending: &mut Vec<u8>, tail: &mut String, flush: bool) {
    let mut start = 0;
    loop {
        match std::str::from_utf8(&pending[start..]) {
            Ok(s) => {
                tail.push_str(s);
                start = pending.len();
                break;
            }
            Err(e) => {
                let ok_end = start + e.valid_up_to();
                tail.push_str(std::str::from_utf8(&pending[start..ok_end]).unwrap_or(""));
                match e.error_len() {
                    Some(n) => {
                        tail.push('\u{fffd}');
                        start = ok_end + n;
                    }
                    None => {
                        start = ok_end;
                        if flush {
                            tail.push('\u{fffd}');
                            start = pending.len();
                        }
                        break;
                    }
                }
            }
        }
    }
    pending.drain(..start);
    let n = tail.chars().count();
    if n > OUTPUT_TAIL {
        let cut = tail.char_indices().nth(n - OUTPUT_TAIL).map_or(0, |(i, _)| i);
        tail.drain(..cut);
    }
}

/// Waits up to `slice_ms` for output on the open pipes and drains them.
fn pump(streams: &mut [Stream; 2], tail: &mut String, slice_ms: i32) {
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let mut idx = Vec::new();
    for (i, st) in streams.iter().enumerate() {
        if let Some(fd) = &st.fd {
            fds.push(libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            idx.push(i);
        }
    }
    if fds.is_empty() {
        std::thread::sleep(Duration::from_millis(slice_ms.max(0) as u64));
        return;
    }
    // SAFETY: fds is a valid array of pollfd of the given length. EINTR just yields no events.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, slice_ms) };
    if n > 0 {
        for (k, pfd) in fds.iter().enumerate() {
            if pfd.revents != 0 {
                streams[idx[k]].drain(tail);
            }
        }
    }
}

/// Runs `cmd` with `sh -c` in its own process group. Blocking; async callers use `spawn_blocking`.
/// The calling thread polls both pipes. On exit the whole group is killed (a check must not leave daemons
/// behind) and the pipes get a 200 ms grace before both fds are dropped, so a `setsid` grandchild that still
/// holds the write end gets EPIPE and nothing leaks.
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
    let mut streams = [
        Stream::new(OwnedFd::from(child.stdout.take().expect("piped stdout"))),
        Stream::new(OwnedFd::from(child.stderr.take().expect("piped stderr"))),
    ];
    let mut tail = String::new();

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let mut kill_at: Option<Instant> = None;
    let mut gave_up = false;
    let status = loop {
        if has_exited(pid) {
            // Kill the group while the zombie leader still pins the pgid, and only then reap.
            kill_group(pid);
            break child.wait();
        }
        let now = Instant::now();
        if !timed_out && now >= deadline {
            // The flag is set only if the child had not exited by the time the timer fired.
            if has_exited(pid) {
                continue;
            }
            timed_out = true;
            kill_at = Some(now);
            kill_group(pid);
            // The leader may have left its own group (setsid); kill it directly too.
            kill_pid(pid);
        }
        if let Some(k) = kill_at
            && now.duration_since(k) >= KILL_WAIT
        {
            gave_up = true;
            break Ok(std::process::ExitStatus::default());
        }
        let slice = if timed_out {
            POLL_SLICE_MS
        } else {
            (deadline.saturating_duration_since(now).as_millis().min(POLL_SLICE_MS as u128) as i32).max(1)
        };
        pump(&mut streams, &mut tail, slice);
    };
    if gave_up {
        // Never leave a zombie behind: reap in the background once it finally dies.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    } else {
        kill_group(pid);
    }
    let grace_end = Instant::now() + GRACE;
    while streams.iter().any(|s| s.fd.is_some()) {
        let left = grace_end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        pump(&mut streams, &mut tail, left.as_millis().min(POLL_SLICE_MS as u128).max(1) as i32);
    }
    for st in &mut streams {
        if st.fd.take().is_some() {
            decode(&mut st.pending, &mut tail, true);
        }
    }
    match status {
        Ok(_) if gave_up => CheckResult { cmd: cmd.to_string(), ok: false, code: None, timed_out: true, output: tail },
        Ok(st) => {
            let code = st.code();
            CheckResult { cmd: cmd.to_string(), ok: code == Some(0) && !timed_out, code, timed_out, output: tail }
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

    #[test]
    fn run_check_decodes_a_utf8_char_split_across_reads_and_streams() {
        let r = run_check("printf '\\303'; printf x >&2; sleep .1; printf '\\251'", tmp(), 5_000);
        assert!(r.output.contains('\u{e9}'), "{:?}", r.output);
        assert!(r.output.contains('x'));
        assert!(!r.output.contains('\u{fffd}'), "{:?}", r.output);
    }

    #[test]
    fn run_check_replaces_genuinely_invalid_bytes() {
        let r = run_check("printf 'a\\377b'", tmp(), 5_000);
        assert_eq!(r.output, "a\u{fffd}b");
    }

    #[test]
    fn run_check_returns_promptly_when_the_timeout_hits_a_stubborn_child() {
        if Command::new("sh").args(["-c", "command -v setsid"]).output().map(|o| !o.status.success()).unwrap_or(true) {
            return;
        }
        let t = Instant::now();
        let r = run_check("setsid sh -c 'trap \"\" TERM; sleep 30' & wait", tmp(), 200);
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert_eq!((r.ok, r.timed_out), (false, true));
    }
}
