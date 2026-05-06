//! Capability composition for exarch.
//!
//! ```text
//!   ceiling   = base ∨ extend_base?
//!   effective = ceiling ⊓ restrict₁ ⊓ restrict₂ ⊓ ...
//! ```
//!
//! Two phases over the same lattice: a single optional join widens the
//! ceiling, then any number of meets attenuate from it.  Both phases are
//! commutative within themselves.  Composition is explicit only — nothing
//! is auto-loaded.
//!
//! ## Sub-modules
//!
//! - `base`  — built-in TOML bake-ins and the dynamic fs prefixes rule.
//! - `load`  — `load_capabilities_toml`, path utilities.

mod base;
mod load;

use base::{resolve_base, root_fs_policy};
use load::{absolute_in, load_capabilities_toml};
use ral_core::path::sigil::FreezeCtx;
use ral_core::types::{Capabilities, RawCapabilities};
use std::path::{Path, PathBuf};

/// Compute the effective `Capabilities` for a session.
///
/// `base_name` selects the ceiling — one of `dangerous`,
/// `reasonable`, `read-only`, `minimal`, or `confined` — see
/// [`base::resolve_base`] for the per-profile shape.
/// `extend_base`, if `Some`, is loaded and joined
/// into the ceiling.  Each entry in `restrict_files` is loaded
/// and meet'd in.  All composition runs on [`RawCapabilities`];
/// a single freeze pass at the end resolves every `~` / `xdg:`
/// sigil and produces the runtime [`Capabilities`].
///
/// Every restrict file's absolute path (lexical *and* canonical)
/// is added to `fs.deny_paths`, making the input bytes
/// structurally unreachable to the agent.  The extend-base file
/// is *not* added to deny_paths: it widens authority, so denying
/// writes to it is a trust-source concern.
pub fn for_invocation(
    cwd: &str,
    base_name: &str,
    extend_base: Option<&Path>,
    restrict_files: &[PathBuf],
) -> Result<(Capabilities, Vec<PathBuf>), String> {
    let mut raw: RawCapabilities = resolve_base(base_name)?;

    if let Some(path) = extend_base {
        let abs = absolute_in(cwd, path);
        raw = raw.join(load_capabilities_toml(&abs, "--extend-base")?);
    }

    let restricts: Vec<PathBuf> = restrict_files.iter().map(|p| absolute_in(cwd, p)).collect();
    for path in &restricts {
        raw = raw.meet(load_capabilities_toml(path, "--restrict")?);
    }

    if !restricts.is_empty() {
        let fs = raw.fs.get_or_insert_with(root_fs_policy);
        for p in &restricts {
            let s = p.to_string_lossy().into_owned();
            fs.deny_paths.push(s.clone());
            fs.deny_paths.push(load::canon(&s));
        }
        fs.deny_paths.sort();
        fs.deny_paths.dedup();
    }

    let home = ral_core::path::home::home_from_env();
    let cwd_path = PathBuf::from(cwd);
    let ctx = FreezeCtx { home: &home, cwd: &cwd_path };
    let caps = raw
        .freeze(&ctx)
        .map_err(|e| format!("exarch: invalid grant after composition: {e}"))?;
    Ok((caps, restricts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restrict_files_are_denied_even_under_dangerous_base() {
        let path = std::env::temp_dir().join(format!(
            "exarch-restrict-test-{}-{}.toml",
            std::process::id(),
            "dangerous",
        ));
        std::fs::write(&path, "[exec]\nls = \"Allow\"\n").unwrap();

        let (caps, _) =
            for_invocation("/", "dangerous", None, std::slice::from_ref(&path)).unwrap();
        let fs = caps.fs.expect("restrict file should install fs carve-out");
        assert_eq!(fs.read_prefixes, vec!["/"]);
        assert_eq!(fs.write_prefixes, vec!["/"]);
        assert!(
            fs.deny_paths.iter().any(|p| p == &path.to_string_lossy()),
            "restrict file path should be write-denied"
        );

        let _ = std::fs::remove_file(path);
    }
}
