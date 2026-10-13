//! The real path of a file: the name exec authority judges.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use super::{FrozenPath, PathRules, Polarity};

/// A path with no symlink, `.` or `..`, as `realpath(3)` gives it.
///
/// Ordered as the host identifies files: by path components off Windows, by
/// [`windows_stored_components`](super::identity::windows_stored_components)
/// on it — so two keys naming one file are one key in a map.
#[derive(Clone, Debug)]
pub struct RealPath(PathBuf);

impl RealPath {
    /// `realpath(3)` of `path`, which must exist.
    pub(crate) fn of(path: &Path) -> std::io::Result<Self> {
        super::canon::canonicalise_strict(path).map(Self)
    }

    /// The name behind the opened image is what the grant judges.
    #[cfg(target_os = "linux")]
    pub(crate) fn open_image(path: &Path) -> std::io::Result<(Self, std::os::fd::OwnedFd)> {
        use rustix::fs::{Mode, OFlags, fstat, open, readlink};
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStringExt;
        let image = open(path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())?;
        let stat = fstat(&image)?;
        if stat.st_nlink == 0 {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        let name = readlink(super::proc_fd_path(image.as_raw_fd()), Vec::new())?;
        if name.as_bytes().ends_with(b" (deleted)") {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        let real = PathBuf::from(std::ffi::OsString::from_vec(name.into_bytes()));
        Ok((Self(real), image))
    }

    /// The prefix's `resolved` form, as it froze: no disk access.
    pub(crate) fn frozen(prefix: &FrozenPath) -> Self {
        Self(prefix.real_path().to_path_buf())
    }

    /// The prefix's surface followed against the live disk: where an exclusion
    /// looks, since it names whatever its spelling reaches now.
    pub(crate) fn live(prefix: &FrozenPath) -> Self {
        Self(prefix.live_real())
    }

    /// A rendered object: already the frozen real path, so no lookup.
    #[cfg(target_os = "linux")]
    pub(crate) fn rendered(object: &super::Rendered) -> Self {
        Self(object.as_str().into())
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn assumed(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Whether this path lies inside `dir`, modulo firmlink aliases, as a
    /// rule of polarity `P` reads names.
    pub(crate) fn within<P: Polarity>(&self, dir: &Self) -> bool {
        super::identity::path_within(&self.0, &dir.0, P::IDENTITY)
    }

    /// Whether `path` lies inside this one, as a rule of polarity `P` reads
    /// names; an fs access against a run place.
    pub(crate) fn contains<P: Polarity>(&self, path: &Path) -> bool {
        super::identity::path_within(path, &self.0, P::IDENTITY)
    }

    /// The entry `name` of this directory: real by construction, since a
    /// directory entry of a real directory opened without following is.
    #[cfg(target_os = "linux")]
    pub(crate) fn entry(&self, name: &std::ffi::OsStr) -> Self {
        Self(self.0.join(name))
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    /// The final component, empty for a root.
    pub(crate) fn name(&self) -> Cow<'_, str> {
        self.0
            .file_name()
            .map_or(Cow::Borrowed(""), |name| name.to_string_lossy())
    }
}

impl Ord for RealPath {
    fn cmp(&self, other: &Self) -> Ordering {
        match PathRules::HOST {
            PathRules::Windows => {
                let identity =
                    |p: &Self| super::identity::windows_stored_components(&p.0.to_string_lossy());
                identity(self).cmp(&identity(other))
            }
            PathRules::Posix => self.0.cmp(&other.0),
        }
    }
}

impl PartialOrd for RealPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RealPath {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RealPath {}

impl std::fmt::Display for RealPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.display().fmt(f)
    }
}
