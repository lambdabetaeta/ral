//! The walk on macOS: the stored-name rule, a leaf the user cannot hold or a
//! special file, and links read through the descriptor that opened them.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build the tree the walk is asked about"
)]

use super::super::*;
use super::LOOKED;
use super::unix::{id, located_with, name, root, root_user, spelled};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// A directory held as the walk holds one.
fn held(path: &Path) -> Held {
    Held {
        file: Arc::new(File::open(path).unwrap()),
        info: host::DirInfo,
    }
}

fn read_all(path: &Path) -> String {
    let mut text = String::new();
    File::open(path).unwrap().read_to_string(&mut text).unwrap();
    text
}

/// A volume mounted from a disk image, detached when it drops.
struct Volume(PathBuf);

impl Volume {
    fn attach(dir: &Path, fs: &str) -> Self {
        let (image, mount) = (dir.join("volume.dmg"), dir.join("mount"));
        std::fs::create_dir(&mount).unwrap();
        hdiutil(&[
            "create".as_ref(),
            "-quiet".as_ref(),
            "-size".as_ref(),
            "8m".as_ref(),
            "-fs".as_ref(),
            fs.as_ref(),
            "-volname".as_ref(),
            "ral-hold".as_ref(),
            image.as_os_str(),
        ]);
        hdiutil(&[
            "attach".as_ref(),
            "-quiet".as_ref(),
            "-nobrowse".as_ref(),
            "-mountpoint".as_ref(),
            mount.as_os_str(),
            image.as_os_str(),
        ]);
        Self(mount)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        hdiutil(&[
            "detach".as_ref(),
            "-quiet".as_ref(),
            "-force".as_ref(),
            self.0.as_os_str(),
        ]);
    }
}

fn hdiutil(args: &[&OsStr]) {
    let status = Command::new("hdiutil").args(args).status().unwrap();
    assert!(status.success(), "hdiutil {args:?}");
}

/// A spelling of the name asked, under every fold a filesystem applies, is
/// the entry's own where the directory stores that spelling.
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

/// The rule over what a lookup answered, with two hard links `a` and `b` in
/// one directory: an answer spelled as the name asked is taken without a
/// listing; any other is decided by the entries whose lookup reaches the
/// object held, the byte-equal name first; none, a directory that will not
/// list, or an object only stated refuses.
#[test]
fn the_stored_name_rule_takes_a_spelling_and_lists_for_another_links_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("a"), b"x").unwrap();
    std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
    let held = held(&dir);
    let wanted = id(&dir.join("a"));

    let byte_present = host::settle(&held.file, &name("a"), Some(wanted), OsStr::new("A")).unwrap();
    assert_eq!(byte_present.as_os_str(), "a", "the name asked is listed");
    let listed = host::settle(&held.file, &name("a"), Some(wanted), OsStr::new("b")).unwrap();
    assert_eq!(
        listed.as_os_str(),
        "a",
        "the other link's name is not adopted"
    );
    let err = host::settle(&held.file, &name("c"), Some(wanted), OsStr::new("b")).unwrap_err();
    assert!(err.to_string().contains("moved during the walk"), "{err}");
    let stated = host::settle(&held.file, &name("a"), None, OsStr::new("a")).unwrap();
    assert_eq!(stated.as_os_str(), "a", "a stated answer spelled as asked");
    let err = host::settle(&held.file, &name("a"), None, OsStr::new("b")).unwrap_err();
    assert!(
        err.to_string().contains("states but does not hold"),
        "{err}"
    );

    if root_user() {
        return;
    }
    std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o311)).unwrap();
    let asked = host::settle(&held.file, &name("a"), Some(wanted), OsStr::new("a"));
    let blind = host::settle(&held.file, &name("a"), Some(wanted), OsStr::new("b"));
    std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o755)).unwrap();
    assert_eq!(
        asked.unwrap().as_os_str(),
        "a",
        "an answer spelled as asked lists nothing"
    );
    let err = blind.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        err.to_string().contains("cannot learn the stored name"),
        "{err}"
    );
}

/// A walk of `walked` records `walked` whichever name the vnode carries, so a
/// `>` on it lands on it and leaves `other` its own object, while another
/// thread keeps the vnode's name moving by looking up `other`.
fn walks_stay_off_the_other_link(dir: &Path, walked: &str, other: &str) {
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let _ = std::fs::symlink_metadata(dir.join(other));
            }
        });
        for _ in 0..500 {
            let loc = located_with(&dir.join(walked), LeafMode::Follow).unwrap();
            assert_eq!(loc.real().as_path(), dir.join(walked));
        }
        stop.store(true, Ordering::Relaxed);
    });
    let loc = located_with(&dir.join(walked), LeafMode::Follow).unwrap();
    let mut staged = loc.stage().unwrap();
    staged.file().write_all(b"new").unwrap();
    staged.publish().unwrap();
    assert_eq!(read_all(&dir.join(walked)), "new");
    assert_eq!(read_all(&dir.join(other)), "old");
}

