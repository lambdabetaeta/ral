//! What Linux and macOS read alike: operations on a held directory handle by
//! entry name, none of which resolves a path.  A host module re-exports these
//! beside the primitives it writes itself (see the seam in `walk.rs`).

use super::{Access, EntryName, Seen, Stat, Visibility, host};
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
    visibility: Visibility,
) -> io::Result<File> {
    let flags = oflags(access) | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mode = match visibility {
        Visibility::Public => 0o666,
        Visibility::Private => 0o600,
    };
    opened(rustix::fs::openat(
        dir,
        name.as_os_str(),
        flags,
        Mode::from_bits_truncate(mode),
    ))
}

/// A fresh sibling, read and write, that no one else has.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-stage] The atomic `>` staging create: a fresh exclusive sibling in the target's own directory, holding the write until the rename publishes it.  A sub-step of the write site; the reported write is the operation's surface."
)]
pub(super) fn create_sibling(dir: &File, name: &EntryName) -> io::Result<File> {
    let flags = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    opened(rustix::fs::openat(
        dir,
        name.as_os_str(),
        flags,
        Mode::from_bits_truncate(0o666),
    ))
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-mkdir] Makes an absent entry of a held directory, which the caller judged as the absent leaf it is; the directory is then stepped onto."
)]
pub(super) fn mkdir(dir: &File, name: &EntryName, visibility: Visibility) -> io::Result<()> {
    let mode = match visibility {
        Visibility::Public => 0o777,
        Visibility::Private => 0o700,
    };
    Ok(rustix::fs::mkdirat(
        dir,
        name.as_os_str(),
        Mode::from_bits_truncate(mode),
    )?)
}

/// `renameat` under `dir`: POSIX publishes by name, so `sibling`, the
/// object, is not asked.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:walk-publish] The atomic `>` commit step: rename the staged sibling over the target's name within the one directory handle.  The write surface fires when the write settles, committed once this returns Ok."
)]
pub(super) fn rename_over(
    dir: &File,
    _sibling: &File,
    from: &EntryName,
    to: &EntryName,
) -> io::Result<()> {
    Ok(rustix::fs::renameat(
        dir,
        from.as_os_str(),
        dir,
        to.as_os_str(),
    )?)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-abandon] Reasoned-silent rollback of the atomic `>`: unlink the staged sibling for a write that will not land.  The aborted write is the surface; this removal raises none of its own."
)]
pub(super) fn unlink(dir: &File, name: &EntryName) -> io::Result<()> {
    Ok(rustix::fs::unlinkat(
        dir,
        name.as_os_str(),
        AtFlags::empty(),
    )?)
}

/// `fsync` of a read handle reopened on `dir`: an `O_PATH` or `O_SEARCH`
/// descriptor cannot be synced.
pub(super) fn sync_dir(dir: &File) -> io::Result<()> {
    Ok(rustix::fs::fsync(reopen(dir)?)?)
}

/// A read handle on the directory `dir` holds, the one a listing, a sync and
/// macOS's stored-name lookup need.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-reopen-dir] Reopens a held directory for reading, which an `O_PATH` or `O_SEARCH` descriptor cannot do: for a listing and a flush, relative to the directory itself."
)]
pub(super) fn reopen(dir: &File) -> io::Result<File> {
    opened(rustix::fs::openat(
        dir,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ))
}

/// `access(2)` against the real uid and gid, as `test -w` asks; follows.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-access] The `is-writable` predicate's `access(2)` against the real uid and gid, by name under the held directory.  A permission predicate, not turn-time model data I/O."
)]
pub(super) fn is_writable(dir: &File, name: &OsStr) -> bool {
    rustix::fs::accessat(dir, name, Amode::WRITE_OK, AtFlags::empty()).is_ok()
}

/// `file` given the permission bits `stat` carries.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-chmod] Gives the staged sibling the mode its target had, before anything is written to it.  A sub-step of the write site."
)]
pub(super) fn copy_mode(file: &File, stat: &Stat) -> io::Result<()> {
    let bits = RawMode::try_from(stat.mode & 0o7777).unwrap_or_default();
    Ok(rustix::fs::fchmod(file, Mode::from_bits_truncate(bits))?)
}

/// The discard device, by its fixed name.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:discard-device] `/dev/null` opened by its fixed name: no bytes reach or leave the model, and no grant region can contain a device that is not a file."
)]
pub(super) fn open_discard() -> io::Result<File> {
    opened(rustix::fs::open(
        "/dev/null",
        OFlags::RDWR | OFlags::CLOEXEC,
        Mode::empty(),
    ))
}

/// The names of a directory, sorted, each stated as it is yielded.
pub(super) struct Lister<'a> {
    dir: &'a File,
    names: IntoIter<EntryName>,
}

/// List `dir`: its names are read through a reopened handle, since the held
/// one cannot enumerate, and sorted so a listing does not depend on the
/// order the kernel hands them back.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-list] `list-dir`'s enumeration of a held directory, relative to its own handle.  A listing predicate, not turn-time model data I/O: the caller still judges each entry and raises no card."
)]
pub(super) fn list(dir: &File) -> io::Result<Lister<'_>> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::new(reopen(dir)?)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(EntryName::parse(OsStr::from_bytes(name))?);
        }
    }
    names.sort();
    Ok(Lister {
        dir,
        names: names.into_iter(),
    })
}

impl Iterator for Lister<'_> {
    type Item = (EntryName, io::Result<Seen>);

    fn next(&mut self) -> Option<Self::Item> {
        let name = self.names.next()?;
        let seen = host::seen_at(self.dir, name.as_os_str());
        Some((name, seen))
    }
}
