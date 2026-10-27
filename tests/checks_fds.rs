#![cfg(target_os = "linux")]
//! Own process so no other test's fds are counted.
use std::path::Path;
use std::time::{Duration, Instant};

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn run_check_does_not_leak_fds_or_threads() {
    let has_setsid = std::process::Command::new("sh")
        .args(["-c", "command -v setsid"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let cmd = if has_setsid { "setsid sleep 1 & echo hi" } else { "echo hi" };
    // warm up lazily-opened fds
    amm::checks::run_check(cmd, Path::new("/tmp"), 5_000);
    let before = fd_count();
    let t = Instant::now();
    for _ in 0..20 {
        let r = amm::checks::run_check(cmd, Path::new("/tmp"), 5_000);
        assert!(r.ok && r.output.contains("hi"));
    }
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(fd_count(), before, "fds leaked");
}
