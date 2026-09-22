//! The loading half of exarch's capability composition: `ral_core`'s profile
//! loader dressed in exarch's error format, and the cwd-relative path helper
//! `super::for_invocation` calls around it.

use ral_core::types::{Break, Capabilities, Escape, Mooring, Shell};
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
    ctx: &ral_core::path::sigil::FreezeCtx<'_>,
) -> Result<Capabilities, String> {
    if !path.exists() {
        return Err(format!("{flag} path does not exist: {}", path.display()));
    }
    ral_core::capability::load_capabilities_from_path(mooring, shell, path, ctx).map_err(|e| {
        let detail = match e {
            Break::Error(err) => err.message,
            Break::Escape(Escape::Exit(code)) => format!("exit {code}"),
        };
        format!("{flag} {}: {detail}", path.display())
    })
}

/// Resolve `p` against `cwd` unless it is already absolute.
pub(super) fn absolute_in(cwd: &str, p: &Path) -> PathBuf {
    path::resolve_str(Some(cwd), &p.to_string_lossy())
}
