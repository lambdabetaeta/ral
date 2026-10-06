//! Pins for [`GrantStack::admits_fs`]: containment is judged on resolved
//! forms on both sides, so a symlink-spelled grant covers its target, in
//! both directions.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: tempdir trees and symlinks for containment pins"
)]

use super::FsOp;
use crate::capability::{Capabilities, FsPolicy, GrantStack};
use crate::path::{FrozenPath, Resolver};

fn stack(fs: FsPolicy) -> GrantStack {
    GrantStack::of(Capabilities {
        fs: Some(fs),
        ..Capabilities::default()
    })
}

fn admits_read(grants: &GrantStack, path: &std::path::Path) -> bool {
    let resolver = Resolver::shell_less();
    let rp = resolver.resolve(&path.to_string_lossy());
    grants.admits_fs(&FsOp::Read, &resolver, &rp)
}

#[test]
fn a_symlink_spelled_read_prefix_admits_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("SKILL.md"), "x").unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let grants = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(&link)],
        ..FsPolicy::default()
    });
    assert!(
        admits_read(&grants, &real.join("SKILL.md")),
        "a prefix granted through a symlink must cover the resolved target"
    );
    assert!(
        !admits_read(&grants, &tmp.path().join("outside")),
        "the region is still the prefix, not the world"
    );
}

/// The threat is a stack with no `fs` opinion at all — the one shape
/// the fold answers `Unrestricted` before consulting anything — so the
/// guard is tested on exactly that stack, and by inode: a hard link is
/// the same file under another name.
#[test]
fn a_write_onto_a_pinned_binary_is_guarded_before_any_grant_is_consulted() {
    use super::{FsVerdict, fs_verdict};
    crate::sandbox::pin();
    let open = GrantStack::of(Capabilities::default());
    let resolver = Resolver::shell_less();
    let own = std::env::current_exe().expect("own path");
    let verdict = |path: &std::path::Path, op: &FsOp| fs_verdict(&open, &resolver, path, op);

    assert!(
        matches!(verdict(&own, &FsOp::Write), FsVerdict::Guarded("ral")),
        "a write onto the pinned executable must be guarded under an open stack"
    );
    assert!(
        matches!(verdict(&own, &FsOp::Read), FsVerdict::Unrestricted),
        "the guard is over rewriting, not reading"
    );
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("other");
    std::fs::write(&other, "x").unwrap();
    assert!(
        matches!(verdict(&other, &FsOp::Write), FsVerdict::Unrestricted),
        "an unpinned file is the stack's to decide"
    );
    let link = tmp.path().join("link-to-own");
    if std::fs::hard_link(&own, &link).is_ok() {
        assert!(
            matches!(verdict(&link, &FsOp::Write), FsVerdict::Guarded("ral")),
            "a hard link names the pinned inode too"
        );
    }
}

#[test]
fn a_symlink_spelled_deny_covers_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    let secret = real.join("secret");
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("SKILL.md"), "x").unwrap();
    let link = tmp.path().join("link-secret");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    let grants = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(&real)],
        deny_paths: vec![FrozenPath::from_surface(&link)],
        ..FsPolicy::default()
    });
    assert!(
        !admits_read(&grants, &secret.join("SKILL.md")),
        "a deny spelled through a symlink must cover the resolved target"
    );
    assert!(
        admits_read(&grants, &real.join("SKILL.md")),
        "the deny is the entry, not the whole read region"
    );
}

/// The refusal cites a respelling only when no deny holds the access as
/// stored, though the deepest deny holds it only by a fold.
#[test]
fn a_deny_holding_the_stored_name_refuses_plainly() {
    use super::{FsVerdict, fs_verdict};
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let under = |denies: &[&str]| {
        stack(FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(&root)],
            deny_paths: (denies.iter())
                .map(|d| FrozenPath::from_surface(root.join(d)))
                .collect(),
            ..FsPolicy::default()
        })
    };
    let verdict = |grants: &GrantStack| {
        let access = root.join("d/secrets/f");
        fs_verdict(grants, &Resolver::shell_less(), &access, &FsOp::Read)
    };
    assert!(matches!(
        verdict(&under(&["d/Secrets"])),
        FsVerdict::Respelled(_)
    ));
    assert!(matches!(
        verdict(&under(&["d", "d/Secrets"])),
        FsVerdict::Denied
    ));
}

/// Default APFS answers `SECRET` with `secret`: the walk must spell it as
/// stored, or the deny on `secret` never meets the access.
#[cfg(target_os = "macos")]
#[test]
fn a_case_variant_spelling_meets_the_deny() {
    use crate::path::walk::{Leaf, walk};
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let secret = root.join("secret");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("key"), "x").unwrap();
    if !root.join("SECRET").exists() {
        return;
    }
    let grants = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(&root)],
        deny_paths: vec![FrozenPath::from_surface(&secret)],
        ..FsPolicy::default()
    });
    let resolver = Resolver::shell_less();
    let rp = resolver.resolve(&root.join("SECRET/key").to_string_lossy());
    let located = walk(&rp, Leaf::Resolve).unwrap();
    assert_eq!(located.real(), secret.join("key"));
    assert!(!admits_read(&grants, located.real()));
}
