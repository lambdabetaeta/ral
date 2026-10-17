#![allow(
    clippy::disallowed_methods,
    reason = "[test] Windows deny fixtures and real ACL regressions"
)]

use super::acl::wide;
use super::*;
use crate::process::cancel::DurableRoot;
use std::fs;
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::{
    GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    SE_DACL_PROTECTED,
};

const CAP_SID: &str = "S-1-15-3-1024-1-2-3-4-5-6-7-8";

pub(crate) const READABLE: &[&str] = &[
    "ok.txt",
    r"D\open.txt",
    r"D\protected\key.txt",
    r"D\protected\nested\key.txt",
    r"D\protected-file.txt",
    r"D\explicit\key.txt",
];

pub(crate) fn with_state(f: impl FnOnce()) {
    let state = tempfile::tempdir().unwrap();
    crate::test_env::with_var("XDG_STATE_HOME", Some(state.path().to_str().unwrap()), f);
}

pub(crate) struct DenyTree {
    root: tempfile::TempDir,
    pub(crate) allow: PathBuf,
    pub(crate) deny: PathBuf,
}

impl DenyTree {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let allow = root.path().join("A");
        fs::create_dir(&allow).unwrap();
        let allow = canonicalize_grant_target(&allow).unwrap();
        let tree = Self {
            deny: allow.join("D"),
            allow,
            root,
        };
        tree.populate();
        tree
    }

    pub(crate) fn cwd(&self) -> PathBuf {
        // cmd.exe needs the ordinary drive spelling, not a verbatim prefix.
        self.root.path().join("A")
    }

    fn populate(&self) {
        for relative in READABLE.iter().copied().chain([r"D\dark\key.txt"]) {
            let file = self.allow.join(relative);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, b"secret\r\n").unwrap();
        }
    }

    pub(crate) fn shape(&self) {
        for relative in ["protected", r"protected\nested", "protected-file.txt"] {
            icacls(&self.deny.join(relative), "/inheritance:d");
        }
        let dark = self.deny.join("dark");
        // Preserve the owner's access when inherited entries are removed.
        apply_explicit_ace(&dark, &owner_sid(&dark), DENY_MASK, true, AceType::Allow).unwrap();
        icacls(&dark, "/inheritance:r");
        apply_explicit_ace(
            &self.deny.join(r"explicit\key.txt"),
            "S-1-15-2-1",
            RO_MASK,
            false,
            AceType::Allow,
        )
        .unwrap();
    }

    fn grant_and_shape(&self) {
        ensure_fs_grant(
            &self.allow,
            CAP_SID,
            GrantKind::ReadOnly,
            &DurableRoot::default().worker(),
        )
        .unwrap();
        self.shape();
    }

    fn assert_denies(&self) {
        for relative in [
            "protected",
            r"protected\nested",
            "protected-file.txt",
            "dark",
            r"explicit\key.txt",
        ] {
            let path = self.deny.join(relative);
            let aces = scan_explicit_aces_for_sid(&path, CAP_SID).unwrap();
            assert!(
                aces.first()
                    .is_some_and(|a| a.ace_type == AceType::Deny && a.access_mask == DENY_MASK),
                "{path:?}: explicit deny must precede any copied allow: {aces:?}"
            );
            if relative != r"explicit\key.txt" {
                assert!(protected(&path), "{path:?}: protection must remain set");
            }
        }
        assert!(!protected(&self.deny.join(r"explicit\key.txt")));
    }
}

fn icacls(path: &Path, flag: &str) {
    let output = std::process::Command::new(r"C:\Windows\System32\icacls.exe")
        .arg(path)
        .arg(flag)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "icacls {path:?} {flag}: {output:?}"
    );
}

fn protected(path: &Path) -> bool {
    descriptor_shape(path).0 & SE_DACL_PROTECTED != 0
}

fn descriptor_shape(path: &Path) -> (u16, bool) {
    let mut dacl = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    assert_eq!(rc, ERROR_SUCCESS);
    let mut control = 0;
    let mut revision = 0;
    let ok = unsafe { GetSecurityDescriptorControl(sd, &raw mut control, &raw mut revision) };
    unsafe { windows_sys::Win32::Foundation::LocalFree(sd) };
    assert_ne!(ok, 0);
    (control, dacl.is_null())
}

fn owner_sid(path: &Path) -> String {
    let mut owner = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    assert_eq!(rc, ERROR_SUCCESS);
    let mut text = std::ptr::null_mut();
    let ok = unsafe { ConvertSidToStringSidW(owner, &raw mut text) };
    unsafe { windows_sys::Win32::Foundation::LocalFree(sd) };
    assert_ne!(ok, 0);
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe { windows_sys::Win32::Foundation::LocalFree(text.cast()) };
    sid
}

