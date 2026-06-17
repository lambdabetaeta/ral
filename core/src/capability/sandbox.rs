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

use crate::path::{NormalizedPrefix, PrefixSet};
use crate::types::{
    Context, ExecDir, ExecPolicy, ExecProjection, FsPolicy, FsProjection, Meet, SandboxProjection,
    meet_literal_exec,
};
use std::collections::{BTreeMap, BTreeSet};

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
/// `Restricted` carries three meet-folded sets:
///
///   * `allow_paths` — literal exec keys (Allow / Subcommands)
///     resolved to absolute paths via PATH.  The OS profile renders
///     them as `(literal …)`.
///   * `allow_dirs` — subpath keys carrying `Allow`, intersected by
///     prefix across opining layers.  Rendered as `(subpath …)`.
///   * `deny_paths` — literal exec keys carrying `Deny`, resolved to
///     absolute paths via PATH and unioned across layers.
///   * `deny_dirs` — subpath keys carrying `Deny`, *unioned* across
///     layers (denies are sticky).  Rendered as `(deny process-exec
///     (subpath …))` after the broad allow so SBPL's last-match-wins
///     gives them precedence.
fn reduce_exec(ctx: &Context) -> ExecProjection {
    let resolver = ctx.resolver();
    let mut subpath_allow: Option<PrefixSet> = None;
    let mut subpath_deny = PrefixSet::default();
    let mut literal_map: Option<BTreeMap<String, ExecPolicy>> = None;
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
        literal_map = Some(match literal_map {
            Some(prev) => meet_literal_exec(prev, map.literals.clone()),
            None => map.literals.clone(),
        });
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
    let allow_dirs = surface_strings(subpath_allow.unwrap_or_default());
    let deny_dirs = surface_strings(subpath_deny);
    let lit = literal_map.unwrap_or_default();
    let deny_paths = resolve_exec_names(ctx, &lit, |p| matches!(p, ExecPolicy::Deny), false);
    let allow_paths = resolve_exec_names(ctx, &lit, |p| !matches!(p, ExecPolicy::Deny), true);
    ExecProjection::Restricted {
        allow_paths,
        allow_dirs,
        deny_paths,
        deny_dirs,
    }
}

/// Resolve the literal exec keys matching `keep` to absolute paths for
/// the OS projection.  Bare names resolve through the grant's PATH
/// override only — no host fallback, so an unresolvable name fails
/// closed (see reduced-authority-witness B6); absolute keys pass through
/// as written.  `trace_miss` logs a debug trace when an admitted name
/// cannot be pinned (allow side); deny resolution is silent.
fn resolve_exec_names(
    ctx: &Context,
    map: &BTreeMap<String, ExecPolicy>,
    keep: impl Fn(&ExecPolicy) -> bool,
    trace_miss: bool,
) -> Vec<String> {
    let path_env = ctx
        .env_overrides()
        .get("PATH")
        .map(String::as_str)
        .unwrap_or("");
    let mut out = std::collections::BTreeSet::new();
    for (name, policy) in map {
        if !keep(policy) {
            continue;
        }
        if crate::path::is_absolute(name) {
            out.insert(name.clone());
        } else if let Some(resolved) = crate::path::which::resolve_in_path(name, path_env) {
            out.insert(resolved);
        } else if trace_miss {
            crate::dbg_trace!(
                "sandbox-exec",
                "exec '{}' not on PATH at projection time; OS gate cannot pin it",
                name
            );
        }
    }
    out.into_iter().collect()
}
