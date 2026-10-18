//! The single XDG base-directory resolver.
//!
//! Everything that asks where the user keeps their config/data/… goes
//! through [`resolve_xdg_in`], which is pure: it reads variables through a closure.
//! [`crate::host::resolve_xdg`] closes it over the process environment, for
//! [`crate::host::xdg`] (the host's own bases) and the `xdg:` grant sigil in
//! [`crate::path::sigil`] with the overlay's home.
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

/// Resolve an XDG kind: an absolute `XDG_*_HOME` read through `env`, else
/// `home` joined with the kind's [default suffix](XdgKind::default_suffix).
///
/// `home` is the caller's, never a process-level `HOME` lookup, so a
/// shell-scoped `HOME=` reaches here exactly as it reaches tilde expansion.
/// The `XDG_*_HOME` vars themselves come from `env`: the process environment
/// for [`crate::host::resolve_xdg`].
///
/// `None` when nothing absolute resolves: no home to default under, and no
/// absolute override to stand in for one.  An unknown home is thus not the
/// same question as an absolute `XDG_CONFIG_HOME` — that one is still
/// answerable, and answered.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: lifts the caller's `home` to join the spec's default suffix under it"
)]
pub(crate) fn resolve_xdg_in(
    env: impl Fn(&str) -> Option<String>,
    kind: XdgKind,
    home: Option<&str>,
) -> Option<PathBuf> {
    // The spec ignores a relative override.
    env(kind.env_var())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
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

// Unix-only: the resolver is platform-agnostic, the asserted literals are not.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// With no home the default has nothing to hang off, so the kind is
    /// unresolved rather than rooted at `/`; an absolute override still
    /// answers, home or no home.
    #[test]
    fn resolution_follows_the_override_then_the_home_default() {
        use XdgKind::*;
        let cases = [
            (State, None, Some("/h"), Some("/h/.local/state")),
            (
                Cache,
                Some("/var/cache/me"),
                Some("/h"),
                Some("/var/cache/me"),
            ),
            (
                Config,
                Some("relative/conf"),
                Some("/h"),
                Some("/h/.config"),
            ),
            (Data, None, None, None),
            (Data, Some("/srv/data"), None, Some("/srv/data")),
        ];
        for (kind, var, home, want) in cases {
            let env = |k: &str| var.filter(|_| k == kind.env_var()).map(String::from);
            assert_eq!(
                resolve_xdg_in(env, kind, home),
                want.map(PathBuf::from),
                "{kind:?} with {var:?} under {home:?}"
            );
        }
    }
}
