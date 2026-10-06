//! Confining one external or bundled child under the platform's OS sandbox.
//! `build_launch` in `runtime::command::process` routes here whenever a
//! projection is active and no guest jail already confines the child.
//!
//! Env, cwd and resource limits are the caller's, layered on the returned
//! launch exactly as on the unsandboxed path so the two cannot drift.  Linux
//! is the exception: bwrap ignores the launcher's `current_dir` and starts the
//! child in its namespace root, so the logical cwd rides the argv as `--chdir`.
//!
//! macOS and Linux share one trampoline: the payload is *ral*, as `ral
//! --warrant`, and the warrant it reads off a descriptor names both the
//! confinement — macOS's Seatbelt profile, compiled here; Linux's count of the
//! Landlock exec admits opened here in the host, which the payload inherits —
//! and the program it becomes once inside ([`serve_warrant`]).  On Linux the
//! trampoline runs inside the bwrap envelope, which is what makes the Landlock
//! layer possible at all: a domain handling any fs right forbids `mount(2)`,
//! bwrap's first act.  Windows has no trampoline — its `LowBox` token is
//! applied at the parent's spawn.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::reexec::Pinned;
#[cfg(target_os = "macos")]
use super::warrant::Seatbelt;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::warrant::{Handoff, Native, Slot, Warrant};
use crate::capability::Admitted;
use crate::types::{Break, Error, Settled, Shell};
use std::ffi::OsString;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{os::fd::AsFd, os::fd::OwnedFd, process::Command};

/// Whether the session keeps owning what it launches.  Read only by the Linux
/// backend, which decides two ties between the session and the envelope:
/// death (bwrap's `--die-with-parent`) and address (`--info-fd`, naming the
/// payload's own session — see `linux::InfoFd`).
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

/// Build a [`Launch`](crate::process::Launch) running the admitted program and
/// its arguments under the sandbox for `projection`; the caller has already
/// cleared [`super::projection_enforceable`] and applies env, cwd and limits
/// after.
///
/// `shell` is taken only for the logical cwd Linux folds in as `--chdir`, and
/// `ownership` likewise reaches Linux alone, the one backend that builds an
/// envelope process to tie to us.
///
/// macOS and Linux both launch the `ral --warrant` trampoline — Linux's under
/// bwrap — while Windows spawns the program itself, confined by the token the
/// parent stamps on it.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        unused_variables,
        reason = "shell.cwd() and ownership are consumed by the bwrap argv alone"
    )
)]
pub(crate) fn sandboxed_command(
    projection: &crate::types::SandboxProjection,
    admitted: &Admitted,
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
        let (cmd, info_fd) = linux_sandboxed_command(projection, admitted, ownership, shell)?;
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
        macos_sandboxed_command(projection, admitted).map(crate::process::Launch::from_command)
    }
    #[cfg(windows)]
    {
        windows_sandboxed_command(projection, admitted, cancel)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (projection, admitted);
        Err(Break::Error(Error::new(
            "no per-command sandbox backend on this platform",
            1,
        )))
    }
}

/// Windows: the parent applies the `LowBox` token at spawn time
/// (`super::windows::session::confine`), so there is no re-exec into the
/// sandbox as on macOS/Linux.  A bundled tool is therefore a plain
/// `ral --ral-bundled-tool <tool> …` self-placement carrying no warrant: a
/// Windows child has nothing to enter, and [`serve_warrant`] refuses one.
#[cfg(windows)]
fn windows_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    admitted: &Admitted,
    cancel: &crate::process::cancel::CancelScope,
) -> Settled<crate::process::Launch> {
    use crate::capability::Program;
    let args = admitted.args();
    // The LowBox token reads only the ALL APPLICATION PACKAGES system paths,
    // so a user-installed image needs `session::confine` to stamp its path RO,
    // mirroring the Linux backend binding the program path into the bwrap argv.
    let (mut launch, image): (crate::process::Launch, Option<std::path::PathBuf>) =
        match admitted.program() {
            Program::File { path, real } => {
                let mut launch = crate::process::Launch::new(real.as_path());
                launch.arg0(path);
                launch.args(args);
                (launch, Some(real.as_path().to_path_buf()))
            }
            Program::Tool(tool) => {
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
                let image = match super::reexec::OWN.get() {
                    Some(own) => Some(own.exec_path().to_path_buf()),
                    None => std::env::current_exe().ok(),
                };
                (launch, image)
            }
        };
    super::windows::session::confine(&mut launch, projection, image.as_deref(), cancel)?;
    Ok(launch)
}

