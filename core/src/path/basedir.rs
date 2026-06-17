//! The single XDG base-directory resolver.
//!
//! [`XdgKind`] names the directory roles the [XDG basedir spec]
//! defines (`config`, `data`, `cache`, `state`) plus the non-spec
//! but conventional `bin`.  [`resolve_xdg`] maps a kind and a home
//! directory to its absolute path, and every caller that asks
//! "where does the user keep their config/data/…?" goes through
//! it — the grant sigil expander in [`crate::path::sigil`] and the
//! `ral` binary's own config/data loaders in [`crate::path::config`]
//! alike — so the two never diverge.
//!
//! The policy is XDG-everywhere: the Linux defaults
//! (`.config`, `.local/share`, …) apply on every platform,
//! including macOS, matching how cross-platform CLI tools and
//! dotfiles use XDG.  An `$XDG_*_HOME` override is honoured only
//! when it holds an absolute path, per the spec's rule that
//! relative values are ignored; the home-joined default applies
//! otherwise.
//!
//! [XDG basedir spec]: https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html

use std::path::{Path, PathBuf};

/// One of the XDG basedir kinds we expose.
///
/// `Config`, `Data`, `Cache`, `State` follow the [XDG basedir spec].
/// `Bin` is non-spec but conventional: many dotfiles set
/// `XDG_BIN_HOME=$HOME/.local/bin` and we honour it the same way.
///
/// [XDG basedir spec]: https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html
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

    /// All known kinds, in the canonical order.  Used by error
    /// messages so a typo lists the alternatives.
    pub fn all() -> &'static [&'static str] {
        &["config", "data", "cache", "state", "bin"]
    }

    /// Lower-case `NAME` form, the way authors write it in policy.
    pub fn token_name(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Data => "data",
            Self::Cache => "cache",
            Self::State => "state",
            Self::Bin => "bin",
        }
    }

    /// The env var that overrides the default for this kind.  Used
    /// both to read the override and to name it in error messages.
    /// `bin` is non-spec but `XDG_BIN_HOME` is the conventional
    /// override.
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data => "XDG_DATA_HOME",
            Self::Cache => "XDG_CACHE_HOME",
            Self::State => "XDG_STATE_HOME",
            Self::Bin => "XDG_BIN_HOME",
        }
    }

    /// The home-relative default suffix this kind falls back to
    /// when its env var is unset or relative — the Linux layout,
    /// applied on every platform.
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

/// Resolve an XDG kind to its absolute filesystem path.
///
/// Reads the corresponding `XDG_*_HOME` env var if it holds an
/// absolute path; otherwise falls back to `home` joined with the
/// kind's [default suffix](XdgKind::default_suffix).  Both routes
/// share the same `home` argument so a `within [shell: HOME=…]`
/// override flows through with the same semantics as tilde sigils —
/// no detour through a separate process-level HOME lookup.
///
/// The XDG spec rule "relative values are ignored" is encoded in
/// [`absolute_env_var`].
#[allow(clippy::disallowed_methods)]
pub fn resolve_xdg(kind: XdgKind, home: &str) -> PathBuf {
    absolute_env_var(kind.env_var()).unwrap_or_else(|| Path::new(home).join(kind.default_suffix()))
}

/// Read an env var and return it as a `PathBuf` only if it parses
/// as an absolute path — matches the XDG spec's rule that relative
/// values are ignored.
#[allow(clippy::disallowed_methods)]
fn absolute_env_var(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

// Unix-only: the assertions compare Unix path shapes; the resolver
// itself is platform-agnostic, but `/h/.config` vs `\h\.config`
// makes the literal comparisons Unix-specific.
#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    /// Mutate one `XDG_*_HOME` var around `f`, restoring it after.
    /// Holds the shared [`crate::test_env::env_guard`] so concurrent
    /// env-mutating tests under `RUST_TEST_THREADS > 1` stay serial.
    fn with_var(key: &str, val: Option<&str>, f: impl FnOnce()) {
        let _guard = crate::test_env::env_guard();
        let prev = std::env::var_os(key);
        match val {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        f();
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

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
