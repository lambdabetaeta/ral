//! Confining one external or bundled child under the platform's OS sandbox.
//! `build_launch` in `shell::exec::command::process` routes here whenever a
//! projection is active and no guest jail already confines the child.
//!
//! Env, cwd and resource limits are the caller's, layered on the returned
//! launch exactly as on the unsandboxed path so the two cannot drift.  Linux
//! is the exception: bwrap ignores the launcher's `current_dir` and starts the
//! child in its namespace root, so the logical cwd rides the argv as `--chdir`.
//!
//! macOS and Linux share one trampoline: the payload is *ral*, as `ral
//! --warrant <fd>`, and the warrant it reads off that descriptor names both
//! the confinement — macOS's Seatbelt profile, compiled here; Linux's promise
//! of the Landlock ruleset built here in the host, which the payload inherits
//! — and the program it becomes once inside ([`serve_warrant`]).  On Linux the
//! trampoline runs inside the bwrap envelope, which is what makes the Landlock
//! layer possible at all: a domain handling any fs right forbids `mount(2)`,
//! bwrap's first act.  Windows has no trampoline — its `LowBox` token is
//! applied at the parent's spawn.

#[cfg(target_os = "linux")]
use super::linux::Envelope;
#[cfg(target_os = "macos")]
use super::warrant::Seatbelt;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::warrant::{Handoff, Native, Slot, Warrant};
use super::{Refusal, SandboxProjection};
use crate::Role;
use crate::authority::policy::{Admitted, Baseline};
use crate::path::CwdPath;
use std::ffi::OsString;
#[cfg(all(test, target_os = "macos"))]
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{os::fd::AsFd, process::Command};

/// Whether the session keeps owning what it launches.  Read by the Linux
/// backend, which decides two ties between the session and the envelope:
/// death (bwrap's `--die-with-parent`) and address (`--info-fd`, naming the
/// payload's own session — see `linux::InfoFd`); and by the guest jail, whose
/// cgroup for a survivor is not its engine's, so the engine's end spares it.
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
/// `cwd` is the logical cwd Linux folds in as `--chdir`, and `ownership`
/// likewise reaches Linux alone, the one backend that builds an
/// envelope process to tie to us; `baseline`, which of the projection's read
/// objects a host may keep from this user, is Linux's alone too.
///
/// macOS and Linux both launch the `ral --warrant` trampoline — Linux's under
/// bwrap — while Windows spawns the program itself, confined by the token the
/// parent stamps on it.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        unused_variables,
        reason = "baseline, cwd and ownership are consumed by the bwrap argv alone"
    )
)]
pub(crate) fn sandboxed_command(
    projection: &SandboxProjection,
    baseline: &Baseline,
    admitted: &Admitted,
    args: &[String],
    ownership: Ownership,
    cwd: &CwdPath,
    cancel: &crate::process::cancel::Scope,
) -> Result<crate::process::Launch, Refusal> {
    // Only the Windows backend does work here a cancel could interrupt: bwrap
    // and Seatbelt render a profile in memory, while an `AppContainer` stamps
    // the projection's prefixes onto the filesystem before the child exists.
    #[cfg(not(windows))]
    let _ = cancel;
    #[cfg(target_os = "linux")]
    {
        let (cmd, info_fd) =
            linux_sandboxed_command(projection, baseline, admitted, args, ownership, cwd)?;
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
        macos_sandboxed_command(projection, admitted, args)
            .map(crate::process::Launch::from_command)
    }
    #[cfg(windows)]
    {
        windows_sandboxed_command(projection, admitted, args, cancel)
    }
}

