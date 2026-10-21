//! Capability composition for exarch.
//!
//! ```text
//!   ceiling = base ◁ extend?
//!   stack   = [ceiling, restrict₁, restrict₂, ..., deny(restricts)?, deny(credentials)?, self?]
//! ```
//!
//! One optional extension shapes the ceiling, in its own layer; every other
//! composition is the [`GrantStack`] itself — each `--restrict` file, and
//! each deny carve-out, is pushed as its own layer rather than folded ahead
//! of time, so the stack's own per-check fold is the one meet that ever
//! runs.  Composition is explicit — nothing is auto-loaded.

mod base;
mod load;

use base::resolve_base;
use load::{absolute_in, load_capabilities_ral, load_fragment_ral};
use ral_core::capability::{
    Capabilities, Evidence, ExecGrant, ExecKey, FsPolicy, GrantLayer, GrantStack,
};
use ral_core::guard::freeze::{FreezeCtx, host_absolute};
use ral_core::path::FrozenPath;
use ral_core::terminal::TerminalState;
use ral_core::types::EnvVars;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Compose a session's effective [`GrantStack`], with the restrict files'
/// absolute paths.
///
/// `base_name` selects a bake-in profile from `base`.  Every profile freezes
/// against the session's `HOME` and working directory as it loads, so the
/// extension and every layer push run on already-resolved bundles.  Wherever
/// some layer's `fs` restricts, each restrict file's own path joins a deny
/// layer, putting the bytes that shape the agent's permissions beyond its
/// reach; the extend file does not, since extending authority is a
/// trust-source concern rather than a containment one.  On the same
/// footing the stack denies
/// [`provider::credential_files`](crate::provider::credential_files) in
/// its own layer.  Last, wherever some layer's exec restricts, a layer
/// excludes exarch's own binary from exec, so the agent cannot start exarch.
/// A session whose fs and exec are unrestricted gets no exclusion, so
/// `dangerous` is ambient authority and needs no OS sandbox.
///
/// # Errors
/// Unknown `base_name`, a profile that fails to load, or an exarch binary
/// that cannot be named.
pub fn for_invocation(
    cwd: &Path,
    base_name: &str,
    extension: Option<&Path>,
    restrict_files: &[PathBuf],
) -> Result<Invocation, String> {
    // The loader evaluates ral source, so it needs a Shell.  This one is
    // scaffolding: the caller builds the session's real shell separately, from
    // the frozen GrantStack returned here.
    let anchor = ral_core::path::CwdPath::new(cwd.to_path_buf()).map_err(|e| e.to_string())?;
    let mut load_shell = ral_core::HostSurface::default()
        .shell(TerminalState::default(), anchor.clone())
        .map_err(|e| format!("the capability loader cannot start: {e}"))?;

    let ctx = FreezeCtx::at(EnvVars::new(), anchor.clone());

    let mut ceiling: GrantLayer = resolve_base(base_name, &ctx)?;
    if let Some(path) = extension {
        let abs = absolute_in(&anchor, path);
        ceiling = ceiling.extend(load_fragment_ral(
            &ral_core::types::Mooring::adrift(),
            &mut load_shell,
            &abs,
            "--extend",
            &ctx,
        )?);
    }

    let mut stack = GrantStack::root();
    stack.push(ceiling);

    let restricts: Vec<PathBuf> = restrict_files
        .iter()
        .map(|p| absolute_in(&anchor, p))
        .collect();
    for path in &restricts {
        stack.push(load_capabilities_ral(
            &ral_core::types::Mooring::adrift(),
            &mut load_shell,
            path,
            "--restrict",
            &ctx,
        )?);
    }

    // Our own credentials and the restrict files are the authority a grant is
    // *made of*, never something it hands out, so they are carved out in their
    // own layer, where no profile can forget them and no `--extend` can widen
    // them back.  Only where some layer's fs restricts: an unrestricted
    // session asked not to be confined, and a layer holding these exclusions
    // would confine its fs (on Windows, to "every path less some", which
    // refuses every launch).  An fs-silent restrict file is thus editable by
    // the agent, as an `--extend` file is.
    if !stack.fs_is_top() {
        if !restricts.is_empty() {
            stack.push(deny_layer(&restricts)?);
        }
        stack.push(deny_layer(&crate::provider::credential_files())?);
    }

    // On the same footing: exec unrestricted asked not to be confined, and an
    // exec exclusion would confine every launch, which on Linux needs bwrap.
    // A child a `dangerous` parent narrows thus runs exarch, regaining only the
    // parent's own authority.
    if !stack.exec_is_top() {
        stack.push(self_layer()?);
    }

    Ok(Invocation { stack, restricts })
}

/// A session's composed authority, with the pieces its callers need by name.
#[derive(Debug)]
pub struct Invocation {
    /// The whole meet, the self layer, if any, last.
    pub stack: GrantStack,
    /// Absolute paths of the `--restrict` files.
    pub restricts: Vec<PathBuf>,
}

