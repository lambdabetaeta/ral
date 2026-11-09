//! The owned process-launch value, built once and handed over once.
//!
//! Preparation owns the choices and resources; `ChildSetup::install` fixes
//! their child-side order; spawning releases child-only descriptors before
//! hearing the payload. No configuration method registers a launch-owned hook.
//!
//! Unix lowers it back to `std::process::Command`; Windows drives
//! `CreateProcessW` itself, because handle admission and Job Object
//! membership must be creation-time facts.

use crate::error::Error;
#[cfg(unix)]
use crate::process::report::Testimony;
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
    confined: Option<Confined>,
    /// The envelope's own stderr, which the parent reads back.
    #[cfg(target_os = "linux")]
    diagnostics: Option<std::fs::File>,
}

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
    fn install(self, cmd: &mut std::process::Command) {
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
        // SAFETY: all resources were prepared in the parent. The hook runs
        // only in the forked child, before exec, as `capture` requires; it
        // makes only async-signal-safe syscalls and neither allocates nor
        // locks. `std` has installed stdio. Merge before capture, capture before a
        // low RLIMIT_NOFILE, mandatory restrictions after session limits.
        unsafe {
            cmd.pre_exec(move || {
                if stderr_to_stdout && libc::dup2(1, 2) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                if let Some(streams) = &streams {
                    streams.capture()?;
                }
                session_limits.apply()?;
                if let Some(limits) = grant_limits {
                    limits.apply()?;
                }
                #[cfg(seccomp)]
                if let Some(jail) = &guest_jail {
                    jail.enter()?;
                }
                Ok(())
            });
        }
    }
}

#[cfg(unix)]
type PayloadPgid = Box<dyn FnOnce() -> Result<Option<crate::process::Pgid>, String> + Send>;

/// A confinement envelope: a separate binary that runs the target in turn,
/// in a session of its own.  Only such a backend declares one.  Windows has
/// none: its confinement is creation-time state on the target itself.
#[cfg(unix)]
pub(crate) struct Envelope {
    pub(crate) program: &'static str,
    /// `None` where the envelope reports no payload group: no `--info-fd`
    /// (a surrendered launch) or a backend without one.  Otherwise asked
    /// once, after the fork, for the payload's own process group, and
    /// answering only once the payload is in it or gone; its `Ok(None)` is an
    /// envelope that died before reporting; an error kills and reaps it.
    pub(crate) payload_pgid: Option<PayloadPgid>,
}

/// Reserved high slots for the payload, and independent envelope stdio.
/// The slots are allocated before spawning, never over an exec-error pipe.
#[cfg(target_os = "linux")]
pub(crate) struct EnvelopeStdio {
    pub(crate) payload: [std::os::fd::OwnedFd; 3],
    pub(crate) null: std::os::fd::OwnedFd,
    pub(crate) error: std::os::fd::OwnedFd,
    pub(crate) diagnostics: std::fs::File,
}

