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
use crate::path::site::{Silent, Surface};
use crate::site::site;
use cap_fs_ext::OpenOptionsFollowExt;
use cap_primitives::fs::{FollowSymlinks, OpenOptions, OpenOptionsExt};
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::mem::offset_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::ptr::null;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::vec::IntoIter;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile, SYMLINK_FLAG_RELATIVE,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NO_MORE_FILES,
    ERROR_NOT_SUPPORTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS,
    OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_SUCCESS, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO,
    FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_NAME_INFO,
    FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_WRITE_DATA, FileAttributeTagInfo, FileBasicInfo,
    FileDispositionInfo, FileDispositionInfoEx, FileIdBothDirectoryInfo,
    FileIdBothDirectoryRestartInfo, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo,
    FileIdInfo, FileNormalizedNameInfo, FileRenameInfoEx, FileStandardInfo, GetDriveTypeW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    MAXIMUM_REPARSE_DATA_BUFFER_SIZE, ReOpenFile, SYNCHRONIZE, SetFileInformationByHandle,
    VOLUME_NAME_GUID,
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
pub(super) fn is_local(f: &File) -> bool {
    let mut path = vec![0u16; 512];
    let len = loop {
        let capacity = u32::try_from(path.len()).expect("a path buffer is under four gibibytes");
        // SAFETY: `f` is a live handle, and `path` is `capacity` units long.
        let len = unsafe {
            site!(
                Silent::WalkVolume,
                GetFinalPathNameByHandleW(
                    f.as_raw_handle(),
                    path.as_mut_ptr(),
                    capacity,
                    VOLUME_NAME_GUID,
                )
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
pub(super) fn open_root(root: &RootName) -> io::Result<Held> {
    let file = site!(
        Silent::WalkRoot,
        cap_primitives::fs::open_ambient_dir(&root.0, cap_primitives::ambient_authority())
    )?;
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
fn open_entry(dir: &File, name: &OsStr, sharing: Sharing) -> io::Result<File> {
    const LEAF: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let open = |access, share| {
        let mut options = OpenOptions::new();
        options
            .access_mode(access)
            .share_mode(share)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .follow(FollowSymlinks::No);
        let name: &Path = name.as_ref();
        site!(
            Silent::WalkStep,
            cap_primitives::fs::open(dir, name, &options)
        )
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
    let path: &Path = name.as_os_str().as_ref();
    let file = site!(
        Surface::Reopen,
        cap_primitives::fs::open(dir, path, &options)
    )?;
    if ObjectId::of(&file)? != ObjectId::of(leaf)? {
        return Err(moved(name));
    }
    Ok(file)
}

/// The security descriptor `visibility` asks of a file it makes: the
/// owner-only one for `Secret`, none for the others, which inherit the
/// directory's ACL.
fn descriptor_for(visibility: Visibility) -> io::Result<Option<owner_only::OwnerOnly>> {
    match visibility {
        Visibility::Secret => owner_only::OwnerOnly::new().map(Some),
        Visibility::Public | Visibility::Private => Ok(None),
    }
}

/// The entry `name` of `dir` made by `call`, an `NtCreateFile` with
/// `RootDirectory` the held handle, so the entry lands under it whatever its
/// path is, a share or a volume with no drive letter included.  `call` is
/// handed what the function takes: the handle to fill, the attributes naming
/// the entry (with `descriptor`, where there is one, as its security), and the
/// status block; it answers the status.  Each caller makes the call itself, so
/// each is its own site.
///
/// # Errors
/// `AlreadyExists` where the name is taken; the create's.
fn made_under(
    dir: &File,
    name: &EntryName,
    descriptor: Option<&owner_only::OwnerOnly>,
    call: impl FnOnce(*mut HANDLE, *const OBJECT_ATTRIBUTES, *mut IO_STATUS_BLOCK) -> NTSTATUS,
) -> io::Result<File> {
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
        SecurityDescriptor: descriptor.map_or(null(), owner_only::OwnerOnly::as_ptr),
        SecurityQualityOfService: null(),
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let mut block = IO_STATUS_BLOCK::default();
    let status = call(&raw mut handle, &raw const attributes, &raw mut block);
    if status != STATUS_SUCCESS {
        // SAFETY: a plain conversion of a status code.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code.cast_signed()));
    }
    // SAFETY: `handle` is a fresh handle the call filled, and this owns it.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// What a created file asks beyond its `rights`: the synchronize right its
/// synchronous handle needs, and the attributes right a stat of it needs, as
/// `CreateFileW` adds both.
const MADE: u32 = SYNCHRONIZE | FILE_READ_ATTRIBUTES;

/// Every sharing a file made here allows, as `std` opens one.
const SHARED: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// The entry `name` made exclusively for `access`: what a leaf absent at the
/// walk becomes.  `FILE_CREATE` fails on a taken name, a link included, which
/// `FILE_OPEN_REPARSE_POINT` keeps from being followed.  A `Secret` file is
/// made with the owner-only descriptor, so it never exists under the
/// directory's inherited ACL.
pub(super) fn create(
    dir: &File,
    name: &EntryName,
    access: Access,
    visibility: Visibility,
) -> io::Result<File> {
    let descriptor = descriptor_for(visibility)?;
    made_under(
        dir,
        name,
        descriptor.as_ref(),
        |handle, attributes, block| {
            // SAFETY: `made_under` hands the call a handle and a status block to
            // fill and attributes that name the entry under `dir`, all of which
            // outlive it.
            unsafe {
                site!(
                    Surface::Create,
                    NtCreateFile(
                        handle,
                        rights(access) | MADE,
                        attributes,
                        block,
                        null(),
                        FILE_ATTRIBUTE_NORMAL,
                        SHARED,
                        FILE_CREATE,
                        FILE_NON_DIRECTORY_FILE
                            | FILE_SYNCHRONOUS_IO_NONALERT
                            | FILE_OPEN_REPARSE_POINT,
                        null(),
                        0,
                    )
                )
            }
        },
    )
}

/// A fresh sibling, read and write, that no one else has, with `DELETE`
/// access, which renaming it by handle needs; made as [`create`] makes a file.
pub(super) fn create_sibling(
    dir: &File,
    name: &EntryName,
    visibility: Visibility,
) -> io::Result<File> {
    let descriptor = descriptor_for(visibility)?;
    made_under(
        dir,
        name,
        descriptor.as_ref(),
        |handle, attributes, block| {
            // SAFETY: as in `create`.
            unsafe {
                site!(
                    Surface::Stage,
                    NtCreateFile(
                        handle,
                        GENERIC_READ | GENERIC_WRITE | DELETE | MADE,
                        attributes,
                        block,
                        null(),
                        FILE_ATTRIBUTE_NORMAL,
                        SHARED,
                        FILE_CREATE,
                        FILE_NON_DIRECTORY_FILE
                            | FILE_SYNCHRONOUS_IO_NONALERT
                            | FILE_OPEN_REPARSE_POINT,
                        null(),
                        0,
                    )
                )
            }
        },
    )
}

/// Make the directory `name` in `dir` ([`made_under`]).  The handle it opens
/// is closed at once; the caller steps onto the name.  The directory inherits
/// the held one's ACL, whatever the visibility.
///
/// # Errors
/// `AlreadyExists` where the name is taken; the create's.
pub(super) fn mkdir(dir: &File, name: &EntryName, _visibility: Visibility) -> io::Result<()> {
    made_under(dir, name, None, |handle, attributes, block| {
        // SAFETY: as in `create`.
        unsafe {
            site!(
                Silent::WalkMkdir,
                NtCreateFile(
                    handle,
                    FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                    attributes,
                    block,
                    null(),
                    FILE_ATTRIBUTE_NORMAL,
                    SHARED,
                    FILE_CREATE,
                    FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
                    null(),
                    0,
                )
            )
        }
    })
    .map(drop)
}

/// `error`, with what `class` needs said where the volume or the OS refuses it
/// as an invalid parameter, which is how a class they lack answers.
fn class_refused(error: io::Error, what: &str, class: &str) -> io::Error {
    if error.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() {
        io::Error::new(
            error.kind(),
            format!(
                "{what} needs {class}, which Windows 10 1809 and NTFS or ReFS answer ({error})"
            ),
        )
    } else {
        error
    }
}

/// Rename the object `handle` holds to the entry `to` of `dir`:
/// `FileRenameInfoEx` with `RootDirectory` the held directory, so the rename
/// lands in it whatever its name now is, replacing the target and leaving a
/// handle on it open.  `handle` needs `DELETE` access.  A volume or host that
/// refuses the class fails with that error, said as `what` needs it; nothing
/// falls back to a rename by name.
fn rename_by_handle(handle: &File, dir: &File, to: &EntryName, what: &str) -> io::Result<()> {
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
    // SAFETY: `handle` is a live handle, and `info` is the struct the class
    // names, `size` bytes of it.
    let ok = unsafe {
        SetFileInformationByHandle(handle.as_raw_handle(), FileRenameInfoEx, info.cast(), size)
    };
    if ok == 0 {
        return Err(class_refused(
            io::Error::last_os_error(),
            what,
            "FileRenameInfoEx",
        ));
    }
    Ok(())
}

/// Rename `sibling`, the staged object, over the entry `to` of `dir`, by its
/// handle ([`rename_by_handle`]).  The sibling's name is not asked.
pub(super) fn rename_over(
    dir: &File,
    sibling: &File,
    _from: &EntryName,
    to: &EntryName,
) -> io::Result<()> {
    rename_by_handle(sibling, dir, to, "the atomic write")
}

/// The handle the walk holds on the entry, which Windows acts through.  A
/// Windows leaf that is present is always held.
fn held(leaf: Option<&File>) -> io::Result<&File> {
    leaf.ok_or_else(|| io::Error::other("the walk holds no handle on the entry"))
}

/// Rename the entry the walk holds by `leaf` to `to` in `dir`, over whatever
/// `to` is: `leaf` reopened with `DELETE`, which a held leaf shares, then
/// renamed as a staged sibling is ([`rename_by_handle`]).  `from` is not
/// asked: no name is resolved.
pub(super) fn rename(
    dir: &File,
    leaf: Option<&File>,
    _from: &EntryName,
    to: &EntryName,
) -> io::Result<()> {
    let renamed = reopen_shared(held(leaf)?, DELETE)?;
    rename_by_handle(&renamed, dir, to, "the rename")
}

/// Remove the entry the walk holds by `leaf`, by its handle: reopened with
/// `DELETE`, which a held leaf shares, then marked for deletion with POSIX
/// semantics, so its name goes at once, whoever else still has it open, and a
/// read-only entry goes too.  A directory goes only once it is empty.  `dir`,
/// `name` and `kind` are not asked: no name is resolved.
pub(super) fn remove(
    _dir: &File,
    leaf: Option<&File>,
    _name: &EntryName,
    _kind: Kind,
) -> io::Result<()> {
    let doomed = reopen_shared(held(leaf)?, DELETE)?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: `doomed` is a live handle, and `info` is the struct the class
    // names, of the size the call is told.
    let ok = unsafe {
        SetFileInformationByHandle(
            doomed.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).expect("an info struct is small"),
        )
    };
    if ok == 0 {
        return Err(class_refused(
            io::Error::last_os_error(),
            "the removal",
            "FileDispositionInfoEx",
        ));
    }
    Ok(())
}

/// Mark the staged `sibling` for deletion by its own handle: it was created
/// with `DELETE` access, and its name goes when the handle closes.  No name
/// is resolved.
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
/// link: `Err` is the open's refusal.
pub(super) fn may_write(dir: &File, name: &OsStr) -> io::Result<()> {
    let name: &Path = name.as_ref();
    site!(
        Silent::WalkAccess,
        cap_primitives::fs::access(
            dir,
            name,
            cap_primitives::fs::AccessType::Access(cap_primitives::fs::AccessModes {
                readable: false,
                writable: true,
                executable: false,
            }),
            FollowSymlinks::Yes,
        )
    )
}

/// The discard device, by its fixed name.
pub(super) fn open_discard() -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    site!(Silent::DiscardDevice, &options => open(r"\\.\NUL"))
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

/// The security descriptor of a file only its owner may reach: one entry
/// granting the process's owner full control and nobody else anything, the
/// DACL protected so the parent directory's inheritable entries are not merged
/// in.  It rides in on the create's object attributes, so the file never
/// exists, not even for an instant, under an inherited ACL.
pub(super) mod owner_only {
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, FALSE, HANDLE, LocalFree, TRUE};
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SetEntriesInAclW, TRUSTEE_IS_SID,
        TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACL, InitializeSecurityDescriptor, NO_INHERITANCE, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        SECURITY_DESCRIPTOR, SetSecurityDescriptorControl, SetSecurityDescriptorDacl, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The only revision Win32 defines (`winnt.h`), restated rather than
    /// pulling in another `windows-sys` feature for one ABI constant.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    /// The current process's owner SID, in the layout `GetTokenInformation`
    /// hands back: a `TOKEN_USER` followed inline by its SID bytes.
    ///
    /// The buffer counts `u64`s though the API counts bytes, because the
    /// caller reads a pointer-leading `TOKEN_USER` back out of it. `Vec<u8>`
    /// is byte-aligned *as a type*; counting in `u64` makes that read sound
    /// by construction rather than by allocator luck.
    pub(in crate::path::walk) fn current_process_owner() -> io::Result<Vec<u64>> {
        use windows_sys::Win32::Security::GetTokenInformation;

        let mut raw_token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle needing no
        // close; `raw_token` is an out-param this call fills on success.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw_token) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `OpenProcessToken` succeeded, so `raw_token` is an open
        // handle that nothing else owns; the guard closes it exactly once, so
        // the early returns below need no `CloseHandle` of their own.
        let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };

        let mut needed: u32 = 0;
        // SAFETY: a null buffer of zero length is the documented sizing
        // query; `needed` is filled despite the expected failure.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &raw mut needed,
            );
        }
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        // SAFETY: `buf` holds at least the `needed` bytes the sizing call
        // reported, rounded up to whole `u64`s.
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf)
    }

    /// A single-ACE ACL granting the process owner full control and nobody
    /// else anything. The caller owns the allocation and frees it once the
    /// create has copied the descriptor into the new file object.
    fn owner_only_acl() -> io::Result<*mut ACL> {
        let owner_buf = current_process_owner()?;
        // SAFETY: `owner_buf` holds a populated `TOKEN_USER` in a `u64`
        // buffer, so aligned for it, and outlives every use of the `PSID`.
        let owner_sid = unsafe { (*owner_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // The trustee-by-SID form reinterprets this field as the PSID;
            // both are raw `*mut _` here, so the cast preserves bits.
            // `trustee_for` in ral-core's `authority::sandbox::windows::dacl` does the same.
            ptstrName: owner_sid.cast::<u16>(),
        };
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: trustee,
        };

        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `ea` outlives the call; a null prior ACL builds a fresh one
        // holding only this entry, merging in nothing inherited.
        let rc = unsafe { SetEntriesInAclW(1, &raw const ea, std::ptr::null(), &raw mut acl) };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc.cast_signed()));
        }
        Ok(acl)
    }

    /// The descriptor and the ACL it points at, which this owns and frees.
    pub(in crate::path::walk) struct OwnerOnly {
        descriptor: SECURITY_DESCRIPTOR,
        acl: *mut ACL,
    }

    impl OwnerOnly {
        /// The owner-only descriptor, with `SE_DACL_PROTECTED` in force.
        ///
        /// # Errors
        /// The OS's, as a token, an ACL or a descriptor refuses.
        pub(in crate::path::walk) fn new() -> io::Result<Self> {
            let mut owner = Self {
                descriptor: SECURITY_DESCRIPTOR::default(),
                acl: owner_only_acl()?,
            };
            let sd: PSECURITY_DESCRIPTOR = (&raw mut owner.descriptor).cast();
            // SAFETY: `sd` is the descriptor `owner` holds, which
            // `InitializeSecurityDescriptor` fills in place.
            if unsafe { InitializeSecurityDescriptor(sd, SECURITY_DESCRIPTOR_REVISION) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `sd` was just initialised; `owner.acl` is freed only
            // when `owner` drops, after any create has copied the descriptor.
            if unsafe { SetSecurityDescriptorDacl(sd, TRUE, owner.acl, FALSE) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // Protecting the DACL is what stops a create merging in the
            // parent directory's inheritable ACEs.
            // SAFETY: `sd` carries the DACL just attached above.
            if unsafe { SetSecurityDescriptorControl(sd, SE_DACL_PROTECTED, SE_DACL_PROTECTED) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(owner)
        }

        /// The descriptor, as the `SecurityDescriptor` of an object's
        /// attributes; good as long as `self` is.
        pub(in crate::path::walk) fn as_ptr(&self) -> *const SECURITY_DESCRIPTOR {
            &raw const self.descriptor
        }
    }

    impl Drop for OwnerOnly {
        fn drop(&mut self) {
            // SAFETY: `acl` came from `SetEntriesInAclW`, which allocates
            // through `LocalAlloc`, and is freed here and nowhere else.  A
            // create has copied the descriptor by the time this runs, so
            // nothing reads it through the pointer.
            unsafe {
                LocalFree(self.acl.cast());
            }
        }
    }
}
