//! The owned process-launch value, built once and handed over once.
//!
//! Preparation owns the choices and resources; `ChildSetup::install` fixes
//! their child-side order; spawning releases child-only descriptors before
//! hearing the payload. No configuration method registers a launch-owned hook.
//!
//! Unix lowers it back to `std::process::Command`; Windows drives
//! `CreateProcessW` itself, because handle admission and Job Object
//! membership must be creation-time facts.

use crate::environ::ChildEnv;
use crate::error::Error;
use crate::fact::{
    LaunchFailure, LauncherFailure, PayloadGroupFailure, PrepareCause, PrepareFailure, PrepareStep,
    PrepareSubject, Reason,
};
#[cfg(unix)]
use crate::fact::{LauncherRole, OsFailure};
#[cfg(unix)]
pub(crate) mod setup;
#[cfg(unix)]
use crate::process::site::Surface;
#[cfg(unix)]
use crate::site::site;
use std::ffi::OsStr;
#[cfg(windows)]
use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(windows)]
use crate::authority::sandbox::windows::appcontainer::{
    CapabilitySids, OwnedCapabilitySid, OwnedContainerSid,
};
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
pub(crate) use windows::RawChild;

#[cfg(unix)]
pub(crate) struct Launch {
    cmd: std::process::Command,
    pub(super) setup: ChildSetup,
    /// Owned until a spawn hands it on.
    jail: Option<crate::process::jail::JailCgroup>,
    launcher: Option<Launcher>,
    evidence: Option<setup::Evidence>,
}

/// What a confined launch execs in the program's place: blamed by `role`
/// when it cannot start, and believed through its trampoline's report.
#[cfg(unix)]
struct Launcher {
    role: LauncherRole,
    report: crate::process::report::Report,
    /// An envelope's own stderr, which is not the payload's.
    #[cfg(target_os = "linux")]
    diagnostics: Option<std::fs::File>,
}

#[cfg(unix)]
impl Launcher {
    /// The first word, once the fork has left the child the writer's only
    /// copies. `piped` reads the stderr a launcher shares with its program.
    fn heard(
        self,
        scope: &crate::process::Scope,
        piped: impl FnOnce() -> Option<String>,
    ) -> Result<crate::process::report::Testimony, Unstarted> {
        let Self {
            role,
            report,
            #[cfg(target_os = "linux")]
            diagnostics,
        } = self;
        let mut testimony = report.forked();
        testimony.first(scope, || {
            #[cfg(target_os = "linux")]
            if let Some(file) = &diagnostics {
                return vanished(role, envelope::file_said(file));
            }
            vanished(role, piped())
        })?;
        Ok(testimony)
    }
}

/// Asked once after the fork for an envelope's payload group, answering only
/// once that group exists or the envelope is gone: `Ok(None)` is an envelope
/// that died before reporting.
type PayloadPgid =
    Box<dyn FnOnce() -> Result<Option<crate::process::Pgid>, PayloadGroupFailure> + Send>;

/// The child's steps between fork and exec, prepared in the parent. The
/// order they are configured in is irrelevant: [`ChildSetup::install`] fixes
/// it once.
#[cfg(unix)]
#[derive(Default)]
pub(super) struct ChildSetup {
    stderr_to_stdout: bool,
    #[cfg(target_os = "linux")]
    streams: Option<EnvelopeStreams>,
    pub(super) session_limits: crate::process::ChildLimits,
    pub(super) grant_limits: Option<crate::process::limits::GrantLimits>,
    #[cfg(seccomp)]
    guest_jail: Option<crate::process::jail::linux::Entry>,
}

#[cfg(unix)]
impl ChildSetup {
    /// Register the child's one hook, in its one order. The incoming
    /// command's descriptor handoff runs first; the spawn appends placement,
    /// signal reset and the final exec-readiness mark. The hook owns every
    /// descriptor it uses, so they live until the command is dropped.
    fn install(self, cmd: &mut std::process::Command, evidence: &mut setup::Evidence) {
        use std::os::unix::process::CommandExt;
        let Self {
            stderr_to_stdout,
            #[cfg(target_os = "linux")]
            streams,
            session_limits,
            grant_limits,
            #[cfg(seccomp)]
            guest_jail,
        } = self;
        let merge = evidence.check(setup::Context::Prepare(
            PrepareStep::MergeStderr,
            PrepareSubject::Stream(crate::fact::StandardStream::Stderr),
        ));
        #[cfg(target_os = "linux")]
        let capture = evidence.check(setup::Context::Prepare(
            PrepareStep::CapturePayloadStreams,
            PrepareSubject::Child,
        ));
        let session = crate::process::ChildLimits::checks(evidence);
        let grant = crate::process::limits::GrantLimits::checks(evidence);
        #[cfg(seccomp)]
        let jail_checks = guest_jail.as_ref().map(|jail| jail.checks(evidence));
        // SAFETY: all resources were prepared in the parent. The hook runs
        // only in the forked child, before exec, as `capture` requires; it
        // makes only async-signal-safe syscalls and neither allocates nor
        // locks. `std` has installed stdio. Merge before capture, capture before a
        // low RLIMIT_NOFILE, mandatory restrictions after session limits.
        unsafe {
            cmd.pre_exec(move || {
                if stderr_to_stdout && libc::dup2(1, 2) < 0 {
                    return merge.run(Err(std::io::Error::last_os_error()));
                }
                #[cfg(target_os = "linux")]
                if let Some(streams) = &streams {
                    capture.run(streams.capture())?;
                }
                session_limits.apply_checked(&session)?;
                if let Some(limits) = grant_limits {
                    limits.apply_checked(&grant)?;
                }
                #[cfg(seccomp)]
                if let Some((jail, checks)) = guest_jail.as_ref().zip(jail_checks.as_ref()) {
                    jail.enter_checked(checks)?;
                }
                Ok(())
            });
        }
    }
}

#[cfg(target_os = "linux")]
mod envelope;
#[cfg(target_os = "linux")]
use envelope::EnvelopeStreams;
#[cfg(target_os = "linux")]
pub(crate) use envelope::{EnvelopeStdio, EnvelopedLaunch};

mod landing;
pub(crate) use landing::Leader;

/// A confined launch's token, described: the projection its spawn stamps and
/// mints into an `AppContainer` token, and the image that token must load.
#[cfg(windows)]
pub(crate) struct Confined {
    pub(crate) projection: crate::authority::sandbox::SandboxProjection,
    pub(crate) image: Option<crate::path::RealPath>,
}

/// Why a launch returned no child: the program's own failure to become a
/// process, a refused launch step (possibly after the program ran), or the
/// run's stop before the child existed or while a confined launch waited
/// to hear its program start. Any child created is torn down before return.
#[derive(Debug)]
pub(crate) enum Unstarted {
    Program(std::io::Error),
    Refused(Error),
    Stopped(crate::process::Cancellation),
}

impl From<Error> for Unstarted {
    fn from(refusal: Error) -> Self {
        Self::Refused(refusal)
    }
}

impl From<crate::process::Cancellation> for Unstarted {
    fn from(stop: crate::process::Cancellation) -> Self {
        Self::Stopped(stop)
    }
}

/// A child created, its jail, and `H`, what is left to hear of it.
type Created<H> = Result<
    (
        crate::process::ChildHandle,
        Option<crate::process::jail::JailCgroup>,
        H,
    ),
    Unstarted,
>;

/// A child born, and where it landed.
pub(crate) struct Spawned<G> {
    pub(crate) child: crate::process::ChildHandle,
    pub(crate) group: G,
    pub(crate) terminal: crate::process::TerminalHold,
    /// The guest jail's cgroup, whatever `apply_guest_jail` staged.
    pub(crate) jail: Option<crate::process::jail::JailCgroup>,
}

