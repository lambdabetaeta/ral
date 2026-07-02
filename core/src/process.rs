//! Process subsystem: outcomes, signals, cancellation, and process-group placement.
//!
//! Three concerns sit under this umbrella:
//!
//!   * **Outcomes** ([`outcome`]) — structured shapes for what the OS
//!     reported when an external command finished ([`Signal`],
//!     [`WaitOutcome`]), plus the user-facing failure types the
//!     evaluator surfaces ([`SpawnFailure`], [`CommandFailure`]).
//!   * **Signals & cancellation** ([`signal`]) — the global termination
//!     flag polled by the evaluator, [`CancelScope`] for cooperative
//!     structured-concurrency cancellation, and the platform-specific
//!     signal-handler / job-control machinery (`signal::unix`,
//!     `signal::windows`).
//!   * **Process-group placement** — [`Pgid`], [`PgidPolicy`],
//!     [`ChildHandle`], and the platform `spawn_with_pgid` family live
//!     in [`signal`] alongside the handlers they cooperate with at
//!     fork / spawn time.

pub mod launch;
pub mod lease;
pub mod outcome;
pub mod reaper;
pub mod signal;

pub(crate) use outcome::not_found_hint;
pub use outcome::{CommandFailure, Signal, SpawnFailure, WaitOutcome};

pub use launch::{Launch, StdioSpec};
pub use lease::TerminalLease;

pub use reaper::{Deadline, arm_callback, arm_lifetime};

pub use signal::{
    CancelCause, CancelScope, ChildHandle, DurableRoot, ForegroundCancelSlot, ForegroundScope,
    Pgid, PgidPolicy, RootCancelSlot, check, clear, interrupt, is_interrupted,
    publish_durable_root, publish_foreground, request_foreground_cancel, request_root_cancel,
};

#[cfg(unix)]
pub use signal::{
    ForegroundGuard, PipelineRelay, install_handlers, interrupt_foreground_child, quit_handler,
    relay_handler, reset_child_signals, spawn_with_pgid, spawn_with_pgid_after, term_handler,
    termios_snapshot, waitpid_eintr,
};

#[cfg(windows)]
pub use signal::{
    ForegroundGuard, PipelineRelay, ReapStatus, apply_group_active_process_limit,
    disown_pipeline_group, install_handlers, is_known_group, kill_pipeline_group,
    release_win_group, reset_child_signals, spawn_with_pgid, try_reap_leader, wait_leader_blocking,
};

#[cfg(not(any(unix, windows)))]
pub use signal::{ForegroundGuard, PipelineRelay, install_handlers, reset_child_signals};
