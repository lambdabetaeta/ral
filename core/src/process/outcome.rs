//! What the OS reported when a child ended, and the failure record it
//! becomes.

use super::cancel::CancelCause;
use crate::fact::Failure;
use std::num::NonZeroU8;

/// The exit code ral's every Windows kill uses (ASCII "RALK"), there being
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

    /// The signal a shell status reports, if it names one this platform has.
    #[cfg(target_os = "linux")]
    pub(crate) fn of_status(code: i32) -> Option<Self> {
        code.checked_sub(128)
            .filter(|number| (1..=libc::SIGRTMAX()).contains(number))
            .map(Self::new)
    }
}

impl std::fmt::Display for Signal {
    /// `N (SIGNAME)` where the platform names it, else `N`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(unix)]
        if let Ok(named) = nix::sys::signal::Signal::try_from(self.number) {
            return write!(f, "{} ({named})", self.number);
        }
        write!(f, "{}", self.number)
    }
}

/// How a process ended, as the OS reported it, or, behind a Linux envelope,
/// as the supervisor reported its program.  Never a stop: the reaper answers
/// every stop itself, with `SIGCONT`, and posts nothing for it, so no
/// terminal reader is ever handed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    Exited(i32),
    #[cfg_attr(not(unix), allow(dead_code))]
    Signaled(Signal),
}

impl WaitOutcome {
    /// Classify an ended process's `ExitStatus`.
    #[cfg(any(windows, target_os = "linux"))]
    pub(crate) fn from_exit_status(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
            return Self::Signaled(Signal::new(signal));
        }
        // Only a stop, which no ended process leaves, has neither.
        Self::Exited(status.code().unwrap_or(1))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn is_success(self) -> bool {
        matches!(self, Self::Exited(0))
    }

    /// The signal this death was by.
    #[cfg(unix)]
    pub(crate) fn death(self) -> Option<Signal> {
        match self {
            Self::Signaled(signal) => Some(signal),
            Self::Exited(_) => None,
        }
    }

    /// The exit status a console event or ral's kill leaves, and nothing
    /// else: every other exit is the child's choice.
    #[cfg(windows)]
    pub(crate) fn death(self) -> Option<Signal> {
        use windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT;
        match self {
            Self::Exited(code @ (KILL_EXIT_CODE | STATUS_CONTROL_C_EXIT)) => {
                Some(Signal::new(code))
            }
            _ => None,
        }
    }

    /// `cause`, the strongest in force when the child ended, where this is a
    /// death by one of its [`signals_of`](crate::process::signals_of): the
    /// cancellation it was.  No death mints a cause.
    pub(crate) fn cancelled_by(self, cause: Option<CancelCause>) -> Option<CancelCause> {
        cause.filter(|&cause| {
            self.death()
                .is_some_and(|signal| crate::process::signals_of(cause).any(|of| of == signal))
        })
    }

    /// The failure this end is, read as reported: `None` for success.
    pub(crate) fn failure(self) -> Option<Failure> {
        match self {
            Self::Exited(0) => None,
            Self::Exited(code) => Some(Failure::Exited(code)),
            Self::Signaled(signal) => Some(Failure::Signaled(signal.number())),
        }
    }
}

