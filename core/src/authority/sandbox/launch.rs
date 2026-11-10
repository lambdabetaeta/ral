//! What a launch runs ([`Image`]), and confining one external or bundled
//! child under the platform's OS sandbox. `Command::prepare` in
//! `shell::exec::command` routes here whenever a projection is
//! active and no guest jail already confines the child.
//!
//! Env, cwd and resource limits are the caller's, layered on the returned
//! launch exactly as on the unsandboxed path so the two cannot drift.  Linux
//! is the exception: bwrap ignores the launcher's `current_dir` and starts the
//! child in its namespace root, so the logical cwd rides the argv as `--chdir`.
//!
//! macOS and Linux share one trampoline: the payload is *ral*, as `ral
//! --warrant <w> <r>` (plus Linux envelope stream slots), and the warrant
//! it reads off slot `w` names both the
//! confinement — macOS's Seatbelt profile, compiled here; Linux's promise of
//! the Landlock ruleset built here in the host, which the payload inherits —
//! and the image it runs once inside (`serve_warrant`); slot `r` is the
//! report socket it says what became of the program on.  On Linux the
//! trampoline runs inside the bwrap envelope, which is what makes the Landlock
//! layer possible at all: a domain handling any fs right forbids `mount(2)`,
//! bwrap's first act.  Windows has no trampoline — its `LowBox` token is
//! applied at the parent's spawn.

use super::SandboxProjection;
#[cfg(target_os = "linux")]
use super::linux::Envelope;
#[cfg(unix)]
use super::warrant::{Confinement as WarrantConfinement, Handoff, Slots, Warrant};
use crate::Role;
use crate::authority::policy::{Admitted, Baseline, GrantStack, Program};
use crate::error::Error;
use crate::path::{CwdPath, RealPath};
#[cfg(target_os = "linux")]
use crate::process::launch::EnvelopedLaunch;
#[cfg(unix)]
use crate::process::report::Report;
use std::borrow::Cow;
use std::collections::HashSet;
#[cfg(unix)]
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ownership {
    Kept,
    #[cfg(unix)]
    Surrendered,
}

/// The selected confinement mechanism. A guest jail replaces, rather than
/// supplements, the host OS sandbox.
pub(crate) enum Confinement {
    Unconfined,
    Projected(Box<SandboxProjection>),
    #[cfg(seccomp)]
    Guest(&'static crate::process::jail::GuestJail),
}

pub(crate) enum ProgramLaunch {
    Direct(crate::process::Launch),
    #[cfg(target_os = "linux")]
    Enveloped(EnvelopedLaunch),
}

impl ProgramLaunch {
    pub(crate) fn launch_mut(&mut self) -> &mut crate::process::Launch {
        match self {
            Self::Direct(launch) => launch,
            #[cfg(target_os = "linux")]
            Self::Enveloped(launch) => launch.launch_mut(),
        }
    }

    pub(crate) fn spawn(
        self,
        placement: crate::process::Placement,
        scope: &crate::process::Scope,
    ) -> Result<crate::process::Spawned<crate::process::Group>, crate::process::Unstarted> {
        match self {
            Self::Direct(launch) => launch.spawn(placement, scope),
            #[cfg(target_os = "linux")]
            Self::Enveloped(launch) => launch.spawn(placement, scope),
        }
    }

