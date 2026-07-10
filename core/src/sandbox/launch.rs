//! Per-command OS sandbox launching.
//!
//! This module confines a single external/bundled child: it takes the
//! already-folded effective [`SandboxProjection`] and builds a `Command`
//! that runs one target program under the platform backend.  The command
//! runner's `build_command` routes through here whenever a projection is
//! active (see `decisions/260617_sandbox-external-children`).
//!
//! Env/cwd and resource limits are deliberately **not** applied here. The
//! caller layers `apply_env` + `apply_resource_limits` on the returned
//! `Command` uniformly, exactly as it does for the unsandboxed path, so
//! the two paths cannot drift on how a child inherits cwd/PWD/overrides.
//! Linux is the one place the in-sandbox cwd needs explicit help: bwrap
//! does not honour the launcher's `current_dir`, so the target's logical
//! cwd is threaded into the bwrap argv as `--chdir <cwd>` from `shell`.

use crate::types::{Break, Error, Settled, Shell};
use std::process::Command;

/// What [`sandboxed_command`] should run under the OS sandbox.  A host
/// program is its resolved path/bare name; a bundled tool is run as a
/// child placement of ral itself (`ral --ral-bundled-tool <tool> …`).
pub(crate) enum LaunchTarget<'a> {
    Host { program: &'a str },
    BundledTool { tool: &'a str },
}

/// Build a `Command` that runs `target` + `args` under the OS sandbox for
/// `projection`.  The caller has already confirmed
/// [`super::projection_enforceable`].  Env/cwd and resource limits are
/// applied by the caller afterwards, not here.
///
/// `shell` is taken only for the target's logical cwd, which Linux folds
/// into the bwrap argv as `--chdir`.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        unused_variables,
        reason = "shell.cwd() is only consumed for the bwrap --chdir"
    )
)]
pub(crate) fn sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
    shell: &Shell,
) -> Settled<Command> {
    #[cfg(target_os = "linux")]
    {
        linux_sandboxed_command(projection, target, args, shell)
    }
    #[cfg(target_os = "macos")]
    {
        macos_sandboxed_command(projection, target, args)
    }
    #[cfg(windows)]
    {
        // TODO(M3 windows subagent): implement AppContainer / restricted-token
        // per-command confinement with net fail-closed.  Until then, fail
        // closed rather than silently run unsandboxed.
        let _ = (projection, target, args);
        Err(Break::Error(Error::new(
            "per-command Windows sandbox not yet implemented",
            1,
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (projection, target, args);
        Err(Break::Error(Error::new(
            "no per-command sandbox backend on this platform",
            1,
        )))
    }
}

/// Linux: expand the target into the bwrap argv via
/// [`super::linux::make_command_with_policy`], which binds an absolute
/// program path RO so bwrap can exec it.  A [`LaunchTarget::BundledTool`]
/// resolves to the on-disk self exe so bwrap re-execs *us* with
/// `--ral-bundled-tool <tool> …`.
///
/// The self-path used here is the on-disk `arg0` (`current_exe()`), not
/// the fd-pinned `exec_path` (`/proc/self/fd/N`): bwrap mounts a fresh
/// `/proc`, so a `/proc/self/fd` target would neither bind nor resolve in
/// the child.  This matches `respawn_under_bwrap`, which likewise hands
/// bwrap `current_exe()`.
///
/// The target's logical cwd is threaded in as bwrap's `--chdir`: the
/// caller's `apply_env` sets the launcher (bwrap) process cwd, but bwrap
/// runs the child in its mount-namespace root by default, so the
/// in-sandbox cwd must be requested explicitly.
#[cfg(target_os = "linux")]
fn linux_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
    shell: &Shell,
) -> Settled<Command> {
    let cwd = shell.cwd().to_string_lossy().into_owned();
    match target {
        LaunchTarget::Host { program } => Ok(super::linux::make_command_with_policy(
            program,
            args,
            projection,
            Some(cwd.as_str()),
        )),
        LaunchTarget::BundledTool { tool } => {
            let self_path = super::reexec::SANDBOX_SELF
                .get()
                .map(|s| s.arg0.clone()).map_or_else(std::env::current_exe, Ok)
                .map_err(|e| {
                    Break::Error(Error::new(
                        format!("sandbox: cannot resolve self exe for bundled tool: {e}"),
                        1,
                    ))
                })?;
            let mut tool_args = Vec::with_capacity(args.len() + 2);
            tool_args.push(crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG.to_string());
            tool_args.push(tool.to_string());
            tool_args.extend_from_slice(args);
            Ok(super::linux::make_command_with_policy(
                &self_path.to_string_lossy(),
                &tool_args,
                projection,
                Some(cwd.as_str()),
            ))
        }
    }
}

/// macOS: re-exec ral with the folded projection so the child enters
/// Seatbelt in `early_init`, then runs the target *inside* that Seatbelt.
///
/// - [`LaunchTarget::BundledTool`] uses the existing `--ral-bundled-tool`
///   tail: after `early_init` enters Seatbelt, `try_run_bundled_tool`
///   runs the tool in-process, confined — no further exec needed.
/// - [`LaunchTarget::Host`] uses the [`super::SANDBOX_EXEC_FLAG`] tail:
///   after Seatbelt is entered, [`super::serve_sandbox_exec`] `execve`s
///   the host program inside the Seatbelt this process just entered.
#[cfg(target_os = "macos")]
fn macos_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
) -> Settled<Command> {
    let json = serde_json::to_string(projection).map_err(|e| {
        Break::Error(Error::new(
            format!("sandbox: failed to encode projection: {e}"),
            1,
        ))
    })?;
    // Preserve binary-swap protection: refuse to re-exec if our pinned
    // executable was swapped on disk since boot (e.g. a mid-session
    // `cargo install`), which would otherwise launch a foreign build.
    if let Some(s) = super::reexec::SANDBOX_SELF.get() {
        super::reexec::verify_unswapped(s).map_err(Break::Error)?;
    }
    let mut cmd = super::self_command().map_err(|e| {
        Break::Error(Error::new(
            format!("sandbox: failed to pin self for re-exec: {e}"),
            1,
        ))
    })?;
    cmd.arg(super::SANDBOX_PROJECTION_FLAG).arg(json);
    match target {
        LaunchTarget::BundledTool { tool } => {
            cmd.arg(crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG)
                .arg(tool)
                .args(args);
        }
        LaunchTarget::Host { program } => {
            cmd.arg(super::SANDBOX_EXEC_FLAG).arg(program).args(args);
        }
    }
    Ok(cmd)
}

/// macOS hook: after `early_init` has entered Seatbelt, run the host
/// program of a `LaunchTarget::Host` re-exec carried in the
/// `--ral-sandbox-exec` tail.  `execve` replaces this process, so on
/// success it never returns; a spawn failure surfaces as 127, the POSIX
/// "command not found / cannot exec" code.
///
/// `args` is the post-`early_init` argv (Seatbelt already applied).  When
/// it does not start with the sentinel this returns `None` and the normal
/// dispatch continues.
#[cfg(target_os = "macos")]
pub fn serve_sandbox_exec(args: &[String]) -> Option<u8> {
    use std::os::unix::process::CommandExt;

    let (flag, rest) = args.split_first()?;
    if flag != super::SANDBOX_EXEC_FLAG {
        return None;
    }
    let Some((program, prog_args)) = rest.split_first() else {
        let flag = super::SANDBOX_EXEC_FLAG;
        crate::diagnostic::cmd_error("ral", &format!("{flag} requires a program"));
        return Some(127);
    };
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:respawn-spawn] sandbox respawn handoff: builds the Command for the confined re-exec; the surface card fired before this handoff, so the exec itself raises no card."
    )]
    let mut cmd = Command::new(program);
    cmd.args(prog_args);
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:respawn-exec] sandbox respawn handoff: `exec` replaces this process image with the confined target; the surface card fired before this handoff, so the exec itself raises no card."
    )]
    let err = cmd.exec();
    crate::diagnostic::cmd_error("ral", &format!("{program}: {err}"));
    Some(127)
}