/// Issue the warrant for `admitted` under `confinement`, parcelled for
/// [`Slot::Warrant`].  The one point both Unix backends pass on their way to
/// re-exec `own` by its on-disk name, so the anti-swap guard belongs here: a
/// build swapped in since boot (a mid-session `cargo install`) would
/// otherwise run under our confinement.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn issue(own: &Pinned, confinement: Native, admitted: &Admitted) -> Settled<OwnedFd> {
    own.verify().map_err(Break::Error)?;
    Warrant::new(confinement, admitted)
        .parcel()
        .map_err(refused)
}

/// A launch that could not be built: nothing ran.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn refused(why: String) -> Break {
    Break::Error(Error::new(why, 1))
}

/// Linux: the payload is always the trampoline, launched under this host's
/// pinned bwrap.
#[cfg(target_os = "linux")]
fn linux_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    admitted: &Admitted,
    ownership: Ownership,
    shell: &Shell,
) -> Settled<(Command, Option<super::linux::InfoFd>)> {
    let envelope = super::linux::envelope()
        .map_err(|why| Break::Error(super::confinement_unavailable(why)))?;
    let host = super::linux::HostEnvelope::probe(envelope);
    let cwd = shell.cwd();
    let cwd = faithful(&cwd, "the working directory")?;
    enveloped(envelope, host, projection, admitted, Some(cwd), ownership)
}

/// `path` as bwrap's argv can carry it: every other name there is a
/// `Rendered`, which refuses what is not Unicode rather than approximating it.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_debug_formatting)]
fn faithful<'a>(path: &'a std::path::Path, what: &str) -> Settled<&'a str> {
    path.to_str().ok_or_else(|| {
        refused(format!(
            "sandbox: {what} {path:?} is not valid UTF-8, and bwrap cannot be given it faithfully"
        ))
    })
}

/// The trampoline under `envelope`, handed its warrant and the Landlock
/// admits.  It re-execs *us* through the on-disk `arg0`, not the fd-pinned
/// `/proc/self/fd/N` exec path: bwrap mounts a fresh `/proc`, where that
/// target would neither bind nor resolve.
#[cfg(target_os = "linux")]
pub(super) fn enveloped(
    envelope: &Pinned,
    host: super::linux::HostEnvelope,
    projection: &crate::types::SandboxProjection,
    admitted: &Admitted,
    chdir: Option<&str>,
    ownership: Ownership,
) -> Settled<(Command, Option<super::linux::InfoFd>)> {
    let own = super::reexec::own().map_err(refused)?;
    let program = faithful(own.arg0(), "ral's own path")?;
    let (exec_admits, admits) =
        super::linux::landlock::open_admits(projection, host.landlock).map_err(Break::Error)?;
    let warrant = issue(own, exec_admits, admitted)?;
    let mut handoff = Handoff::default();
    handoff.lend(Slot::Warrant, warrant.as_fd());
    super::linux::landlock::lend(&admits, &mut handoff);
    let image = match admitted.program() {
        crate::capability::Program::File { path, .. } => {
            Some(faithful(path, "the program's path")?)
        }
        crate::capability::Program::Tool(_) => None,
    };
    super::linux::bwrap_command(
        envelope,
        super::linux::Payload {
            program,
            args: &[super::WARRANT_FLAG],
            image,
        },
        handoff,
        projection,
        chdir,
        ownership,
        host,
    )
    .map_err(refused)
}

