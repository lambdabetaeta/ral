//! What the OS reported when a child ended, and the user-facing failure it
//! becomes: a `CommandFailure` travels on as `Status::Process` in `types/error.rs`.

/// The NTSTATUS for Windows' SIGPIPE analogue, which reaches us through
/// `ExitStatus::code()` as an ordinary `Exited` code rather than as a signal.
#[cfg(windows)]
const STATUS_PIPE_CLOSING: u32 = 0xC000_00B1;

/// One wording for "command not found", shared with `check_existence` in
/// `runtime/command/vet.rs` so the pre-spawn probe and the spawn failure agree.
pub(crate) fn not_found_hint(cmd: &str) -> String {
    format!("{cmd}: command not found")
}

/// An OS signal number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    number: i32,
}

impl Signal {
    pub const fn new(number: i32) -> Self {
        Self { number }
    }

    pub const fn number(self) -> i32 {
        self.number
    }

    /// The conventional name, for the standard signals only.
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

    /// SIGTTIN: a background process tried to read the controlling terminal.
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

    /// SIGTTOU: a background process tried to reconfigure the controlling terminal.
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

    /// The shell convention for a signal death, 128 + N.
    pub fn user_exit_code(self) -> i32 {
        128 + self.number
    }
}

/// What the OS reported when a process stopped or exited.
///
/// `Stopped` is a live suspended child: park its pgid as a job and resume it
/// with SIGCONT rather than reap it.  It arises only for an interactive
/// foreground wait — `handle_stopped` in `process/signal/unix.rs` otherwise
/// kills the stopped child and reports `StoppedThenKilled`.
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
    /// Classify a platform `ExitStatus` without collapsing signal death into a code.
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

    pub fn to_user_exit_code(self) -> i32 {
        match self {
            Self::Exited(code) | Self::NativeCode(code) => code,
            Self::Signaled(sig) | Self::Stopped(sig) => sig.user_exit_code(),
            Self::StoppedThenKilled { stopped_by, .. } => stopped_by.user_exit_code(),
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Exited(0) | Self::NativeCode(0))
    }

    /// SIGPIPE, or the Windows exit code that stands in for it.
    pub fn is_broken_pipe(self) -> bool {
        match self {
            Self::Signaled(sig) => sig.is_sigpipe(),
            #[cfg(windows)]
            Self::Exited(code) | Self::NativeCode(code) => {
                code.cast_unsigned() == STATUS_PIPE_CLOSING
            }
            _ => false,
        }
    }
}

/// Why a spawn failed before there was a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnFailure {
    NotFound,
    PermissionDenied,
    Io(String),
}

/// What a `WaitOutcome` amounts to for the user.
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
    /// The failure an outcome amounts to, or `None` for success — a non-final
    /// pipeline stage is forgiven a broken pipe, since its reader simply left.
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

    /// The follow-up line under the message.  `None` for a plain exit code, where
    /// `Error::from_command_failure` falls back to the user's exit-hints table.
    pub fn default_hint(&self, cmd: &str) -> Option<String> {
        match self {
            Self::ExitCode(_) | Self::Spawn(_) => None,
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
        }
    }

    /// The conventional numeric code — POSIX's 127 for not found, 126 for cannot-run.
    pub fn to_user_exit_code(&self) -> i32 {
        match self {
            Self::ExitCode(code) => *code,
            Self::Signal(sig) => sig.user_exit_code(),
            Self::StoppedByJobControl { stop_signal, .. } => stop_signal.user_exit_code(),
            Self::Spawn(SpawnFailure::NotFound | SpawnFailure::Io(_)) => 127,
            Self::Spawn(SpawnFailure::PermissionDenied) => 126,
        }
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
        let outcome = WaitOutcome::Exited(STATUS_PIPE_CLOSING.cast_signed());
        assert!(outcome.is_broken_pipe());
        assert_eq!(CommandFailure::from_outcome(outcome, true), None);
        assert!(CommandFailure::from_outcome(outcome, false).is_some());
    }
}
