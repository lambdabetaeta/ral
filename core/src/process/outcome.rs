//! What the OS reported when a child ended, and the user-facing failure it
//! becomes: a `CommandFailure` travels on as `Status::Process` in `types/error.rs`.

use super::cancel::CancelCause;

/// The exit code ral's every Windows kill uses — ASCII "RALK" — there being
/// no signal a terminated Windows process could carry.  A child exiting with
/// this exact code, or with Ctrl-Break's, while a cause was sent would be
/// attributed too; Unix has no such residue, a signal death being
/// uncounterfeitable by an exit.
#[cfg(windows)]
pub(crate) const KILL_EXIT_CODE: i32 = 0x5241_4c4b;

/// An OS signal number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    number: i32,
}

impl Signal {
    pub const fn new(number: i32) -> Self {
        Self { number }
    }

    pub(crate) const fn number(self) -> i32 {
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
    pub(crate) fn display(self) -> String {
        match self.name() {
            Some(name) => format!("{} ({name})", self.number),
            None => self.number.to_string(),
        }
    }

    pub(crate) fn is_sigkill(self) -> bool {
        #[cfg(unix)]
        {
            self.number == libc::SIGKILL
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            false
        }
    }

    pub(crate) fn is_sigsegv(self) -> bool {
        #[cfg(unix)]
        {
            self.number == libc::SIGSEGV
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            false
        }
    }
}

/// What the OS reported when a process ended.  Never a stop: the reaper
/// answers every stop itself, with `SIGCONT`, and posts nothing for it, so
/// no terminal reader is ever handed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    Exited(i32),
    #[cfg_attr(not(unix), allow(dead_code))]
    Signaled(Signal),
    NativeCode(i32),
}

/// How a child's end reads once ral's own teardown is accounted for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChildEnd {
    Failed(CommandFailure),
    Cancelled(CancelCause),
}

impl WaitOutcome {
    /// Classify a platform `ExitStatus` without collapsing signal death into a code.
    pub(crate) fn from_exit_status(status: std::process::ExitStatus) -> Self {
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

    pub(crate) fn is_success(self) -> bool {
        matches!(self, Self::Exited(0) | Self::NativeCode(0))
    }

    /// The signal this death was by: `Signaled(s)`, or `128 + n` behind an
    /// envelope, bwrap reporting its payload's signal death as its own exit;
    /// an unenveloped `Exited(128 + n)` is the child's choice.
    #[cfg(unix)]
    pub(crate) fn death(self, enveloped: bool) -> Option<Signal> {
        match self {
            Self::Signaled(signal) => Some(signal),
            Self::Exited(code) if enveloped && code > 128 => Some(Signal::new(code - 128)),
            _ => None,
        }
    }

    /// The exit status a console event or ral's kill leaves, and nothing
    /// else: every other exit is the child's choice.
    #[cfg(windows)]
    pub(crate) fn death(self, _enveloped: bool) -> Option<Signal> {
        use windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT;
        match self {
            Self::Exited(code @ (KILL_EXIT_CODE | STATUS_CONTROL_C_EXIT)) => {
                Some(Signal::new(code))
            }
            _ => None,
        }
    }

    /// Whether this death is by one of `cause`'s
    /// [`signals_of`](crate::process::signals_of).
    fn is_death_by(self, cause: CancelCause, enveloped: bool) -> bool {
        self.death(enveloped)
            .is_some_and(|signal| crate::process::signals_of(cause).any(|of| of == signal))
    }

    /// The end this outcome amounts to, or `None` for success, given the
    /// strongest `cause` in force when the child ended.  A death by one of
    /// that cause's signals is the cause's — forgiven outright for
    /// `ReaderGone`, the collector reclaiming a producer nobody read from —
    /// and anything else stays as the OS reported it: no death mints a cause.
    pub(crate) fn classify(self, cause: Option<CancelCause>, enveloped: bool) -> Option<ChildEnd> {
        match cause.filter(|&cause| self.is_death_by(cause, enveloped)) {
            Some(CancelCause::ReaderGone) => None,
            Some(cause) => Some(ChildEnd::Cancelled(cause)),
            None => match self {
                Self::Exited(0) | Self::NativeCode(0) => None,
                Self::Exited(code) | Self::NativeCode(code) => {
                    Some(ChildEnd::Failed(CommandFailure::ExitCode(code)))
                }
                Self::Signaled(sig) => Some(ChildEnd::Failed(CommandFailure::Signal(sig))),
            },
        }
    }
}

/// Why a spawn failed before there was a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnFailure {
    NotFound,
    /// `found` is the file a `PATH` walk stopped at, when it differs from the
    /// name the user typed.
    PermissionDenied {
        found: Option<std::path::PathBuf>,
    },
    Io(String),
}

