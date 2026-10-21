//! The macOS host: a directory is entered by search authority alone, a leaf
//! is held by an event-only descriptor that names it without a read
//! permission, and the name a step records is the one its directory stores,
//! which XNU's by-name lookup does not by itself prove.

use super::unix::{self, oflags};
use super::{Access, Child, EntryName, Held, Kind, ObjectId, RootName, Seen, Sharing, Stat, Step};
use crate::path::identity::collision_key;
use crate::path::shown;
use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::time::SystemTime;

pub(super) use super::unix::{
    DirInfo, Lister, copy_mode, create, create_sibling, is_writable, list, mkdir, open_discard,
    open_named, rename_over, sync_dir, unlink,
};

/// The identity a by-name `stat` reports, for a caller comparing it with
/// [`ObjectId::of`] of a handle.
impl From<&rustix::fs::Stat> for ObjectId {
    fn from(stat: &rustix::fs::Stat) -> Self {
        Self {
            dev: u64::from(stat.st_dev.cast_unsigned()),
            ino: stat.st_ino,
        }
    }
}

/// Search authority on a directory, as the kernel's own resolver asks it.
const O_SEARCH: OFlags = OFlags::from_bits_retain(libc::O_SEARCH.cast_unsigned());

/// An event-only descriptor: it needs no more than the right to name the
/// object.
const O_EVTONLY: OFlags = OFlags::from_bits_retain(libc::O_EVTONLY.cast_unsigned());

/// `fsobj_type_t`'s `VREG`, `VDIR` and `VLNK`.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