/// Windows: the parent applies the `LowBox` token at spawn time
/// (`super::windows::session::confine`), so there is no re-exec into the
/// sandbox as on macOS/Linux.  A bundled tool is therefore a plain
/// `ral --ral-bundled-tool <tool> …` self-placement carrying no warrant: a
/// Windows child has nothing to enter, and [`serve_warrant`] refuses one.
#[cfg(windows)]
fn windows_sandboxed_command(
    projection: &SandboxProjection,
    admitted: &Admitted,
    args: &[String],
    cancel: &crate::process::cancel::Scope,
) -> Result<crate::process::Launch, Refusal> {
    use crate::authority::policy::Program;
    // The LowBox token reads only the ALL APPLICATION PACKAGES system paths,
    // so a user-installed image needs `session::confine` to stamp its path RO,
    // mirroring the Linux backend binding the program path into the bwrap argv.
    let (mut launch, image): (crate::process::Launch, Option<crate::path::RealPath>) =
        match admitted.program() {
            Program::File { arg0, located } => {
                let mut launch = crate::process::Launch::new(located.real().as_path());
                launch.arg0(arg0);
                launch.args(args);
                (launch, Some(located.real().clone()))
            }
            Program::Tool(tool) => {
                let mut launch = super::reexec::launch(Role::BundledTool).map_err(|e| {
                    Refusal::Launch(format!(
                        "sandbox: cannot resolve self exe for bundled tool '{tool}': {e}"
                    ))
                })?;
                launch.arg(tool);
                launch.args(args);
                // The confined child is ral.exe itself; the token must load
                // it, at its real path, since a link on the path refuses.
                let image = super::reexec::own_binary().map(|own| own.real().clone());
                (launch, image)
            }
        };
    super::windows::session::confine(&mut launch, projection, image.as_ref(), cancel)?;
    Ok(launch)
}

/// Issue the warrant for `admitted` under `confinement`, lent to `handoff`;
/// `projection` is read for its exclusions, to diagnose.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn issue(
    confinement: Native,
    admitted: &Admitted,
    args: &[String],
    projection: &SandboxProjection,
    handoff: &mut Handoff,
) -> Result<Slot, Refusal> {
    let parcel = Warrant::new(confinement, admitted, args, projection).parcel()?;
    Ok(handoff.lend(parcel.as_fd())?)
}

/// Linux: the payload is always the trampoline, launched under this host's
/// pinned bwrap.
#[cfg(target_os = "linux")]
fn linux_sandboxed_command(
    projection: &SandboxProjection,
    baseline: &Baseline,
    admitted: &Admitted,
    args: &[String],
    ownership: Ownership,
    cwd: &CwdPath,
) -> Result<(Command, Option<super::linux::InfoFd>), Refusal> {
    let bwrap = super::linux::envelope().map_err(|why| Refusal::Unavailable(why.into()))?;
    let own = super::reexec::own()?;
    let env = Envelope::probe(bwrap, own);
    enveloped(
        &env,
        projection,
        baseline,
        admitted,
        args,
        Some(cwd),
        ownership,
    )
}

/// `env`'s trampoline, handed its warrant and the Landlock ruleset built
/// here, against the binds the envelope will show.  The trampoline is *us*:
/// our boot pin, which bwrap execs by descriptor.
#[cfg(target_os = "linux")]
pub(super) fn enveloped(
    env: &Envelope<'_>,
    projection: &SandboxProjection,
    baseline: &Baseline,
    admitted: &Admitted,
    args: &[String],
    chdir: Option<&CwdPath>,
    ownership: Ownership,
) -> Result<(Command, Option<super::linux::InfoFd>), Refusal> {
    let rendered = projection.rendered()?;
    let shown = super::linux::Shown::of(&rendered, baseline)?;
    let mut handoff = Handoff::default();
    let confinement = super::linux::landlock::build(
        &rendered.exec,
        &rendered.fs,
        &shown.roots,
        env.host.landlock,
        &mut handoff,
    )?;
    let warrant = issue(confinement, admitted, args, projection, &mut handoff)?;
    super::linux::bwrap_command(env, shown, handoff, warrant, &rendered, chdir, ownership)
}

/// macOS: re-exec the pinned ral, with the compiled profile in its warrant,
/// so the child enters Seatbelt and only then becomes the target.  The
/// re-exec is by name, so the pin is verified first: a build swapped in
/// since boot (a mid-session `cargo install`) would otherwise run under our
/// confinement.
#[cfg(target_os = "macos")]
fn macos_sandboxed_command(
    projection: &SandboxProjection,
    admitted: &Admitted,
    args: &[String],
) -> Result<Command, Refusal> {
    let own = super::reexec::own()?;
    own.verify()?;
    let profile = super::macos::build_profile(projection)?;
    let mut handoff = Handoff::default();
    let warrant = issue(Seatbelt(profile), admitted, args, projection, &mut handoff)?;
    let mut cmd = own.command();
    cmd.arg(Role::Warrant.flag()).arg(warrant.to_string());
    handoff.install(&mut cmd);
    Ok(cmd)
}

