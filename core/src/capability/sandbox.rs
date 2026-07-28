//! The OS-renderable sandbox projection.
//!
//! [`sandbox_projection`] meet-folds the whole dynamic
//! [`GrantStack`](crate::types::GrantStack) into the
//! [`SandboxProjection`] the sandbox backends render.  The point-of-use
//! gates over that same authority live in the sibling
//! [`super::enforce`].

use super::exec::{ExecNames, ExecVerdict, evaluate_exec};
use crate::path::{NormalizedPrefix, PrefixSet, Resolver};
use crate::types::{
    ExecPolicy, ExecProjection, FsPolicy, FsProjection, GrantStack, Meet, SandboxProjection,
};
use std::collections::BTreeSet;

/// Meet-fold the stack's fs, net, and exec dimensions into the
/// OS-renderable projection, with literal exec keys resolved under
/// `path_env`, the shell's `PATH` override.  `None` when no layer
/// restricts fs or net — nor, on macOS, exec — so the caller can skip
/// OS sandbox setup entirely.
pub(crate) fn sandbox_projection(
    grants: &GrantStack,
    resolver: &Resolver,
    path_env: &str,
) -> Option<SandboxProjection> {
    let mut read_prefixes: Option<PrefixSet> = None;
    let mut write_prefixes: Option<PrefixSet> = None;
    let mut deny_paths: Vec<NormalizedPrefix> = Vec::new();
    let mut net_allowed = true;
    let mut saw_fs = false;
    let mut saw_net = false;

    for fs in grants.fs() {
        saw_fs = true;
        read_prefixes = read_prefixes.meet(Some(PrefixSet::resolve(resolver, &fs.read_prefixes)));
        write_prefixes =
            write_prefixes.meet(Some(PrefixSet::resolve(resolver, &fs.write_prefixes)));
        for p in &fs.deny_paths {
            // Both spellings, or a backend that masks the literal path
            // (bwrap's tmpfs overlay) misses a symlinked deny target.
            deny_paths.push(p.clone());
            deny_paths.push(NormalizedPrefix::from_surface(resolver.check(p.as_str())));
        }
    }
    for net in grants.net() {
        saw_net = true;
        net_allowed &= net;
    }

    let exec = reduce_exec(grants, resolver, path_env);
    // Attenuated exec is worth an OS sandbox only where the backend can
    // filter exec: Seatbelt renders a path rule, bwrap has none.  The
    // in-ral exec gate runs on every platform regardless.
    #[cfg(target_os = "macos")]
    let exec_triggers_sandbox = !matches!(exec, ExecProjection::Unrestricted);
    #[cfg(not(target_os = "macos"))]
    let exec_triggers_sandbox = false;

    if !saw_fs && (!saw_net || net_allowed) && !exec_triggers_sandbox {
        return None;
    }

    let fs = if saw_fs {
        FsProjection::Restricted(FsPolicy {
            read_prefixes: read_prefixes.unwrap_or_default().surface(),
            write_prefixes: write_prefixes.unwrap_or_default().surface(),
            deny_paths: deny_paths
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        })
    } else {
        FsProjection::Unrestricted
    };
    Some(SandboxProjection {
        fs,
        net: net_allowed,
        exec,
    })
}

/// Reduce the exec component of the stack into an [`ExecProjection`],
/// whose doc gives the OS rendering of each set.  Allows meet across
/// layers, denies union — a `Deny` is sticky — and a bare-name `Deny`
/// lands in `deny_basenames`, vetoing the command wherever it resolves
/// and closing the interpreter-bypass route (`sh -c git`).
fn reduce_exec(grants: &GrantStack, resolver: &Resolver, path_env: &str) -> ExecProjection {
    let mut subpath_allow: Option<PrefixSet> = None;
    let mut subpath_deny = PrefixSet::default();
    let mut literal_names: BTreeSet<String> = BTreeSet::new();
    let mut denied_names: BTreeSet<String> = BTreeSet::new();
    let mut saw = false;
    for map in grants.exec() {
        saw = true;
        let allow_dirs: Vec<&NormalizedPrefix> = map.allow_dirs.iter().collect();
        let deny_dirs: Vec<&NormalizedPrefix> = map.deny_dirs.iter().collect();
        subpath_allow = subpath_allow.meet(Some(PrefixSet::resolve(resolver, &allow_dirs)));
        subpath_deny = subpath_deny.union(PrefixSet::resolve(resolver, &deny_dirs));
        for (name, policy) in &map.literals {
            literal_names.insert(name.clone());
            if matches!(policy, ExecPolicy::Deny) {
                denied_names.insert(name.clone());
            }
        }
    }
    if !saw {
        return ExecProjection::Unrestricted;
    }
    let surface_strings = |set: PrefixSet| -> Vec<String> {
        set.surface()
            .into_iter()
            .map(NormalizedPrefix::into_string)
            .collect()
    };
    let mut deny_paths = Vec::new();
    let mut deny_basenames = Vec::new();
    for name in &denied_names {
        if crate::path::is_absolute(name) {
            deny_paths.push(name.clone());
        } else {
            deny_basenames.push(name.clone());
        }
    }
    ExecProjection::Restricted {
        allow_paths: admitted_literal_paths(grants, &literal_names, path_env),
        allow_dirs: surface_strings(subpath_allow.unwrap_or_default()),
        deny_paths,
        deny_dirs: surface_strings(subpath_deny),
        deny_basenames,
    }
}

/// Resolve one literal exec key to the absolute path the OS gate names.
/// Bare names walk the grant's `PATH` override alone — no host fallback,
/// so an unresolvable name fails closed (reduced-authority-witness B6).
fn resolve_literal(name: &str, path_env: &str) -> Option<String> {
    if crate::path::is_absolute(name) {
        Some(name.to_string())
    } else {
        crate::path::resolve_in_path(name, path_env)
    }
}

/// Keep the literal exec keys whose resolved path the live stack verdict
/// admits, so the OS profile admits exactly what the in-ral gate does.
///
/// A literal named in one layer can be covered only by a *sibling*
/// layer's allow-dir — `git: Allow` here, `/usr/bin/` there — and the
/// two meet only once the name is resolved, hence [`evaluate_exec`] over
/// the resolved identity rather than an intersection of names and dirs.
/// The [`ExecNames`] query is the one a real invocation makes, deny
/// broadened to the basename, so a sibling `git: Deny` still vetoes.
fn admitted_literal_paths(
    grants: &GrantStack,
    names: &BTreeSet<String>,
    path_env: &str,
) -> Vec<String> {
    let mut allowed = BTreeSet::new();
    for name in names {
        let Some(resolved) = resolve_literal(name, path_env) else {
            crate::dbg_trace!(
                "sandbox-exec",
                "exec '{}' not on PATH at projection time; OS gate cannot pin it",
                name
            );
            continue;
        };
        let allow: Vec<&str> = [name.as_str(), resolved.as_str()]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let deny: Vec<&str> = [
            name.as_str(),
            resolved.as_str(),
            crate::path::basename(&resolved),
        ]
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
        if let ExecVerdict::Allowed(_) = evaluate_exec(
            grants,
            ExecNames {
                deny: &deny,
                allow: &allow,
            },
        ) {
            allowed.insert(resolved);
        }
    }
    allowed.into_iter().collect()
}
