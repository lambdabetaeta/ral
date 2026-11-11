//! The trampoline, `ral --warrant <w> <r>`: the confined child's side of a
//! warrant. Linux envelopes append the three payload stream slots.
//! It takes the report socket at `r` and the warrant at `w`, enters
//! the confinement, runs the image, and says on the report what became of
//! it.  Nothing runs unconfined, and nothing is printed: the parent words
//! what it is told.  Only a call naming no report socket, made by hand, is
//! refused on stderr.
//!
//! macOS becomes the image.  Linux runs it as this process's child inside the
//! bwrap envelope and supervises it, reporting the exact end bwrap would
//! fold into an exit status.

use super::super::launch::Image;
use super::{Confined, Slots, Warrant};
use crate::fact::{
    HandoffField, LaunchFailure, LaunchImageSubject, LauncherFailure, LauncherFailureChildFork,
    LauncherFailureChildPipe, LauncherFailureChildSetup, LauncherFailureNonNativeExecError,
    LauncherFailureSpawnBeforeSetup, OsFailure, WarrantFailure, WarrantFailureMissingDescriptor,
};
use crate::path::RealPath;
#[cfg(target_os = "linux")]
use crate::path::shown;
use crate::process::report::Witness;
use crate::site::{Silent, site};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
#[cfg(target_os = "linux")]
use {
    libc::c_int,
    rustix::process::Pid,
    std::io::{self, Read},
    std::os::fd::AsFd,
};

/// This process's exit when its program never ran.  The parent reads the
/// report instead.
const UNSTARTED: u8 = 126;

/// Serve the warrant and report slots, plus a Linux envelope's stream slots.
pub(crate) fn serve_warrant(args: &[OsString]) -> u8 {
    match received(args) {
        Ok((slots, witness)) => match Warrant::confine(slots) {
            Ok(confined) => confined.run(witness),
            Err(why) => {
                witness.refused(&why);
                UNSTARTED
            }
        },
        Err(e) => {
            crate::io::terminal::cmd_error("ral", &terminal_refusal(&e));
            UNSTARTED
        }
    }
}

fn terminal_refusal(failure: &LaunchFailure) -> String {
    use std::fmt::Write as _;
    let mut message = failure.describe();
    for advice in failure.advice() {
        let _ = write!(message, "\nhint: {advice}");
    }
    message
}

/// The slots `args` name, and the report socket at the second.
fn received(args: &[OsString]) -> Result<(Slots, Witness), LaunchFailure> {
    let slots = Slots::parse(args).map_err(LaunchFailure::Warrant)?;
    let at = slots.report;
    let fd = at.take().ok_or_else(|| {
        LaunchFailure::Warrant(WarrantFailure::MissingDescriptor(
            WarrantFailureMissingDescriptor {
                field: HandoffField::Report,
                fd: at.fd(),
            },
        ))
    })?;
    let witness = Witness::new(fd)
        .ok_or_else(|| LaunchFailure::Warrant(WarrantFailure::NotReportSocket(at.fd())))?;
    Ok((slots, witness))
}

/// The host file `real`, run under the spelling `arg0` it was named by.
fn program(real: &RealPath, arg0: &Path, args: Vec<OsString>) -> Command {
    let mut program = site!(Silent::RespawnSpawn, Command::new(real.as_path()));
    program.arg0(arg0).args(args);
    program
}

#[cfg(target_os = "macos")]
impl Confined {
    /// Become the image.  A host file is `execve`d, closing the
    /// close-on-exec report, which the parent reads as its start.  A bundled
    /// tool runs here once the report is closed; an `execve` of its own keeps
    /// this pid, which the parent waits on.
    fn run(self, witness: Witness) -> u8 {
        match self.image {
            Image::File { real, arg0 } => {
                let mut program = program(&real, &arg0, self.args);
                let e = site!(Silent::RespawnExec, &mut program => exec());
                witness.unstarted(
                    &e,
                    LaunchImageSubject::File(crate::path::shown(real.as_path())),
                );
                UNSTARTED
            }
            Image::Tool(tool) => {
                drop(witness);
                crate::uutils::serve(tool.as_ref(), self.args)
            }
        }
    }
}

/// Why a program did not begin: the failure is the program's only if its own
/// `execve` raised it.
#[cfg(target_os = "linux")]
#[derive(Debug)]
enum Unbegun {
    /// The `execve` of the program's own file failed.
    Program(io::Error),
    /// ral's own: a fork, or the child's setup before it became the program.
    Ours(LaunchFailure),
}