#[cfg(windows)]
pub(crate) struct Launch {
    program: OsString,
    arg0: Option<OsString>,
    args: Vec<OsString>,
    env: std::collections::BTreeMap<EnvKey, EnvEdit>,
    cwd: Option<PathBuf>,
    stdin: StdioSpec,
    stdout: StdioSpec,
    stderr: StdioSpec,
    stderr_to_stdout: bool,
    creation_flags: u32,
    admitted_handles: Vec<std::os::windows::io::RawHandle>,
    confined: Option<Confined>,
    /// A grant's process budget, set on the child's Job Object while it is
    /// still suspended.
    pub(super) process_cap: bool,
}

/// A confined spawn's `SECURITY_CAPABILITIES`, as `session::confine` mints
/// them: owners held until `CreateProcessW` returns.
#[cfg(windows)]
pub(crate) struct SecurityCapabilitiesAttr {
    app_container_sid: Arc<OwnedContainerSid>,
    filesystem: Vec<Arc<OwnedCapabilitySid>>,
    network: Option<Arc<CapabilitySids>>,
}

#[cfg(windows)]
impl SecurityCapabilitiesAttr {
    pub(crate) const fn new(
        app_container_sid: Arc<OwnedContainerSid>,
        filesystem: Vec<Arc<OwnedCapabilitySid>>,
        network: Option<Arc<CapabilitySids>>,
    ) -> Self {
        Self {
            app_container_sid,
            filesystem,
            network,
        }
    }

    #[cfg(test)]
    pub(crate) fn network_capability_count(&self) -> usize {
        self.network.as_ref().map_or(0, |caps| caps.sids().count())
    }
}

#[cfg(windows)]
#[derive(Clone, Eq)]
struct EnvKey {
    folded: Vec<u8>,
    original: OsString,
}

#[cfg(windows)]
impl PartialEq for EnvKey {
    fn eq(&self, other: &Self) -> bool {
        self.folded == other.folded
    }
}

#[cfg(windows)]
impl PartialOrd for EnvKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(windows)]
impl Ord for EnvKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.folded.cmp(&other.folded)
    }
}

#[cfg(windows)]
enum EnvEdit {
    Set(OsString),
    Remove,
}

pub(crate) enum StdioSpec {
    Inherit,
    Null,
    #[cfg(test)]
    Piped,
    Owned(OwnedStdio),
}

#[cfg(unix)]
type OwnedStdio = std::os::fd::OwnedFd;
#[cfg(windows)]
type OwnedStdio = std::os::windows::io::OwnedHandle;

impl StdioSpec {
    #[cfg(unix)]
    fn into_stdio(self) -> std::process::Stdio {
        match self {
            Self::Inherit => std::process::Stdio::inherit(),
            Self::Null => std::process::Stdio::null(),
            #[cfg(test)]
            Self::Piped => std::process::Stdio::piped(),
            Self::Owned(fd) => fd.into(),
        }
    }

    /// The child's fd becomes this one; a pipe end or file converts as is.
    pub(crate) fn from_owned(owned: impl Into<OwnedStdio>) -> Self {
        Self::Owned(owned.into())
    }
}

impl Launch {
    /// Apply `env`: its cwd, then its sets, then its unsets. The one way to
    /// give a launch a directory or environment, so none bypasses
    /// [`EnvVars::for_child`](crate::environ::EnvVars::for_child).
    pub(crate) fn situate(&mut self, env: ChildEnv<'_>) -> Result<&mut Self, Error> {
        let ChildEnv { cwd, set, unset } = env;
        self.current_dir(cwd);
        for (key, value) in set {
            let name = || crate::path::shown(key);
            nul_free(
                key,
                PrepareStep::Environment,
                PrepareSubject::EnvironmentKey(name()),
            )?;
            nul_free(
                value,
                PrepareStep::Environment,
                PrepareSubject::EnvironmentValue(name()),
            )?;
            self.env(key, value);
        }
        for key in unset {
            self.env_remove(key);
        }
        Ok(self)
    }
}

#[cfg(unix)]
impl Launch {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let cmd = site!(Surface::ProcessLaunch, std::process::Command::new(program));
        Self::from_command(cmd)
    }

    /// Adopt an existing `Command`. Set stdio and redirections on the
    /// returned `Launch`, never on the incoming one: `std` exposes no stdio
    /// getters, so the Windows arm cannot carry them across. Existing hooks
    /// (the descriptor handoff) precede [`ChildSetup::install`]; configure
    /// limits and redirections through `Launch`, not through incoming hooks.
    pub fn from_command(cmd: std::process::Command) -> Self {
        Self {
            cmd,
            setup: ChildSetup::default(),
            jail: None,
            launcher: None,
            evidence: None,
        }
    }

    pub(crate) fn with_setup_evidence(mut self, evidence: setup::Evidence) -> Self {
        self.evidence = Some(evidence);
        self
    }

    /// Adopt the trampoline's command and its report together.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn trampolined(
        cmd: std::process::Command,
        report: crate::process::report::Report,
    ) -> Self {
        Self {
            launcher: Some(Launcher {
                role: LauncherRole::RalTrampoline,
                report,
                #[cfg(target_os = "linux")]
                diagnostics: None,
            }),
            ..Self::from_command(cmd)
        }
    }

    pub(crate) fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.arg(arg);
        self
    }

    /// `argv[0]`, which need not name the program run.
    pub(crate) fn arg0(&mut self, arg0: impl AsRef<OsStr>) -> &mut Self {
        use std::os::unix::process::CommandExt;
        self.cmd.arg0(arg0);
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.cmd.args(args);
        self
    }

    fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env(key, val);
        self
    }

    fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env_remove(key);
        self
    }

    fn current_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.cmd.current_dir(dir.into());
        self
    }

    pub fn stdin(&mut self, stdio: StdioSpec) -> &mut Self {
        self.cmd.stdin(stdio.into_stdio());
        self
    }

    pub fn stdout(&mut self, stdio: StdioSpec) -> &mut Self {
        self.cmd.stdout(stdio.into_stdio());
        self
    }

    pub fn stderr(&mut self, stdio: StdioSpec) -> &mut Self {
        self.cmd.stderr(stdio.into_stdio());
        self
    }

    pub(crate) fn dup_stdout_to_stderr(&mut self) {
        self.setup.stderr_to_stdout = true;
    }

    /// Prepare the cgroup and confinement without installing child actions.
    ///
    /// # Errors
    /// Seccomp compilation or any cgroup setup error.
    #[cfg(seccomp)]
    pub(crate) fn apply_guest_jail(
        &mut self,
        plan: &crate::process::jail::JailPlan,
    ) -> Result<(), Error> {
        let (cgroup, entry) = crate::process::jail::linux::Entry::prepare(plan)?;
        self.jail = cgroup;
        self.setup.guest_jail = Some(entry);
        Ok(())
    }

    /// The command with its setup installed, and its jail cgroup, for a test
    /// that probes the child after that setup.
    #[cfg(all(test, seccomp))]
    pub(super) fn installed(
        self,
    ) -> (
        std::process::Command,
        Option<crate::process::jail::JailCgroup>,
    ) {
        let Self {
            mut cmd,
            setup,
            jail,
            ..
        } = self;
        let mut evidence = setup::Evidence::new().expect("test setup evidence");
        setup.install(&mut cmd, &mut evidence);
        use std::os::unix::process::CommandExt;
        // SAFETY: retain the reader through earlier hooks, without child-side work.
        unsafe {
            cmd.pre_exec(move || {
                let _ = &evidence;
                Ok(())
            });
        }
        (cmd, jail)
    }

    /// Fork under `step`: the child, its jail, and what is left to hear of
    /// it, the launcher's first word where one runs in the program's place.
    fn create(
        self,
        step: crate::process::group::Step,
        scope: &crate::process::Scope,
    ) -> Created<impl FnOnce(&mut crate::process::ChildHandle) -> Result<(), Unstarted>> {
        scope.poll()?;
        let Self {
            mut cmd,
            setup,
            jail,
            launcher,
            evidence,
        } = self;
        let mut evidence = evidence
            .map_or_else(setup::Evidence::new, Ok)
            .map_err(|e| prepare_io(PrepareStep::ReadinessMark, PrepareSubject::Child, &e))?;
        setup.install(&mut cmd, &mut evidence);
        let child = fork(cmd, launcher.as_ref(), evidence, |cmd, evidence| {
            crate::process::group::spawn_with_evidence(cmd, step, evidence)
        })?;
        let hear = move |child: &mut crate::process::ChildHandle| {
            let Some(launcher) = launcher else {
                return Ok(());
            };
            let testimony = launcher.heard(scope, || child.stderr_said())?;
            // Kept for the reaper, which amends the child's end by it.
            #[cfg(target_os = "linux")]
            child.testify(testimony);
            #[cfg(not(target_os = "linux"))]
            drop(testimony);
            Ok(())
        };
        Ok((crate::process::ChildHandle::from_std(child), jail, hear))
    }

    /// Spawn *detached* by double fork: the survivor is this process's
    /// grandchild, reparented onto init in a session of its own, and at most its
    /// pid comes back.  Nothing here can then name it — the whole difference
    /// from [`Self::spawn`].
    ///
    /// So point all three standard streams at something that outlives this
    /// process; an inherited one dies with us, taking the survivor's writes
    /// with it. Under a guest jail the survivor joins the one `detached`
    /// cgroup every survivor shares, and its limits. Membership grants this
    /// launch no teardown authority, even on failure.
    ///
    /// # Errors
    /// [`Unstarted`], as for [`Self::spawn`], but the survivor is never
    /// killed; a survivor whose pid is lost is `Survivor::Unnamed`.
    pub(crate) fn spawn_detached(
        self,
        scope: &crate::process::Scope,
    ) -> Result<crate::process::group::Survivor, Unstarted> {
        scope.poll()?;
        let Self {
            mut cmd,
            setup,
            jail,
            launcher,
            evidence,
        } = self;
        assert!(
            jail.is_none(),
            "a detached launch must not own a kept jail cgroup: its drop would kill the survivor"
        );
        let mut evidence = evidence
            .map_or_else(setup::Evidence::new, Ok)
            .map_err(|e| prepare_io(PrepareStep::DetachedHandshake, PrepareSubject::Child, &e))?;
        setup.install(&mut cmd, &mut evidence);
        let survivor = fork(
            cmd,
            launcher.as_ref(),
            evidence,
            crate::process::group::spawn_detached_with_evidence,
        )?;
        if let Some(launcher) = launcher {
            launcher.heard(scope, || None)?;
        }
        Ok(survivor)
    }

    /// The command, for a test that reads its argv.
    #[cfg(test)]
    pub(crate) fn command(&self) -> &std::process::Command {
        &self.cmd
    }

    /// Run to the end, for a test: the streams, and the ending as the
    /// reaper hands it to every waiter.
    #[cfg(test)]
    pub(crate) fn output(mut self) -> Result<std::process::Output, Unstarted> {
        self.stdout(StdioSpec::Piped).stderr(StdioSpec::Piped);
        let unstopped = crate::process::DurableRoot::default().child();
        Ok(output(
            self.spawn(crate::process::Placement::Inherit, &unstopped)?,
        ))
    }
}

