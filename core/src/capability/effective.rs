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
use crate::types::{Audit, Capabilities, Dynamic, EvalSignal, FsPolicy, Location, SandboxProjection};

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
/// Returns `None` when no layer imposes fs or net restrictions so callers
/// can cheaply skip OS sandbox setup.
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

    if !saw_fs && (!saw_net || net_allowed) {
        return None;
    }

    Some(SandboxProjection {
        fs: FsPolicy {
            read_prefixes: grant_path_raws(&read_prefixes.unwrap_or_default()),
            write_prefixes: grant_path_raws(&write_prefixes.unwrap_or_default()),
            deny_paths: crate::types::unique_strings(deny_paths),
        },
        net: net_allowed,
    })
}
