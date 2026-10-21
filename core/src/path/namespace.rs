//! Whose namespace a path names: this computer's, or a guest machine's.
//!
//! A [`PathBuf`] is a path on *this* computer, spelled and joined under its
//! rules.  A path inside the Linux guest a hypervisor runs is not one: on a
//! Windows host a `PathBuf` would join `/tmp` and `pip` as `/tmp\pip`, which
//! names nothing inside the guest, and would judge `/work` relative.  So a
//! guest's path is a [`GuestPath`], a string under Posix rules whatever the
//! host, and what an engine is told of its own machine is an
//! [`EnginePath`], which says which of the two it was minted in.

use super::PathRules;
use super::lex::{Rerooting, absolute_under, join_under};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

/// A path inside a Posix guest: `/`-separated, and held as the string the
/// guest spells it, since the host has no type for another machine's paths.
///
/// A convention (`/tmp`) is a `const` built [`from_static`](Self::from_static);
/// a path a machine reports is taken [`new`](Self::new); either grows by
/// [`join`](Self::join).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GuestPath(Cow<'static, str>);

impl GuestPath {
    /// The guest path `path` spells.
    #[must_use]
    pub const fn from_static(path: &'static str) -> Self {
        Self(Cow::Borrowed(path))
    }

    /// The guest path `path` spells.
    #[must_use]
    pub fn new(path: String) -> Self {
        Self(Cow::Owned(path))
    }

    /// The path as the guest spells it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the path is absolute by the guest's rule — a leading `/` —
    /// and not the host's, under which, on Windows, `/work` is not.
    #[must_use]
    pub fn is_absolute(&self) -> bool {
        absolute_under(self.as_str(), PathRules::Posix)
    }

    /// [`join_slash`](super::lex::join_slash) in the guest's namespace: `sub`
    /// joined onto this path under Posix rules, whatever the host
    /// ([`join_under`]).
    ///
    /// `sub` is a constant the program spells, `/`-separated; an empty
    /// component, from a leading, doubled or trailing `/`, adds nothing, so
    /// the two joins differ only in the namespace they answer in.
    #[must_use]
    pub fn join(&self, sub: &'static str) -> Self {
        let joined = join_under(&self.0, sub, PathRules::Posix)
            .unwrap_or_else(|Rerooting| unreachable!("Posix rules re-root no join"));
        Self(Cow::Owned(joined))
    }
}

/// As the guest spells it: a guest path is already `/`-separated, which is
/// how ral shows every path.
impl fmt::Display for GuestPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A path on the machine an engine runs on, named by the front-end that
/// attaches it, in the namespace the front-end can name it in.
///
/// An engine that shares the front-end's computer — in its process, or a
/// child of it — is told a [`Host`](Self::Host) path; one inside a guest is
/// told a [`Guest`](Self::Guest) path, which the front-end has no `PathBuf`
/// for.  Either way it is the engine's own namespace, so the engine reads both
/// as [`native`](Self::native).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnginePath {
    /// A path on the front-end's own computer, which the engine shares.
    Host(PathBuf),
    /// A path inside the guest the engine runs in.
    Guest(GuestPath),
}

impl EnginePath {
    /// The path as the engine's operating system names it.
    ///
    /// Engine-side only: a [`Guest`](Self::Guest) path is native to the
    /// guest whose engine reads it, and to no front-end.
    pub(crate) fn native(&self) -> &Path {
        match self {
            Self::Host(path) => path,
            #[allow(
                clippy::disallowed_methods,
                reason = "native: a guest path read by the guest's own engine, whose OS spells it \
                          as the guest does"
            )]
            Self::Guest(path) => Path::new(path.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GuestPath;

    #[test]
    fn a_guest_join_is_slash_separated_on_every_host() {
        let tmp = GuestPath::from_static("/tmp");
        assert_eq!(tmp.join("pip").as_str(), "/tmp/pip");
        assert_eq!(tmp.join("/a//b/").as_str(), "/tmp/a/b");
        assert_eq!(GuestPath::from_static("/").join("work").as_str(), "/work");
        // A drive is a name to the guest, whatever the host reads it as.
        assert_eq!(tmp.join("C:/x").as_str(), "/tmp/C:/x");
    }

    #[test]
    fn a_guest_path_is_absolute_by_the_guests_rule() {
        assert!(GuestPath::from_static("/work").is_absolute());
        assert!(!GuestPath::from_static(r"C:\work").is_absolute());
        assert!(!GuestPath::from_static("work").is_absolute());
    }
}
