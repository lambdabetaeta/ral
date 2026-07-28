//! Named subpaths under the XDG bases, plus the legacy `$HOME/.<name>`
//! form, for `ral`'s own config and data files.
//!
//! The bases come from [`resolve_xdg`], the resolver the `xdg:` grant
//! sigil also expands through.  `None` throughout means nothing absolute
//! resolved: `$HOME` unset and no absolute `$XDG_*_HOME` override.

use std::path::PathBuf;

use super::basedir::{XdgKind, resolve_xdg};
use super::home_from_env;

fn base(kind: XdgKind) -> Option<PathBuf> {
    let path = resolve_xdg(kind, &home_from_env());
    path.is_absolute().then_some(path)
}

/// The XDG config base joined with `subpath` (e.g. `"ral/rc"`).
pub fn xdg_config_subpath(subpath: &str) -> Option<PathBuf> {
    base(XdgKind::Config).map(|base| base.join(subpath))
}

/// The XDG data base joined with `subpath` (e.g. `"ral/exit-hints.txt"`).
pub fn xdg_data_subpath(subpath: &str) -> Option<PathBuf> {
    base(XdgKind::Data).map(|base| base.join(subpath))
}

/// `$HOME/<dot_name>` (e.g. `home_dot(".ralrc")`) — the single-file
/// convention rc loaders probe as a fallback to the XDG form.
#[allow(clippy::disallowed_methods)]
pub fn home_dot(dot_name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(home_from_env()).join(dot_name);
    p.is_absolute().then_some(p)
}
