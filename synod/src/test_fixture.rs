//! Test fixtures shared across the crate: a private temp directory per test,
//! and one already holding a granted folder and its history store, side by
//! side.

use crate::workspace::history::HistoryStore;
use std::path::PathBuf;

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

/// A workshop holding a granted folder and a history store, side by side.
pub(crate) fn granted_workshop(tag: &str) -> (tempfile::TempDir, PathBuf, HistoryStore) {
    let dir = workshop(tag);
    let folder = dir.path().join("Admissions");
    std::fs::create_dir_all(&folder).expect("temp workshop");
    let store = HistoryStore::open_at(&dir.path().join("history")).expect("store opens");
    (dir, folder, store)
}
