//! The OS-renderable sandbox projection.
//!
//! [`sandbox_projection`] meet-folds the whole dynamic
//! [`GrantStack`](crate::types::GrantStack) into the
//! [`SandboxProjection`] the sandbox backends render.  The in-process
//! guards over that same authority live in the sibling [`super::enforce`],
//! and consume the same per-dimension folds this module renders —
//! [`super::fs::allow_region`], and for exec the very table
//! [`super::enforce::check_exec`] judged with — so guard and profile cannot
//! disagree about what the stack permits.  All that separates them is when
//! the fold runs: here once, because the OS profile is written once at
//! spawn; there afresh on every check.

use super::enforce::Admitted;
use super::exec::{ExecRules, rules};
use super::fs::{FsOp, allow_region, deny_region};
use crate::path::{NormalizedPrefix, Polarity, PrefixSet, RealPath};
use crate::types::{Context, ExecProjection, FsProjection, FsRules, SandboxProjection};
use std::collections::BTreeSet;

/// Meet-fold the stack's fs, net, and exec dimensions into the
/// OS-renderable projection.  Exec is projected from the table that admitted
/// `admitted`, with the carriers of its program; a caller launching nothing
/// passes `None`, and the stack is compiled afresh.  `None` when no layer restricts fs or net — nor exec,
/// where the host's backend renders it ([`crate::sandbox::EXEC_ENFORCED`]) —
/// so the caller can skip OS sandbox setup entirely.
pub(crate) fn sandbox_projection(
    ctx: &Context,
    admitted: Option<&Admitted>,
) -> Option<SandboxProjection> {
    let grants = &ctx.grants;
    let resolver = ctx.resolver();
    // Traced because this fold is not the pure reduction it reads as: every
    // `PrefixSet::resolve` canonicalises against the filesystem, and compiling
    // the exec table walks the host `PATH` for every bare key, so its cost
    // tracks the host's fs latency and is paid again on each rebuild.
    #[cfg(debug_assertions)]
    let t_fold = std::time::Instant::now();
    // Computed once so read and write are each projected against the same
    // deny region: under deny-wins an allow beneath a deny is dead
    // authority, and no backend may ever be handed one to reorder.
    let deny = deny_region(grants, &resolver);
    // Zipped because the two allow regions are `Some` on the same condition —
    // some layer held an `fs` opinion — so there is no mixed case to weigh.
    let read = allow_region(grants, &resolver, &FsOp::Read).map(|r| r.outside(&deny));
    let write = allow_region(grants, &resolver, &FsOp::Write).map(|w| w.outside(&deny));
    let regions = read.zip(write);
    let mut net_allowed = true;
    let mut saw_net = false;
    for net in grants.net() {
        saw_net = true;
        net_allowed &= net;
    }

    let compiled = admitted.is_none().then(|| rules(grants)).flatten();
    let exec = admitted
        .map_or(compiled.as_ref(), Admitted::rules)
        .map_or(ExecProjection::Unrestricted, |rules| {
            ExecProjection::Restricted(rules.kernel(&carriers(rules, admitted)))
        });
    // Attenuated exec is worth an OS sandbox exactly where a backend carries
    // the rules into the kernel, which is the backends' fact to state and
    // `sandbox::EXEC_ENFORCED`'s to answer.  The in-process guard runs on
    // every platform regardless; the kernel layer is what sees the re-execs it
    // cannot (`sh -c`).
    let exec_triggers_sandbox =
        crate::sandbox::EXEC_ENFORCED && !matches!(exec, ExecProjection::Unrestricted);

    if regions.is_none() && (!saw_net || net_allowed) && !exec_triggers_sandbox {
        crate::dbg_trace!(
            "sandbox-proj",
            "fold unrestricted in {:?} (no OS sandbox needed)",
            t_fold.elapsed()
        );
        return None;
    }

    let fs = match regions {
        Some((read, write)) => FsProjection::Restricted(FsRules {
            read_prefixes: surface_strings(&read),
            write_prefixes: surface_strings(&write),
            deny_paths: surface_strings(&deny),
            pinned_dirs: Vec::new(),
        }),
        None => FsProjection::Unrestricted,
    };
    let projection = SandboxProjection {
        fs,
        net: net_allowed,
        exec,
    };
    crate::dbg_trace!("sandbox-proj", "fold restricted in {:?}", t_fold.elapsed());
    Some(projection)
}