#[test]
fn a_hard_link_is_recorded_under_the_name_it_was_opened_by() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("a"), b"old").unwrap();
    std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
    walks_stay_off_the_other_link(&dir, "a", "b");
}

/// On a case-sensitive volume `a` and `A` are two entries of one object, and
/// no fold makes them one.
#[test]
fn a_case_sensitive_volume_keeps_two_links_apart() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = Volume::attach(&root(&tmp), "Case-sensitive APFS");
    std::fs::write(volume.path().join("a"), b"old").unwrap();
    std::fs::hard_link(volume.path().join("a"), volume.path().join("A")).unwrap();
    walks_stay_off_the_other_link(volume.path(), "a", "A");
}

/// Two entries of one object that fold alike, neither spelled as asked, are
/// not guessed between.
#[test]
fn an_ambiguous_fold_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = Volume::attach(&root(&tmp), "Case-sensitive APFS");
    let mount = volume.path();
    std::fs::write(mount.join("\u{e1}"), b"x").unwrap();
    std::fs::hard_link(mount.join("\u{e1}"), mount.join("\u{c1}")).unwrap();
    let err = host::settle(
        &held(mount).file,
        &name("a\u{301}"),
        Some(id(&mount.join("\u{e1}"))),
        OsStr::new("\u{e1}"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("spellings of one object"), "{err}");
}

#[test]
fn pick_takes_the_byte_equal_name_then_the_unique_fold() {
    let entries = |names: &[&str]| names.iter().map(|n| name(n)).collect::<Vec<_>>();
    let picked = |names: &[&str], asked: &str| host::pick(&entries(names), &name(asked));
    assert_eq!(picked(&["a", "A"], "A").unwrap(), name("A"));
    assert_eq!(picked(&["Desktop"], "desktop").unwrap(), name("Desktop"));
    let err = picked(&["\u{e1}", "\u{c1}"], "a\u{301}").unwrap_err();
    assert!(err.to_string().contains("2 spellings"), "{err}");
    let err = picked(&["b"], "a").unwrap_err();
    assert!(err.to_string().contains("moved during the walk"), "{err}");
}

/// What the kernel packs for one entry: its length, the returned set, the
/// name as a reference to bytes past the fixed fields `rest`, which follow it.
fn packed(common: u32, name: &str, rest: &[u8]) -> Vec<u8> {
    let at = 4 + 5 * 4;
    let offset = i32::try_from(8 + rest.len()).unwrap();
    let length = u32::try_from(at + 8 + rest.len() + name.len() + 1).unwrap();
    let mut packed = length.to_ne_bytes().to_vec();
    packed.extend(common.to_ne_bytes());
    packed.extend([0; 16]);
    packed.extend(offset.to_ne_bytes());
    packed.extend(u32::try_from(name.len() + 1).unwrap().to_ne_bytes());
    packed.extend(rest);
    packed.extend(name.as_bytes());
    packed.push(0);
    packed
}

