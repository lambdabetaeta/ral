//! The walk on macOS: the stored-name rule, a leaf the user cannot hold, and
//! links read through the descriptor that opened them.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build the tree the walk is asked about"
)]

use super::super::*;
use super::unix::{id, located_with, name, root, root_user, spelled};
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::{AtomicBool, Ordering};

/// A directory held as the walk holds one.
fn held(path: &Path) -> Held {
    Held {
        file: File::open(path).unwrap(),
        info: host::DirInfo,
    }
}

fn read_all(path: &Path) -> String {
    let mut text = String::new();
    File::open(path).unwrap().read_to_string(&mut text).unwrap();
    text
}

/// The first branch: a spelling of the name asked, under every fold a
/// filesystem applies, is the entry's own, and is what the directory stores.
#[test]
fn a_case_variant_is_recorded_at_the_spelling_the_directory_stores() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("Secret"), b"x").unwrap();
    if !dir.join("SECRET").exists() {
        eprintln!("skip: this volume is case-sensitive, so no other spelling is an entry");
        return;
    }
    let loc = located_with(&dir.join("SECRET"), LeafMode::Follow).unwrap();
    assert_eq!(loc.real().as_path(), dir.join("Secret"));
    let step = loc.trace().steps().last().unwrap();
    assert_eq!(step.name().as_os_str(), "Secret");
}

/// The rule over what a lookup answered, branch by branch, with two hard links
/// `a` and `b` in one directory: an answer that spells the name asked is
/// taken; the other link's name is replaced by the listing's entry for the
/// object held; and no such entry, or a directory that will not list, refuses.
#[test]
fn the_stored_name_rule_takes_a_spelling_and_lists_for_another_links_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("a"), b"x").unwrap();
    std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
    let held = held(&dir);
    let wanted = id(&dir.join("a"));

    let kept = host::settle(&held.file, &name("a"), wanted, OsStr::new("A")).unwrap();
    assert_eq!(
        kept.as_os_str(),
        "A",
        "the spelling answered is the entry's"
    );

    let listed = host::settle(&held.file, &name("a"), wanted, OsStr::new("b")).unwrap();
    assert_eq!(
        listed.as_os_str(),
        "a",
        "the other link's name is not adopted"
    );

    let err = host::settle(&held.file, &name("c"), wanted, OsStr::new("b")).unwrap_err();
    assert!(err.to_string().contains("moved during the walk"), "{err}");

    if root_user() {
        return;
    }
    std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o311)).unwrap();
    let blind = host::settle(&held.file, &name("a"), wanted, OsStr::new("b"));
    std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o755)).unwrap();
    let err = blind.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        err.to_string().contains("cannot learn the stored name"),
        "{err}"
    );
}

/// A walk of `a` records `a` whichever name the vnode carries, so a `>` on it
/// lands on `a` and leaves `b` its own object, while another thread keeps the
/// vnode's name moving by looking up `b`.
#[test]
fn a_hard_link_is_recorded_under_the_name_it_was_opened_by() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("a"), b"old").unwrap();
    std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let _ = std::fs::symlink_metadata(dir.join("b"));
            }
        });
        for _ in 0..500 {
            let loc = located_with(&dir.join("a"), LeafMode::Follow).unwrap();
            assert_eq!(loc.real().as_path(), dir.join("a"));
        }
        stop.store(true, Ordering::Relaxed);
    });
    let loc = located_with(&dir.join("a"), LeafMode::Follow).unwrap();
    let mut staged = loc.stage().unwrap();
    std::io::Write::write_all(staged.file(), b"new").unwrap();
    staged.publish().unwrap();
    assert_eq!(read_all(&dir.join("a")), "new");
    assert_eq!(read_all(&dir.join("b")), "old");
}

/// An entry that moved between its open and the check is refused, never
/// adopted: renamed within its directory the lookup no longer finds it,
/// replaced the lookup finds another object, and moved to another directory
/// it is not an entry of this one.
#[test]
fn a_component_that_moved_before_its_name_was_checked_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("a")).unwrap();
    std::fs::create_dir(dir.join("b")).unwrap();
    std::fs::write(dir.join("a/x"), b"x").unwrap();
    let parent = held(&dir.join("a"));
    let opened = id(&dir.join("a/x"));
    assert_eq!(
        host::stored_name(&parent.file, &name("x"), opened)
            .unwrap()
            .as_os_str(),
        "x"
    );

    let refused = |entry: &str| {
        host::stored_name(&parent.file, &name(entry), opened)
            .unwrap_err()
            .to_string()
    };
    std::fs::rename(dir.join("a/x"), dir.join("a/y")).unwrap();
    assert!(refused("x").contains("moved during the walk"));

    std::fs::write(dir.join("a/x"), b"another").unwrap();
    assert!(refused("x").contains("moved during the walk"));

    std::fs::rename(dir.join("a/y"), dir.join("b/y")).unwrap();
    assert!(refused("y").contains("moved during the walk"));
}

