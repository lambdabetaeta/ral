//! Turn a [`SpawnPlan`] into a `std::process::Command`, spawn it via
//! the canonical pgid + sandbox funnel, and translate spawn errors into
//! [`Break`]s.  No stdio routing here — that lives in [`super::stdio`]
//! and [`super::redirect`] and is layered on top before spawn.

use crate::types::{Break, Error, Settled, Shell};

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
///
/// Invoked by [`super::run`] and by the pipeline stage builder
/// (`launch_external_stage_direct`); it is never spawned here.
pub(crate) fn build_command(plan: &SpawnPlan, shell: &Shell) -> Settled<crate::process::Launch> {
    // A guest jail IS the guest's sandbox: bwrap needs unprivileged user
    // namespaces, which the guest's boot-time sysctl turns off, so a
    // jailed shell never wraps a spawn in bwrap on top — `check_exec_args`
    // still gates the call, only the OS-confinement layer changes.
    let jail = shell.guest_jail();
    let mut cmd = if jail.is_none()
        && let Some(projection) = shell.sandbox_projection()
    {
        // Per-command OS confinement: when a projection is active, confine
        // the child per-command under the effective projection.  The grant
        // body itself evaluates locally; this launcher is the sole OS-sandbox
        // locus.
        crate::sandbox::projection_enforceable(&projection).map_err(|reason| {
            Break::Error(Error::new(
                format!("sandbox confinement unavailable: {reason}"),
                1,
            ))
        })?;
        let target = match &plan.image {
            ExecImage::Host(program) => crate::sandbox::LaunchTarget::Host { program },
            ExecImage::BundledTool { tool } => crate::sandbox::LaunchTarget::BundledTool { tool },
        };
        crate::sandbox::sandboxed_command(&projection, target, &plan.args, shell)?
    } else {
        match &plan.image {
            ExecImage::Host(path) => {
                let mut cmd = crate::process::Launch::new(path);
                cmd.args(&plan.args);
                cmd
            }
            ExecImage::BundledTool { tool } => {
                use crate::runtime::pipeline::helper::{BUNDLED_TOOL_FLAG, self_reexec};
                let mut cmd = self_reexec(BUNDLED_TOOL_FLAG).map_err(|e| {
                    Break::Error(Error::new(format!("bundled tool '{tool}': {e}"), 1))
                })?;
                cmd.arg(tool);
                cmd.args(&plan.args);
                cmd
            }
        }
    };
    apply_env(&mut cmd, shell);
    #[cfg(unix)]
    if shell.has_active_capabilities() {
        cmd.apply_unix_resource_limits();
    }
    #[cfg(target_os = "linux")]
    if let Some(jail) = jail {
        cmd.apply_guest_jail(&jail.plan())
            .map_err(|e| Break::Error(Error::new(format!("guest jail: {e}"), 1)))?;
    }
    Ok(cmd)
}

/// Spawn a standalone external child the canonical way: install the
/// canonical `pre_exec` (apply pgid, reset child signals) via
/// `signal::spawn_with_pgid`, mirror `setpgid` in the parent, then
/// apply sandbox child limits if any capability grant is active.
///
/// The standalone-exec funnel only ([`super::run`]).  Pipeline stages
/// take a parallel post-spawn path — `PipelineGroup::spawn` followed by
/// `apply_child_limits_in_pipeline` — because they must join the group's
/// pgid rather than lead their own.
///
/// Returns the child plus its leader pgid: `Some` when `pgid` is
/// `NewLeader`, `NewSession`, or `Join`, `None` for `Inherit`.
pub(crate) fn spawn(
    cmd: &mut crate::process::Launch,
    pgid: crate::process::PgidPolicy,
    shell: &Shell,
) -> std::io::Result<(
    crate::process::ChildHandle,
    Option<crate::process::Pgid>,
    Option<crate::process::jail::JailCgroup>,
)> {
    let (child, leader, jail) = cmd.spawn(pgid)?;
    if shell.has_active_capabilities() {
        crate::sandbox::apply_child_limits(&child);
    }
    Ok((child, leader, jail))
}

/// Map a spawn `io::Error` for command `name` into a [`Break`].
///
/// Uses `compat::not_found_hint` for `NotFound` so the error message mentions
/// PATH issues and platform conventions.  Exit status 127 matches the POSIX
/// convention for "command not found".
pub(crate) fn spawn_error(name: &str, e: &std::io::Error) -> Break {
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
pub(super) fn pipe_err(e: &std::io::Error) -> Break {
    Break::Error(Error::new(format!("pipe: {e}"), 1))
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
pub fn apply_env(cmd: &mut crate::process::Launch, shell: &Shell) {
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
