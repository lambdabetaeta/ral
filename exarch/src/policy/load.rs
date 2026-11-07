//! The loading half of exarch's capability composition: `ral_core`'s profile
//! loader under the flag that named the file, and the cwd-relative path helper
//! `super::for_invocation` calls around it.

use ral_core::authority::grant::freeze::FreezeCtx;
use ral_core::authority::policy::{Fragment, GrantLayer};
use ral_core::error::Error;
use ral_core::shell::runtime::{Break, Mooring, Shell};
use std::path::{Path, PathBuf};

use ral_core::path;

/// Read a capabilities profile from `path` as a resolved [`GrantLayer`].
///
/// Sigils freeze against `ctx` here, so a bad `xdg:` fails at the profile that
/// names it even where a later `meet` would have discarded the grant, and an
/// `exit` inside the profile flattens into an error: a profile is
/// configuration, not control flow.
pub(super) fn load_capabilities_ral(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    flag: &str,
    ctx: &FreezeCtx,
) -> Result<GrantLayer, Error> {
    load(flag, || {
        ral_core::shell::load::profile::load_capabilities_from_path(mooring, shell, path, ctx)
    })
}

/// [`load_capabilities_ral`] for an extension file, which only
/// [`GrantLayer::extend`] may consume.
pub(super) fn load_fragment_ral(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    flag: &str,
    ctx: &FreezeCtx,
) -> Result<Fragment, Error> {
    load(flag, || {
        ral_core::shell::load::profile::load_fragment_from_path(mooring, shell, path, ctx)
    })
}

/// `read`'s failure under `flag`: its words already name the file.
fn load<T>(flag: &str, read: impl FnOnce() -> Result<T, Break>) -> Result<T, Error> {
    read().map_err(|e| match e {
        Break::Error(err) => err.with_context(flag),
        escape @ Break::Escape(_) => Error::new(format!("{flag}: {escape}")),
    })
}

/// Anchor `p`, a path the host was handed on the command line, at `cwd`
/// unless it is already absolute: see [`path::Resolver::anchor`].
pub(super) fn absolute_in(cwd: &path::CwdPath, p: &Path) -> PathBuf {
    path::Resolver::at(&ral_core::host::EnvVars::new(), cwd)
        .anchor(p)
        .into_inner()
}