/// The child-side part of [`EnvelopeStdio`]: the payload's reserved slots and
/// the envelope's own endpoints.
#[cfg(target_os = "linux")]
struct EnvelopeStreams {
    payload: [std::os::fd::OwnedFd; 3],
    null: std::os::fd::OwnedFd,
    error: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl EnvelopeStreams {
    /// Save the final payload streams, then give the envelope its own
    /// endpoints. Only dup2/ioctl: no allocation or locks after fork.
    ///
    /// # Safety
    /// Only in the forked child, between fork and exec: it replaces fds 0–2
    /// and the reserved slots, which it only borrows.
    unsafe fn capture(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: every target is a reserved slot or a standard stream of
        // this child, never an exec-error pipe.
        unsafe {
            for (source, target) in [0, 1, 2].into_iter().zip(&self.payload) {
                if libc::dup2(source, target.as_raw_fd()) < 0 {
                    // A closed inherited stream refuses preparation. An absent
                    // slot is not a marker: bwrap could reuse it.
                    return Err(std::io::Error::last_os_error());
                }
            }
            // bwrap discovers the host tty from its actual stdout and binds it
            // as /dev/console. Tty lifetime is shared; pipe EOF/EPIPE is not.
            let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
            let tty_stdout = libc::ioctl(1, libc::TCGETS, termios.as_mut_ptr()) == 0;
            let sources = [&self.null, &self.null, &self.error];
            for (target, source) in [0, 1, 2].into_iter().zip(sources) {
                if target == 1 && tty_stdout {
                    continue;
                }
                if libc::dup2(source.as_raw_fd(), target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        }
    }
}

/// A confined launch's trampoline: the report it makes on its program, and
/// the envelope that runs it, where one does.  Forked, the report is the
/// parent's read end.
#[cfg(unix)]
pub(crate) struct Confined<R = crate::process::report::Report> {
    pub(crate) report: R,
    pub(crate) envelope: Option<Envelope>,
}

/// A confined launch's token, described: the projection its spawn stamps and
/// mints into an `AppContainer` token, and the image that token must load.
#[cfg(windows)]
pub(crate) struct Confined {
    pub(crate) projection: crate::authority::sandbox::SandboxProjection,
    pub(crate) image: Option<crate::path::RealPath>,
}

/// Why a launch made no child: the program's own failure to become a
/// process, something that is never the program's fault, or the run's stop
/// before the child existed or while a confined launch waited to hear its
/// program start.
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

/// A child born, and where it landed.
pub(crate) struct Spawned {
    pub(crate) child: crate::process::ChildHandle,
    pub(crate) group: crate::process::Group,
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
            Self::Piped => std::process::Stdio::piped(),
            Self::Owned(fd) => fd.into(),
        }
    }

    /// The child's fd becomes this one; a pipe end or file converts as is.
    pub(crate) fn from_owned(owned: impl Into<OwnedStdio>) -> Self {
        Self::Owned(owned.into())
    }
}

#[cfg(unix)]
impl Launch {
    /// Whether an envelope stands between ral and the program, whose payload
    /// then leads a session of its own.
    pub(crate) fn enveloped(&self) -> bool {
        self.confined
            .as_ref()
            .is_some_and(|confined| confined.envelope.is_some())
    }

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
            confined: None,
            #[cfg(target_os = "linux")]
            diagnostics: None,
        }
    }

    /// Retain the reserved slots; installation captures the final stdio.
    #[cfg(target_os = "linux")]
    pub(crate) fn envelope_stdio(&mut self, stdio: EnvelopeStdio) {
        let EnvelopeStdio {
            payload,
            null,
            error,
            diagnostics,
        } = stdio;
        self.setup.streams = Some(EnvelopeStreams {
            payload,
            null,
            error,
        });
        self.diagnostics = Some(diagnostics);
    }

    /// Run under the trampoline `confined` describes.
    pub(crate) fn confine(&mut self, confined: Confined) -> &mut Self {
        self.confined = Some(confined);
        self
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

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env(key, val);
        self
    }

    pub(crate) fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env_remove(key);
        self
    }

    pub(crate) fn current_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
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
        setup.install(&mut cmd);
        (cmd, jail)
    }

    /// Spawn into `pgid` under `scope`, and on a confined launch read what
    /// its trampoline says of the start: the child, where it landed, and
    /// whatever jail cgroup `apply_guest_jail` staged.  An enveloped launch
    /// is placed `NewLeader` (asserted): the envelope process is nobody's to
    /// address, and its payload's group, named once it exists, is where the
    /// child lands.
    ///
    /// # Errors
    /// [`Unstarted`]: the program's own `execve`; a refusal for the fork,
    /// any of ral's pre-exec steps, or any failure of the envelope or
    /// trampoline; or a stop on `scope` before the fork or before the
    /// trampoline spoke.  Any child is then killed and reaped.
    pub(crate) fn spawn(
        self,
        pgid: crate::process::PgidPolicy,
        scope: &crate::process::Scope,
    ) -> Result<Spawned, Unstarted> {
        use crate::process::{Group, PgidPolicy};
        let enveloped = self.enveloped();
        assert!(
            !enveloped || matches!(pgid, PgidPolicy::NewLeader),
            "an envelope leads a group of its own: place it by `PgidPolicy::unless_enveloped`"
        );
        scope.poll()?;
        let Self {
            mut cmd,
            setup,
            jail,
            confined,
            #[cfg(target_os = "linux")]
            diagnostics,
        } = self;
        setup.install(&mut cmd);
        let ((child, group), confined) = fork(cmd, confined, |cmd| {
            crate::process::spawn_with_pgid(cmd, pgid)
        })?;
        let mut child = crate::process::ChildHandle::from_std(child);
        #[cfg(target_os = "linux")]
        child.envelope_stderr(diagnostics);
        let payload = match confined.map(|confined| confined.hear(&mut child, scope)) {
            None => None,
            Some(Ok(payload)) => payload,
            Some(Err(unstarted)) => return Err(discarded(child, unstarted)),
        };
        let group = match group {
            Group::Owns(envelope) if enveloped => Group::Enveloped(payload.unwrap_or(envelope)),
            group => group,
        };
        Ok(Spawned { child, group, jail })
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
        let Self {
            mut cmd,
            setup,
            jail,
            confined,
            #[cfg(target_os = "linux")]
            diagnostics,
        } = self;
        assert!(
            jail.is_none(),
            "a detached launch must not own a kept jail cgroup: its drop would kill the survivor"
        );
        scope.poll()?;
        setup.install(&mut cmd);
        let (survivor, confined) = fork(cmd, confined, crate::process::group::spawn_detached)?;
        if let Some(confined) = confined {
            let launcher = confined.launcher();
            let Confined { mut report, .. } = confined;
            report.first(scope, || {
                #[cfg(target_os = "linux")]
                let said = diagnostics
                    .as_ref()
                    .and_then(crate::process::child::file_said);
                #[cfg(not(target_os = "linux"))]
                let said = None;
                vanished(launcher, said)
            })?;
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
        use std::io::Read;
        use std::os::unix::process::ExitStatusExt;
        self.stdout(StdioSpec::Piped).stderr(StdioSpec::Piped);
        let unstopped = crate::process::DurableRoot::default().child();
        let pgid = crate::process::PgidPolicy::Inherit.unless_enveloped(self.enveloped());
        let Spawned { mut child, .. } = self.spawn(pgid, &unstopped)?;
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
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(raw),
            stdout: stdout.join().expect("the stdout drain"),
            stderr: stderr.join().expect("the stderr drain"),
        })
    }
}

