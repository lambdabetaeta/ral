// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::*;
use crate::process::cancel::DurableRoot;
use std::fs;
use std::path::Path;
use std::process::Command;
use windows_sys::Win32::Security::SID_AND_ATTRIBUTES;
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;

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

// ---------------- integration tests -----------------
//
// Real-ACL integration tests, each under its own XDG_STATE_HOME. The
// crate-wide env guard also covers the session e2e test, which reads
// this variable mid-operation without scoping it.

pub(super) fn with_scoped_state_dir<R>(f: impl FnOnce() -> R) -> R {
    let scratch = tempfile::tempdir().unwrap();
    let state_home = scratch.path().to_string_lossy().into_owned();
    crate::host::env::test_support::with_var("XDG_STATE_HOME", Some(&state_home), f)
}

/// `path`'s real path: the spelling a frozen allowance holds.
pub(super) fn real(path: &Path) -> PathBuf {
    crate::test_helper::real(path).as_path().to_path_buf()
}

/// `path` as the text a stamp is keyed by.
fn text(path: &Path) -> &str {
    crate::path::utf8(path).unwrap()
}

/// `mklink /J link target`, as `cmd` reports it.
pub(super) fn mklink(link: &Path, target: &Path) -> std::process::Output {
    Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap()
}

pub(super) fn junction(link: &Path, target: &Path) {
    let output = mklink(link, target);
    assert!(output.status.success(), "create junction: {output:?}");
}

/// The frozen allowance at `path`'s position now, as `pin_all` opens one:
/// held by the walk with every link refused, `None` where nothing is there.
pub(super) fn pin(path: &Path, kind: GrantKind) -> Result<Option<FrozenPin>, DaclError> {
    FrozenPin::open(&crate::test_helper::place(path), kind)
}

/// Stamp `kind` for `sid` on `canonical` through its pin, as `confine` does.
pub(super) fn grant(
    canonical: &Path,
    sid: &str,
    kind: GrantKind,
    cancel: &Scope,
) -> Result<(), DaclError> {
    let pin = pin(canonical, kind)?.expect("the grant target is on disk");
    ensure_frozen_grant(&pin, sid, kind, cancel)
}

/// Whether `path` carries the `Low` label, read through its pin.
pub(super) fn labelled(path: &Path) -> bool {
    let pin = pin(path, GrantKind::ReadOnly).unwrap().unwrap();
    low_integrity_label_present(pin.target())
}

/// [`capability_ace_present`] on `path`, read through its pin.
fn ace_present(path: &Path, sid: &str, kind: GrantKind, inheritable: bool) -> bool {
    let pin = pin(path, kind).unwrap().unwrap();
    capability_ace_present(pin.target(), sid, kind, inheritable)
}

#[test]
fn ensure_grant_stamps_the_ace_and_records_the_witness() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = real(td.path());
        assert!(!ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));
        grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        assert!(ace_present(&canonical, CAP_SID, GrantKind::ReadWrite, true));
        assert!(stamp_recorded(&stamp_key(
            text(&canonical),
            GrantKind::ReadWrite
        )));
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

        assert!(labelled(&canonical));
        assert!(labelled(&sub));
        assert!(!labelled(&before));
        assert!(!labelled(&after));
    });
}

#[test]
fn only_a_read_write_directory_grant_takes_the_label() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = real(td.path());
        grant(&canonical, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
        assert!(!labelled(&canonical));

        let file = td.path().join("f.txt");
        fs::write(&file, b"").unwrap();
        let file = real(&file);
        grant(&file, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        assert!(!labelled(&file));
    });
}

/// `ALL APPLICATION PACKAGES`, which every `AppContainer` token carries.
const ALL_APPLICATION_PACKAGES: &str = "S-1-15-2-1";