/// A lookup is read by the set it returned: a volume without timestamps
/// answers without them, and one that does not answer an identity is refused.
#[test]
fn parse_lookup_reads_by_the_returned_set() {
    const TIMES: u32 = libc::ATTR_CMN_CRTIME | libc::ATTR_CMN_MODTIME | libc::ATTR_CMN_ACCTIME;
    const SET: u32 = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_ACCESSMASK
        | libc::ATTR_CMN_FILEID;
    let reply = |common: u32| {
        let mut rest = Vec::new();
        rest.extend(7u32.to_ne_bytes());
        rest.extend(1u32.to_ne_bytes());
        if common & TIMES == TIMES {
            for _ in 0..3 {
                rest.extend(1_000i64.to_ne_bytes());
                rest.extend(0i64.to_ne_bytes());
            }
        }
        rest.extend(0o100_644u32.to_ne_bytes());
        if common & libc::ATTR_CMN_FILEID != 0 {
            rest.extend(42u64.to_ne_bytes());
        }
        packed(common, "x", &rest)
    };

    let full = host::parse_lookup(&reply(SET | TIMES)).unwrap();
    assert_eq!(full.name, "x");
    assert_eq!((full.seen.id.dev, full.seen.id.ino), (7, 42));
    assert_eq!(
        (full.seen.stat.kind, full.seen.stat.mode),
        (Kind::File, 0o644)
    );
    let stat = full.seen.stat;
    assert!(stat.btime.is_some() && stat.mtime.is_some() && stat.atime.is_some());

    let bare = host::parse_lookup(&reply(SET)).unwrap();
    assert_eq!((bare.seen.id.dev, bare.seen.id.ino), (7, 42));
    assert_eq!(
        (bare.seen.stat.kind, bare.seen.stat.mode),
        (Kind::File, 0o644)
    );
    let stat = bare.seen.stat;
    assert!(stat.btime.is_none() && stat.mtime.is_none() && stat.atime.is_none());

    let err = host::parse_lookup(&reply(SET & !libc::ATTR_CMN_FILEID))
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

/// An entry that moved between the lookup and the open is refused, never
/// adopted: replaced, the open finds another object; renamed within its
/// directory or moved to another, it finds none.
#[test]
fn an_object_replaced_between_lookup_and_open_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("a")).unwrap();
    std::fs::create_dir(dir.join("b")).unwrap();
    let parent = held(&dir.join("a"));
    let stepped = |moves: Box<dyn FnMut()>| {
        std::fs::write(dir.join("a/x"), b"x").unwrap();
        LOOKED.with(|looked| *looked.borrow_mut() = Some(moves));
        let stepped = host::step(&parent, &name("x"), Sharing::Leaf);
        LOOKED.with(|looked| looked.borrow_mut().take());
        let Err(refused) = stepped else {
            panic!("the entry moved before it was opened");
        };
        refused
    };
    let at = |path: &str| dir.join(path);

    let replaced = stepped(Box::new({
        let x = at("a/x");
        move || {
            std::fs::remove_file(&x).unwrap();
            std::fs::write(&x, b"another").unwrap();
        }
    }));
    assert!(
        replaced.to_string().contains("moved during the walk"),
        "{replaced}"
    );

    let renamed = stepped(Box::new({
        let (x, y) = (at("a/x"), at("a/y"));
        move || std::fs::rename(&x, &y).unwrap()
    }));
    assert_eq!(renamed.kind(), io::ErrorKind::NotFound);

    let elsewhere = stepped(Box::new({
        let (x, moved) = (at("a/x"), at("b/x"));
        move || std::fs::rename(&x, &moved).unwrap()
    }));
    assert_eq!(elsewhere.kind(), io::ErrorKind::NotFound);

    // The open cannot refuse a special file by kind: the substitute FIFO is
    // opened, without blocking, and refused by identity — the residual.
    let fifo = stepped(Box::new({
        let x = at("a/x");
        move || {
            std::fs::remove_file(&x).unwrap();
            mkfifo(&x);
        }
    }));
    assert!(fifo.to_string().contains("moved during the walk"), "{fifo}");
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
/// `exists` and `file-info` answer from its stat, and `read` and `stage` are
/// refused without a data handle having been opened or a sibling staged.
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
        let Err(refused) = loc.stage() else {
            panic!("{mode:?}: a stated leaf was staged");
        };
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{mode:?}");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "nothing staged"
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

/// A firmlink spelled in another case is recorded at the name the root
/// stores: the listing's own entry for it carries the firmlink's identity, a
/// lookup of it the identity the step reached.
#[test]
fn a_firmlink_in_another_case_is_recorded_at_its_stored_name() {
    if !Path::new("/USERS").exists() {
        eprintln!("skip: the boot volume is case-sensitive");
        return;
    }
    let loc = located_with(Path::new("/users"), LeafMode::Follow).unwrap();
    assert_eq!(loc.real().as_path(), Path::new("/Users"));
    assert_eq!(loc.trace().last_id(), id(Path::new("/Users")));
}

/// A mount point spelled in another case is recorded at the name its parent
/// stores, though the listing reports the covered directory's identity and
/// the step the mounted root's.
#[test]
fn a_mount_point_in_another_case_is_recorded_at_its_stored_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let volume = Volume::attach(&dir, "APFS");
    std::fs::write(volume.path().join("f"), b"x").unwrap();
    if !dir.join("MOUNT").exists() {
        eprintln!("skip: this volume is case-sensitive, so no other spelling is an entry");
        return;
    }
    let loc = located_with(&dir.join("MOUNT").join("f"), LeafMode::Follow).unwrap();
    assert_eq!(loc.real().as_path(), dir.join("mount").join("f"));
}

/// A stated object reached by another spelling than the lookup answered is
/// not named by an identity nothing holds: the walk refuses.
#[test]
fn a_stated_object_in_another_spelling_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkfifo(&dir.join("Pipe"));
    if !dir.join("PIPE").exists() {
        eprintln!("skip: this volume is case-sensitive, so no other spelling is an entry");
        return;
    }
    let Err(err) = located_with(&dir.join("PIPE"), LeafMode::Follow) else {
        panic!("a stated FIFO was named by an identity nothing holds");
    };
    assert!(
        err.to_string().contains("states but does not hold"),
        "{err}"
    );
    let loc = located_with(&dir.join("Pipe"), LeafMode::Follow).unwrap();
    assert_eq!(loc.real().as_path(), dir.join("Pipe"));
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

