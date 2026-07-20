//! Process subsystem: outcomes, signals, cancellation, and process-group placement.
//!
//! The concerns sitting under this umbrella:
//!
//!   * **Outcomes** ([`outcome`]) — structured shapes for what the OS
//!     reported when an external command finished ([`Signal`],
//!     [`WaitOutcome`]), plus the user-facing failure types the
//!     evaluator surfaces ([`SpawnFailure`], [`CommandFailure`]).
//!   * **Signals & placement** ([`signal`]) — the escalation ladder polled
//!     by the evaluator, the platform-specific signal-handler / job-control
//!     machinery (`signal::unix`, `signal::windows`), and [`PgidPolicy`] /
//!     [`ChildHandle`] for process-group placement at spawn time.
//!   * **Cancellation** ([`cancel`]) — [`CancelScope`] and its typed
//!     [`DurableRoot`] / [`ForegroundScope`] relation for cooperative
//!     structured-concurrency cancellation, plus the signal-reachable slots
//!     the handlers deliver onto.
//!   * **Launch** ([`launch`]) — the owned [`Launch`] value the runtime
//!     hands the subsystem to spawn one external command.
//!   * **Lease** ([`lease`]) — [`TerminalLease`], the unforgeable
//!     controlling-terminal-foreground authority.
//!   * **Reaper** ([`reaper`]) — the process-global deadline daemon that
//!     fires an armed lifetime ceiling as a [`CancelCause::Deadline`].

pub mod cancel;
pub mod launch;
pub mod lease;
pub mod outcome;
pub mod reaper;
pub mod signal;
#[cfg(unix)]
pub mod slot;

pub(crate) use outcome::not_found_hint;
pub use outcome::{CommandFailure, Signal, SpawnFailure, WaitOutcome};

pub use launch::{Launch, StdioSpec};
pub use lease::TerminalLease;

pub use reaper::{Deadline, arm_callback, arm_lifetime};

pub use cancel::{
    CancelCause, CancelScope, CancelSlot, DurableRoot, ForegroundScope, foreground_cancel_cause,
    publish_durable_root, publish_foreground, request_foreground_cancel, request_root_cancel,
};

pub use signal::{ChildHandle, Pgid, PgidPolicy, check, clear, escalation_pending};

#[cfg(unix)]
pub use signal::{
    ForegroundGuard, PipelineRelay, install_handlers, interrupt_foreground_child, quit_handler,
    relay_handler, reset_child_signals, spawn_with_pgid, spawn_with_pgid_after, term_handler,
    termios_snapshot, waitpid_eintr,
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

#[cfg(not(any(unix, windows)))]
pub use signal::{ForegroundGuard, PipelineRelay, install_handlers, reset_child_signals};