/// Serve `ral --warrant <fd>`: `args` must be that one slot.
///
/// Take the warrant the launch left there, enter its
/// confinement — Seatbelt on macOS, the Landlock layer inside the bwrap
/// envelope on Linux; sweep every descriptor above stderr, then become its
/// program.
///
/// Nothing runs unconfined: any failure before the program starts is 126, and
/// so is a host program `execve` refuses for any reason but its absence, which
/// is 127 — POSIX's two codes, as the in-process spawn assigns them.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn serve_warrant(args: &[OsString]) -> u8 {
    let confined = match args {
        [at] => Slot::parse(&crate::path::shown(at)).and_then(Warrant::confine),
        _ => Err(format!(
            "{} takes one argument, the descriptor its warrant arrives on: it is how \
             ral starts its own confined children, never a command to run by hand",
            Role::Warrant.flag()
        )),
    };
    match confined {
        Ok(confined) => confined.run(),
        Err(e) => {
            crate::io::terminal::cmd_error("ral", &e);
            126
        }
    }
}

/// Windows confines at the parent's spawn, so a child asking to confine
/// itself is a regression to the Unix shape, or forged, and is refused
/// rather than run unconfined.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn serve_warrant(_args: &[OsString]) -> u8 {
    crate::io::terminal::cmd_error(
        "ral",
        &format!(
            "{} is not served on {}: confinement is applied to a child from outside, \
             never by the child itself; refusing to run unconfined",
            Role::Warrant.flag(),
            std::env::consts::OS
        ),
    );
    126
}

