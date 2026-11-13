//! A builtin application is not a fact.  The pure fragment performs no
//! effect, and a builtin that reaches the world does so through an operation (a
//! redirect, `atomic_write`, `spawn_child`) which observes it there, typed as
//! the effect it is; its body runs in a frame that stamps nothing and tees
//! nothing ([`BuiltinEntry::framed`]).  The fan-out is `shell::runtime::audit::fan_out`.
//!
//! `within`, `grant`, `guard`, `try`, and `audit` are collection boundaries,
//! not observations.  Only `audit` collects: `evaluator::machine` opens a
//! trail scope (`Audit::open` / `Audit::close`) around its body.

use crate::builtin::BuiltinEntry;
use crate::ir::ValClos;
use crate::shell::runtime::{Mooring, Settled, Shell};
use std::sync::Arc;

/// Run a native body in a fresh call frame.
pub(crate) fn run_native(
    entry: &BuiltinEntry,
    args: &[ValClos],
    site: Option<&Arc<crate::ty::Site>>,
    span: Option<crate::source::Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    entry.framed(span, shell, |shell, frame| {
        entry.call_body(frame, args, site, mooring, shell)
    })
}

impl BuiltinEntry {
    /// Framed public surface for hosts and tests: the body runs under its
    /// own call frame.
    ///
    /// # Errors
    /// Propagates a `Break` raised by the body.
    pub fn run(&self, args: &[ValClos], mooring: &Mooring, shell: &mut Shell) -> Settled<ValClos> {
        run_native(self, args, None, None, mooring, shell)
    }
}
