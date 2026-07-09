//! The OS-renderable sandbox projection.
//!
//! [`sandbox_projection`] meet-folds the whole dynamic
//! [`GrantStack`](crate::types::GrantStack) (`ctx.grants`) into the
//! [`SandboxProjection`] the sandbox backends render — the fs, net, and
//! exec dimensions intersected across every layer.  It returns `None`
//! when no layer imposes a restriction the OS must enforce, so callers
//! can cheaply skip sandbox setup.  The point-of-use gates that share
//! this authority model — and the audit trail the exec/fs ones carry —
//! live in the sibling [`super::enforce`].

use super::exec::{ExecNames, ExecVerdict, evaluate_exec};
use crate::path::{NormalizedPrefix, PrefixSet};
use crate::types::{
    Context, ExecDir, ExecPolicy, ExecProjection, FsPolicy, FsProjection, Meet, SandboxProjection,
};
use std::collections::BTreeSet;

/// Meet-fold the stack's fs, net, and exec dimensions into the
/// OS-renderable projection.  Returns `None` when no layer imposes fs
/// or net restrictions — nor, on macOS, an exec restriction Seatbelt
/// enforces — so callers can cheaply skip OS sandbox setup.
pub(crate) fn sandbox_projection(ctx: &Context) -> Option<SandboxProjection> {
    let mut read_prefixes: Option<PrefixSet> = None;
    let mut write_prefixes: Option<PrefixSet> = None;
    let mut deny_paths: Vec<NormalizedPrefix> = Vec::new();
    let mut net_allowed = true;
    let mut saw_fs = false;
    let mut saw_net = false;

    let resolver = ctx.resolver();
    for fs in ctx.grants.fs() {
        saw_fs = true;
        read_prefixes = read_prefixes.meet(Some(PrefixSet::resolve(&resolver, &fs.read_prefixes)));
        write_prefixes =
            write_prefixes.meet(Some(PrefixSet::resolve(&resolver, &fs.write_prefixes)));
        for p in &fs.deny_paths {
            deny_paths.push(p.clone());
            deny_paths.push(NormalizedPrefix::from_surface(resolver.check(p.as_str())));
        }
    }
    for net in ctx.grants.net() {
        saw_net = true;
        net_allowed &= net;
    }

    let exec = reduce_exec(ctx);
    // Exec attenuation only triggers an OS-layer sandbox where the
    // backend can actually filter exec — Seatbelt on macOS does it via
    // the rendered `(allow file-read* process-exec …)` rule; bwrap on
    // Linux has no path-exec filter, so paying the sandbox-subprocess
    // cost there buys nothing.  In-ral exec gating still runs on every
    // platform regardless.
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

/// Reduce the exec component of the stack.
///
/// `Unrestricted` means no layer attenuated exec; the OS profile
/// leaves `process-exec` open and the in-ral gate is the only check.
/// `Restricted` carries four sets:
///
///   * `allow_paths` — the literal exec keys named anywhere in the
///     stack, resolved to absolute paths and kept where the full-stack
///     live verdict admits them (see [`admitted_literal_paths`]).  The
///     OS profile renders them as `(literal …)`.
///   * `allow_dirs` — subpath keys carrying `Allow`, intersected by
///     prefix across opining layers.  Rendered as `(subpath …)`.
///   * `deny_paths` — absolute exec keys carrying `Deny`, unioned across
///     layers (a `Deny` is sticky).  Rendered as `(deny process-exec
///     (literal …))` after the broad allow so SBPL's last-match-wins
///     carves them out of a covering `allow_dirs` region.
///   * `deny_dirs` — subpath keys carrying `Deny`, *unioned* across
///     layers (denies are sticky).  Rendered as `(deny process-exec
///     (subpath …))` after the broad allow so SBPL's last-match-wins
///     gives them precedence.
///   * `deny_basenames` — bare-name exec keys carrying `Deny`, unioned
///     across layers.  A bare name vetoes a command wherever it lands,
///     so it renders as a final-path-component match, not a single
///     resolved path: this keeps the deny sound when the name lives
///     somewhere other than PATH resolves it, and closes the
///     interpreter-bypass route (`sh -c git`) by which a denied name
///     could otherwise slip in through an admitted dir.
///
/// `allow_paths` draws its candidates from the raw union of every
/// layer's literal keys and resolves each to a path before deciding: a
/// literal and a covering allow-dir interact only once the name is
/// resolved (`git` under a sibling layer's `/usr/bin/`), so the allow
/// decision defers to [`evaluate_exec`] (see [`admitted_literal_paths`])
/// over the resolved identity rather than intersecting names and dirs as
/// separate maps.
fn reduce_exec(ctx: &Context) -> ExecProjection {
    let resolver = ctx.resolver();
    let mut subpath_allow: Option<PrefixSet> = None;
    let mut subpath_deny = PrefixSet::default();
    let mut literal_names: BTreeSet<String> = BTreeSet::new();
    let mut denied_names: BTreeSet<String> = BTreeSet::new();
    let mut saw = false;
    for map in ctx.grants.exec() {
        saw = true;
        let mut allow_dirs = Vec::new();
        let mut deny_dirs = Vec::new();
        for (dir, verdict) in &map.dirs {
            match verdict {
                ExecDir::Allow => allow_dirs.push(dir.clone()),
                ExecDir::Deny => deny_dirs.push(dir.clone()),
            }
        }
        subpath_allow = subpath_allow.meet(Some(PrefixSet::resolve(&resolver, &allow_dirs)));
        subpath_deny = subpath_deny.union(PrefixSet::resolve(&resolver, &deny_dirs));
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
    let path_env = ctx
        .env_overrides()
        .get("PATH")
        .map_or("", String::as_str);
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
        allow_paths: admitted_literal_paths(ctx, &literal_names, path_env),
        allow_dirs: surface_strings(subpath_allow.unwrap_or_default()),
        deny_paths,
        deny_dirs: surface_strings(subpath_deny),
        deny_basenames,
    }
}

/// Resolve one literal exec key to the absolute path the OS gate names.
/// Absolute keys pass through as written; bare names resolve through the
/// grant's PATH override only — no host fallback, so an unresolvable
/// name fails closed (see reduced-authority-witness B6).
fn resolve_literal(name: &str, path_env: &str) -> Option<String> {
    if crate::path::is_absolute(name) {
        Some(name.to_string())
    } else {
        crate::path::resolve_in_path(name, path_env)
    }
}

/// Resolve every literal exec key named in the stack and keep the paths
/// whose natural invocation the full-stack live verdict admits — the
/// same [`evaluate_exec`] the in-ral gate runs.
///
/// A literal admitted by one layer can be covered only by a *sibling*
/// layer's allow-dir — `git: Allow` in one layer, `/usr/bin/` in
/// another.  The two dimensions meet only once the name is resolved to a
/// path, so the allow decision runs through [`evaluate_exec`] over the
/// resolved identity rather than combining names and dirs separately.
/// The OS profile then admits a path only where the live gate does.
///
/// The verdict query mirrors [`ExecNames`] for a real invocation:
/// allow-narrow is the key and its resolved path; deny-broad adds the
/// resolved basename, so a sibling `git: Deny` still vetoes a
/// `/usr/bin/git` literal exactly as it would at runtime.  An
/// unresolvable admitted name fails closed, with a trace it can no longer
/// be pinned.
fn admitted_literal_paths(ctx: &Context, names: &BTreeSet<String>, path_env: &str) -> Vec<String> {
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
        let deny: Vec<&str> = [name.as_str(), resolved.as_str(), crate::path::basename(&resolved)]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if let ExecVerdict::Allowed(_) = evaluate_exec(
            ctx,
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
