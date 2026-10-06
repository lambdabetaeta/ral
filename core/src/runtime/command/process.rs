//! Turn a [`SpawnPlan`] into a launchable child, spawn it through the
//! canonical pgid + sandbox funnel, and render spawn failures as [`Break`]s.
//! Stdio routing lives in `stdio` and `redirect`, layered on before spawn.

use crate::capability::Program;
use crate::sandbox::Refusal;
use crate::types::{Break, Error, Settled, Shell, sig};

use super::vet::SpawnPlan;

impl From<Refusal> for Break {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            #[cfg(windows)]
            Refusal::Cancelled(cause) => Self::Error(Error::cancelled(cause)),
            other => sig(other.to_string()),
        }
    }
}

/// Build a launch for `plan` and apply the shell's scoped env and cwd.  Stdio
/// and `pre_exec` hooks are the caller's: they differ between the standalone
/// and pipeline contexts.
///
/// `ownership` reaches only the Linux bwrap backend, which ties the envelope
/// to this process's death unless the child is being surrendered.
pub(crate) fn build_launch(
    plan: &SpawnPlan,
    ownership: crate::sandbox::Ownership,
    shell: &Shell,
    cancel: &crate::process::cancel::CancelScope,
) -> Settled<crate::process::Launch> {
    // A guest jail is already the guest's sandbox: bwrap needs unprivileged
    // user namespaces and the guest boots with `user.max_user_namespaces = 0`.
    // The in-process guard in `vet` runs either way.
    let jail = shell.guest_jail();
    let admitted = &plan.admitted;
    let mut cmd = if jail.is_none()
        && let Some(projection) = crate::sandbox::SandboxProjection::of(
            &shell.context.grants,
            &shell.context.resolver(),
            Some(admitted),
        ) {
        // A `grant` body evaluates in this process unconfined; spawned children
        // are the only thing an OS sandbox reaches, and this is where it does.
        crate::sandbox::projection_enforceable(&projection)?;
        crate::sandbox::sandboxed_command(&projection, admitted, ownership, &shell.cwd(), cancel)?
    } else {
        match admitted.program() {
            Program::File { path, real } => {
                let mut cmd = crate::process::Launch::new(real.as_path());
                cmd.arg0(path);
                cmd.args(admitted.args());
                cmd
            }
            Program::Tool(tool) => {
                let mut cmd = crate::sandbox::reexec::launch(crate::Role::BundledTool)
                    .map_err(|e| sig(format!("bundled tool '{tool}': {e}")))?;
                cmd.arg(tool);
                cmd.args(admitted.args());
                cmd
            }
        }
    };
    apply_env(&mut cmd, shell);
    #[cfg(unix)]
    if shell.has_active_capabilities() {
        cmd.limit_resources().map_err(|e| unbraked(&e))?;
    }
    #[cfg(target_os = "linux")]
    if let Some(jail) = jail {
        let plan = jail.plan().map_err(|e| sig(format!("guest jail: {e}")))?;
        cmd.apply_guest_jail(&plan)
            .map_err(|e| sig(format!("guest jail: {e}")))?;
    }
    Ok(cmd)
}

/// Spawn external command `name` into `pgid`, braked under an active grant.
/// A pipeline stage comes here with `Join`, and its brake caps the group's job.
pub(crate) fn spawn(
    cmd: &mut crate::process::Launch,
    pgid: crate::process::PgidPolicy,
    name: &str,
    shell: &Shell,
) -> Settled<(
    crate::process::ChildHandle,
    Option<crate::process::Pgid>,
    Option<crate::process::jail::JailCgroup>,
)> {
    let confinement = cmd.confinement();
    let (mut child, led, jail) = cmd
        .spawn(pgid)
        .map_err(|e| spawn_error(confinement, name, &e))?;
    if shell.has_active_capabilities() {
        brake(&mut child, pgid, led)?;
    }
    Ok((child, led, jail))
}

