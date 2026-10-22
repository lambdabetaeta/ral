//! The Windows host: a step opens its child relative to the directory held
//! and reads everything about it from the handle it gets back.  A directory
//! step holds the directory itself in place: it asks the traverse right and
//! shares no delete, which an attribute-only open would not enforce
//! (MS-FSA 2.1.5.1.2.2); a leaf step blocks no one.  So a held directory's
//! path is the one the walk recorded when it opened it, and the stored name
//! of a child, which is read from the child, is checked against that path.
//! Every create is relative to a held handle (`NtCreateFile` with
//! `RootDirectory`), and no path is ever derived from a handle.

use super::{
    Access, Child, EntryName, Held, Kind, ObjectId, RootName, Seen, Sharing, Stat, Step, Visibility,
};
use crate::path::shown;
use cap_fs_ext::OpenOptionsFollowExt;
use cap_primitives::fs::{FollowSymlinks, OpenOptions, OpenOptionsExt};
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::mem::offset_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr::null;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::vec::IntoIter;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    NtCreateFile, SYMLINK_FLAG_RELATIVE,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NO_MORE_FILES,
    ERROR_NOT_SUPPORTED, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE,
    RtlNtStatusToDosError, STATUS_SUCCESS, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO,
    FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_NAME_INFO,
    FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_WRITE_DATA, FileAttributeTagInfo, FileBasicInfo,
    FileDispositionInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
    FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo, FileNormalizedNameInfo,
    FileRenameInfoEx, FileStandardInfo, GetDriveTypeW, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
    ReOpenFile, SYNCHRONIZE, SetFileInformationByHandle, VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};
use windows_sys::Win32::System::WindowsProgramming::{
    DRIVE_FIXED, FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
};

/// What the host records of a directory as it opens it: its normalized path
/// on its volume, and the volume's serial, which the step of a child checks
/// the child's own against.
#[derive(Clone)]
pub(super) struct DirInfo {
    path: Vec<u16>,
    volume: u64,
}

const SEPARATOR: u16 = 0x5c;

/// A walk's refusal of an entry that is no longer one of the directory it
/// was opened in.
fn moved(name: &EntryName) -> io::Error {
    io::Error::other(format!(
        "{} moved during the walk: it is no longer an entry of the directory the walk held when it opened it",
        shown(name.as_os_str())
    ))
}

/// A zeroed buffer aligned for any struct the kernel fills in it, read back
/// as bytes.
struct Buf(Vec<u64>);

impl Buf {
    fn new(bytes: usize) -> Self {
        Self(vec![0; bytes.div_ceil(8)])
    }

    fn size(&self) -> u32 {
        u32::try_from(self.0.len() * 8).expect("a buffer is under four gibibytes")
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr().cast()
    }

    fn bytes(&self) -> Vec<u8> {
        self.0.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

fn short() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the answer is shorter than its class says",
    )
}

fn at<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    let raw = bytes.get(offset..offset + N).ok_or_else(short)?;
    raw.try_into().map_err(|_| short())
}

fn u16_at(bytes: &[u8], offset: usize) -> io::Result<u16> {
    at(bytes, offset).map(u16::from_le_bytes)
}

fn u32_at(bytes: &[u8], offset: usize) -> io::Result<u32> {
    at(bytes, offset).map(u32::from_le_bytes)
}

fn i64_at(bytes: &[u8], offset: usize) -> io::Result<i64> {
    at(bytes, offset).map(i64::from_le_bytes)
}

/// `len` bytes at `offset` as UTF-16 code units.
fn wide_at(bytes: &[u8], offset: usize, len: usize) -> io::Result<Vec<u16>> {
    let raw = bytes.get(offset..offset + len).ok_or_else(short)?;
    Ok(raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&unit| u16::from_le_bytes(unit))
        .collect())
}