// Negative Windows NTSTATUS values must remain failures.
pub(crate) fn failure_byte(n: i32) -> NonZeroU8 {
    u8::try_from(n.clamp(1, 255))
        .ok()
        .and_then(NonZeroU8::new)
        .unwrap_or(NonZeroU8::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{FailureCode as _, NotRunnable, Reason, Unrunnable};
    use strum::VariantArray as _;

    #[cfg(unix)]
    fn signaled(n: i32) -> WaitOutcome {
        WaitOutcome::Signaled(Signal::new(n))
    }

    /// How the run's ending reads an outcome: a stop by the cause it
    /// answers, else the failure, else (`None`) success.
    #[derive(Debug, PartialEq, Eq)]
    enum Read {
        Stopped(CancelCause),
        Failed(Failure),
    }

    fn read(outcome: WaitOutcome, cause: Option<CancelCause>) -> Option<Read> {
        match outcome.cancelled_by(cause) {
            Some(cause) => Some(Read::Stopped(cause)),
            None => outcome.failure().map(Read::Failed),
        }
    }

    #[cfg(unix)]
    fn died_of(n: i32) -> Read {
        Read::Failed(Failure::Signaled(n))
    }

    #[cfg(unix)]
    #[test]
    fn signal_names_follow_platform_constants() {
        assert_eq!(Signal::new(libc::SIGKILL).to_string(), "9 (SIGKILL)");
        assert_eq!(
            Signal::new(libc::SIGTTOU).to_string(),
            format!("{} (SIGTTOU)", libc::SIGTTOU)
        );
        assert_eq!(Signal::new(0).to_string(), "0");
    }

    /// `128 + n` names a signal only for an `n` the platform has: ssh's
    /// and git's 255 is an exit.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_status_names_only_a_real_signal() {
        let max = libc::SIGRTMAX();
        for (code, expected) in [
            (128 + libc::SIGINT, Some(libc::SIGINT)),
            (129, Some(1)),
            (128 + max, Some(max)),
            (129 + max, None),
            (255, None),
            (128, None),
            (127, None),
            (0, None),
            (i32::MIN, None),
        ] {
            assert_eq!(
                Signal::of_status(code).map(Signal::number),
                expected,
                "status {code}"
            );
        }
    }

    /// An ended status is its signal death, or its exit code.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exit_status_is_its_signal_or_its_code() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        assert_eq!(
            WaitOutcome::from_exit_status(ExitStatus::from_raw(139 << 8)),
            WaitOutcome::Exited(139)
        );
        assert_eq!(
            WaitOutcome::from_exit_status(ExitStatus::from_raw(libc::SIGSEGV)),
            signaled(libc::SIGSEGV)
        );
    }

    fn code(failure: Failure) -> u8 {
        Reason::process("cmd", failure).failure_code().get()
    }

    /// Every failure's number, from the one table on `Reason`.
    #[test]
    fn every_failure_has_its_code() {
        for (failure, expected) in [
            (Failure::Exited(3), 3),
            (Failure::Signaled(11), 139),
            (Failure::NotFound, 127),
            (
                Failure::NotRunnable(NotRunnable {
                    found: "cmd".into(),
                    why: Unrunnable::PermissionDenied,
                }),
                126,
            ),
            (
                Failure::NotRunnable(NotRunnable {
                    found: "cmd".into(),
                    why: Unrunnable::Io("boom".into()),
                }),
                126,
            ),
        ] {
            assert_eq!(code(failure.clone()), expected, "{failure:?}");
        }
    }

    #[test]
    fn an_externals_failure_is_never_a_clean_exit() {
        for (raw, expected) in [
            (300, 255),
            (-1, 1),
            (0xC000_013A_u32.cast_signed(), 1),
            (3, 3),
        ] {
            assert_eq!(code(Failure::Exited(raw)), expected, "raw status {raw}");
        }
        assert_eq!(code(Failure::Signaled(9)), 137);
    }

    #[test]
    fn ordinary_exit_and_signal_death_stay_distinct() {
        assert_eq!(read(WaitOutcome::Exited(0), None), None);
        assert_eq!(
            read(WaitOutcome::Exited(137), None),
            Some(Read::Failed(Failure::Exited(137)))
        );
        assert_eq!(
            read(WaitOutcome::Signaled(Signal::new(9)), None),
            Some(Read::Failed(Failure::Signaled(9)))
        );
    }

    /// A death by any of a cause's signals — its grace signal, its key, the
    /// SIGKILL that ends every teardown — is the cause's.
    #[cfg(unix)]
    #[test]
    fn a_death_by_a_causes_signal_is_the_causes() {
        use libc::{SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGTERM};
        for (cause, expected) in [
            (CancelCause::Interrupted, vec![SIGINT, SIGKILL]),
            (CancelCause::Cancelled, vec![SIGTERM, SIGKILL]),
            (CancelCause::TimedOut, vec![SIGTERM, SIGKILL]),
            (CancelCause::Terminated, vec![SIGTERM, SIGHUP, SIGKILL]),
            (CancelCause::Aborted, vec![SIGKILL, SIGQUIT]),
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
                    read(signaled(n), Some(cause)),
                    Some(Read::Stopped(cause)),
                    "{cause:?}, signal {n}"
                );
            }
        }
    }

    /// A signal none of the cause's is the child's own death, whatever was
    /// in force when it landed: a segfault in the grace window keeps the
    /// segfault's words.  An exit of `128 + n` is an exit, never a signal: a
    /// Linux envelope's supervisor says which its program's end was.
    #[cfg(unix)]
    #[test]
    fn a_death_off_the_causes_teardown_stays_a_signal() {
        for &cause in CancelCause::VARIANTS {
            assert_eq!(
                read(signaled(libc::SIGSEGV), Some(cause)),
                Some(died_of(libc::SIGSEGV)),
                "{cause:?}"
            );
            let code = 128 + libc::SIGKILL;
            assert_eq!(
                read(WaitOutcome::Exited(code), Some(cause)),
                Some(Read::Failed(Failure::Exited(code))),
                "{cause:?}, exit {code}"
            );
        }
    }

    /// No death mints a cause: with none in force every signal death is the
    /// child's own, a key's included, and a cause is kept only for its own
    /// signals.
    #[cfg(unix)]
    #[test]
    fn a_death_is_a_cancellation_only_by_a_cause_in_force() {
        for n in 1..=31 {
            assert_eq!(read(signaled(n), None), Some(died_of(n)), "{n}");
        }
        assert_eq!(
            read(signaled(libc::SIGQUIT), Some(CancelCause::Aborted)),
            Some(Read::Stopped(CancelCause::Aborted))
        );
        assert_eq!(
            read(signaled(libc::SIGINT), Some(CancelCause::TimedOut)),
            Some(died_of(libc::SIGINT))
        );
        assert_eq!(
            read(signaled(libc::SIGTERM), Some(CancelCause::ReaderGone)),
            Some(died_of(libc::SIGTERM))
        );
    }

    /// A signal no cause sent stays a signal, and keeps its words.
    #[cfg(unix)]
    #[test]
    fn a_foreign_signal_is_still_reported_as_a_signal() {
        assert_eq!(
            read(signaled(libc::SIGKILL), None),
            Some(died_of(libc::SIGKILL))
        );
        assert_eq!(
            Reason::process("sh", Failure::Signaled(libc::SIGKILL)).describe(),
            "sh: killed by signal 9 (SIGKILL)"
        );
        assert_eq!(
            Reason::process("sh", Failure::Signaled(libc::SIGSEGV))
                .hint()
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
        for &cause in CancelCause::VARIANTS {
            assert_eq!(
                read(WaitOutcome::Exited(3), Some(cause)),
                Some(Read::Failed(Failure::Exited(3))),
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
                read(signaled(libc::SIGPIPE), sent),
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
        let sent = Some(CancelCause::ReaderGone).max(Some(CancelCause::Aborted));
        assert_eq!(sent, Some(CancelCause::Aborted));
        assert_eq!(
            read(signaled(libc::SIGKILL), sent),
            Some(Read::Stopped(CancelCause::Aborted))
        );
    }

    /// ral's kill exit code ends every Windows teardown, so it is the sent
    /// cause's whichever cause that was.
    #[cfg(windows)]
    #[test]
    fn the_kill_exit_code_is_attributed_to_the_cause_sent() {
        for &cause in CancelCause::VARIANTS {
            assert_eq!(
                read(WaitOutcome::Exited(KILL_EXIT_CODE), Some(cause)),
                Some(Read::Stopped(cause)),
                "{cause:?}"
            );
        }
        assert_eq!(
            read(WaitOutcome::Exited(KILL_EXIT_CODE), None),
            Some(Read::Failed(Failure::Exited(KILL_EXIT_CODE)))
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
            read(ctrl_c, Some(CancelCause::Interrupted)),
            Some(Read::Stopped(CancelCause::Interrupted))
        );
        assert_eq!(
            read(ctrl_c, None),
            Some(Read::Failed(Failure::Exited(STATUS_CONTROL_C_EXIT)))
        );
        assert_eq!(
            Reason::process("ping", Failure::Exited(STATUS_CONTROL_C_EXIT)).describe(),
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
            read(ctrl_break, Some(CancelCause::TimedOut)),
            Some(Read::Stopped(CancelCause::TimedOut))
        );
        for cause in [
            CancelCause::Interrupted,
            CancelCause::Cancelled,
            CancelCause::Terminated,
        ] {
            assert_eq!(
                read(ctrl_break, Some(cause)),
                Some(Read::Stopped(cause)),
                "{cause:?}"
            );
        }
        for cause in [CancelCause::ReaderGone, CancelCause::Aborted] {
            assert_eq!(
                read(ctrl_break, Some(cause)),
                Some(Read::Failed(Failure::Exited(STATUS_CONTROL_C_EXIT))),
                "{cause:?}"
            );
        }
    }
}
