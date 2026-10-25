//! The walk on a Windows volume.

use super::super::*;
use super::{writable, writing};
use crate::path::Resolver;
use std::io::{Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use windows_sys::Wdk::Storage::FileSystem::SYMLINK_FLAG_RELATIVE;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION,
    ERROR_SUCCESS, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetVolumeNameForVolumeMountPointW, READ_CONTROL,
};
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

fn spelled(path: &Path) -> LexicalPath {
    let cwd = crate::test_helper::process_cwd();
    Resolver::at(&crate::host::EnvVars::new(), &cwd).anchor(path)
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

/// A handle on `path` that asks attributes alone, which Windows grants for a
/// directory too, given backup semantics.
fn attributes_of(path: &Path) -> File {
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .unwrap()
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

fn icacls(path: &Path, args: &[&str]) {
    let output = std::process::Command::new(r"C:\Windows\System32\icacls.exe")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "icacls {path:?} {args:?}: {output:?}"
    );
}

/// A deny of `rights` to the current user on `path`, lifted when dropped.
struct Denied<'a> {
    path: &'a Path,
    user: String,
}

impl<'a> Denied<'a> {
    fn new(path: &'a Path, rights: &str) -> Self {
        let whoami = std::process::Command::new(r"C:\Windows\System32\whoami.exe")
            .output()
            .unwrap();
        let user = String::from_utf8(whoami.stdout).unwrap().trim().to_owned();
        icacls(path, &["/deny", &format!("{user}:({rights})")]);
        Self { path, user }
    }
}

impl Drop for Denied<'_> {
    fn drop(&mut self) {
        icacls(self.path, &["/remove:d", &self.user]);
    }
}

/// `path` spelled through the GUID of its volume, `\\?\Volume{…}\…`, which
/// has no drive letter.
fn volume_spelling(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        panic!("{path:?} has no drive");
    };
    let (Prefix::Disk(drive) | Prefix::VerbatimDisk(drive)) = prefix.kind() else {
        panic!("{path:?} is not on a drive");
    };
    let root: Vec<u16> = format!(r"{}:\", char::from(drive))
        .encode_utf16()
        .chain([0])
        .collect();
    let mut volume = [0u16; 64];
    // SAFETY: `root` is NUL-terminated and `volume` holds the 64 units the
    // call is told of.
    let ok = unsafe { GetVolumeNameForVolumeMountPointW(root.as_ptr(), volume.as_mut_ptr(), 64) };
    assert_ne!(ok, 0, "{path:?} is on a volume with no GUID path");
    let len = volume.iter().position(|&unit| unit == 0).unwrap();
    PathBuf::from(std::ffi::OsString::from_wide(&volume[..len]))
        .join(path.components().skip(2).collect::<PathBuf>())
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
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("Parent/Child")).unwrap();
    let handle = attributes_of(&tmp.path().join("parent").join("child"));
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

/// A volume whose 128-bit ids are zero, or which refuses `FileIdInfo` and the
/// reparse-tag class, walks and lists with the 64-bit id and a zero tag, and
/// agrees with [`ObjectId::of`]; it creates a directory under the held one,
/// and its atomic write fails, for want of `FileRenameInfoEx`, leaving no
/// sibling.  `RAL_TEST_FAT_VOLUME` names a directory on such a volume, a FAT
/// VHD, which may be mounted with no drive letter (a `\\?\Volume{GUID}\`
/// spelling).
#[test]
fn a_listing_without_128_bit_ids_falls_back_to_the_64_bit_id() {
    let Some(volume) = std::env::var_os("RAL_TEST_FAT_VOLUME") else {
        eprintln!("skip: RAL_TEST_FAT_VOLUME names no directory on a volume without 128-bit ids");
        return;
    };
    let tmp = tempfile::tempdir_in(volume).unwrap();
    let at = |entry: &str| tmp.path().join(entry);
    std::fs::write(at("f"), b"body").unwrap();
    std::fs::create_dir(at("d")).unwrap();
    let dir = entered(tmp.path());
    let mut listed = 0;
    for entry in dir.read_dir().unwrap() {
        let file = attributes_of(&at(&entry.name().as_os_str().to_string_lossy()));
        assert_eq!(entry.seen().unwrap().id, ObjectId::of(&file).unwrap());
        listed += 1;
    }
    assert_eq!(listed, 2);
    assert_eq!(
        walked(&at("f")).trace().last_id(),
        ObjectId::of(&attributes_of(&at("f"))).unwrap()
    );
    assert!(walked(&at("d")).stat().unwrap().reparse_tag.is_none());

    drop(
        writing(dir.absent(name("made")))
            .create_dir(Visibility::Public)
            .unwrap(),
    );
    assert!(at("made").is_dir());
    let mut staged = walked(&at("f")).stage(Visibility::Public).unwrap();
    staged.file().write_all(b"new").unwrap();
    assert!(staged.publish().is_err());
    assert_eq!(names_in(tmp.path()), ["d", "f", "made"]);
}