#[test]
fn deny_covers_protected_and_explicit_descendants() {
    with_state(|| {
        let tree = DenyTree::new();
        tree.grant_and_shape();
        ensure_fs_grant(
            &tree.deny,
            CAP_SID,
            GrantKind::Deny,
            &DurableRoot::default().worker(),
        )
        .unwrap();
        tree.assert_denies();
    });
}

#[test]
fn an_old_deny_stamp_does_not_witness_the_descendant_walk() {
    with_state(|| {
        let tree = DenyTree::new();
        tree.grant_and_shape();
        apply_explicit_ace(&tree.deny, CAP_SID, DENY_MASK, true, AceType::Deny).unwrap();
        record_stamp(&format!("deny|{}", tree.deny.display())).unwrap();
        assert!(
            scan_explicit_aces_for_sid(&tree.deny.join("protected"), CAP_SID)
                .unwrap()
                .iter()
                .any(|ace| ace.ace_type == AceType::Allow)
        );
        ensure_fs_grant(
            &tree.deny,
            CAP_SID,
            GrantKind::Deny,
            &DurableRoot::default().worker(),
        )
        .unwrap();
        tree.assert_denies();
    });
}

#[test]
fn a_failed_walk_of_a_recreated_root_leaves_no_witness_and_retries() {
    with_state(|| {
        let tree = DenyTree::new();
        tree.grant_and_shape();
        ensure_fs_grant(
            &tree.deny,
            CAP_SID,
            GrantKind::Deny,
            &DurableRoot::default().worker(),
        )
        .unwrap();
        fs::rename(&tree.deny, tree.allow.join("old")).unwrap();
        tree.populate();
        tree.shape();

        let blocked = tree.deny.join("blocked.txt");
        fs::write(&blocked, b"").unwrap();
        let busy = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&blocked)
            .unwrap();
        let error = ensure_fs_grant(
            &tree.deny,
            CAP_SID,
            GrantKind::Deny,
            &DurableRoot::default().worker(),
        )
        .expect_err("a descendant that cannot be inspected must refuse the grant");
        assert!(error.to_string().contains("blocked.txt"), "{error}");
        assert!(!stamp_recorded(&stamp_key(&tree.deny, GrantKind::Deny)));
        drop(busy);

        ensure_fs_grant(
            &tree.deny,
            CAP_SID,
            GrantKind::Deny,
            &DurableRoot::default().worker(),
        )
        .unwrap();
        tree.assert_denies();
        assert!(stamp_recorded(&stamp_key(&tree.deny, GrantKind::Deny)));
    });
}

#[test]
fn a_deny_walk_does_not_follow_a_junction() {
    with_state(|| {
        let tree = DenyTree::new();
        let outside = tree.allow.parent().unwrap().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("key.txt"), b"secret").unwrap();
        let output = std::process::Command::new(r"C:\Windows\System32\cmd.exe")
            .current_dir(tree.cwd())
            .args(["/d", "/c", "mklink", "/J", r"D\junction", r"..\outside"])
            .output()
            .unwrap();
        assert!(output.status.success(), "create junction: {output:?}");
        tree.grant_and_shape();
        ensure_fs_grant(
            &tree.deny,
            CAP_SID,
            GrantKind::Deny,
            &DurableRoot::default().worker(),
        )
        .unwrap();
        for path in [&outside, &outside.join("key.txt")] {
            assert!(
                super::tests::sid_ace_sequence(path, CAP_SID)
                    .iter()
                    .all(|(kind, _)| *kind != 0x01),
                "the walk must not stamp the junction's target: {path:?}"
            );
        }
        tree.assert_denies();
    });
}

#[test]
fn a_null_dacl_is_refused_without_removing_ordinary_access() {
    with_state(|| {
        for relative in ["D", r"D\open.txt"] {
            let tree = DenyTree::new();
            tree.grant_and_shape();
            let path = tree.allow.join(relative);
            let rc = unsafe {
                SetNamedSecurityInfoW(
                    wide(&path).as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, ERROR_SUCCESS);
            assert!(descriptor_shape(&path).1);
            let error = ensure_fs_grant(
                &tree.deny,
                CAP_SID,
                GrantKind::Deny,
                &DurableRoot::default().worker(),
            )
            .expect_err("a deny-only DACL would also deny the host user");
            assert!(error.to_string().contains("null DACL"), "{error}");
            assert!(
                error.to_string().contains(path.to_str().unwrap()),
                "{error}"
            );
            assert!(descriptor_shape(&path).1);
            assert!(!stamp_recorded(&stamp_key(&tree.deny, GrantKind::Deny)));
        }
    });
}
