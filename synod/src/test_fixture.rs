//! Test fixtures shared across the crate: a private, self-cleaning temp
//! directory per test, and one already holding a granted folder and its
//! history store, side by side.

use crate::workspace::history::HistoryStore;
use std::path::PathBuf;

/// A private directory for one test, named after it so a failed run
/// leaves readable wreckage.
pub(crate) fn workshop(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synod-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp workshop");
    std::fs::canonicalize(&dir).expect("temp workshop resolves")
}

/// A workshop holding a granted folder and a history store, side by side.
pub(crate) fn granted_workshop(tag: &str) -> (PathBuf, PathBuf, HistoryStore) {
    let dir = workshop(tag);
    let folder = dir.join("Admissions");
    std::fs::create_dir_all(&folder).expect("temp workshop");
    let store = HistoryStore::open_at(&dir.join("history")).expect("store opens");
    (dir, folder, store)
}
