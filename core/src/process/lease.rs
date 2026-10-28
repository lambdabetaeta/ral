//! The authority to lend terminal state to a child, reified as an
//! unforgeable value.
//!
//! No public constructor, neither `Clone` nor `Copy`, so a host can ask
//! core for a run whose terminal policy makes the borrow reachable, but
//! never mint the authority itself.

use serde::{Deserialize, Serialize};

/// The session's single witness that ral owns the controlling terminal's
/// foreground and may hand it to a child.
///
/// Held on `SessionState`; lent as `&TerminalLease` to
/// [`TerminalLoan::prepare`](crate::process::TerminalLoan::prepare)
/// only when the run's access permits
/// ([`Shell::terminal_lease`](crate::shell::runtime::Shell::terminal_lease)).
#[derive(Debug, PartialEq, Eq)]
pub struct TerminalLease {
    _seal: (),
}

impl TerminalLease {
    /// Mint iff ral owned the terminal foreground at startup. On Windows,
    /// console attachment grants the same authority to preserve console modes.
    pub(crate) fn mint_at_startup(startup_foreground: bool) -> Option<Self> {
        startup_foreground.then_some(Self { _seal: () })
    }
}

/// Whether a run may hand the controlling terminal to a child.
///
/// The host states it, and [`Shell::terminal_lease`](crate::shell::runtime::Shell::terminal_lease) decides whether the
/// session's [`TerminalLease`] is reachable
/// from it.  `ExplicitLoan` is absent because no host can seed it — only a
/// within-run loan token raises it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestedTerminalAccess {
    Denied,
    Leased,
}

#[cfg(test)]
mod tests {
    use super::TerminalLease;

    #[test]
    fn only_a_terminal_owner_can_mint_a_lease() {
        assert!(TerminalLease::mint_at_startup(true).is_some());
        assert!(TerminalLease::mint_at_startup(false).is_none());
    }
}
