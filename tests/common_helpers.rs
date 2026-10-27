mod common;
#[test]
fn helpers_work() {
    let r = common::init_repo();
    let w = common::add_worktree(&r, "z");
    assert!(w.exists());
}
