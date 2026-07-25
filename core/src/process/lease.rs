//! The terminal-foreground capability, reified as an unforgeable value.
//!
//! A [`TerminalLease`] is the authority to hand the controlling terminal to a
//! child via `tcsetpgrp`. It is not a predicate code re-derives from
//! process-global startup state — it is a value the runtime is *given* at
//! session construction and then lends, per run, to the one chokepoint that
//! performs a foreground handoff
//! ([`ForegroundGuard::try_acquire`](super::ForegroundGuard::try_acquire)).
//!
//! The token has no public constructor and is neither `Clone` nor `Copy`, so a
//! host cannot forge one or duplicate it; it can only ask core to perform a run
//! with a stated terminal policy and let core decide whether the borrow is
//! reachable. This is the witness discipline of the reduced-authority-witness
//! decision applied to the terminal: a readable flag becomes a capability value
//! that only the runtime can hold.

/// The session's single witness that ral owns the controlling terminal's
/// foreground and may hand it to a child.
///
/// Minted at most once, while the session is constructed, from the same
/// `tcgetpgrp(stdin) == getpgrp()` predicate that populates
/// [`startup_foreground`](crate::io::TerminalState::startup_foreground). Held
/// on `SessionState`; lent as `&TerminalLease` to the post-startup foreground
/// handoff only when the installed run's access permits it (see
/// [`Shell::terminal_lease`](crate::types::Shell::terminal_lease)).
#[derive(Debug, PartialEq, Eq)]
pub struct TerminalLease {
    _seal: (),
}

impl TerminalLease {
    /// Mint the session's lease iff ral owned the controlling terminal's
    /// foreground at startup. `None` when it did not (a backgrounded launch, a
    /// piped or tty-less eval) and `None` always on platforms with no
    /// `tcsetpgrp` (Windows, other), where no foreground handoff exists to
    /// gate — the helper protocol there never selects a foreground group.
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