#[cfg(all(test, unix))]
fn output(spawned: Spawned<crate::process::Group>) -> std::process::Output {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    let Spawned { mut child, .. } = spawned;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let (stdout, stderr) = (drain(child.take_stdout()), drain(child.take_stderr()));
    let (tx, rx) = std::sync::mpsc::channel();
    let watch = child.into_watch(tx, std::convert::identity);
    let outcome = rx.recv().expect("the reaper reports every watched exit");
    let _ = watch.reap();
    let raw = match outcome {
        crate::process::WaitOutcome::Exited(code) => code << 8,
        crate::process::WaitOutcome::Signaled(signal) => signal.number(),
    };
    std::process::Output {
        status: std::process::ExitStatus::from_raw(raw),
        stdout: stdout.join().expect("the stdout drain"),
        stderr: stderr.join().expect("the stderr drain"),
    }
}

/// Fork, then release the parent's child-only descriptors before anything
/// is heard. A failed `execve` is the program's own, unless `launcher` was
/// to run in its place: then it is the launcher's, and a refusal. Ral's own
/// callers refuse NULs earlier ([`nul_free`]); `Prepare` is the backstop for
/// the rest.
#[cfg(unix)]
fn fork<T>(
    mut cmd: std::process::Command,
    launcher: Option<&Launcher>,
    evidence: setup::Evidence,
    spawn: impl FnOnce(
        &mut std::process::Command,
        &mut setup::Evidence,
    ) -> Result<T, crate::process::SpawnError>,
) -> Result<T, Unstarted> {
    use crate::process::SpawnError::{Exec, Prepare, Setup};
    let mut evidence = evidence;
    let forked = spawn(&mut cmd, &mut evidence);
    let subject = crate::fact::LaunchImageSubject::File(crate::path::shown(cmd.get_program()));
    drop(cmd);
    forked.map_err(|error| match (error, launcher) {
        (Prepare(error), _) => {
            Unstarted::Refused(refused(evidence.reported_failure().unwrap_or_else(|| {
                LaunchFailure::Launcher(match error.raw_os_error() {
                    Some(_) => LauncherFailure::SpawnBeforeSetup(
                        crate::fact::LauncherFailureSpawnBeforeSetup {
                            subject,
                            cause: OsFailure::from(&error),
                        },
                    ),
                    None => LauncherFailure::NonNativeExecError(
                        crate::fact::LauncherFailureNonNativeExecError {
                            subject,
                            cause: Box::new(crate::io::failure(&error)),
                        },
                    ),
                })
            })))
        }
        (Setup(failure), _) => Unstarted::Refused(refused(failure)),
        (Exec(error), None) => Unstarted::Program(error),
        (Exec(error), Some(launcher)) => Unstarted::Refused(refused(LaunchFailure::Launcher(
            LauncherFailure::Exec(crate::fact::LauncherFailureExec {
                launcher: launcher.role,
                cause: OsFailure::from(&error),
            }),
        ))),
    })
}

#[cfg(unix)]
fn vanished(launcher: LauncherRole, stderr: Option<String>) -> Error {
    refused(LaunchFailure::Launcher(LauncherFailure::DiedBeforeReport(
        crate::fact::LauncherFailureDiedBeforeReport { launcher, stderr },
    )))
}

/// Refuse `text` if it holds a NUL: no process byte string can.
pub(crate) fn nul_free(
    text: &OsStr,
    step: PrepareStep,
    subject: PrepareSubject,
) -> Result<(), Error> {
    if text.as_encoded_bytes().contains(&0) {
        Err(refused(LaunchFailure::Prepare(PrepareFailure {
            step,
            subject,
            cause: PrepareCause::Invalid(crate::fact::PrepareInvalid::Nul),
        })))
    } else {
        Ok(())
    }
}

pub(crate) fn refused(failure: LaunchFailure) -> Error {
    Error::new(Reason::Launch(Box::new(failure)))
}

pub(crate) fn prepare_io(
    step: PrepareStep,
    subject: PrepareSubject,
    error: &std::io::Error,
) -> Error {
    refused(LaunchFailure::Prepare(PrepareFailure {
        step,
        subject,
        cause: PrepareCause::Io(Box::new(crate::io::failure(error))),
    }))
}

/// A child created and not yet landed. Unless it lands, its drop kills every
/// group named for it, then the child, and reaps it: no exit from a birth
/// leaves either alive. No group ID is kept for a kill after the reap.
struct Unlanded<'a> {
    child: &'a mut crate::process::ChildHandle,
    /// An envelope's payload group, once named. Nothing pins it.
    payload: Option<crate::process::Pgid>,
    /// The fresh group the child leads and, unreaped, pins.
    led: Option<crate::process::Pgid>,
}

