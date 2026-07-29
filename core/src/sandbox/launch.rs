//! Confining one external or bundled child under the platform's OS sandbox.
//! `build_command` in `runtime::command::process` routes here whenever a
//! projection is active and no guest jail already confines the child.
//!
//! Env, cwd and resource limits are the caller's, layered on the returned
//! launch exactly as on the unsandboxed path so the two cannot drift.  Linux
//! is the exception: bwrap ignores the launcher's `current_dir` and starts the
//! child in its namespace root, so the logical cwd rides the argv as `--chdir`.

use crate::types::{Break, Error, Settled, Shell};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

/// What to run under the sandbox: a host program by resolved path or bare
/// name, or a bundled tool as a child placement of ral itself
/// (`ral --ral-bundled-tool <tool> …`).
#[derive(Clone, Copy)]
pub(crate) enum LaunchTarget<'a> {
    Host { program: &'a str },
    BundledTool { tool: &'a str },
}

/// Whether the session keeps owning what it launches.  Read only by the Linux
/// backend: `Kept` adds bwrap's `--die-with-parent`, so an envelope orphaned
/// by a crash cannot outlive the session that authorised it, while
/// `Surrendered` — the `detach` verb, whose child is meant to survive us —
/// must drop that flag or the survivor dies moments after birth.  No hole: the
/// confinement holds for the survivor's whole life, frozen as the frame that
/// birthed it left it.  macOS `execve`s the target in place and Windows has no
/// `detach`, so neither has an envelope to tie to a parent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    Kept,
    #[cfg(unix)]
    Surrendered,
}

/// Build a [`Launch`](crate::process::Launch) running `target` + `args` under
/// the sandbox for `projection`; the caller has already cleared
/// [`super::projection_enforceable`] and applies env, cwd and limits after.
///
/// `shell` is taken only for the logical cwd Linux folds in as `--chdir`, and
/// `ownership` likewise reaches Linux alone, the one backend that builds an
/// envelope process to tie to us.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        unused_variables,
        reason = "shell.cwd() and ownership are consumed by the bwrap argv alone"
    )
)]
pub(crate) fn sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
    ownership: Ownership,
    shell: &Shell,
) -> Settled<crate::process::Launch> {
    #[cfg(target_os = "linux")]
    {
        linux_sandboxed_command(projection, target, args, ownership, shell)
            .map(crate::process::Launch::from_command)
    }
    #[cfg(target_os = "macos")]
    {
        macos_sandboxed_command(projection, target, args).map(crate::process::Launch::from_command)
    }
    #[cfg(windows)]
    {
        windows_sandboxed_command(projection, target, args)
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

/// Windows: the parent applies the `LowBox` token at spawn time
/// (`super::windows::session::confine`), so there is no re-exec into the
/// sandbox as on macOS/Linux.  A bundled tool is therefore a plain
/// `ral --ral-bundled-tool <tool> …` self-placement carrying no
/// `--sandbox-projection`: a Windows child has nothing to enter, and
/// `super::reexec::maybe_enter_process_sandbox` fails closed if it sees one.
#[cfg(windows)]
fn windows_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
) -> Settled<crate::process::Launch> {
    // The LowBox token reads only the ALL APPLICATION PACKAGES system paths,
    // so a user-installed image needs `session::confine` to stamp its path RO,
    // mirroring the Linux backend binding the program path into the bwrap argv.
    let (mut launch, image): (crate::process::Launch, Option<std::path::PathBuf>) = match target {
        LaunchTarget::Host { program } => {
            let mut launch = crate::process::Launch::new(program);
            launch.args(args);
            // A bare name resolves on PATH inside the loader and is not a path
            // we can stamp, so its readability rests on the fs read projection
            // or the ALL APPLICATION PACKAGES grants instead.
            let image =
                crate::path::is_absolute(program).then(|| std::path::PathBuf::from(program));
            (launch, image)
        }
        LaunchTarget::BundledTool { tool } => {
            use crate::runtime::pipeline::helper::{BUNDLED_TOOL_FLAG, self_reexec};
            let mut launch = self_reexec(BUNDLED_TOOL_FLAG).map_err(|e| {
                Break::Error(Error::new(
                    format!("sandbox: cannot resolve self exe for bundled tool '{tool}': {e}"),
                    1,
                ))
            })?;
            launch.arg(tool);
            launch.args(args);
            // The confined child is ral.exe itself; the token must load it.
            let image = super::reexec::self_exec_path();
            (launch, image)
        }
    };
    super::windows::session::confine(&mut launch, projection, image.as_deref())?;
    Ok(launch)
}

/// Linux: expand the target into the bwrap argv via
/// `super::linux::make_command_with_policy`, which binds an absolute program
/// path RO so bwrap can exec it.  A bundled tool re-execs *us* through the
/// on-disk `arg0`, not the fd-pinned `/proc/self/fd/N` exec path: bwrap mounts
/// a fresh `/proc`, where that target would neither bind nor resolve.
#[cfg(target_os = "linux")]
fn linux_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
    ownership: Ownership,
    shell: &Shell,
) -> Settled<Command> {
    let cwd = shell.cwd().to_string_lossy().into_owned();
    match target {
        LaunchTarget::Host { program } => super::linux::make_command_with_policy(
            program,
            args,
            projection,
            Some(cwd.as_str()),
            ownership,
        )
        .map_err(|e| Break::Error(Error::new(e, 1))),
        LaunchTarget::BundledTool { tool } => {
            let self_path = super::reexec::self_arg0().map_err(|e| {
                Break::Error(Error::new(
                    format!("sandbox: cannot resolve self exe for bundled tool: {e}"),
                    1,
                ))
            })?;
            let mut tool_args = Vec::with_capacity(args.len() + 2);
            tool_args.push(crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG.to_string());
            tool_args.push(tool.to_string());
            tool_args.extend_from_slice(args);
            super::linux::make_command_with_policy(
                &self_path.to_string_lossy(),
                &tool_args,
                projection,
                Some(cwd.as_str()),
                ownership,
            )
            .map_err(|e| Break::Error(Error::new(e, 1)))
        }
    }
}

