//! The real path of a file: the name exec authority judges.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use super::{NormalizedPrefix, Polarity};

/// A path with no symlink, `.` or `..`, as `realpath(3)` gives it.
///
/// Ordered as the host identifies files: by path components off Windows, by
/// [`windows_identity_components`](super::lex::windows_identity_components)
/// on it — so two keys naming one file are one key in a map.
#[derive(Clone, Debug)]
pub struct RealPath(PathBuf);

impl RealPath {
    /// `realpath(3)` of `path`, which must exist.
    pub(crate) fn of(path: &Path) -> std::io::Result<Self> {
        super::canon::canonicalise_strict(path).map(Self)
    }

    /// The prefix's `resolved` form, as it froze: no disk access.
    pub(crate) fn frozen(prefix: &NormalizedPrefix) -> Self {
        Self(prefix.resolved_path().to_path_buf())
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn assumed(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Whether this path lies inside `dir`, modulo firmlink aliases, as a
    /// rule of polarity `P` reads names.
    pub(crate) fn within<P: Polarity>(&self, dir: &Self) -> bool {
        super::lex::path_within(&self.0, &dir.0, P::IDENTITY)
    }

    /// Depth in components of the alias-folded form, so a firmlink spelling
    /// buys no rank.
    pub(crate) fn depth(&self) -> usize {
        super::lex::identity_depth(&self.0.to_string_lossy(), cfg!(windows))
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
        if cfg!(windows) {
            let identity =
                |p: &Self| super::lex::windows_identity_components(&p.0.to_string_lossy());
            identity(self).cmp(&identity(other))
        } else {
            self.0.cmp(&other.0)
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