impl Unlanded<'_> {
    /// Ask an envelope for its payload's group, and hold the answer.
    fn name(&mut self, payload: Option<PayloadPgid>) -> Result<(), Unstarted> {
        self.payload = payload
            .map_or(Ok(None), |ask| ask())
            .map_err(|e| refused(LaunchFailure::Launcher(LauncherFailure::PayloadGroup(e))))?;
        Ok(())
    }

    /// Landed: the child and its groups are their collector's from here.
    fn landed(self) {
        std::mem::forget(self);
    }
}

impl Drop for Unlanded<'_> {
    fn drop(&mut self) {
        let groups = [self.payload, self.led];
        for group in groups.into_iter().flatten() {
            group.kill();
        }
        let _ = self.child.kill();
        let _ = self.child.reap();
        #[cfg(windows)]
        for group in groups.into_iter().flatten() {
            crate::process::release_win_group(group.as_raw());
        }
    }
}

impl Launch {
    /// One birth for every landing: create the child under `step`, name its
    /// envelope's `payload` group before anything is heard, hear its
    /// launcher, then `land` on the child's own pgid and the payload's.
    ///
    /// A failed birth tears its child down, and only then drops `request`,
    /// restoring what it saved, and after it the jail.
    fn born<R, G>(
        self,
        step: crate::process::group::Step,
        request: R,
        scope: &crate::process::Scope,
        payload: Option<PayloadPgid>,
        land: impl FnOnce(
            R,
            crate::process::Pgid,
            Option<crate::process::Pgid>,
        ) -> Result<(G, crate::process::TerminalHold), (R, Unstarted)>,
    ) -> Result<Spawned<G>, Unstarted> {
        let (mut child, jail, hear) = self.create(step, scope)?;
        let own = crate::process::Pgid::from_raw(
            i32::try_from(child.id()).expect("a child pid fits in i32"),
        )
        .expect("a child pid is positive");
        let mut unlanded = Unlanded {
            child: &mut child,
            payload: None,
            led: step.leads().then_some(own),
        };
        let heard = unlanded.name(payload).and_then(|()| hear(unlanded.child));
        let landed = match heard {
            Ok(()) => land(request, own, unlanded.payload),
            Err(why) => Err((request, why)),
        };
        match landed {
            Ok((group, terminal)) => {
                unlanded.landed();
                Ok(Spawned {
                    child,
                    group,
                    terminal,
                    jail,
                })
            }
            Err((request, why)) => {
                drop(unlanded);
                drop(request);
                drop(jail);
                Err(why)
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::disallowed_methods, reason = "test code")]
#[path = "launch/terminal_tests.rs"]
mod terminal_tests;

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod regression_tests;

#[cfg(all(test, unix))]
mod tests {
    use super::{Launch, Unstarted};
    use crate::environ::ChildEnv;
    use crate::fact::FailureCode as _;
    use crate::fact::{
        LaunchFailure, PrepareCause, PrepareFailure, PrepareStep, PrepareSubject, Reason,
    };
    use crate::process::{CancelCause, DurableRoot, Membership, Pgid, Placement, Scope, ScopeNode};

    fn unstopped() -> ScopeNode {
        DurableRoot::default().child()
    }

    /// `situate` refuses the pair with a NUL fact about `subject`.
    fn situate_refuses(key: &str, value: &str, subject: &PrepareSubject) {
        let cwd = crate::test_helper::process_cwd();
        let error = Launch::new("/bin/true")
            .situate(ChildEnv {
                cwd: cwd.as_path(),
                set: vec![(key.as_ref(), value.as_ref())],
                unset: Vec::new(),
            })
            .map(drop)
            .unwrap_err();
        let Reason::Launch(failure) = &error.reason else {
            panic!("a NUL is a launch refusal");
        };
        assert!(matches!(&**failure, LaunchFailure::Prepare(PrepareFailure {
            step: PrepareStep::Environment, subject: found, cause: PrepareCause::Invalid(crate::fact::PrepareInvalid::Nul),
        }) if found == subject));
        assert_eq!(error.failure_code().get(), 1);
    }

    #[test]
    fn situate_refuses_a_nul_value() {
        situate_refuses("K", "a\0b", &PrepareSubject::EnvironmentValue("K".into()));
    }

    #[test]
    fn situate_refuses_a_nul_key() {
        let subject = PrepareSubject::EnvironmentKey(crate::path::shown("K\0"));
        situate_refuses("K\0", "v", &subject);
    }

    #[test]
    fn a_failed_session_limit_reports_its_exact_subject() {
        let mut launch = Launch::new("/bin/true");
        launch.limit_session(crate::process::ChildLimits {
            open_files: Some(u64::MAX - 1),
            ..Default::default()
        });
        let Err(Unstarted::Refused(error)) = launch.spawn(Placement::Inherit, &unstopped()) else {
            panic!("a soft limit above the hard limit must fail");
        };
        assert!(matches!(&error.reason, crate::fact::Reason::Launch(failure)
        if matches!(**failure, crate::fact::LaunchFailure::Prepare(crate::fact::PrepareFailure {
            step: crate::fact::PrepareStep::SessionLimit,
            subject: crate::fact::PrepareSubject::SessionLimit(crate::fact::SessionLimit::OpenFiles), ..
        }))));
    }

    /// A group nobody leads: no pid reaches `i32::MAX`.
    fn nobody() -> Placement {
        Placement::Join(Membership::new(
            Pgid::from_raw(i32::MAX).expect("positive"),
            Scope::clone(&unstopped()),
        ))
    }

    #[test]
    fn stderr_merge_uses_the_final_stdout() {
        let mut launch = Launch::new("/bin/sh");
        launch.dup_stdout_to_stderr();
        launch.args(["-c", "printf OUT; printf ERR >&2"]);
        // output() supplies the streams after the merge was configured.
        let output = launch.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"OUTERR");
        assert!(output.stderr.is_empty(), "{output:?}");
    }

    #[test]
    fn grant_limits_win_even_when_configured_before_session_limits() {
        use rustix::process::{Resource, getrlimit};
        let core_bytes = getrlimit(Resource::Core).maximum.unwrap_or(1024).min(1024);
        if core_bytes == 0 {
            return; // This host already forbids core dumps; it cannot witness precedence.
        }
        let mut launch = Launch::new("/bin/sh");
        launch.args(["-c", "ulimit -Sc; ulimit -Hc"]);
        launch.limit_resources().unwrap();
        launch.limit_session(crate::process::ChildLimits {
            core_bytes: Some(core_bytes),
            ..Default::default()
        });
        let output = launch.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"0\n0\n");
    }

    #[test]
    fn detached_launches_install_the_same_merge_and_limits() {
        use crate::process::StdioSpec;
        use std::io::Read;
        let (mut reader, writer) = crate::process::cloexec_pipe().unwrap();
        let mut launch = Launch::new("/bin/sh");
        launch.args(["-c", "ulimit -n >&2"]);
        launch.limit_session(crate::process::ChildLimits {
            open_files: Some(64),
            ..Default::default()
        });
        launch.dup_stdout_to_stderr();
        launch.stdin(StdioSpec::Null);
        launch.stdout(StdioSpec::from_owned(writer));
        launch.stderr(StdioSpec::Null);
        launch.spawn_detached(&unstopped()).unwrap();
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap();
        assert_eq!(output, "64\n");
    }

    /// ral's own step failing refuses the launch; the `execve` failing is
    /// the program's.
    #[test]
    fn ral_s_failed_step_is_refused_and_the_program_s_exec_is_its_own() {
        let Err(Unstarted::Refused(why)) = Launch::new("/bin/sh").spawn(nobody(), &unstopped())
        else {
            panic!("a group nobody leads cannot be joined");
        };
        assert!(matches!(&why.reason, crate::fact::Reason::Launch(failure)
        if matches!(**failure, crate::fact::LaunchFailure::Prepare(crate::fact::PrepareFailure {
            step: crate::fact::PrepareStep::Group, ..
        }))));
        assert!(
            why.describe().contains("place the child in process group"),
            "{why}"
        );

        let missing =
            Launch::new("ral-no-such-program-exists").spawn(Placement::Inherit, &unstopped());
        assert!(
            matches!(&missing, Err(Unstarted::Program(e)) if e.kind() == std::io::ErrorKind::NotFound),
            "{:?}",
            missing.err()
        );
    }

