//! Named subpaths under the XDG bases, plus the legacy `$HOME/.<name>`
//! form, for `ral`'s own config and data files.
//!
//! The bases come from [`resolve_xdg`], the resolver the `xdg:` grant
//! sigil also expands through.  `None` throughout means nothing absolute
//! resolved: `$HOME` unset and no absolute `$XDG_*_HOME` override.

use std::path::PathBuf;

use crate::host;

use super::basedir::{XdgKind, resolve_xdg};

#[allow(
    clippy::disallowed_methods,
    reason = "host-env: ral's own config/data live where the tool is installed — a script's env overlay must not relocate them"
)]
fn base(kind: XdgKind) -> Option<PathBuf> {
    resolve_xdg(kind, host::home().as_deref()).filter(|p| p.is_absolute())
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
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: the dot-file convention names the launching user's real home — a script's env overlay must not relocate it"
)]
pub fn home_dot(dot_name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(host::home()?).join(dot_name);
    p.is_absolute().then_some(p)
}