/// A leaf is held by the walk's own `O_EVTONLY` descriptor: the object is
/// kept, though every name of it is gone, while the exclusion stands.
#[test]
fn an_exclusion_holds_its_leaf_by_the_walks_own_o_evtonly_descriptor() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("secret"), b"s").unwrap();
    let excluded = walk_excluded(&spelled(&dir.join("secret"))).unwrap();
    let held = excluded.named.handle.as_ref().expect("the leaf is held");
    assert_eq!(ObjectId::of(held).unwrap(), excluded.named.trace.last_id());
    std::fs::remove_file(dir.join("secret")).unwrap();
    assert_eq!(held.metadata().unwrap().nlink(), 0, "the vnode is kept");
}

/// An unmount revokes an `O_EVTONLY` hold rather than failing: the exclusion
/// stands unheld, by its position, where the volume was and is not.
#[test]
fn an_unmounted_volume_leaves_an_exclusion_unheld_by_position() {
    use crate::path::{Depth, MatchedBy, Subject};
    let tmp = tempfile::tempdir().unwrap();
    let volume = Volume::attach(&root(&tmp), "APFS");
    let mount = volume.path().to_path_buf();
    std::fs::write(mount.join("secret"), b"s").unwrap();
    let mut excluded = walk_excluded(&spelled(&mount.join("secret"))).unwrap();
    assert!(excluded.named.handle.is_some());

    drop(volume);
    excluded.rehold();
    assert!(excluded.named.handle.is_none());
    let access = walk_lenient(&spelled(&mount.join("secret")), LeafMode::Follow).unwrap();
    assert_eq!(
        excluded.matches(Subject::Access(&access), Depth::Within),
        Some(MatchedBy::Name)
    );
}

fn mkfifo(path: &Path) {
    assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
}

/// macOS cannot hold a special file without opening it, so the walk states
/// one: a FIFO keeps no reader of ours, an exclusion on it holds nothing, and
/// a socket walks, exists and is not readable through a data handle.
#[test]
fn a_special_file_is_stated_and_never_held() {
    use std::os::unix::net::UnixListener;
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let derived = || host::DERIVED.with(std::cell::Cell::get);
    let before = derived();
    let stated = |loc: &Located<Judged<Writing>>| matches!(&loc.leaf, Leaf::Stated { stat, .. } if stat.kind == Kind::Other);

    mkfifo(&dir.join("pipe"));
    let loc = located_with(&dir.join("pipe"), LeafMode::Follow).unwrap();
    assert!(stated(&loc));
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
        .open(dir.join("pipe"));
    assert_eq!(
        writer.unwrap_err().raw_os_error(),
        Some(rustix::io::Errno::NXIO.raw_os_error()),
        "no reader of ours"
    );
    let excluded = walk_excluded(&spelled(&dir.join("pipe"))).unwrap();
    assert!(excluded.named.handle.is_none());

    let _listener = UnixListener::bind(dir.join("sock")).unwrap();
    let loc = located_with(&dir.join("sock"), LeafMode::Follow).unwrap();
    assert!(stated(&loc));
    assert!(loc.stat().is_some());
    assert!(loc.read().is_err());
    assert_eq!(derived(), before, "nothing was opened");
}

/// A stream to a stated FIFO is opened by name under the held directory, and
/// must find a special file there.
#[test]
fn a_stream_to_a_fifo_is_opened_by_name_and_must_find_a_fifo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkfifo(&dir.join("pipe"));
    // A reader first: a write open on a FIFO with nobody to meet would block.
    let mut sink = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
        .open(dir.join("pipe"))
        .unwrap();
    let mut streamed = located_with(&dir.join("pipe"), LeafMode::Follow)
        .unwrap()
        .open_streaming(Streaming::Truncate)
        .unwrap();
    streamed.file().write_all(b"through").unwrap();
    let mut body = [0; 7];
    sink.read_exact(&mut body).unwrap();
    assert_eq!(&body, b"through");

    mkfifo(&dir.join("pipe2"));
    let loc = located_with(&dir.join("pipe2"), LeafMode::Follow).unwrap();
    std::fs::remove_file(dir.join("pipe2")).unwrap();
    std::fs::write(dir.join("pipe2"), b"kept").unwrap();
    let Err(refused) = loc.open_streaming(Streaming::Append) else {
        panic!("a regular file is no FIFO");
    };
    assert!(
        refused.to_string().contains("moved during the walk"),
        "{refused}"
    );
    assert_eq!(read_all(&dir.join("pipe2")), "kept");
}
