//! "Put everything back as it was" for the two kinds a byte-for-byte copy
//! cannot carry on its own: a symbolic link, and a file's permission bits.
//!
//! Both go through the public API only — a checkpoint before, a job that
//! wrecks them, and one whole-folder undo after.

#![cfg(unix)]
// A test binary, not the ral shell: the clippy.toml invariants target
// ral-core's fs discipline, not a harness that builds a folder to undo.
#![allow(clippy::disallowed_methods)]

use std::os::unix::fs::PermissionsExt;
use synod::workspace::manifest::{EntryKind, Manifest};
use synod::workspace::{HistoryStore, Moment, Resolution, undo_all};

/// A restore's promise is bytes and mode; the stat clock a quick capture
/// leans on is not part of it, so "the folder matches its baseline" must
/// look past mtime.
fn without_mtimes(mut manifest: Manifest) -> Manifest {
    for kind in manifest.entries.values_mut() {
        if let EntryKind::File { mtime_ns, .. } = kind {
            *mtime_ns = 0;
        }
    }
    manifest
}

#[test]
fn an_undo_puts_back_a_link_as_a_link_and_a_file_at_its_recorded_mode() {
    let dir = std::env::temp_dir().join(format!("synod-undo-kinds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let folder = dir.join("Admissions");
    std::fs::create_dir_all(&folder).expect("fixture");
    let script = folder.join("run.sh");
    std::fs::write(&script, b"#!/bin/sh\nexit 0\n").expect("fixture");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("fixture");
    // Dangling on purpose: a link is its target text, never followed.
    std::os::unix::fs::symlink("reports/2026.pdf", folder.join("latest")).expect("fixture");

    let store = HistoryStore::open_at(&dir.join("history")).expect("store opens");
    let before = store.capture(&folder, Moment::Before).expect("baseline");

    // The job: the link goes, and the script comes back a plain 0644 file.
    std::fs::remove_file(folder.join("latest")).expect("job");
    std::fs::remove_file(&script).expect("job");
    std::fs::write(&script, b"#!/bin/sh\nexit 0\n").expect("job");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).expect("job");
    store.capture(&folder, Moment::After).expect("after");

    undo_all(&store, &folder, Resolution::KeepCurrent).expect("undoes");

    let link = folder.join("latest");
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("the link is back")
            .file_type()
            .is_symlink(),
        "a deleted link must come back a link, not a file holding its target"
    );
    assert_eq!(
        std::fs::read_link(&link).expect("the link reads"),
        std::path::Path::new("reports/2026.pdf")
    );
    assert_eq!(
        std::fs::metadata(&script)
            .expect("the script reads")
            .permissions()
            .mode()
            & 0o777,
        0o755,
        "a restored file must carry the mode it was recorded with, not the copy's"
    );
    assert_eq!(
        without_mtimes(Manifest::of_folder(&folder).expect("rereads")),
        without_mtimes(before.manifest),
        "the folder must match its baseline exactly, bytes and mode"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