/// Fork, then release the parent's child-only descriptors before hearing
/// the payload. The report's writer must remain open across the fork only.
#[cfg(unix)]
fn fork<T>(
    mut cmd: std::process::Command,
    confined: Option<Confined>,
    spawn: impl FnOnce(&mut std::process::Command) -> Result<T, crate::process::SpawnError>,
) -> Result<(T, Option<Confined<Testimony>>), Unstarted> {
    let forked = spawn(&mut cmd);
    drop(cmd);
    let confined = confined.map(Confined::forked);
    match forked {
        Ok(born) => Ok((born, confined)),
        Err(e) => Err(unforked(e, confined.as_ref().map(Confined::launcher))),
    }
}

#[cfg(unix)]
impl Confined {
    fn forked(self) -> Confined<Testimony> {
        Confined {
            report: self.report.forked(),
            envelope: self.envelope,
        }
    }
}

#[cfg(unix)]
impl<R> Confined<R> {
    /// What runs in the program's place.
    fn launcher(&self) -> &'static str {
        self.envelope
            .as_ref()
            .map_or("ral's sandbox trampoline", |envelope| envelope.program)
    }
}

#[cfg(unix)]
impl Confined<Testimony> {
    /// Hear `child`'s start from its trampoline, a stop on `scope` ending the
    /// wait: its payload's group, where the envelope names one.  On Linux the
    /// child keeps the testimony for its end.
    fn hear(
        self,
        child: &mut crate::process::ChildHandle,
        scope: &crate::process::Scope,
    ) -> Result<Option<crate::process::Pgid>, Unstarted> {
        let launcher = self.launcher();
        let Self {
            mut report,
            envelope,
        } = self;
        let payload = match envelope.and_then(|envelope| envelope.payload_pgid) {
            Some(ask) => ask().map_err(unavailable)?,
            None => None,
        };
        report.first(scope, || vanished(launcher, child.stderr_said()))?;
        #[cfg(target_os = "linux")]
        child.testify(report);
        Ok(payload)
    }
}

