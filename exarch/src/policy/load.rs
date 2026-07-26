//! Capability profile file loading and path utilities.
//!
//! The actual loader (`load_capabilities_from_path`) lives in
//! `ral_core::capability::load`; this module wraps that surface with the
//! exarch-specific orchestrator's error format and contributes the
//! `absolute_in` path helper that `for_invocation` uses to make
//! user-supplied relative paths absolute against the session cwd.

use ral_core::types::{Break, Capabilities, Escape, Mooring, Shell};
use std::path::{Path, PathBuf};

use ral_core::path;

/// Read a capabilities profile from `path` as a frozen [`Capabilities`].
///
/// `flag` is the CLI flag the path arrived through, used in error messages.
/// Missing files are an error: composition is explicit, so a path the user
/// typed must resolve.
///
/// The script's sigils are resolved against `ctx` at load, so the
/// orchestrator composes (`meet` / `join`) already-resolved policies.  An
/// `xdg:` path escaping `$HOME` is rejected here, at the profile that names
/// it, before composition can discard it; that rejection surfaces as a
/// [`Break::Error`], not an [`Break::Escape`].
pub(super) fn load_capabilities_ral(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    flag: &str,
    ctx: &ral_core::path::sigil::FreezeCtx<'_>,
) -> Result<Capabilities, String> {
    if !path.exists() {
        return Err(format!(
            "exarch: {flag} path does not exist: {}",
            path.display()
        ));
    }
    ral_core::capability::load_capabilities_from_path(mooring, shell, path, ctx).map_err(|e| {
        let detail = match e {
            Break::Error(err) => err.message,
            Break::Escape(Escape::Exit(code)) => format!("exit {code}"),
            #[cfg(unix)]
            Break::Escape(Escape::Stopped { signal, cmd, .. }) => {
                format!("{cmd}: stopped by signal {}", signal.display())
            }
        };
        format!("exarch: {flag} {}: {detail}", path.display())
    })
}

/// Resolve `p` relative to `cwd` if not already absolute.  Delegates
/// to [`path::resolve_str`] so the join (and `.`/`..` normalisation)
/// rule lives in the canonical path module, not duplicated here.
pub(super) fn absolute_in(cwd: &str, p: &Path) -> PathBuf {
    path::resolve_str(Some(cwd), &p.to_string_lossy())
}
