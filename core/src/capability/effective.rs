//! Effective grant — the single decision front door for the dynamic stack.
//!
//! `EffectiveGrant` is a thin lazy borrow over `Dynamic`: it does not
//! cache a reduced authority object, but every yes/no answer the
//! runtime asks of the capability stack — exec, fs, editor, shell —
//! and the OS-renderable `SandboxProjection` flow through it.  The
//! sandbox path and the in-ral check path therefore share one
//! semantic source even though each method recomputes its fold.

use super::check::{
    check_editor_bool, check_exec_args_impl, check_fs_read_impl, check_fs_write_impl,
    check_shell_bool,
};
use super::exec::{ExecVerdict, evaluate_exec};
use super::prefix::{GrantPath, canonical_grant_paths, grant_path_raws, intersect_grant_paths};
use crate::types::{
    is_subpath_key, meet_literal_exec, Audit, Capabilities, Dynamic, EvalSignal, ExecPolicy,
    ExecProjection, FsPolicy, FsProjection, Location, SandboxProjection,
};
use std::collections::BTreeMap;

/// The single decision front door for the dynamic capability stack.
///
/// A zero-cost borrow over `Dynamic`: each `check_*` method walks the
/// stack on demand rather than caching a folded result, but every
/// runtime question goes through the same handle so the rules live in
/// one place.  Use [`EffectiveGrant::sandbox_projection`] for the
/// OS-renderable form and the various `check_*` methods for in-ral
/// decisions.
pub(crate) struct EffectiveGrant<'a> {
    dynamic: &'a Dynamic,
}

impl<'a> EffectiveGrant<'a> {
    /// Wrap a `Dynamic` to expose its capability decisions.
    pub(crate) fn from_dynamic(dynamic: &'a Dynamic) -> Self {
        Self { dynamic }
    }

    /// Meet-fold the stack into the OS-renderable projection.  Returns
    /// `None` when no layer imposes fs or net restrictions, so callers
    /// can cheaply skip OS sandbox setup.
    pub(crate) fn sandbox_projection(&self) -> Option<SandboxProjection> {
        reduce(self.dynamic)
    }

    /// Whether the active stack denies every candidate name outright.
    /// Used by `classify_command_head` to colour the dispatch site
    /// before any args are parsed.
    pub(crate) fn is_exec_denied_for(&self, names: &[&str]) -> bool {
        matches!(evaluate_exec(self.dynamic, names), ExecVerdict::Denied)
    }

    /// Validate an exec capability check and emit an audit node when
    /// auditing is enabled.
    pub(crate) fn check_exec_args(
        &self,
        display_name: &str,
        policy_names: &[&str],
        args: &[String],
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        check_exec_args_impl(self.dynamic, display_name, policy_names, args, audit, location)
    }

    /// Validate an fs read against the active stack.
    pub(crate) fn check_fs_read(
        &self,
        path: &str,
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        check_fs_read_impl(self.dynamic, path, audit, location)
    }

    /// Validate an fs write against the active stack.
    pub(crate) fn check_fs_write(
        &self,
        path: &str,
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        check_fs_write_impl(self.dynamic, path, audit, location)
    }

    /// Check `editor.read` capability is available.
    pub(crate) fn check_editor_read(&self, subcmd: &str) -> Result<(), EvalSignal> {
        check_editor_bool(
            self.dynamic,
            || format!("denied: _editor '{subcmd}' requires editor.read"),
            |ed| ed.read,
        )
    }

    /// Check `editor.write` capability is available.
    pub(crate) fn check_editor_write(&self, subcmd: &str) -> Result<(), EvalSignal> {
        check_editor_bool(
            self.dynamic,
            || format!("denied: _editor '{subcmd}' requires editor.write"),
            |ed| ed.write,
        )
    }

    /// Check `editor.tui` capability is available.
    pub(crate) fn check_editor_tui(&self) -> Result<(), EvalSignal> {
        check_editor_bool(
            self.dynamic,
            || "denied: _editor 'tui' requires editor.tui".into(),
            |ed| ed.tui,
        )
    }

    /// Check `shell.chdir` capability is available.
    pub(crate) fn check_shell_chdir(&self) -> Result<(), EvalSignal> {
        check_shell_bool(self.dynamic, || "denied: cd requires shell.chdir".into(), |sh| sh.chdir)
    }
}

/// Walk the capabilities stack; if any layer with a relevant policy
/// votes `false`, return a denial error.  `test` returns
/// `Some(allowed)` when the layer has an opinion, `None` to abstain.
///
/// Shared by editor/shell bool gates; lives here because it's the
/// reduction step those checks share.
pub(super) fn check_grant_bool(
    dynamic: &Dynamic,
    msg: impl Fn() -> String,
    test: impl Fn(&Capabilities) -> Option<bool>,
) -> Result<(), EvalSignal> {
    for ctx in &dynamic.capabilities_stack {
        if test(ctx) == Some(false) {
            return Err(EvalSignal::Error(crate::types::Error::new(msg(), 1)));
        }
    }
    Ok(())
}

