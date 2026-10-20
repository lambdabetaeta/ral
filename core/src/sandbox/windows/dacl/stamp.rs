// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::ledger::{ensure_ledger_dir, ledger_dir, write_bytes_atomic};
use super::{DaclError, GrantKind, PathMutexGuard};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The stamp store: the completion witness for [`super::ensure_frozen_grant`],
/// one [`stamp_key`] entry per propagation that ran to the end. No upstream
/// counterpart — mxc restores its stamps, so completion never needs
/// witnessing across sessions.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StampStore {
    #[serde(default)]
    stamps: BTreeSet<String>,
}

/// Keyed on the canonical spelling exactly as [`super::fs_capability_name`] is, and
/// unfolded for the same case-sensitive-directory reason: a folded witness
/// would let one object stand for another's stamp.  The path mutex guarding
/// it folds, by design (`super::mutex`'s `mutex_name_for`): a lock errs the
/// other way.
#[allow(
    clippy::disallowed_methods,
    reason = "native: the canonical spelling the OS returned, kept as-is as a key"
)]
pub(super) fn stamp_key(canonical: &Path, kind: GrantKind) -> String {
    format!("{}|{}", kind.tag(), canonical.to_string_lossy())
}

/// [`stamp_key`]'s counterpart for the mandatory-label stamp — a distinct tag
/// so the label's witness can never collide with a DACL grant's, even though
/// both key off the same canonical path.
#[allow(
    clippy::disallowed_methods,
    reason = "native: the canonical spelling the OS returned, kept as-is as a key"
)]
pub(super) fn label_stamp_key(canonical: &Path) -> String {
    format!("label|{}", canonical.to_string_lossy())
}

/// The stamp store's file name within [`ledger_dir`] — named once so the
/// orphan sweep can recognize and skip it by identity rather than by a second
/// spelling that could drift from this one.
pub(super) const STAMP_STORE_FILE_NAME: &str = "stamps.json";

fn stamp_store_path() -> PathBuf {
    ledger_dir().join(STAMP_STORE_FILE_NAME)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-stamp-read] Reads the persistent stamp-store witness deciding whether a grant's propagation already ran. Sandbox infrastructure, not model data I/O."
)]
fn read_stamp_store_bytes(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(path)
}

/// Missing or unreadable reads as "not recorded": the cost of a false no is
/// one redundant — idempotent — propagation, whose record then rewrites the
/// store whole.
pub(super) fn stamp_recorded(key: &str) -> bool {
    let Ok(bytes) = read_stamp_store_bytes(&stamp_store_path()) else {
        return false;
    };
    serde_json::from_slice::<StampStore>(&bytes).is_ok_and(|s| s.stamps.contains(key))
}

/// Read-merge-write under the store's own path mutex, so two sessions
/// recording different stamps never drop each other's. A corrupt store parses
/// as empty and is rewritten — entries lost that way cost redundant restamps,
/// never authority.
pub(super) fn record_stamp(key: &str) -> Result<(), DaclError> {
    update_stamp(key, true)
}

pub(super) fn update_stamp(key: &str, complete: bool) -> Result<(), DaclError> {
    let path = ensure_ledger_dir()?.join(STAMP_STORE_FILE_NAME);
    let _guard = PathMutexGuard::acquire(&path)?;
    let mut store: StampStore = match read_stamp_store_bytes(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => StampStore::default(),
        Err(e) => return Err(DaclError::LedgerIo(e)),
    };
    let changed = if complete {
        store.stamps.insert(key.to_string())
    } else {
        store.stamps.remove(key)
    };
    if !changed {
        return Ok(());
    }
    let json = serde_json::to_vec_pretty(&store)
        .map_err(|e| DaclError::LedgerParse(format!("serialize stamp store: {e}")))?;
    write_bytes_atomic(&path, &json)
}

#[cfg(test)]
mod tests;