    #[cfg(unix)]
    pub(crate) fn spawn_detached(
        self,
        scope: &crate::process::Scope,
    ) -> Result<crate::process::group::Survivor, crate::process::Unstarted> {
        match self {
            Self::Direct(launch) => launch.spawn_detached(scope),
            #[cfg(target_os = "linux")]
            Self::Enveloped(launch) => launch.spawn_detached(scope),
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn output(self) -> Result<std::process::Output, crate::process::Unstarted> {
        match self {
            Self::Direct(launch) => launch.output(),
            #[cfg(target_os = "linux")]
            Self::Enveloped(launch) => launch.output(),
        }
    }
}

impl Confinement {
    pub(crate) fn of(grants: &GrantStack, baseline: &Baseline, admitted: &Admitted) -> Self {
        // Guests disable unprivileged user namespaces, so they cannot nest bwrap.
        #[cfg(seccomp)]
        if let Some(jail) = crate::process::jail::guest() {
            return Self::Guest(jail);
        }
        SandboxProjection::of(grants, baseline, Some(admitted))
            .map_or(Self::Unconfined, |projection| {
                Self::Projected(Box::new(projection))
            })
    }

    pub(crate) fn is_confined(&self) -> bool {
        !matches!(self, Self::Unconfined)
    }

    /// Build the launch, to which the caller adds environment, limits and any
    /// guest jail, in any order.
    pub(crate) fn launch(
        &self,
        image: &Image,
        args: &[Cow<'_, str>],
        ownership: Ownership,
        cwd: &CwdPath,
    ) -> Result<ProgramLaunch, Error> {
        match self {
            Self::Projected(projection) => {
                sandboxed_command(projection, image, args, ownership, cwd)
            }
            Self::Unconfined => image.launch(args).map(ProgramLaunch::Direct),
            #[cfg(seccomp)]
            Self::Guest(_) => image.launch(args).map(ProgramLaunch::Direct),
        }
    }

    #[cfg(seccomp)]
    pub(crate) fn apply_guest_jail(
        &self,
        launch: &mut crate::process::Launch,
        ownership: Ownership,
        net: Option<crate::authority::policy::Net>,
    ) -> Result<(), Error> {
        if let Self::Guest(jail) = self {
            let plan = jail
                .plan(ownership, net)
                .map_err(crate::process::jail::refused)?;
            launch.apply_guest_jail(&plan)?;
        }
        Ok(())
    }

    /// The hint for a script whose `#!` `interpreter` this sandbox withholds.
    #[cfg_attr(
        not(unix),
        allow(unused_variables, reason = "Windows programs name no interpreter")
    )]
    pub(crate) fn interpreter_hint(&self, interpreter: &Path) -> Option<String> {
        match self {
            #[cfg(unix)]
            Self::Projected(projection) => super::interpreter_hint(projection, interpreter),
            _ => None,
        }
    }

    /// The PIDs to look for in the kernel's sandbox-denial log, or `None`
    /// when nothing confines `spawned`: the child's, or behind an envelope
    /// the payload's host PIDs. Taken at spawn, while the processes live.
    pub(crate) fn denial_pids(
        &self,
        spawned: &crate::process::Spawned<crate::process::Group>,
    ) -> Option<HashSet<u32>> {
        if !self.is_confined() {
            return None;
        }
        #[cfg(target_os = "linux")]
        if let Some(payload) = spawned.group.enveloped() {
            return Some(super::payload_pids(payload));
        }
        Some(HashSet::from([spawned.child.id()]))
    }
}

/// What a launch runs: a host file by the real path the guard judged, under
/// the spelling the user wrote, or a bundled tool, run as ral itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Image {
    File { real: RealPath, arg0: PathBuf },
    Tool(&'static str),
}

impl From<Admitted> for Image {
    /// Consume the admission evidence, releasing its handles before launch.
    fn from(admitted: Admitted) -> Self {
        match admitted.into_program() {
            Program::File { arg0, located } => Self::File {
                real: located.into_real(),
                arg0,
            },
            Program::Tool(tool) => Self::Tool(tool),
        }
    }
}

impl Image {
    /// A snapshot for tests that reuse one admitted program across launches.
    #[cfg(all(test, unix))]
    pub(crate) fn of(admitted: &Admitted) -> Self {
        match admitted.program() {
            Program::File { arg0, located } => Self::File {
                real: located.real().clone(),
                arg0: arg0.clone(),
            },
            Program::Tool(tool) => Self::Tool(tool),
        }
    }