/// A spawn that made no child: ral's step before the `execve`, or the
/// `execve`, the program's own unless `launcher` was to run in its place.
#[cfg(unix)]
fn unforked(e: crate::process::SpawnError, launcher: Option<&'static str>) -> Unstarted {
    use crate::process::SpawnError::{Exec, Prepare};
    match (e, launcher) {
        (Prepare(e), _) => unprepared(e),
        (Exec(e), None) => Unstarted::Program(e),
        (Exec(e), Some(launcher)) => Unstarted::Refused(unavailable(format!(
            "cannot start {launcher}: {e}; the command never ran"
        ))),
    }
}

/// `launcher`, ended before its trampoline spoke; `said`, what it left on a
/// stderr ral reads.
#[cfg(unix)]
fn vanished(launcher: &str, said: Option<String>) -> Error {
    unavailable(match said {
        Some(said) => format!("{launcher} ended before the program started: {said}"),
        None => format!(
            "{launcher} ended before the program started; what it said, if anything, went \
             to the command's stderr"
        ),
    })
}

/// This host cannot establish the confinement or budget the active grant
/// asks for: an axis no backend here enforces, an envelope binary that was
/// not there to pin at boot, an envelope or trampoline that failed.  Nothing
/// ran, so it is never phrased as the command's fault.
pub(crate) fn unavailable(why: impl std::fmt::Display) -> Error {
    Error::new(format!("sandbox confinement unavailable: {why}"))
}

/// One of ral's own steps before the program could start failed.
fn unprepared(why: impl std::fmt::Display) -> Unstarted {
    Unstarted::Refused(Error::new(format!(
        "ral could not prepare the command's process: {why}; the command never ran"
    )))
}