/// What a `WaitOutcome` amounts to for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandFailure {
    ExitCode(i32),
    Signal(Signal),
    Spawn(SpawnFailure),
}

impl CommandFailure {
    pub fn message(&self, cmd: &str) -> String {
        match self {
            #[cfg(windows)]
            Self::ExitCode(windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT) => {
                format!("{cmd}: ended by Ctrl-C")
            }
            Self::ExitCode(code) => format!("{cmd}: exited with status {code}"),
            Self::Signal(sig) => format!("{cmd}: killed by signal {}", sig.display()),
            Self::Spawn(SpawnFailure::NotFound) => format!("{cmd}: command not found"),
            Self::Spawn(SpawnFailure::PermissionDenied { found: None }) => {
                format!("{cmd}: permission denied")
            }
            Self::Spawn(SpawnFailure::PermissionDenied { found: Some(path) }) => format!(
                "{cmd}: permission denied ({} is not executable)",
                path.display()
            ),
            Self::Spawn(SpawnFailure::Io(msg)) => format!("{cmd}: {msg}"),
        }
    }

    /// The follow-up line under the message.  `None` for a plain exit code, where
    /// `Error::from_command_failure` falls back to the user's exit-hints table.
    pub(crate) fn default_hint(&self) -> Option<String> {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Status;

    /// What `sent` makes of a death it caused: its own cancellation, or, for
    /// `ReaderGone`, the collector's forgiven kill.
    fn attributed(cause: CancelCause) -> Option<ChildEnd> {
        (cause != CancelCause::ReaderGone).then_some(ChildEnd::Cancelled(cause))
    }

    #[cfg(unix)]
    fn signaled(n: i32) -> WaitOutcome {
        WaitOutcome::Signaled(Signal::new(n))
    }

    #[cfg(unix)]
    fn died_of(n: i32) -> ChildEnd {
        ChildEnd::Failed(CommandFailure::Signal(Signal::new(n)))
    }

    #[cfg(unix)]
    #[test]
    fn signal_names_follow_platform_constants() {
        assert_eq!(Signal::new(libc::SIGKILL).display(), "9 (SIGKILL)");
        assert_eq!(Signal::new(libc::SIGTTOU).name(), Some("SIGTTOU"));
    }

    /// Every status's number, from the one table that holds them.
    #[test]
    fn every_status_has_its_code() {
        for (status, code) in [
            (Status::Raised(7), 7),
            (Status::Process(CommandFailure::ExitCode(3)), 3),
            (
                Status::Process(CommandFailure::Signal(Signal::new(11))),
                139,
            ),
            (
                Status::Process(CommandFailure::Spawn(SpawnFailure::NotFound)),
                127,
            ),
            (
                Status::Process(CommandFailure::Spawn(SpawnFailure::PermissionDenied {
                    found: None,
                })),
                126,
            ),
            (
                Status::Process(CommandFailure::Spawn(SpawnFailure::Io("boom".into()))),
                126,
            ),
            (Status::Cancelled(CancelCause::Interrupt), 130),
            (Status::Cancelled(CancelCause::Explicit), 143),
            (Status::Cancelled(CancelCause::Deadline), 124),
            (Status::Cancelled(CancelCause::Terminate), 143),
            (Status::Cancelled(CancelCause::RootAbort), 131),
            (Status::Cancelled(CancelCause::ReaderGone), 141),
        ] {
            assert_eq!(status.code(), code, "{status:?}");
        }
    }

    #[test]
    fn ordinary_exit_and_signal_death_stay_distinct() {
        assert_eq!(
            WaitOutcome::Exited(137).classify(None, false),
            Some(ChildEnd::Failed(CommandFailure::ExitCode(137)))
        );
        assert_eq!(
            WaitOutcome::Signaled(Signal::new(9)).classify(None, false),
            Some(ChildEnd::Failed(CommandFailure::Signal(Signal::new(9))))
        );
    }

    /// A death by any of a cause's signals — its grace signal, its key, the
    /// SIGKILL that ends every teardown — is the cause's, and an enveloped
    /// `Exited(128 + n)` for the same `n` reads alike.
    #[cfg(unix)]
    #[test]
    fn a_death_by_a_causes_signal_is_the_causes() {
        use libc::{SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGTERM};
        for (cause, expected) in [
            (CancelCause::Interrupt, vec![SIGINT, SIGKILL]),
            (CancelCause::Explicit, vec![SIGTERM, SIGKILL]),
            (CancelCause::Deadline, vec![SIGTERM, SIGKILL]),
            (CancelCause::Terminate, vec![SIGTERM, SIGHUP, SIGKILL]),
            (CancelCause::RootAbort, vec![SIGKILL, SIGQUIT]),
            (CancelCause::ReaderGone, vec![SIGKILL]),
        ] {
            let actual: std::collections::BTreeSet<i32> = crate::process::signals_of(cause)
                .map(Signal::number)
                .collect();
            assert_eq!(
                actual,
                expected.iter().copied().collect(),
                "{cause:?}'s signals"
            );
            for n in expected {
                assert_eq!(
                    signaled(n).classify(Some(cause), false),
                    attributed(cause),
                    "{cause:?}, signal {n}"
                );
                assert_eq!(
                    WaitOutcome::Exited(128 + n).classify(Some(cause), true),
                    attributed(cause),
                    "{cause:?}, enveloped exit {}",
                    128 + n
                );
            }
        }
    }

    /// A signal none of the cause's is the child's own
    /// death, whatever was in force when it landed: a segfault in the grace
    /// window keeps the segfault's words.
    #[cfg(unix)]
    #[test]
    fn a_death_off_the_causes_teardown_stays_a_signal() {
        for cause in CancelCause::ALL {
            assert_eq!(
                signaled(libc::SIGSEGV).classify(Some(cause), false),
                Some(died_of(libc::SIGSEGV)),
                "{cause:?}"
            );
            assert_eq!(
                WaitOutcome::Exited(128 + libc::SIGSEGV).classify(Some(cause), true),
                Some(ChildEnd::Failed(CommandFailure::ExitCode(
                    128 + libc::SIGSEGV
                ))),
                "{cause:?}, enveloped"
            );
        }
        assert_eq!(
            WaitOutcome::Exited(128 + libc::SIGTERM).classify(Some(CancelCause::ReaderGone), true),
            Some(ChildEnd::Failed(CommandFailure::ExitCode(
                128 + libc::SIGTERM
            )))
        );
    }

    /// No death mints a cause: with none in force every signal death is the
    /// child's own, a key's included, and a cause is kept only for its own
    /// signals.
    #[cfg(unix)]
    #[test]
    fn a_death_is_a_cancellation_only_by_a_cause_in_force() {
        for n in 1..=31 {
            assert_eq!(signaled(n).classify(None, false), Some(died_of(n)), "{n}");
        }
        assert_eq!(
            signaled(libc::SIGQUIT).classify(Some(CancelCause::RootAbort), false),
            Some(ChildEnd::Cancelled(CancelCause::RootAbort))
        );
        assert_eq!(
            signaled(libc::SIGINT).classify(Some(CancelCause::Deadline), false),
            Some(died_of(libc::SIGINT))
        );
        assert_eq!(
            signaled(libc::SIGTERM).classify(Some(CancelCause::ReaderGone), false),
            Some(died_of(libc::SIGTERM))
        );
    }

    /// Only an envelope reports its payload's signal death as an exit; an
    /// unenveloped `Exited(143)` is the child's own choice, cause or none.
    #[cfg(unix)]
    #[test]
    fn a_propagated_exit_is_attributed_only_enveloped_and_with_a_cause() {
        let code = 128 + libc::SIGTERM;
        let own = Some(ChildEnd::Failed(CommandFailure::ExitCode(code)));
        assert_eq!(
            WaitOutcome::Exited(code).classify(Some(CancelCause::Explicit), true),
            Some(ChildEnd::Cancelled(CancelCause::Explicit))
        );
        assert_eq!(WaitOutcome::Exited(code).classify(None, true), own);
        assert_eq!(
            WaitOutcome::Exited(code).classify(Some(CancelCause::Explicit), false),
            own
        );
    }

    /// A signal no cause sent stays a signal, and keeps its words.
    #[cfg(unix)]
    #[test]
    fn a_foreign_signal_is_still_reported_as_a_signal() {
        assert_eq!(
            signaled(libc::SIGKILL).classify(None, false),
            Some(died_of(libc::SIGKILL))
        );
        assert_eq!(
            CommandFailure::Signal(Signal::new(libc::SIGKILL)).message("sh"),
            "sh: killed by signal 9 (SIGKILL)"
        );
        assert_eq!(
            CommandFailure::Signal(Signal::new(libc::SIGSEGV))
                .default_hint()
                .as_deref(),
            Some("the process crashed with a segmentation fault")
        );
    }

    /// A zombie's exit status cannot be overwritten by a kill that arrives
    /// too late: an exit is always kept, whoever ended the stage, which is
    /// exactly what makes a real failure impossible to launder through
    /// forgiveness.
    #[test]
    fn an_exit_status_is_kept_even_when_ral_ended_the_stage() {
        for cause in CancelCause::ALL {
            assert_eq!(
                WaitOutcome::Exited(3).classify(Some(cause), false),
                Some(ChildEnd::Failed(CommandFailure::ExitCode(3))),
                "{cause:?}"
            );
        }
    }

    /// SIGPIPE carries no special case: with no interior edge left to deliver
    /// it, a pipe of the stage's own making that breaks is its own failure,
    /// whoever ended the stage.
    #[cfg(unix)]
    #[test]
    fn a_sigpipe_death_is_kept_under_every_ending() {
        for sent in [Some(CancelCause::ReaderGone), None] {
            assert_eq!(
                signaled(libc::SIGPIPE).classify(sent, false),
                Some(died_of(libc::SIGPIPE))
            );
        }
    }

    /// A cancellation in force outranks forgiveness: `Option<CancelCause>`
    /// orders the stronger cause above `ReaderGone`, so the SIGKILL death it
    /// attributes is kept rather than forgiven.
    #[cfg(unix)]
    #[test]
    fn a_stronger_ending_outranks_forgiveness() {
        let sent = Some(CancelCause::ReaderGone).max(Some(CancelCause::RootAbort));
        assert_eq!(sent, Some(CancelCause::RootAbort));
        assert_eq!(
            signaled(libc::SIGKILL).classify(sent, false),
            Some(ChildEnd::Cancelled(CancelCause::RootAbort))
        );
    }

    /// ral's kill exit code ends every Windows teardown, so it is the sent
    /// cause's whichever cause that was.
    #[cfg(windows)]
    #[test]
    fn the_kill_exit_code_is_attributed_to_the_cause_sent() {
        for cause in CancelCause::ALL {
            assert_eq!(
                WaitOutcome::Exited(KILL_EXIT_CODE).classify(Some(cause), false),
                attributed(cause),
                "{cause:?}"
            );
        }
        assert_eq!(
            WaitOutcome::Exited(KILL_EXIT_CODE).classify(None, false),
            Some(ChildEnd::Failed(CommandFailure::ExitCode(KILL_EXIT_CODE)))
        );
    }

    /// Windows' Ctrl-C death status is the interrupt's only while the
    /// interrupt is in force; otherwise it is the child's exit, told as Ctrl-C.
    #[cfg(windows)]
    #[test]
    fn the_ctrl_c_exit_status_is_an_interrupt_only_in_force() {
        use windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT;
        let ctrl_c = WaitOutcome::Exited(STATUS_CONTROL_C_EXIT);
        assert_eq!(
            ctrl_c.classify(Some(CancelCause::Interrupt), false),
            attributed(CancelCause::Interrupt)
        );
        assert_eq!(
            ctrl_c.classify(None, false),
            Some(ChildEnd::Failed(CommandFailure::ExitCode(
                STATUS_CONTROL_C_EXIT
            )))
        );
        assert_eq!(
            CommandFailure::ExitCode(STATUS_CONTROL_C_EXIT).message("ping"),
            "ping: ended by Ctrl-C"
        );
    }

    /// Ctrl-Break opens every graceful teardown, so the death it leaves is
    /// each graceful cause's — a `Deadline` teardown reads as the time limit,
    /// never as an exit status — and a kill-only cause's child's own.
    #[cfg(windows)]
    #[test]
    fn a_ctrl_break_death_is_every_graceful_causes() {
        use windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT;
        let ctrl_break = WaitOutcome::Exited(STATUS_CONTROL_C_EXIT);
        assert_eq!(
            ctrl_break.classify(Some(CancelCause::Deadline), false),
            Some(ChildEnd::Cancelled(CancelCause::Deadline))
        );
        for cause in [
            CancelCause::Interrupt,
            CancelCause::Explicit,
            CancelCause::Terminate,
        ] {
            assert_eq!(
                ctrl_break.classify(Some(cause), false),
                attributed(cause),
                "{cause:?}"
            );
        }
        for cause in [CancelCause::ReaderGone, CancelCause::RootAbort] {
            assert_eq!(
                ctrl_break.classify(Some(cause), false),
                Some(ChildEnd::Failed(CommandFailure::ExitCode(
                    STATUS_CONTROL_C_EXIT
                ))),
                "{cause:?}"
            );
        }
    }
}
