//! The authority to `tcsetpgrp` a child into the foreground, reified as an
//! unforgeable value: no public constructor, neither `Clone` nor `Copy`, so a
//! host can ask core for a run whose terminal policy makes the borrow
//! reachable, but never mint the authority itself.

/// The session's single witness that ral owns the controlling terminal's
/// foreground and may hand it to a child.
///
/// Held on `SessionState`; lent as `&TerminalLease` to
/// [`ForegroundGuard::try_acquire`](crate::process::ForegroundGuard::try_acquire)
/// only when the run's access permits
/// ([`Shell::terminal_lease`](crate::types::Shell::terminal_lease)).
#[derive(Debug, PartialEq, Eq)]
pub struct TerminalLease {
    _seal: (),
}

impl TerminalLease {
    /// Mint the session's lease iff ral owned the terminal foreground at
    /// startup — the same `tcgetpgrp(stdin) == getpgrp()` predicate behind
    /// `TerminalState::startup_foreground`. `None` off Unix: nothing to gate.
    pub(crate) fn mint_at_startup(startup_foreground: bool) -> Option<Self> {
        #[cfg(unix)]
        {
            startup_foreground.then_some(Self { _seal: () })
        }
        #[cfg(not(unix))]
        {
            let _ = startup_foreground;
            None
        }
    }
}