    /// The plain launch of this image with `args`: unconfined, or for
    /// Windows to confine at the spawn.
    pub(crate) fn launch(&self, args: &[Cow<'_, str>]) -> Result<crate::process::Launch, Error> {
        let mut launch = match self {
            Self::File { real, arg0 } => {
                let mut launch = crate::process::Launch::new(real.as_path());
                launch.arg0(arg0);
                launch
            }
            Self::Tool(tool) => {
                let mut launch = super::reexec::launch(Role::BundledTool).map_err(|e| {
                    Error::new(format!(
                        "bundled tool '{tool}': cannot resolve ral's own program: {e}"
                    ))
                })?;
                launch.arg(tool);
                launch
            }
        };
        launch.args(args.iter().map(AsRef::as_ref));
        Ok(launch)
    }
}

/// The `#!` interpreter of the admitted file, read through its judged
/// handle: the parent's alone, to word a failure to start it.
#[cfg_attr(
    windows,
    allow(unused_variables, reason = "no Windows file names an interpreter")
)]
pub(crate) fn interpreter_of(admitted: &Admitted) -> Option<PathBuf> {
    #[cfg(unix)]
    if let Program::File { located, .. } = admitted.program() {
        return located.interpreter();
    }
    None
}

/// Describe a [`Launch`](crate::process::Launch) running `image` and its
/// arguments under the sandbox for `projection`; the caller applies env, cwd
/// and limits after.  Building only describes: it waits on nothing a stop
/// must end, so it can only be refused.  Its one wait is Linux's bwrap host
/// probe, run once per process.  Windows' stamps and the wait for a
/// trampoline's first word are the spawn's, under the scope it runs in.
///
/// `cwd` is the logical cwd Linux folds in as `--chdir`, and `ownership`
/// likewise reaches Linux alone, the one backend that builds an
/// envelope process to tie to us.
///
/// macOS and Linux both launch the `ral --warrant` trampoline — Linux's under
/// bwrap — while Windows spawns the program itself, confined by the token the
/// spawn stamps on it.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        unused_variables,
        reason = "cwd and ownership are consumed by the bwrap argv alone"
    )
)]
fn sandboxed_command(
    projection: &SandboxProjection,
    image: &Image,
    args: &[Cow<'_, str>],
    ownership: Ownership,
    cwd: &CwdPath,
) -> Result<ProgramLaunch, Error> {
    #[cfg(target_os = "linux")]
    {
        linux_sandboxed_command(projection, image, args, ownership, cwd)
            .map(ProgramLaunch::Enveloped)
    }
    #[cfg(target_os = "macos")]
    {
        macos_sandboxed_command(projection, image, args).map(ProgramLaunch::Direct)
    }
    #[cfg(windows)]
    {
        // The token loads the image at its real path, a bundled tool's being
        // ral.exe's own; a Windows child has no warrant to enter.
        let loaded = match image {
            Image::File { real, .. } => Some(real.clone()),
            Image::Tool(_) => super::reexec::own_binary().map(|own| own.real().clone()),
        };
        let mut launch = image.launch(args)?;
        launch.confine(crate::process::launch::Confined {
            projection: projection.clone(),
            image: loaded,
        });
        Ok(ProgramLaunch::Direct(launch))
    }
}

/// The warrant lent to `handoff`, and the report socket kept for the child:
/// the trampoline's two slots, and the parent's ends of the report.
#[cfg(unix)]
pub(super) fn issue(
    warrant: &Warrant<'_>,
    handoff: &mut Handoff,
) -> Result<(Slots, Report), Error> {
    let parcel = warrant.parcel().map_err(Error::new)?;
    let warrant_at = handoff.lend(parcel.as_fd()).map_err(Error::new)?;
    let (reader, writer) = crate::process::cloexec_socketpair().map_err(|e| {
        Error::new(format!(
            "sandbox: cannot open the confined launch's report socket: {e}"
        ))
    })?;
    let (kept, report_at) = handoff.keep(writer.as_fd()).map_err(Error::new)?;
    let slots = Slots {
        warrant: warrant_at,
        report: report_at,
        #[cfg(target_os = "linux")]
        streams: None,
    };
    Ok((slots, Report::new(reader, kept)))
}

/// Linux: the payload is always the trampoline, launched under this host's
/// pinned bwrap.
#[cfg(target_os = "linux")]
fn linux_sandboxed_command(
    projection: &SandboxProjection,
    image: &Image,
    args: &[Cow<'_, str>],
    ownership: Ownership,
    cwd: &CwdPath,
) -> Result<EnvelopedLaunch, Error> {
    let bwrap = super::linux::envelope().map_err(super::unavailable)?;
    let own = super::reexec::own().map_err(Error::new)?;
    let env = Envelope::probe(bwrap, own);
    enveloped(&env, projection, image, args, Some(cwd), ownership)
}

/// `env`'s trampoline, handed its warrant and the Landlock ruleset built
/// here, against what the envelope will show.  The trampoline is *us*:
/// our boot pin, which bwrap execs by descriptor.
#[cfg(target_os = "linux")]
pub(super) fn enveloped(
    env: &Envelope<'_>,
    projection: &SandboxProjection,
    image: &Image,
    args: &[Cow<'_, str>],
    chdir: Option<&CwdPath>,
    ownership: Ownership,
) -> Result<EnvelopedLaunch, Error> {
    let rendered = projection.rendered().map_err(Error::new)?;
    let shown = super::linux::Shown::of(&rendered).map_err(Error::new)?;
    let mut handoff = Handoff::default();
    let confinement = super::linux::landlock::build(
        &rendered.exec,
        &rendered.fs,
        &shown,
        env.host.landlock,
        &mut handoff,
    )?;
    let warrant = Warrant::new(WarrantConfinement(confinement), image, args);
    let (slots, report) = issue(&warrant, &mut handoff)?;
    let (cmd, info, stdio) =
        super::linux::bwrap_command(env, shown, handoff, slots, &rendered, chdir, ownership)?;
    Ok(bwrapped(cmd, info, stdio, report))
}

/// bwrap's `cmd` as a launch, with the payload's `--info-fd` peer and the
/// trampoline's report.
#[cfg(target_os = "linux")]
pub(super) fn bwrapped(
    cmd: Command,
    info: Option<super::linux::InfoFd>,
    stdio: crate::process::launch::EnvelopeStdio,
    report: Report,
) -> EnvelopedLaunch {
    EnvelopedLaunch::new(
        cmd,
        stdio,
        // A host package rather than part of ral, so it may simply be absent.
        super::linux::BWRAP,
        info.map(|fd| Box::new(move || fd.payload_pgid()) as _),
        report,
    )
}

/// macOS: re-exec the pinned ral, with the compiled profile in its warrant,
/// so the child enters Seatbelt and only then becomes the target.  The
/// re-exec is by name, so the pin is verified first: a build swapped in
/// since boot (a mid-session `cargo install`) would otherwise run under our
/// confinement.
#[cfg(target_os = "macos")]
fn macos_sandboxed_command(
    projection: &SandboxProjection,
    image: &Image,
    args: &[Cow<'_, str>],
) -> Result<crate::process::Launch, Error> {
    let own = super::reexec::own().map_err(Error::new)?;
    own.verify().map_err(Error::new)?;
    let profile = super::macos::build_profile(projection).map_err(Error::new)?;
    let mut handoff = Handoff::default();
    let (slots, report) = issue(
        &Warrant::new(WarrantConfinement(profile), image, args),
        &mut handoff,
    )?;
    Ok(trampoline(own, handoff, slots, report))
}

/// The pinned ral, summoned to its slots.
#[cfg(target_os = "macos")]
pub(super) fn trampoline(
    own: &super::reexec::Pinned,
    handoff: Handoff,
    slots: Slots,
    report: Report,
) -> crate::process::Launch {
    let mut cmd = own.command();
    cmd.args(slots.argv());
    handoff.install(&mut cmd);
    crate::process::Launch::trampolined(cmd, report)
}

/// What an unrestricted in-process guard admits, for a test that builds a launch without
/// a dispatch.
#[cfg(all(test, unix))]
pub(super) fn admitted(program: crate::authority::policy::Program) -> Admitted {
    crate::authority::policy::GrantStack::root()
        .admit(&Baseline::default(), program)
        .expect("an unrestricted stack admits everything")
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::sandbox::warrant::Slot;

    fn sh() -> Program {
        Program::file("/bin/sh".into()).expect("/bin/sh exists")
    }

    /// What an unrestricted guard admits of `program`, as a launch runs it.
    fn image(program: Program) -> Image {
        Image::of(&admitted(program))
    }

    #[cfg(target_os = "macos")]
    use crate::authority::policy::{ExecAllow, ExecDeny, ExecRules, Rule};
    #[cfg(target_os = "macos")]
    use crate::authority::sandbox::ExecProjection;
    use crate::{
        authority::policy::{FsAllow, FsDeny, GrantPath},
        authority::sandbox::{FsLists, FsProjection},
        path::{Excluded, FrozenPath, Trace},
    };

    fn prefix(path: &str) -> FsAllow {
        FsAllow::Prefix(
            GrantPath::<Trace>::walk(FrozenPath::from_surface(path)).expect("a prefix walks"),
        )
    }

    fn excluding(path: &str) -> FsDeny {
        FsDeny(
            GrantPath::<Excluded>::walk(FrozenPath::from_surface(path))
                .expect("an exclusion walks"),
        )
    }

    fn restrictive() -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                read: vec![prefix("/tmp")],
                write: vec![prefix("/tmp")],
                deny: vec![excluding("/etc")],
                ..FsLists::default()
            }),
            ..SandboxProjection::default()
        }
    }

    fn argv(launch: &crate::process::Launch) -> Vec<String> {
        (launch.command().get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Whatever the child is to run crosses in its warrant, so its argv names
    /// only the trampoline asking for one and the slots its warrant and
    /// report arrive at.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_launch_is_the_warrant_trampoline_alone() {
        for program in [sh(), Program::Tool("ls")] {
            let launch = macos_sandboxed_command(&restrictive(), &image(program), &["-l".into()])
                .expect("build macOS command");
            let args = argv(&launch);
            let [flag, warrant, report] = &args[..] else {
                panic!("not `--warrant <w> <r>`: {args:?}");
            };
            assert_eq!(flag, Role::Warrant.flag());
            for at in [warrant, report] {
                Slot::parse(at).expect("a slot");
            }
        }
    }

    /// Writable only under `write_dir`, with `net`/`exec` left wide so the one
    /// thing the profile denies is a write outside it.  It reads the baseline, as
    /// every restricted region does.
    #[cfg(target_os = "macos")]
    fn write_confined_to(write_dir: &str) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                read: std::iter::once(prefix(write_dir))
                    .chain(baseline())
                    .collect(),
                write: vec![prefix(write_dir)],
                deny: Vec::new(),
                ..FsLists::default()
            }),
            ..SandboxProjection::default()
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

    /// `launch` run to its end, stdin empty and the rest captured, so a
    /// denied write's diagnostic stays out of the test runner's output.
    #[cfg(target_os = "macos")]
    fn confined(
        mut launch: crate::process::Launch,
    ) -> Result<std::process::Output, crate::process::Unstarted> {
        launch.stdin(crate::process::StdioSpec::Null);
        launch.output()
    }

    /// Whether `launch` started and succeeded.
    #[cfg(target_os = "macos")]
    fn run_confined(launch: crate::process::Launch) -> bool {
        confined(launch).is_ok_and(|out| out.status.success())
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
            &image(sh()),
            &["-c".into(), format!("echo x > {allowed_s}").into()],
        )
        .expect("build host command (inside)");
        assert!(run_confined(ok), "write into the write prefix must succeed");
        assert!(allowed.exists(), "in-prefix write should have landed");

        let denied = std::env::temp_dir().join(format!("ral_denied_ext_{}", std::process::id()));
        let _ = std::fs::remove_file(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = macos_sandboxed_command(
            &proj,
            &image(sh()),
            &["-c".into(), format!("echo x > {denied_s}").into()],
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
            &image(judged),
            &[mark.to_string_lossy()],
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
            &image(Program::file(link.clone()).expect("the link names a program")),
            &[
                "-c".into(),
                format!("printf %s \"$0\" > {}", mark.display()).into(),
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
        let ok =
            macos_sandboxed_command(&proj, &image(Program::Tool("mkdir")), &[allowed_s.into()])
                .expect("build bundled command (inside)");
        assert!(run_confined(ok), "mkdir into the write prefix must succeed");
        assert!(allowed.is_dir(), "in-prefix mkdir should have landed");

        let denied = std::env::temp_dir().join(format!("ral_denied_bun_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&denied);
        let denied_s = denied.to_string_lossy();
        let bad = macos_sandboxed_command(
            &proj,
            &image(Program::Tool("mkdir")),
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
            fs: FsProjection::Restricted(FsLists {
                read: scopes(read).into_iter().chain(baseline()).collect(),
                write: scopes(write),
                deny: deny.iter().copied().map(excluding).collect(),
                ..FsLists::default()
            }),
            exec,
            ..SandboxProjection::default()
        }
    }

    /// `every` program, and nothing else.
    #[cfg(target_os = "macos")]
    fn every() -> ExecProjection {
        let every: ExecRules = std::iter::once(Rule::Allow(ExecAllow::Any)).collect();
        ExecProjection::Restricted(every.for_kernel())
    }

    /// A Mach-O of this user's own, ad-hoc signed so that relocating it is
    /// not what kills it.
    #[cfg(target_os = "macos")]
    fn mach_o(dir: &Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::copy("/usr/bin/true", &path).expect("copy a Mach-O");
        let signed = std::process::Command::new("codesign")
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
                &image(sh()),
                &["-c".into(), line.into()],
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
    /// table (`fs_deny` empty) or by the fs (`exec` unrestricted by it): how
    /// its start went, and the hint the shell would add.
    #[cfg(target_os = "macos")]
    fn launch_with_excluded_shell(
        by_exec: bool,
    ) -> (
        Result<std::process::Output, crate::process::Unstarted>,
        Option<String>,
    ) {
        let work = scratch(if by_exec { "excl_exec" } else { "excl_fs" });
        let script = work.join("run");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let projection = if by_exec {
            let shell = GrantPath::<Excluded>::walk(FrozenPath::from_surface("/bin/sh"))
                .expect("/bin/sh walks");
            let exec: ExecRules = [
                Rule::Allow(ExecAllow::Any),
                Rule::Deny(Box::new(ExecDeny::File(shell))),
            ]
            .into_iter()
            .collect();
            confined_to(
                &[&work],
                &[&work],
                &[],
                ExecProjection::Restricted(exec.for_kernel()),
            )
        } else {
            confined_to(&[&work, Path::new("/bin")], &[&work], &["/bin/sh"], every())
        };
        let admitted = admitted(Program::file(script).expect("a script"));
        let interpreter = interpreter_of(&admitted).expect("a script names its interpreter");
        let launch = macos_sandboxed_command(&projection, &Image::of(&admitted), &[])
            .expect("build host command");
        let out = confined(launch);
        let hint = super::super::interpreter_hint(&projection, &interpreter);
        let _ = std::fs::remove_dir_all(&work);
        (out, hint)
    }

    /// The kernel refusing a script's `execve`, as the trampoline reports it.
    #[cfg(target_os = "macos")]
    fn refused(out: &Result<std::process::Output, crate::process::Unstarted>) -> bool {
        matches!(
            out,
            Err(crate::process::Unstarted::Program(e))
                if e.kind() == std::io::ErrorKind::PermissionDenied
        )
    }

    /// No allowance reopens an excluded interpreter, and the hint says which
    /// exclusion it is (§7.1).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_script_whose_interpreter_is_excluded_fails_and_the_hint_names_it() {
        for by_exec in [true, false] {
            let (out, hint) = launch_with_excluded_shell(by_exec);
            assert!(refused(&out), "by exec: {by_exec}; {out:?}");
            let hint = hint.unwrap_or_default();
            assert!(
                hint.contains("is excluded by") && hint.contains("/bin/sh"),
                "by exec: {by_exec}; {hint}"
            );
        }
    }

    /// A script whose interpreter lies outside the read set, exec
    /// unrestricted, is refused by Seatbelt, and the hint names the read that
    /// admits it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_script_whose_interpreter_is_unread_fails_and_the_hint_names_the_read() {
        let (work, lib) = (scratch("unread_work"), scratch("unread_lib"));
        let interp = lib.join("sh");
        std::fs::copy("/bin/sh", &interp).expect("copy /bin/sh");
        let script = work.join("run");
        std::fs::write(&script, format!("#!{}\nexit 0\n", interp.display())).expect("write script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let projection = confined_to(&[&work], &[&work], &[], ExecProjection::Unrestricted);
        let admitted = admitted(Program::file(script).expect("a script"));
        let interpreter = interpreter_of(&admitted).expect("a script names its interpreter");
        let launch = macos_sandboxed_command(&projection, &Image::of(&admitted), &[])
            .expect("build host command");
        let out = confined(launch);
        let hint = super::super::interpreter_hint(&projection, &interpreter).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&lib);
        assert!(refused(&out), "{out:?}");
        assert!(
            hint.contains("lies outside the grant's fs read set")
                && hint.contains("fs: [read: [`absolute ")
                && hint.contains(&interp.display().to_string()),
            "{hint}"
        );
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
        let interpreter = interpreter_of(&admitted).expect("a script names its interpreter");
        let launch = macos_sandboxed_command(&projection, &Image::of(&admitted), &[]).unwrap();
        std::fs::write(&script, "#!/usr/bin/false\n").unwrap();
        let out = confined(launch);
        assert!(refused(&out), "{out:?}");
        let hint = super::super::interpreter_hint(&projection, &interpreter).unwrap_or_default();
        assert!(hint.contains("its interpreter /bin/sh"), "{hint}");
        assert!(!hint.contains("/usr/bin/false"), "{hint}");
    }

    /// A system script whose system interpreter lies elsewhere: no grant of
    /// the script's directory admits the interpreter, only support does.
    #[cfg(target_os = "macos")]
    fn system_script_elsewhere() -> Option<std::path::PathBuf> {
        use super::super::carriers::trusted_real;

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
                && located.interpreter().is_some_and(|interpreter| {
                    trusted_real(&interpreter).is_some() && interpreter.parent() != script.parent()
                })
        })
    }

    /// How the system script's start went under a restricted fs, beside
    /// `exec` as given: it starts, through the live support alone.
    #[cfg(target_os = "macos")]
    fn start_system_script(
        script: &Path,
        exec: Option<crate::authority::policy::ExecGrant>,
    ) -> Result<std::process::Output, crate::process::Unstarted> {
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
        let launch = macos_sandboxed_command(&projection, &Image::of(&admitted), &[])
            .expect("build host command");
        let out = confined(launch);
        let _ = std::fs::remove_dir_all(&work);
        out
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
            let out = start_system_script(&script, exec.clone());
            assert!(
                out.is_ok(),
                "exec {exec:?}: the script did not start: {out:?}"
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
        let used = std::process::Command::new("xcrun")
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
        let launch =
            macos_sandboxed_command(&projection, &Image::of(&admitted), &["--version".into()])
                .expect("build host command");
        let out = confined(launch).expect("the stub starts");
        assert!(out.status.success(), "{out:?}");
    }

    /// The baseline, as read scopes.
    #[cfg(target_os = "macos")]
    fn baseline() -> Vec<FsAllow> {
        host_system_reads().allows().cloned().collect()
    }

    /// `cat file` confined to reading `read` and the baseline, minus `deny`.
    #[cfg(target_os = "macos")]
    fn reads(read: Vec<FsAllow>, deny: Vec<FsDeny>, file: &Path) -> bool {
        let projection = SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                read: read.into_iter().chain(baseline()).collect(),
                write: Vec::new(),
                deny,
                ..FsLists::default()
            }),
            ..SandboxProjection::default()
        };
        let cat = Program::file("/bin/cat".into()).expect("/bin/cat exists");
        let args = [file.to_string_lossy()];
        run_confined(
            macos_sandboxed_command(&projection, &image(cat), &args).expect("build command"),
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
            let mut launch = linux_sandboxed_command(
                &restrictive(),
                &image(program),
                &["-l".into()],
                Ownership::Kept,
                &CwdPath::new("/".into()).expect("absolute cwd"),
            )
            .expect("build Linux command");
            let cmd = launch.launch_mut().command();
            assert!(
                cmd.get_program()
                    .to_string_lossy()
                    .starts_with("/proc/self/fd/"),
                "the launcher is the fd-pinned envelope, never a name PATH resolves: {:?}",
                cmd.get_program()
            );
            let args = argv(launch.launch_mut());
            let [
                opt,
                options,
                dashes,
                trampoline,
                flag,
                warrant,
                report,
                input,
                output,
                error,
            ] = &args[..]
            else {
                panic!("not a bwrap warrant launch with three stream slots: {args:?}");
            };
            assert_eq!(
                [opt.as_str(), dashes, flag],
                ["--args", "--", Role::Warrant.flag()]
            );
            for at in [options, warrant, report, input, output, error] {
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
