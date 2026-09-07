//! Confining one external or bundled child under the platform's OS sandbox.
//! `build_command` in `runtime::command::process` routes here whenever a
//! projection is active and no guest jail already confines the child.
//!
//! Env, cwd and resource limits are the caller's, layered on the returned
//! launch exactly as on the unsandboxed path so the two cannot drift.  Linux
//! is the exception: bwrap ignores the launcher's `current_dir` and starts the
//! child in its namespace root, so the logical cwd rides the argv as `--chdir`.
//!
//! macOS and Linux share one trampoline: the payload is *ral*, carrying the
//! folded projection and the target as its argv tail ([`trampoline_tail`]), so
//! the child enters the process sandbox in `early_init` and only then becomes
//! the target.  On Linux the trampoline runs inside the bwrap envelope, which
//! is what makes the Landlock layer possible at all: a domain handling any fs
//! right forbids `mount(2)`, bwrap's first act.  Windows has no trampoline —
//! its `LowBox` token is applied at the parent's spawn.

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
/// backend, which decides two ties between the session and the envelope:
/// death (bwrap's `--die-with-parent`) and address (`--info-fd`, naming the
/// payload's own session — see `linux::open_info_fd`).
/// `Surrendered` — the `detach` verb, whose child is meant to survive us —
/// drops both: the survivor must not be killed by our death, and there is no
/// session left here to address once we stop watching it.  No hole: the
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
///
/// macOS and Linux both launch the [`trampoline_tail`] re-exec of ral —
/// Linux's under bwrap — while Windows spawns the target itself, confined by
/// the token the parent stamps on it.
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
    cancel: &crate::process::cancel::CancelScope,
) -> Settled<crate::process::Launch> {
    // Only the Windows backend does work here a cancel could interrupt: bwrap
    // and Seatbelt render a profile in memory, while an `AppContainer` stamps
    // the projection's prefixes onto the filesystem before the child exists.
    #[cfg(not(windows))]
    let _ = cancel;
    #[cfg(target_os = "linux")]
    {
        let (cmd, info_fd) = linux_sandboxed_command(projection, target, args, ownership, shell)?;
        let mut launch = crate::process::Launch::from_command(cmd);
        // A host package rather than part of ral, so it may simply be absent.
        launch.envelope(crate::process::launch::Envelope {
            program: super::linux::BWRAP,
            payload_pgid: info_fd.map(|fd| Box::new(move || fd.payload_pgid()) as _),
        });
        Ok(launch)
    }
    #[cfg(target_os = "macos")]
    {
        macos_sandboxed_command(projection, target, args).map(crate::process::Launch::from_command)
    }
    #[cfg(windows)]
    {
        windows_sandboxed_command(projection, target, args, cancel)
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
    cancel: &crate::process::cancel::CancelScope,
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
    super::windows::session::confine(&mut launch, projection, image.as_deref(), cancel)?;
    Ok(launch)
}

/// The argv every confined re-exec of ral carries: the folded projection the
/// child enters in `early_init`, then the target it becomes inside that
/// confinement — a bundled tool run in-process under the `--ral-bundled-tool`
/// tail, a host program `execve`d by [`serve_sandbox_exec`] under the
/// `--ral-sandbox-exec` one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn trampoline_tail(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
) -> Settled<Vec<String>> {
    let json = serde_json::to_string(projection).map_err(|e| {
        Break::Error(Error::new(
            format!("sandbox: failed to encode projection: {e}"),
            1,
        ))
    })?;
    let (sentinel, name) = match target {
        LaunchTarget::BundledTool { tool } => {
            (crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG, tool)
        }
        LaunchTarget::Host { program } => (super::SANDBOX_EXEC_FLAG, program),
    };
    let mut tail = Vec::with_capacity(args.len() + 4);
    tail.push(super::SANDBOX_PROJECTION_FLAG.to_string());
    tail.push(json);
    tail.push(sentinel.to_string());
    tail.push(name.to_string());
    tail.extend_from_slice(args);
    Ok(tail)
}

/// Linux: the payload is always the trampoline, launched under bwrap by
/// `super::linux::make_command_with_policy`.  It re-execs *us* through the
/// on-disk `arg0`, not the fd-pinned `/proc/self/fd/N` exec path: bwrap mounts
/// a fresh `/proc`, where that target would neither bind nor resolve.
#[cfg(target_os = "linux")]
fn linux_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
    ownership: Ownership,
    shell: &Shell,
) -> Settled<(Command, Option<super::linux::InfoFd>)> {
    let envelope = super::linux::envelope()
        .map_err(|why| Break::Error(super::confinement_unavailable(why)))?;
    let host = super::linux::HostEnvelope::probe(envelope);
    if let unprobed @ super::linux::landlock::Landlock::Unprobed(_) = host.landlock {
        return Err(Break::Error(super::confinement_unavailable(
            &unprobed.to_string(),
        )));
    }
    let cwd = shell.cwd().to_string_lossy().into_owned();
    // bwrap execs the trampoline by its on-disk name, where a swap would land.
    if let Some(s) = super::reexec::SANDBOX_SELF.get() {
        super::reexec::verify_unswapped(s).map_err(Break::Error)?;
    }
    let self_path = super::reexec::self_arg0().map_err(|e| {
        Break::Error(Error::new(
            format!("sandbox: cannot resolve self exe for the confined re-exec: {e}"),
            1,
        ))
    })?;
    let tail = trampoline_tail(projection, target, args)?;
    let image = match target {
        LaunchTarget::Host { program } => Some(program),
        LaunchTarget::BundledTool { .. } => None,
    };
    super::linux::make_command_with_policy(
        envelope,
        super::linux::Payload {
            program: &self_path.to_string_lossy(),
            args: &tail,
            image,
        },
        projection,
        Some(cwd.as_str()),
        ownership,
        host,
    )
    .map_err(|e| Break::Error(Error::new(e, 1)))
}