/// `freadlink` reads a link's target through the `O_SYMLINK` descriptor that
/// opened it, under either sharing.
#[test]
fn a_link_is_read_through_its_o_symlink_descriptor() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    symlink("some/target", dir.join("l")).unwrap();
    let parent = held(&dir);
    for sharing in [Sharing::Leaf, Sharing::Directory] {
        let child = host::step(&parent, &name("l"), sharing).unwrap();
        assert_eq!(child.step.kind, Kind::Symlink);
        let handle = child.handle.unwrap();
        assert_eq!(host::read_link(&handle).unwrap(), Path::new("some/target"));
    }
}

/// A present file the user cannot read is stated, not held: the probes
/// `exists` and `file-info` answer from its stat, and `read` is refused
/// without a data handle having been opened.
#[test]
fn a_file_the_user_cannot_read_is_stated_and_never_opened() {
    if root_user() {
        eprintln!("skip: root reads every file");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("closed"), b"body").unwrap();
    std::fs::set_permissions(dir.join("closed"), PermissionsExt::from_mode(0o000)).unwrap();
    let derived = || host::DERIVED.with(std::cell::Cell::get);
    let before = derived();
    for mode in [LeafMode::Follow, LeafMode::NoFollow] {
        let loc = located_with(&dir.join("closed"), mode).unwrap();
        assert!(matches!(loc.leaf, Leaf::Stated { .. }), "{mode:?}");
        assert_eq!(loc.real().as_path(), dir.join("closed"));
        let stat = loc.stat().unwrap();
        assert_eq!((stat.kind, stat.len), (Kind::File, 4), "{mode:?}");
        assert!(stat.mtime.is_some());
        assert_eq!(
            loc.read().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
    assert_eq!(derived(), before, "nothing was opened");
}

/// `/Users` is a firmlink: the by-name lookup crosses it as `openat` did, so
/// the step records the identity of the object `openat` reached.
#[test]
fn a_walk_through_a_firmlink_records_the_identity_openat_reached() {
    let loc = located_with(Path::new("/Users"), LeafMode::Follow).unwrap();
    assert_eq!(loc.trace().last_id(), id(Path::new("/Users")));
    let slash = held(Path::new("/"));
    let named = host::seen_at(&slash.file, OsStr::new("Users")).unwrap();
    assert_eq!(named.id, loc.trace().last_id());
}

/// A link is held whatever the mode: `Follow` reaches its target, `NoFollow`
/// the link, which answers its target; a link has no permission to refuse.
#[test]
fn a_leaf_link_is_held_under_every_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("target"), b"body").unwrap();
    symlink("target", dir.join("link")).unwrap();

    let followed = located_with(&dir.join("link"), LeafMode::Follow).unwrap();
    assert_eq!(followed.real().as_path(), dir.join("target"));
    assert_eq!(followed.stat().unwrap().kind, Kind::File);

    let named = located_with(&dir.join("link"), LeafMode::NoFollow).unwrap();
    assert!(matches!(named.leaf, Leaf::Held { .. }));
    assert_eq!(named.stat().unwrap().kind, Kind::Symlink);
    assert_eq!(named.read_link().unwrap(), Path::new("target"));
}

/// A dangling link under `Follow` is a create target at the name it leads to,
/// which `> dangling` needs, while a real path of it is `NotFound`.
#[test]
fn a_dangling_link_under_follow_is_a_create_target_with_no_real_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    symlink("gone", dir.join("dangling")).unwrap();
    let loc = located_with(&dir.join("dangling"), LeafMode::Follow).unwrap();
    assert!(loc.stat().is_none());
    assert_eq!(loc.real().as_path(), dir.join("gone"));
    let err = RealPath::of(&spelled(&dir.join("dangling"))).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}
