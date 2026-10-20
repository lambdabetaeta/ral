//! The real path of a file: the name exec authority judges.

use crate::path::shown;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use super::{FrozenPath, PathRules, Polarity};

/// A path with no symlink, `.` or `..`, as `realpath(3)` gives it.
///
/// Ordered as the host identifies files: by path components off Windows, by
/// [`windows_stored_components`](super::identity::windows_stored_components)
/// on it — so two keys naming one file are one key in a map.  Either way on
/// the names as the host encodes them, never on their lossy decodings, so a
/// name that is not Unicode is not the name holding U+FFFD it decodes to.
#[derive(Clone, Debug)]
pub struct RealPath(PathBuf);

impl RealPath {
    /// `realpath(3)` of `path`, which must exist.
    ///
    /// # Errors
    /// The `io::Error` of the failed resolution, e.g. `NotFound`.
    pub fn of(path: &Path) -> std::io::Result<Self> {
        super::canon::canonicalise_strict(path).map(Self)
    }

    /// The real path `path` has, or will have once its missing tail is created.
    pub fn lenient(path: &Path) -> Self {
        Self(super::canon::canonicalise_lenient(path))
    }

    /// Where a walk landed: real by construction, since the walk splices
    /// every link itself and takes every `..` physically.
    #[cfg(unix)]
    pub(crate) fn walked(located: &super::Located) -> Self {
        Self(located.real().to_path_buf())
    }

    /// The prefix's `resolved` form, as it froze: no disk access.
    pub(crate) fn frozen(prefix: &FrozenPath) -> Self {
        Self(prefix.real_path().to_path_buf())
    }

    /// The prefix's surface followed against the live disk: where an exclusion
    /// looks, since it names whatever its spelling reaches now.
    pub(crate) fn live(prefix: &FrozenPath) -> Self {
        Self(prefix.live().real_path().to_path_buf())
    }

    /// A rendered object: already the frozen real path, so no lookup.
    #[cfg(target_os = "linux")]
    pub(crate) fn rendered(object: &super::Rendered) -> Self {
        Self(object.as_str().into())
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn assumed(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Whether this path lies inside `dir`, modulo firmlink aliases, as a
    /// rule of polarity `P` reads names.
    pub fn within<P: Polarity>(&self, dir: &Self) -> bool {
        super::identity::path_within(&self.0, &dir.0, P::IDENTITY)
    }

    /// Whether `path` lies inside this one, as a rule of polarity `P` reads
    /// names; an fs access against a run place.
    pub(crate) fn contains<P: Polarity>(&self, path: &Path) -> bool {
        super::identity::path_within(path, &self.0, P::IDENTITY)
    }

    /// The entry `name` of this directory: real by construction, since a
    /// directory entry of a real directory opened without following is.
    #[cfg(any(target_os = "linux", windows, test))]
    pub(crate) fn entry(&self, name: &std::ffi::OsStr) -> Self {
        Self(self.0.join(name))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The final component, which a command name can speak of: `None` for a
    /// root, and for a name that is not Unicode, which no command name is.
    /// Never a lossy decoding, which would make such a file the one named
    /// with U+FFFD in its place.
    #[allow(
        clippy::disallowed_methods,
        reason = "native: a final component, refused where it is not UTF-8"
    )]
    pub(crate) fn name(&self) -> Option<&str> {
        self.0.file_name()?.to_str()
    }
}

impl Ord for RealPath {
    fn cmp(&self, other: &Self) -> Ordering {
        match PathRules::HOST {
            PathRules::Windows => {
                let identity =
                    |p: &Self| super::identity::windows_stored_components(p.0.as_os_str());
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
        shown(&self.0).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::RealPath;

    /// A name that is not Unicode orders apart from the U+FFFD spelling it
    /// decodes to, and is no command name.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_byte_is_not_its_replacement_character() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let raw = RealPath::assumed(OsStr::from_bytes(b"/opt/\xFFtool"));
        assert_ne!(raw, RealPath::assumed("/opt/\u{FFFD}tool"));
        assert_eq!(raw.name(), None);
    }

    /// An unpaired surrogate orders apart from the U+FFFD spelling it
    /// decodes to, and is no command name.
    #[cfg(windows)]
    #[test]
    fn an_unpaired_surrogate_is_not_its_replacement_character() {
        use std::os::windows::ffi::OsStringExt;
        let wide: Vec<u16> = r"C:\opt\"
            .encode_utf16()
            .chain([0xD800])
            .chain("tool".encode_utf16())
            .collect();
        let raw = RealPath::assumed(std::ffi::OsString::from_wide(&wide));
        assert_ne!(raw, RealPath::assumed("C:\\opt\\\u{FFFD}tool"));
        assert_eq!(raw.name(), None);
        assert_eq!(RealPath::assumed(r"C:\opt\tool").name(), Some("tool"));
    }
}
