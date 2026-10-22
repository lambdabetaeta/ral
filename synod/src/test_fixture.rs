//! Test fixtures shared across the crate: a private temp directory per test.
#![allow(
    clippy::disallowed_methods,
    reason = "[test] test-fixture scratch directories, made and read only by \
              the tests themselves."
)]

/// A directory for one test, deleted when the returned guard falls.  Hold the
/// guard: binding only its path deletes the directory on the spot.
///
/// Made inside the *canonical* target directory rather than canonicalised
/// afterwards, because a grant is compared by path and a temp directory may
/// itself be a symlink (macOS's `$TMPDIR`).  Not the system temp directory:
/// on Linux that is `/tmp`, the guest scratch the office grant admits, so a
/// host path there would be readable by the grant.
pub(crate) fn workshop(tag: &str) -> tempfile::TempDir {
    // `CARGO_TARGET_TMPDIR` is only set for integration tests.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/synod-test");
    std::fs::create_dir_all(&root).expect("test workshop root");
    let root = std::fs::canonicalize(root).expect("canonical test workshop root");
    tempfile::Builder::new()
        .prefix(&format!("synod-{tag}-"))
        .tempdir_in(root)
        .expect("temp workshop")
}