/// `child`, killed and reaped, for `why` it must not run.
#[cfg(unix)]
fn discarded(mut child: crate::process::ChildHandle, why: Unstarted) -> Unstarted {
    let _ = child.kill();
    let _ = child.reap();
    why
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Launch, Unstarted};
    use crate::process::{
        CancelCause, DurableRoot, Membership, Pgid, PgidPolicy, Scope, ScopeNode,
    };

    fn unstopped() -> ScopeNode {
        DurableRoot::default().child()
    }

    /// A group nobody leads: no pid reaches `i32::MAX`.
    fn nobody() -> PgidPolicy {
        PgidPolicy::Join(Membership::new(
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
        assert!(why.describe().contains("could not prepare"), "{why}");

        let missing =
            Launch::new("ral-no-such-program-exists").spawn(PgidPolicy::Inherit, &unstopped());
        assert!(
            matches!(&missing, Err(Unstarted::Program(e)) if e.kind() == std::io::ErrorKind::NotFound),
            "{:?}",
            missing.err()
        );
    }

    /// `program` run under a trampoline that is never told anything.
    fn confined(program: &str) -> Launch {
        let (reader, writer) = crate::process::cloexec_socketpair().expect("socketpair");
        let mut launch = Launch::new(program);
        launch.confine(super::Confined {
            report: crate::process::report::Report::new(reader, writer.into()),
            envelope: None,
        });
        launch
    }

    /// Confinement changes whose the `execve` is, never whose ral's own
    /// step is: a failed placement is ral's, not the launcher's.
    #[test]
    fn a_confined_launch_s_failed_step_is_ral_s_and_its_exec_the_launcher_s() {
        let Err(Unstarted::Refused(why)) = confined("/bin/sh").spawn(nobody(), &unstopped()) else {
            panic!("a group nobody leads cannot be joined, confined or not");
        };
        assert!(why.describe().contains("could not prepare"), "{why}");

        let missing =
            confined("ral-no-such-program-exists").spawn(PgidPolicy::Inherit, &unstopped());
        let Err(Unstarted::Refused(why)) = missing else {
            panic!(
                "a launcher that cannot start is unavailable: {:?}",
                missing.err()
            );
        };
        assert!(
            why.describe().starts_with(
                "sandbox confinement unavailable: cannot start ral's sandbox trampoline"
            ),
            "{why}"
        );
    }

    /// A stop in force when a launch starts is the run's stop, and no child
    /// is made, confined or not.
    #[test]
    fn a_launch_started_under_a_stop_makes_no_child() {
        let stopped = unstopped();
        stopped.cancel(CancelCause::Cancelled);
        for launch in [Launch::new("/bin/sh"), confined("/bin/sh")] {
            let spawned = launch.spawn(PgidPolicy::Inherit, &stopped);
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
    /// Windows has no envelope: confinement is creation-time state on the
    /// target itself.
    #[allow(clippy::unused_self, reason = "one signature with the Unix arm")]
    pub(crate) const fn enveloped(&self) -> bool {
        false
    }

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

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(
            env_key(key.as_ref()),
            EnvEdit::Set(val.as_ref().to_os_string()),
        );
        self
    }

    pub(crate) fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(env_key(key.as_ref()), EnvEdit::Remove);
        self
    }

    pub(crate) fn current_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
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

    /// Run under the `AppContainer` token `confined` describes, which the
    /// spawn stamps and mints.
    pub(crate) fn confine(&mut self, confined: Confined) -> &mut Self {
        self.confined = Some(confined);
        self
    }

    /// Confine under `scope`, then lower through `CreateProcessW` and spawn
    /// with the requested process-group placement: the child, and where it
    /// landed.  The jail cgroup is always `None`: Linux guests only.  The
    /// token goes straight from the confinement to `CreateProcessW`.
    ///
    /// # Errors
    /// [`Unstarted`]: `CreateProcessW`'s own failure, the program's; a
    /// refusal for the confinement, for a `.bat`/`.cmd` program and for any
    /// of ral's steps: a NUL in an argument, path or environment entry,
    /// handle admission, or a Job Object that cannot take the child or its
    /// process budget; or a stop on `scope` before, during or after the
    /// confinement, short of `CreateProcessW`.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the Unix arm moves the policy into its spawn; both arms present one signature"
    )]
    pub(crate) fn spawn(
        mut self,
        pgid: crate::process::PgidPolicy,
        scope: &crate::process::Scope,
    ) -> Result<Spawned, Unstarted> {
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
        windows::spawn(&mut self, &pgid, caps.as_ref()).map(|(child, group)| Spawned {
            child,
            group,
            jail: None,
        })
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
    use std::io;
    use std::os::windows::ffi::OsStrExt;

    pub(super) fn make_command_line(
        program: &OsStr,
        args: &[std::ffi::OsString],
    ) -> io::Result<Vec<u16>> {
        ensure_no_nuls(program)?;
        let mut cmd = Vec::new();
        cmd.push(u16::from(b'"'));
        cmd.extend(program.encode_wide());
        cmd.push(u16::from(b'"'));
        for arg in args {
            cmd.push(u16::from(b' '));
            append_arg(&mut cmd, arg)?;
        }
        cmd.push(0);
        Ok(cmd)
    }

    fn append_arg(cmd: &mut Vec<u16>, arg: &OsStr) -> io::Result<()> {
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

    pub(super) fn ensure_no_nuls(s: &OsStr) -> io::Result<()> {
        if s.encode_wide().any(|c| c == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows process argument contains a NUL byte",
            ));
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
        EnvEdit, Launch, SecurityCapabilitiesAttr, StdioSpec, Unstarted, unprepared, windows_args,
    };
    use crate::error::Error;
    use crate::process::outcome::KILL_EXIT_CODE;
    use crate::process::{ChildHandle, PgidPolicy};
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
        policy: &PgidPolicy,
        caps: Option<&SecurityCapabilitiesAttr>,
    ) -> Result<(ChildHandle, crate::process::Group), Unstarted> {
        reject_batch(&launch.program)?;
        let prepared = crate::process::group::prepare_group(policy)
            .map_err(|e| Error::new(format!("cannot prepare the child's Job Object: {e}")))?;
        match launched(launch, policy, &prepared, caps) {
            Ok(child) => {
                let group = crate::process::group::register_prepared_group(prepared, &child);
                Ok((child, group))
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
        policy: &PgidPolicy,
        prepared: &crate::process::group::PreparedGroup,
        caps: Option<&SecurityCapabilitiesAttr>,
    ) -> Result<ChildHandle, Unstarted> {
        let grouped = crate::process::group::prepared_job(prepared);
        let own = (launch.process_cap && grouped.is_none())
            .then(own_job)
            .transpose()?;
        let job = grouped.or_else(|| own.as_ref().map(|job| job.as_raw_handle() as HANDLE));
        if let (true, Some(job)) = (launch.process_cap, job)
            && !crate::process::group::set_active_process_limit(
                job,
                crate::process::limits::ACTIVE_PROCESS_CAP,
            )
        {
            return Err(
                crate::process::limits::unbudgeted("cannot cap the child's Job Object").into(),
            );
        }
        let created = spawn_inner(launch, policy, job.is_some(), caps)?;
        if let Some(job) = job {
            created.enter(job)?;
        }
        // An own job outlives this handle: it ends with its last member.
        drop(own);
        Ok(created.into_child())
    }

    fn own_job() -> Result<OwnedHandle, Unstarted> {
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            return Err(crate::process::limits::unbudgeted(io::Error::last_os_error()).into());
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
        stdout: Option<std::fs::File>,
        stderr: Option<std::fs::File>,
    }

    impl Created {
        /// Put the suspended child in `job`, then let it run; killed if
        /// either fails.
        fn enter(&self, job: HANDLE) -> Result<(), Unstarted> {
            let refused = |what: &str| {
                let error = io::Error::last_os_error();
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(
                        self.process.as_raw_handle(),
                        1,
                    );
                }
                Unstarted::Refused(Error::new(format!("{what}: {error}")))
            };
            if unsafe { AssignProcessToJobObject(job, self.process.as_raw_handle()) } == 0 {
                return Err(refused(
                    "could not place the child in its Job Object before start; is ral already \
                     running in a restrictive Windows Job Object?",
                ));
            }
            if unsafe { ResumeThread(self.thread.as_raw_handle()) } == u32::MAX {
                return Err(refused("could not resume the child once in its Job Object"));
            }
            Ok(())
        }

        fn into_child(self) -> ChildHandle {
            ChildHandle::from_windows_raw(RawChild::new(
                self.process,
                self.pid,
                self.stdout,
                self.stderr,
            ))
        }
    }

    fn spawn_inner(
        launch: &mut Launch,
        policy: &PgidPolicy,
        suspended: bool,
        caps: Option<&SecurityCapabilitiesAttr>,
    ) -> Result<Created, Unstarted> {
        let stdin = ChildStdio::lower(
            std::mem::replace(&mut launch.stdin, StdioSpec::Null),
            Stream::Stdin,
        )
        .map_err(unprepared)?;
        let mut stdout = ChildStdio::lower(
            std::mem::replace(&mut launch.stdout, StdioSpec::Null),
            Stream::Stdout,
        )
        .map_err(unprepared)?;
        let mut stderr = ChildStdio::lower(
            std::mem::replace(&mut launch.stderr, StdioSpec::Null),
            Stream::Stderr,
        )
        .map_err(unprepared)?;

        let mut inherited = vec![stdin.child, stdout.child, stderr.child];
        inherited.extend(launch.admitted_handles.iter().map(|h| *h as HANDLE));
        inherited.sort();
        inherited.dedup();

        let mut cmdline = windows_args::make_command_line(
            launch.arg0.as_ref().unwrap_or(&launch.program),
            &launch.args,
        )
        .map_err(unprepared)?;
        let application = wide_null(&launch.program).map_err(unprepared)?;
        let cwd = launch
            .cwd
            .as_ref()
            .map(|p| wide_null(p.as_os_str()))
            .transpose()
            .map_err(unprepared)?;
        let mut env = environment_block(&launch.env).map_err(unprepared)?;

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
        let mut attrs = AttributeList::new(attr_count).map_err(unprepared)?;
        attrs.update_handle_list(&inherited).map_err(unprepared)?;
        if let Some(value) = security_caps_value.as_ref() {
            attrs
                .update_security_capabilities(value)
                .map_err(unprepared)?;
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
        if matches!(
            policy,
            PgidPolicy::NewLeader | PgidPolicy::NewSession | PgidPolicy::Join(_)
        ) {
            flags |= CREATE_NEW_PROCESS_GROUP;
        }
        if suspended {
            flags |= CREATE_SUSPENDED;
        }

        let guard = crate::process::spawn_lock::lock_inheritance();
        let inheritable = InheritableHandles::set(&inherited).map_err(unprepared)?;
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
            stdout: stdout.parent_stdout.take(),
            stderr: stderr.parent_stderr.take(),
        })
    }

    /// Refuse `.bat`/`.cmd` images outright: `cmd.exe` re-interprets the
    /// command line through its own escaping rules on top of
    /// `CreateProcessW`'s, so batch argument quoting has no safe general
    /// encoding (the CVE-2024-24576 class).  A `cmd /c` wrapper only moves it.
    fn reject_batch(program: &OsStr) -> Result<(), Error> {
        if crate::path::is_batch_image(program) {
            return Err(Error::new(format!(
                "refusing to launch '{}': .bat/.cmd images are not supported on the \
                     raw Windows launch path. ral will not synthesize a `cmd /c` wrapper \
                     to run it, because batch-file argument quoting has no safe general \
                     encoding (the CVE-2024-24576 class of bug): a crafted argument could \
                     inject additional commands through cmd.exe's own escaping rules. Invoke \
                     the batch file through cmd.exe yourself if you accept that risk, or run \
                     the underlying program directly.",
                crate::path::shown(program)
            )));
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
        parent_stdout: Option<std::fs::File>,
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
                        parent_stdout: None,
                        parent_stderr: None,
                        _owned_child: None,
                    })
                }
                StdioSpec::Null => Self::null(),
                StdioSpec::Piped => Self::piped(stream),
                StdioSpec::Owned(handle) => {
                    let raw = handle.as_raw_handle() as HANDLE;
                    Ok(Self {
                        child: raw,
                        parent_stdout: None,
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
                parent_stdout: None,
                parent_stderr: None,
                _owned_child: Some(handle),
            })
        }

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

    fn pipe_reader_to_owned(reader: os_pipe::PipeReader) -> OwnedHandle {
        OwnedHandle::from(reader)
    }

    fn pipe_writer_to_owned(writer: os_pipe::PipeWriter) -> OwnedHandle {
        OwnedHandle::from(writer)
    }

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
                return Err(io::Error::other(
                    "could not launch child with an explicit handle list",
                ));
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
                return Err(io::Error::other(
                    "could not launch sandboxed child with AppContainer security capabilities",
                ));
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

    fn wide_null(s: &OsStr) -> io::Result<Vec<u16>> {
        windows_args::ensure_no_nuls(s)?;
        Ok(s.encode_wide().chain([0]).collect())
    }

    fn environment_block(
        edits: &std::collections::BTreeMap<super::EnvKey, EnvEdit>,
    ) -> io::Result<Vec<u16>> {
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
            windows_args::ensure_no_nuls(&k.original)?;
            windows_args::ensure_no_nuls(&v)?;
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
        stdout: Option<std::fs::File>,
        stderr: Option<std::fs::File>,
    }

    impl RawChild {
        pub(crate) fn new(
            process: OwnedHandle,
            pid: u32,
            stdout: Option<std::fs::File>,
            stderr: Option<std::fs::File>,
        ) -> Self {
            Self {
                process,
                pid,
                stdout,
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

        pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
            self.stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn std::io::Read + Send>)
        }

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
