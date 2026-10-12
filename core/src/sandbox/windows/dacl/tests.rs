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
use super::acl::{AclKind, INHERITED_ACE_FLAG, OwnedSid, Security};
use super::*;

/// An uncancelled scope: every test here exercises the stamp itself, while
/// the cancel path has its own test below.
fn live() -> CancelScope {
    CancelScope::default()
}

/// A syntactically valid capability SID no real token carries: stamps for
/// it are inert, which is exactly the persistence story under test.
const CAP_SID: &str = "S-1-15-3-1024-1-2-3-4-5-6-7-8";

// The mask-shape invariants are `const _` asserts at the declarations
// above, not tests here, so drift fails the build rather than depending
// on someone running an unfiltered suite.

#[test]
fn ensure_local_canonical_prefix_accepts_local() {
    assert!(ensure_local_canonical_prefix(Path::new(r"\\?\C:\tmp\foo")).is_ok());
    assert!(
        ensure_local_canonical_prefix(Path::new(
            r"\\.\Volume{12345678-1234-1234-1234-123456789abc}\foo"
        ))
        .is_ok()
    );
    assert!(ensure_local_canonical_prefix(Path::new(r"\\.\C:\tmp\foo")).is_ok());
    assert!(ensure_local_canonical_prefix(Path::new(r"C:\tmp\foo")).is_ok());
}

#[test]
fn ensure_local_canonical_prefix_rejects_unc_namespace() {
    let err = ensure_local_canonical_prefix(Path::new(r"\\?\UNC\server\share\foo"));
    assert!(matches!(err, Err(DaclError::NetworkPathRejected(_))));
    let err = ensure_local_canonical_prefix(Path::new(r"\\?\unc\server\share\foo"));
    assert!(matches!(err, Err(DaclError::NetworkPathRejected(_))));
}

#[test]
fn ensure_local_canonical_prefix_rejects_raw_unc() {
    let err = ensure_local_canonical_prefix(Path::new(r"\\server\share\foo"));
    assert!(matches!(err, Err(DaclError::NetworkPathRejected(_))));
}

// ---------------- integration tests -----------------
//
// Real-ACL integration tests, each under its own XDG_STATE_HOME. The
// crate-wide env guard also covers the session e2e test, which reads
// this variable mid-operation without scoping it.

pub(super) fn with_scoped_state_dir<R>(f: impl FnOnce() -> R) -> R {
    let scratch = tempfile::tempdir().unwrap();
    let state_home = scratch.path().to_string_lossy().into_owned();
    crate::test_env::with_var("XDG_STATE_HOME", Some(&state_home), f)
}

#[test]
fn ensure_grant_stamps_the_ace_and_records_the_witness() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = canonicalize_grant_target(td.path()).unwrap();
        assert!(!capability_ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));
        ensure_fs_grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        assert!(capability_ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));
        assert!(stamp_recorded(&stamp_key(&canonical, GrantKind::ReadWrite)));
    });
}

/// The `Low` label must reach subdirectories and never files: Windows
/// caps a process at its image file's label, so a labeled file would run
/// every host-built executable under the tree at `Low`.
#[test]
fn a_read_write_label_reaches_directories_and_never_files() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let before = td.path().join("before.exe");
        let sub = td.path().join("sub");
        fs::write(&before, b"").unwrap();
        fs::create_dir(&sub).unwrap();
        let canonical = canonicalize_grant_target(td.path()).unwrap();
        ensure_fs_grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        let after = td.path().join("after.exe");
        fs::write(&after, b"").unwrap();

        assert!(low_integrity_label_present(&canonical));
        assert!(low_integrity_label_present(&sub));
        assert!(!low_integrity_label_present(&before));
        assert!(!low_integrity_label_present(&after));
    });
}

#[test]
fn only_a_read_write_directory_grant_takes_the_label() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = canonicalize_grant_target(td.path()).unwrap();
        ensure_fs_grant(&canonical, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
        assert!(!low_integrity_label_present(&canonical));

        let file = td.path().join("f.txt");
        fs::write(&file, b"").unwrap();
        let file = canonicalize_grant_target(&file).unwrap();
        ensure_fs_grant(&file, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        assert!(!low_integrity_label_present(&file));
    });
}

/// A wall or an Esc must be able to end the stamp, and end it having
/// touched nothing.
#[test]
fn a_cancel_in_force_stops_the_stamp_before_it_applies() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = canonicalize_grant_target(td.path()).unwrap();
        let cancel = CancelScope::default();
        cancel.cancel(CancelCause::TimedOut);

        let err = ensure_fs_grant(&canonical, CAP_SID, GrantKind::ReadWrite, &cancel);
        assert!(
            matches!(err, Err(DaclError::Cancelled(CancelCause::TimedOut))),
            "the poll must report the cause it saw, not a grant failure"
        );
        assert!(
            !stamp_recorded(&stamp_key(&canonical, GrantKind::ReadWrite)),
            "a cancel read before the apply leaves no witness behind"
        );
    });
}

#[test]
fn a_read_only_stamp_does_not_satisfy_a_read_write_probe() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = canonicalize_grant_target(td.path()).unwrap();
        ensure_fs_grant(&canonical, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
        assert!(capability_ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadOnly,
            true
        ));
        assert!(
            !capability_ace_present(&canonical, CAP_SID, GrantKind::ReadWrite, true),
            "RO_MASK must not cover a read-write probe"
        );
        assert!(!stamp_recorded(&stamp_key(
            &canonical,
            GrantKind::ReadWrite
        )));
    });
}

