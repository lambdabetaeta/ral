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
use crate::process::cancel::DurableRoot;
use std::fs;

/// An uncancelled scope: every test here exercises the stamp itself, while
/// the cancel path has its own test below.
fn live() -> Scope {
    Scope::clone(&DurableRoot::default())
}

/// A syntactically valid capability SID no real token carries: stamps for
/// it are inert, which is exactly the persistence story under test.
const CAP_SID: &str = "S-1-15-3-1024-1-2-3-4-5-6-7-8";

// The mask-shape invariants are `const _` asserts at the declarations
// above, not tests here, so drift fails the build rather than depending
// on someone running an unfiltered suite.

#[test]
fn ensure_local_canonical_prefix_accepts_local() {
    for local in [
        r"\\?\C:\tmp\foo",
        r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\foo",
        r"\\.\Volume{12345678-1234-1234-1234-123456789abc}\foo",
    ] {
        assert!(
            ensure_local_canonical_prefix(Path::new(local)).is_ok(),
            "{local}"
        );
    }
}

/// An allow-list: whatever is neither a verbatim drive path nor a volume
/// GUID path is refused, not let through for being no share.
#[test]
fn ensure_local_canonical_prefix_rejects_every_other_form() {
    for other in [
        r"C:\tmp\foo",
        r"\\.\C:\tmp\foo",
        r"\\.\pipe\x",
        r"\??\C:\tmp\foo",
        r"\\?\C:",
        r"\tmp\foo",
        r"tmp\foo",
        "",
    ] {
        let err = ensure_local_canonical_prefix(Path::new(other));
        assert!(
            matches!(err, Err(DaclError::NotCanonicalLocal(_))),
            "{other}: {err:?}"
        );
    }
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

/// `path`'s real path: the spelling a frozen allowance holds.
pub(super) fn real(path: &Path) -> PathBuf {
    crate::path::canon::canonicalise_strict(path).unwrap()
}

/// Stamp `kind` for `sid` on `canonical` through its pin, as `confine` does.
pub(super) fn grant(
    canonical: &Path,
    sid: &str,
    kind: GrantKind,
    cancel: &Scope,
) -> Result<(), DaclError> {
    let pin = FrozenPin::open(canonical, kind)?.expect("the grant target is on disk");
    ensure_frozen_grant(&pin, sid, kind, cancel)
}

#[test]
fn ensure_grant_stamps_the_ace_and_records_the_witness() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = real(td.path());
        assert!(!capability_ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));
        grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
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
        let canonical = real(td.path());
        grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
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
        let canonical = real(td.path());
        grant(&canonical, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
        assert!(!low_integrity_label_present(&canonical));

        let file = td.path().join("f.txt");
        fs::write(&file, b"").unwrap();
        let file = real(&file);
        grant(&file, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        assert!(!low_integrity_label_present(&file));
    });
}

/// A wall or an Esc must be able to end the stamp, and end it having
/// touched nothing.
#[test]
fn a_cancel_in_force_stops_the_stamp_before_it_applies() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = real(td.path());
        let cancel = DurableRoot::default().worker();
        cancel.cancel(CancelCause::TimedOut);

        let err = grant(&canonical, CAP_SID, GrantKind::ReadWrite, &cancel);
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
        let canonical = real(td.path());
        grant(&canonical, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
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
        let canonical = real(&root);
        grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        let canonical = real(&root);
        assert!(stamp_recorded(&stamp_key(&canonical, GrantKind::ReadWrite)));
        assert!(!capability_ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));

        grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
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
        let canonical = real(td.path());
        grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
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
    for kind in [GrantKind::ReadWrite, GrantKind::ReadOnly] {
        let name = fs_capability_name(a, kind);
        assert!(name.starts_with("ral.fs."));
        assert!(
            name.encode_utf16().count() <= 64,
            "stay inside the most conservative name cap: {name}"
        );
    }
}