#[cfg(test)]
thread_local! {
    /// How many data handles [`derive`] has opened on this thread, which a
    /// test reads to show that nothing was opened.
    pub(super) static DERIVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A walk's refusal of an entry that is no longer the one it opened.
fn moved(name: &EntryName) -> io::Error {
    io::Error::other(format!(
        "{} moved during the walk: it is no longer the entry its directory held when the walk opened it",
        shown(name.as_os_str())
    ))
}

/// `error` of a lookup of an entry the walk just found: absence now is a move,
/// never a create target.
fn gone(name: &EntryName, error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound {
        moved(name)
    } else {
        error
    }
}

/// `/`, held as a search handle.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-root] Opens `/` as a search handle, the root every walk descends from: path resolution, not the model's data I/O."
)]
pub(super) fn open_root(_root: &RootName) -> io::Result<Held> {
    let file = File::from(rustix::fs::open(
        "/",
        O_SEARCH | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    Ok(Held {
        file,
        info: DirInfo,
    })
}

/// The entry `name` of `dir` as a step holds it: `None` where the OS refused
/// the hold for permission, so the object is stated and never held.
///
/// A directory step asks search authority alone, so a `--x` ancestor is
/// walked; `EACCES` is an unenterable directory and `ENOENT` an absent one.
/// Any other failure — XNU answers `O_DIRECTORY` on a link `ENOTDIR` before
/// it would say `ELOOP` — is not a classification, so the entry is opened as
/// a leaf and told by its own `fstat`.  `O_SYMLINK` opens a link itself, and
/// is not combined with `O_NOFOLLOW`, which XNU refuses on a link whatever
/// else is set.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-step] Opens one component relative to a held directory and never follows it, to reach the object a grant will judge: path resolution, not the model's data I/O; the card belongs to the site that then opens the leaf."
)]
fn open_entry(dir: &File, name: &EntryName, sharing: Sharing) -> io::Result<Option<File>> {
    let open = |flags: OFlags| {
        rustix::fs::openat(
            dir,
            name.as_os_str(),
            flags | OFlags::CLOEXEC,
            Mode::empty(),
        )
    };
    if sharing == Sharing::Directory {
        match open(O_SEARCH | OFlags::NOFOLLOW) {
            Ok(fd) => return Ok(Some(File::from(fd))),
            Err(e @ (Errno::ACCESS | Errno::NOENT)) => return Err(e.into()),
            Err(_) => {}
        }
    }
    match open(O_EVTONLY | OFlags::SYMLINK | OFlags::NOCTTY | OFlags::NONBLOCK) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(Errno::ACCESS | Errno::PERM) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The entry `name` of `parent`, opened and then named as `parent` stores it.
pub(super) fn step(parent: &Held, name: &EntryName, sharing: Sharing) -> io::Result<Child> {
    let dir = &parent.file;
    let handle = open_entry(dir, name, sharing)?;
    let (seen, stored) = match &handle {
        Some(file) => {
            let seen = seen(file)?;
            let stored = stored_name(dir, name, seen.id)?;
            (seen, stored)
        }
        None => {
            let looked = lookup(dir, name.as_os_str()).map_err(|e| gone(name, e))?;
            let stored = settle(dir, name, looked.seen.id, &looked.name)?;
            (looked.seen, stored)
        }
    };
    let step = Step {
        name: stored,
        id: seen.id,
        kind: seen.stat.kind,
    };
    Ok(Child {
        handle,
        step,
        stat: seen.stat,
        info: DirInfo,
    })
}

/// The name `dir` stores for the entry `name` whose object, held by the walk,
/// has identity `id`.  A query by name is not a proof: XNU fills the name
/// from the vnode, so of two hard links `a` and `b` in one directory a query
/// for `a` can answer `b`.  The query's identity must be the handle's, else
/// the entry moved; its answer is then judged by [`settle`].
pub(super) fn stored_name(dir: &File, name: &EntryName, id: ObjectId) -> io::Result<EntryName> {
    let looked = lookup(dir, name.as_os_str()).map_err(|e| gone(name, e))?;
    if looked.seen.id != id {
        return Err(moved(name));
    }
    settle(dir, name, id, &looked.name)
}

/// What the lookup of `name` answered, `answered`, as the name of the entry
/// of `dir` whose identity is `id`.  A spelling of `name` under every fold a
/// filesystem applies is the entry's own, since a directory holds no two
/// entries with one key.  Otherwise it is another hard link's name, and the
/// directory is listed for the entry with `id` and `name`'s key, which is by
/// construction an entry of `dir`; none refuses.  A directory that cannot be
/// read for the listing refuses too: the name is not guessed.
pub(super) fn settle(
    dir: &File,
    name: &EntryName,
    id: ObjectId,
    answered: &OsStr,
) -> io::Result<EntryName> {
    let key = collision_key(name.as_os_str());
    if *collision_key(answered) == *key {
        return Ok(EntryName::parse(answered)?);
    }
    stored_entry(dir, id, &key)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "cannot learn the stored name of {}, a hard link whose directory will not list: {e}",
                    shown(name.as_os_str())
                ),
            )
        })?
        .ok_or_else(|| moved(name))
}

/// A `getattrlist` answer, read in the order the kernel packs it: its length,
/// the set of attributes it returned, then the attributes themselves.
struct Reply<'a> {
    buf: &'a [u8],
    at: usize,
    common: u32,
    file: u32,
}

fn short() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the kernel's attribute answer is shorter than it says",
    )
}

impl<'a> Reply<'a> {
    fn new(buf: &'a [u8]) -> io::Result<Self> {
        let mut reply = Self {
            buf,
            at: 0,
            common: 0,
            file: 0,
        };
        let length = usize::try_from(reply.u32()?).map_err(|_| short())?;
        reply.buf = buf.get(..length).ok_or_else(short)?;
        reply.common = reply.u32()?;
        reply.u32()?;
        reply.u32()?;
        reply.file = reply.u32()?;
        reply.u32()?;
        Ok(reply)
    }

