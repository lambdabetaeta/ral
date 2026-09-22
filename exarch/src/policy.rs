//! Capability composition for exarch.
//!
//! ```text
//!   ceiling = base ∨ extend_base?
//!   stack   = [ceiling, restrict₁, restrict₂, ..., deny(restricts)?, deny(credentials)?]
//! ```
//!
//! One optional join widens the ceiling into its own layer; every other
//! composition is the [`GrantStack`] itself — each `--restrict` file, and
//! each deny carve-out, is pushed as its own layer rather than folded ahead
//! of time, so the stack's own per-check fold is the one meet that ever
//! runs.  Composition is explicit — nothing is auto-loaded.

mod base;
mod load;

use base::{resolve_base, root_fs_policy};
use load::{absolute_in, load_capabilities_ral};
use ral_core::host;
use ral_core::io::TerminalState;
use ral_core::path::{sigil::FreezeCtx, sigil::freeze_path_list};
use ral_core::types::{Capabilities, GrantStack, Shell};
use std::path::{Path, PathBuf};

/// Compose a session's effective [`GrantStack`], with the restrict files'
/// absolute paths.
///
/// `base_name` selects a bake-in profile from `base`.  Every profile freezes
/// against the session's `$HOME` and working directory as it loads, so the
/// join and every layer push run on already-resolved bundles.  Each restrict
/// file's own path joins a deny layer, putting the bytes that shape the
/// agent's permissions beyond its reach; the extend-base file does not,
/// since widening authority is a trust-source concern rather than a
/// containment one.  On the same footing, and for the same reason, the
/// stack denies [`provider::credential_files`](crate::provider::credential_files)
/// in its own layer wherever some layer holds an `fs` opinion.
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
) -> Result<(GrantStack, Vec<PathBuf>), String> {
    // The loader evaluates ral source, so it needs a Shell.  This one is
    // scaffolding: the caller builds the session's real shell separately, from
    // the frozen GrantStack returned here.
    let mut load_shell = Shell::new(TerminalState::default());

    let cwd_path = PathBuf::from(cwd);
    let home = host::home();
    let ctx = FreezeCtx {
        home: home.as_deref(),
        cwd: &cwd_path,
    };

    let mut ceiling: Capabilities = resolve_base(base_name, &ctx)?;
    if let Some(path) = extend_base {
        let abs = absolute_in(cwd, path);
        ceiling = ceiling.join(load_capabilities_ral(
            &ral_core::types::Mooring::adrift(),
            &mut load_shell,
            &abs,
            "--extend-base",
            &ctx,
        )?);
    }

    let mut stack = GrantStack::root();
    stack.push(ceiling);

    let restricts: Vec<PathBuf> = restrict_files.iter().map(|p| absolute_in(cwd, p)).collect();
    for path in &restricts {
        stack.push(load_capabilities_ral(
            &ral_core::types::Mooring::adrift(),
            &mut load_shell,
            path,
            "--restrict",
            &ctx,
        )?);
    }

    if !restricts.is_empty() {
        stack.push(deny_layer(&restricts, &ctx)?);
    }

    // Our own credentials are the authority a grant is *made of*, never
    // something it hands out — so they are carved out in their own layer,
    // where no profile can forget them and no `--extend-base` can widen them
    // back.  Only where some layer holds an fs opinion: `dangerous`
    // attenuates nothing by contract, and installing one there to hold these
    // denies would silently confine every session that asked not to be,
    // against an agent that can read the same bytes a hundred other ways.
    if stack.iter().any(|c| c.fs.is_some()) {
        stack.push(deny_layer(&crate::provider::credential_files(), &ctx)?);
    }

    Ok((stack, restricts))
}

/// Resolve a bake-in base for a spawned child — the layer a caller pushes onto the parent's stack.
///
/// Frozen against the child's working directory. The stack is the meet, so a
/// spawn narrowing a child only ever adds a layer; naming a base looser than
/// the parent changes nothing once folded.
///
/// # Errors
/// Unknown `base_name`.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: the child's base freezes against the launching user's real home, like for_invocation's"
)]
pub fn base_layer(base_name: &str, cwd: &str) -> Result<Capabilities, String> {
    let cwd_path = PathBuf::from(cwd);
    let home = host::home();
    let ctx = FreezeCtx {
        home: home.as_deref(),
        cwd: &cwd_path,
    };
    resolve_base(base_name, &ctx)
}

/// A deny layer carving `paths` out of a fresh, otherwise-unrestricted fs
/// policy — a constructor, not a mutator, since composition is now the
/// stack: each caller pushes the layer this returns rather than folding it
/// into one already on the stack.
///
/// Only the lexical form is pushed: the in-process check in
/// `core/src/capability/enforce.rs` and the OS sandbox profiles each expand a
/// deny entry to its canonical and macOS-firmlink variants themselves.
fn deny_layer(paths: &[PathBuf], ctx: &FreezeCtx<'_>) -> Result<Capabilities, String> {
    let mut frozen = freeze_path_list(
        paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        ctx,
    )
    .map_err(|e| e.message)?;
    frozen.sort();
    frozen.dedup();
    let mut fs = root_fs_policy();
    fs.deny_paths = frozen;
    Ok(Capabilities {
        fs: Some(fs),
        ..Capabilities::default()
    })
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
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
        let cwd = if cfg!(windows) { r"C:\" } else { "/" };
        let (stack, _) =
            for_invocation(cwd, "dangerous", None, std::slice::from_ref(&path)).unwrap();

        let resolver = ral_core::path::Resolver::shell_less();
        let rp = resolver.resolve(&path.to_string_lossy());
        assert!(
            !stack.admits_fs(&ral_core::capability::FsOp::Write, &resolver, &rp),
            "restrict file path should be write-denied"
        );

        let _ = std::fs::remove_file(path);
    }

    /// The stack ANDs the verdicts, so a `confined` layer (network off) under
    /// the looser `minimal` layer (network on) stays offline whichever order
    /// they were pushed.
    #[test]
    fn narrow_cannot_escalate_a_restricted_parent() {
        let confined = base_layer("confined", "/work/proj").unwrap();
        assert_eq!(confined.net, Some(false), "confined layer has net off");
        let minimal = base_layer("minimal", "/work/proj").unwrap();

        let mut stack = GrantStack::root();
        stack.push(confined);
        stack.push(minimal);
        assert!(
            !stack.net().all(|n| n),
            "naming a looser base must not turn the network back on"
        );
    }
}
