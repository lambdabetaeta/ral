// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::super::tests::with_scoped_state_dir;
use super::*;
#[test]
fn stamp_store_roundtrip_and_merge() {
    with_scoped_state_dir(|| {
        let key_a = "rw|\\\\?\\c:\\work\\a";
        let key_b = "ro|\\\\?\\c:\\work\\b";
        assert!(!stamp_recorded(key_a));
        record_stamp(key_a).unwrap();
        record_stamp(key_b).unwrap();
        record_stamp(key_a).unwrap(); // idempotent
        assert!(stamp_recorded(key_a));
        assert!(stamp_recorded(key_b));
        assert!(!stamp_recorded("ro|\\\\?\\c:\\work\\a"));
    });
}

#[test]
fn corrupt_stamp_store_is_unrecorded_and_replaced_on_update() {
    with_scoped_state_dir(|| {
        let dir = ledger_dir().unwrap();
        for bad in [b"{".as_slice(), br#"{"stamps":42}"#] {
            std::fs::write(dir.path().join(STAMP_STORE_FILE_NAME), bad).unwrap();
            assert!(!stamp_recorded("fixture"));
            record_stamp("fixture").unwrap();
            assert!(stamp_recorded("fixture"));
        }
    });
}

#[test]
fn stamp_json_preserves_defaults_sets_unknowns_duplicates_and_sequences() {
    for bytes in [b"{}".as_slice(), br#"{"unknown":true}"#, b"[[]]"] {
        assert!(
            json::from_slice::<StampStore>(bytes)
                .unwrap()
                .stamps
                .is_empty()
        );
    }
    let store = json::from_slice::<StampStore>(br#"{"stamps":["a","a","b"]}"#).unwrap();
    assert_eq!(store.stamps.len(), 2);
    assert_eq!(
        json::to_vec_pretty(&store).unwrap(),
        serde_json::to_vec_pretty(&store).unwrap()
    );
    assert!(
        matches!(json::from_slice::<StampStore>(br#"{"stamps":[],"stamps":[]}"#), Err(crate::fact::JsonInputFailure::Decode(failure)) if *failure.cause == crate::fact::DecodeCause::DuplicateField("stamps".into()))
    );
}