/// What `class` of `f` answers, as the struct it names.
fn query<T: Default>(f: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
    let mut info = T::default();
    // SAFETY: `f` is a live handle, and `info` is the struct `class` names,
    // of the size the call is told.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            f.as_raw_handle(),
            class,
            (&raw mut info).cast(),
            u32::try_from(size_of::<T>()).expect("an info struct is small"),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// A new handle on the object `f` holds, with `rights`, sharing `share`.
fn reopen(f: &File, rights: u32, share: u32, flags: u32) -> io::Result<File> {
    // SAFETY: `f` is a live handle; the call answers a new one or
    // `INVALID_HANDLE_VALUE`.
    let handle = unsafe { ReOpenFile(f.as_raw_handle(), rights, share, flags) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a fresh handle this call owns.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// A new handle on the object `f` holds with `access`, sharing read, write
/// and delete: what a DACL stamp needs beyond the attributes a held handle
/// asks.  Derived from the held handle, so no name is resolved again, and a
/// reparse point is the object it is, never followed.
pub(super) fn reopen_shared(f: &File, access: u32) -> io::Result<File> {
    reopen(
        f,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Whether the object `f` holds is on a local fixed volume: the volume's GUID
/// path asked of the handle, then that volume's drive type.  A share has no
/// GUID path, so it is not local, nor is a volume that cannot be named.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-volume] asks a held handle for the GUID path of its volume, to say whether the volume is local; the handle's own answer, no name is resolved"
)]
pub(super) fn is_local(f: &File) -> bool {
    let mut path = vec![0u16; 512];
    let len = loop {
        let capacity = u32::try_from(path.len()).expect("a path buffer is under four gibibytes");
        // SAFETY: `f` is a live handle, and `path` is `capacity` units long.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                f.as_raw_handle(),
                path.as_mut_ptr(),
                capacity,
                VOLUME_NAME_GUID,
            )
        } as usize;
        match len {
            0 => return false,
            n if n >= path.len() => path.resize(n + 1, 0),
            n => break n,
        }
    };
    // `\\?\Volume{GUID}\…`: the volume's root runs through the fourth separator.
    let root_len = path[..len]
        .iter()
        .enumerate()
        .filter(|&(_, &unit)| unit == SEPARATOR)
        .nth(3)
        .map(|(at, _)| at + 1);
    let Some(root_len) = root_len else {
        return false;
    };
    let mut root = path[..root_len].to_vec();
    root.push(0);
    // SAFETY: `root` is a NUL-terminated wide string that outlives the call.
    unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED }
}

/// The root a walk descends from, held without delete sharing, with its own
/// normalized path and volume recorded.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-root] Opens a drive, share or volume root as a directory handle, the root every walk descends from: path resolution, not the model's data I/O."
)]
pub(super) fn open_root(root: &RootName) -> io::Result<Held> {
    let file = cap_primitives::fs::open_ambient_dir(&root.0, cap_primitives::ambient_authority())?;
    let info = DirInfo {
        path: normalized(&file)?,
        volume: seen(&file)?.id.volume,
    };
    Ok(Held {
        file: Arc::new(file),
        info,
    })
}

/// The entry `name` of `dir`, never following a final link.  A directory step
/// asks the traverse right, the one data-class right every passable directory
/// grants, and shares no delete: that is what makes the sharing check hold the
/// directory in place (MS-FSA 2.1.5.1.2.2 ignores an attribute-only open).  A
/// leaf asks attributes alone and shares delete, so it blocks nobody.  Where
/// the traverse right is refused, attributes alone say what the entry is: a
/// directory is unenterable, and anything else is handed on for the engine to
/// classify.
///
/// # Errors
/// `PermissionDenied` for a directory the user may not traverse; the open's.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-step] Opens one component relative to a held directory and never follows it, to reach the object a grant will judge: path resolution, not the model's data I/O; the card belongs to the site that then opens the leaf."
)]
fn open_entry(dir: &File, name: &OsStr, sharing: Sharing) -> io::Result<File> {
    const LEAF: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let open = |access, share| {
        let mut options = OpenOptions::new();
        options
            .access_mode(access)
            .share_mode(share)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .follow(FollowSymlinks::No);
        cap_primitives::fs::open(dir, name.as_ref(), &options)
    };
    match sharing {
        Sharing::Leaf => open(FILE_READ_ATTRIBUTES, LEAF),
        Sharing::Directory => match open(
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        ) {
            Err(e) if e.raw_os_error() == i32::try_from(ERROR_ACCESS_DENIED).ok() => {
                let file = open(FILE_READ_ATTRIBUTES, LEAF)?;
                let basic: FILE_BASIC_INFO = query(&file, FileBasicInfo)?;
                let tag = reparse_tag(&file, basic.FileAttributes)?;
                if kind_of(basic.FileAttributes, tag) == Kind::Dir {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("{} cannot be traversed", shown(name)),
                    ));
                }
                Ok(file)
            }
            opened => opened,
        },
    }
}

