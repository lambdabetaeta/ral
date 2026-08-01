//! Capability composition for exarch.
//!
//! ```text
//!   ceiling   = base ∨ extend_base?
//!   effective = ceiling ⊓ restrict₁ ⊓ restrict₂ ⊓ ...
//! ```
//!
//! One optional join widens the ceiling, then any number of meets attenuate
//! from it; each phase commutes within itself.  Composition is explicit —
//! nothing is auto-loaded.

mod base;
mod load;

use base::{resolve_base, root_fs_policy};
use load::{absolute_in, lint_deputy_prefixes, load_capabilities_ral};
use ral_core::host;
use ral_core::io::TerminalState;
use ral_core::path::{sigil::FreezeCtx, sigil::freeze_path_list};
use ral_core::types::{Capabilities, Shell};
use std::path::{Path, PathBuf};

/// Compose a session's effective [`Capabilities`], with the restrict files'
/// absolute paths.
///
/// `base_name` selects a bake-in profile from `base`.  Every profile freezes
/// against the session's `$HOME` and working directory as it loads, so join and
/// meet run on already-resolved bundles.  Each restrict file's own path joins
/// `fs.deny_paths`, putting the bytes that shape the agent's permissions beyond
/// its reach; the extend-base file does not, since widening authority is a
/// trust-source concern rather than a containment one.
///
/// # Errors
/// Unknown `base_name`, or a profile that fails to load.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: capability profiles freeze against the launching user's real home — no shell overlay exists yet"
)]
pub fn for_invocation(
    cwd: &str,
    base_name: &str,
    extend_base: Option<&Path>,
    restrict_files: &[PathBuf],
) -> Result<(Capabilities, Vec<PathBuf>), String> {
    // The loader evaluates ral source, so it needs a Shell.  This one is
    // scaffolding: the caller builds the session's real shell separately, from
    // the frozen Capabilities returned here.
    let mut load_shell = Shell::new(TerminalState::default());

    let cwd_path = PathBuf::from(cwd);
    let home = host::home();
    let ctx = FreezeCtx {
        home: &home,
        cwd: &cwd_path,
    };

    let mut caps: Capabilities = resolve_base(base_name, &ctx)?;

    if let Some(path) = extend_base {
        let abs = absolute_in(cwd, path);
        caps = caps.join(load_capabilities_ral(
            &ral_core::types::Mooring::adrift(),
            &mut load_shell,
            &abs,
            "--extend-base",
            &ctx,
        )?);
    }

    let restricts: Vec<PathBuf> = restrict_files.iter().map(|p| absolute_in(cwd, p)).collect();
    for path in &restricts {
        caps = caps.meet(load_capabilities_ral(
            &ral_core::types::Mooring::adrift(),
            &mut load_shell,
            path,
            "--restrict",
            &ctx,
        )?);
    }

    if !restricts.is_empty() {
        deny_paths(&mut caps, &restricts, &ctx)?;
    }

    lint_deputy_prefixes(&caps);

    Ok((caps, restricts))
}

/// Attenuate `parent` to a bake-in base for a spawned child.
///
/// `parent ⊓ base`, frozen against the child's working directory.  The meet is
/// ≤ both operands, so a spawn can only reduce a child's reach: naming a base
/// looser than the parent changes nothing.
///
/// # Errors
/// Unknown `base_name`.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: the child's base freezes against the launching user's real home, like for_invocation's"
)]
pub fn narrow(parent: &Capabilities, base_name: &str, cwd: &str) -> Result<Capabilities, String> {
    let cwd_path = PathBuf::from(cwd);
    let home = host::home();
    let ctx = FreezeCtx {
        home: &home,
        cwd: &cwd_path,
    };
    let base = resolve_base(base_name, &ctx)?;
    Ok(parent.clone().meet(base))
}

/// Add each path to `fs.deny_paths`, installing the root policy first when none
/// is set, so a deny always lands on a concrete policy rather than the implicit
/// ceiling.
///
/// Only the lexical form is pushed: the in-process check in
/// `core/src/capability/enforce.rs` and the OS sandbox profiles each expand a
/// deny entry to its canonical and macOS-firmlink variants themselves.
fn deny_paths(
    caps: &mut Capabilities,
    paths: &[PathBuf],
    ctx: &FreezeCtx<'_>,
) -> Result<(), String> {
    let frozen = freeze_path_list(
        paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        ctx,
    )
    .map_err(|e| e.message)?;
    let fs = caps.fs.get_or_insert_with(root_fs_policy);
    fs.deny_paths.extend(frozen);
    fs.deny_paths.sort();
    fs.deny_paths.dedup();
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    #[test]
    fn restrict_files_are_denied_even_under_dangerous_base() {
        let path = std::env::temp_dir().join(format!(
            "exarch-restrict-test-{}-{}.ral",
            std::process::id(),
            "dangerous",
        ));
        std::fs::write(&path, "return [exec: [ls: 'allow']]\n").unwrap();

        // The `/` ceiling freezes to the native root spelling: `\` on Windows,
        // still a universal prefix there since it folds to zero components.
        let (cwd, root) = if cfg!(windows) {
            (r"C:\", r"\")
        } else {
            ("/", "/")
        };
        let (caps, _) =
            for_invocation(cwd, "dangerous", None, std::slice::from_ref(&path)).unwrap();
        let fs = caps.fs.expect("restrict file should install fs carve-out");
        assert_eq!(fs.read_prefixes, vec![root]);
        assert_eq!(fs.write_prefixes, vec![root]);
        assert!(
            fs.deny_paths.iter().any(|p| *p == *path.to_string_lossy()),
            "restrict file path should be write-denied"
        );

        let _ = std::fs::remove_file(path);
    }

    /// Meet ANDs the verdicts, so a `confined` parent (network off) naming the
    /// looser `minimal` base (network on) stays offline.
    #[test]
    fn narrow_cannot_escalate_a_restricted_parent() {
        let parent = narrow(&Capabilities::root(), "confined", "/work/proj").unwrap();
        assert_eq!(parent.net, Some(false), "confined parent has net off");
        let child = narrow(&parent, "minimal", "/work/proj").unwrap();
        assert_eq!(
            child.net,
            Some(false),
            "naming a looser base must not turn the network back on"
        );
    }
}
