//! Structured process outcomes and user-facing command failures.

/// `STATUS_PIPE_CLOSING` (NTSTATUS): a Windows process died writing to a
/// pipe whose read end has closed — the moral equivalent of SIGPIPE.  The
/// kernel surfaces it both as a native exit code and, through
/// `ExitStatus::code()`, as an ordinary `Exited` code.
#[cfg(windows)]
const STATUS_PIPE_CLOSING: u32 = 0xC000_00B1;

/// Format the "command not found" message used by both
/// [`CommandFailure::Spawn(SpawnFailure::NotFound)`] rendering and the
/// resolver's pre-spawn check for an unresolvable external.
pub(crate) fn not_found_hint(cmd: &str) -> String {
    format!("{cmd}: command not found")
}

/// OS signal number with local formatting helpers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    number: i32,
}

impl Signal {
    /// Construct a signal wrapper from its numeric value.
    pub const fn new(number: i32) -> Self {
        Self { number }
    }

    /// Return the raw signal number.
    pub const fn number(self) -> i32 {
        self.number
    }

    /// Return the conventional signal name when known on this platform.
    #[cfg(unix)]
    pub fn name(self) -> Option<&'static str> {
        let n = self.number;
        let names = [
            (libc::SIGHUP, "SIGHUP"),
            (libc::SIGINT, "SIGINT"),
            (libc::SIGQUIT, "SIGQUIT"),
            (libc::SIGILL, "SIGILL"),
            (libc::SIGTRAP, "SIGTRAP"),
            (libc::SIGABRT, "SIGABRT"),
            (libc::SIGBUS, "SIGBUS"),
            (libc::SIGFPE, "SIGFPE"),
            (libc::SIGKILL, "SIGKILL"),
            (libc::SIGUSR1, "SIGUSR1"),
            (libc::SIGSEGV, "SIGSEGV"),
            (libc::SIGUSR2, "SIGUSR2"),
            (libc::SIGPIPE, "SIGPIPE"),
            (libc::SIGALRM, "SIGALRM"),
            (libc::SIGTERM, "SIGTERM"),
            (libc::SIGCHLD, "SIGCHLD"),
            (libc::SIGCONT, "SIGCONT"),
            (libc::SIGSTOP, "SIGSTOP"),
            (libc::SIGTSTP, "SIGTSTP"),
            (libc::SIGTTIN, "SIGTTIN"),
            (libc::SIGTTOU, "SIGTTOU"),
            (libc::SIGURG, "SIGURG"),
            (libc::SIGXCPU, "SIGXCPU"),
            (libc::SIGXFSZ, "SIGXFSZ"),
            (libc::SIGVTALRM, "SIGVTALRM"),
            (libc::SIGPROF, "SIGPROF"),
            (libc::SIGWINCH, "SIGWINCH"),
        ];
        names
            .iter()
            .find_map(|(sig, name)| (*sig == n).then_some(*name))
    }

    /// Return no conventional name on platforms without Unix signals.
    #[cfg(not(unix))]
    pub fn name(self) -> Option<&'static str> {
        let _ = self;
        None
    }

    /// Format the signal as `N (SIGNAME)` when known.
    pub fn display(self) -> String {
        match self.name() {
            Some(name) => format!("{} ({name})", self.number),
            None => self.number.to_string(),
        }
    }

    /// True when the signal is SIGPIPE.
    pub fn is_sigpipe(self) -> bool {
        #[cfg(unix)]
        {
            self.number == libc::SIGPIPE
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// True for job-control stop signals.
    pub fn is_job_control_stop(self) -> bool {
        #[cfg(unix)]
        {
            matches!(
                self.number,
                n if n == libc::SIGSTOP
                    || n == libc::SIGTSTP
                    || n == libc::SIGTTIN
                    || n == libc::SIGTTOU
            )
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// True when a background process tried to read from its controlling
    /// terminal (SIGTTIN).
    pub fn is_background_tty_input(self) -> bool {
        #[cfg(unix)]
        {
            self.number == libc::SIGTTIN
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// True when a background process tried to configure its controlling
    /// terminal (SIGTTOU).
    pub fn is_background_tty_config(self) -> bool {
        #[cfg(unix)]
        {
            self.number == libc::SIGTTOU
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// True for user-initiated stops (SIGSTOP, SIGTSTP).  The numeric
    /// values diverge across platforms (Linux 19/20, macOS 17/18), so
    /// matching on libc constants is required for portability.
    pub fn is_user_initiated_stop(self) -> bool {
        #[cfg(unix)]
        {
            self.number == libc::SIGSTOP || self.number == libc::SIGTSTP
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// True for SIGKILL.
    pub fn is_sigkill(self) -> bool {
        #[cfg(unix)]
        {
            self.number == libc::SIGKILL
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// True for SIGSEGV.
    pub fn is_sigsegv(self) -> bool {
        #[cfg(unix)]
        {
            self.number == libc::SIGSEGV
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Conventional shell exit code for a signal death.
    pub fn user_exit_code(self) -> i32 {
        128 + self.number
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn signal_names_follow_platform_constants() {
        assert_eq!(Signal::new(libc::SIGKILL).display(), "9 (SIGKILL)");
        assert_eq!(Signal::new(libc::SIGTTOU).name(), Some("SIGTTOU"));
        assert!(Signal::new(libc::SIGSTOP).is_job_control_stop());
        assert!(Signal::new(libc::SIGPIPE).is_sigpipe());
    }

    #[test]
    fn ordinary_exit_and_signal_death_stay_distinct() {
        assert_eq!(
            CommandFailure::from_outcome(WaitOutcome::Exited(137), false),
            Some(CommandFailure::ExitCode(137))
        );
        assert_eq!(
            CommandFailure::from_outcome(WaitOutcome::Signaled(Signal::new(9)), false),
            Some(CommandFailure::Signal(Signal::new(9)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn stopped_then_killed_reports_the_stop_status() {
        let outcome = WaitOutcome::StoppedThenKilled {
            stopped_by: Signal::new(libc::SIGSTOP),
            killed_by: Signal::new(libc::SIGKILL),
        };
        assert_eq!(outcome.to_user_exit_code(), 128 + libc::SIGSTOP);
        let failure = CommandFailure::from_outcome(outcome, false).unwrap();
        assert!(failure.message("cmd").contains("stopped by signal"));
        assert_eq!(failure.to_user_exit_code(), 128 + libc::SIGSTOP);
    }

    #[cfg(unix)]
    #[test]
    fn non_final_sigpipe_is_success() {
        let outcome = WaitOutcome::Signaled(Signal::new(libc::SIGPIPE));
        assert_eq!(CommandFailure::from_outcome(outcome, true), None);
        assert!(CommandFailure::from_outcome(outcome, false).is_some());
    }

    #[cfg(windows)]
    #[test]
    fn non_final_pipe_closing_is_success() {
        // STATUS_PIPE_CLOSING reaches us as an ordinary `Exited` code on
        // Windows; a non-final pipeline stage that died this way is the
        // SIGPIPE analogue and must be forgiven.
        let outcome = WaitOutcome::Exited(STATUS_PIPE_CLOSING as i32);
        assert!(outcome.is_broken_pipe());
        assert_eq!(CommandFailure::from_outcome(outcome, true), None);
        assert!(CommandFailure::from_outcome(outcome, false).is_some());
    }
}

/// What the OS reported when a process stopped or exited.
///
/// `Stopped` is the parked-job outcome: the child was suspended (SIGTSTP /
/// SIGSTOP / SIGTTIN / SIGTTOU) and the kernel still holds its slot.  The
/// caller is expected to register the pgid in the job table and resume it
/// later via SIGCONT — not to reap it.  `StoppedThenKilled` is the legacy
/// path used when no pgid is available to register (the child is killed
/// instead).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Exited(i32),
    Signaled(Signal),
    Stopped(Signal),
    StoppedThenKilled {
        stopped_by: Signal,
        killed_by: Signal,
    },
    NativeCode(i32),
}

impl WaitOutcome {
    /// Convert a platform `ExitStatus` without losing signal information.
    pub fn from_exit_status(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(code) = status.code() {
                Self::Exited(code)
            } else if let Some(sig) = status.signal() {
                Self::Signaled(Signal::new(sig))
            } else {
                Self::NativeCode(1)
            }
        }
        #[cfg(not(unix))]
        {
            match status.code() {
                Some(code) => Self::Exited(code),
                None => Self::NativeCode(1),
            }
        }
    }

    /// Reduce the outcome to the user-visible numeric exit status.
    pub fn to_user_exit_code(self) -> i32 {
        match self {
            Self::Exited(code) | Self::NativeCode(code) => code,
            Self::Signaled(sig) => sig.user_exit_code(),
            Self::Stopped(sig) => sig.user_exit_code(),
            Self::StoppedThenKilled { stopped_by, .. } => stopped_by.user_exit_code(),
        }
    }

    /// True when the process is parked in a stopped state (not yet exited).
    pub fn is_stopped(self) -> bool {
        matches!(self, Self::Stopped(_))
    }

    /// True when the process exited successfully.
    pub fn is_success(self) -> bool {
        matches!(self, Self::Exited(0) | Self::NativeCode(0))
    }

    /// True when the outcome represents a broken pipe.
    pub fn is_broken_pipe(self) -> bool {
        match self {
            Self::Signaled(sig) => sig.is_sigpipe(),
            #[cfg(windows)]
            Self::Exited(code) | Self::NativeCode(code) => (code as u32) == STATUS_PIPE_CLOSING,
            _ => false,
        }
    }
}

/// Structured spawn failures for external commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnFailure {
    NotFound,
    PermissionDenied,
    Io(String),
}

/// User-facing failure shape for an external command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandFailure {
    ExitCode(i32),
    Signal(Signal),
    StoppedByJobControl {
        stop_signal: Signal,
        killed_by: Signal,
    },
    Spawn(SpawnFailure),
}

impl CommandFailure {
    /// Convert a process outcome into a user-facing failure, forgiving
    /// non-final SIGPIPE as pipeline success.
    pub fn from_outcome(outcome: WaitOutcome, is_pipeline_non_final: bool) -> Option<Self> {
        match outcome {
            WaitOutcome::Exited(0) => None,
            #[cfg(windows)]
            WaitOutcome::Exited(_) if is_pipeline_non_final && outcome.is_broken_pipe() => None,
            WaitOutcome::Exited(code) => Some(Self::ExitCode(code)),
            WaitOutcome::Signaled(sig) if is_pipeline_non_final && sig.is_sigpipe() => None,
            WaitOutcome::Signaled(sig) => Some(Self::Signal(sig)),
            WaitOutcome::Stopped(_) => unreachable!(
                "WaitOutcome::Stopped must be intercepted by the caller \
                 (RunningChild::wait) and surfaced as Escape::Stopped \
                 before reaching CommandFailure::from_outcome"
            ),
            WaitOutcome::StoppedThenKilled {
                stopped_by,
                killed_by,
            } => Some(Self::StoppedByJobControl {
                stop_signal: stopped_by,
                killed_by,
            }),
            WaitOutcome::NativeCode(code)
                if is_pipeline_non_final && WaitOutcome::NativeCode(code).is_broken_pipe() =>
            {
                None
            }
            WaitOutcome::NativeCode(0) => None,
            WaitOutcome::NativeCode(code) => Some(Self::ExitCode(code)),
        }
    }

    /// Render the primary command failure message.
    pub fn message(&self, cmd: &str) -> String {
        match self {
            Self::ExitCode(code) => format!("{cmd}: exited with status {code}"),
            Self::Signal(sig) => format!("{cmd}: killed by signal {}", sig.display()),
            Self::StoppedByJobControl { stop_signal, .. }
                if stop_signal.is_background_tty_input() =>
            {
                format!(
                    "{cmd}: stopped by signal {} while reading from the terminal",
                    stop_signal.display()
                )
            }
            Self::StoppedByJobControl { stop_signal, .. }
                if stop_signal.is_background_tty_config() =>
            {
                format!(
                    "{cmd}: stopped by signal {} while configuring the terminal",
                    stop_signal.display()
                )
            }
            Self::StoppedByJobControl { stop_signal, .. } => format!(
                "{cmd}: stopped by signal {}; ral killed the pipeline",
                stop_signal.display()
            ),
            Self::Spawn(SpawnFailure::NotFound) => not_found_hint(cmd),
            Self::Spawn(SpawnFailure::PermissionDenied) => format!("{cmd}: permission denied"),
            Self::Spawn(SpawnFailure::Io(msg)) => format!("{cmd}: {msg}"),
        }
    }

    /// Return a default hint when one is useful.
    pub fn default_hint(&self, cmd: &str) -> Option<String> {
        match self {
            Self::ExitCode(_) | Self::Spawn(SpawnFailure::NotFound) => None,
            Self::Signal(sig) if sig.is_sigkill() => Some(
                "the process was killed with SIGKILL; the kernel or another process may have terminated it"
                    .to_string(),
            ),
            Self::Signal(sig) if sig.is_sigsegv() => {
                Some("the process crashed with a segmentation fault".to_string())
            }
            Self::Signal(sig) => Some(format!("the process terminated from {}", sig.display())),
            Self::StoppedByJobControl { stop_signal, .. } if stop_signal.is_background_tty_input() => Some(
                format!(
                    "ral killed the pipeline because {cmd} tried to read the terminal from a background process group. Is an earlier stage internal, an alias, a handler, or a builtin? Use `explain {cmd}` to inspect command resolution."
                ),
            ),
            Self::StoppedByJobControl { stop_signal, .. } if stop_signal.is_background_tty_config() => Some(
                format!(
                    "ral killed the pipeline because {cmd} tried to configure the terminal from a background process group. This often means an interactive tail ran in a mixed pipeline."
                ),
            ),
            Self::StoppedByJobControl { .. } => Some(
                "ral's job control doesn't reach inside a pipeline; stopping one stage still tears the whole pipeline down."
                    .to_string(),
            ),
            Self::Spawn(_) => None,
        }
    }

    /// Reduce the failure to the conventional numeric exit code.
    pub fn to_user_exit_code(&self) -> i32 {
        match self {
            Self::ExitCode(code) => *code,
            Self::Signal(sig) => sig.user_exit_code(),
            Self::StoppedByJobControl { stop_signal, .. } => stop_signal.user_exit_code(),
            Self::Spawn(SpawnFailure::NotFound) => 127,
            Self::Spawn(SpawnFailure::PermissionDenied) => 126,
            Self::Spawn(SpawnFailure::Io(_)) => 127,
        }
    }
}
