//! A builtin application is not a fact.  The pure fragment performs no
//! effect, and a builtin that reaches the world does so through a door (a
//! redirect, `atomic_write`, `spawn_child`) which observes it there, typed as
//! the effect it is; its body runs in a frame that stamps nothing and tees
//! nothing ([`BuiltinEntry::framed`]).  The fan-out is `fact::door`.
//!
//! `within`, `grant`, `guard`, `try`, and `audit` are collection boundaries,
//! not observations.  Only `audit` collects: `evaluator::machine` opens a
//! trail scope (`Audit::open` / `Audit::close`) around its body.

use crate::types::{BuiltinEntry, Mooring, Settled, Shell, Value};
use std::sync::Arc;

/// Run a native body in a fresh call frame.
pub(crate) fn run_native(
    entry: &BuiltinEntry,
    args: &[Value],
    site: Option<&Arc<crate::ty::Site>>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    entry.framed(shell, |shell, frame| {
        entry.call_body(frame, args, site, mooring, shell)
    })
}

impl BuiltinEntry {
    /// Framed public surface for hosts and tests: the body runs under its
    /// own call frame.
    ///
    /// # Errors
    /// Propagates a `Break` raised by the body.
    pub fn run(&self, args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        run_native(self, args, None, mooring, shell)
    }
}
