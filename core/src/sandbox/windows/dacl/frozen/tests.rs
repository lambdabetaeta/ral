#![allow(
    clippy::disallowed_methods,
    reason = "[test] Windows junction fixtures and real ACL regressions"
)]

use super::super::tests::with_scoped_state_dir;
use super::super::{canonicalize_grant_target, ensure_frozen_grant};
use super::*;
use crate::process::cancel::CancelScope;
use std::process::Command;

const CAP_SID: &str = "S-1-15-3-1024-1-2-3-4-5-6-7-8";

fn junction(link: &Path, target: &Path) {
    let output = Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(output.status.success(), "create junction: {output:?}");
}

fn real(path: &Path) -> PathBuf {
    canonicalize_grant_target(path).unwrap()
}

fn is_redirected(r: &Result<Option<FrozenPin>, DaclError>) -> bool {
    matches!(r, Err(DaclError::Redirected { .. }))
}

#[test]
fn a_real_directory_is_held() {
    let td = tempfile::tempdir().unwrap();
    let pin = FrozenPin::open(&real(td.path()), GrantKind::ReadOnly).unwrap();
    assert!(pin.is_some());
}

#[test]
fn an_absent_path_is_not_an_error() {
    let td = tempfile::tempdir().unwrap();
    let absent = real(td.path()).join("absent").join("deeper");
    assert!(
        FrozenPin::open(&absent, GrantKind::ReadOnly)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_reparse_tag_is_redirecting_exactly_when_it_is_a_name_surrogate() {
    // Symlink and mount point carry the bit; a cloud-files tag does not.
    for (tag, redirects) in [
        (0xA000_000C_u32, true),
        (0xA000_0003, true),
        (0x9000_001A, false),
    ] {
        assert_eq!(tag & REPARSE_TAG_NAME_SURROGATE != 0, redirects, "{tag:#x}");
    }
}

#[test]
fn a_junction_on_the_leaf_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let root = real(td.path());
    let (outside, link) = (root.join("outside"), root.join("link"));
    fs::create_dir(&outside).unwrap();
    junction(&link, &outside);
    assert!(is_redirected(&FrozenPin::open(&link, GrantKind::ReadOnly)));
}

#[test]
fn a_junction_on_an_ancestor_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let root = real(td.path());
    let (outside, link) = (root.join("outside"), root.join("link"));
    fs::create_dir_all(outside.join("inner")).unwrap();
    junction(&link, &outside);
    assert!(is_redirected(&FrozenPin::open(
        &link.join("inner"),
        GrantKind::ReadWrite
    )));
}

#[test]
fn a_held_directory_cannot_be_swapped_for_a_junction() {
    let td = tempfile::tempdir().unwrap();
    let root = real(td.path());
    let (granted, outside) = (root.join("granted"), root.join("outside"));
    fs::create_dir_all(&granted).unwrap();
    fs::create_dir(&outside).unwrap();
    let _pin = FrozenPin::open(&granted, GrantKind::ReadOnly)
        .unwrap()
        .unwrap();
    assert!(
        fs::rename(&granted, root.join("moved")).is_err(),
        "a held component must refuse rename: that is what closes the window"
    );
    assert!(fs::remove_dir(&granted).is_err());
}

#[test]
fn a_directory_converted_to_a_junction_while_its_path_is_opened_is_never_descended() {
    // `a` flips between an empty directory and a junction to `outside`, which
    // holds `b`; only a walk that followed the junction could reach `b`.
    let td = tempfile::tempdir().unwrap();
    let root = real(td.path());
    let (a, outside) = (root.join("a"), root.join("outside"));
    fs::create_dir_all(outside.join("b")).unwrap();
    fs::create_dir(&a).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if fs::remove_dir(&a).is_ok() {
                    junction(&a, &outside);
                    fs::remove_dir(&a).unwrap();
                    fs::create_dir(&a).unwrap();
                }
            }
        });
        for _ in 0..300 {
            let pinned = FrozenPin::open(&a.join("b"), GrantKind::ReadOnly);
            assert!(
                matches!(pinned, Ok(None) | Err(DaclError::Redirected { .. })),
                "descended through a converted component"
            );
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
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
            let pin = FrozenPin::open(&granted, GrantKind::ReadOnly)
                .unwrap()
                .unwrap();
            ensure_frozen_grant(&pin, CAP_SID, GrantKind::ReadOnly, &CancelScope::default())
                .unwrap();
        }
        fs::remove_dir(&granted).unwrap();
        junction(&granted, &outside);
        let err = pin_all(&[granted], GrantKind::ReadOnly).err();
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
        let pin = FrozenPin::open(&root, GrantKind::ReadWrite)
            .unwrap()
            .unwrap();
        ensure_frozen_grant(&pin, CAP_SID, GrantKind::ReadWrite, &CancelScope::default()).unwrap();
        assert!(super::super::low_integrity_label_present(&sub));
        assert!(
            super::super::scan_explicit_aces_for_sid(&root, CAP_SID)
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