/// What the kernel must admit beside `rules` for its admitted files, and the
/// launched program, to start.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn carriers(rules: &ExecRules, admitted: Option<&Admitted>) -> BTreeSet<RealPath> {
    let launched = admitted.and_then(|a| match a.program() {
        super::exec::Program::File { real, .. } => Some(real),
        super::exec::Program::Tool(_) => None,
    });
    crate::sandbox::carriers(rules.allowed_files().chain(launched))
}

/// No kernel exec layer here ([`crate::sandbox::EXEC_ENFORCED`]), so nothing
/// to carry.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn carriers(_: &ExecRules, _: Option<&Admitted>) -> BTreeSet<RealPath> {
    BTreeSet::new()
}

/// The fs projection is lexical: `resolved`/`namespace` have no reader below
/// this fold, so each prefix flattens to its surface spelling here, once, and
/// every backend widens that into its own name class at render time.
fn surface_strings<P: Polarity>(set: &PrefixSet<P>) -> Vec<String> {
    set.surface()
        .into_iter()
        .map(NormalizedPrefix::into_string)
        .collect()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use crate::path::{NormalizedPrefix, render_real};
    use crate::types::{
        Capabilities, ExecGrant, ExecProjection, ExecRule, FsPolicy, Shell, WriteReach,
    };

    /// A layer with an opinion on writes, on exec, or both.
    fn layer(write: Option<&str>, admit: Option<&str>) -> Capabilities {
        let prefix = NormalizedPrefix::from_surface;
        Capabilities {
            fs: write.map(|w| FsPolicy {
                read_prefixes: vec![prefix(w)],
                write_prefixes: vec![prefix(w)],
                deny_paths: Vec::new(),
            }),
            exec: admit.map(|dir| ExecGrant {
                dirs: [(prefix(dir), true)].into(),
                ..ExecGrant::default()
            }),
            ..Capabilities::root()
        }
    }

    /// The reach of the write region the stack folds to over the one dir it
    /// admits.
    fn folded_reach(base: Capabilities, inner: Capabilities) -> WriteReach {
        let projection = Shell::default().with_capabilities(base, |sh| {
            sh.with_capabilities(inner, |sh| sh.sandbox_projection().expect("restricted"))
        });
        let rendered = projection.rendered().expect("ASCII paths render");
        let fs = rendered.fs.rules().expect("a layer restricted fs");
        let ExecProjection::Restricted(rules) = &rendered.exec else {
            panic!("a layer restricted exec");
        };
        let dirs: Vec<_> = rules
            .iter()
            .filter_map(|rule| match rule {
                ExecRule::Dir { path, allow: true } => Some(render_real(path)),
                _ => None,
            })
            .collect::<Result<Vec<_>, _>>()
            .expect("ASCII paths render")
            .concat();
        fs.write_reach(
            dirs.first().expect("the admitted dir reaches the rules"),
            &dirs,
        )
    }

    /// The base layer's prefix survives the fold when no inner layer says
    /// anything about writes, so the admit an inner layer adds is covered by a
    /// grant it never wrote.
    #[test]
    fn a_shallower_base_layer_covers_what_an_inner_layer_admits() {
        let reach = folded_reach(
            layer(Some("/ral-test/w"), None),
            layer(None, Some("/ral-test/w/bin")),
        );
        assert_eq!(reach, WriteReach::Covered);
    }

    /// The control: an inner layer naming the admit as a write prefix too
    /// meets to the deeper prefix, which is the admit itself.
    #[test]
    fn an_inner_layer_that_names_the_admit_as_writable_trusts_it() {
        let reach = folded_reach(
            layer(Some("/ral-test/w"), None),
            layer(Some("/ral-test/w/bin"), Some("/ral-test/w/bin")),
        );
        assert_eq!(reach, WriteReach::Trusted);
    }
}