/// The bake-in bases a spawn's `grant` may name — a subset of what
/// [`base::resolve_base`] offers a launching human, kept in step by hand.
/// `` `inherit `` and `` `restrict `` name no base, so neither appears here.
///
/// Each admits the bundled coreutils (`ral_core::uutils`). Those spawn by bare
/// name, so a base that states its exec admissions as directory prefixes alone
/// denies every one of them, and a child that cannot run `ls` cannot widen its
/// own grant to get it back.
pub(crate) const SPAWN_BASES: [&str; 4] = ["confined", "read-only", "edit-only", "reasonable"];

/// Resolve a bake-in base for a spawned child — the layer a caller pushes onto
/// the parent's stack, and the narrower every spawn grant naming a base
/// resolves through.
///
/// Frozen against the child's working directory. The stack is the meet, so a
/// spawn narrowing a child only ever adds a layer; naming a base looser than
/// the parent changes nothing once folded.
///
/// # Errors
/// Unknown `base_name`.
pub fn base_layer(base_name: &str, cwd: &Path) -> Result<GrantLayer, String> {
    let anchor = ral_core::path::CwdPath::new(cwd.to_path_buf()).map_err(|e| e.to_string())?;
    resolve_base(base_name, &FreezeCtx::at(EnvVars::new(), anchor))
}

/// A deny layer carving `paths` out of an otherwise-unrestricted fs policy:
/// `t ⊕ (f if X)`, written as everywhere less `paths`.  A constructor, not a
/// mutator, since composition is the stack: each caller pushes the layer this
/// returns.
///
/// Each path's walk holds the object it names, which the in-process check and
/// the OS sandbox profiles each judge, expanded to the firmlink variants.
/// A path [`host_absolute`] refuses fails the layer whole: a deny that named
/// nothing would fail open.
fn deny_layer(paths: &[PathBuf]) -> Result<GrantLayer, String> {
    let deny_paths: Vec<_> = paths
        .iter()
        .map(|p| host_absolute(p))
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|e| e.message)?
        .into_iter()
        .collect();
    let everywhere = || vec![FrozenPath::root()];
    let fs = (!deny_paths.is_empty()).then(|| FsPolicy {
        read_prefixes: everywhere(),
        write_prefixes: everywhere(),
        deny_paths,
    });
    GrantLayer::from_capabilities(
        Capabilities {
            fs,
            ..Capabilities::default()
        },
        &EnvVars::new(),
    )
    .map_err(|e| e.message)
}

/// The layer that keeps the agent from starting exarch: an exec exclusion of
/// the path exarch's own binary was pinned from at boot, the object its walk
/// held, under every program less it.  Exec only: an fs exclusion would
/// refuse every bundled tool.  Unpinned, it names nothing rather than guess.
fn self_layer() -> Result<GrantLayer, String> {
    let exe = ral_core::sandbox::pinned_arg0().ok_or(
        "exarch could not pin its own binary at startup (is it missing or unreadable?), \
         so cannot exclude it from exec",
    )?;
    let exarch = ExecKey::Path(FrozenPath::freeze(exe)?);
    GrantLayer::from_capabilities(
        Capabilities {
            exec: Some(ExecGrant::from_iter([(exarch, Evidence::DENY)]).with_every()),
            ..Capabilities::default()
        },
        &EnvVars::new(),
    )
    .map_err(|e| e.message)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use ral_core::capability::Net;

    #[test]
    fn an_fs_restricting_restrict_file_is_denied_under_dangerous_base() {
        let path = std::env::temp_dir().join(format!(
            "exarch-restrict-test-{}-{}.ral",
            std::process::id(),
            "dangerous",
        ));
        std::fs::write(&path, "return [fs: [write: [`cwd .]]]\n").unwrap();

        let cwd = std::env::temp_dir().join(format!("exarch-restrict-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let Invocation { stack, .. } =
            for_invocation(&cwd, "dangerous", None, std::slice::from_ref(&path)).unwrap();

        let runtime = ral_core::test_helper::core_runtime();
        assert!(
            !stack.admits_fs(
                runtime.floor(),
                &ral_core::capability::FsOp::Write,
                &ral_core::test_helper::trace(&path)
            ),
            "restrict file path should be write-denied"
        );

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(cwd);
    }

    /// `dangerous` restricts no axis, so no exclusion is pushed and no launch
    /// needs an OS sandbox: a host without bwrap still runs commands.
    #[test]
    fn a_dangerous_session_restricts_nothing() {
        let Invocation { stack, .. } =
            for_invocation(&std::env::temp_dir(), "dangerous", None, &[]).unwrap();
        assert!(!stack.is_restrictive(), "{stack:?}");
    }

    /// The stack ANDs the verdicts, so a `confined` layer (network off) under
    /// the looser `minimal` layer (network on) stays offline whichever order
    /// they were pushed.
    #[test]
    fn narrow_cannot_escalate_a_restricted_parent() {
        let cwd = std::env::temp_dir();
        let confined = base_layer("confined", &cwd).unwrap();
        assert_eq!(
            confined.summary().and_then(|s| s.net),
            Some(Net::Offline),
            "confined layer has net off"
        );
        let minimal = base_layer("minimal", &cwd).unwrap();

        let mut stack = GrantStack::root();
        stack.push(confined);
        stack.push(minimal);
        assert!(
            stack.net().next().is_some(),
            "naming a looser base must not turn the network back on"
        );
    }
}
