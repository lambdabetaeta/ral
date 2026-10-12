// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
use super::super::tests::with_scoped_state_dir;
use super::super::{GrantKind, RW_MASK, canonicalize_grant_target, ensure_fs_grant};
use super::*;
#[test]
fn sid_parse_valid_and_invalid() {
    let ok = OwnedSid::parse("S-1-1-0");
    assert!(ok.is_ok(), "S-1-1-0 should parse: {:?}", ok.err());
    drop(ok);
    let err = OwnedSid::parse("not-a-sid");
    assert!(matches!(err, Err(DaclError::InvalidSid(_))));
}

#[test]
fn sid_parse_rejects_empty_and_oversized() {
    let empty = OwnedSid::parse("");
    assert!(matches!(empty, Err(DaclError::InvalidSid(_))));
    let huge = "S-1-".to_string() + &"1".repeat(MAX_SID_STRING_LEN);
    let err = OwnedSid::parse(&huge);
    assert!(matches!(err, Err(DaclError::InvalidSid(_))));
}

#[test]
fn effective_access_is_zero_on_fresh_dir() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let access = compute_appcontainer_effective_access(td.path()).unwrap();
        assert_eq!(access, 0);
    });
}

#[test]
fn filter_paths_needing_grant_drops_well_known_grant() {
    with_scoped_state_dir(|| {
        let td_grant = tempfile::tempdir().unwrap();
        let td_no_grant = tempfile::tempdir().unwrap();

        // Everyone is one of the well-known AC SIDs, so a grant for it is
        // exactly what the filter must recognize as already covering.
        let granted = canonicalize_grant_target(td_grant.path()).unwrap();
        ensure_fs_grant(
            &granted,
            "S-1-1-0",
            GrantKind::ReadWrite,
            &crate::process::cancel::CancelScope::default(),
        )
        .unwrap();

        let input = vec![
            td_grant.path().to_path_buf(),
            td_no_grant.path().to_path_buf(),
        ];
        let kept = filter_paths_needing_grant(input, RW_MASK);
        assert!(
            !kept.iter().any(|p| p == td_grant.path()),
            "already-granted path should be filtered out: kept={kept:?}"
        );
        assert!(
            kept.iter().any(|p| p == td_no_grant.path()),
            "non-granted path should survive the filter: kept={kept:?}"
        );
    });
}
