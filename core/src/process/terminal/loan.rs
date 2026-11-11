//! Captured terminal state and the opaque responsibility a launch holds.

use std::io;

use super::keys::KeyAttribution;
use super::lease::TerminalPermit;
use crate::process::cancel::{CancelCause, ScopeNode};
use crate::process::{Leader, Signal};

#[cfg(unix)]
#[path = "loan/unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "loan/windows.rs"]
mod sys;

/// State captured before spawn: even a background child can change modes.
#[derive(Debug)]
pub(crate) struct PreparedTerminalLoan {
    saved: sys::Saved,
    home: ScopeNode,
}

impl PreparedTerminalLoan {
    pub(crate) fn capture(permit: TerminalPermit<'_>) -> Self {
        Self {
            saved: sys::Saved::capture(),
            home: permit.into_home(),
        }
    }

    /// Refusal retains state until any mode-changing child is torn down.
    pub(crate) fn lend(self, leader: &Leader) -> Result<TerminalHold, (Self, io::Error)> {
        match sys::hand_over(leader.pgid()) {
            Ok(()) => Ok(self.into_lent()),
            Err(error) => Err((self, error)),
        }
    }

    /// An envelope keeps restoration responsibility without a handoff.
    #[cfg(target_os = "linux")]
    pub(in crate::process) fn guard(self) -> TerminalHold {
        TerminalHold(Hold::Guarded(self.saved))
    }

    /// An isolated collector fixture: no handoff, but drop still restores.
    #[cfg(test)]
    pub(crate) fn assumed(self) -> TerminalHold {
        self.into_lent()
    }

    fn into_lent(self) -> TerminalHold {
        TerminalHold(Hold::Lent(TerminalLoan {
            attribution: KeyAttribution::new(self.home),
            _saved: self.saved,
        }))
    }
}

/// An established loan: restoration on drop and attribution of reported keys.
struct TerminalLoan {
    attribution: KeyAttribution,
    _saved: sys::Saved,
}

impl TerminalLoan {
    fn hear(&mut self, signal: Signal) {
        self.attribution.hear(signal);
    }

    fn pressed(&self) -> Option<CancelCause> {
        self.attribution.pressed()
    }

    fn reclaim(mut self, last: Option<Signal>) {
        if let Some(signal) = last {
            self.hear(signal);
        }
    }
}

/// What a launch holds of the terminal while its child runs.
#[derive(Default)]
pub(crate) struct TerminalHold(Hold);

#[derive(Default)]
enum Hold {
    #[default]
    Unheld,
    /// Modes restored on drop, nothing handed over, no key heard back.
    #[cfg(target_os = "linux")]
    Guarded(sys::Saved),
    Lent(TerminalLoan),
}

impl TerminalHold {
    #[cfg(unix)]
    pub(crate) fn hear(&mut self, signal: Signal) {
        match &mut self.0 {
            Hold::Lent(loan) => loan.hear(signal),
            Hold::Unheld => {}
            #[cfg(target_os = "linux")]
            Hold::Guarded(_) => {}
        }
    }

    pub(crate) fn pressed(&self) -> Option<CancelCause> {
        match &self.0 {
            Hold::Lent(loan) => loan.pressed(),
            Hold::Unheld => None,
            #[cfg(target_os = "linux")]
            Hold::Guarded(_) => None,
        }
    }

    pub(crate) fn reclaim(self, last: Option<Signal>) {
        match self.0 {
            Hold::Lent(loan) => loan.reclaim(last),
            Hold::Unheld => {}
            #[cfg(target_os = "linux")]
            Hold::Guarded(saved) => drop(saved),
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
#[path = "loan/tests.rs"]
mod tests;
