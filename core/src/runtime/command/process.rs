//! Turn a [`SpawnPlan`] into a launchable child, spawn it through the
//! canonical pgid + sandbox funnel, and render spawn failures as [`Break`]s.
//! Stdio routing lives in `stdio` and `redirect`, layered on before spawn.

use crate::types::{Break, Error, Settled, Shell};

use super::vet::{ExecImage, SpawnPlan};

/// Build a launch for `plan` and apply the shell's scoped env and cwd.  Stdio
/// and `pre_exec` hooks are the caller's: they differ between the standalone
/// and pipeline contexts.
///
/// `ownership` reaches only the Linux bwrap backend, which ties the envelope
/// to this process's death unless the child is being surrendered.
pub(crate) fn build_command(
    plan: &SpawnPlan,
    ownership: crate::sandbox::Ownership,
    shell: &Shell,
    cancel: &crate::process::cancel::CancelScope,
) -> Settled<crate::process::Launch> {
    // A guest jail is already the guest's sandbox: bwrap needs unprivileged
    // user namespaces and the guest boots with `user.max_user_namespaces = 0`.
    // The capability gate in `vet` runs either way.
    let jail = shell.guest_jail();
    let mut cmd = if jail.is_none()
        && let Some(projection) = shell.sandbox_projection()
    {
        // A `grant` body evaluates in this process unconfined; spawned children
        // are the only thing an OS sandbox reaches, and this is where it does.
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
        crate::sandbox::sandboxed_command(
            &projection,
            target,
            &plan.args,
            ownership,
            shell,
            cancel,
        )?
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
        let plan = jail
            .plan()
            .map_err(|e| Break::Error(Error::new(format!("guest jail: {e}"), 1)))?;
        cmd.apply_guest_jail(&plan)
            .map_err(|e| Break::Error(Error::new(format!("guest jail: {e}"), 1)))?;
    }
    Ok(cmd)
}

/// Spawn a standalone external child, then apply any active grant's post-spawn
/// child limits.  Pipeline stages take the parallel path through
/// `spawn_into_group` in `runtime/pipeline/launch.rs`: they join the group's
/// pgid rather than lead their own, and their limits ride the group's job.
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

/// Render a spawn `io::Error` for command `name` as a [`Break`].  `NotFound`
/// reuses the one wording `vet`'s pre-spawn existence probe emits, so the two
/// paths never disagree about a missing command.
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
        span: None,
        hint: failure.default_hint(name),
    })
}

/// Wrap an I/O error from pipe creation or cloning as a [`Break`].
pub(super) fn pipe_err(e: &std::io::Error) -> Break {
    Break::Error(Error::new(format!("pipe: {e}"), 1))
}

/// Thread the shell's env overrides, logical cwd and `PWD`/`OLDPWD` into the
/// child; strip dynamic-loader overrides under an active grant.  `current_dir`
/// is set unconditionally because `cd` moves shell state and leaves the process
/// cwd alone, so an inherited `getcwd(3)` would be the wrong directory.
pub fn apply_env(cmd: &mut crate::process::Launch, shell: &Shell) {
    for (k, v) in shell.mobile.context.env_overrides() {
        cmd.env(k, v);
    }
    let cwd = shell.cwd();
    cmd.current_dir(&cwd);
    cmd.env("PWD", &cwd);
    if let Some(oldpwd) = shell.oldpwd() {
        cmd.env("OLDPWD", oldpwd);
    } else {
        // An inherited `OLDPWD` names whichever shell launched ral, not this
        // session, and would mislead a `cd -` inside the child.
        cmd.env_remove("OLDPWD");
    }
    if shell.has_active_capabilities() {
        // A loader hook makes an admitted binary run someone else's code, so
        // the grant's judgment about which program may run would mean nothing.
        // On macOS the stakes are higher still: a sandboxed external is a
        // re-exec of ral itself, and dyld honours these before `main` runs, so
        // an injected dylib would execute before the child enters Seatbelt.
        for var in &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            cmd.env_remove(var);
        }
        for var in dyld_vars(shell) {
            cmd.env_remove(var);
        }
    }
}

/// Every `DYLD_`-prefixed name the child would otherwise see, from this
/// process's environment and from the shell's own overrides.  The whole prefix
/// rather than dyld's current list: the loader owns that namespace, and a name
/// added by a future dyld must not become a hole.
fn dyld_vars(shell: &Shell) -> Vec<std::ffi::OsString> {
    const PREFIX: &str = "DYLD_";
    let inherited = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| k.to_string_lossy().starts_with(PREFIX));
    let overridden = shell
        .mobile
        .context
        .env_overrides()
        .iter()
        .filter(|(k, _)| k.starts_with(PREFIX))
        .map(|(k, _)| std::ffi::OsString::from(k));
    inherited.chain(overridden).collect()
}
