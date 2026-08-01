//! The single XDG base-directory resolver.
//!
//! Everything that asks where the user keeps their config/data/… goes
//! through [`resolve_xdg`], so the `xdg:` grant sigil in
//! [`crate::path::sigil`] and the loaders in [`crate::path::config`] cannot
//! drift apart.
//!
//! XDG everywhere: the Linux defaults (`.config`, `.local/share`, …) apply on
//! every platform, macOS included.  An `$XDG_*_HOME` override counts only when
//! it is absolute, per the spec's rule that relative values are ignored.

use std::path::{Path, PathBuf};

/// An XDG basedir role.  `Bin` is outside the spec, but `XDG_BIN_HOME` is
/// conventional enough that we honour it identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdgKind {
    Config,
    Data,
    Cache,
    State,
    Bin,
}

impl XdgKind {
    /// Parse the `NAME` part of an `xdg:NAME` token.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "config" => Some(Self::Config),
            "data" => Some(Self::Data),
            "cache" => Some(Self::Cache),
            "state" => Some(Self::State),
            "bin" => Some(Self::Bin),
            _ => None,
        }
    }

    /// Every kind's token name, so a typo can be answered with the alternatives.
    pub fn all() -> &'static [&'static str] {
        &["config", "data", "cache", "state", "bin"]
    }

    /// The lower-case `NAME` a policy author writes.
    pub fn token_name(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Data => "data",
            Self::Cache => "cache",
            Self::State => "state",
            Self::Bin => "bin",
        }
    }

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
    pub fn default_suffix(self) -> &'static str {
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
/// `home` is the caller's, never a process-level `$HOME` lookup, so a
/// shell-scoped `HOME=` reaches here exactly as it reaches tilde expansion.
/// The `XDG_*_HOME` vars themselves still come from the process environment.
#[allow(clippy::disallowed_methods)]
pub fn resolve_xdg(kind: XdgKind, home: &str) -> PathBuf {
    absolute_env_var(kind.env_var()).unwrap_or_else(|| {
        // `default_suffix()` is spelled with the spec's `/`, so join it
        // component-by-component rather than as one literal — otherwise the
        // result mixes native separators with a stray `/` on Windows.
        kind.default_suffix()
            .split('/')
            .fold(Path::new(home).to_path_buf(), |acc, part| acc.join(part))
    })
}

/// The var's value, kept only when absolute — the spec ignores relative ones.
#[allow(clippy::disallowed_methods)]
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
                resolve_xdg(XdgKind::State, "/h"),
                PathBuf::from("/h/.local/state")
            );
        });
    }

    #[test]
    fn absolute_var_overrides_home() {
        with_var("XDG_CACHE_HOME", Some("/var/cache/me"), || {
            assert_eq!(
                resolve_xdg(XdgKind::Cache, "/h"),
                PathBuf::from("/var/cache/me")
            );
        });
    }

    #[test]
    fn relative_var_is_ignored_per_spec() {
        with_var("XDG_CONFIG_HOME", Some("relative/conf"), || {
            assert_eq!(
                resolve_xdg(XdgKind::Config, "/h"),
                PathBuf::from("/h/.config")
            );
        });
    }
}