/// The entry `name` of `parent`, opened and then named as `parent` stores it.
pub(super) fn step(parent: &Held, name: &EntryName, sharing: Sharing) -> io::Result<Child> {
    let file = open_entry(&parent.file, name.as_os_str(), sharing)?;
    let Seen { stat, id } = seen(&file)?;
    let (stored, info) = stored_name(&parent.info, name, &file, id.volume)?;
    let step = Step {
        name: stored,
        id,
        kind: stat.kind,
    };
    Ok(Child {
        handle: Some(file),
        step,
        stat,
        info,
    })
}

/// The name the volume stores for `child`, opened as the entry `name` of the
/// directory `parent` describes, and `child`'s own record.  The handle's
/// normalized path — long names, stored case, no alias — must be `parent`'s
/// plus one component and its volume `parent`'s, else the entry moved: the
/// directory's own path cannot have changed, since it is held without delete
/// sharing.  A leaf renamed within its directory since its open is adopted
/// at its new name, which is where it is.
pub(super) fn stored_name(
    parent: &DirInfo,
    name: &EntryName,
    child: &File,
    volume: u64,
) -> io::Result<(EntryName, DirInfo)> {
    let path = normalized(child)?;
    let above = parent
        .path
        .strip_suffix(&[SEPARATOR])
        .unwrap_or(&parent.path);
    let stored = path
        .strip_prefix(above)
        .and_then(|rest| rest.strip_prefix(&[SEPARATOR]))
        .filter(|stored| !stored.is_empty() && !stored.contains(&SEPARATOR));
    match stored {
        Some(stored) if volume == parent.volume => Ok((
            EntryName::parse(&OsString::from_wide(stored))?,
            DirInfo { path, volume },
        )),
        _ => Err(moved(name)),
    }
}

/// `f`'s path on its volume, as `FileNormalizedNameInfo` answers: long names
/// and stored case.  `FileNameInfo` would answer the name the handle was
/// opened by, alias and case included.
pub(super) fn normalized(f: &File) -> io::Result<Vec<u16>> {
    let mut size = 1024;
    loop {
        let mut buf = Buf::new(size);
        // SAFETY: `f` is a live handle and `buf` is as large as the call is
        // told.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                f.as_raw_handle(),
                FileNormalizedNameInfo,
                buf.as_mut_ptr(),
                buf.size(),
            )
        };
        if ok != 0 {
            let bytes = buf.bytes();
            let len = usize::try_from(u32_at(&bytes, 0)?).map_err(|_| short())?;
            return wide_at(&bytes, offset_of!(FILE_NAME_INFO, FileName), len);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == i32::try_from(ERROR_MORE_DATA).ok() && size < 1 << 18 {
            size *= 4;
            continue;
        }
        return Err(io::Error::new(
            error.kind(),
            format!(
                "cannot learn the stored name of an entry, which ral needs: FileNormalizedNameInfo is Windows 10 1709 or later ({error})"
            ),
        ));
    }
}

/// A reparse tag that names another path: what makes an object a link.
const fn is_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

/// What an object with these attributes and reparse tag is.  A reparse point
/// whose tag names no path — a cloud placeholder — is the file or directory
/// it presents, not a link.
pub(super) fn kind_of(attributes: u32, tag: u32) -> Kind {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && is_surrogate(tag) {
        Kind::Symlink
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Dir
    } else {
        Kind::File
    }
}

