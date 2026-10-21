//! The loading half of exarch's capability composition: `ral_core`'s profile
//! loader dressed in exarch's error format, and the cwd-relative path helper
//! `super::for_invocation` calls around it.

use ral_core::capability::{Capabilities, Fragment};
use ral_core::guard::freeze::FreezeCtx;
use ral_core::types::{Break, Escape, Mooring, Shell};
use std::path::{Path, PathBuf};

use ral_core::path;

/// Read a capabilities profile from `path` as a frozen [`Capabilities`].
///
/// Sigils freeze against `ctx` here, so a bad `xdg:` fails at the profile that
/// names it even where a later `meet` would have discarded the grant, and an
/// `exit` inside the profile flattens into the error string: a profile is
/// configuration, not control flow.
pub(super) fn load_capabilities_ral(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    flag: &str,
    ctx: &FreezeCtx,
) -> Result<Capabilities, String> {
    load(path, flag, || {
        ral_core::load::profile::load_capabilities_from_path(mooring, shell, path, ctx)
    })
}

/// [`load_capabilities_ral`] for an extension file, which only
/// [`Capabilities::extend`] may consume.
pub(super) fn load_fragment_ral(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    flag: &str,
    ctx: &FreezeCtx,
) -> Result<Fragment, String> {
    load(path, flag, || {
        ral_core::load::profile::load_fragment_from_path(mooring, shell, path, ctx)
    })
}

fn load<T>(path: &Path, flag: &str, read: impl FnOnce() -> Result<T, Break>) -> Result<T, String> {
    if !path.exists() {
        return Err(format!(
            "{flag} path does not exist: {}",
            ral_core::path::shown(path)
        ));
    }
    read().map_err(|e| {
        let detail = match e {
            Break::Error(err) => err.message,
            Break::Escape(Escape::Exit(code)) => format!("exit {code}"),
            Break::Escape(Escape::Stopped { cause, .. }) => {
                format!("stopped: {}", cause.message())
            }
        };
        format!("{flag} {}: {detail}", ral_core::path::shown(path))
    })
}

/// Anchor `p`, a path the host was handed on the command line, at `cwd`
/// unless it is already absolute: see [`path::Resolver::anchor`].
pub(super) fn absolute_in(cwd: &path::CwdPath, p: &Path) -> PathBuf {
    path::Resolver::at(&ral_core::types::EnvVars::new(), cwd)
        .anchor(p)
        .into_inner()
}
