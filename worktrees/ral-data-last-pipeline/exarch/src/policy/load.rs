//! Capability profile file loading and path utilities.

use ral_core::types::RawCapabilities;
use std::path::{Path, PathBuf};

/// Read a capabilities profile from `path` as a [`RawCapabilities`].
///
/// `flag` is the CLI flag the path arrived through, used in error messages.
/// Missing files are an error: composition is explicit, so a path the user
/// typed must resolve.
///
/// The TOML is parsed but **not** frozen here.  The orchestrator
/// composes (`meet` / `join`) raw policies and freezes once at
/// the end, so a single freeze pass resolves every sigil exactly
/// once after the lattice operations have settled.  See
/// [`RawCapabilities::freeze`] for the resolution rules.
pub(super) fn load_capabilities_toml(path: &Path, flag: &str) -> Result<RawCapabilities, String> {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("exarch: {flag} path does not exist: {}", path.display())
        }
        _ => format!("exarch: failed to read {}: {e}", path.display()),
    })?;
    toml::from_str::<RawCapabilities>(&text)
        .map_err(|e| format!("exarch: failed to parse {}: {e}", path.display()))
}

/// Resolve `p` relative to `cwd` if not already absolute.
pub(super) fn absolute_in(cwd: &str, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Canonicalise `p` leniently — falls back to the input when no
/// ancestor exists.  Thin string wrapper over
/// [`ral_core::path::canon::canonicalise_lenient`] for the
/// `runtime_fs_policy` cwd / `/tmp` / tempdir prefix builder.
pub(super) fn canon(p: &str) -> String {
    ral_core::path::canon::canonicalise_lenient(Path::new(p))
        .to_string_lossy()
        .into_owned()
}