/// What an unrestricted in-process guard admits, for a test that builds a launch without
/// a dispatch.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(super) fn admitted(program: crate::authority::policy::Program) -> Admitted {
    crate::authority::policy::GrantStack::root()
        .admit(&Baseline::default(), program)
        .expect("an unrestricted stack admits everything")
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn sh() -> Program {
        Program::file("/bin/sh".into()).expect("/bin/sh exists")
    }

    // Needed only by the per-platform launch tests; elsewhere the two
    // refusals are the only tests, and they need no scaffolding.
    #[cfg(target_os = "macos")]
    use crate::authority::policy::{ExecAllow, ExecDeny, Rule};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::{
        authority::policy::{FsAllow, GrantPath, Program},
        authority::sandbox::{ExecProjection, FsProjection, FsRules},
        path::{Excluded, FrozenPath, Trace},
    };

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn prefix(path: &str) -> FsAllow {
        FsAllow::Prefix(
            GrantPath::<Trace>::walk(FrozenPath::from_surface(path)).expect("a prefix walks"),
        )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn excluding(path: &str) -> GrantPath<Excluded> {
        GrantPath::<Excluded>::walk(FrozenPath::from_surface(path)).expect("an exclusion walks")
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn restrictive() -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read: vec![prefix("/tmp")],
                write: vec![prefix("/tmp")],
                deny: vec![excluding("/etc")],
                pinned_dirs: Vec::new(),
            }),
            net: true,
            gpu: true,
            exec: ExecProjection::default(),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn argv(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Whatever the child is to run crosses in its warrant, so its argv names
    /// only the trampoline asking for one and the slot it arrives at.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_launch_is_the_warrant_trampoline_alone() {
        for program in [sh(), Program::Tool("ls")] {
            let cmd = macos_sandboxed_command(&restrictive(), &admitted(program), &["-l".into()])
                .expect("build macOS command");
            let args = argv(&cmd);
            let [flag, at] = &args[..] else {
                panic!("not `--warrant <w>`: {args:?}");
            };
            assert_eq!(flag, Role::Warrant.flag());
            Slot::parse(at).expect("the warrant's slot");
        }
    }

    /// Refused before any descriptor is touched, so it is safe in-process:
    /// the trampoline takes one argument, a slot.
    #[test]
    fn a_warrant_flag_naming_no_slot_runs_nothing() {
        for args in [
            &[][..],
            &["sh"],
            &["97"],
            &["+101"],
            &["0101"],
            &["101", "sh"],
        ] {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(serve_warrant(&args), 126, "{args:?}");
        }
    }

    /// Writable only under `write_dir`, with `net`/`exec` left wide so the one
    /// thing the profile denies is a write outside it.  It reads the baseline, as
    /// every restricted region does.
    #[cfg(target_os = "macos")]
    fn write_confined_to(write_dir: &str) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read: std::iter::once(prefix(write_dir))
                    .chain(baseline())
                    .collect(),
                write: vec![prefix(write_dir)],
                deny: Vec::new(),
                pinned_dirs: Vec::new(),
            }),
            net: true,
            gpu: true,
            exec: ExecProjection::default(),
        }
    }

    /// The host's baseline, which a confined child needs to start at all.
    #[cfg(target_os = "macos")]
    fn host_system_reads() -> Baseline {
        crate::test_helper::core_runtime().baseline().clone()
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
            &admitted(sh()),
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
            &admitted(sh()),
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
            &admitted(judged),
            &[mark.to_string_lossy().into_owned()],
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
            &admitted(Program::file(link.clone()).expect("the link names a program")),
            &[
                "-c".into(),
                format!("printf %s \"$0\" > {}", mark.display()),
            ],
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
        let ok = macos_sandboxed_command(&proj, &admitted(Program::Tool("mkdir")), &[allowed_s])
            .expect("build bundled command (inside)");
        assert!(run_confined(ok), "mkdir into the write prefix must succeed");
        assert!(allowed.is_dir(), "in-prefix mkdir should have landed");

        let denied = std::env::temp_dir().join(format!("ral_denied_bun_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = macos_sandboxed_command(
            &proj,
            &admitted(Program::Tool("mkdir")),
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

    /// A fresh work directory under its canonical name, which is what the
    /// kernel matches and what a frozen allowance holds.
    #[cfg(target_os = "macos")]
    fn scratch(tag: &str) -> std::path::PathBuf {
        std::fs::canonicalize(unique_workdir(tag)).expect("canonical work dir")
    }

    /// Reads `read` and the baseline, as every restricted region does, and writes
    /// `write`, with `deny` carved out of both and `exec` as given.
    #[cfg(target_os = "macos")]
    fn confined_to(
        read: &[&Path],
        write: &[&Path],
        deny: &[&str],
        exec: ExecProjection,
    ) -> SandboxProjection {
        let scopes = |dirs: &[&Path]| {
            dirs.iter()
                .map(|d| prefix(&d.to_string_lossy()))
                .collect::<Vec<_>>()
        };
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read: scopes(read).into_iter().chain(baseline()).collect(),
                write: scopes(write),
                deny: deny.iter().copied().map(excluding).collect(),
                pinned_dirs: Vec::new(),
            }),
            net: true,
            gpu: true,
            exec,
        }
    }

    /// `every` program, and nothing else.
    #[cfg(target_os = "macos")]
    fn every() -> ExecProjection {
        ExecProjection::Restricted(std::iter::once(Rule::Allow(ExecAllow::Any)).collect())
    }

    /// The exit code and stderr of `cmd` run confined, stdout silenced.
    #[cfg(target_os = "macos")]
    fn confined_stderr(mut cmd: Command) -> (Option<i32>, String) {
        use std::process::Stdio;
        let out = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sandboxed child")
            .wait_with_output()
            .expect("wait for sandboxed child");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// A Mach-O of this user's own, ad-hoc signed so that relocating it is
    /// not what kills it.
    #[cfg(target_os = "macos")]
    fn mach_o(dir: &Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::copy("/usr/bin/true", &path).expect("copy a Mach-O");
        let signed = Command::new("codesign")
            .args(["--force", "-s", "-"])
            .arg(&path)
            .status()
            .expect("run codesign");
        assert!(signed.success(), "ad-hoc signing {}", path.display());
        path
    }

    /// Law 7 (§7.4): Seatbelt starts no program the fs rules do not let it
    /// read, so one that runs only through `every` is no run place for a
    /// descendant unless the fs reads it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_descendant_starts_no_program_every_admits_and_the_fs_does_not_read() {
        let (bin, mark) = (scratch("law7_bin"), scratch("law7_mark"));
        let (prog, ran) = (mach_o(&bin, "prog"), mark.join("ran"));
        let starts = |read: &[&Path]| {
            let _ = std::fs::remove_file(&ran);
            let line = format!("{} && echo > {}", prog.display(), ran.display());
            let cmd = macos_sandboxed_command(
                &confined_to(read, &[&mark], &[], every()),
                &admitted(sh()),
                &["-c".into(), line],
            )
            .expect("build host command");
            run_confined(cmd);
            ran.exists()
        };
        assert!(starts(&[&mark, &bin]), "control: the fs reads the program");
        assert!(
            !starts(&[&mark]),
            "a Mach-O the fs does not read started under `every`"
        );
        let _ = std::fs::remove_dir_all(&bin);
        let _ = std::fs::remove_dir_all(&mark);
    }

    /// A `#!/bin/sh` script, launched with `/bin/sh` excluded by the exec
    /// table (`fs_deny` empty) or by the fs (`exec` unrestricted by it).
    #[cfg(target_os = "macos")]
    fn launch_with_excluded_shell(by_exec: bool) -> (Option<i32>, String) {
        let work = scratch(if by_exec { "excl_exec" } else { "excl_fs" });
        let script = work.join("run");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let projection = if by_exec {
            let shell = GrantPath::<Excluded>::walk(FrozenPath::from_surface("/bin/sh"))
                .expect("/bin/sh walks");
            let exec = [
                Rule::Allow(ExecAllow::Any),
                Rule::Deny(Box::new(ExecDeny::File(shell))),
            ];
            confined_to(
                &[&work],
                &[&work],
                &[],
                ExecProjection::Restricted(exec.into_iter().collect()),
            )
        } else {
            confined_to(&[&work, Path::new("/bin")], &[&work], &["/bin/sh"], every())
        };
        let cmd = macos_sandboxed_command(
            &projection,
            &admitted(Program::file(script).expect("a script")),
            &[],
        )
        .expect("build host command");
        let out = confined_stderr(cmd);
        let _ = std::fs::remove_dir_all(&work);
        out
    }

    /// No allowance reopens an excluded interpreter, and the hint says which
    /// exclusion it is (§7.1).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_script_whose_interpreter_is_excluded_fails_and_the_hint_names_it() {
        for by_exec in [true, false] {
            let (code, stderr) = launch_with_excluded_shell(by_exec);
            assert_eq!(code, Some(126), "by exec: {by_exec}; {stderr}");
            assert!(
                stderr.contains("is excluded by") && stderr.contains("/bin/sh"),
                "by exec: {by_exec}; {stderr}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_refused_exec_hint_keeps_the_interpreter_read_before_spawn() {
        use crate::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
        use crate::test_helper::{layer, stack};
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("run");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let grants = stack([layer(Capabilities {
            exec: Some(ExecGrant::of(
                [ExecKey::Path(FrozenPath::freeze(&script).unwrap())],
                [],
            )),
            fs: Some(FsPolicy {
                read_prefixes: vec![FrozenPath::freeze(tmp.path()).unwrap()],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        })]);
        let baseline = host_system_reads();
        let admitted = grants
            .admit(&baseline, Program::file(script.clone()).unwrap())
            .unwrap();
        let projection = SandboxProjection::of(&grants, &baseline, Some(&admitted)).unwrap();
        let cmd = macos_sandboxed_command(&projection, &admitted, &[]).unwrap();
        std::fs::write(&script, "#!/usr/bin/false\n").unwrap();
        let (code, stderr) = confined_stderr(cmd);
        assert_eq!(code, Some(126), "{stderr}");
        assert!(stderr.contains("its interpreter /bin/sh"), "{stderr}");
        assert!(!stderr.contains("/usr/bin/false"), "{stderr}");
    }

    /// A system script whose system interpreter lies elsewhere: no grant of
    /// the script's directory admits the interpreter, only support does.
    #[cfg(target_os = "macos")]
    fn system_script_elsewhere() -> Option<std::path::PathBuf> {
        use super::super::carriers::{launched_shebang, trusted_real};

        let scripts = std::fs::read_dir("/usr/bin").ok()?;
        scripts.flatten().map(|e| e.path()).find(|script| {
            let Some(program) = Program::file(script.clone()) else {
                return false;
            };
            let admitted = admitted(program);
            let Program::File { located, .. } = admitted.program() else {
                return false;
            };
            trusted_real(script).is_some()
                && launched_shebang(located).is_some_and(|interpreter| {
                    trusted_real(&interpreter).is_some() && interpreter.parent() != script.parent()
                })
        })
    }

    /// The exit code and stderr of the system script under a restricted fs,
    /// beside `exec` as given: it starts, through the live support alone.
    #[cfg(target_os = "macos")]
    fn start_system_script(
        script: &Path,
        exec: Option<crate::authority::policy::ExecGrant>,
    ) -> String {
        use crate::authority::policy::{Capabilities, FsPolicy};
        use crate::test_helper::{layer, stack};
        let work = scratch("support");
        let caps = Capabilities {
            exec,
            fs: Some(FsPolicy {
                read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&work).unwrap())],
                write_prefixes: Vec::new(),
                deny_paths: Vec::new(),
            }),
            ..Capabilities::default()
        };
        let stack = stack([layer(caps)]);
        let baseline = host_system_reads();
        let program = Program::file(script.to_path_buf()).expect("a program");
        let admitted = stack
            .admit(&baseline, program)
            .expect("the grant admits the script");
        let projection = SandboxProjection::of(&stack, &baseline, Some(&admitted))
            .expect("a restricted fs sandboxes");
        let cmd = macos_sandboxed_command(&projection, &admitted, &[]).expect("build host command");
        let (_, stderr) = confined_stderr(cmd);
        let _ = std::fs::remove_dir_all(&work);
        stderr
    }

    /// A trusted script starts through its interpreter's support, whether the
    /// grant supplies `exec` (admitting the script's directory alone) or
    /// omits it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_trusted_script_starts_through_its_interpreters_support() {
        use crate::authority::policy::{ExecGrant, ExecKey};
        let Some(script) = system_script_elsewhere() else {
            eprintln!("skipping: no system script under /usr/bin");
            return;
        };
        let dir = FrozenPath::from_surface(
            crate::path::utf8(script.parent().expect("a directory")).expect("a Unicode path"),
        );
        let supplied = ExecGrant::of([ExecKey::Dir(dir)], []);
        for exec in [Some(supplied), None] {
            let stderr = start_system_script(&script, exec.clone());
            assert!(
                !stderr.contains(&format!("{}:", script.display())),
                "exec {exec:?}: the script did not start: {stderr}"
            );
        }
    }

    /// `/usr/bin/git`'s stub forwards to one of two candidates (§7.4); the
    /// one `xcrun` does not name may be excluded without refusing the stub.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_excluded_shim_candidate_the_stub_does_not_use_refuses_nothing() {
        use crate::authority::policy::{Capabilities, ExecGrant, ExecKey};
        use crate::test_helper::{layer, stack};
        let stub = Path::new("/usr/bin/git");
        let used = Command::new("xcrun")
            .args(["--find", "git"])
            .output()
            .ok()
            .and_then(|o| std::fs::canonicalize(String::from_utf8(o.stdout).ok()?.trim()).ok());
        let unused = used.and_then(|used| {
            super::super::carriers::shims(crate::test_helper::trace(stub).real())
                .into_iter()
                .filter_map(|c| std::fs::canonicalize(c).ok())
                .find(|c| *c != used)
        });
        let Some(unused) = unused else {
            eprintln!("skipping: the stub has one candidate here");
            return;
        };
        let freeze = |p: &Path| FrozenPath::freeze(p).expect("a Unicode path");
        let exec = ExecGrant::of(
            [ExecKey::Dir(freeze(Path::new("/usr/bin")))],
            [ExecKey::Path(freeze(&unused))],
        );
        let caps = Capabilities {
            exec: Some(exec),
            ..Capabilities::default()
        };
        let stack = stack([layer(caps)]);
        let baseline = host_system_reads();
        let admitted = stack
            .admit(&baseline, Program::file(stub.into()).expect("git stub"))
            .expect("the grant admits the stub");
        let projection =
            SandboxProjection::of(&stack, &baseline, Some(&admitted)).expect("exec is restricted");
        let cmd = macos_sandboxed_command(&projection, &admitted, &["--version".into()])
            .expect("build host command");
        let (code, stderr) = confined_stderr(cmd);
        assert_eq!(code, Some(0), "{stderr}");
    }

    /// The baseline, as read scopes.
    #[cfg(target_os = "macos")]
    fn baseline() -> Vec<FsAllow> {
        host_system_reads().allows().cloned().collect()
    }

    /// `cat file` confined to reading `read` and the baseline, minus `deny`.
    #[cfg(target_os = "macos")]
    fn reads(read: Vec<FsAllow>, deny: Vec<GrantPath<Excluded>>, file: &Path) -> bool {
        let projection = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read: read.into_iter().chain(baseline()).collect(),
                write: Vec::new(),
                deny,
                pinned_dirs: Vec::new(),
            }),
            net: true,
            gpu: true,
            exec: ExecProjection::default(),
        };
        let cat = Program::file("/bin/cat".into()).expect("/bin/cat exists");
        let args = [file.to_string_lossy().into_owned()];
        run_confined(
            macos_sandboxed_command(&projection, &admitted(cat), &args).expect("build command"),
        )
    }

    /// Two directories holding a file `f`, and a link `link` to the first.
    #[cfg(target_os = "macos")]
    fn linked_dirs(
        tag: &str,
    ) -> (
        std::path::PathBuf,
        [std::path::PathBuf; 2],
        std::path::PathBuf,
    ) {
        let root = scratch(tag);
        let dirs = ["a", "b"].map(|d| {
            let dir = root.join(d);
            std::fs::create_dir(&dir).expect("dir");
            std::fs::write(dir.join("f"), d).expect("file");
            dir
        });
        let link = root.join("link");
        std::os::unix::fs::symlink(&dirs[0], &link).expect("link");
        (root, dirs, link)
    }

    /// An allowance renders as the real path it froze (§7.3): a spelling that
    /// becomes a real directory, or a link retargeted, reaches nothing new.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_allowance_through_a_respelled_link_reaches_nothing_beneath_the_new_name() {
        let (root, [a, b], link) = linked_dirs("respell");
        let held = prefix(&link.to_string_lossy());
        let frozen = || vec![held.clone()];
        assert!(reads(frozen(), Vec::new(), &link.join("f")), "control");

        std::fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&b, &link).expect("retarget");
        assert!(
            !reads(frozen(), Vec::new(), &link.join("f")),
            "the redirect reached b"
        );
        assert!(
            reads(frozen(), Vec::new(), &a.join("f")),
            "the frozen target stays"
        );

        std::fs::remove_file(&link).expect("unlink");
        std::fs::create_dir(&link).expect("a real directory");
        std::fs::write(link.join("f"), "new").expect("file");
        assert!(
            !reads(frozen(), Vec::new(), &link.join("f")),
            "the new directory was reached"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An exclusion follows its name to wherever the name leads when the
    /// grant is made.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_exclusion_follows_a_retargeted_spelling() {
        let (root, [a, b], link) = linked_dirs("exclusion");
        std::fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&b, &link).expect("retarget");
        let whole = || vec![prefix(&root.to_string_lossy())];
        let deny = || vec![excluding(&link.to_string_lossy())];
        assert!(reads(whole(), Vec::new(), &b.join("f")), "control");
        assert!(
            !reads(whole(), deny(), &b.join("f")),
            "the new spelling's target stayed readable"
        );
        assert!(
            reads(whole(), deny(), &a.join("f")),
            "the old target is no longer excluded"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `ps` shows the envelope taking its options from a slot and running
    /// the trampoline from another, asking for its warrant, for a host
    /// program and a bundled tool alike: what either is to run crosses on
    /// descriptors, each by the slot it was lent at.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_launch_is_the_pinned_envelope_running_the_warrant_trampoline() {
        super::super::linux::pin_envelope();
        if super::super::linux::envelope().is_err() {
            eprintln!("skipping: this host has no bwrap to pin");
            return;
        }
        for program in [sh(), Program::Tool("ls")] {
            let (cmd, _info_fd) = linux_sandboxed_command(
                &restrictive(),
                &Baseline::default(),
                &admitted(program),
                &["-l".into()],
                Ownership::Kept,
                &CwdPath::new("/".into()).expect("absolute cwd"),
            )
            .expect("build Linux command");
            assert!(
                cmd.get_program()
                    .to_string_lossy()
                    .starts_with("/proc/self/fd/"),
                "the launcher is the fd-pinned envelope, never a name PATH resolves: {:?}",
                cmd.get_program()
            );
            let args = argv(&cmd);
            let [opt, options, dashes, trampoline, flag, warrant] = &args[..] else {
                panic!("not `bwrap --args <a> -- /proc/self/fd/<t> --warrant <w>`: {args:?}");
            };
            assert_eq!(
                [opt.as_str(), dashes, flag],
                ["--args", "--", Role::Warrant.flag()]
            );
            for at in [options, warrant] {
                Slot::parse(at).expect("a slot");
            }
            assert!(
                (trampoline.strip_prefix("/proc/self/fd/"))
                    .is_some_and(|at| Slot::parse(at).is_ok()),
                "{args:?}"
            );
        }
    }
}