/// A directory step holds the directory in place by asking the traverse right
/// and sharing no delete, so a handle with `DELETE` access refuses it; a leaf
/// step asks attributes alone, which takes no part in the sharing check, and
/// is let by.
#[test]
fn a_directory_step_holds_by_traverse_and_a_leaf_step_by_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("d")).unwrap();
    let parent = entered(tmp.path());
    let step = |sharing| host::step(&parent.node.held, &name("d"), sharing).map(|_| ());

    let deleter = std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(tmp.path().join("d"))
        .unwrap();
    let err = step(Sharing::Directory).unwrap_err();
    assert_eq!(
        err.raw_os_error(),
        i32::try_from(ERROR_SHARING_VIOLATION).ok(),
        "{err}"
    );
    step(Sharing::Leaf).unwrap();
    drop(deleter);
    step(Sharing::Directory).unwrap();
}

/// An ancestor the user may pass through and not list is walked: the step
/// asks the traverse right, not the right to list, and only the listing,
/// which reopens the handle, refuses.
#[test]
fn an_ancestor_the_user_may_traverse_but_not_list_is_walked() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    std::fs::create_dir_all(a.join("b")).unwrap();
    std::fs::write(a.join("b/f"), b"x").unwrap();
    let _denied = Denied::new(&a, "RD");

    assert!(walked(&a.join("b/f")).stat().is_some());
    let err = entered(&a).read_dir().map(|_| ()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
}

/// A file whose traverse right is denied is a file where a directory was
/// expected, not a directory the user may not enter.
#[test]
fn a_file_in_a_directorys_place_is_not_a_directory_whatever_its_rights() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f");
    std::fs::write(&f, b"x").unwrap();
    let _denied = Denied::new(&f, "X");

    let err = walk(&spelled(&f.join("g")), LeafMode::Follow)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{err}");
}

/// A directory is made under the held handle, whatever its path, and a taken
/// name is `AlreadyExists`; a spelling through the volume's GUID, which has no
/// drive letter, lands in the same place.
#[test]
fn create_dir_makes_the_entry_under_the_held_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let create = |at: &Path, made: &str| {
        writing(entered(at).absent(name(made))).create_dir(Visibility::Public)
    };

    let made = create(tmp.path(), "made").unwrap();
    assert!(made.real().as_path().ends_with("made"), "{}", made.real());
    assert!(tmp.path().join("made").is_dir());
    let again = create(tmp.path(), "made").map(|_| ()).unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists, "{again}");

    let guid = volume_spelling(tmp.path());
    drop(create(&guid, "by_guid").unwrap());
    assert!(tmp.path().join("by_guid").is_dir());
    let again = create(&guid, "by_guid").map(|_| ()).unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists, "{again}");
}

/// A staged write that is dropped, and one whose commit fails, discard the
/// sibling by its own handle: the directory shows the target alone.
#[test]
fn a_dropped_or_failed_staged_write_leaves_no_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f");
    std::fs::write(&f, b"old").unwrap();

    drop(walked(&f).stage(Visibility::Public).unwrap());
    assert_eq!(names_in(tmp.path()), ["f"]);

    // The staged write holds `f`, so its name is moved aside rather than
    // deleted, which would stay pending until that handle closes.
    let mut staged = walked(&f).stage(Visibility::Public).unwrap();
    staged.file().write_all(b"new").unwrap();
    let old = tmp.path().join("old");
    std::fs::rename(&f, &old).unwrap();
    std::fs::create_dir(&f).unwrap();
    assert!(staged.publish().is_err());
    std::fs::remove_file(&old).unwrap();
    assert_eq!(names_in(tmp.path()), ["f"]);
    assert!(f.is_dir());
}

/// A read-only file is refused at the stage, before anything is made beside
/// it, not at the commit, where the rename would refuse it.
#[test]
fn a_read_only_file_is_not_staged_over() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f");
    std::fs::write(&f, b"old").unwrap();
    let mut permissions = std::fs::metadata(&f).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&f, permissions).unwrap();

    let err = walked(&f).stage(Visibility::Public).err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    assert_eq!(read_all(&f), "old");
    assert_eq!(names_in(tmp.path()), ["f"]);
}

