//! The walk on a Windows volume.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build the file whose short alias the walk is asked about"
)]

use super::super::*;
use super::{writable, writing};
use crate::path::Resolver;
use std::io::{Read, Write};
use windows_sys::Wdk::Storage::FileSystem::SYMLINK_FLAG_RELATIVE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
};
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

fn spelled(path: &Path) -> LexicalPath {
    let cwd = crate::test_helper::process_cwd();
    Resolver::at(&crate::types::EnvVars::new(), &cwd).anchor(path)
}

/// `path` walked, and the directory `path` entered, as these tests hold
/// them: retagged as judged for writing, since the tests judge nothing.
fn walked(path: &Path) -> Located<Judged<Writing>> {
    writing(walk(&spelled(path), LeafMode::Follow).unwrap())
}

fn entered(path: &Path) -> Dir<Judged<Writing>> {
    writable(walk_dir(&spelled(path)).unwrap())
}

fn name(raw: &str) -> EntryName {
    EntryName::parse(OsStr::new(raw)).expect("a name")
}

fn read_all(path: &Path) -> String {
    let mut text = String::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text
}

#[test]
fn existing_and_absent_leaves_use_the_parents_stored_case() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("StoredCase");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("File"), "x").unwrap();
    for (spelled, stored) in [("file", "File"), ("new", "new")] {
        let located = walked(&tmp.path().join("storedcase").join(spelled));
        assert!(
            located
                .real()
                .as_path()
                .ends_with(Path::new("StoredCase").join(stored)),
            "{}",
            located.real()
        );
    }
}

/// What a link target may splice in, and the walk refuses on restart:
/// a drive-relative name, which `Path::join` lets replace the walked
/// prefix, and a head `std::path` reads on the current drive.
#[test]
fn a_walk_starts_only_from_a_parsed_root() {
    for raw in ["x", "C:x", r"\x", r"\\x", r"/\x", r"\\\srv\sh"] {
        assert_eq!(
            rooted(Path::new(raw)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "{raw}"
        );
    }
    for raw in [r"C:\x", r"\\srv\sh\x", r"\\?\C:\x"] {
        assert!(rooted(Path::new(raw)).is_ok(), "{raw}");
    }
}

/// The 8.3 alias Windows keeps for `path`, if this volume keeps one.
fn short_name(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: `wide` is NUL-terminated and `buf` holds the 1024 units
    // the call is told of.
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), 1024) };
    buf.truncate(usize::try_from(len).ok().filter(|&n| n > 0 && n < 1024)?);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)))
}

/// M4b: a *file* reached by its short alias locates at its long name, so
/// a deny frozen on the long name meets it.
#[test]
fn a_file_reached_by_its_short_alias_locates_at_its_long_name() {
    let tmp = tempfile::tempdir().unwrap();
    let long = tmp.path().join("secretfile-with-a-long-name.txt");
    std::fs::write(&long, "x").unwrap();
    let Some(short) = short_name(&long).filter(|s| s.file_name() != long.file_name()) else {
        eprintln!("skip: this volume keeps no 8.3 names, so there is no alias to walk");
        return;
    };
    let located = walked(&short);
    assert_eq!(
        located.real().as_path().file_name(),
        long.file_name(),
        "the walk kept the short alias {short:?}"
    );
}

/// An identity is the volume serial and the 128-bit file id of the handle:
/// every handle on an object and every name of it agree, and two objects do
/// not.
#[test]
fn an_identity_is_the_file_id_of_the_handle() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("f"), b"body").unwrap();
    std::fs::hard_link(tmp.path().join("f"), tmp.path().join("linked")).unwrap();
    std::fs::write(tmp.path().join("other"), b"body").unwrap();
    let ids = |entry: &str| ObjectId::of(&File::open(tmp.path().join(entry)).unwrap()).unwrap();
    assert_eq!(ids("f"), ids("f"));
    assert_eq!(ids("f"), ids("linked"));
    assert_ne!(ids("f"), ids("other"));
    assert_eq!(walked(&tmp.path().join("f")).trace().last_id(), ids("f"));
    let dir = entered(tmp.path());
    for entry in dir.read_dir().unwrap() {
        let name = entry.name().as_os_str().to_owned();
        assert_eq!(entry.seen().unwrap().id, ids(&name.to_string_lossy()));
    }
}

