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
use super::super::tests::{grant, pin, real, with_scoped_state_dir};
use super::super::{GrantKind, RW_MASK, needing_grant, pin_all};
use super::*;
use crate::path::RealPath;
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
        let pin = pin(&real(td.path()), GrantKind::ReadOnly).unwrap().unwrap();
        assert_eq!(
            compute_appcontainer_effective_access(pin.target()).unwrap(),
            0
        );
    });
}

#[test]
fn needing_grant_drops_well_known_grant() {
    with_scoped_state_dir(|| {
        let td_grant = tempfile::tempdir().unwrap();
        let td_no_grant = tempfile::tempdir().unwrap();

        // Everyone is one of the well-known AC SIDs, so a grant for it is
        // exactly what the filter must recognize as already covering.
        let granted = real(td_grant.path());
        grant(
            &granted,
            "S-1-1-0",
            GrantKind::ReadWrite,
            &crate::process::cancel::DurableRoot::default().worker(),
        )
        .unwrap();

        let not_granted = real(td_no_grant.path());
        let pins = pin_all(
            &[RealPath::assumed(&granted), RealPath::assumed(&not_granted)],
            GrantKind::ReadWrite,
        )
        .unwrap();
        let kept: Vec<_> = needing_grant(pins, RW_MASK)
            .iter()
            .map(|p| p.path().to_path_buf())
            .collect();
        assert!(
            !kept.contains(&granted),
            "already-granted path should be filtered out: kept={kept:?}"
        );
        assert!(
            kept.contains(&not_granted),
            "non-granted path should survive the filter: kept={kept:?}"
        );
    });
}
