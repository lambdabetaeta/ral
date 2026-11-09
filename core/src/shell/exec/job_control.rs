//! Job-control modes and terminal-loan preparation, before any child is spawned.

use crate::io::{Sink, Source};
use crate::process::{Membership, PgidPolicy, PreparedTerminalLoan, TerminalLoan};
use crate::shell::runtime::{Mooring, Shell, TerminalAccess};

/// Only terminal input can be withheld; pipes and explicit EOF belong to the caller.
#[derive(Clone, Copy)]
pub(super) enum TerminalInput {
    Retain,
    Withhold,
}

impl TerminalInput {
    /// A nested pipeline preserves its owner's selection. A new pipeline reads
    /// the terminal only after a successful handoff, not merely preparation.
    pub(super) fn for_pipeline(shell: &Shell, holds_terminal: bool) -> Self {
        if shell.session.stage.is_some() || holds_terminal {
            Self::Retain
        } else {
            Self::Withhold
        }
    }

    pub(super) fn stdin(self, source: &Source) -> &Source {
        match (self, source) {
            (Self::Withhold, Source::Terminal) => &Source::Empty,
            (Self::Retain, Source::Terminal)
            | (Self::Retain | Self::Withhold, Source::Reader(_) | Source::Empty) => source,
        }
    }
}

/// A foreground mode owns the preparation until collection; joined commands
/// instead preserve the input already selected by their group's owner.
pub(super) enum Placement {
    Joined(Membership),
    Foreground(PreparedTerminalLoan),
    Inherit,
    Headless,
    Unloaned { terminal_input: TerminalInput },
}

impl Placement {
    pub(super) fn for_command(shell: &Shell, mooring: &Mooring, enveloped: bool) -> Self {
        if let Some(stage) = &shell.session.stage {
            return if enveloped {
                Self::Unloaned {
                    terminal_input: TerminalInput::Retain,
                }
            } else {
                Self::Joined(stage.clone())
            };
        }
        if enveloped {
            // The payload has no controlling terminal: reads do not trigger SIGTTIN.
            return Self::Unloaned {
                terminal_input: if shell.session.interactive {
                    TerminalInput::Retain
                } else {
                    TerminalInput::Withhold
                },
            };
        }
        Self::from_preparation(
            prepare_terminal_loan(shell, mooring),
            shell.session.interactive,
        )
    }

    fn from_preparation(preparation: LoanPreparation, interactive: bool) -> Self {
        match preparation {
            LoanPreparation::Prepared(loan) => Self::Foreground(loan),
            LoanPreparation::Unavailable => Self::Unloaned {
                terminal_input: TerminalInput::Withhold,
            },
            LoanPreparation::NotEligible if interactive => Self::Inherit,
            LoanPreparation::NotEligible => Self::Headless,
        }
    }

    pub(super) fn policy(&self) -> PgidPolicy {
        match self {
            Self::Joined(membership) => PgidPolicy::Join(membership.clone()),
            Self::Foreground(_) | Self::Unloaned { .. } => PgidPolicy::NewLeader,
            Self::Inherit => PgidPolicy::Inherit,
            Self::Headless => PgidPolicy::NewSession,
        }
    }

    pub(super) fn stdin<'a>(&self, source: &'a Source) -> &'a Source {
        let terminal_input = match self {
            Self::Joined(_) | Self::Foreground(_) | Self::Inherit => TerminalInput::Retain,
            Self::Headless => TerminalInput::Withhold,
            Self::Unloaned { terminal_input } => *terminal_input,
        };
        terminal_input.stdin(source)
    }

    pub(super) fn into_loan(self) -> Option<PreparedTerminalLoan> {
        match self {
            Self::Foreground(loan) => Some(loan),
            Self::Joined(_) | Self::Inherit | Self::Headless | Self::Unloaned { .. } => None,
        }
    }
}

/// Eligibility without a preparation is distinct from no authority: on Windows
/// console-mode capture can fail, but the command must still lead its own group.
pub(super) enum LoanPreparation {
    NotEligible,
    Unavailable,
    Prepared(PreparedTerminalLoan),
}

/// Shared by standalone commands and pipelines, before even the group anchor
/// runs. Enveloped commands bypass this: their payload lives in another session.
/// A terminal-launched script may hold a lease without being interactive.
pub(super) fn prepare_terminal_loan(shell: &Shell, mooring: &Mooring) -> LoanPreparation {
    if shell.session.stage.is_some() {
        return LoanPreparation::NotEligible;
    }
    let foreground = matches!(shell.io.stdout, Sink::Stdout)
        || mooring.terminal_access == TerminalAccess::ExplicitLoan;
    match shell.terminal_lease(mooring).filter(|_| foreground) {
        Some(lease) => match TerminalLoan::prepare(lease, &mooring.home) {
            Some(loan) => LoanPreparation::Prepared(loan),
            None => LoanPreparation::Unavailable,
        },
        None => LoanPreparation::NotEligible,
    }
}

#[cfg(test)]
mod tests;