/// A `FILETIME`'s ticks, a hundred nanoseconds from 1601, as the instant they
/// name; `None` for the zero a volume answers where it records none.
fn time(ticks: i64) -> Option<SystemTime> {
    const UNIX_AFTER_1601: u64 = 11_644_473_600;
    const TICKS_PER_SECOND: u64 = 10_000_000;
    let ticks = u64::try_from(ticks).ok().filter(|&t| t != 0)?;
    let nanos = u32::try_from(ticks % TICKS_PER_SECOND).ok()? * 100;
    let since = Duration::new(ticks / TICKS_PER_SECOND, nanos);
    UNIX_EPOCH
        .checked_sub(Duration::from_secs(UNIX_AFTER_1601))?
        .checked_add(since)
}

/// What a directory listing or a handle query answers of one object.
struct Raw {
    attributes: u32,
    tag: u32,
    len: i64,
    btime: i64,
    atime: i64,
    mtime: i64,
}

impl Raw {
    fn stat(&self) -> Stat {
        let reparse = self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        Stat {
            kind: kind_of(self.attributes, self.tag),
            len: u64::try_from(self.len).unwrap_or_default(),
            readonly: self.attributes & FILE_ATTRIBUTE_READONLY != 0,
            mtime: time(self.mtime),
            atime: time(self.atime),
            btime: time(self.btime),
            reparse_tag: reparse.then_some(self.tag),
            hidden: self.attributes & FILE_ATTRIBUTE_HIDDEN != 0,
        }
    }
}

/// The serial and 64-bit index `GetFileInformationByHandle` answers, which a
/// volume with no 128-bit ids still has.
fn by_handle(f: &File) -> io::Result<(u32, u64)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `f` is a live handle and `info` the struct the call fills.
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow),
    ))
}

/// Whether `e` is a volume's refusal of an information class, as FAT answers
/// the ones NTFS and `ReFS` add.
pub(super) fn unsupported_class(e: &io::Error) -> bool {
    [ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED]
        .into_iter()
        .any(|code| e.raw_os_error() == i32::try_from(code).ok())
}

/// `FileIdInfo`, or the 64-bit index where the volume's 128-bit id is zero.
/// On a volume that refuses the class both the serial and the index come from
/// `BY_HANDLE_FILE_INFORMATION`; every identity on that volume is read the
/// same way, so they agree with one another, and identity is only ever
/// compared negatively.
fn object_id(f: &File) -> io::Result<ObjectId> {
    match query::<FILE_ID_INFO>(f, FileIdInfo) {
        Ok(info) => {
            let id = u128::from_le_bytes(info.FileId.Identifier);
            Ok(ObjectId {
                volume: info.VolumeSerialNumber,
                id: if id == 0 {
                    u128::from(by_handle(f)?.1)
                } else {
                    id
                },
            })
        }
        Err(e) if unsupported_class(&e) => {
            let (serial, index) = by_handle(f)?;
            Ok(ObjectId {
                volume: u64::from(serial),
                id: u128::from(index),
            })
        }
        Err(e) => Err(e),
    }
}

/// The reparse tag of `f`, whose `FileBasicInfo` attributes are `attributes`:
/// a volume that cannot answer a tag has no reparse points.
fn reparse_tag(f: &File, attributes: u32) -> io::Result<u32> {
    match query::<FILE_ATTRIBUTE_TAG_INFO>(f, FileAttributeTagInfo) {
        Ok(tag) => Ok(tag.ReparseTag),
        Err(e) if unsupported_class(&e) && attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 => Ok(0),
        Err(e) => Err(e),
    }
}

/// What `f` is: `FileBasicInfo`, `FileAttributeTagInfo` and
/// `FileStandardInfo`, which carries the length the first two do not, and
/// its identity.
pub(super) fn seen(f: &File) -> io::Result<Seen> {
    let basic: FILE_BASIC_INFO = query(f, FileBasicInfo)?;
    let tag = reparse_tag(f, basic.FileAttributes)?;
    let standard: FILE_STANDARD_INFO = query(f, FileStandardInfo)?;
    let raw = Raw {
        attributes: basic.FileAttributes,
        tag,
        len: standard.EndOfFile,
        btime: basic.CreationTime,
        atime: basic.LastAccessTime,
        mtime: basic.LastWriteTime,
    };
    Ok(Seen {
        stat: raw.stat(),
        id: object_id(f)?,
    })
}