/// `err` with the kernel's denial hint, when a sandboxed child failed on its
/// own account.  Only then are `pids` sampled: an unsandboxed or cancelled
/// failure never pays for the process-table walk.
pub(crate) fn with_denials(
    err: Error,
    shell: &Shell,
    pids: impl FnOnce() -> std::collections::HashSet<u32>,
    since: std::time::Instant,
) -> Error {
    if shell.sandbox_projection().is_none() || err.cancelled_by().is_some() {
        return err;
    }
    match crate::sandbox::denial_hint(&pids(), since, err.code()) {
        Some(hint) => err.append_hint(hint),
        None => err,
    }
}

/// Cap `child`'s process tree in the group `pgid` placed it in.  When the cap
/// cannot be installed nothing the spawn made survives, the group it `led`
/// included: a child under a grant never runs unbraked.
pub(crate) fn brake(
    child: &mut crate::process::ChildHandle,
    pgid: crate::process::PgidPolicy,
    led: Option<crate::process::Pgid>,
) -> Result<(), Refusal> {
    let joined = match pgid {
        crate::process::PgidPolicy::Join(group) => Some(group),
        _ => None,
    };
    child.limit_processes(led.or(joined)).map_err(|e| {
        let _ = child.kill();
        let _ = child.reap();
        #[cfg(windows)]
        if let Some(group) = led {
            crate::process::release_win_group(group.as_raw());
        }
        unbraked(&e)
    })
}

fn unbraked(e: &std::io::Error) -> Refusal {
    Refusal::Unavailable(format!(
        "{e}, so this command would run with no process budget"
    ))
}

/// Render a spawn `io::Error` for command `name` as a [`Break`], through the
/// same `Error::spawn_failure` `vet`'s pre-spawn probe mints, so the two paths
/// never disagree about a command that could not start.
///
/// With a `confinement` — the envelope binary exec'd in `name`'s place — the
/// failure is the envelope's, not `name`'s: `vet` resolved `name` before we got
/// here, and an envelope execs its own target, so a missing one comes back as an
/// exit status rather than a spawn failure.  Blaming `name` would accuse the one
/// program we know exists.
fn spawn_error(confinement: Option<&'static str>, name: &str, e: &std::io::Error) -> Break {
    use crate::process::SpawnFailure;

    if let Some(envelope) = confinement {
        let why = Refusal::Unavailable(format!("cannot start {envelope}: {e}"));
        return Break::Error(Error::new(why.to_string()).with_hint(format!(
            "The envelope failed to launch, so {name} never ran."
        )));
    }

    Break::Error(Error::spawn_failure(name, SpawnFailure::from(e)))
}

/// Thread the shell's env overrides, logical cwd and `PWD` into the
/// child; strip dynamic-loader overrides under an active grant.  `current_dir`
/// is set unconditionally because `cd` moves shell state and leaves the process
/// cwd alone, so an inherited `getcwd(3)` would be the wrong directory.
pub(crate) fn apply_env(cmd: &mut crate::process::Launch, shell: &Shell) {
    for (k, v) in shell.context.env_overrides() {
        cmd.env(k, v);
    }
    let cwd = shell.cwd();
    cmd.current_dir(&cwd);
    cmd.env("PWD", &cwd);
    // ral keeps no previous directory; an inherited `OLDPWD` names the
    // launcher's, and would mislead a `cd -` inside the child.
    cmd.env_remove("OLDPWD");
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
        .context
        .env_overrides()
        .iter()
        .filter(|(k, _)| k.starts_with(PREFIX))
        .map(|(k, _)| std::ffi::OsString::from(k));
    inherited.chain(overridden).collect()
}

#[cfg(test)]
mod tests {
    use super::spawn_error;
    use crate::types::Break;

    fn message(b: &Break) -> &str {
        let Break::Error(e) = b else {
            panic!("spawn_error yields an Error break")
        };
        &e.message
    }

