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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_terminal_owner_can_mint_a_lease() {
        assert!(TerminalLease::mint_at_startup(true).is_some());
        assert!(TerminalLease::mint_at_startup(false).is_none());
    }

    #[test]
    fn reach_checks_access_lease_and_terminal_output() {
        let lease = TerminalLease::mint_at_startup(true).unwrap();
        let home = crate::process::DurableRoot::new().child();
        for (access, to_terminal, eligible) in [
            (TerminalAccess::Denied, false, false),
            (TerminalAccess::Denied, true, false),
            (TerminalAccess::Leased, false, false),
            (TerminalAccess::Leased, true, true),
            (TerminalAccess::ExplicitLoan, false, true),
            (TerminalAccess::ExplicitLoan, true, true),
        ] {
            for held in [false, true] {
                let permit = access.reach(held.then_some(&lease), &home, to_terminal);
                assert_eq!(
                    permit.is_some(),
                    held && eligible,
                    "{access:?}, lease={held}, to_terminal={to_terminal}"
                );
                if let Some(permit) = permit {
                    assert_eq!(permit.into_home().id(), home.id());
                }
            }
        }
    }

    #[test]
    fn requested_access_preserves_the_host_policy() {
        assert_eq!(TerminalAccess::default(), TerminalAccess::Denied);
        assert_eq!(
            TerminalAccess::from(RequestedTerminalAccess::Denied),
            TerminalAccess::Denied
        );
        assert_eq!(
            TerminalAccess::from(RequestedTerminalAccess::Leased),
            TerminalAccess::Leased
        );
    }
}
