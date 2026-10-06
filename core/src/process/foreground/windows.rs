//! Windows foreground ownership: there is none to lend.

use crate::process::Signal;
use crate::process::cancel::CancelCause;

/// A console is never lent, and the type is uninhabited to say so.
///
/// Windows shares one console across every attached process: console programs
/// (fzf, less, vim) drive the Console API directly and need no handoff from ral.
pub enum TerminalLoan {}

impl TerminalLoan {
    pub(crate) fn try_acquire(
        _target: i32,
        _lease: &crate::process::TerminalLease,
        _frame: &crate::process::ForegroundScope,
    ) -> Option<Self> {
        None
    }

    #[expect(
        clippy::uninhabited_references,
        reason = "no loan exists to be borrowed"
    )]
    pub(crate) fn pressed(&self) -> Option<CancelCause> {
        match *self {}
    }

    pub(crate) fn reclaim(self, _: Option<Signal>) -> Option<CancelCause> {
        match self {}
    }
}
