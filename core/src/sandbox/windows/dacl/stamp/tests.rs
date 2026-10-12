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
        assert!(!stamp_recorded("deny|\\\\?\\c:\\work\\a"));
    });
}