    fn bytes<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let raw = self.buf.get(self.at..self.at + N).ok_or_else(short)?;
        self.at += N;
        raw.try_into().map_err(|_| short())
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.bytes().map(u32::from_ne_bytes)
    }

    fn u64(&mut self) -> io::Result<u64> {
        self.bytes().map(u64::from_ne_bytes)
    }

    fn time(&mut self) -> io::Result<Option<SystemTime>> {
        let sec = i64::from_ne_bytes(self.bytes()?);
        let nsec = i64::from_ne_bytes(self.bytes()?);
        Ok(stamp(sec, nsec))
    }

    /// An `attrreference_t`, whose offset is from itself, to a NUL-ended name.
    fn name(&mut self) -> io::Result<OsString> {
        let base = self.at;
        let offset = isize::try_from(i32::from_ne_bytes(self.bytes()?)).map_err(|_| short())?;
        let len = usize::try_from(u32::from_ne_bytes(self.bytes()?)).map_err(|_| short())?;
        let start = base.checked_add_signed(offset).ok_or_else(short)?;
        let raw = self.buf.get(start..start + len).ok_or_else(short)?;
        Ok(OsString::from_vec(
            raw.strip_suffix(&[0]).unwrap_or(raw).to_vec(),
        ))
    }
}

fn stamp(sec: i64, nsec: i64) -> Option<SystemTime> {
    unix::time(sec, u32::try_from(nsec).ok()?)
}

/// What one lookup of an entry by name answered: what it is, and the name the
/// vnode gave.
struct Looked {
    seen: Seen,
    name: OsString,
}

const fn wanted(common: u32, file: u32) -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: common,
        volattr: 0,
        dirattr: 0,
        fileattr: file,
        forkattr: 0,
    }
}

/// The entry `name` of `dir`, stated by one lookup that follows nothing.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-stat] Looks up one entry of a held directory by name without following it: a metadata read, never turn-time data I/O.  It needs no permission on the object, which is how an object that cannot be held is still stated."
)]
fn lookup(dir: &File, name: &OsStr) -> io::Result<Looked> {
    const COMMON: u32 = libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_CRTIME
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_ACCTIME
        | libc::ATTR_CMN_ACCESSMASK
        | libc::ATTR_CMN_FILEID;
    let mut list = wanted(
        libc::ATTR_CMN_RETURNED_ATTRS | COMMON,
        libc::ATTR_FILE_DATALENGTH,
    );
    let c_name =
        CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut buf = [0u8; 2048];
    // SAFETY: `dir` is a live descriptor, `c_name` a NUL-terminated string,
    // and `list` and `buf` outlive the call, which writes at most the
    // `buf.len()` bytes it is told of.
    let rc = unsafe {
        libc::getattrlistat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            (&raw mut list).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::c_ulong::from(libc::FSOPT_NOFOLLOW),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut reply = Reply::new(&buf)?;
    if reply.common & COMMON != COMMON {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this volume does not answer what a lookup by name asks",
        ));
    }
    let name = reply.name()?;
    let dev = reply.u32()?;
    let kind = match reply.u32()? {
        VREG => Kind::File,
        VDIR => Kind::Dir,
        VLNK => Kind::Symlink,
        _ => Kind::Other,
    };
    let btime = reply.time()?;
    let mtime = reply.time()?;
    let atime = reply.time()?;
    let mode = reply.u32()? & 0o7777;
    let ino = reply.u64()?;
    let len = if reply.file & libc::ATTR_FILE_DATALENGTH != 0 {
        reply.u64()?
    } else {
        0
    };
    Ok(Looked {
        seen: Seen {
            id: ObjectId {
                dev: u64::from(dev),
                ino,
            },
            stat: Stat {
                kind,
                len,
                readonly: mode & 0o222 == 0,
                mtime,
                atime,
                btime,
                mode,
            },
        },
        name,
    })
}