/// A read-write directory every sandbox may already write needs no capability
/// ACE, and still takes the `Low` label: no ACE admits a create without it.
#[test]
fn a_read_write_directory_every_sandbox_may_write_takes_the_label_and_no_ace() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let canonical = real(td.path());
        let covering = pin(&canonical, GrantKind::ReadWrite).unwrap().unwrap();
        apply_explicit_ace(covering.target(), ALL_APPLICATION_PACKAGES, RW_MASK, true).unwrap();
        assert!(!labelled(&canonical));

        grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        assert!(labelled(&canonical));
        assert!(!ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));
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
            !stamp_recorded(&stamp_key(text(&canonical), GrantKind::ReadWrite)),
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
        assert!(ace_present(&canonical, CAP_SID, GrantKind::ReadOnly, true));
        assert!(
            !ace_present(&canonical, CAP_SID, GrantKind::ReadWrite, true),
            "RO_MASK must not cover a read-write probe"
        );
        assert!(!stamp_recorded(&stamp_key(
            text(&canonical),
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
        assert!(stamp_recorded(&stamp_key(
            text(&canonical),
            GrantKind::ReadWrite
        )));
        assert!(!ace_present(
            &canonical,
            CAP_SID,
            GrantKind::ReadWrite,
            true
        ));

        grant(&canonical, CAP_SID, GrantKind::ReadWrite, &live()).unwrap();
        assert!(ace_present(&canonical, CAP_SID, GrantKind::ReadWrite, true));
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
    let a = r"\\?\C:\Work\Proj";
    let a_case = r"\\?\c:\work\proj";
    let b = r"\\?\C:\Work\Other";
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

/// Run `check` against the access of a token that carries `CAP_SID` as its
/// one capability, in a package nothing is stamped for.
fn with_token_access<R>(check: impl FnOnce(&ContainerAccess) -> R) -> R {
    let package = acl::OwnedSid::parse("S-1-15-2-1-2-3-4-5-6-7").unwrap();
    let capability = acl::OwnedSid::parse(CAP_SID).unwrap();
    let capabilities = [SID_AND_ATTRIBUTES {
        Sid: capability.as_psid(),
        Attributes: SE_GROUP_ENABLED as u32,
    }];
    check(&ContainerAccess::of(package.as_psid(), &capabilities).unwrap())
}

/// The position `ensure_unreachable` reports as reachable, if it does.
fn reachable(excluded: &Path, access: &ContainerAccess) -> Option<PathBuf> {
    match ensure_unreachable(&crate::test_helper::place(excluded), access, &live()) {
        Err(DaclError::Reachable { at, .. }) => Some(at),
        other => {
            other.unwrap();
            None
        }
    }
}

/// A file excluded by its exact name is held and read like a directory is: a
/// descriptor that already admits the token is reported, not skipped.
#[test]
fn an_exact_file_exclusion_is_verified_and_reported() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let file = real(td.path()).join("secret.env");
        fs::write(&file, b"x").unwrap();
        grant(&file, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
        with_token_access(|access| {
            let at = reachable(&file, access).expect("the file admits the token");
            assert!(at.ends_with("secret.env"), "{at:?}");
        });
    });
}

/// A link beneath an exclusion is checked as itself and never followed: what
/// it leads to is reachable by the token and is not what was excluded, while
/// a real directory beneath it is descended into.
#[test]
fn a_junction_and_a_symlink_beneath_an_exclusion_are_not_followed() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let root = real(td.path());
        let (excluded, outside) = (root.join("excluded"), root.join("outside"));
        let deep = excluded.join("sub").join("deep.txt");
        fs::create_dir_all(deep.parent().unwrap()).unwrap();
        fs::write(&deep, b"x").unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("open.txt"), b"x").unwrap();
        grant(&outside, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
        junction(&excluded.join("junction"), &outside);
        std::os::windows::fs::symlink_dir(&outside, excluded.join("symlink")).unwrap();
        with_token_access(|access| {
            assert_eq!(reachable(&excluded, access), None);
            grant(&deep, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
            let at = reachable(&excluded, access).expect("the real directory is descended into");
            assert!(at.ends_with("deep.txt"), "{at:?}");
        });
    });
}

#[test]
fn carving_stamps_the_examined_object_after_its_name_is_replaced() {
    use crate::authority::sandbox::carve::carve;
    use crate::path::{Excluded, LexicalPath, ObjectId, Walked};

    with_scoped_state_dir(|| {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::test_helper::real(tmp.path());
        let secret = root.as_path().join("secret");
        let ok = root.as_path().join("ok");
        let saved = root.as_path().join("examined");
        fs::write(&secret, "secret").unwrap();
        fs::write(&ok, "ok").unwrap();
        let excluded = Excluded::walk(&LexicalPath::absolute(&secret).unwrap()).unwrap();
        let carving = carve(&[root], &[&excluded]).unwrap();
        let [examined] = carving.held.as_slice() else {
            panic!("only ok is granted")
        };
        let identity = examined.trace().leaf_id().unwrap();
        let between = || {
            fs::rename(&ok, &saved).unwrap();
            fs::rename(&secret, &ok).unwrap();
        };
        between();
        let pinned = FrozenPin::held(examined, GrantKind::ReadOnly).unwrap();
        let file = fs::File::from(pinned.target().handle.try_clone().unwrap());
        assert_eq!(ObjectId::of(&file).unwrap(), identity);
        assert_ne!(
            identity,
            crate::test_helper::located(&ok).trace().leaf_id().unwrap()
        );
        ensure_frozen_grant(&pinned, CAP_SID, GrantKind::ReadOnly, &live()).unwrap();
        with_token_access(|access| {
            assert_eq!(
                reachable(&saved, access),
                Some(crate::test_helper::real(&saved).as_path().to_path_buf())
            );
            assert_eq!(reachable(&secret, access), None);
            assert_eq!(reachable(&ok, access), None);
        });
    });
}