/// Off-macOS the `--ral-sandbox-exec` tail is never emitted (Linux
/// expands the host target into the bwrap argv directly; Windows fails
/// closed), so the hook is a no-op that always declines.
#[cfg(not(target_os = "macos"))]
pub fn serve_sandbox_exec(_args: &[String]) -> Option<u8> {
    None
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    // Used only by the per-platform argv-shape tests (Linux / macOS); on
    // other targets the only test is the sentinel-decline check, which
    // needs neither helper.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::types::{FsPolicy, FsProjection, SandboxProjection};

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn restrictive() -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: vec!["/tmp".into()],
                write_prefixes: vec!["/tmp".into()],
                deny_paths: vec!["/etc".into()],
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn argv(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// macOS renders a host launch as a self re-exec carrying the
    /// projection and the `--ral-sandbox-exec` tail: the child enters
    /// Seatbelt in `early_init`, then `execve`s the host program inside it.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_host_command_is_self_reexec_with_sandbox_exec_tail() {
        let projection = restrictive();
        let cmd = macos_sandboxed_command(
            &projection,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), "echo x > /etc/ral_denied".into()],
        )
        .expect("build macOS host command");
        let args = argv(&cmd);
        // ral --sandbox-projection <json> --ral-sandbox-exec /bin/sh -c …
        assert_eq!(args[0], super::super::SANDBOX_PROJECTION_FLAG);
        let decoded: SandboxProjection =
            serde_json::from_str(&args[1]).expect("projection round-trips");
        assert_eq!(decoded, projection);
        assert_eq!(args[2], super::super::SANDBOX_EXEC_FLAG);
        assert_eq!(args[3], "/bin/sh");
        assert_eq!(&args[4..], ["-c", "echo x > /etc/ral_denied"]);
    }

    /// macOS renders a bundled-tool launch with the existing
    /// `--ral-bundled-tool` tail (no `--ral-sandbox-exec`): the tool runs
    /// in-process inside Seatbelt, no further exec needed.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_bundled_tool_command_uses_bundled_tool_tail() {
        let cmd = macos_sandboxed_command(
            &restrictive(),
            LaunchTarget::BundledTool { tool: "ls" },
            &["-l".into()],
        )
        .expect("build macOS bundled command");
        let args = argv(&cmd);
        assert_eq!(args[0], super::super::SANDBOX_PROJECTION_FLAG);
        assert_eq!(args[2], crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG);
        assert_eq!(args[3], "ls");
        assert_eq!(args[4], "-l");
        assert!(
            !args.iter().any(|a| a == super::super::SANDBOX_EXEC_FLAG),
            "bundled tool must not carry the host-exec sentinel"
        );
    }

    /// `serve_sandbox_exec` declines argv that does not open with the
    /// sentinel, so the normal post-`early_init` dispatch continues.
    #[test]
    fn serve_sandbox_exec_declines_without_sentinel() {
        assert_eq!(serve_sandbox_exec(&["echo".into(), "hi".into()]), None);
    }

    /// A `SandboxProjection` whose only writable region is `write_dir`,
    /// readable everywhere `/bin/sh` and bundled tools need (the baseline
    /// `system_paths` reads under `/bin`, `/usr`, `/System`, `/lib`,
    /// `/private/etc`, … are emitted unconditionally by the macOS profile,
    /// so the grant need not re-list them) plus `write_dir` for read.
    /// `net`/`exec` are left wide so the *only* thing the profile denies is
    /// a write outside `write_dir` — the test then fails for that reason
    /// alone, not because the interpreter could not start.
    #[cfg(target_os = "macos")]
    fn write_confined_to(write_dir: &str) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: vec![write_dir.into()],
                write_prefixes: vec![write_dir.into()],
                deny_paths: vec![],
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    /// A process-unique temp directory under the system temp root, created
    /// on the host (outside any sandbox) so the confined child can write
    /// *into* it.  `std::process::id` keeps it unique without `Date.now` /
    /// randomness, per the deterministic-temp rule.
    #[cfg(target_os = "macos")]
    fn unique_workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral_launch_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create work dir");
        dir
    }

    /// Spawn `cmd` confined, wait, and report whether the child exited
    /// successfully.  Stdio is silenced so a denied write's diagnostic does
    /// not pollute the test runner's output.
    #[cfg(target_os = "macos")]
    fn run_confined(mut cmd: Command) -> bool {
        use std::process::Stdio;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn sandboxed child");
        // A one-shot sandboxed test child that cannot be SIGSTOP'd by user
        // code; the WUNTRACED dance of ChildHandle would be pointless here,
        // matching `linux::respawn_under_bwrap` and the pipeline anchor.
        #[allow(clippy::disallowed_methods)]
        let status = child.wait().expect("wait for sandboxed child");
        status.success()
    }

    /// End-to-end per-command denial through the host seam on macOS: a
    /// `LaunchTarget::Host { /bin/sh }` confined to `work` may write a file
    /// *inside* `work` (positive control) but is denied a write *outside*
    /// it — proving Seatbelt enforces the projection rather than failing
    /// blanket.  `/bin/sh` needs no extra read grants: the profile's
    /// baseline system reads cover its dylibs and `/bin/sh` itself.
    #[cfg(target_os = "macos")]
    #[test]
    fn external_denied_write_outside_fs_grant() {
        let work = unique_workdir("ext");
        let work_s = work.to_string_lossy().into_owned();
        let proj = write_confined_to(&work_s);
        let shell = Shell::default();

        // Positive control: a write inside the granted prefix succeeds.
        let allowed = work.join("allowed.txt");
        let allowed_s = allowed.to_string_lossy().into_owned();
        let ok = sandboxed_command(
            &proj,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), format!("echo x > {allowed_s}")],
            &shell,
        )
        .expect("build host command (inside)");
        assert!(run_confined(ok), "write into the write prefix must succeed");
        assert!(allowed.exists(), "in-prefix write should have landed");

        // Denial: a write outside the granted prefix fails, and the file
        // never appears.
        let denied = std::env::temp_dir().join(format!("ral_denied_ext_{}", std::process::id()));
        let _ = std::fs::remove_file(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = sandboxed_command(
            &proj,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), format!("echo x > {denied_s}")],
            &shell,
        )
        .expect("build host command (outside)");
        assert!(
            !run_confined(bad),
            "write outside the write prefix must fail"
        );
        assert!(
            !denied.exists(),
            "out-of-prefix write must not have landed at {denied_s}"
        );

        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_file(&denied);
    }

    /// Same enforcement proof through the bundled-tool seam: `mkdir` run as
    /// a confined `--ral-bundled-tool` child creates a directory inside the
    /// write prefix (positive control) but is denied one outside it.
    #[cfg(all(target_os = "macos", feature = "coreutils"))]
    #[test]
    fn bundled_tool_denied_outside_fs_grant() {
        let work = unique_workdir("bun");
        let work_s = work.to_string_lossy().into_owned();
        let proj = write_confined_to(&work_s);
        let shell = Shell::default();

        // Positive control: mkdir inside the granted prefix succeeds.
        let allowed = work.join("sub");
        let allowed_s = allowed.to_string_lossy().into_owned();
        let ok = sandboxed_command(
            &proj,
            LaunchTarget::BundledTool { tool: "mkdir" },
            &[allowed_s],
            &shell,
        )
        .expect("build bundled command (inside)");
        assert!(run_confined(ok), "mkdir into the write prefix must succeed");
        assert!(allowed.is_dir(), "in-prefix mkdir should have landed");

        // Denial: mkdir outside the granted prefix fails, dir never appears.
        let denied = std::env::temp_dir().join(format!("ral_denied_bun_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = sandboxed_command(
            &proj,
            LaunchTarget::BundledTool { tool: "mkdir" },
            std::slice::from_ref(&denied_s),
            &shell,
        )
        .expect("build bundled command (outside)");
        assert!(
            !run_confined(bad),
            "mkdir outside the write prefix must fail"
        );
        assert!(
            !denied.exists(),
            "out-of-prefix mkdir must not have landed at {denied_s}"
        );

        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&denied);
    }

    /// Linux expands a host launch directly into the bwrap argv with the
    /// in-sandbox cwd requested via `--chdir`, ending in `-- /bin/sh …`.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_host_command_is_bwrap_with_chdir_and_target_tail() {
        let shell = Shell::default();
        let cmd = sandboxed_command(
            &restrictive(),
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), "echo x > /etc/ral_denied".into()],
            &shell,
        )
        .expect("build Linux host command");
        assert_eq!(cmd.get_program().to_string_lossy(), "bwrap");
        let args = argv(&cmd);
        let chdir = args
            .iter()
            .position(|a| a == "--chdir")
            .expect("--chdir present");
        assert_eq!(args[chdir + 1], shell.cwd().to_string_lossy());
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("bwrap -- separator");
        assert!(chdir < sep, "--chdir must precede the -- separator");
        assert_eq!(args[sep + 1], "/bin/sh");
        assert_eq!(&args[sep + 2..], ["-c", "echo x > /etc/ral_denied"]);
    }
}
