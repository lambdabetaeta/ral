//! Running an external command: launch, process-group placement, cancellation,
//! and the outcome the OS reports.
//!
//! Cancellation is the subsystem's common currency — a signal handler, a
//! terminal interrupt, and an elapsed deadline all arrive as a [`CancelCause`]
//! on a [`CancelScope`], observed at the evaluator's poll points via [`check`].

pub mod cancel;
pub mod jail;
pub mod launch;
pub mod lease;
pub mod outcome;
pub mod reaper;
pub mod signal;
#[cfg(unix)]
pub mod slot;

pub(crate) use outcome::not_found_hint;
pub use outcome::{CommandFailure, Reader, Signal, SpawnFailure, WaitOutcome};

pub use launch::{Launch, StdioSpec};
pub use lease::TerminalLease;

pub use reaper::{Deadline, arm_callback, arm_lifetime};

pub use cancel::{
    CancelCause, CancelScope, DurableRoot, ForegroundScope, request_foreground_cancel,
    request_root_cancel,
};

pub use signal::{ChildHandle, Pgid, PgidPolicy, check, clear, escalation_pending};

#[cfg(unix)]
pub use signal::{
    ForegroundGuard, PipelineRelay, install_handlers, interrupt_foreground_child, quit_handler,
    relay_handler, reset_child_signals, spawn_with_pgid, spawn_with_pgid_after, term_handler,
    termios_snapshot, try_waitpgid_eintr, waitpgid_eintr,
};

#[cfg(unix)]
pub use slot::clobber_slot;

#[cfg(windows)]
pub use signal::{
    ForegroundGuard, PipelineRelay, ReapStatus, apply_group_active_process_limit,
    break_pipeline_group, disown_pipeline_group, install_handlers, is_known_group,
    kill_pipeline_group, relay_interrupt, release_win_group, reset_child_signals,
    set_active_process_limit, try_reap_leader, wait_leader_blocking,
};