/// The entry of `dir` whose identity is `id` and whose name has collision key
/// `key`, found by listing `dir` through a read handle reopened from it.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-stored-name] Lists a held directory for the one entry whose identity the walk holds, to learn the name the directory stores for it: path resolution, not turn-time model data I/O."
)]
fn stored_entry(dir: &File, id: ObjectId, key: &OsStr) -> io::Result<Option<EntryName>> {
    let reader = unix::reopen(dir)?;
    let mut list = wanted(
        libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_FILEID,
        0,
    );
    let mut buf = vec![0u8; 1 << 16];
    loop {
        // SAFETY: `reader` is a live descriptor, and `list` and `buf` outlive
        // the call, which writes at most the `buf.len()` bytes it is told of.
        let count = unsafe {
            libc::getattrlistbulk(
                reader.as_raw_fd(),
                (&raw mut list).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
            )
        };
        match count {
            ..0 => return Err(io::Error::last_os_error()),
            0 => return Ok(None),
            _ => {}
        }
        let mut rest = &buf[..];
        for _ in 0..count {
            let mut reply = Reply::new(rest)?;
            rest = &rest[reply.buf.len()..];
            let name = reply.name()?;
            let dev = reply.u32()?;
            let ino = reply.u64()?;
            let entry = ObjectId {
                dev: u64::from(dev),
                ino,
            };
            if entry == id && *collision_key(&name) == *key {
                return Ok(Some(EntryName::parse(&name)?));
            }
        }
    }
}

/// What `f` is, by `fstat` of the descriptor itself.
pub(super) fn seen(f: &File) -> io::Result<Seen> {
    let st = rustix::fs::fstat(f)?;
    let kind = match FileType::from_raw_mode(st.st_mode) {
        FileType::Symlink => Kind::Symlink,
        FileType::Directory => Kind::Dir,
        FileType::RegularFile => Kind::File,
        _ => Kind::Other,
    };
    let mode = u32::from(st.st_mode) & 0o7777;
    Ok(Seen {
        id: ObjectId::from(&st),
        stat: Stat {
            kind,
            len: u64::try_from(st.st_size).unwrap_or_default(),
            readonly: mode & 0o222 == 0,
            mtime: stamp(st.st_mtime, st.st_mtime_nsec),
            atime: stamp(st.st_atime, st.st_atime_nsec),
            btime: stamp(st.st_birthtime, st.st_birthtime_nsec),
            mode,
        },
    })
}

/// What the entry `name` of `dir` is, not followed.
pub(super) fn seen_at(dir: &File, name: &OsStr) -> io::Result<Seen> {
    lookup(dir, name).map(|looked| looked.seen)
}

/// A link's target, read through the descriptor that opened it.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-link-read] Reads a link's target through the handle that opened it, to splice into the remaining name: path resolution, not data I/O."
)]
pub(super) fn read_link(link: &File) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; usize::try_from(libc::PATH_MAX).unwrap_or(1024)];
    // SAFETY: `link` is a live descriptor and `buf` is writable for the
    // `buf.len()` bytes the call is told of.
    let read = unsafe { libc::freadlink(link.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    buf.truncate(usize::try_from(read).map_err(|_| io::Error::last_os_error())?);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

/// A data handle on the object `leaf` holds, opened by `name` under `dir`
/// and verified against the held handle's identity: macOS cannot derive one
/// from a descriptor.  A regular file is opened `O_NONBLOCK`, so a swap for a
/// FIFO cannot block the open before it is checked, and the flag is cleared
/// once it has been.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-reopen] The one open of a held leaf, verified against its handle so a name that now leads elsewhere is refused.  Every caller surfaces it, each in its own way: a redirect's card is fused on by the frame that wrapped the locate, exarch's readers speak their own card, and a write's before- and after-images ride the card of the operation that asked for them."
)]
pub(super) fn derive(
    leaf: &File,
    dir: &File,
    name: &EntryName,
    stat: &Stat,
    access: Access,
) -> io::Result<File> {
    #[cfg(test)]
    DERIVED.with(|opened| opened.set(opened.get() + 1));
    let regular = stat.kind == Kind::File;
    let mut flags = oflags(access) | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::CLOEXEC;
    if regular {
        flags |= OFlags::NONBLOCK;
    }
    let file = File::from(rustix::fs::openat(
        dir,
        name.as_os_str(),
        flags,
        Mode::empty(),
    )?);
    if ObjectId::of(&file)? != ObjectId::of(leaf)? {
        return Err(moved(name));
    }
    if regular {
        rustix::fs::fcntl_setfl(&file, rustix::fs::fcntl_getfl(&file)? - OFlags::NONBLOCK)?;
    }
    Ok(file)
}