/// macOS: re-exec the pinned ral `issue` verified, with the compiled profile
/// in its warrant, so the child enters Seatbelt and only then becomes the
/// target.
#[cfg(target_os = "macos")]
fn macos_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    admitted: &Admitted,
) -> Settled<Command> {
    let own = super::reexec::own().map_err(refused)?;
    let profile = super::macos::build_profile(projection).map_err(refused)?;
    let warrant = issue(own, Seatbelt(profile), admitted)?;
    let mut cmd = own.command();
    cmd.arg(super::WARRANT_FLAG);
    let mut handoff = Handoff::default();
    handoff.lend(Slot::Warrant, warrant.as_fd());
    handoff.install(&mut cmd).map_err(refused)?;
    Ok(cmd)
}

/// Serve `ral --warrant`, which takes no arguments: `extra` is what it was
/// wrongly given.
///
/// Take the warrant the launch left at [`Slot::Warrant`], enter its
/// confinement — Seatbelt on macOS, the Landlock layer inside the bwrap
/// envelope on Linux — close the rest of the handoff, and only then become its
/// program.
///
/// Nothing runs unconfined: any failure before the program starts is 126, and
/// so is a host program `execve` refuses for any reason but its absence, which
/// is 127 — POSIX's two codes, as the in-process spawn assigns them.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn serve_warrant(extra: &[OsString]) -> u8 {
    let confined = if extra.is_empty() {
        Warrant::confine()
    } else {
        Err(format!(
            "{} takes no arguments: its warrant arrives on fd {}",
            super::WARRANT_FLAG,
            Slot::Warrant.fd()
        ))
    };
    match confined {
        Ok(confined) => confined.run(),
        Err(e) => {
            crate::diagnostic::cmd_error("ral", &e);
            126
        }
    }
}

/// Windows confines at the parent's spawn, so a child asking to confine
/// itself is a regression to the Unix shape, or forged, and is refused
/// rather than run unconfined.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn serve_warrant(_extra: &[OsString]) -> u8 {
    crate::diagnostic::cmd_error(
        "ral",
        &format!(
            "{} is not served on {}: confinement is applied to a child from outside, \
             never by the child itself; refusing to run unconfined",
            super::WARRANT_FLAG,
            std::env::consts::OS
        ),
    );
    126
}

