//! Foreground-job decision for a standalone external command.
//!
//! One predicate drives two correlated outputs — the [`PgidPolicy`] to spawn
//! under and the post-spawn terminal handoff — so they are bundled in one
//! witness and cannot drift apart.  Pipeline stages never come through here:
//! they own their pgid via `PipelineGroup` in `core/src/runtime/pipeline/group.rs`
//! and gate foreground on pure-vs-mixed mode instead.

use crate::process::{ForegroundGuard, PgidPolicy};
use crate::types::{Mooring, Shell};

/// Whether a freshly-spawned standalone external takes the controlling
/// terminal, and whether it leads its own process group.
pub(super) struct ForegroundDecision {
    want_fg: bool,
    /// Top-level and non-interactive: no session whose terminal-foreground
    /// group the child must stay consistent with, so it may lead its own
    /// group and let a cancel tree-kill it.
    own_group_when_background: bool,
    /// Park the child as a resumable job on a stop signal instead of
    /// reaping it.  Only an interactive REPL has a job table to `fg` it
    /// back, hence foreground *and* interactive.
    park_on_stop: bool,
}

impl ForegroundDecision {
    /// Foreground requires all three: a top-level launch role, a run holding
    /// the session's terminal lease ([`Shell::terminal_lease`]), and
    /// terminal-bound stdout with no shell-side pump.
    ///
    /// The lease — not `interactive` — is the terminal-ownership oracle: a
    /// non-interactive script launched at a terminal holds one exactly like
    /// the REPL and must foreground its interactive children, or they raise
    /// SIGTTOU on their first `tcsetattr` from a background pgroup.  An
    /// exarch tool run installs `TerminalAccess::Denied`, so no lease borrow
    /// exists and the handoff cannot be constructed at all.
    pub(super) fn for_standalone(shell: &Shell, needs_pump: bool, mooring: &Mooring) -> Self {
        let want_fg = shell.io.launch_role.is_top_level()
            && shell.terminal_lease(mooring).is_some()
            && !needs_pump
            && matches!(
                shell.io.stdout,
                crate::io::Sink::Terminal | crate::io::Sink::External(_)
            );
        Self {
            want_fg,
            own_group_when_background: shell.io.launch_role.is_top_level() && !shell.io.interactive,
            park_on_stop: want_fg && shell.io.interactive,
        }
    }

    /// `NewLeader` when the child takes the foreground, `NewSession` for a
    /// detached non-interactive top-level child, `Inherit` otherwise.
    ///
    /// The detached child needs a group of its own so a watchdog cancel can
    /// `kill(-pgid, …)` the whole subtree: an `Inherit` child's only killable
    /// handle is its pid, so a grandchild it forks (`/bin/sh -c '… &'`)
    /// survives holding the stdout pipe open and the pump drain outlasts the
    /// timeout.  `NewSession` rather than `NewLeader` because such a child
    /// never takes the terminal, and severing the session stops it — or
    /// anything it spawns — from signalling whatever owns the tty; the new
    /// session's pgid still equals its pid, so the tree-kill is unchanged.
    ///
    /// `Inherit` covers the two cases that depend on sharing ral's pgid: a
    /// pipeline stage, which must join the pipeline group to keep the
    /// foreground handoff consistent, and an interactive background child,
    /// which relies on the kernel's terminal-driven SIGINT reaching it.
    pub(super) fn pgid_policy(&self) -> PgidPolicy {
        if self.want_fg {
            PgidPolicy::NewLeader
        } else if self.own_group_when_background {
            PgidPolicy::NewSession
        } else {
            PgidPolicy::Inherit
        }
    }

    /// True when this decision elected to take foreground.  Gated to match
    /// its sole caller, the debug-only `trace_io_wiring`; the spawn path
    /// reads the field directly.
    #[cfg(debug_assertions)]
    pub(super) fn want_fg(&self) -> bool {
        self.want_fg
    }

    pub(super) fn park_on_stop(&self) -> bool {
        self.park_on_stop
    }

    /// Hand the controlling terminal to the freshly-spawned child.
    ///
    /// `None` when this decision declined foreground or the platform handoff
    /// failed.  The [`ForegroundGuard`]'s `Drop` is what restores ral's pgid,
    /// and it must be RAII: any early return between spawn and `child.wait()`
    /// would otherwise strand the shell in a background pgroup.
    pub(super) fn acquire(
        &self,
        child_id: u32,
        shell: &Shell,
        mooring: &Mooring,
    ) -> Option<ForegroundGuard> {
        if !self.want_fg {
            return None;
        }
        // `want_fg` already required the lease, so the `?` never fires; the
        // borrow is the unforgeable proof `try_acquire` demands.
        let lease = shell.terminal_lease(mooring)?;
        #[cfg(unix)]
        {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "child_id is a live OS pid from Child::id(): positive and below i32::MAX, so the u32→pid_t reinterpretation never wraps"
            )]
            let pid = child_id as libc::pid_t;
            ForegroundGuard::try_acquire(pid, lease)
        }
        #[cfg(windows)]
        {
            ForegroundGuard::try_acquire(child_id.cast_signed(), lease)
        }
    }
}