/// macOS: re-exec ral with the folded projection so the child enters Seatbelt
/// in `early_init`, then runs the target *inside* it — a bundled tool
/// in-process under the `--ral-bundled-tool` tail, a host program by the
/// `execve` `serve_sandbox_exec` performs under the `--ral-sandbox-exec` one.
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
    // Refuse to re-exec a pinned executable swapped on disk since boot (a
    // mid-session `cargo install`), which would launch a foreign build.
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

/// macOS: `execve` the host program carried in the `--ral-sandbox-exec` tail,
/// now that `early_init` has entered Seatbelt.
///
/// `args` is the post-`early_init` argv, and `None` — the sentinel absent —
/// lets normal dispatch continue.  On success `execve` never returns; a
/// failure surfaces as 127, the POSIX "cannot exec" code.
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

/// Off-macOS nothing emits the tail: Linux expands the host target into the
/// bwrap argv and Windows confines at the parent's spawn, so a child never
/// re-enters and has nothing to serve.
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

    // Needed only by the per-platform argv-shape tests; elsewhere the sentinel
    // decline is the only test, and it needs no scaffolding.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::types::{FsProjection, FsRules, SandboxProjection};

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn restrictive() -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: vec!["/tmp".into()],
                write_prefixes: vec!["/tmp".into()],
                deny_paths: vec!["/etc".into()],
                pinned_dirs: Vec::new(),
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

    #[test]
    fn serve_sandbox_exec_declines_without_sentinel() {
        assert_eq!(serve_sandbox_exec(&["echo".into(), "hi".into()]), None);
    }

    /// Writable only under `write_dir`, with `net`/`exec` left wide so the one
    /// thing the profile denies is a write outside it.  The system reads
    /// `/bin/sh` and the bundled tools need come from the macOS profile's
    /// unconditional baseline, so the grant need not re-list them.
    #[cfg(target_os = "macos")]
    fn write_confined_to(write_dir: &str) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: vec![write_dir.to_string()],
                write_prefixes: vec![write_dir.to_string()],
                deny_paths: Vec::new(),
                pinned_dirs: Vec::new(),
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    /// Created on the host, outside any sandbox, so a confined child can write
    /// *into* it; the pid keeps it unique without randomness.
    #[cfg(target_os = "macos")]
    fn unique_workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral_launch_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create work dir");
        dir
    }

    /// Spawn `cmd` confined and report success; stdio is silenced so a denied
    /// write's diagnostic stays out of the test runner's output.
    #[cfg(target_os = "macos")]
    fn run_confined(mut cmd: Command) -> bool {
        use std::process::Stdio;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn sandboxed child");
        // A one-shot child no user code can SIGSTOP, so `ChildHandle`'s
        // WUNTRACED dance would buy nothing.
        #[allow(clippy::disallowed_methods)]
        let status = child.wait().expect("wait for sandboxed child");
        status.success()
    }

    /// The in-prefix write is what makes the denial meaningful: it shows
    /// Seatbelt enforcing the projection rather than the child failing to
    /// start at all.
    #[cfg(target_os = "macos")]
    #[test]
    fn external_denied_write_outside_fs_grant() {
        let work = unique_workdir("ext");
        let work_s = work.to_string_lossy().into_owned();
        let proj = write_confined_to(&work_s);

        let allowed = work.join("allowed.txt");
        let allowed_s = allowed.to_string_lossy().into_owned();
        let ok = macos_sandboxed_command(
            &proj,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), format!("echo x > {allowed_s}")],
        )
        .expect("build host command (inside)");
        assert!(run_confined(ok), "write into the write prefix must succeed");
        assert!(allowed.exists(), "in-prefix write should have landed");

        let denied = std::env::temp_dir().join(format!("ral_denied_ext_{}", std::process::id()));
        let _ = std::fs::remove_file(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = macos_sandboxed_command(
            &proj,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), format!("echo x > {denied_s}")],
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

    /// The same proof through the bundled-tool seam.
    #[cfg(all(target_os = "macos", feature = "coreutils"))]
    #[test]
    fn bundled_tool_denied_outside_fs_grant() {
        let work = unique_workdir("bun");
        let work_s = work.to_string_lossy().into_owned();
        let proj = write_confined_to(&work_s);

        let allowed = work.join("sub");
        let allowed_s = allowed.to_string_lossy().into_owned();
        let ok = macos_sandboxed_command(
            &proj,
            LaunchTarget::BundledTool { tool: "mkdir" },
            &[allowed_s],
        )
        .expect("build bundled command (inside)");
        assert!(run_confined(ok), "mkdir into the write prefix must succeed");
        assert!(allowed.is_dir(), "in-prefix mkdir should have landed");

        let denied = std::env::temp_dir().join(format!("ral_denied_bun_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = macos_sandboxed_command(
            &proj,
            LaunchTarget::BundledTool { tool: "mkdir" },
            std::slice::from_ref(&denied_s),
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_host_command_is_bwrap_with_chdir_and_target_tail() {
        let shell = Shell::default();
        let cmd = linux_sandboxed_command(
            &restrictive(),
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), "echo x > /etc/ral_denied".into()],
            Ownership::Kept,
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
