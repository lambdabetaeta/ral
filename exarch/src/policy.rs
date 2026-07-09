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
//! - `base`  — built-in `.ral` bake-ins and the dynamic fs prefixes rule.
//! - `load`  — `load_capabilities_ral`, path utilities.

mod base;
mod load;

use base::{resolve_base, root_fs_policy};
use load::{absolute_in, load_capabilities_ral};
use ral_core::path::{home_from_env, sigil::FreezeCtx, sigil::freeze_path_list};
use ral_core::io::TerminalState;
use ral_core::types::{Capabilities, Shell};
use std::path::{Path, PathBuf};

/// Compute the effective `Capabilities` for a session.
///
/// `base_name` selects the ceiling — one of `dangerous`,
/// `reasonable`, `read-only`, `minimal`, or `confined` — see
/// [`base::resolve_base`] for the per-profile shape.
/// `extend_base`, if `Some`, is loaded and joined
/// into the ceiling.  Each entry in `restrict_files` is loaded
/// and meet'd in.  Every profile is frozen against the session's
/// `$HOME` and working directory as it loads, so composition runs
/// on already-resolved [`Capabilities`].
///
/// Every restrict file's absolute lexical path is added to
/// `fs.deny_paths`, making the input bytes structurally
/// unreachable to the agent.  The extend-base file
/// is *not* added to deny_paths: it widens authority, so denying
/// writes to it is a trust-source concern.
pub fn for_invocation(
    cwd: &str,
    base_name: &str,
    extend_base: Option<&Path>,
    restrict_files: &[PathBuf],
) -> Result<(Capabilities, Vec<PathBuf>), String> {
    // Loading goes through the ral evaluator, so we need a Shell.  This
    // shell exists only to back the loader's source-eval; its dynamic
    // state never reaches the runtime — for_invocation hands a frozen
    // Capabilities back to its caller, which builds the actual session
    // shell separately.
    let mut load_shell = Shell::new(TerminalState::default());

    let cwd_path = PathBuf::from(cwd);
    let home = home_from_env();
    let ctx = FreezeCtx {
        home: &home,
        cwd: &cwd_path,
    };

    let mut caps: Capabilities = resolve_base(base_name, &ctx)?;

    if let Some(path) = extend_base {
        let abs = absolute_in(cwd, path);
        caps = caps.join(load_capabilities_ral(
            &mut load_shell,
            &abs,
            "--extend-base",
            &ctx,
        )?);
    }

    let restricts: Vec<PathBuf> = restrict_files.iter().map(|p| absolute_in(cwd, p)).collect();
    for path in &restricts {
        caps = caps.meet(load_capabilities_ral(
            &mut load_shell,
            path,
            "--restrict",
            &ctx,
        )?);
    }

    if !restricts.is_empty() {
        deny_paths(&mut caps, &restricts, &ctx)?;
    }

    Ok((caps, restricts))
}

/// Attenuate `parent` to a named base for a spawned child.
///
/// A subagent's authority is its parent's met with the requested base —
/// `parent ⊓ base` — frozen against the child's working directory.  Meet
/// only ever removes authority and the result is ≤ both operands, so a
/// spawn can *reduce* a child's reach but never escalate it past the
/// parent's: naming a base looser than the parent simply changes nothing
/// (e.g. a network-off parent stays offline even under `minimal`).
/// `base_name` is one of the five bake-ins; an unknown name returns the
/// same diagnostic [`for_invocation`] gives.
pub fn narrow(parent: &Capabilities, base_name: &str, cwd: &str) -> Result<Capabilities, String> {
    let cwd_path = PathBuf::from(cwd);
    let home = home_from_env();
    let ctx = FreezeCtx {
        home: &home,
        cwd: &cwd_path,
    };
    let base = resolve_base(base_name, &ctx)?;
    Ok(parent.clone().meet(base))
}

/// Add each lexical path in `paths` to the session's `fs.deny_paths`,
/// making those input bytes structurally unreachable to the agent. Used
/// for `--restrict` files, whose bytes shape the agent's own permissions.
///
/// Only the user-supplied lexical form is pushed: both capability enforcers
/// expand a deny entry to its canonical (and, on macOS, firmlink) variants
/// themselves, so canonicalising here would duplicate — less completely —
/// work that belongs to core.
///
/// Installs the root fs policy if none is set, so a deny entry always lands
/// on a concrete policy rather than the implicit ceiling.  Each path is
/// frozen through the same lexer the grant decoder uses, so the deny
/// entries are [`NormalizedPrefix`](ral_core::path::NormalizedPrefix)es
/// in the grant-side normal form.
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
    )?;
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

        let (caps, _) =
            for_invocation("/", "dangerous", None, std::slice::from_ref(&path)).unwrap();
        let fs = caps.fs.expect("restrict file should install fs carve-out");
        assert_eq!(fs.read_prefixes, vec!["/"]);
        assert_eq!(fs.write_prefixes, vec!["/"]);
        assert!(
            fs.deny_paths.iter().any(|p| *p == *path.to_string_lossy()),
            "restrict file path should be write-denied"
        );

        let _ = std::fs::remove_file(path);
    }

    /// A spawn can narrow a child but never escalate it: a `confined`
    /// parent (network off) that names the looser `minimal` base (network
    /// on) keeps the network off, because meet ANDs the two verdicts.
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