    /// `program` run under a trampoline that is never told anything.
    fn confined(program: &str) -> Launch {
        let (reader, writer) = crate::process::cloexec_socketpair().expect("socketpair");
        Launch::trampolined(
            Launch::new(program).cmd,
            crate::process::report::Report::new(reader, writer.into()),
        )
    }

    /// Confinement changes whose the `execve` is, never whose ral's own
    /// step is: a failed placement is ral's, not the launcher's.
    #[test]
    fn a_confined_launch_s_failed_step_is_ral_s_and_its_exec_the_launcher_s() {
        let Err(Unstarted::Refused(why)) = confined("/bin/sh").spawn(nobody(), &unstopped()) else {
            panic!("a group nobody leads cannot be joined, confined or not");
        };
        assert!(matches!(&why.reason, crate::fact::Reason::Launch(failure)
        if matches!(**failure, crate::fact::LaunchFailure::Prepare(crate::fact::PrepareFailure {
            step: crate::fact::PrepareStep::Group, ..
        }))));
        assert!(
            why.describe().contains("place the child in process group"),
            "{why}"
        );

        let missing =
            confined("ral-no-such-program-exists").spawn(Placement::Inherit, &unstopped());
        let Err(Unstarted::Refused(why)) = missing else {
            panic!(
                "a launcher that cannot start is unavailable: {:?}",
                missing.err()
            );
        };
        assert!(matches!(&why.reason, crate::fact::Reason::Launch(failure)
            if matches!(**failure, crate::fact::LaunchFailure::Launcher(crate::fact::LauncherFailure::Exec(_)))));
    }

    /// A synthetic envelope report isolates placement from backend support;
    /// actual envelope mode restoration is a separate PTY regression.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_envelope_leads_its_own_group_even_when_asked_to_inherit_or_join() {
        for placement in [Placement::Inherit, nobody()] {
            let (reader, writer) = crate::process::cloexec_socketpair().expect("socketpair");
            let witness = crate::process::report::Witness::new(
                writer.try_clone().expect("clone report writer").into(),
            )
            .expect("a report is a socket");
            drop(witness.started());
            let mut launch = Launch::new("/bin/sh");
            launch.args(["-c", "exit 0"]);
            let launch = super::EnvelopedLaunch::synthetic(
                launch.cmd,
                crate::fact::LauncherRole::Bwrap,
                None,
                crate::process::report::Report::new(reader, writer.into()),
            );
            let mut spawned = launch.spawn(placement, &unstopped()).unwrap();
            let pid = spawned.child.id();
            let status = spawned.child.reap().expect("reap envelope");
            assert!(status.success(), "{status:?}");
            assert_eq!(
                spawned.group.owned().map(crate::process::Pgid::as_raw),
                Some(i32::try_from(pid).unwrap())
            );
        }
    }

    /// A stop in force when a launch starts is the run's stop, and no child
    /// is made, confined or not.
    #[test]
    fn a_launch_started_under_a_stop_makes_no_child() {
        let stopped = unstopped();
        stopped.cancel(CancelCause::Cancelled);
        for launch in [Launch::new("/bin/sh"), confined("/bin/sh")] {
            let spawned = launch.spawn(Placement::Inherit, &stopped);
            assert!(
                matches!(spawned, Err(Unstarted::Stopped(_))),
                "{:?}",
                spawned.err()
            );
        }
        let survivor = confined("/bin/sh").spawn_detached(&stopped);
        assert!(
            matches!(survivor, Err(Unstarted::Stopped(_))),
            "{:?}",
            survivor.err()
        );
    }
}

#[cfg(windows)]
impl Launch {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            arg0: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            stdin: StdioSpec::Inherit,
            stdout: StdioSpec::Inherit,
            stderr: StdioSpec::Inherit,
            stderr_to_stdout: false,
            creation_flags: 0,
            admitted_handles: Vec::new(),
            confined: None,
            process_cap: false,
        }
    }

    /// Adopt an existing `Command` — program, args, environment, cwd.  `std`
    /// exposes no stdio getters, so stdio set on it is dropped and must be
    /// re-set here, exactly as on the non-Windows arm.
    #[allow(
        dead_code,
        reason = "shape, not use: every caller of the pair (`authority::sandbox::reexec::launch`, `authority::sandbox::launch`) is `cfg(unix)` or Linux/macOS-only today, and both arms present one signature for the day one is not"
    )]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the non-Windows arm moves the `Command` into the launch it returns; both arms take it by value so the shared callers see one cross-platform signature"
    )]
    pub fn from_command(cmd: std::process::Command) -> Self {
        let mut launch = Self::new(cmd.get_program());
        launch.args(cmd.get_args());
        for (key, value) in cmd.get_envs() {
            match value {
                Some(value) => {
                    launch.env(key, value);
                }
                None => {
                    launch.env_remove(key);
                }
            }
        }
        if let Some(cwd) = cmd.get_current_dir() {
            launch.current_dir(cwd);
        }
        launch
    }

    pub(crate) fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// The command line's first token, which need not name the image.
    pub(crate) fn arg0(&mut self, arg0: impl AsRef<OsStr>) -> &mut Self {
        self.arg0 = Some(arg0.as_ref().to_os_string());
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(
            env_key(key.as_ref()),
            EnvEdit::Set(val.as_ref().to_os_string()),
        );
        self
    }

    fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(env_key(key.as_ref()), EnvEdit::Remove);
        self
    }

    fn current_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn stdin(&mut self, stdio: StdioSpec) -> &mut Self {
        self.stdin = stdio;
        self
    }

    pub fn stdout(&mut self, stdio: StdioSpec) -> &mut Self {
        self.stdout = stdio;
        self
    }

    pub fn stderr(&mut self, stdio: StdioSpec) -> &mut Self {
        self.stderr = stdio;
        self
    }

    /// Alias the final stdout handle when lowering standard streams.
    pub(crate) fn dup_stdout_to_stderr(&mut self) {
        self.stderr_to_stdout = true;
    }

    /// Run under the `AppContainer` token `confined` describes, which the
    /// spawn stamps and mints.
    pub(crate) fn confine(&mut self, confined: Confined) -> &mut Self {
        self.confined = Some(confined);
        self
    }

    /// Create the child under `step`. Windows has no launcher to hear and
    /// no jail.
    fn create(
        mut self,
        step: crate::process::group::Step,
        scope: &crate::process::Scope,
    ) -> Created<impl FnOnce(&mut crate::process::ChildHandle) -> Result<(), Unstarted>> {
        scope.poll()?;
        let caps = self
            .confined
            .as_ref()
            .map(|Confined { projection, image }| {
                crate::authority::sandbox::windows::session::confine(
                    projection,
                    image.as_ref(),
                    scope,
                )
            })
            .transpose()?;
        scope.poll()?;
        let child = windows::spawn(&mut self, step, caps.as_ref())?;
        Ok((child, None, |_: &mut crate::process::ChildHandle| Ok(())))
    }
}

#[cfg(windows)]
fn env_key(key: &OsStr) -> EnvKey {
    EnvKey {
        folded: key.as_encoded_bytes().to_ascii_uppercase(),
        original: key.to_os_string(),
    }
}