/// Meet-fold the stack into an `Option<SandboxProjection>`.
///
/// Returns `None` when no layer imposes fs, net, or exec restrictions so
/// callers can cheaply skip OS sandbox setup.
fn reduce(dynamic: &Dynamic) -> Option<SandboxProjection> {
    let mut read_prefixes: Option<Vec<GrantPath>> = None;
    let mut write_prefixes: Option<Vec<GrantPath>> = None;
    let mut deny_paths: Vec<String> = Vec::new();
    let mut net_allowed = true;
    let mut saw_fs = false;
    let mut saw_net = false;

    for ctx in &dynamic.capabilities_stack {
        if let Some(fs) = &ctx.fs {
            saw_fs = true;
            let read = canonical_grant_paths(dynamic, &fs.read_prefixes);
            let write = canonical_grant_paths(dynamic, &fs.write_prefixes);
            read_prefixes = Some(match read_prefixes {
                Some(current) => intersect_grant_paths(&current, &read),
                None => read,
            });
            write_prefixes = Some(match write_prefixes {
                Some(current) => intersect_grant_paths(&current, &write),
                None => write,
            });
            let resolver = dynamic.resolver();
            for p in &fs.deny_paths {
                deny_paths.push(p.clone());
                deny_paths.push(resolver.check(p).to_string_lossy().into_owned());
            }
        }
        if let Some(net) = ctx.net {
            saw_net = true;
            net_allowed &= net;
        }
    }

    let exec = reduce_exec(dynamic);
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
            read_prefixes: grant_path_raws(&read_prefixes.unwrap_or_default()),
            write_prefixes: grant_path_raws(&write_prefixes.unwrap_or_default()),
            deny_paths: crate::types::unique_strings(deny_paths),
        })
    } else {
        FsProjection::Unrestricted
    };
    Some(SandboxProjection { fs, net: net_allowed, exec })
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
///   * `deny_dirs` — subpath keys carrying `Deny`, *unioned* across
///     layers (denies are sticky).  Rendered as `(deny process-exec
///     (subpath …))` after the broad allow so SBPL's last-match-wins
///     gives them precedence.
fn reduce_exec(dynamic: &Dynamic) -> ExecProjection {
    let mut subpath_allow: Option<Vec<GrantPath>> = None;
    let mut subpath_deny: Vec<GrantPath> = Vec::new();
    let mut literal_map: Option<BTreeMap<String, ExecPolicy>> = None;
    let mut saw = false;
    for ctx in &dynamic.capabilities_stack {
        let Some(map) = &ctx.exec else { continue };
        saw = true;
        let SplitMap { literal, allow_subpaths, deny_subpaths } = split_exec_map(map);
        let allow_canon = canonical_grant_paths(dynamic, &allow_subpaths);
        let deny_canon = canonical_grant_paths(dynamic, &deny_subpaths);
        subpath_allow = Some(match subpath_allow {
            Some(prev) => intersect_grant_paths(&prev, &allow_canon),
            None => allow_canon,
        });
        subpath_deny.extend(deny_canon);
        literal_map = Some(match literal_map {
            Some(prev) => meet_literal_exec(prev, literal),
            None => literal,
        });
    }
    if !saw {
        return ExecProjection::Unrestricted;
    }
    let allow_dirs = grant_path_raws(&subpath_allow.unwrap_or_default())
        .into_iter()
        .map(|p| p.trim_end_matches('/').to_string())
        .collect();
    let deny_dirs = grant_path_raws(&subpath_deny)
        .into_iter()
        .map(|p| p.trim_end_matches('/').to_string())
        .collect();
    let allow_paths = resolve_allow_names(dynamic, literal_map.unwrap_or_default());
    ExecProjection::Restricted { allow_paths, allow_dirs, deny_dirs }
}

/// Split a unified exec map into its three render-time halves.  The
/// trailing `/` is dropped from subpath keys so they can flow through
/// the existing `canonical_grant_paths` / `intersect_grant_paths`
/// pipeline alongside fs prefixes; the marker is restored only at
/// SBPL render time, not in the projection.
struct SplitMap {
    literal: BTreeMap<String, ExecPolicy>,
    allow_subpaths: Vec<String>,
    deny_subpaths: Vec<String>,
}

fn split_exec_map(map: &BTreeMap<String, ExecPolicy>) -> SplitMap {
    let mut literal = BTreeMap::new();
    let mut allow_subpaths = Vec::new();
    let mut deny_subpaths = Vec::new();
    for (key, policy) in map {
        if is_subpath_key(key) {
            let path = key.trim_end_matches('/').to_string();
            match policy {
                ExecPolicy::Deny => deny_subpaths.push(path),
                ExecPolicy::Allow => allow_subpaths.push(path),
                // Rejected at validate_paths; ignore defensively.
                ExecPolicy::Subcommands(_) => {}
            }
        } else {
            literal.insert(key.clone(), policy.clone());
        }
    }
    SplitMap { literal, allow_subpaths, deny_subpaths }
}

/// Resolve named `[exec]` allows to absolute paths via the parent's PATH.
/// `Deny` entries contribute no positive authority and are skipped — the
/// OS-layer rule is purely an allow-list, with deny-default doing the work
/// for everything not on the list.  Absolute names are kept as-is.  Names
/// that fail PATH resolution are dropped with a debug trace.
fn resolve_allow_names(dynamic: &Dynamic, map: BTreeMap<String, ExecPolicy>) -> Vec<String> {
    let path_env = dynamic
        .env_vars()
        .get("PATH")
        .map(String::as_str)
        .unwrap_or("");
    let mut out = Vec::new();
    for (name, policy) in &map {
        match policy {
            ExecPolicy::Deny => continue,
            ExecPolicy::Allow | ExecPolicy::Subcommands(_) => {}
        }
        if name.starts_with('/') {
            out.push(name.clone());
            continue;
        }
        if let Some(resolved) = crate::path::which::resolve_in_path(name, path_env) {
            out.push(resolved);
        } else {
            crate::dbg_trace!(
                "sandbox-exec",
                "exec '{}' not on PATH at projection time; OS gate cannot pin it",
                name
            );
        }
    }
    out.sort();
    out.dedup();
    out
}
