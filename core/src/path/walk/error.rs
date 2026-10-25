use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// `ELOOP`'s wording; `io::ErrorKind::FilesystemLoop` is not yet stable.
pub(super) const LOOP_MESSAGE: &str = "too many levels of symbolic links";

/// A walk's refusal of a link, or of a name still splicing past the budget.
#[derive(Debug)]
struct Looped;

impl fmt::Display for Looped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(LOOP_MESSAGE)
    }
}

impl std::error::Error for Looped {}

pub(super) fn loop_error() -> io::Error {
    io::Error::other(Looped)
}

/// Whether `e` is the walk's `ELOOP`: a link met where none is allowed, or a
/// splice budget spent.
pub(crate) fn is_loop(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Looped>)
}

/// A walk's refusal where the link at the spelling's last name leads nowhere:
/// the `NotFound` met past it, and the target the link named.  Shows as that
/// `NotFound`.
#[derive(Debug)]
struct Dangling {
    target: PathBuf,
    error: io::Error,
}

impl fmt::Display for Dangling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for Dangling {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub(super) fn dangling_error(source: io::Error, target: PathBuf) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        Dangling {
            target,
            error: source,
        },
    )
}

/// The target a link at the spelling's last name named, as the link spelled
/// it, where the walk could not reach it: read off its `NotFound`, as
/// [`is_loop`] reads `ELOOP`.  `None` for any other error.
pub(crate) fn dangling(e: &io::Error) -> Option<&Path> {
    e.get_ref()?
        .downcast_ref::<Dangling>()
        .map(|d| d.target.as_path())
}

/// A staged write's refusal where the sibling's create was refused: the
/// directory's refusal, not the leaf's.  Shows as the words that say so.
#[derive(Debug)]
struct DirectoryRefused;

impl fmt::Display for DirectoryRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("its directory is not writable")
    }
}

impl std::error::Error for DirectoryRefused {}

pub(super) fn directory_error() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, DirectoryRefused)
}

/// Whether `e` is a staged write's [`DirectoryRefused`]: read off its
/// `PermissionDenied`, as [`is_loop`] reads `ELOOP`.  `false` for any other
/// error, including the leaf's own refusal.
pub(crate) fn directory_refused(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<DirectoryRefused>)
}

/// The error [`Located::stage`](super::held::Located::stage) returns where the
/// sibling's create is refused, built as it builds it, for the tests that
/// render it where no directory can be made unwritable (root writes every
/// directory).
#[cfg(test)]
pub(crate) fn directory_refusal_for_test() -> io::Error {
    directory_error()
}