/// macOS: re-exec ral with the trampoline tail, so the child enters Seatbelt
/// in `early_init` and only then becomes the target.
#[cfg(target_os = "macos")]
fn macos_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
) -> Settled<Command> {
    let tail = trampoline_tail(projection, target, args)?;
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
    cmd.args(tail);
    Ok(cmd)
}

/// `execve` the host program carried in the `--ral-sandbox-exec` tail, now
/// that `early_init` has entered the process sandbox — Seatbelt on macOS, the
/// Landlock layer inside the bwrap envelope on Linux.
///
/// `args` is the post-`early_init` argv, and `None` — the sentinel absent —
/// lets normal dispatch continue.  On success `execve` never returns; a
/// failure surfaces as 127, the POSIX "cannot exec" code.
#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// Windows alone emits no tail: it confines at the parent's spawn, so its
/// child is the target already and has nothing to serve.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
        // A one-shot child no user code can SIGSTOP, so routing this wait
        // through the reaper — whose only extra service is answering a
        // stop with SIGCONT — would buy nothing.
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
    fn linux_host_command_is_the_pinned_envelope_with_chdir_and_target_tail() {
        super::super::linux::register_envelope();
        if super::super::linux::envelope().is_err() {
            eprintln!("skipping: this host has no bwrap to pin");
            return;
        }
        let shell = Shell::default();
        let projection = restrictive();
        let (cmd, _info_fd) = linux_sandboxed_command(
            &projection,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), "echo x > /etc/ral_denied".into()],
            Ownership::Kept,
            &shell,
        )
        .expect("build Linux host command");
        assert!(
            cmd.get_program()
                .to_string_lossy()
                .starts_with("/proc/self/fd/"),
            "the launcher is the fd-pinned envelope, never a name PATH resolves: {:?}",
            cmd.get_program()
        );
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
        // ral --sandbox-projection <json> --ral-sandbox-exec /bin/sh -c …
        let self_arg0 = super::super::reexec::self_arg0().expect("own path");
        assert_eq!(args[sep + 1], self_arg0.to_string_lossy());
        assert_eq!(args[sep + 2], super::super::SANDBOX_PROJECTION_FLAG);
        let decoded: SandboxProjection =
            serde_json::from_str(&args[sep + 3]).expect("projection round-trips");
        assert_eq!(decoded, projection);
        assert_eq!(args[sep + 4], super::super::SANDBOX_EXEC_FLAG);
        assert_eq!(args[sep + 5], "/bin/sh");
        assert_eq!(&args[sep + 6..], ["-c", "echo x > /etc/ral_denied"]);
        // The trampoline execs it in turn, so bwrap must let it see the file —
        // under its real name, since bwrap will not mount onto a symlink.
        let real_sh = std::fs::canonicalize("/bin/sh").expect("resolve /bin/sh");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--ro-bind" && w[1] == real_sh.to_string_lossy()),
            "the host image must be bound read-only into the envelope: {args:?}"
        );
    }

    /// The bundled-tool seam takes the same trampoline, differing only in the
    /// sentinel: there is no host binary to `execve` afterwards.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bundled_tool_command_uses_the_bundled_tool_tail() {
        super::super::linux::register_envelope();
        if super::super::linux::envelope().is_err() {
            eprintln!("skipping: this host has no bwrap to pin");
            return;
        }
        let shell = Shell::default();
        let (cmd, _info_fd) = linux_sandboxed_command(
            &restrictive(),
            LaunchTarget::BundledTool { tool: "ls" },
            &["-l".into()],
            Ownership::Kept,
            &shell,
        )
        .expect("build Linux bundled command");
        let args = argv(&cmd);
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("bwrap -- separator");
        assert_eq!(args[sep + 2], super::super::SANDBOX_PROJECTION_FLAG);
        assert_eq!(
            args[sep + 4],
            crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG
        );
        assert_eq!(args[sep + 5], "ls");
        assert_eq!(args[sep + 6], "-l");
        assert!(
            !args.iter().any(|a| a == super::super::SANDBOX_EXEC_FLAG),
            "bundled tool must not carry the host-exec sentinel: {args:?}"
        );
    }
}
