#![allow(
    clippy::disallowed_methods,
    reason = "[test] Windows junction fixtures and real ACL regressions"
)]

use super::super::ensure_frozen_grant;
use super::super::tests::{junction, labelled, mklink, pin, real, with_scoped_state_dir};
use super::*;
use crate::path::RealPath;
use crate::process::cancel::DurableRoot;
use std::path::PathBuf;

const CAP_SID: &str = "S-1-15-3-1024-1-2-3-4-5-6-7-8";

fn is_redirected(r: &Result<Option<FrozenPin>, DaclError>) -> bool {
    matches!(r, Err(DaclError::Redirected { .. }))
}

#[test]
fn a_real_directory_is_held() {
    let td = tempfile::tempdir().unwrap();
    let pin = pin(&real(td.path()), GrantKind::ReadOnly).unwrap();
    assert!(pin.is_some());
}

/// `path`, a verbatim drive path, as its drive's administrative share names
/// it: `\\localhost\C$\…`.
fn through_share(path: &Path) -> PathBuf {
    let text = path.to_str().unwrap();
    let (drive, rest) = text
        .strip_prefix(r"\\?\")
        .and_then(|local| local.split_once(':'))
        .expect("a verbatim drive path");
    PathBuf::from(format!(r"\\localhost\{drive}${rest}"))
}

#[test]
fn a_share_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let shared = through_share(&real(td.path()));
    let refused = pin(&shared, GrantKind::ReadOnly);
    assert!(
        matches!(refused, Err(DaclError::NotLocal(_))),
        "{:?}",
        refused.err()
    );
}

#[test]
fn an_absent_path_is_not_an_error() {
    let td = tempfile::tempdir().unwrap();
    let absent = real(td.path()).join("absent").join("deeper");
    assert!(pin(&absent, GrantKind::ReadOnly).unwrap().is_none());
}

#[test]
fn a_junction_on_the_leaf_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let root = real(td.path());
    let (outside, link) = (root.join("outside"), root.join("link"));
    fs::create_dir(&outside).unwrap();
    junction(&link, &outside);
    assert!(is_redirected(&pin(&link, GrantKind::ReadOnly)));
}

#[test]
fn a_junction_on_an_ancestor_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let root = real(td.path());
    let (outside, link) = (root.join("outside"), root.join("link"));
    fs::create_dir_all(outside.join("inner")).unwrap();
    junction(&link, &outside);
    assert!(is_redirected(&pin(
        &link.join("inner"),
        GrantKind::ReadWrite
    )));
}

/// How many times the race below turns `a` into a junction and back.
const FLIPS: usize = 100;

/// Turn `a` from an empty directory into a junction to `outside` and back.
/// The walk racing it holds `a` open at moments, so Windows may refuse any
/// step, `a` pending deletion or its name still taken; a refused step leaves
/// `a` as it is, for the next flip to start from.
fn flip(a: &Path, outside: &Path) {
    if fs::remove_dir(a).is_ok() && mklink(a, outside).status.success() {
        let _ = fs::remove_dir(a);
    }
    let _ = fs::create_dir(a);
}

#[test]
fn a_directory_converted_to_a_junction_while_its_path_is_opened_is_never_descended() {
    // `a` flips between an empty directory and a junction to `outside`, which
    // holds `b`; only a walk that followed the junction could reach `b`.  A
    // walk that meets `a` mid-flip may also fail closed, on a name pending
    // deletion: what it must never do is hold `b`.
    let td = tempfile::tempdir().unwrap();
    let root = real(td.path());
    let (a, outside) = (root.join("a"), root.join("outside"));
    fs::create_dir_all(outside.join("b")).unwrap();
    fs::create_dir(&a).unwrap();
    std::thread::scope(|s| {
        // Bounded on its own, and the walks end when it does: a panic on
        // either side leaves nothing for the scope to wait on forever.
        let flipper = s.spawn(|| {
            for _ in 0..FLIPS {
                flip(&a, &outside);
            }
        });
        // Walk first, test after: a flipper that finishes before this thread
        // is scheduled still meets one walk, so the test is never vacuous.
        loop {
            let pinned = pin(&a.join("b"), GrantKind::ReadOnly);
            assert!(
                !matches!(pinned, Ok(Some(_))),
                "descended through a converted component"
            );
            if flipper.is_finished() {
                break;
            }
        }
    });
}

#[test]
fn a_grant_since_replaced_by_a_junction_is_refused_on_the_next_launch() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let root = real(td.path());
        let (granted, outside) = (root.join("granted"), root.join("outside"));
        fs::create_dir_all(&granted).unwrap();
        fs::create_dir(&outside).unwrap();
        {
            let pin = pin(&granted, GrantKind::ReadOnly).unwrap().unwrap();
            ensure_frozen_grant(
                &pin,
                CAP_SID,
                GrantKind::ReadOnly,
                &DurableRoot::default().worker(),
            )
            .unwrap();
        }
        fs::remove_dir(&granted).unwrap();
        junction(&granted, &outside);
        let err = pin_all(&[RealPath::assumed(&granted)], GrantKind::ReadOnly).err();
        assert!(matches!(err, Some(DaclError::Redirected { .. })), "{err:?}");
    });
}

#[test]
fn a_read_write_grant_stamps_dacl_and_label_through_the_held_handle() {
    with_scoped_state_dir(|| {
        let td = tempfile::tempdir().unwrap();
        let root = real(td.path());
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        let pin = pin(&root, GrantKind::ReadWrite).unwrap().unwrap();
        ensure_frozen_grant(
            &pin,
            CAP_SID,
            GrantKind::ReadWrite,
            &DurableRoot::default().worker(),
        )
        .unwrap();
        assert!(labelled(&sub));
        assert!(
            super::super::scan_explicit_aces_for_sid(pin.target(), CAP_SID)
                .unwrap()
                .iter()
                .any(|a| a.access_mask & super::super::RW_MASK == super::super::RW_MASK)
        );
    });
}

#[test]
fn the_redirect_refusal_names_the_link_and_how_to_grant_its_target() {
    let e = DaclError::Redirected {
        granted: PathBuf::from(r"\\?\C:\rustup"),
        at: PathBuf::from(r"\\?\C:\rustup"),
        target: Some(PathBuf::from(r"\\?\D:\x")),
    };
    let text = e.to_string();
    assert!(text.contains("is now a link to"), "{text}");
    assert!(
        text.contains("an allowance names what it named when granted"),
        "{text}"
    );
}

#[test]
fn a_volume_mount_refusal_asks_for_a_drive_letter() {
    let at = PathBuf::from(r"C:\mnt\data");
    let e = |target| DaclError::Redirected {
        granted: at.clone(),
        at: at.clone(),
        target: Some(PathBuf::from(target)),
    };
    for t in [r"\\?\Volume{1234}\", r"\??\volume{1234}\"] {
        assert!(e(t).to_string().contains("drive letter"));
    }
    for t in [r"\\?\D:\x", r"\\?\VolumeX", r"D:\Volume{1}"] {
        assert!(e(t).to_string().contains("is now a link to"));
    }
}
