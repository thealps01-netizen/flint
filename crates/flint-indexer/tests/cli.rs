//! The CLI without administrator rights: a full volume scan must say why it
//! cannot run instead of failing silently.

#[cfg(windows)]
#[test]
fn scan_without_admin_says_why() {
    if flint_indexer::volume::is_elevated() {
        eprintln!("skipped: the test process is elevated");
        return;
    }
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_flint-indexer"))
        .args(["scan", "C:"])
        .output()
        .expect("run flint-indexer");
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(flint_indexer::volume::NEEDS_ADMIN), "{err}");
}
