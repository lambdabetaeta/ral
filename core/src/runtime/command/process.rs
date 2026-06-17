//! Turn a [`SpawnPlan`] into a `std::process::Command`, spawn it via
//! the canonical pgid + sandbox funnel, and translate spawn / wait /
//! exit-status errors into [`Break`]s.  No stdio routing here — that
//! lives in [`super::stdio`] and [`super::redirect`] and is layered on
//! top before spawn.

use crate::types::*;
use std::process::Command;

use super::vet::{ExecImage, SpawnPlan};

/// Build a `Command` from a [`SpawnPlan`] and apply the shell's
/// scoped env vars + cwd.  Stdio routing and `pre_exec` hooks are the
/// caller's responsibility: those vary between single-command and
/// pipeline contexts.
///
/// A [`ExecImage::Host`] is `Command::new`'d directly.  A
/// [`ExecImage::BundledTool`] is rendered as a child placement of ral
/// itself (`ral --ral-bundled-tool <tool> …`) via the cross-platform
/// self-exec helper, so building it is fallible (the self-path lookup
/// can fail); the resulting child inherits cwd/env/PWD through
/// `apply_env` exactly like a host external.
pub(crate) fn build_command(plan: &SpawnPlan, shell: &Shell) -> Settled<Command> {
    let mut cmd = match &plan.image {
        ExecImage::Host(path) => crate::sandbox::make_command(path, &plan.args, shell),
        ExecImage::BundledTool { tool } => {
            use crate::runtime::pipeline::helper::{BUNDLED_TOOL_FLAG, self_reexec};
            let mut cmd = self_reexec(BUNDLED_TOOL_FLAG)
                .map_err(|e| Break::Error(Error::new(format!("bundled tool '{tool}': {e}"), 1)))?;
            cmd.arg(tool);
            cmd.args(&plan.args);
            if shell.has_active_capabilities() {
                crate::sandbox::apply_resource_limits(&mut cmd);
            }
            cmd
        }
    };
    apply_env(&mut cmd, shell);
    Ok(cmd)
}

/// Spawn an external child the canonical way: install the canonical
/// `pre_exec` (apply pgid, reset child signals) via
/// `signal::spawn_with_pgid`, mirror `setpgid` in the parent, then
/// apply sandbox child limits if any capability grant is active.
///
/// One funnel for both standalone exec ([`super::run`]) and pipeline
/// stages (`launch_external_stage` via `PipelineGroup::spawn`) so the
/// post-spawn boilerplate cannot drift.
///
/// Returns the child plus its leader pgid: `Some` when `pgid` is
/// `NewLeader` or `Join`, `None` for `Inherit`.
pub(crate) fn spawn(
    cmd: &mut Command,
    pgid: crate::process::PgidPolicy,
    shell: &Shell,
) -> std::io::Result<(std::process::Child, Option<crate::process::Pgid>)> {
    let (child, leader) = crate::process::spawn_with_pgid(cmd, pgid)?;
    if shell.has_active_capabilities() {
        crate::sandbox::apply_child_limits(&child);
    }
    Ok((child, leader))
}

/// Map a spawn `io::Error` for command `name` into a [`Break`].
///
/// Uses `compat::not_found_hint` for `NotFound` so the error message mentions
/// PATH issues and platform conventions.  Exit status 127 matches the POSIX
/// convention for "command not found".
pub(crate) fn spawn_error(name: &str, e: std::io::Error) -> Break {
    use crate::process::{CommandFailure, SpawnFailure};

    let failure = match e.kind() {
        std::io::ErrorKind::NotFound => CommandFailure::Spawn(SpawnFailure::NotFound),
        std::io::ErrorKind::PermissionDenied => {
            CommandFailure::Spawn(SpawnFailure::PermissionDenied)
        }
        _ => CommandFailure::Spawn(SpawnFailure::Io(e.to_string())),
    };
    Break::Error(Error {
        message: failure.message(name),
        status: crate::types::Status::Process(failure.clone()),
        loc: None,
        hint: failure.default_hint(name),
    })
}

/// Wrap an I/O error from pipe creation/cloning into a [`Break`].
pub(super) fn pipe_err(e: std::io::Error) -> Break {
    Break::Error(Error::new(format!("pipe: {e}"), 1))
}

/// Map a `std::io::Error` from a path operation into a [`Break`],
/// rendering `NotFound` / `PermissionDenied` as their canonical messages
/// and any other kind as the underlying error.  `ctx` is the path the
/// operation touched, used as the message prefix.  The error carries exit
/// code 1.
pub(super) fn io_error(ctx: &str, e: std::io::Error) -> Break {
    let msg = match e.kind() {
        std::io::ErrorKind::NotFound => format!("{ctx}: no such file or directory"),
        std::io::ErrorKind::PermissionDenied => format!("{ctx}: permission denied"),
        _ => format!("{ctx}: {e}"),
    };
    Break::Error(Error::new(msg, 1))
}

/// Propagate `context.env_overrides`, the shell's effective cwd, and
/// `PWD`/`OLDPWD` shell state into the child; strip dynamic-loader
/// overrides under an active grant.  Used by [`super::run`] and by
/// the pipeline stage builder.
///
/// `Command::current_dir` is always set, not only when `context.dir`
/// is present.  The shell's logical cwd may differ from the process
/// cwd whenever the user has run `cd` (which no longer mutates the
/// process cwd to avoid racing parallel threads), so the child's
/// cwd must come from `Shell::cwd()` rather than from a fork-inherit
/// of `getcwd(3)`.  `PWD` / `OLDPWD` ride along the same way: they
/// live in shell state and are threaded into each spawn, not
/// installed via `std::env::set_var`.
pub fn apply_env(cmd: &mut Command, shell: &Shell) {
    for (k, v) in shell.mobile.context.env_overrides() {
        cmd.env(k, v);
    }
    let cwd = shell.cwd();
    cmd.current_dir(&cwd);
    cmd.env("PWD", &cwd);
    if let Some(oldpwd) = &shell.mobile.context.cwd.previous {
        cmd.env("OLDPWD", oldpwd);
    } else {
        // The child must never inherit a `OLDPWD` from this process's
        // env: that value belongs to whichever shell launched us, not
        // to the ral session, and would mislead `cd -` inside the
        // child.  Strip it explicitly.
        cmd.env_remove("OLDPWD");
    }
    if shell.has_active_capabilities() {
        for var in &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            cmd.env_remove(var);
        }
    }
}
