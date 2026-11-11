// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::*;
use std::path::PathBuf;
#[test]
fn mutex_name_case_insensitive() {
    let a = mutex_name_for(&PathBuf::from(r"C:\Tmp\Foo"));
    let b = mutex_name_for(&PathBuf::from(r"c:\tmp\foo"));
    assert_eq!(a, b);
}