    /// One `ENOENT` means two different things depending on who the launcher
    /// exec'd, and the wrong reading turns a failed envelope into a grant that
    /// appears to deny every command.
    #[test]
    fn a_failed_envelope_is_not_reported_as_a_missing_command() {
        let enoent = std::io::Error::from(std::io::ErrorKind::NotFound);

        let unconfined = spawn_error(None, "pwd", &enoent);
        assert_eq!(message(&unconfined), "pwd: command not found");

        let confined = spawn_error(Some("bwrap"), "pwd", &enoent);
        assert!(
            message(&confined).starts_with("sandbox confinement unavailable: cannot start bwrap"),
            "{}",
            message(&confined)
        );
    }
}

/// What runs is the file that was judged, by its real path and under the
/// spelling the user wrote, however the spelling moves in between.
#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod judged_program {
    use super::super::{
        head::Head,
        vet::{SpawnPlan, vet},
    };
    use super::build_launch;
    use crate::ir::CommandName;
    use crate::path::RealPath;
    use crate::process::{CancelScope, PgidPolicy, StdioSpec};
    use crate::types::{Shell, Value};
    use std::io::Read;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    /// A scratch directory holding `tools/`, as the shell's cwd.
    fn workdir() -> (tempfile::TempDir, Shell) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("tools")).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(tmp.path().to_path_buf());
        (tmp, shell)
    }

    /// An executable that prints `who`.
    fn plant(dir: &Path, who: &str) -> PathBuf {
        let path = dir.join(who);
        std::fs::write(&path, format!("#!/bin/sh\necho {who}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `tools/build`, made to point at `target`.
    fn point_build_at(dir: &Path, target: &Path) -> PathBuf {
        let link = dir.join("tools/build");
        let _ = std::fs::remove_file(&link);
        symlink(target, &link).unwrap();
        link
    }

    fn judge(shell: &mut Shell, args: &[&str]) -> SpawnPlan {
        let head = Head::resolve(&CommandName::Path("tools/build".into()), &shell.context);
        let args: Vec<_> = args.iter().map(|arg| Value::string(*arg)).collect();
        vet(&head, &args, shell).expect("an unrestricted shell admits the file")
    }

    fn stdout_of(plan: &SpawnPlan, shell: &Shell) -> String {
        let mut cmd = build_launch(
            plan,
            crate::sandbox::Ownership::Kept,
            shell,
            &CancelScope::default(),
        )
        .expect("the launch builds");
        cmd.stdout(StdioSpec::piped());
        let (mut child, ..) = cmd.spawn(PgidPolicy::Inherit).expect("the file runs");
        let mut out = String::new();
        child
            .take_stdout()
            .expect("stdout was piped")
            .read_to_string(&mut out)
            .unwrap();
        child.reap().expect("reaps");
        out
    }

    /// The control: the harness tells `a` from `b`, so the next test cannot
    /// pass by the two being alike.
    #[test]
    fn a_judged_link_runs_its_target() {
        for who in ["a", "b"] {
            let (tmp, mut shell) = workdir();
            point_build_at(tmp.path(), &plant(tmp.path(), who));
            assert_eq!(
                stdout_of(&judge(&mut shell, &[]), &shell),
                format!("{who}\n")
            );
        }
    }

    /// A pipeline's earlier stage may retarget a later one's link after the
    /// head is resolved and before it launches: the judged `a` still runs.
    #[test]
    fn a_link_retargeted_after_judgment_runs_the_file_judged() {
        let (tmp, mut shell) = workdir();
        let (a, b) = (plant(tmp.path(), "a"), plant(tmp.path(), "b"));
        let link = point_build_at(tmp.path(), &a);

        let plan = judge(&mut shell, &[]);
        point_build_at(tmp.path(), &b);
        assert_eq!(
            RealPath::of(&link).unwrap(),
            RealPath::of(&b).unwrap(),
            "the link moved"
        );
        assert_eq!(stdout_of(&plan, &shell), "a\n");
    }

    #[test]
    fn a_program_sees_the_spelling_it_was_named_by_as_argv0() {
        let (tmp, mut shell) = workdir();
        let link = point_build_at(tmp.path(), Path::new("/bin/sh"));

        let plan = judge(&mut shell, &["-c", r#"printf %s "$0""#]);
        assert_eq!(stdout_of(&plan, &shell), link.to_string_lossy());
    }
}