/// A `BACKUP_SEMANTICS` handle opened by any spelling answers the long name
/// the volume stores, which is what the step records.
#[test]
fn file_normalized_name_info_answers_the_stored_case() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    };
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("Parent/Child")).unwrap();
    let lower = tmp.path().join("parent").join("child");
    let handle = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&lower)
        .unwrap();
    let wide = host::normalized(&handle).unwrap();
    let stored = String::from_utf16(&wide).unwrap();
    assert!(stored.ends_with(r"\Parent\Child"), "{stored}");
}

/// A directory the walk holds shares no delete, so neither it nor anything
/// above it moves while the `Dir` or the `Located` lives, and each moves once
/// it is dropped.
#[test]
fn a_held_prefix_refuses_a_rename_while_it_lives() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
    std::fs::write(tmp.path().join("a/b/f"), b"x").unwrap();

    let dir = entered(&tmp.path().join("a/b"));
    assert!(std::fs::rename(tmp.path().join("a/b"), tmp.path().join("a/c")).is_err());
    assert!(std::fs::rename(tmp.path().join("a"), tmp.path().join("z")).is_err());
    drop(dir);
    std::fs::rename(tmp.path().join("a"), tmp.path().join("z")).unwrap();

    let located = walked(&tmp.path().join("z/b/f"));
    assert!(std::fs::rename(tmp.path().join("z/b"), tmp.path().join("z/c")).is_err());
    assert!(std::fs::rename(tmp.path().join("z"), tmp.path().join("y")).is_err());
    drop(located);
    std::fs::rename(tmp.path().join("z"), tmp.path().join("y")).unwrap();
}

/// A volume whose 128-bit ids are zero lists with the 64-bit id, and agrees
/// with [`ObjectId::of`].  `RAL_TEST_FAT_VOLUME` names a directory on such a
/// volume, a FAT VHD.
#[test]
fn a_listing_without_128_bit_ids_falls_back_to_the_64_bit_id() {
    let Some(volume) = std::env::var_os("RAL_TEST_FAT_VOLUME") else {
        eprintln!("skip: RAL_TEST_FAT_VOLUME names no directory on a volume without 128-bit ids");
        return;
    };
    let tmp = tempfile::tempdir_in(volume).unwrap();
    std::fs::write(tmp.path().join("f"), b"body").unwrap();
    std::fs::create_dir(tmp.path().join("d")).unwrap();
    let dir = entered(tmp.path());
    let mut listed = 0;
    for entry in dir.read_dir().unwrap() {
        let file = File::open(tmp.path().join(entry.name().as_os_str())).unwrap();
        assert_eq!(entry.seen().unwrap().id, ObjectId::of(&file).unwrap());
        listed += 1;
    }
    assert_eq!(listed, 2);
}

/// `FileStandardInfo` carries the length, which a step and a listing answer.
#[test]
fn a_stat_carries_the_length() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("f"), b"12345").unwrap();
    let located = walked(&tmp.path().join("f"));
    assert_eq!(located.stat().unwrap().len, 5);
    let dir = entered(tmp.path());
    let entry = dir.read_dir().unwrap().next().unwrap();
    assert_eq!(entry.seen().unwrap().stat.len, 5);
}