#[cfg(windows)]
mod windows_args {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    pub(super) fn make_command_line(
        program: &OsStr,
        args: &[std::ffi::OsString],
    ) -> Result<Vec<u16>, crate::fact::PrepareFailure> {
        use crate::fact::{PrepareCause, PrepareFailure, PrepareStep, PrepareSubject};
        ensure_no_nuls(program).map_err(|cause| PrepareFailure {
            step: PrepareStep::Argv,
            subject: PrepareSubject::Argument(0usize.into()),
            cause: PrepareCause::Invalid(cause),
        })?;
        let mut cmd = Vec::new();
        cmd.push(u16::from(b'"'));
        cmd.extend(program.encode_wide());
        cmd.push(u16::from(b'"'));
        for (index, arg) in args.iter().enumerate() {
            cmd.push(u16::from(b' '));
            append_arg(&mut cmd, arg).map_err(|cause| PrepareFailure {
                step: PrepareStep::Argv,
                subject: PrepareSubject::Argument((index + 1).into()),
                cause: PrepareCause::Invalid(cause),
            })?;
        }
        cmd.push(0);
        Ok(cmd)
    }

    fn append_arg(cmd: &mut Vec<u16>, arg: &OsStr) -> Result<(), crate::fact::PrepareInvalid> {
        ensure_no_nuls(arg)?;
        let units: Vec<u16> = arg.encode_wide().collect();
        let quote = units.is_empty()
            || units
                .iter()
                .any(|c| *c == u16::from(b' ') || *c == u16::from(b'\t'));
        if quote {
            cmd.push(u16::from(b'"'));
        }
        let mut backslashes = 0usize;
        for unit in units {
            if unit == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if unit == u16::from(b'"') {
                    cmd.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes + 1));
                }
                backslashes = 0;
            }
            cmd.push(unit);
        }
        if quote {
            cmd.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            cmd.push(u16::from(b'"'));
        }
        Ok(())
    }

    pub(super) fn ensure_no_nuls(s: &OsStr) -> Result<(), crate::fact::PrepareInvalid> {
        if s.encode_wide().any(|c| c == 0) {
            return Err(crate::fact::PrepareInvalid::Nul);
        }
        Ok(())
    }

    // ── Unicode round-trip ──────────────────────────────────────────────
    //
    // Non-BMP text through the argv UTF-16 conversion, so a wide-char
    // regression is caught apart from a quoting one.  Windows CI only.
    #[cfg(test)]
    #[allow(clippy::disallowed_methods, reason = "test code")]
    mod tests;
}

