//! The session's terminal authority and the access that reaches it.

use serde::{Deserialize, Serialize};

use crate::process::ScopeNode;

/// The session's authority to lend the terminal and restore its state.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TerminalLease {
    _seal: (),
}

impl TerminalLease {
    /// Mint iff ral owned the terminal foreground at startup. On Windows,
    /// console attachment grants authority to preserve console modes.
    pub(crate) fn mint_at_startup(startup_foreground: bool) -> Option<Self> {
        startup_foreground.then_some(Self { _seal: () })
    }
}

/// Whether a host requests access to the session's terminal lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestedTerminalAccess {
    Denied,
    Leased,
}

/// Run-local access; only a within-run loan can raise `ExplicitLoan`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum TerminalAccess {
    #[default]
    Denied,
    Leased,
    /// Reach the lease even with captured stdout: the body draws on the tty.
    ExplicitLoan,
}

impl From<RequestedTerminalAccess> for TerminalAccess {
    fn from(requested: RequestedTerminalAccess) -> Self {
        match requested {
            RequestedTerminalAccess::Denied => Self::Denied,
            RequestedTerminalAccess::Leased => Self::Leased,
        }
    }
}

/// An authorised borrow permitting one capture; reported keys strike `home`.
pub(crate) struct TerminalPermit<'a> {
    lease: &'a TerminalLease,
    home: &'a ScopeNode,
}

impl TerminalPermit<'_> {
    pub(super) fn into_home(self) -> ScopeNode {
        let _ = self.lease;
        self.home.clone()
    }
}

impl TerminalAccess {
    /// Permit capture where access reaches the lease, for a group whose output
    /// goes to the terminal iff `to_terminal`.
    pub(crate) fn reach<'a>(
        self,
        lease: Option<&'a TerminalLease>,
        home: &'a ScopeNode,
        to_terminal: bool,
    ) -> Option<TerminalPermit<'a>> {
        match self {
            Self::Denied => None,
            Self::Leased if !to_terminal => None,
            Self::Leased | Self::ExplicitLoan => lease.map(|lease| TerminalPermit { lease, home }),
        }
    }
}
