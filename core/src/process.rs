//! Running an external command: launch, process-group placement, cancellation,
//! and the outcome the OS reports.
//!
//! Cancellation is the subsystem's common currency — a signal its host
//! forwards, a terminal interrupt, and an elapsed deadline all arrive as a
//! [`CancelCause`] on a [`Scope`], observed at the evaluator's poll points via
//! [`Shell::poll`](crate::shell::runtime::Shell::poll).

pub mod cancel;
pub(crate) mod child;
pub mod deadline;
mod fatal;
#[cfg(unix)]
mod fork;
pub(crate) mod group;
pub(crate) mod jail;
pub(crate) mod launch;
pub(crate) mod limits;
pub(crate) mod outcome;
pub(crate) mod reaper;
#[cfg(unix)]
pub(crate) mod report;
pub(crate) mod signal;
mod site;
#[cfg(unix)]
pub(crate) mod slot;
pub(crate) mod spawn_lock;
mod terminal;

pub(crate) use outcome::{Signal, WaitOutcome};

pub(crate) use launch::{Launch, Spawned, StdioSpec, Unstarted};
pub use terminal::RequestedTerminalAccess;
pub(crate) use terminal::{
    GroupOwner, PreparedTerminalLoan, TerminalAccess, TerminalHold, TerminalInput, TerminalLease,
    TerminalPermit,
};

pub use deadline::{Deadline, arm_callback, arm_lifetime};
pub use fatal::Fatal;
pub(crate) use reaper::Watch;

pub use cancel::{
    Ambient, AmbientForward, CancelCause, Cancellation, DurableRoot, RootCause, Scope, ScopeId,
    ScopeNode, forward_ambient, interrupt_epoch, request_interrupt, request_root_cancel,
};
pub(crate) use cancel::{DEFAULT_CLEANUP_GRACE, POLL_SLICE, TEARDOWN_GRACE, wait_ended};

pub use child::ChildHandle;
pub use group::Pgid;
pub(crate) use group::{Group, Membership, Placement};
#[cfg(unix)]
pub use limits::ChildLimit;
pub use limits::ChildLimits;
pub use signal::{clear, escalation_pending};
pub(crate) use signal::{grace_signal, signals_of};

#[cfg(unix)]
pub use spawn_lock::cloexec_socketpair;
pub use spawn_lock::{cloexec_pipe, output, spawn, status};

#[cfg(unix)]
pub(crate) use fork::{Forked, fork, reap};
#[cfg(unix)]
pub(crate) use group::{SpawnError, spawn_with_pgid};
#[cfg(unix)]
pub(crate) use signal::reset_child_signals;
#[cfg(unix)]
pub use signal::{Disposition, install, install_handlers};

#[cfg(unix)]
pub use slot::clobber_slot;

#[cfg(windows)]
pub(crate) use group::release_win_group;
#[cfg(windows)]
pub use group::{ReapStatus, break_pipeline_group, try_reap_leader};
#[cfg(windows)]
pub use signal::{install_handlers, install_interrupt_handler};
