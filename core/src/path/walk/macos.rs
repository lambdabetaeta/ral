//! The macOS host: a directory is entered by search authority alone, a leaf
//! is held by an event-only descriptor that names it without a read
//! permission, a special file is stated and never held, and the name a step
//! records is the one its directory stores, which XNU's by-name lookup does
//! not by itself prove.

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
use std::sync::Arc;
use std::time::SystemTime;

pub(super) use super::unix::{
    DirInfo, Lister, copy_mode, create, create_sibling, discard, is_writable, list, mkdir,
    open_discard, rename_over, sync_dir,
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
        file: Arc::new(file),
        info: DirInfo,
    })
}

/// The entry `name` of `parent`, looked up first so a special file is known
/// before anything opens it, then opened where it can be held without
/// touching it — a directory by search authority, a file or a link by an
/// event-only descriptor — and stated otherwise: a special file, or a leaf
/// the OS refused the hold for permission.  A directory step asks search
/// authority alone, so a `--x` ancestor is walked; `EACCES` is an unenterable
/// directory.  `O_SYMLINK` opens a link itself, and is not combined with
/// `O_NOFOLLOW`, which XNU refuses on a link whatever else is set.  The
/// handle's identity must be the lookup's, else the entry moved.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-step] Opens one component relative to a held directory and never follows it, to reach the object a grant will judge: path resolution, not the model's data I/O; the card belongs to the site that then opens the leaf."
)]
pub(super) fn step(parent: &Held, name: &EntryName, sharing: Sharing) -> io::Result<Child> {
    let dir = &*parent.file;
    let looked = lookup(dir, name.as_os_str())?;
    #[cfg(test)]
    super::tests::after_lookup();
    let open = |flags: OFlags| {
        rustix::fs::openat(
            dir,
            name.as_os_str(),
            flags | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
    };
    let handle = match looked.seen.stat.kind {
        Kind::Other => None,
        Kind::Dir if sharing == Sharing::Directory => {
            Some(open(O_SEARCH | OFlags::DIRECTORY | OFlags::NOFOLLOW)?)
        }
        _ => match open(O_EVTONLY | OFlags::SYMLINK | OFlags::NOCTTY | OFlags::NONBLOCK) {
            Ok(file) => Some(file),
            Err(Errno::ACCESS | Errno::PERM) => None,
            Err(e) => return Err(e.into()),
        },
    };
    let seen = match &handle {
        Some(file) => {
            let seen = seen(file)?;
            if seen.id != looked.seen.id {
                return Err(moved(name));
            }
            seen
        }
        None => looked.seen,
    };
    let stored = settle(dir, name, handle.as_ref().map(|_| seen.id), &looked.name)?;
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

/// What the lookup of `name` answered, `answered`, as the name of the entry
/// of `dir` the walk reached, whose identity is `held` where the walk holds
/// it.  An answer spelled as the name asked is the entry's own: a volume that
/// folds cannot hold two entries in one fold class, and one that does not
/// resolved exactly that spelling.  Any other answer is settled by the held
/// identity ([`stored_entry`]); an object the walk only states has none that
/// anything pins, so its name is not chosen by it.
pub(super) fn settle(
    dir: &File,
    name: &EntryName,
    held: Option<ObjectId>,
    answered: &OsStr,
) -> io::Result<EntryName> {
    if answered == name.as_os_str() {
        return Ok(name.clone());
    }
    let Some(id) = held else {
        return Err(io::Error::other(format!(
            "cannot learn the stored name of {}, which the walk states but does not hold: the lookup answered {}, and only a held object's identity can tell which entry is its own",
            shown(name.as_os_str()),
            shown(answered)
        )));
    };
    stored_entry(dir, id, name)
}

/// The name `dir` stores for the entry `name` reached, of identity `id`: the
/// entries whose name folds as `name` does, each looked up as the step looked
/// `name` up — across a mount point or a firmlink, as the step crossed it —
/// and kept where the lookup reaches `id`, then decided by [`pick`].  A
/// directory that cannot be listed refuses: the name is not guessed.
fn stored_entry(dir: &File, id: ObjectId, name: &EntryName) -> io::Result<EntryName> {
    let cannot = |e: io::Error| {
        io::Error::new(
            e.kind(),
            format!(
                "cannot learn the stored name of {}: {e}",
                shown(name.as_os_str())
            ),
        )
    };
    let key = collision_key(name.as_os_str());
    let mut reaching = Vec::new();
    for entry in unix::names(dir).map_err(cannot)? {
        if *collision_key(entry.as_os_str()) != *key {
            continue;
        }
        match lookup(dir, entry.as_os_str()) {
            Ok(looked) if looked.seen.id == id => reaching.push(entry),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(cannot(e)),
        }
    }
    pick(&reaching, name)
}

/// The entry of `entries`, those that reach one object, that `name` asked
/// for: `name` itself where one is spelled so; else the unique one under
/// `name`'s fold, which is coarser than every volume's, so equality under it
/// proves nothing of which entry a name resolved to.  None is a move; several
/// refuse.
pub(super) fn pick(entries: &[EntryName], name: &EntryName) -> io::Result<EntryName> {
    if entries.contains(name) {
        return Ok(name.clone());
    }
    let key = collision_key(name.as_os_str());
    let folded: Vec<_> = entries
        .iter()
        .filter(|entry| *collision_key(entry.as_os_str()) == *key)
        .collect();
    match folded.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(moved(name)),
        several => Err(io::Error::other(format!(
            "{} is one of {} spellings of one object in this directory, and the walk cannot tell which the directory stores",
            shown(name.as_os_str()),
            several.len()
        ))),
    }
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
pub(super) struct Looked {
    pub(super) seen: Seen,
    pub(super) name: OsString,
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
    let mut list = wanted(
        libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_CRTIME
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_ACCTIME
            | libc::ATTR_CMN_ACCESSMASK
            | libc::ATTR_CMN_FILEID,
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
    parse_lookup(&buf)
}

/// A lookup's answer, read by the set of attributes it returned: an attribute
/// a volume lacks is absent from the set and the buffer.  The name, identity,
/// kind and mode are required, the timestamps are not.
pub(super) fn parse_lookup(buf: &[u8]) -> io::Result<Looked> {
    const REQUIRED: u32 = libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_ACCESSMASK
        | libc::ATTR_CMN_FILEID;
    let mut reply = Reply::new(buf)?;
    let common = reply.common;
    if common & REQUIRED != REQUIRED {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this volume does not answer the name, identity, kind and mode of an entry, which a walk needs",
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
    let mut time = |bit| {
        if common & bit != 0 {
            reply.time()
        } else {
            Ok(None)
        }
    };
    let btime = time(libc::ATTR_CMN_CRTIME)?;
    let mtime = time(libc::ATTR_CMN_MODTIME)?;
    let atime = time(libc::ATTR_CMN_ACCTIME)?;
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

/// The entry `name` of `dir`, opened by name with `flags` and never
/// followed, and refused unless its identity is `id`, the identity the walk
/// recorded of the object at the name: macOS cannot derive a data handle from
/// a descriptor, and the check runs before any byte moves through the one it
/// opens.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-reopen] The one open of a leaf the walk reached, by name under its held directory and verified against the identity the walk recorded, so a name that now leads elsewhere is refused.  Every caller surfaces it, each in its own way: a redirect's card is fused on by the frame that wrapped the locate, exarch's readers speak their own card, and a write's before- and after-images ride the card of the operation that asked for them."
)]
fn reopen(dir: &File, name: &EntryName, id: ObjectId, flags: OFlags) -> io::Result<File> {
    let file = File::from(rustix::fs::openat(
        dir,
        name.as_os_str(),
        flags | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if ObjectId::of(&file)? != id {
        return Err(moved(name));
    }
    Ok(file)
}

/// A data handle on the object `leaf` holds, opened by `name` under `dir`
/// and verified against the held handle's identity ([`reopen`]).  A regular
/// file is opened `O_NONBLOCK`, so a swap for a FIFO cannot block the open
/// before it is checked, and the flag is cleared once it has been.
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
    let mut flags = oflags(access);
    if regular {
        flags |= OFlags::NONBLOCK;
    }
    let file = reopen(dir, name, ObjectId::of(leaf)?, flags)?;
    if regular {
        rustix::fs::fcntl_setfl(&file, rustix::fs::fcntl_getfl(&file)? - OFlags::NONBLOCK)?;
    }
    Ok(file)
}

/// A data handle on the special file `name` of `dir`, which the walk stated
/// as `id`, opened by name and verified to be that object ([`reopen`]) and
/// still a special file: macOS cannot hold a FIFO, a socket or a device
/// without opening it, so the walk states it and a stream is opened here.
/// Unheld, its identity can be reused, and on a volume that reuses one a
/// regular file could come to bear it; the kind refuses that file.  Not
/// `O_NONBLOCK`: a FIFO's open is its rendezvous, blocking as a shell's does.
pub(super) fn open_special(
    dir: &File,
    name: &EntryName,
    id: ObjectId,
    access: Access,
) -> io::Result<File> {
    let file = reopen(dir, name, id, oflags(access))?;
    if seen(&file)?.stat.kind != Kind::Other {
        return Err(moved(name));
    }
    Ok(file)
}