/// A link's target, read from the reparse buffer of the handle that opened
/// it.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-link-read] Reads a link's target through the handle that opened it, to splice into the remaining name: path resolution, not data I/O."
)]
pub(super) fn read_link(link: &File) -> io::Result<PathBuf> {
    let mut buf = Buf::new(MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize);
    let mut written = 0u32;
    // SAFETY: `link` is a live handle, and `buf` and `written` outlive the
    // call, which writes at most the `buf.size()` bytes it is told of.
    let ok = unsafe {
        DeviceIoControl(
            link.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buf.as_mut_ptr(),
            buf.size(),
            &raw mut written,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = buf.bytes();
    decode(
        bytes
            .get(..usize::try_from(written).map_err(|_| short())?)
            .ok_or_else(short)?,
    )
}

/// The target a reparse buffer holds, as a path `rooted` can judge.
/// `winx::file::read_link` strips the NT prefix `\??\` and leaves
/// `UNC\srv\share\x` and `Volume{GUID}\x` looking relative, so the buffer is
/// read here: a symlink with `SYMLINK_FLAG_RELATIVE` holds a relative
/// target; otherwise, and for a mount point, the substitute name is an NT
/// path, `\??\C:\x` read as `C:\x`, `\??\UNC\srv\share\x` as
/// `\\srv\share\x` and `\??\Volume{GUID}\x` as `\\?\Volume{GUID}\x`.
///
/// # Errors
/// `InvalidInput` for a tag that is neither a symlink's nor a mount point's.
pub(super) fn decode(buf: &[u8]) -> io::Result<PathBuf> {
    let (names, relative) = match u32_at(buf, 0)? {
        IO_REPARSE_TAG_SYMLINK => (20, u32_at(buf, 16)? & SYMLINK_FLAG_RELATIVE != 0),
        IO_REPARSE_TAG_MOUNT_POINT => (16, false),
        tag => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("reparse tag {tag:#x} names no path, so it is no link"),
            ));
        }
    };
    let start = names + usize::from(u16_at(buf, 8)?);
    let substitute = wide_at(buf, start, usize::from(u16_at(buf, 10)?))?;
    let target = if relative {
        substitute
    } else {
        win32_path(&substitute)
    };
    Ok(PathBuf::from(OsString::from_wide(&target)))
}

/// An NT path as the Win32 path that names the same object.
fn win32_path(nt: &[u16]) -> Vec<u16> {
    let units = |s: &str| s.encode_utf16().collect::<Vec<_>>();
    let Some(rest) = nt.strip_prefix(units(r"\??\").as_slice()) else {
        return nt.to_vec();
    };
    if let Some(share) = rest.strip_prefix(units(r"UNC\").as_slice()) {
        [units(r"\\"), share.to_vec()].concat()
    } else if rest.get(1) == units(":").first() {
        rest.to_vec()
    } else {
        [units(r"\\?\"), rest.to_vec()].concat()
    }
}

/// The rights a data handle of `access` asks.
fn rights(access: Access) -> u32 {
    match access {
        Access::Read => GENERIC_READ,
        Access::Append => FILE_GENERIC_WRITE & !FILE_WRITE_DATA,
        Access::Write => GENERIC_WRITE,
    }
}

/// A data handle on the object `leaf` holds, sharing read, write and delete.
/// `ReOpenFile` derives it from the held handle, so no name is resolved
/// again; a reparse point that names no path, a cloud placeholder, is
/// instead opened by `name` under `dir` and verified against the held
/// handle's identity.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-reopen] The one open of a held leaf, derived from its handle so no name is resolved twice.  Every caller surfaces it, each in its own way: a redirect's card is fused on by the frame that wrapped the locate, exarch's readers speak their own card, and a write's before- and after-images ride the card of the operation that asked for them."
)]
pub(super) fn derive(
    leaf: &File,
    dir: &File,
    name: &EntryName,
    stat: &Stat,
    access: Access,
) -> io::Result<File> {
    if stat.reparse_tag.is_none() {
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        return reopen(leaf, rights(access), share, 0);
    }
    let mut options = OpenOptions::new();
    options
        .access_mode(rights(access))
        .follow(FollowSymlinks::No);
    let file = cap_primitives::fs::open(dir, name.as_os_str().as_ref(), &options)?;
    if ObjectId::of(&file)? != ObjectId::of(leaf)? {
        return Err(moved(name));
    }
    Ok(file)
}

/// The options of a data open of `access`, never following.
fn data_options(access: Access) -> OpenOptions {
    let mut options = OpenOptions::new();
    match access {
        Access::Read => options.read(true),
        Access::Append => options.append(true),
        Access::Write => options.write(true),
    }
    .follow(FollowSymlinks::No);
    options
}

/// The entry `name` made exclusively for `access`: what a leaf absent at the
/// walk becomes.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-create] Creates an absent leaf exclusively beside the directory the walk holds, so a file that appeared since the walk is refused, never written.  The write that follows surfaces it."
)]
pub(super) fn create(
    dir: &File,
    name: &EntryName,
    access: Access,
    _visibility: Visibility,
) -> io::Result<File> {
    let mut options = data_options(access);
    options.create_new(true);
    cap_primitives::fs::open(dir, name.as_os_str().as_ref(), &options)
}