/// What an unrestricted in-process guard admits, for a test that builds a launch without
/// a dispatch.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(super) fn admitted(program: crate::capability::Program, args: &[String]) -> Admitted {
    Shell::default()
        .check_exec("test", program, args.to_vec())
        .expect("an unrestricted shell admits everything")
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn sh() -> Program {
        Program::file("/bin/sh".into()).expect("/bin/sh exists")
    }

    // Needed only by the per-platform launch tests; elsewhere the two
    // refusals are the only tests, and they need no scaffolding.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::{
        capability::Program,
        types::{FsProjection, FsRules, SandboxProjection},
    };

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

    /// Whatever the child is to run crosses in its warrant, so its argv names
    /// only the trampoline asking for one.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_launch_is_the_warrant_trampoline_alone() {
        for program in [sh(), Program::Tool("ls".into())] {
            let cmd = macos_sandboxed_command(&restrictive(), &admitted(program, &["-l".into()]))
                .expect("build macOS command");
            assert_eq!(argv(&cmd), [super::super::WARRANT_FLAG]);
        }
    }

    /// Refused before any descriptor is touched, so it is safe in-process.
    #[test]
    fn a_warrant_flag_with_arguments_runs_nothing() {
        assert_eq!(serve_warrant(&["sh".into()]), 126);
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
            &admitted(sh(), &["-c".into(), format!("echo x > {allowed_s}")]),
        )
        .expect("build host command (inside)");
        assert!(run_confined(ok), "write into the write prefix must succeed");
        assert!(allowed.exists(), "in-prefix write should have landed");

        let denied = std::env::temp_dir().join(format!("ral_denied_ext_{}", std::process::id()));
        let _ = std::fs::remove_file(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = macos_sandboxed_command(
            &proj,
            &admitted(sh(), &["-c".into(), format!("echo x > {denied_s}")]),
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

    /// An executable that writes its name into the file it is given.
    #[cfg(target_os = "macos")]
    fn plant(dir: &std::path::Path, who: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(who);
        std::fs::write(&path, format!("#!/bin/sh\necho {who} > \"$1\"\n")).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// `build` was judged a link to `a`, and is a link to `b` by the time the
    /// confined child starts: the child runs `a`, the file the warrant judged.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_confined_launch_runs_the_file_judged_not_what_its_link_names_later() {
        let work = unique_workdir("retarget");
        let (a, b) = (plant(&work, "a"), plant(&work, "b"));
        let link = work.join("build");
        std::os::unix::fs::symlink(&a, &link).expect("link to a");
        let judged = Program::file(link.clone()).expect("the link names a program");
        std::fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&b, &link).expect("retarget to b");

        let mark = work.join("who");
        let cmd = macos_sandboxed_command(
            &write_confined_to(&work.to_string_lossy()),
            &admitted(judged, &[mark.to_string_lossy().into_owned()]),
        )
        .expect("build host command");
        assert!(run_confined(cmd), "the judged file must run");
        assert_eq!(
            std::fs::read_to_string(&mark).expect("the program wrote its name"),
            "a\n"
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    /// The program runs by its real path and is told the spelling it was
    /// named by: `sh` prints its `argv[0]` as `$0` when given no other name.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_confined_program_sees_the_spelling_it_was_named_by_as_argv0() {
        let work = unique_workdir("argv0");
        let link = work.join("build");
        std::os::unix::fs::symlink("/bin/sh", &link).expect("link to sh");
        let mark = work.join("argv0");
        let cmd = macos_sandboxed_command(
            &write_confined_to(&work.to_string_lossy()),
            &admitted(
                Program::file(link.clone()).expect("the link names a program"),
                &[
                    "-c".into(),
                    format!("printf %s \"$0\" > {}", mark.display()),
                ],
            ),
        )
        .expect("build host command");
        assert!(run_confined(cmd), "the program must run");
        assert_eq!(
            std::fs::read_to_string(&mark).expect("the program wrote its name"),
            link.to_string_lossy()
        );

        let _ = std::fs::remove_dir_all(&work);
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
            &admitted(Program::Tool("mkdir".into()), &[allowed_s]),
        )
        .expect("build bundled command (inside)");
        assert!(run_confined(ok), "mkdir into the write prefix must succeed");
        assert!(allowed.is_dir(), "in-prefix mkdir should have landed");

        let denied = std::env::temp_dir().join(format!("ral_denied_bun_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = macos_sandboxed_command(
            &proj,
            &admitted(
                Program::Tool("mkdir".into()),
                std::slice::from_ref(&denied_s),
            ),
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

    /// `ps` shows the envelope taking its options from a slot and the
    /// trampoline asking for its warrant, for a host program and a bundled
    /// tool alike: what either is to run crosses on descriptors.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_launch_is_the_pinned_envelope_running_the_warrant_trampoline() {
        super::super::linux::pin_envelope();
        if super::super::linux::envelope().is_err() {
            eprintln!("skipping: this host has no bwrap to pin");
            return;
        }
        let own = super::super::reexec::own().expect("ral pins itself");
        let own = own.arg0().to_string_lossy().into_owned();
        let slot = Slot::Args.fd().to_string();
        for program in [sh(), Program::Tool("ls".into())] {
            let (cmd, _info_fd) = linux_sandboxed_command(
                &restrictive(),
                &admitted(program, &["-l".into()]),
                Ownership::Kept,
                &Shell::default(),
            )
            .expect("build Linux command");
            assert!(
                cmd.get_program()
                    .to_string_lossy()
                    .starts_with("/proc/self/fd/"),
                "the launcher is the fd-pinned envelope, never a name PATH resolves: {:?}",
                cmd.get_program()
            );
            assert_eq!(
                argv(&cmd),
                [
                    "--args",
                    slot.as_str(),
                    "--",
                    own.as_str(),
                    super::super::WARRANT_FLAG
                ]
            );
        }
    }
}