/// The self-healing loop: a granted root deleted and recreated keeps its
/// stamp-store witness but fails the DACL probe, so the next grant
/// re-stamps rather than trusting the stale record.
#[test]
fn a_recreated_root_fails_the_probe_and_is_restamped() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let canonical = canonicalize_grant_target(&root).unwrap();
        ensure_fs_grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        let canonical = canonicalize_grant_target(&root).unwrap();
        assert!(stamp_recorded(&stamp_key(&canonical, GrantKind::ReadWrite)));
        assert!(!capability_ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));

        ensure_fs_grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        assert!(capability_ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));
    });
}

#[test]
fn inheritance_propagation_applies_without_error() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let sub = td.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("file.txt"), b"x").unwrap();
        let canonical = canonicalize_grant_target(td.path()).unwrap();
        ensure_fs_grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
    });
}

#[test]
fn nonexistent_path_errors_cleanly() {
    let err = canonicalize_grant_target(Path::new(r"C:\__definitely_not_a_real_path__\xyzzy"));
    assert!(matches!(err, Err(DaclError::PathNotFound(_))));
}

#[test]
fn network_path_rejected_e2e() {
    with_scoped_state_dir(|| {
        let err = canonicalize_grant_target(Path::new(r"\\someserver\share\foo"));
        // Which variant depends on how the host resolves an unreachable
        // server name; all three mean "never silently succeeded". The
        // unit tests above pin the classification deterministically.
        match &err {
            Err(
                DaclError::NetworkPathRejected(_)
                | DaclError::PathNotFound(_)
                | DaclError::Win32 { .. },
            ) => {}
            other => panic!(
                "expected NetworkPathRejected | PathNotFound | Win32 for UNC path, got: {other:?}"
            ),
        }
    });
}

/// Every allow/deny ACE for `sid_str` as `(AceType byte, inherited?)` in
/// ACL order — inherited ones included, unlike
/// [`scan_explicit_aces_for_sid`] — so a test can assert the ordering.
pub(super) fn sid_ace_sequence(path: &Path, sid_str: &str) -> Vec<(u8, bool)> {
    let sid = OwnedSid::parse(sid_str).unwrap();
    let security = Security::named(path, AclKind::Dacl).unwrap();
    security
        .aces()
        .map(Result::unwrap)
        .filter_map(|ace| {
            ace.kind()?;
            ace.mask_for(&sid)?;
            Some((ace.0.AceType, ace.0.AceFlags & INHERITED_ACE_FLAG != 0))
        })
        .collect()
}

/// The property a token relies on when it carries both a grant's and a
/// deny's capability: canonical order puts the explicit deny ahead of the
/// allow inherited from the enclosing grant, whoever the trustees are —
/// here one SID plays both so a single scan sees the whole sequence.
#[test]
fn deny_nested_in_grant_precedes_the_inherited_allow() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let parent = canonicalize_grant_target(td.path()).unwrap();
        let child = td.path().join("cred");
        std::fs::create_dir(&child).unwrap();
        let child = canonicalize_grant_target(&child).unwrap();

        ensure_fs_grant(&parent, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        ensure_fs_grant(&child, CAP_SID, GrantKind::Deny, &live()).unwrap();

        let explicit = scan_explicit_aces_for_sid(&child, CAP_SID).unwrap();
        assert_eq!(explicit.len(), 1, "one explicit ACE expected: {explicit:?}");
        assert_eq!(explicit[0].ace_type, AceType::Deny);
        assert!(stamp_recorded(&stamp_key(&child, GrantKind::Deny)));

        let seq = sid_ace_sequence(&child, CAP_SID);
        let deny_idx = seq
            .iter()
            .position(|(t, inh)| *t == 0x01 && !*inh)
            .expect("explicit deny present in DACL");
        if let Some(allow_idx) = seq.iter().position(|(t, _)| *t == 0x00) {
            assert!(deny_idx < allow_idx, "deny must precede allow: {seq:?}");
        }
    });
}

#[test]
fn capability_name_is_deterministic_kind_and_path_keyed() {
    let a = Path::new(r"\\?\C:\Work\Proj");
    let a_case = Path::new(r"\\?\c:\work\proj");
    let b = Path::new(r"\\?\C:\Work\Other");
    assert_eq!(
        fs_capability_name(a, GrantKind::ReadWrite),
        fs_capability_name(a, GrantKind::ReadWrite),
        "one spelling, one name"
    );
    assert_ne!(
        fs_capability_name(a, GrantKind::ReadWrite),
        fs_capability_name(a_case, GrantKind::ReadWrite),
        "case variants are distinct names: canonicalisation unifies the \
         spellings of one object, and an NTFS case-sensitive directory \
         really does hold two"
    );
    assert_ne!(
        fs_capability_name(a, GrantKind::ReadWrite),
        fs_capability_name(a, GrantKind::ReadOnly)
    );
    assert_ne!(
        fs_capability_name(a, GrantKind::ReadWrite),
        fs_capability_name(b, GrantKind::ReadWrite)
    );
    for kind in [GrantKind::ReadWrite, GrantKind::ReadOnly, GrantKind::Deny] {
        let name = fs_capability_name(a, kind);
        assert!(name.starts_with("ral.fs."));
        assert!(
            name.encode_utf16().count() <= 64,
            "stay inside the most conservative name cap: {name}"
        );
    }
}