#[cfg(target_os = "linux")]
impl Confined {
    /// Run the image as this process's child and [`supervise`] it.
    fn run(self, witness: Witness) -> u8 {
        let caught = outlive_group_signals();
        let supervisor = rustix::process::getpid();
        let subject = match &self.image {
            Image::File { real, .. } => {
                LaunchImageSubject::File(crate::path::shown(real.as_path()))
            }
            Image::Tool(tool) => LaunchImageSubject::Bundled(tool.to_string()),
        };
        let (witness, started) = start(self.image, self.args, &caught, supervisor, witness);
        match started {
            Ok(pid) => supervise(witness, pid),
            Err(Unbegun::Program(e)) => {
                witness.unstarted(&e, subject);
                UNSTARTED
            }
            Err(Unbegun::Ours(why)) => {
                witness.refused(&why);
                UNSTARTED
            }
        }
    }
}

/// Begin `image` as a child of this process: a file is spawned, and a
/// bundled tool forked, since a tool may itself `execve` (`env`) where the
/// supervisor never does.  The forked child drops `witness`; it is handed
/// back to the supervisor.
#[cfg(target_os = "linux")]
fn start(
    image: Image,
    args: Vec<OsString>,
    caught: &[c_int],
    supervisor: Pid,
    witness: Witness,
) -> (Witness, Result<Pid, Unbegun>) {
    match image {
        Image::File { real, arg0 } => (witness, start_file(&real, &arg0, args, supervisor)),
        Image::Tool(tool) => start_tool(tool, args, caught, supervisor, witness),
    }
}

/// What a child that began, or failed to, says on the pipe `Dawn` reads: one
/// word, 0 or the errno of what failed.
#[cfg(target_os = "linux")]
enum Dawn {
    Begun,
    Failed(io::Error),
    /// No word: the child never ran its setup.
    Silent,
}

#[cfg(target_os = "linux")]
impl Dawn {
    fn heard(mut hears: impl Read) -> Self {
        let mut word = [0; 4];
        match hears
            .read_exact(&mut word)
            .map(|()| i32::from_ne_bytes(word))
        {
            Err(_) => Self::Silent,
            Ok(0) => Self::Begun,
            Ok(errno) => Self::Failed(io::Error::from_raw_os_error(errno)),
        }
    }

    /// The word for `begun`.  Async-signal-safe, for a forked child.
    fn tell(tells: &impl AsFd, begun: &io::Result<()>) {
        let errno = begun
            .as_ref()
            .err()
            .map_or(0, |e| e.raw_os_error().unwrap_or(libc::EIO));
        let _ = rustix::io::write(tells, &errno.to_ne_bytes());
    }
}

/// Spawn the host file `real`, its child begun once it has died with
/// `supervisor`.
#[cfg(target_os = "linux")]
fn start_file(
    real: &RealPath,
    arg0: &Path,
    args: Vec<OsString>,
    supervisor: Pid,
) -> Result<Pid, Unbegun> {
    let subject = LaunchImageSubject::File(shown(real.as_path()));
    let ours = |cause| Unbegun::Ours(LaunchFailure::Launcher(cause));
    let (hears, tells) = crate::process::cloexec_pipe().map_err(|e| {
        ours(LauncherFailure::ChildPipe(LauncherFailureChildPipe {
            subject: subject.clone(),
            cause: OsFailure::from(&e),
        }))
    })?;
    let mut program = program(real, arg0, args);
    // SAFETY: `die_with` and the write make only async-signal-safe syscalls.
    unsafe {
        program.pre_exec(move || {
            let begun = die_with(supervisor);
            Dawn::tell(&tells, &begun);
            begun
        });
    }
    let spawned = crate::process::spawn(&mut program);
    // The hook's writer goes with the program, so the pipe ends with the child.
    drop(program);
    spawned
        .map(|child| Pid::from_child(&child))
        .map_err(|e| match Dawn::heard(hears) {
            Dawn::Begun if e.raw_os_error().is_some() => Unbegun::Program(e),
            Dawn::Begun => ours(LauncherFailure::NonNativeExecError(
                LauncherFailureNonNativeExecError {
                    subject: subject.clone(),
                    cause: Box::new(crate::io::failure(&e)),
                },
            )),
            Dawn::Failed(setup) => ours(LauncherFailure::ChildSetup(LauncherFailureChildSetup {
                subject: subject.clone(),
                cause: OsFailure::from(&setup),
            })),
            Dawn::Silent => ours(LauncherFailure::SpawnBeforeSetup(
                LauncherFailureSpawnBeforeSetup {
                    subject: subject.clone(),
                    cause: OsFailure::from(&e),
                },
            )),
        })
}

