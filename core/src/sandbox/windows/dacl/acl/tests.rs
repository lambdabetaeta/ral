// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::super::GrantKind;
use super::super::tests::{pin, real, with_scoped_state_dir};
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
        let pin = pin(&real(td.path()), GrantKind::ReadOnly).unwrap().unwrap();
        assert_eq!(
            compute_appcontainer_effective_access(pin.target()).unwrap(),
            0
        );
    });
}
