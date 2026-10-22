//! The Linux host: a step is an `O_PATH` open, and everything about it is read
//! from the descriptor it returns.

use super::unix::oflags;
use super::{Access, Child, EntryName, Held, ObjectId, RootName, Seen, Sharing, Stat, Step};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawMode, Statx, StatxFlags, StatxTimestamp};
use rustix::io::Errno;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
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
        use rustix::fs::{major, makedev, minor};
        Self {
            dev: makedev(major(stat.st_dev), minor(stat.st_dev)),
            ino: stat.st_ino,
        }
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
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    Ok(Held {
        file: Arc::new(file),
        info: DirInfo,
    })
}

/// The entry `name` of `parent` as an `O_PATH` descriptor, never followed.
///
/// A directory is asked for first: `O_DIRECTORY` sets `LOOKUP_DIRECTORY`,
/// which triggers an automount, whereas a plain `O_PATH` open of an
/// untriggered autofs mount point answers the empty directory underneath it.
/// `ENOTDIR` is a file or a link, opened again without it.  The name stored
/// is the name asked: `openat` made it an entry of `parent`, and Linux cannot be
/// asked for more of a casefolded one.  Sharing is a Windows matter.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-step] Opens one component relative to a held directory and never follows it, to reach the object a grant will judge: path resolution, not the model's data I/O; the card belongs to the site that then opens the leaf."
)]
pub(super) fn step(parent: &Held, name: &EntryName, _sharing: Sharing) -> io::Result<Child> {
    let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let open = |flags| rustix::fs::openat(&parent.file, name.as_os_str(), flags, Mode::empty());
    let fd = match open(flags | OFlags::DIRECTORY) {
        Err(Errno::NOTDIR) => open(flags),
        opened => opened,
    }?;
    let handle = File::from(fd);
    let Seen { stat, id } = seen(&handle)?;
    let step = Step {
        name: name.clone(),
        id,
        kind: stat.kind,
    };
    Ok(Child {
        handle: Some(handle),
        step,
        stat,
        info: DirInfo,
    })
}

/// What `f` is, by `statx` of the descriptor itself.
pub(super) fn seen(f: &File) -> io::Result<Seen> {
    stated(f, "", AtFlags::EMPTY_PATH)
}

/// What the entry `name` of `dir` is, not followed.
pub(super) fn seen_at(dir: &File, name: &OsStr) -> io::Result<Seen> {
    stated(dir, name, AtFlags::SYMLINK_NOFOLLOW)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-stat] Stats the handle the walk holds, or one entry of a held directory without following: a metadata read, never turn-time data I/O."
)]
fn stated(at: &File, name: impl rustix::path::Arg, flags: AtFlags) -> io::Result<Seen> {
    let want = StatxFlags::BASIC_STATS | StatxFlags::BTIME;
    Ok(seen_of(&rustix::fs::statx(at, name, flags, want)?))
}

fn seen_of(sx: &Statx) -> Seen {
    let kind = match FileType::from_raw_mode(RawMode::from(sx.stx_mode)) {
        FileType::Symlink => super::Kind::Symlink,
        FileType::Directory => super::Kind::Dir,
        FileType::RegularFile => super::Kind::File,
        _ => super::Kind::Other,
    };
    let mode = u32::from(sx.stx_mode) & 0o7777;
    let btime = (sx.stx_mask & StatxFlags::BTIME.bits() != 0).then_some(&sx.stx_btime);
    Seen {
        id: ObjectId {
            dev: rustix::fs::makedev(sx.stx_dev_major, sx.stx_dev_minor),
            ino: sx.stx_ino,
        },
        stat: Stat {
            kind,
            len: sx.stx_size,
            readonly: mode & 0o222 == 0,
            mtime: time(&sx.stx_mtime),
            atime: time(&sx.stx_atime),
            btime: btime.and_then(time),
            mode,
        },
    }
}

fn time(t: &StatxTimestamp) -> Option<SystemTime> {
    super::unix::time(t.tv_sec, t.tv_nsec)
}

/// A link's target, read through the descriptor that opened it.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-link-read] Reads a link's target through the handle that opened it, to splice into the remaining name: path resolution, not data I/O."
)]
pub(super) fn read_link(link: &File) -> io::Result<PathBuf> {
    let target = rustix::fs::readlinkat(link, "", Vec::new())?;
    Ok(PathBuf::from(OsString::from_vec(target.into_bytes())))
}

/// A data handle on the object `leaf` holds, derived from the descriptor
/// through `/proc/self/fd`, so no name is resolved again.  Nothing is asked of
/// `dir`, `name` and `stat`: macOS cannot derive and opens by name.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-reopen] The one open of a held leaf, derived from its handle so no name is resolved twice.  Every caller surfaces it, each in its own way: a redirect's card is fused on by the frame that wrapped the locate, exarch's readers speak their own card, and a write's before- and after-images ride the card of the operation that asked for them."
)]
pub(super) fn derive(
    leaf: &File,
    _dir: &File,
    _name: &EntryName,
    _stat: &Stat,
    access: Access,
) -> io::Result<File> {
    let path = crate::path::proc_fd_path(leaf.as_raw_fd());
    Ok(File::from(rustix::fs::open(
        path,
        oflags(access) | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
