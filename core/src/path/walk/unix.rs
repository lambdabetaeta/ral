//! What Linux and macOS read alike: operations on a held directory handle by
//! entry name, none of which resolves a path.  A host module re-exports these
//! beside the primitives it writes itself (see the seam in `walk.rs`).

use super::{Access, EntryName, Kind, Seen, Stat, Visibility, host};
use crate::path::site::{Silent, Surface};
use crate::site::site;
use rustix::fs::{Access as Amode, AtFlags, Mode, OFlags, RawMode};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::vec::IntoIter;

/// What a Unix host records of a directory it opens: nothing, for it checks
/// no child against the directory's own path.
#[derive(Clone)]
pub(super) struct DirInfo;

/// The flags a data open of `access` asks, beside what the host adds.
pub(super) fn oflags(access: Access) -> OFlags {
    match access {
        Access::Read => OFlags::RDONLY,
        Access::Append => OFlags::WRONLY | OFlags::APPEND,
        Access::Write => OFlags::WRONLY,
    }
}

/// The instant `nanos` nanoseconds past `sec` seconds from the epoch; `None`
/// where the clock cannot show it.
pub(super) fn time(sec: i64, nanos: u32) -> Option<SystemTime> {
    let nanos = Duration::from_nanos(u64::from(nanos));
    match u64::try_from(sec) {
        Ok(secs) => UNIX_EPOCH.checked_add(Duration::from_secs(secs)),
        Err(_) => UNIX_EPOCH.checked_sub(Duration::from_secs(sec.unsigned_abs())),
    }?
    .checked_add(nanos)
}

fn opened(fd: rustix::io::Result<rustix::fd::OwnedFd>) -> io::Result<File> {
    Ok(File::from(fd?))
}

/// The permission bits a file of `visibility` is made with, under the umask:
/// only `Public` leaves the rest to it.
const fn file_mode(visibility: Visibility) -> RawMode {
    match visibility {
        Visibility::Public => 0o666,
        Visibility::Private | Visibility::Secret => 0o600,
    }
}

/// The entry `name` made exclusively for `access`: what a leaf absent at the
/// walk becomes.
pub(super) fn create(
    dir: &File,
    name: &EntryName,
    access: Access,
    visibility: Visibility,
) -> io::Result<File> {
    let flags = oflags(access) | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    opened(site!(
        Surface::Create,
        rustix::fs::openat(
            dir,
            name.as_os_str(),
            flags,
            Mode::from_bits_truncate(file_mode(visibility))
        )
    ))
}

/// A fresh sibling, read and write, that no one else has.
pub(super) fn create_sibling(
    dir: &File,
    name: &EntryName,
    visibility: Visibility,
) -> io::Result<File> {
    let flags = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    opened(site!(
        Surface::Stage,
        rustix::fs::openat(
            dir,
            name.as_os_str(),
            flags,
            Mode::from_bits_truncate(file_mode(visibility))
        )
    ))
}

pub(super) fn mkdir(dir: &File, name: &EntryName, visibility: Visibility) -> io::Result<()> {
    let mode = match visibility {
        Visibility::Public => 0o777,
        Visibility::Private | Visibility::Secret => 0o700,
    };
    Ok(site!(
        Silent::WalkMkdir,
        rustix::fs::mkdirat(dir, name.as_os_str(), Mode::from_bits_truncate(mode))
    )?)
}

/// `renameat` under `dir`: POSIX publishes by name, so `sibling`, the
/// object, is not asked.
pub(super) fn rename_over(
    dir: &File,
    _sibling: &File,
    from: &EntryName,
    to: &EntryName,
) -> io::Result<()> {
    Ok(site!(
        Surface::Publish,
        rustix::fs::renameat(dir, from.as_os_str(), dir, to.as_os_str())
    )?)
}

