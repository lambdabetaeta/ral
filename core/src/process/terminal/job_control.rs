//! The group-owner decision and the terminal input each placement permits.

use crate::process::{Membership, Placement, PreparedTerminalLoan, TerminalPermit};

/// Who owns the group a launch is about to land in.
pub(crate) enum GroupOwner<'a> {
    /// Its owner already chose what the stage's stdin reads.
    Stage(Membership),
    Session {
        permit: Option<TerminalPermit<'a>>,
        interactive: bool,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum TerminalInput {
    Readable,
    Withheld,
}

impl Placement {
    pub(crate) fn command(owner: GroupOwner<'_>) -> Self {
        match owner {
            GroupOwner::Stage(membership) => Self::Join(membership),
            GroupOwner::Session {
                permit: Some(permit),
                ..
            } => Self::Tenant(PreparedTerminalLoan::capture(permit)),
            GroupOwner::Session {
                permit: None,
                interactive: true,
            } => Self::Inherit,
            GroupOwner::Session {
                permit: None,
                interactive: false,
            } => Self::NewSession,
        }
    }

    pub(crate) fn input(&self) -> TerminalInput {
        match self {
            Self::Inherit | Self::Join(_) | Self::Tenant(_) => TerminalInput::Readable,
            Self::NewSession => TerminalInput::Withheld,
        }
    }
}