/// Fork the bundled `tool`, which tells whether it began before it runs, so
/// that a tool that cannot begin is refused, never a program that exited.
#[cfg(target_os = "linux")]
fn start_tool(
    tool: &'static str,
    args: Vec<OsString>,
    caught: &[c_int],
    supervisor: Pid,
    witness: Witness,
) -> (Witness, Result<Pid, Unbegun>) {
    let subject = LaunchImageSubject::Bundled(tool.to_owned());
    let ours = |cause| Unbegun::Ours(LaunchFailure::Launcher(cause));
    let (hears, tells) = match crate::process::cloexec_pipe() {
        Ok(pipe) => pipe,
        Err(e) => {
            return (
                witness,
                Err(ours(LauncherFailure::ChildPipe(LauncherFailureChildPipe {
                    subject,
                    cause: OsFailure::from(&e),
                }))),
            );
        }
    };
    // SAFETY: the warrant role starts no thread, so the child may run
    // anything.
    match unsafe { crate::process::fork() } {
        Ok(crate::process::Forked::Child) => {
            drop((witness, hears));
            tool_child(tool, args, caught, supervisor, tells)
        }
        Ok(crate::process::Forked::Parent(pid)) => {
            drop(tells);
            let started = match Dawn::heard(hears) {
                Dawn::Begun => Ok(pid),
                Dawn::Failed(e) => Err(ours(LauncherFailure::ChildSetup(
                    LauncherFailureChildSetup {
                        subject,
                        cause: OsFailure::from(&e),
                    },
                ))),
                Dawn::Silent => Err(ours(LauncherFailure::ChildDiedBeforeSetup(subject))),
            };
            if started.is_err() {
                let _ = crate::process::reap(pid);
            }
            (witness, started)
        }
        Err(e) => (
            witness,
            Err(ours(LauncherFailure::ChildFork(LauncherFailureChildFork {
                subject,
                cause: OsFailure::from(&e),
            }))),
        ),
    }
}

/// Tell the start of `pid`, the program, and, waited for, its exact end;
/// exit as it did.  What the payload's group is sent this process outlives,
/// but `SIGKILL`, whose silence the parent reads as such.
#[cfg(target_os = "linux")]
fn supervise(witness: Witness, pid: Pid) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    // The program has inherited its streams; only it may hold up EOF or EPIPE.
    // The report's handoff slot is above stderr and stays ours until the end.
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: the warrant role is single-threaded, owns no Rust handles for
        // stdio, and never uses it again. Linux releases even an interrupted
        // close's descriptor: do not retry, or close a reused number.
        unsafe { libc::close(fd) };
    }
    let _ = crate::process::install(libc::SIGPIPE, crate::process::Disposition::Ignore);
    let running = witness.started();
    let Ok(ended) = crate::process::reap(pid) else {
        // Unreachable: the program is this process's own child.
        return UNSTARTED;
    };
    running.ended(ended);
    ended
        .code()
        .or_else(|| ended.signal().map(|signal| 128 + signal))
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(u8::MAX)
}

/// The forked tool's whole life: it tells whether it [`begin`]s, then runs,
/// holding only what the trampoline's program would.  It never returns to the
/// trampoline's frames, by a panic either.
#[cfg(target_os = "linux")]
fn tool_child(
    tool: &'static str,
    args: Vec<OsString>,
    caught: &[c_int],
    supervisor: Pid,
    tells: os_pipe::PipeWriter,
) -> ! {
    use std::io::Write;
    let begun = begin(caught, supervisor);
    Dawn::tell(&tells, &begun);
    // No `execve` here closes it on exec.
    drop(tells);
    if begun.is_err() {
        leave(UNSTARTED);
    }
    // 101: `std`'s exit for a panicking `main`.
    let code =
        std::panic::catch_unwind(move || crate::uutils::serve(tool.as_ref(), args)).unwrap_or(101);
    let _ = (std::io::stdout().flush(), std::io::stderr().flush());
    leave(code)
}

/// Begin as a spawned program begins: no signal blocked, each one `caught`
/// here at its default, and death with `supervisor`.
#[cfg(target_os = "linux")]
fn begin(caught: &[c_int], supervisor: Pid) -> io::Result<()> {
    for &signal in caught {
        crate::process::install(signal, crate::process::Disposition::Default)?;
    }
    nix::sys::signal::SigSet::empty().thread_set_mask()?;
    die_with(supervisor)
}

/// Exit at once, running no atexit hook over the trampoline's state.
#[cfg(target_os = "linux")]
fn leave(code: u8) -> ! {
    // SAFETY: `_exit` only ends this process.
    unsafe { libc::_exit(i32::from(code)) }
}

