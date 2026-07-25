//! XDG-style base directories and the named-subpath helpers
//! that build on them.
//!
//! Both flavours defer to the single resolver,
//! [`resolve_xdg`], so the directories
//! they report match the ones the `xdg:` grant sigil expands to:
//! an absolute `$XDG_*_HOME` override, else the home-joined Linux
//! default (`.config`, `.local/share`) on every platform.  A base
//! resolves to `None` only when neither route yields an absolute
//! path — `$HOME` unset and no absolute override — leaving the
//! caller to decide how to surface the failure.
//!
//! [`home_dot`] complements the XDG locations with the legacy
//! `$HOME/.<name>` convention so loaders can probe both shapes
//! through one module.

use std::path::PathBuf;

use super::basedir::{XdgKind, resolve_xdg};
use super::home_from_env;

/// Resolve a kind against the process `$HOME`, keeping the result
/// only when it is absolute — i.e. an absolute override was set, or
/// the home-joined default is itself absolute.
fn base(kind: XdgKind) -> Option<PathBuf> {
    let path = resolve_xdg(kind, &home_from_env());
    path.is_absolute().then_some(path)
}

/// The XDG config base joined with `subpath` (e.g. `"ral/rc"`).
/// `None` when no config base resolves.
pub fn xdg_config_subpath(subpath: &str) -> Option<PathBuf> {
    base(XdgKind::Config).map(|base| base.join(subpath))
}

/// The XDG data base joined with `subpath` (e.g. `"ral/exit-hints.txt"`).
/// `None` when no data base resolves.
pub fn xdg_data_subpath(subpath: &str) -> Option<PathBuf> {
    base(XdgKind::Data).map(|base| base.join(subpath))
}

/// `$HOME/<dot_name>` (e.g. `home_dot(".ralrc")`), or `None` when the
/// result is not absolute — i.e. `$HOME` is empty or itself relative.
///
/// Encodes the legacy single-file convention so rc loaders can probe
/// `$XDG_CONFIG_HOME/<app>/<file>` *and* the home-dot form through one
/// module.  Applies the same absolute-path filter as [`base`].
#[allow(clippy::disallowed_methods)]
pub fn home_dot(dot_name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(home_from_env()).join(dot_name);
    p.is_absolute().then_some(p)
}
