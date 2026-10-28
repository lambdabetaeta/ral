//! What Linux and macOS read alike: operations on a held directory handle by
//! entry name, none of which resolves a path.  A host module re-exports these
//! beside the primitives it writes itself (see the seam in `walk.rs`).

use super::{Access, EntryName, Kind, ObjectId, Seen, Stat, Visibility, host};
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

/// Check physical ancestry: a walk's held lineage can predate a rename.
pub(super) fn validate_scoped_parent(file: &File) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let search = OFlags::PATH;
    #[cfg(target_os = "macos")]
    let search = OFlags::from_bits_retain(libc::O_SEARCH.cast_unsigned());
    let flags = search | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = file.try_clone()?;
    loop {
        validate_scoped_ancestor(&directory)?;
        let mode = Mode::empty();
        let parent = File::from(site!(
            Silent::WalkStep,
            rustix::fs::openat(&directory, "..", flags, mode)
        )?);
        if ObjectId::of(&directory)? == ObjectId::of(&parent)? {
            return Ok(());
        }
        directory = parent;
    }
}

/// Reject ancestors that permit another user to replace a trusted child.
fn validate_scoped_ancestor(file: &File) -> io::Result<()> {
    let stat = rustix::fs::fstat(file)?;
    let uid = rustix::process::geteuid().as_raw();
    if (stat.st_uid != uid && stat.st_uid != 0)
        || (stat.st_mode & 0o022 != 0 && stat.st_mode & 0o1000 == 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private workspace requires ancestors owned by the current user or root, with no group/other write permission unless sticky",
        ));
    }
    validate_scoped_acl(file)
}

/// Check the object actually opened, not the preceding mkdir's intention.
pub(super) fn validate_scoped_directory(file: &File) -> io::Result<()> {
    let stat = rustix::fs::fstat(file)?;
    if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private workspace must be owned by the current user with no group/other permissions",
        ));
    }
    validate_scoped_acl(file)
}

#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps, reason = "the macOS ACL check is fallible")]
fn validate_scoped_acl(_: &File) -> io::Result<()> {
    // POSIX ACL effective permissions are bounded by the mode's group mask.
    Ok(())
}

#[cfg(target_os = "macos")]
fn validate_scoped_acl(file: &File) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut c_void;
        fn acl_get_entry(
            acl: *mut c_void,
            index: libc::c_int,
            entry: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_free(acl: *mut c_void) -> libc::c_int;
    }
    // Darwin ACL_TYPE_EXTENDED and ACL_FIRST_ENTRY (sys/acl.h).
    // SAFETY: the descriptor is live; the returned ACL is owned until acl_free.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        // Darwin's filesec reports an object with no extended ACL as ENOENT.
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(()),
            _ => Err(error),
        };
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: acl is valid, entry is an out-parameter, and neither escapes.
    let result = unsafe { acl_get_entry(acl, 0, &raw mut entry) };
    let error = io::Error::last_os_error();
    // SAFETY: this allocation came from acl_get_fd_np and is freed once.
    unsafe { acl_free(acl) };
    match result {
        -1 if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        -1 => Err(error),
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private workspace requires ancestors and workspace without extended ACL entries",
        )),
    }
}

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

/// Roll back a create without entering or traversing the directory.
pub(super) fn remove_empty_dir(dir: &File, name: &EntryName) -> io::Result<()> {
    remove(dir, None, name, Kind::Dir)
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