/// Be killed when `supervisor`, this process's parent, dies.
#[cfg(target_os = "linux")]
fn die_with(supervisor: Pid) -> std::io::Result<()> {
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
    // A supervisor gone before the prctl leaves no death to hear.
    if rustix::process::getppid() != Some(supervisor) {
        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

/// A no-op handler for each group signal not ignored, which resets to its
/// default at the program's `execve`; a signal ignored stays so, for the
/// program to inherit.  The signals given the handler.
#[cfg(target_os = "linux")]
fn outlive_group_signals() -> Vec<c_int> {
    extern "C" fn heard(_: c_int) {}
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .filter(|&signal| {
            !crate::process::signal::ignored(signal)
                && crate::process::signal::catch(signal, heard).is_ok()
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    /// Refused before any descriptor is touched, so it is safe in-process:
    /// the trampoline accepts only distinct, well-formed handoff slots.
    #[test]
    fn a_warrant_flag_naming_no_slot_runs_nothing() {
        for args in [
            &[][..],
            &["sh"],
            &["101"],
            &["97", "101"],
            &["101", "101"],
            &["+101", "102"],
            &["0101", "102"],
            &["101", "sh"],
            &["101", "102", "sh"],
        ] {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(serve_warrant(&args), UNSTARTED, "{args:?}");
        }
    }

    #[test]
    fn a_malformed_standalone_warrant_prints_its_explanation_then_intrinsic_advice_once() {
        const HELPER: &str = "RAL_MALFORMED_WARRANT_TEST";
        if std::env::var_os(HELPER).is_some() {
            assert_eq!(serve_warrant(&[]), UNSTARTED);
            return;
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "authority::sandbox::warrant::trampoline::tests::a_malformed_standalone_warrant_prints_its_explanation_then_intrinsic_advice_once", "--nocapture"])
            .env(HELPER, "1")
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = crate::process::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "ral: --warrant descriptor-argument count is 0; expected 2 (warrant and report), or 5 under a Linux envelope\nhint: --warrant is how ral starts its own confined children, never a command to run by hand.\n"
        );
    }

    /// A pid no process has: a child asked to die with it finds its parent
    /// is another.
    #[cfg(target_os = "linux")]
    fn a_stranger() -> Pid {
        Pid::from_raw(i32::MAX).expect("a positive pid")
    }

    #[cfg(target_os = "linux")]
    fn witness() -> Witness {
        let (ours, _theirs) = crate::process::cloexec_socketpair().expect("socketpair");
        Witness::new(ours.into()).expect("a socket")
    }

    #[cfg(target_os = "linux")]
    fn file(path: &str) -> (RealPath, std::path::PathBuf) {
        let real = RealPath::off_the_wire(std::ffi::OsStr::new(path)).expect("a real path");
        (real, path.into())
    }

    /// The child's word is read as written: begun, the errno it failed
    /// with, or silence.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_childs_word_is_begun_failed_or_silent() {
        let word = |errno: i32| errno.to_ne_bytes();
        assert!(matches!(Dawn::heard(&word(0)[..]), Dawn::Begun));
        assert!(matches!(
            Dawn::heard(&word(libc::EAGAIN)[..]),
            Dawn::Failed(e) if e.raw_os_error() == Some(libc::EAGAIN)
        ));
        assert!(matches!(Dawn::heard(&[][..]), Dawn::Silent));
        assert!(matches!(Dawn::heard(&word(0)[..2]), Dawn::Silent));
    }

    /// Only the file's own `execve` failing is the program's failure.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_whose_execve_fails_is_the_programs_failure() {
        let (real, arg0) = file("/ral/no/such/program");
        let me = rustix::process::getpid();
        let started = start_file(&real, &arg0, Vec::new(), me);
        assert!(
            matches!(&started, Err(Unbegun::Program(e)) if e.kind() == io::ErrorKind::NotFound),
            "{started:?}"
        );
    }

    /// A file whose child cannot die with its supervisor never reaches its
    /// `execve`: that is ral's failure, though the file is runnable.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_that_cannot_die_with_its_supervisor_is_ours_not_the_programs() {
        let (real, arg0) = file("/bin/sh");
        let started = start_file(&real, &arg0, Vec::new(), a_stranger());
        assert!(
            matches!(
                &started,
                Err(Unbegun::Ours(LaunchFailure::Launcher(
                    LauncherFailure::ChildSetup(_)
                )))
            ),
            "{started:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_that_begins_is_spawned() {
        let (real, arg0) = file("/bin/sh");
        let me = rustix::process::getpid();
        let pid = start_file(&real, &arg0, vec!["-c".into(), "exit 3".into()], me)
            .expect("a runnable file spawns");
        assert_eq!(crate::process::reap(pid).expect("reaped").code(), Some(3));
    }

    /// A tool that cannot begin is refused with its cause; it never runs,
    /// and never reads as a program that exited 126.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_tool_that_cannot_begin_is_refused_not_exited() {
        let (_, started) = start_tool("true", Vec::new(), &[], a_stranger(), witness());
        assert!(
            matches!(
                &started,
                Err(Unbegun::Ours(LaunchFailure::Launcher(
                    LauncherFailure::ChildSetup(_)
                )))
            ),
            "{started:?}"
        );
    }
}
