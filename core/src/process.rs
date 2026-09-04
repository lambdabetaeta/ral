//! Running an external command: launch, process-group placement, cancellation,
//! and the outcome the OS reports.
//!
//! Cancellation is the subsystem's common currency — a signal handler, a
//! terminal interrupt, and an elapsed deadline all arrive as a [`CancelCause`]
//! on a [`CancelScope`], observed at the evaluator's poll points via [`check`].

pub mod cancel;
pub mod deadline;
pub mod jail;
pub mod launch;
pub mod lease;
pub mod outcome;
pub mod reaper;
pub mod signal;
#[cfg(unix)]
pub mod slot;
pub mod spawn_lock;
pub mod wake;

pub(crate) use outcome::not_found_hint;
pub use outcome::{CommandFailure, Signal, SpawnFailure, WaitOutcome, WaitPoll};

pub use launch::{Launch, StdioSpec};
pub use lease::TerminalLease;

pub use deadline::{Deadline, arm_callback, arm_lifetime};
pub use reaper::{Reaper, Watch};

pub use cancel::{
    CancelCause, CancelScope, CancelWatch, DurableRoot, ForegroundScope, request_foreground_cancel,
    request_root_cancel, watch_cancel,
};
#[cfg(unix)]
pub(crate) use cancel::TEARDOWN_GRACE;

pub(crate) use signal::KillTarget;
#[cfg(unix)]
pub(crate) use signal::cont_stage_by_pid;
#[cfg(unix)]
pub(crate) use signal::cause_signal;
pub use signal::{ChildHandle, Pgid, PgidPolicy, check, clear, escalation_pending};

pub use spawn_lock::{cloexec_pipe, output, spawn, status};
#[cfg(unix)]
pub use spawn_lock::cloexec_socketpair;
pub use wake::Wake;

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
    ForegroundGuard, ReapStatus, apply_group_active_process_limit,
    break_pipeline_group, disown_pipeline_group, install_handlers, is_known_group,
    kill_pipeline_group, relay_interrupt, release_win_group, reset_child_signals,
    set_active_process_limit, try_reap_leader, wait_leader_blocking,
};
