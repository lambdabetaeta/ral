//! What the OS reported when a child ended, and the user-facing failure it
//! becomes: a `CommandFailure` travels on as `Status::Process` in `types/error.rs`.

use super::cancel::CancelCause;

/// The exit code the collector's `TerminateProcess` uses — ASCII "RALK" —
/// the only signature a terminated Windows process carries.  A stage exiting
/// with this exact code inside the kill window would be forgiven too; Unix
/// has no such residue, a SIGKILL death being uncounterfeitable by an exit.
#[cfg(windows)]
pub(crate) const STAGE_KILL_EXIT_CODE: i32 = 0x5241_4c4b;

/// The escalation ladder ral's own teardown sends, and the whole of it:
/// whatever `grace_signal` opens with, then SIGKILL to finish.  Every teardown
/// sends exactly these, so a death by any other signal is the child's own
/// however the wait left.
#[cfg(unix)]
const TEARDOWN_LADDER: [i32; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGKILL];

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

    #[cfg_attr(not(unix), allow(dead_code))]
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

    /// A signal on ral's own [`TEARDOWN_LADDER`] — the only ones a cancelled
    /// wait may claim as its doing.
    pub(crate) fn is_teardown(self) -> bool {
        #[cfg(unix)]
        {
            TEARDOWN_LADDER.contains(&self.number)
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

    /// The shell convention for a signal death, 128 + N.
    pub(crate) fn user_exit_code(self) -> i32 {
        128 + self.number
    }
}

/// What the OS reported when a process ended.  Never a stop: the reaper
/// answers every stop itself, with `SIGCONT`, and posts nothing for it, so
/// no terminal reader is ever handed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    Exited(i32),
    /// A signal death nobody in ral asked for.
    #[cfg_attr(not(unix), allow(dead_code))]
    Signaled(Signal),
    /// A signal death ral itself caused: a scope carried `cause`, and the
    /// teardown in `RunningChild::wait` sent `signal`.  Its own variant so that
    /// no reader can mistake our doing for a signal from outside, and so the
    /// message can name the cause instead of the number.
    Cancelled {
        cause: CancelCause,
        signal: Signal,
    },
    NativeCode(i32),
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

    /// Read a signal death as ral's own teardown for `cause` — but only a death
    /// by a signal ral sends, and only a signal death at all.  A child that
    /// chose its own status inside the grace window keeps it; so does one the
    /// kernel or a third party felled with something off the ladder, a segfault
    /// or a broken pipe inside that same window.  A stop is job control's
    /// business either way.
    ///
    /// An `enveloped` child's `Exited(128 + n)` reads the same way: bwrap
    /// reports its payload's signal death as its own exit, whereas an
    /// unenveloped `Exited(143)` is the child's own choice.
    fn attribute_to(self, cause: CancelCause, enveloped: bool) -> Self {
        let signal = match self {
            Self::Signaled(signal) => signal,
            Self::Exited(code) if enveloped => Signal::new(code - 128),
            _ => return self,
        };
        if signal.is_teardown() {
            Self::Cancelled { cause, signal }
        } else {
            self
        }
    }

    #[cfg_attr(not(all(unix, test)), allow(dead_code))]
    pub(crate) fn to_user_exit_code(self) -> i32 {
        match self {
            Self::Exited(code) | Self::NativeCode(code) => code,
            Self::Signaled(sig) | Self::Cancelled { signal: sig, .. } => sig.user_exit_code(),
        }
    }

    pub(crate) fn is_success(self) -> bool {
        matches!(self, Self::Exited(0) | Self::NativeCode(0))
    }

    /// Whether this death reads as ral's own kill of a stage.  The attributed
    /// form counts: which *reason* the kill had is `sent`'s to say, not this
    /// predicate's — a cancellation in force outranks forgiveness, since
    /// `Option<CancelCause>` orders a stronger cause above `ReaderGone`.
    pub(crate) fn is_stage_kill(self) -> bool {
        #[cfg(unix)]
        {
            matches!(
                self,
                Self::Signaled(sig) | Self::Cancelled { signal: sig, .. } if sig.is_sigkill()
            )
        }
        #[cfg(windows)]
        {
            matches!(self, Self::Exited(code) if code == STAGE_KILL_EXIT_CODE)
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
    /// ral stopped the command itself, because a scope was cancelled for
    /// `cause`; `signal` is what we sent.  Carries the same status as
    /// [`Self::Signal`] would — only the words the user reads differ.
    Cancelled {
        cause: CancelCause,
        signal: Signal,
    },
    Spawn(SpawnFailure),
}

impl CommandFailure {
    /// The failure an outcome amounts to, or `None` for success.  `sent` is
    /// the strongest cause anything sent this child, joined by `max` where
    /// two parties each ended it — a cancellation in force outranks
    /// forgiveness by that order rather than by a special case. Forgiveness
    /// reaches only the collector's own kill of a producer whose reader was
    /// gone, and only a death that kill actually caused — never an exit
    /// status, which killing a zombie cannot rewrite.
    ///
    /// Attribution is the same fact read the other way and so belongs here
    /// too: a death by a signal on ral's own ladder, with a cause in `sent`,
    /// is ral's doing whichever teardown sent it — and, for an `enveloped`
    /// child, so is bwrap's `128 + n` exit for the payload's death by `n`.
    pub(crate) fn from_outcome(
        outcome: WaitOutcome,
        sent: Option<CancelCause>,
        enveloped: bool,
    ) -> Option<Self> {
        let outcome = sent.map_or(outcome, |cause| outcome.attribute_to(cause, enveloped));
        if sent == Some(CancelCause::ReaderGone) && outcome.is_stage_kill() {
            return None;
        }
        match outcome {
            WaitOutcome::Exited(0) | WaitOutcome::NativeCode(0) => None,
            WaitOutcome::Exited(code) | WaitOutcome::NativeCode(code) => Some(Self::ExitCode(code)),
            WaitOutcome::Signaled(sig) => Some(Self::Signal(sig)),
            WaitOutcome::Cancelled { cause, signal } => Some(Self::Cancelled { cause, signal }),
        }
    }

    pub fn message(&self, cmd: &str) -> String {
        match self {
            Self::ExitCode(code) => format!("{cmd}: exited with status {code}"),
            Self::Signal(sig) => format!("{cmd}: killed by signal {}", sig.display()),
            Self::Cancelled { cause, .. } => {
                format!("{cmd}: stopped because {}", cause.event())
            }
            Self::Spawn(SpawnFailure::NotFound) => not_found_hint(cmd),
            Self::Spawn(SpawnFailure::PermissionDenied) => format!("{cmd}: permission denied"),
            Self::Spawn(SpawnFailure::Io(msg)) => format!("{cmd}: {msg}"),
        }
    }

    /// The follow-up line under the message.  `None` for a plain exit code, where
    /// `Error::from_command_failure` falls back to the user's exit-hints table.
    pub(crate) fn default_hint(&self, cmd: &str) -> Option<String> {
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
            // The message already says why; the hint adds only how, and where
            // the status the user reads back comes from.
            Self::Cancelled { signal, .. } => Some(format!(
                "ral stopped it with signal {}, so the status is that signal's and not an exit code {cmd} chose",
                signal.display()
            )),
        }
    }

    /// The conventional numeric code — POSIX's 127 for not found, 126 for cannot-run.
    pub(crate) fn to_user_exit_code(&self) -> i32 {
        match self {
            Self::ExitCode(code) => *code,
            Self::Signal(sig) | Self::Cancelled { signal: sig, .. } => sig.user_exit_code(),
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
    }

    #[test]
    fn ordinary_exit_and_signal_death_stay_distinct() {
        assert_eq!(
            CommandFailure::from_outcome(WaitOutcome::Exited(137), None, false),
            Some(CommandFailure::ExitCode(137))
        );
        assert_eq!(
            CommandFailure::from_outcome(WaitOutcome::Signaled(Signal::new(9)), None, false),
            Some(CommandFailure::Signal(Signal::new(9)))
        );
    }

    /// The words change with the cause; the status does not.  A cause-killed
    /// child reports exactly what the same signal death reports today, so a
    /// script reading `$status` cannot tell the two apart.
    #[cfg(unix)]
    #[test]
    fn a_cancelled_child_names_its_cause_and_keeps_its_status() {
        let term = Signal::new(libc::SIGTERM);
        for (cause, event) in [
            (CancelCause::Deadline, "the call's time limit expired"),
            (CancelCause::Explicit, "the call was cancelled"),
            (CancelCause::Interrupt, "the call was interrupted"),
            (CancelCause::Terminate, "ral was asked to shut down"),
            (CancelCause::RootAbort, "ral was aborted"),
        ] {
            let outcome = WaitOutcome::Signaled(term).attribute_to(cause, false);
            assert_eq!(
                outcome.to_user_exit_code(),
                WaitOutcome::Signaled(term).to_user_exit_code()
            );
            let failure = CommandFailure::from_outcome(outcome, None, false).unwrap();
            assert_eq!(failure.to_user_exit_code(), 128 + libc::SIGTERM);
            assert_eq!(
                failure.message("sleep"),
                format!("sleep: stopped because {event}")
            );
            let hint = failure.default_hint("sleep").unwrap();
            assert_eq!(
                hint,
                "ral stopped it with signal 15 (SIGTERM), \
                 so the status is that signal's and not an exit code sleep chose"
            );
            assert!(
                !hint.contains(event),
                "the hint must add to the message, not repeat it: {hint}"
            );
        }
    }

    /// A signal off ral's teardown ladder is the child's own death, whatever was
    /// in force when it landed: a segfault inside the grace window must keep the
    /// segfault's words and the segfault's hint, not confess to our SIGTERM.
    #[cfg(unix)]
    #[test]
    fn a_signal_off_the_ladder_survives_the_cancel_branch() {
        let segv = Signal::new(libc::SIGSEGV);
        let outcome = WaitOutcome::Signaled(segv).attribute_to(CancelCause::Deadline, false);
        assert_eq!(outcome, WaitOutcome::Signaled(segv));
        let failure = CommandFailure::from_outcome(outcome, None, false).unwrap();
        assert_eq!(failure.message("sh"), "sh: killed by signal 11 (SIGSEGV)");
        assert_eq!(
            failure.default_hint("sh").as_deref(),
            Some("the process crashed with a segmentation fault")
        );
    }

    /// A signal nobody in ral sent stays a signal: only a teardown we performed
    /// earns the cause wording, and only a signal death earns it at all.
    #[cfg(unix)]
    #[test]
    fn a_foreign_signal_is_still_reported_as_a_signal() {
        let failure = CommandFailure::from_outcome(
            WaitOutcome::Signaled(Signal::new(libc::SIGKILL)),
            None,
            false,
        )
        .unwrap();
        assert_eq!(failure.message("sh"), "sh: killed by signal 9 (SIGKILL)");
        assert_eq!(
            WaitOutcome::Exited(3).attribute_to(CancelCause::Deadline, false),
            WaitOutcome::Exited(3)
        );
    }

    /// The kill the collector itself sent is the one death forgiven: the
    /// stage's reader was already reaped, so a SIGKILL is not a failure but
    /// the collector reclaiming a producer nobody was reading from anymore.
    #[cfg(unix)]
    #[test]
    fn a_stage_kill_is_forgiven() {
        let outcome = WaitOutcome::Signaled(Signal::new(libc::SIGKILL));
        let sent = Some(CancelCause::ReaderGone);
        assert_eq!(CommandFailure::from_outcome(outcome, sent, false), None);
    }

    /// The very same death, which nothing in ral caused, is an ordinary
    /// SIGKILL failure: nothing about the signal itself carries forgiveness,
    /// only the ending recording who ended the stage and why.
    #[cfg(unix)]
    #[test]
    fn the_same_death_unsent_is_kept() {
        let outcome = WaitOutcome::Signaled(Signal::new(libc::SIGKILL));
        assert_eq!(
            CommandFailure::from_outcome(outcome, None, false),
            Some(CommandFailure::Signal(Signal::new(libc::SIGKILL)))
        );
    }

    /// A zombie's exit status cannot be overwritten by a kill that arrives
    /// too late: an exit is always kept, whoever ended the stage, which is
    /// exactly what makes a real failure impossible to launder through
    /// forgiveness.
    #[test]
    fn an_exit_status_is_kept_even_when_ral_ended_the_stage() {
        assert_eq!(
            CommandFailure::from_outcome(
                WaitOutcome::Exited(3),
                Some(CancelCause::ReaderGone),
                false
            ),
            Some(CommandFailure::ExitCode(3))
        );
    }

    /// SIGPIPE carries no special case: with no interior edge left to deliver
    /// it, a pipe of the stage's own making that breaks is its own failure,
    /// whoever ended the stage.
    #[cfg(unix)]
    #[test]
    fn a_sigpipe_death_is_kept_under_every_ending() {
        let outcome = WaitOutcome::Signaled(Signal::new(libc::SIGPIPE));
        for sent in [Some(CancelCause::ReaderGone), None] {
            assert_eq!(
                CommandFailure::from_outcome(outcome, sent, false),
                Some(CommandFailure::Signal(Signal::new(libc::SIGPIPE)))
            );
        }
    }

    /// A death on ral's ladder is attributed to the cause that was sent,
    /// wherever the teardown ran — a pipeline stage's SIGTERM names the
    /// cancellation, not the number.  Only SIGKILL is the reader-gone kill, so
    /// a SIGTERM under `ReaderGone` is a cancellation and not forgiveness.
    #[cfg(unix)]
    #[test]
    fn from_outcome_attributes_a_ladder_death_to_the_cause_sent() {
        let term = Signal::new(libc::SIGTERM);
        assert_eq!(
            CommandFailure::from_outcome(
                WaitOutcome::Signaled(term),
                Some(CancelCause::Deadline),
                false
            ),
            Some(CommandFailure::Cancelled {
                cause: CancelCause::Deadline,
                signal: term
            })
        );
        assert_eq!(
            CommandFailure::from_outcome(
                WaitOutcome::Signaled(term),
                Some(CancelCause::ReaderGone),
                false
            ),
            Some(CommandFailure::Cancelled {
                cause: CancelCause::ReaderGone,
                signal: term
            })
        );
    }

    /// A cancellation in force outranks forgiveness: `Option<CancelCause>`
    /// orders the stronger cause above `ReaderGone`, so the SIGKILL death it
    /// attributes is kept rather than forgiven.
    #[cfg(unix)]
    #[test]
    fn a_stronger_ending_outranks_forgiveness() {
        let sent = Some(CancelCause::ReaderGone).max(Some(CancelCause::RootAbort));
        assert_eq!(sent, Some(CancelCause::RootAbort));
        let outcome = WaitOutcome::Cancelled {
            cause: CancelCause::RootAbort,
            signal: Signal::new(libc::SIGKILL),
        };
        assert!(CommandFailure::from_outcome(outcome, sent, false).is_some());
    }

    /// An enveloped `Exited(143)` with a teardown cause in `sent` is bwrap
    /// reporting its SIGTERM'd payload and reads as `Cancelled`; unenveloped,
    /// or with no cause sent, it is the child's own exit.
    #[cfg(unix)]
    #[test]
    fn a_propagated_exit_is_attributed_only_enveloped_and_with_a_cause() {
        let code = 128 + libc::SIGTERM;
        assert_eq!(
            CommandFailure::from_outcome(
                WaitOutcome::Exited(code),
                Some(CancelCause::Explicit),
                true
            ),
            Some(CommandFailure::Cancelled {
                cause: CancelCause::Explicit,
                signal: Signal::new(libc::SIGTERM)
            })
        );
        assert_eq!(
            CommandFailure::from_outcome(WaitOutcome::Exited(code), None, true),
            Some(CommandFailure::ExitCode(code))
        );
        assert_eq!(
            CommandFailure::from_outcome(
                WaitOutcome::Exited(code),
                Some(CancelCause::Explicit),
                false
            ),
            Some(CommandFailure::ExitCode(code))
        );
    }
}
