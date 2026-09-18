//! Test fixtures shared across the crate: a private temp directory per test.
#![allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: test-fixture scratch directories, made and read only by \
              the tests themselves."
)]

/// A directory for one test, deleted when the returned guard falls.  Hold the
/// guard: binding only its path deletes the directory on the spot.
///
/// Made inside the *canonical* temp directory rather than canonicalised
/// afterwards, because a grant is compared by path and macOS's `$TMPDIR` is
/// itself a symlink.
pub(crate) fn workshop(tag: &str) -> tempfile::TempDir {
    let root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
    tempfile::Builder::new()
        .prefix(&format!("synod-{tag}-"))
        .tempdir_in(root)
        .expect("temp workshop")
}