/// A fresh sibling, read and write, that no one else has, with `DELETE`
/// access, which renaming it by handle needs.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-stage] The atomic `>` staging create: a fresh exclusive sibling in the target's own directory, holding the write until the rename publishes it.  A sub-step of the write site; the reported write is the operation's surface."
)]
pub(super) fn create_sibling(dir: &File, name: &EntryName) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .follow(FollowSymlinks::No);
    cap_primitives::fs::open(dir, name.as_os_str().as_ref(), &options)
}

/// Make the directory `name` in `dir`: `NtCreateFile` with `RootDirectory`
/// the held handle, so the entry lands under it whatever its path is, a share
/// or a volume with no drive letter included.  The handle it opens is closed
/// at once; the caller steps onto the name.
///
/// # Errors
/// `AlreadyExists` where the name is taken; the create's.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-mkdir] Makes an absent entry of a held directory, which the caller judged as the absent leaf it is; the directory is then stepped onto."
)]
pub(super) fn mkdir(dir: &File, name: &EntryName, _visibility: Visibility) -> io::Result<()> {
    let mut units: Vec<u16> = name.as_os_str().encode_wide().collect();
    let bytes = u16::try_from(units.len() * size_of::<u16>())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidFilename))?;
    let object = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: units.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).expect("a struct is small"),
        RootDirectory: dir.as_raw_handle(),
        ObjectName: &raw const object,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let mut block = IO_STATUS_BLOCK::default();
    // SAFETY: `dir` is a live handle, and `attributes` names `object`, whose
    // buffer `units` holds, `bytes` long; all outlive the call, which fills
    // `handle` and `block`.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &raw const attributes,
            &raw mut block,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_CREATE,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            null(),
            0,
        )
    };
    if status != STATUS_SUCCESS {
        // SAFETY: a plain conversion of a status code.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code.cast_signed()));
    }
    // SAFETY: `handle` is a fresh handle this call owns.
    drop(unsafe { OwnedHandle::from_raw_handle(handle) });
    Ok(())
}

