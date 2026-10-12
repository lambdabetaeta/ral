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
use super::*;
use std::path::PathBuf;
#[test]
fn mutex_name_is_deterministic_and_local_prefix() {
    let p = PathBuf::from(r"C:\tmp\foo");
    let n1 = mutex_name_for(&p);
    let n2 = mutex_name_for(&p);
    assert_eq!(n1, n2);
    assert!(n1.starts_with("Local\\ral.sandbox.dacl."));
    assert_eq!(n1.len(), "Local\\ral.sandbox.dacl.".len() + 16);
}

#[test]
fn mutex_name_case_insensitive() {
    let a = mutex_name_for(&PathBuf::from(r"C:\Tmp\Foo"));
    let b = mutex_name_for(&PathBuf::from(r"c:\tmp\foo"));
    assert_eq!(a, b);
}