/// A reparse buffer of `tag`, holding `substitute` and, after it, the same as
/// the print name: a symlink's carries `flags`, a mount point's has none.
fn reparse_buffer(tag: u32, flags: Option<u32>, substitute: &str) -> Vec<u8> {
    let wide: Vec<u8> = substitute
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let len = u16::try_from(wide.len()).unwrap();
    let mut buf = Vec::new();
    buf.extend(tag.to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    buf.extend(len.to_le_bytes());
    buf.extend(len.to_le_bytes());
    buf.extend(len.to_le_bytes());
    buf.extend(flags.map(u32::to_le_bytes).into_iter().flatten());
    buf.extend(&wide);
    buf.extend(&wide);
    buf
}

/// What a link's target decodes to, which `rooted` then decides: an NT path
/// is the Win32 path that names the same object, a relative symlink stays
/// relative.
#[test]
fn a_reparse_buffer_decodes_to_the_target_it_names() {
    let decoded =
        |tag, flags, substitute| host::decode(&reparse_buffer(tag, flags, substitute)).unwrap();
    let symlink = |substitute| decoded(IO_REPARSE_TAG_SYMLINK, Some(0), substitute);
    assert_eq!(symlink(r"\??\C:\x"), Path::new(r"C:\x"));
    assert_eq!(symlink(r"\??\UNC\srv\share\x"), Path::new(r"\\srv\share\x"));
    assert_eq!(
        decoded(
            IO_REPARSE_TAG_MOUNT_POINT,
            None,
            r"\??\Volume{12345678-1234-1234-1234-123456789abc}\x"
        ),
        Path::new(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\x")
    );
    assert_eq!(
        decoded(
            IO_REPARSE_TAG_SYMLINK,
            Some(SYMLINK_FLAG_RELATIVE),
            r"..\sibling\x"
        ),
        Path::new(r"..\sibling\x")
    );
    assert_eq!(symlink(""), Path::new(""));
}

/// A reparse tag that names no path — a cloud placeholder's — is no link: the
/// object is the file or directory it presents, and its buffer is no target.
#[test]
fn a_reparse_point_that_names_no_path_is_no_link() {
    const CLOUD: u32 = 0x9000_001A;
    let reparse = FILE_ATTRIBUTE_REPARSE_POINT;
    assert_eq!(host::kind_of(reparse, CLOUD), Kind::File);
    assert_eq!(
        host::kind_of(reparse | FILE_ATTRIBUTE_DIRECTORY, CLOUD),
        Kind::Dir
    );
    assert_eq!(
        host::kind_of(reparse, IO_REPARSE_TAG_SYMLINK),
        Kind::Symlink
    );
    assert_eq!(
        host::kind_of(
            reparse | FILE_ATTRIBUTE_DIRECTORY,
            IO_REPARSE_TAG_MOUNT_POINT
        ),
        Kind::Symlink
    );
    assert_eq!(host::kind_of(FILE_ATTRIBUTE_DIRECTORY, 0), Kind::Dir);
    let err = host::decode(&reparse_buffer(CLOUD, None, r"\??\C:\x")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// A leaf renamed within its directory between its open and its check is
/// adopted at its new name, which is where it is; one moved to another
/// directory refuses the walk.
#[test]
fn a_component_renamed_is_adopted_and_one_moved_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("d")).unwrap();
    std::fs::create_dir(tmp.path().join("e")).unwrap();
    std::fs::write(tmp.path().join("d/old"), b"x").unwrap();
    let parent = entered(&tmp.path().join("d"));
    let child = host::step(&parent.handle, &name("old"), Sharing::Leaf).unwrap();
    let file = child.handle.as_ref().unwrap();
    let volume = child.step.id.volume;

    std::fs::rename(tmp.path().join("d/old"), tmp.path().join("d/new")).unwrap();
    let (stored, _) = host::stored_name(&parent.handle.info, &name("old"), file, volume).unwrap();
    assert_eq!(stored, name("new"));

    std::fs::rename(tmp.path().join("d/new"), tmp.path().join("e/new")).unwrap();
    let err = host::stored_name(&parent.handle.info, &name("new"), file, volume)
        .map(|_| ())
        .unwrap_err();
    assert!(err.to_string().contains("moved during the walk"), "{err}");
}

/// The commit renames the staged sibling by its handle with the held
/// directory as `RootDirectory`, over a target the walk held and into a name
/// the walk found absent alike; no sibling is left beside it.
#[test]
fn publish_through_file_rename_info_ex_lands_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("f"), b"old").unwrap();
    for (leaf, before) in [("f", "old"), ("g", "")] {
        let located = walked(&tmp.path().join(leaf));
        assert_eq!(located.stat().is_some(), !before.is_empty());
        let mut staged = located.stage().unwrap();
        staged.file().write_all(b"new").unwrap();
        staged.publish().unwrap();
        assert_eq!(read_all(&tmp.path().join(leaf)), "new");
    }
    let mut names: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["f", "g"]);
}

/// Windows holds nothing: a file removed and made again at its name is met by
/// the deny's position, and its id is not the old object's.
#[test]
fn a_recreated_file_meets_the_deny_by_position_under_another_id() {
    use crate::path::{Depth, MatchedBy, Subject};
    let tmp = tempfile::tempdir().unwrap();
    let secret = tmp.path().join("secret");
    std::fs::write(&secret, "old").unwrap();
    let excluded = walk_excluded(&spelled(&secret)).unwrap();
    let old = excluded.named().last_id();
    std::fs::remove_file(&secret).unwrap();
    std::fs::write(&secret, "new").unwrap();

    let met = walk_lenient(&spelled(&secret), LeafMode::Follow).unwrap();
    assert_ne!(met.last_id(), old);
    assert_eq!(
        excluded.matches(Subject::Access(&met), Depth::Within),
        Some(MatchedBy::Name)
    );
}
