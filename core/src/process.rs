//! Running an external command: launch, process-group placement, cancellation,
//! and the outcome the OS reports.
//!
//! Cancellation is the subsystem's common currency — a signal handler, a
//! terminal interrupt, and an elapsed deadline all arrive as a [`CancelCause`]
//! on a [`CancelScope`], observed at the evaluator's poll points via [`Mooring::check`](crate::types::Mooring::check).

pub mod cancel;
pub(crate) mod child;
pub mod deadline;
pub(crate) mod foreground;
pub(crate) mod group;
pub(crate) mod jail;
pub(crate) mod launch;
pub mod lease;
pub(crate) mod limits;
pub(crate) mod outcome;
pub(crate) mod reaper;
pub(crate) mod signal;
#[cfg(unix)]
pub(crate) mod slot;
pub(crate) mod spawn_lock;
pub(crate) mod tree;

pub(crate) use outcome::{ChildEnd, CommandFailure, Signal, SpawnFailure, WaitOutcome};

pub(crate) use launch::{Launch, StdioSpec};
pub use lease::{RequestedTerminalAccess, TerminalLease};

pub use deadline::{Deadline, arm_callback, arm_lifetime};
pub(crate) use reaper::Watch;
pub(crate) use tree::sample_descendants;

pub(crate) use cancel::TEARDOWN_GRACE;
pub use cancel::{
    Ambient, AmbientForward, CancelCause, CancelScope, CancelWatch, DurableRoot, ForegroundScope,
    forward_ambient, request_interrupt, request_root_cancel, watch_cancel,
};

pub use child::ChildHandle;
pub(crate) use group::{Group, Membership};
pub use group::{Pgid, PgidPolicy};
pub use signal::{clear, escalation_pending};
pub(crate) use signal::{grace_signal, signals_of};

#[cfg(unix)]
pub use spawn_lock::cloexec_socketpair;
pub use spawn_lock::{cloexec_pipe, output, spawn, status};

#[cfg(unix)]
pub use foreground::{TerminalLoan, interrupt_foreground_child, termios_snapshot};
#[cfg(unix)]
pub use group::{spawn_with_pgid, spawn_with_pgid_after};
#[cfg(unix)]
pub use signal::{
    ignore, install, install_handlers, interrupt_handler, quit_handler, reset_child_signals,
    term_handler,
};

#[cfg(unix)]
pub use slot::clobber_slot;

#[cfg(windows)]
pub use foreground::TerminalLoan;
#[cfg(windows)]
pub use group::{ReapStatus, break_pipeline_group, disown_pipeline_group, try_reap_leader};
#[cfg(windows)]
pub(crate) use group::{release_win_group, wait_leader_blocking};
#[cfg(windows)]
pub use signal::{install_handlers, reset_child_signals};