/// Rename `sibling`, the staged object, over the entry `to` of `dir`, by its
/// handle: `FileRenameInfoEx` with `RootDirectory` the held directory, so the
/// rename lands in it whatever its name now is, replacing the target and
/// leaving a handle on it open.  The sibling's name is not asked.  A volume
/// or host that refuses the class fails the write with that error; nothing
/// falls back to a rename by name.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-publish] The atomic `>` commit step: rename the staged sibling over the target's name within the one directory handle.  The write surface fires when the write settles, committed once this returns Ok."
)]
pub(super) fn rename_over(
    dir: &File,
    sibling: &File,
    _from: &EntryName,
    to: &EntryName,
) -> io::Result<()> {
    let name: Vec<u16> = to.as_os_str().encode_wide().collect();
    let name_bytes = name.len() * size_of::<u16>();
    let mut buf = Buf::new(size_of::<FILE_RENAME_INFO>() + name_bytes);
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buf` is zeroed, aligned for the struct and as long as it plus
    // the name, which is copied in whole beside the one unit it carries.
    unsafe {
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_POSIX_SEMANTICS | FILE_RENAME_FLAG_REPLACE_IF_EXISTS;
        (*info).RootDirectory = dir.as_raw_handle();
        (*info).FileNameLength = u32::try_from(name_bytes).map_err(|_| short())?;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            name.len(),
        );
    }
    let size = buf.size();
    // SAFETY: `sibling` is a live handle, and `info` is the struct the class
    // names, `size` bytes of it.
    let ok = unsafe {
        SetFileInformationByHandle(sibling.as_raw_handle(), FileRenameInfoEx, info.cast(), size)
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        return Err(
            if error.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() {
                io::Error::new(
                    error.kind(),
                    format!(
                        "the atomic write needs FileRenameInfoEx, which Windows 10 1809 and NTFS or ReFS answer ({error})"
                    ),
                )
            } else {
                error
            },
        );
    }
    Ok(())
}

/// Mark the staged `sibling` for deletion by its own handle: it was created
/// with `DELETE` access, and its name goes when the handle closes.  No name
/// is resolved.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-abandon] Reasoned-silent rollback of the atomic `>`: discard the staged sibling, by its own handle, for a write that will not land.  The aborted write is the surface; this removal raises none of its own."
)]
pub(super) fn discard(_dir: &File, sibling: &File, _name: &EntryName) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `sibling` is a live handle, and `info` is the struct the class
    // names, of the size the call is told.
    let ok = unsafe {
        SetFileInformationByHandle(
            sibling.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).expect("an info struct is small"),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the entry `name` of `dir` may be opened for writing, following a
/// link.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-access] The `is-writable` predicate: whether a write open would be granted, by name under the held directory.  A permission predicate, not turn-time model data I/O."
)]
pub(super) fn is_writable(dir: &File, name: &OsStr) -> bool {
    cap_primitives::fs::access(
        dir,
        name.as_ref(),
        cap_primitives::fs::AccessType::Access(cap_primitives::fs::AccessModes {
            readable: false,
            writable: true,
            executable: false,
        }),
        FollowSymlinks::Yes,
    )
    .is_ok()
}

/// The discard device, by its fixed name.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:discard-device] `\\\\.\\NUL` opened by its fixed name: no bytes reach or leave the model, and no grant region can contain a device that is not a file."
)]
pub(super) fn open_discard() -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(r"\\.\NUL")
}

/// One entry of a directory pass: its name, what it answered, and its id.
struct Row {
    name: Vec<u16>,
    raw: Raw,
    id: u128,
}

/// The entries `class` of `reader` lists, `restart` first, from one pass.
fn rows(
    reader: &File,
    restart: FILE_INFO_BY_HANDLE_CLASS,
    next: FILE_INFO_BY_HANDLE_CLASS,
    row: fn(&[u8]) -> io::Result<Row>,
) -> io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut class = restart;
    loop {
        let mut buf = Buf::new(1 << 16);
        // SAFETY: `reader` is a live handle and `buf` as large as the call is
        // told.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                reader.as_raw_handle(),
                class,
                buf.as_mut_ptr(),
                buf.size(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == i32::try_from(ERROR_NO_MORE_FILES).ok() {
                return Ok(rows);
            }
            return Err(error);
        }
        class = next;
        let bytes = buf.bytes();
        let mut offset = 0;
        loop {
            let entry = bytes.get(offset..).ok_or_else(short)?;
            rows.push(row(entry)?);
            match usize::try_from(u32_at(entry, 0)?).map_err(|_| short())? {
                0 => break,
                step => offset += step,
            }
        }
    }
}

fn extd_row(entry: &[u8]) -> io::Result<Row> {
    type Info = FILE_ID_EXTD_DIR_INFO;
    let len =
        usize::try_from(u32_at(entry, offset_of!(Info, FileNameLength))?).map_err(|_| short())?;
    Ok(Row {
        name: wide_at(entry, offset_of!(Info, FileName), len)?,
        raw: Raw {
            attributes: u32_at(entry, offset_of!(Info, FileAttributes))?,
            tag: u32_at(entry, offset_of!(Info, ReparsePointTag))?,
            len: i64_at(entry, offset_of!(Info, EndOfFile))?,
            btime: i64_at(entry, offset_of!(Info, CreationTime))?,
            atime: i64_at(entry, offset_of!(Info, LastAccessTime))?,
            mtime: i64_at(entry, offset_of!(Info, LastWriteTime))?,
        },
        id: u128::from_le_bytes(at(entry, offset_of!(Info, FileId))?),
    })
}

/// `FileIdBothDirectoryInfo`'s row, whose id has 64 bits and whose `EaSize`
/// holds the reparse tag of a reparse point.
fn both_row(entry: &[u8]) -> io::Result<Row> {
    type Info = FILE_ID_BOTH_DIR_INFO;
    let len =
        usize::try_from(u32_at(entry, offset_of!(Info, FileNameLength))?).map_err(|_| short())?;
    let attributes = u32_at(entry, offset_of!(Info, FileAttributes))?;
    let tag = u32_at(entry, offset_of!(Info, EaSize))?;
    Ok(Row {
        name: wide_at(entry, offset_of!(Info, FileName), len)?,
        raw: Raw {
            attributes,
            tag: if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                tag
            } else {
                0
            },
            len: i64_at(entry, offset_of!(Info, EndOfFile))?,
            btime: i64_at(entry, offset_of!(Info, CreationTime))?,
            atime: i64_at(entry, offset_of!(Info, LastAccessTime))?,
            mtime: i64_at(entry, offset_of!(Info, LastWriteTime))?,
        },
        id: u128::from(u64::from_le_bytes(at(entry, offset_of!(Info, FileId))?)),
    })
}

/// The names of a directory, sorted, each stated by the pass that listed it.
/// The pass is read whole, so the lister borrows nothing: the lifetime is the
/// seam's.
pub(super) struct Lister<'a> {
    entries: IntoIter<(EntryName, io::Result<Seen>)>,
    dir: PhantomData<&'a File>,
}

/// List `dir`: a handle reopened from it with `FILE_LIST_DIRECTORY`, since
/// the held one asks attributes alone, which is where a directory the user
/// cannot list refuses.  `FileIdExtdDirectoryInfo` gives names, stats and
/// 128-bit ids in one pass, kept by name; where the volume answers a zero
/// 128-bit id, or refuses the class, the pass is made again as
/// `FileIdBothDirectoryInfo`, whose 64-bit id is the one [`ObjectId::of`]
/// falls back to.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-list] `list-dir`'s enumeration of a held directory, relative to its own handle.  A listing predicate, not turn-time model data I/O: the caller still judges each entry and raises no card."
)]
pub(super) fn list(dir: &File) -> io::Result<Lister<'_>> {
    let reader = reopen(
        dir,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_FLAG_BACKUP_SEMANTICS,
    )?;
    let volume = seen(dir)?.id.volume;
    let extd = rows(
        &reader,
        FileIdExtdDirectoryRestartInfo,
        FileIdExtdDirectoryInfo,
        extd_row,
    );
    let listed = match extd {
        Ok(listed) if listed.iter().all(|row| row.id != 0) => listed,
        Ok(_) => both(&reader)?,
        Err(e) if unsupported_class(&e) => both(&reader)?,
        Err(e) => return Err(e),
    };
    let mut entries: Vec<(EntryName, io::Result<Seen>)> = listed
        .into_iter()
        .filter_map(|row| {
            let name = EntryName::parse(&OsString::from_wide(&row.name)).ok()?;
            let seen = Seen {
                stat: row.raw.stat(),
                id: ObjectId { volume, id: row.id },
            };
            Some((name, Ok(seen)))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Lister {
        entries: entries.into_iter(),
        dir: PhantomData,
    })
}

fn both(reader: &File) -> io::Result<Vec<Row>> {
    rows(
        reader,
        FileIdBothDirectoryRestartInfo,
        FileIdBothDirectoryInfo,
        both_row,
    )
}

impl Iterator for Lister<'_> {
    type Item = (EntryName, io::Result<Seen>);

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next()
    }
}
