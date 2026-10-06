//! The single XDG base-directory resolver.
//!
//! Everything that asks where the user keeps their config/data/… goes
//! through [`resolve_xdg`]: [`super::xdg`] for the host's own, and the `xdg:`
//! grant sigil in [`crate::path::sigil`] with the overlay's home.
//!
//! XDG everywhere: the Linux defaults (`.config`, `.local/share`, …) apply on
//! every platform, macOS included.  An `XDG_*_HOME` override counts only when
//! it is absolute, per the spec's rule that relative values are ignored.

use std::path::{Path, PathBuf};
use strum::{EnumString, IntoStaticStr, VariantArray};

/// An XDG basedir role.  `Bin` is outside the spec, but `XDG_BIN_HOME` is
/// conventional enough that we honour it identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum XdgKind {
    Config,
    Data,
    Cache,
    State,
    Bin,
}

impl XdgKind {
    /// The env var that overrides this kind's default.
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data => "XDG_DATA_HOME",
            Self::Cache => "XDG_CACHE_HOME",
            Self::State => "XDG_STATE_HOME",
            Self::Bin => "XDG_BIN_HOME",
        }
    }

    /// The home-relative default used when the env var is unset or relative.
    pub(crate) fn default_suffix(self) -> &'static str {
        match self {
            Self::Config => ".config",
            Self::Data => ".local/share",
            Self::Cache => ".cache",
            Self::State => ".local/state",
            Self::Bin => ".local/bin",
        }
    }
}

/// Resolve an XDG kind: an absolute `XDG_*_HOME`, else `home` joined with the
/// kind's [default suffix](XdgKind::default_suffix).
///
/// `home` is the caller's, never a process-level `HOME` lookup, so a
/// shell-scoped `HOME=` reaches here exactly as it reaches tilde expansion.
/// The `XDG_*_HOME` vars themselves still come from the process environment.
///
/// `None` when nothing absolute resolves: no home to default under, and no
/// absolute override to stand in for one.  An unknown home is thus not the
/// same question as an absolute `XDG_CONFIG_HOME` — that one is still
/// answerable, and answered.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: lifts the caller's `home` to join the spec's default suffix under it"
)]
pub(crate) fn resolve_xdg(kind: XdgKind, home: Option<&str>) -> Option<PathBuf> {
    absolute_env_var(kind.env_var()).or_else(|| {
        // `default_suffix()` is spelled with the spec's `/`, so join it
        // component-by-component rather than as one literal — otherwise the
        // result mixes native separators with a stray `/` on Windows.
        Some(
            kind.default_suffix()
                .split('/')
                .fold(Path::new(home?).to_path_buf(), |acc, part| acc.join(part)),
        )
    })
}

/// The var's value, kept only when absolute — the spec ignores relative ones.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: an `XDG_*_HOME` override is the host's, kept only when absolute per the spec"
)]
fn absolute_env_var(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

// Unix-only: the resolver is platform-agnostic, the asserted literals are not.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_env::with_var;

    #[test]
    fn unset_var_falls_back_to_home_default() {
        with_var("XDG_STATE_HOME", None, || {
            assert_eq!(
                resolve_xdg(XdgKind::State, Some("/h")),
                Some(PathBuf::from("/h/.local/state"))
            );
        });
    }

    #[test]
    fn absolute_var_overrides_home() {
        with_var("XDG_CACHE_HOME", Some("/var/cache/me"), || {
            assert_eq!(
                resolve_xdg(XdgKind::Cache, Some("/h")),
                Some(PathBuf::from("/var/cache/me"))
            );
        });
    }

    #[test]
    fn relative_var_is_ignored_per_spec() {
        with_var("XDG_CONFIG_HOME", Some("relative/conf"), || {
            assert_eq!(
                resolve_xdg(XdgKind::Config, Some("/h")),
                Some(PathBuf::from("/h/.config"))
            );
        });
    }

    /// With no home the default has nothing to hang off, so the kind is
    /// unresolved rather than rooted at `/` — but an absolute override still
    /// answers, home or no home.
    #[test]
    fn unknown_home_resolves_only_through_an_absolute_override() {
        with_var("XDG_DATA_HOME", None, || {
            assert_eq!(resolve_xdg(XdgKind::Data, None), None);
        });
        with_var("XDG_DATA_HOME", Some("/srv/data"), || {
            assert_eq!(
                resolve_xdg(XdgKind::Data, None),
                Some(PathBuf::from("/srv/data"))
            );
        });
    }
}