/// `unlinkat` under `dir`: POSIX discards by name, so `sibling`, the object, is
/// not asked.
pub(super) fn discard(dir: &File, _sibling: &File, name: &EntryName) -> io::Result<()> {
    Ok(site!(
        Silent::WalkAbandon,
        rustix::fs::unlinkat(dir, name.as_os_str(), AtFlags::empty())
    )?)
}

/// `unlinkat` under `dir`: POSIX removes by name, so `leaf`, the object the
/// walk holds, is not asked.  A directory goes with `REMOVEDIR`, and only once
/// it is empty.
pub(super) fn remove(
    dir: &File,
    _leaf: Option<&File>,
    name: &EntryName,
    kind: Kind,
) -> io::Result<()> {
    let flags = if kind == Kind::Dir {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    Ok(site!(
        Silent::WalkRemove,
        rustix::fs::unlinkat(dir, name.as_os_str(), flags)
    )?)
}

/// `renameat` under `dir`, over whatever `to` is: POSIX renames by name, so
/// `leaf`, the object the walk holds, is not asked.
pub(super) fn rename(
    dir: &File,
    _leaf: Option<&File>,
    from: &EntryName,
    to: &EntryName,
) -> io::Result<()> {
    Ok(site!(
        Silent::WalkRename,
        rustix::fs::renameat(dir, from.as_os_str(), dir, to.as_os_str())
    )?)
}

/// `fsync` of a read handle reopened on `dir`: an `O_PATH` or `O_SEARCH`
/// descriptor cannot be synced.
pub(super) fn sync_dir(dir: &File) -> io::Result<()> {
    Ok(rustix::fs::fsync(reopen(dir)?)?)
}

/// A read handle on the directory `dir` holds, the one a listing, a sync and
/// macOS's stored-name lookup need.
pub(super) fn reopen(dir: &File) -> io::Result<File> {
    opened(site!(
        Silent::WalkReopenDir,
        rustix::fs::openat(
            dir,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
    ))
}

/// `access(2)` for writing against the real uid and gid, as `test -w` asks;
/// follows.
pub(super) fn may_write(dir: &File, name: &OsStr) -> io::Result<()> {
    Ok(site!(
        Silent::WalkAccess,
        rustix::fs::accessat(dir, name, Amode::WRITE_OK, AtFlags::empty())
    )?)
}

/// `file` given the permission bits `stat` carries.
pub(super) fn copy_mode(file: &File, stat: &Stat) -> io::Result<()> {
    let bits = RawMode::try_from(stat.mode & 0o7777).unwrap_or_default();
    Ok(rustix::fs::fchmod(file, Mode::from_bits_truncate(bits))?)
}

/// The discard device, by its fixed name.
pub(super) fn open_discard() -> io::Result<File> {
    opened(site!(
        Silent::DiscardDevice,
        rustix::fs::open("/dev/null", OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
    ))
}

/// The names of a directory, sorted, each stated as it is yielded.
pub(super) struct Lister<'a> {
    dir: &'a File,
    names: IntoIter<EntryName>,
}

/// List `dir`, each name stated as it is yielded ([`names`]).
pub(super) fn list(dir: &File) -> io::Result<Lister<'_>> {
    Ok(Lister {
        dir,
        names: names(dir)?.into_iter(),
    })
}

/// The names of `dir`, read through a reopened handle, since the held one
/// cannot enumerate, and sorted so a listing does not depend on the order the
/// kernel hands them back.
pub(super) fn names(dir: &File) -> io::Result<Vec<EntryName>> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::new(reopen(dir)?)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(EntryName::parse(OsStr::from_bytes(name))?);
        }
    }
    names.sort();
    Ok(names)
}

impl Iterator for Lister<'_> {
    type Item = (EntryName, io::Result<Seen>);

    fn next(&mut self) -> Option<Self::Item> {
        let name = self.names.next()?;
        let seen = host::seen_at(self.dir, name.as_os_str());
        Some((name, seen))
    }
}