#[cfg(windows)]
mod windows {
    use super::{
        EnvEdit, Launch, SecurityCapabilitiesAttr, StdioSpec, Unstarted, prepare_io, refused,
        windows_args,
    };
    use crate::error::Error;
    use crate::fact::{
        BudgetFailure, BudgetFailureSetBudget, BudgetSetFailure, BudgetSubject, LaunchFailure,
        OsFailure, PrepareCause, PrepareFailure, PrepareInvalid, PrepareStep, PrepareSubject,
        StandardStream,
    };
    use crate::process::ChildHandle;
    use crate::process::group::Step;
    use crate::process::outcome::KILL_EXIT_CODE;
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{
        HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::SECURITY_CAPABILITIES;
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    /// `STARTUPINFOEXW`'s own byte count, as the `u32` its `cb` field wants.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a fixed Win32 layout; size_of is a compile-time constant of ~112, nowhere near u32::MAX"
    )]
    const STARTUPINFOEXW_CB: u32 = std::mem::size_of::<STARTUPINFOEXW>() as u32;

    pub(super) fn spawn(
        launch: &mut Launch,
        step: Step,
        caps: Option<&SecurityCapabilitiesAttr>,
    ) -> Result<ChildHandle, Unstarted> {
        reject_batch(&launch.program)?;
        let prepared = crate::process::group::prepare_group(step)
            .map_err(|e| refused(LaunchFailure::Prepare(e)))?;
        match launched(launch, step, &prepared, caps) {
            Ok(child) => {
                crate::process::group::register_prepared_group(prepared, &child);
                Ok(child)
            }
            Err(err) => {
                crate::process::group::close_prepared_group(prepared);
                Err(err)
            }
        }
    }

    /// The child, born suspended where a Job Object must hold it first: its
    /// group's, or one made for it alone to carry a grant's process budget.
    fn launched(
        launch: &mut Launch,
        step: Step,
        prepared: &crate::process::group::PreparedGroup,
        caps: Option<&SecurityCapabilitiesAttr>,
    ) -> Result<ChildHandle, Unstarted> {
        let grouped = crate::process::group::prepared_job(prepared);
        let own = (launch.process_cap && grouped.is_none())
            .then(own_job)
            .transpose()?;
        let job = grouped.or_else(|| own.as_ref().map(|job| job.as_raw_handle() as HANDLE));
        if let (true, Some(job)) = (launch.process_cap, job) {
            crate::process::group::set_active_process_limit(
                job,
                crate::process::limits::ACTIVE_PROCESS_CAP,
            )
            .map_err(|error| {
                budget_error(BudgetSetFailure::SetJobLimit(OsFailure::from(&error)))
            })?;
        }
        let created = spawn_inner(launch, step, job.is_some(), caps)?;
        if let Some(job) = job {
            created.enter(job)?;
        }
        // An own job outlives this handle: it ends with its last member.
        drop(own);
        Ok(created.into_child())
    }

    fn budget_error(cause: BudgetSetFailure) -> Error {
        refused(LaunchFailure::Budget(BudgetFailure::SetBudget(
            BudgetFailureSetBudget {
                subject: BudgetSubject::ChildJob,
                limit: u64::from(crate::process::limits::ACTIVE_PROCESS_CAP).into(),
                cause,
            },
        )))
    }

    fn invalid(step: PrepareStep, subject: PrepareSubject, cause: PrepareInvalid) -> Error {
        refused(LaunchFailure::Prepare(PrepareFailure {
            step,
            subject,
            cause: PrepareCause::Invalid(cause),
        }))
    }

    fn own_job() -> Result<OwnedHandle, Unstarted> {
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            let error = io::Error::last_os_error();
            return Err(budget_error(BudgetSetFailure::CreateJob(OsFailure::from(&error))).into());
        }
        // SAFETY: a fresh owning handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(job) })
    }

    /// A process `CreateProcessW` made, suspended where a job must hold it
    /// before it runs.
    struct Created {
        process: OwnedHandle,
        thread: OwnedHandle,
        pid: u32,
        #[cfg(test)]
        stdout: Option<std::fs::File>,
        #[cfg(test)]
        stderr: Option<std::fs::File>,
    }

    impl Created {
        /// Put the suspended child in `job`, then let it run; killed if
        /// either fails.
        fn enter(&self, job: HANDLE) -> Result<(), Unstarted> {
            let fail = |step| {
                let error = io::Error::last_os_error();
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(
                        self.process.as_raw_handle(),
                        1,
                    );
                }
                Unstarted::Refused(prepare_io(step, PrepareSubject::Child, &error))
            };
            if unsafe { AssignProcessToJobObject(job, self.process.as_raw_handle()) } == 0 {
                return Err(fail(PrepareStep::AssignJob));
            }
            if unsafe { ResumeThread(self.thread.as_raw_handle()) } == u32::MAX {
                return Err(fail(PrepareStep::ResumeThread));
            }
            Ok(())
        }

        fn into_child(self) -> ChildHandle {
            ChildHandle::from_windows_raw(RawChild::new(
                self.process,
                self.pid,
                #[cfg(test)]
                self.stdout,
                #[cfg(test)]
                self.stderr,
            ))
        }
    }

    fn spawn_inner(
        launch: &mut Launch,
        step: Step,
        suspended: bool,
        caps: Option<&SecurityCapabilitiesAttr>,
    ) -> Result<Created, Unstarted> {
        let stdin = ChildStdio::lower(
            std::mem::replace(&mut launch.stdin, StdioSpec::Null),
            Stream::Stdin,
        )
        .map_err(|e| {
            prepare_io(
                PrepareStep::Stdio,
                PrepareSubject::Stream(StandardStream::Stdin),
                &e,
            )
        })?;
        let stdout = ChildStdio::lower(
            std::mem::replace(&mut launch.stdout, StdioSpec::Null),
            Stream::Stdout,
        )
        .map_err(|e| {
            prepare_io(
                PrepareStep::Stdio,
                PrepareSubject::Stream(StandardStream::Stdout),
                &e,
            )
        })?;
        let stderr_spec = std::mem::replace(&mut launch.stderr, StdioSpec::Null);
        let stderr = if launch.stderr_to_stdout {
            drop(stderr_spec);
            // stdout owns this handle until CreateProcessW returns. The
            // inheritance list below admits the shared handle only once.
            ChildStdio {
                child: stdout.child,
                #[cfg(test)]
                parent_stdout: None,
                #[cfg(test)]
                parent_stderr: None,
                _owned_child: None,
            }
        } else {
            ChildStdio::lower(stderr_spec, Stream::Stderr).map_err(|e| {
                prepare_io(
                    PrepareStep::Stdio,
                    PrepareSubject::Stream(StandardStream::Stderr),
                    &e,
                )
            })?
        };

        let mut inherited = vec![stdin.child, stdout.child, stderr.child];
        inherited.extend(launch.admitted_handles.iter().map(|h| *h as HANDLE));
        inherited.sort();
        inherited.dedup();

        let mut cmdline = windows_args::make_command_line(
            launch.arg0.as_ref().unwrap_or(&launch.program),
            &launch.args,
        )
        .map_err(|e| refused(LaunchFailure::Prepare(e)))?;
        let application = wide_null(&launch.program).map_err(|e| {
            invalid(
                PrepareStep::Application,
                PrepareSubject::Program(crate::path::shown(&launch.program)),
                e,
            )
        })?;
        let cwd = launch
            .cwd
            .as_ref()
            .map(|p| wide_null(p.as_os_str()))
            .transpose()
            .map_err(|e| {
                invalid(
                    PrepareStep::WorkingDirectory,
                    PrepareSubject::Path(crate::path::shown(
                        launch.cwd.as_ref().expect("cwd validation requires cwd"),
                    )),
                    e,
                )
            })?;
        let mut env =
            environment_block(&launch.env).map_err(|e| refused(LaunchFailure::Prepare(e)))?;

        let attr_count: u32 = if caps.is_some() { 2 } else { 1 };
        let mut capabilities = caps.map_or_else(Vec::new, |caps| {
            crate::authority::sandbox::windows::appcontainer::capability_entries(
                &caps.filesystem,
                caps.network.as_deref(),
            )
        });
        // The local array and the caller's owners outlive CreateProcessW.
        let security_caps_value: Option<SECURITY_CAPABILITIES> =
            caps.map(|caps| SECURITY_CAPABILITIES {
                AppContainerSid: caps.app_container_sid.sid(),
                Capabilities: capabilities.as_mut_ptr(),
                CapabilityCount: u32::try_from(capabilities.len())
                    .expect("a projection declares a handful of capability SIDs, not four billion"),
                Reserved: 0,
            });
        let mut attrs = AttributeList::new(attr_count)
            .map_err(|e| prepare_io(PrepareStep::AttributeList, PrepareSubject::Child, &e))?;
        attrs
            .update_handle_list(&inherited)
            .map_err(|e| prepare_io(PrepareStep::HandleList, PrepareSubject::Child, &e))?;
        if let Some(value) = security_caps_value.as_ref() {
            attrs.update_security_capabilities(value).map_err(|e| {
                prepare_io(PrepareStep::SecurityCapabilities, PrepareSubject::Child, &e)
            })?;
        }

        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = STARTUPINFOEXW_CB;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin.child;
        startup.StartupInfo.hStdOutput = stdout.child;
        startup.StartupInfo.hStdError = stderr.child;
        startup.lpAttributeList = attrs.as_mut_ptr();

        let mut flags =
            launch.creation_flags | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
        if !matches!(step, Step::Inherit) {
            flags |= CREATE_NEW_PROCESS_GROUP;
        }
        if suspended {
            flags |= CREATE_SUSPENDED;
        }

        let guard = crate::process::spawn_lock::lock_inheritance();
        let inheritable = InheritableHandles::set(&inherited)
            .map_err(|e| prepare_io(PrepareStep::HandleInheritance, PrepareSubject::Child, &e))?;
        #[cfg(test)]
        crate::process::spawn_lock::test_phase::pause();
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            CreateProcessW(
                application.as_ptr(),
                cmdline.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                env.as_mut_ptr() as *const _,
                cwd.as_ref().map_or(null(), std::vec::Vec::as_ptr),
                &raw const startup.StartupInfo,
                &raw mut pi,
            )
        };
        // Capture before flag restoration (or unlocking) can change last-error.
        let error = (ok == 0).then(io::Error::last_os_error);
        drop(inheritable);
        drop(guard);
        // Only `CreateProcessW`'s own failure is the program's.
        if let Some(error) = error {
            return Err(Unstarted::Program(error));
        }

        // SAFETY: CreateProcessW returned two fresh owning handles.
        Ok(Created {
            process: unsafe { OwnedHandle::from_raw_handle(pi.hProcess) },
            thread: unsafe { OwnedHandle::from_raw_handle(pi.hThread) },
            pid: pi.dwProcessId,
            #[cfg(test)]
            stdout: stdout.parent_stdout,
            #[cfg(test)]
            stderr: stderr.parent_stderr,
        })
    }

    /// Refuse `.bat`/`.cmd` images outright: `cmd.exe` re-interprets the
    /// command line through its own escaping rules on top of
    /// `CreateProcessW`'s, so batch argument quoting has no safe general
    /// encoding (the CVE-2024-24576 class).  A `cmd /c` wrapper only moves it.
    fn reject_batch(program: &OsStr) -> Result<(), Error> {
        if crate::path::is_batch_image(program) {
            return Err(refused(LaunchFailure::BatchImage(crate::path::shown(
                program,
            ))));
        }
        Ok(())
    }

    #[derive(Clone, Copy)]
    enum Stream {
        Stdin,
        Stdout,
        Stderr,
    }

    struct ChildStdio {
        child: HANDLE,
        #[cfg(test)]
        parent_stdout: Option<std::fs::File>,
        #[cfg(test)]
        parent_stderr: Option<std::fs::File>,
        _owned_child: Option<OwnedHandle>,
    }

    impl ChildStdio {
        fn lower(spec: StdioSpec, stream: Stream) -> io::Result<Self> {
            match spec {
                StdioSpec::Inherit => {
                    let handle = match stream {
                        Stream::Stdin => unsafe {
                            windows_sys::Win32::System::Console::GetStdHandle(
                                windows_sys::Win32::System::Console::STD_INPUT_HANDLE,
                            )
                        },
                        Stream::Stdout => unsafe {
                            windows_sys::Win32::System::Console::GetStdHandle(
                                windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
                            )
                        },
                        Stream::Stderr => unsafe {
                            windows_sys::Win32::System::Console::GetStdHandle(
                                windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
                            )
                        },
                    };
                    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                        return Self::null();
                    }
                    Ok(Self {
                        child: handle,
                        #[cfg(test)]
                        parent_stdout: None,
                        #[cfg(test)]
                        parent_stderr: None,
                        _owned_child: None,
                    })
                }
                StdioSpec::Null => Self::null(),
                #[cfg(test)]
                StdioSpec::Piped => Self::piped(stream),
                StdioSpec::Owned(handle) => {
                    let raw = handle.as_raw_handle() as HANDLE;
                    Ok(Self {
                        child: raw,
                        #[cfg(test)]
                        parent_stdout: None,
                        #[cfg(test)]
                        parent_stderr: None,
                        _owned_child: Some(handle),
                    })
                }
            }
        }

        fn null() -> io::Result<Self> {
            let handle = OwnedHandle::from(crate::path::open_discard()?);
            let raw = handle.as_raw_handle() as HANDLE;
            Ok(Self {
                child: raw,
                #[cfg(test)]
                parent_stdout: None,
                #[cfg(test)]
                parent_stderr: None,
                _owned_child: Some(handle),
            })
        }

        #[cfg(test)]
        fn piped(stream: Stream) -> io::Result<Self> {
            let (reader, writer) = crate::process::cloexec_pipe()?;
            match stream {
                Stream::Stdin => {
                    let raw = reader.as_raw_handle() as HANDLE;
                    Ok(Self {
                        child: raw,
                        parent_stdout: None,
                        parent_stderr: None,
                        _owned_child: Some(pipe_reader_to_owned(reader)),
                    })
                }
                Stream::Stdout => {
                    let raw = writer.as_raw_handle() as HANDLE;
                    let parent = pipe_reader_to_file(reader);
                    Ok(Self {
                        child: raw,
                        parent_stdout: Some(parent),
                        parent_stderr: None,
                        _owned_child: Some(pipe_writer_to_owned(writer)),
                    })
                }
                Stream::Stderr => {
                    let raw = writer.as_raw_handle() as HANDLE;
                    let parent = pipe_reader_to_file(reader);
                    Ok(Self {
                        child: raw,
                        parent_stdout: None,
                        parent_stderr: Some(parent),
                        _owned_child: Some(pipe_writer_to_owned(writer)),
                    })
                }
            }
        }
    }

    #[cfg(test)]
    fn pipe_reader_to_owned(reader: os_pipe::PipeReader) -> OwnedHandle {
        OwnedHandle::from(reader)
    }

    #[cfg(test)]
    fn pipe_writer_to_owned(writer: os_pipe::PipeWriter) -> OwnedHandle {
        OwnedHandle::from(writer)
    }

    #[cfg(test)]
    fn pipe_reader_to_file(reader: os_pipe::PipeReader) -> std::fs::File {
        std::fs::File::from(OwnedHandle::from(reader))
    }

    struct AttributeList {
        buf: Vec<u8>,
    }

    impl AttributeList {
        /// Allocate for exactly `attribute_count` entries — Win32 sizes the
        /// buffer up front, so it must match the `update_*` calls that follow.
        fn new(attribute_count: u32) -> io::Result<Self> {
            let mut bytes = 0usize;
            unsafe {
                InitializeProcThreadAttributeList(null_mut(), attribute_count, 0, &raw mut bytes);
            }
            let mut buf = vec![0; bytes];
            let ok = unsafe {
                InitializeProcThreadAttributeList(
                    buf.as_mut_ptr().cast(),
                    attribute_count,
                    0,
                    &raw mut bytes,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { buf })
        }

        fn as_mut_ptr(&mut self) -> *mut std::ffi::c_void {
            self.buf.as_mut_ptr().cast()
        }

        fn update_handle_list(&mut self, handles: &[HANDLE]) -> io::Result<()> {
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    self.as_mut_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    std::mem::size_of_val(handles),
                    null_mut(),
                    null(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn update_security_capabilities(
            &mut self,
            value: &SECURITY_CAPABILITIES,
        ) -> io::Result<()> {
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    self.as_mut_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    std::ptr::from_ref::<SECURITY_CAPABILITIES>(value).cast(),
                    std::mem::size_of::<SECURITY_CAPABILITIES>(),
                    null_mut(),
                    null(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.as_mut_ptr());
            }
        }
    }

    struct InheritableHandles(Vec<HANDLE>);

    impl InheritableHandles {
        fn set(handles: &[HANDLE]) -> io::Result<Self> {
            for &handle in handles {
                let ok = unsafe {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
                };
                if ok == 0 {
                    let error = io::Error::last_os_error();
                    for &done in handles.iter().take_while(|h| **h != handle) {
                        unsafe {
                            SetHandleInformation(done, HANDLE_FLAG_INHERIT, 0);
                        }
                    }
                    return Err(error);
                }
            }
            Ok(Self(handles.to_vec()))
        }
    }

    impl Drop for InheritableHandles {
        fn drop(&mut self) {
            for &handle in &self.0 {
                unsafe {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }

    fn wide_null(s: &OsStr) -> Result<Vec<u16>, PrepareInvalid> {
        windows_args::ensure_no_nuls(s)?;
        Ok(s.encode_wide().chain([0]).collect())
    }

    fn environment_block(
        edits: &std::collections::BTreeMap<super::EnvKey, EnvEdit>,
    ) -> Result<Vec<u16>, PrepareFailure> {
        let mut env = std::collections::BTreeMap::<super::EnvKey, OsString>::new();
        for (k, v) in crate::host::vars_os() {
            env.insert(super::env_key(&k), v);
        }
        for (k, edit) in edits {
            match edit {
                EnvEdit::Set(v) => {
                    env.insert(k.clone(), v.clone());
                }
                EnvEdit::Remove => {
                    env.remove(k);
                }
            }
        }
        let mut block = Vec::new();
        for (k, v) in env {
            windows_args::ensure_no_nuls(&k.original).map_err(|cause| PrepareFailure {
                step: PrepareStep::Environment,
                subject: PrepareSubject::EnvironmentKey(k.original.to_string_lossy().into_owned()),
                cause: PrepareCause::Invalid(cause),
            })?;
            windows_args::ensure_no_nuls(&v).map_err(|cause| PrepareFailure {
                step: PrepareStep::Environment,
                subject: PrepareSubject::EnvironmentValue(
                    k.original.to_string_lossy().into_owned(),
                ),
                cause: PrepareCause::Invalid(cause),
            })?;
            block.extend(k.original.encode_wide());
            block.push(u16::from(b'='));
            block.extend(v.encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    // ── Raw CreateProcessW child ───────────────────────────────────────────
    //
    // The owning handle to a child born above, plus the wait/reap methods
    // `ChildHandle` dispatches to.

    pub(crate) struct RawChild {
        process: OwnedHandle,
        pid: u32,
        #[cfg(test)]
        stdout: Option<std::fs::File>,
        #[cfg(test)]
        stderr: Option<std::fs::File>,
    }

    impl RawChild {
        pub(crate) fn new(
            process: OwnedHandle,
            pid: u32,
            #[cfg(test)] stdout: Option<std::fs::File>,
            #[cfg(test)] stderr: Option<std::fs::File>,
        ) -> Self {
            Self {
                process,
                pid,
                #[cfg(test)]
                stdout,
                #[cfg(test)]
                stderr,
            }
        }

        pub(crate) fn id(&self) -> u32 {
            self.pid
        }

        pub(crate) fn raw_process_handle(&self) -> HANDLE {
            self.process.as_raw_handle() as HANDLE
        }

        pub(crate) fn kill(&mut self) -> io::Result<()> {
            let ok = unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(
                    self.raw_process_handle(),
                    KILL_EXIT_CODE as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        #[cfg(test)]
        pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
            self.stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn std::io::Read + Send>)
        }

        #[cfg(test)]
        pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
            self.stderr
                .take()
                .map(|stderr| Box::new(stderr) as Box<dyn std::io::Read + Send>)
        }

        /// Block until exit and read the status.
        fn wait_and_exit_status(&self) -> io::Result<std::process::ExitStatus> {
            let r = unsafe { WaitForSingleObject(self.raw_process_handle(), INFINITE) };
            if r != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
            self.exit_status()
        }

        #[allow(
            clippy::needless_pass_by_ref_mut,
            reason = "mirrors `ChildHandle::reap`, whose `std::process::Child` arm calls the raw `Child::wait`; reaping is an exclusive act even where Windows performs it through a shared handle"
        )]
        pub(crate) fn reap(&mut self) -> io::Result<std::process::ExitStatus> {
            self.wait_and_exit_status()
        }

        fn exit_status(&self) -> io::Result<std::process::ExitStatus> {
            use std::os::windows::process::ExitStatusExt;
            let mut code = 0;
            let ok = unsafe { GetExitCodeProcess(self.raw_process_handle(), &raw mut code) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(std::process::ExitStatus::from_raw(code))
        }
    }

    // ── Unicode round-trip ──────────────────────────────────────────────
    //
    // The other two UTF-16 conversions: `wide_null` for the program path and
    // cwd, `environment_block` for the env block.
    #[cfg(test)]
    #[allow(clippy::disallowed_methods, reason = "test code")]
    mod tests;
}