/// A volume's refusal of an information class is the two codes FAT answers
/// and no other: a denial is not one.
#[test]
fn an_unsupported_class_is_the_two_codes_and_no_other() {
    let refused =
        |code: u32| host::unsupported_class(&io::Error::from_raw_os_error(code.cast_signed()));
    assert!(refused(ERROR_INVALID_PARAMETER));
    assert!(refused(ERROR_NOT_SUPPORTED));
    assert!(!refused(ERROR_ACCESS_DENIED));
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
    let child = host::step(&parent.node.held, &name("old"), Sharing::Leaf).unwrap();
    let file = child.handle.as_ref().unwrap();
    let volume = child.step.id.volume;

    std::fs::rename(tmp.path().join("d/old"), tmp.path().join("d/new")).unwrap();
    let (stored, _) =
        host::stored_name(&parent.node.held.info, &name("old"), file, volume).unwrap();
    assert_eq!(stored, name("new"));

    std::fs::rename(tmp.path().join("d/new"), tmp.path().join("e/new")).unwrap();
    let err = host::stored_name(&parent.node.held.info, &name("new"), file, volume)
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
        let mut staged = located.stage(Visibility::Public).unwrap();
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

/// What the DACL of `path` holds: whether it is protected from inheritance,
/// and for each entry whether it allows, whether it names the process's owner,
/// and the rights it grants.
fn dacl_of(path: &Path) -> (bool, Vec<(bool, bool, u32)>) {
    let file = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .unwrap();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `file` is a live handle with `READ_CONTROL`, and `dacl` and `sd`
    // are the out-parameters the call fills.
    let rc = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    assert_eq!(rc, ERROR_SUCCESS, "{path:?}");
    let (mut control, mut revision) = (0u16, 0u32);
    // SAFETY: `sd` is the descriptor `GetSecurityInfo` allocated.
    let ok = unsafe { GetSecurityDescriptorControl(sd, &raw mut control, &raw mut revision) };
    assert_ne!(ok, 0);
    let owner = host::owner_only::current_process_owner().unwrap();
    // SAFETY: `owner` holds a populated `TOKEN_USER` in a `u64` buffer.
    let owner_sid = unsafe { (*owner.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: a non-null `dacl` is part of the descriptor `sd`.
    let count = if dacl.is_null() {
        0
    } else {
        unsafe { (*dacl).AceCount }
    };
    let aces = (0..count)
        .map(|index| {
            let mut ace = std::ptr::null_mut();
            // SAFETY: the index is in range and `sd` outlives the entry.
            assert_ne!(unsafe { GetAce(dacl, u32::from(index), &raw mut ace) }, 0);
            let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: allow and deny ACEs share this prefix, and each ACE
            // is DWORD-aligned inside the ACL.
            unsafe {
                let sid = (&raw mut (*ace).SidStart).cast();
                (
                    (*ace).Header.AceType == 0,
                    EqualSid(sid, owner_sid) != 0,
                    (*ace).Mask,
                )
            }
        })
        .collect();
    // SAFETY: `sd` came from `GetSecurityInfo`, which allocates through
    // `LocalAlloc`, and nothing reads it after this.
    unsafe { LocalFree(sd) };
    (control & SE_DACL_PROTECTED != 0, aces)
}

/// `Secret` is a protected DACL naming the owner alone, over a leaf that
/// inherits whatever its directory hands down; `Public` and `Private` inherit.
#[test]
fn a_staged_write_is_owner_only_when_it_is_secret() {
    let tmp = tempfile::tempdir().unwrap();
    for (visibility, owner_only) in [
        (Visibility::Public, false),
        (Visibility::Private, false),
        (Visibility::Secret, true),
    ] {
        let at = tmp.path().join(format!("{visibility:?}"));
        std::fs::write(&at, b"old").unwrap();
        assert!(!dacl_of(&at).0, "the leaf inherits before it is replaced");
        let mut staged = walked(&at).stage(visibility).unwrap();
        staged.file().write_all(b"new").unwrap();
        staged.publish().unwrap();
        assert_eq!(read_all(&at), "new");
        let (protected, aces) = dacl_of(&at);
        assert_eq!(protected, owner_only, "{visibility:?}");
        if owner_only {
            assert_eq!(aces, [(true, true, FILE_ALL_ACCESS)]);
        }
    }
}

/// A file made at an absent leaf is made as asked, with no instant under an
/// inherited ACL, and a taken name is refused, a link included.
#[test]
fn a_data_open_makes_an_absent_leaf_as_asked() {
    let tmp = tempfile::tempdir().unwrap();
    for (visibility, owner_only) in [
        (Visibility::Public, false),
        (Visibility::Private, false),
        (Visibility::Secret, true),
    ] {
        let at = tmp.path().join(format!("{visibility:?}"));
        let mut file = walked(&at).open_data(Access::Write, visibility).unwrap();
        file.write_all(b"made").unwrap();
        drop(file);
        assert_eq!(read_all(&at), "made");
        let (protected, aces) = dacl_of(&at);
        assert_eq!(protected, owner_only, "{visibility:?}");
        if owner_only {
            assert_eq!(aces, [(true, true, FILE_ALL_ACCESS)]);
        }
    }

    let absent = walked(&tmp.path().join("raced"));
    std::fs::write(tmp.path().join("raced"), b"first").unwrap();
    let err = absent
        .open_data(Access::Write, Visibility::Secret)
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(read_all(&tmp.path().join("raced")), "first");

    let appended = tmp.path().join("appended");
    let mut file = walked(&appended)
        .open_data(Access::Append, Visibility::Private)
        .unwrap();
    file.write_all(b"a").unwrap();
    file.write_all(b"b").unwrap();
    drop(file);
    assert_eq!(read_all(&appended), "ab");
}

/// The making walk makes each absent name of the spelling and steps onto it,
/// and takes a present name as it is.
#[test]
fn the_making_walk_makes_each_absent_name() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("present")).unwrap();

    let made = walk_dir_made(&spelled(&tmp.path().join("present/a/b"))).unwrap();
    assert!(
        made.real().as_path().ends_with(Path::new("present/a/b")),
        "{}",
        made.real()
    );
    assert!(tmp.path().join("present/a/b").is_dir());
    let again = walk_dir_made(&spelled(&tmp.path().join("present/a/b"))).unwrap();
    assert_eq!(again.trace().last_id(), made.trace().last_id());

    std::fs::write(tmp.path().join("file"), b"").unwrap();
    let err = walk_dir_made(&spelled(&tmp.path().join("file/sub")))
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{err}");
}

/// A removal is by the handle the walk holds: a tree goes with everything in
/// it, a read-only file and a file someone else holds open included, since the
/// name goes at once.
#[test]
fn remove_takes_a_tree_by_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir_all(tree.join("sub/deeper")).unwrap();
    std::fs::write(tree.join("f"), b"").unwrap();
    std::fs::write(tree.join("sub/g"), b"").unwrap();
    std::fs::write(tree.join("sub/deeper/h"), b"").unwrap();
    let mut permissions = std::fs::metadata(tree.join("sub/g")).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(tree.join("sub/g"), permissions).unwrap();
    let open = std::fs::File::open(tree.join("f")).unwrap();
    std::fs::write(tmp.path().join("kept"), b"kept").unwrap();

    walked(&tree).remove().unwrap();
    assert!(!tree.exists());
    drop(open);
    assert_eq!(read_all(&tmp.path().join("kept")), "kept");

    std::fs::write(tmp.path().join("one"), b"").unwrap();
    walked(&tmp.path().join("one")).remove().unwrap();
    assert!(!tmp.path().join("one").exists());
    let err = walked(&tmp.path().join("absent")).remove().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

/// A rename is by the handle the walk holds, in the directory it is in, over
/// whatever is at the new name, even a file someone else holds open.
#[test]
fn rename_replaces_by_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let at = |entry: &str| tmp.path().join(entry);
    std::fs::write(at("f"), b"new").unwrap();
    std::fs::write(at("g"), b"old").unwrap();
    let open = std::fs::File::open(at("g")).unwrap();

    walked(&at("f")).rename(&name("g")).unwrap();
    assert!(!at("f").exists());
    assert_eq!(read_all(&at("g")), "new");
    drop(open);

    walked(&at("g")).rename(&name("fresh")).unwrap();
    assert_eq!(read_all(&at("fresh")), "new");
    assert_eq!(names_in(tmp.path()), ["fresh"]);

    std::fs::create_dir(at("d")).unwrap();
    walked(&at("d")).rename(&name("e")).unwrap();
    assert!(at("e").is_dir() && !at("d").exists());

    let err = walked(&at("absent")).rename(&name("x")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}
