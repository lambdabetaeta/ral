// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::ledger::ledger_dir;
use super::{DaclError, GrantKind, PathMutexGuard};
use crate::own::OwnError;
use crate::path::Visibility;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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
pub(super) fn stamp_key(name: &str, kind: GrantKind) -> String {
    format!("{}|{name}", kind.tag())
}

/// [`stamp_key`]'s counterpart for the mandatory-label stamp — a distinct tag
/// so the label's witness can never collide with a DACL grant's, even though
/// both key off the same canonical path.
pub(super) fn label_stamp_key(name: &str) -> String {
    format!("label|{name}")
}

/// The stamp store's file name within [`ledger_dir`] — named once so the
/// orphan sweep can recognize and skip it by identity rather than by a second
/// spelling that could drift from this one.
pub(super) const STAMP_STORE_FILE_NAME: &str = "stamps.json";

/// Missing, unreadable or corrupt reads as "not recorded": the cost of a false
/// no is one redundant — idempotent — propagation, whose record then rewrites
/// the store whole.
pub(super) fn stamp_recorded(key: &str) -> bool {
    ledger_dir()
        .ok()
        .and_then(|dir| dir.load::<StampStore>(STAMP_STORE_FILE_NAME).ok().flatten())
        .is_some_and(|s| s.stamps.contains(key))
}

/// Read-merge-write under the store's own path mutex, so two sessions
/// recording different stamps never drop each other's. A corrupt store reads
/// as empty and is rewritten — entries lost that way cost redundant restamps,
/// never authority.
pub(super) fn record_stamp(key: &str) -> Result<(), DaclError> {
    update_stamp(key, true)
}

pub(super) fn update_stamp(key: &str, complete: bool) -> Result<(), DaclError> {
    let dir = ledger_dir()?;
    let _guard = PathMutexGuard::acquire(&dir.path().join(STAMP_STORE_FILE_NAME))?;
    let mut store: StampStore = match dir.load(STAMP_STORE_FILE_NAME) {
        Ok(found) => found.unwrap_or_default(),
        Err(OwnError::Json { .. }) => StampStore::default(),
        Err(e) => return Err(e.into()),
    };
    let changed = if complete {
        store.stamps.insert(key.to_string())
    } else {
        store.stamps.remove(key)
    };
    if !changed {
        return Ok(());
    }
    Ok(dir.store(STAMP_STORE_FILE_NAME, &store, Visibility::Public)?)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
