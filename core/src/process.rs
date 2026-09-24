//! Running an external command: launch, process-group placement, cancellation,
//! and the outcome the OS reports.
//!
//! Cancellation is the subsystem's common currency — a signal handler, a
//! terminal interrupt, and an elapsed deadline all arrive as a [`CancelCause`]
//! on a [`CancelScope`], observed at the evaluator's poll points via [`check`].

pub mod cancel;
pub mod deadline;
pub(crate) mod jail;
pub(crate) mod launch;
pub mod lease;
pub(crate) mod outcome;
pub(crate) mod reaper;
pub(crate) mod signal;
#[cfg(unix)]
pub(crate) mod slot;
pub(crate) mod spawn_lock;
pub(crate) mod wake;

#[cfg(unix)]
pub(crate) use outcome::Signal;
pub(crate) use outcome::{ChildEnd, CommandFailure, SpawnFailure, WaitOutcome};

pub(crate) use launch::{Launch, StdioSpec};
pub use lease::TerminalLease;

pub use deadline::{Deadline, arm_callback, arm_lifetime};
pub(crate) use reaper::Watch;

#[cfg(unix)]
pub(crate) use cancel::TEARDOWN_GRACE;
pub use cancel::{
    Ambient, AmbientForward, CancelCause, CancelScope, CancelWatch, DurableRoot, ForegroundScope,
    forward_ambient, request_interrupt, request_root_cancel, watch_cancel,
};

pub use signal::{ChildHandle, Pgid, PgidPolicy, check, clear, escalation_pending};
#[cfg(unix)]
pub(crate) use signal::{gesture, grace_signal, teardown_signals};

#[cfg(unix)]
pub use spawn_lock::cloexec_socketpair;
pub use spawn_lock::{cloexec_pipe, output, spawn, status};
pub(crate) use wake::Wake;

#[cfg(unix)]
pub use signal::{
    ForegroundGuard, install_handlers, interrupt_foreground_child, interrupt_handler, quit_handler,
    reset_child_signals, spawn_with_pgid, spawn_with_pgid_after, term_handler, termios_snapshot,
};

#[cfg(unix)]
pub use slot::clobber_slot;

#[cfg(windows)]
pub use signal::{
    ForegroundGuard, ReapStatus, break_pipeline_group, disown_pipeline_group, install_handlers,
    relay_interrupt, reset_child_signals, try_reap_leader,
};
#[cfg(windows)]
pub(crate) use signal::{
    apply_group_active_process_limit, is_known_group, release_win_group, set_active_process_limit,
    wait_leader_blocking,
};
