//! Reads and sandbox handoffs through a held leaf.

use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;

use super::{
    Access, Data, Installing, Kind, Leaf, Located, ObjectId, Peeks, Reads, Stat, Visibility, host,
};

impl ObjectId {
    /// The identity of the object `f` holds, read from the handle.
    pub(crate) fn of(f: &File) -> io::Result<Self> {
        host::seen(f).map(|seen| seen.id)
    }
}

impl<A: Peeks> Located<A> {
    /// `None` when the leaf is absent.  No syscall: taken at the walk.
    pub fn stat(&self) -> Option<&Stat> {
        self.observed()
    }

    pub(crate) fn kind(&self) -> Option<Kind> {
        self.stat().map(|stat| stat.kind)
    }

    /// Kind `File` and, on Unix, `mode & 0o111`; on Windows what
    /// `is_executable_file` decides today.  Asked of an admitted program,
    /// never before the judgment.
    pub(crate) fn is_executable(&self) -> bool {
        #[cfg(unix)]
        let executable = self.stat().is_some_and(|s| s.mode & 0o111 != 0);
        #[cfg(not(unix))]
        let executable = crate::path::is_executable_file(self.real().as_path());
        self.kind() == Some(Kind::File) && executable
    }

    /// The link's target, read through the held leaf handle.
    ///
    /// # Errors
    /// `InvalidInput` where the leaf is not a link: a `Stated` leaf is never
    /// one, since a link is always held.  `NotFound` for an absent leaf.
    pub fn read_link(&self) -> io::Result<PathBuf> {
        match &self.leaf {
            Leaf::Held { file, stat, .. } if stat.kind == Kind::Symlink => host::read_link(file),
            Leaf::Absent { .. } => Err(io::ErrorKind::NotFound.into()),
            _ => Err(io::ErrorKind::InvalidInput.into()),
        }
    }

    /// `access(2)` for writing against the real uid/gid (Windows: an open for
    /// writing), never the stat's mode bits, which ignore ownership.  Follows,
    /// as `test -w` does.
    ///
    /// # Errors
    /// The host's refusal.
    pub(crate) fn may_write(&self) -> io::Result<()> {
        host::may_write(self.dir(), self.entry())
    }
}

impl Located<Installing> {
    /// The held leaf, for a bind source; `None` for an absent or stated leaf.
    #[cfg(target_os = "linux")]
    pub(crate) fn leaf_fd(&self) -> Option<BorrowedFd<'_>> {
        self.handle().map(AsFd::as_fd)
    }

    /// A handle on the leaf with `access`, sharing read, write and delete,
    /// derived from the held one: what DACL stamping needs beyond attributes.
    ///
    /// # Errors
    /// The reopen's; `NotFound` for a leaf that is not held.
    #[cfg(windows)]
    pub(crate) fn reopen(&self, access: u32) -> io::Result<File> {
        self.handle()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            .and_then(|file| host::reopen_shared(file, access))
    }

    /// Whether the leaf is on a local fixed volume.
    ///
    /// # Errors
    /// `NotFound` for a leaf that is not held.
    #[cfg(windows)]
    pub(crate) fn is_local(&self) -> io::Result<bool> {
        self.handle()
            .map(host::is_local)
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }
}

impl<A: Reads> Located<A> {
    /// A data handle on the leaf.  Held: derived from the held handle where
    /// the host can, else opened by name under `dir` and verified.  A special
    /// file (macOS): opened by name under `dir` and verified against the
    /// identity its step recorded, and to be a special file still.  Any
    /// other stated leaf: `PermissionDenied`, the walk's; nothing is opened.
    /// Absent: created exclusively with `visibility` for `Append` and `Write`,
    /// `NotFound` for `Read`.
    ///
    /// # Errors
    /// The open's; `IsADirectory` for a directory leaf, `InvalidInput` for a
    /// link, which has no data.
    pub(super) fn open(&self, access: Access, visibility: Visibility) -> io::Result<File> {
        let named = || {
            self.leaf_name()
                .expect("a leaf that is not a directory has a name")
        };
        match (self.data()?, access) {
            (Data::Held(file, stat), _) => host::derive(file, self.dir(), named(), stat, access),
            #[cfg(target_os = "macos")]
            (Data::Special, _) => {
                host::open_special(self.dir(), named(), self.trace.last_id(), access)
            }
            #[cfg(not(target_os = "macos"))]
            (Data::Special, _) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the object could not be held, so nothing is opened through it",
            )),
            (Data::Absent, Access::Read) => Err(io::ErrorKind::NotFound.into()),
            (Data::Absent, _) => host::create(self.dir(), self.absent_name()?, access, visibility),
        }
    }

    /// # Errors
    /// The open's, including `NotFound`.
    pub fn read(&self) -> io::Result<File> {
        // A read makes nothing, so no visibility is asked.
        self.open(Access::Read, Visibility::Public)
    }
}

/// The discard device, opened by its fixed name — `/dev/null`, `\\.\NUL` —
/// and never by a spelling: a spelling that folds to the device might open
/// whatever a link on the way leads to.  No argument, so there is nothing to
/// open but the device.
///
/// The one object this module opens without walking to it: exempt from the
/// walk and from the grant alike, since there is nothing to locate.
///
/// # Errors
/// The open's.
pub(crate) fn open_discard() -> io::Result<File> {
    host::open_discard()
}
