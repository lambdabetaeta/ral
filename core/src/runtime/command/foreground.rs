//! Foreground-job decision for a standalone external command.
//!
//! Two correlated outputs depend on the same predicate: the [`PgidPolicy`]
//! to spawn under (`NewLeader` if foreground, else `Inherit`) and the
//! post-spawn handoff via [`ForegroundGuard::try_acquire`].  Bundling them
//! in one witness keeps them in lock-step and parallels the
//! [`super::stdio::TtyInputPermit`] discipline next door.
//!
//! Pipeline stages do *not* come through here — they own their pgid via
//! [`super::super::pipeline::group::PipelineGroup`] and gate foreground on
//! pure-vs-mixed mode rather than on a single shell's runtime conditions.

use crate::process::{ForegroundGuard, PgidPolicy};
use crate::types::Shell;

/// Whether a freshly-spawned standalone external should take the
/// controlling terminal, and whether it should lead its own process
/// group.
pub(super) struct ForegroundDecision {
    want_fg: bool,
    /// True when this caller owns the foreground decision (top-level
    /// orchestrator, not a pipeline stage) *and* the shell is
    /// non-interactive — the regime where leading an own group is safe
    /// and useful for cancel tree-kill.
    own_group_when_background: bool,
    /// True when a stop signal should *park* the child as a resumable job
    /// rather than kill-and-reap it.  Only an interactive REPL has a job
    /// table to `fg` a parked child, so this requires taking foreground
    /// *and* being interactive — a non-interactive script that foregrounds
    /// an interactive child (e.g. `ral run-claude.ral`) cannot resume a
    /// stopped job and so must reap it instead.
    park_on_stop: bool,
}

impl ForegroundDecision {
    /// Decide foreground ownership for a standalone external command.
    ///
    /// Foreground requires *both* the runtime conditions (ral owns the
    /// controlling terminal's foreground, terminal-bound stdout, no
    /// shell-side pump) AND an explicit permit from the caller's
    /// [`crate::io::JobControl`].  The terminal-ownership predicate is
    /// `startup_foreground`, not `interactive`: a non-interactive script
    /// launched at a terminal owns the foreground exactly like the REPL
    /// and must foreground its interactive children, or they raise SIGTTOU
    /// on their first `tcsetattr` from a background pgroup.
    /// An internal pipeline stage runs with
    /// `JobControl::pipeline_child`, so even when its `shell.turn.io`
    /// appears to satisfy the runtime conditions it cannot take
    /// foreground — the orchestrator owns that decision.  Without this
    /// positive whitelist, such a child would call `tcsetpgrp` from a
    /// thread, putting ral into a background pgroup whose next read of
    /// the tty raises EIO (SIGTTIN is ignored, see `repl.rs`).
    pub(super) fn for_standalone(shell: &Shell, needs_pump: bool) -> Self {
        let want_fg = shell.turn.io.job_control.may_foreground()
            && shell.turn.io.terminal.startup_foreground
            && !needs_pump
            && matches!(
                shell.turn.io.stdout,
                crate::io::Sink::Terminal | crate::io::Sink::External(_)
            );
        Self {
            want_fg,
            // Lead an own group in the background only when this caller
            // owns the foreground decision (a pipeline stage does not —
            // it must join the pipeline's pgid) and there is no
            // interactive session whose terminal-foreground group the
            // child must stay consistent with.
            own_group_when_background: shell.turn.io.job_control.may_foreground()
                && !shell.turn.io.interactive,
            // Park only a foreground child of an interactive REPL — see
            // the field doc.
            park_on_stop: want_fg && shell.turn.io.interactive,
        }
    }

    /// `PgidPolicy` for the spawn: `NewLeader` when the child takes the
    /// foreground (as a foreground job leader) *or* when it is a
    /// non-interactive top-level background child; `Inherit` otherwise.
    ///
    /// A non-interactive top-level standalone external (exarch's tool
    /// eval, `ral -c`, a script) leads its own group so a watchdog cancel
    /// (exarch's tool timeout) can `kill(-pgid, …)` the *whole subtree* —
    /// not just the direct child.  Without its own group an `Inherit`
    /// child's only killable handle is its pid, so a launcher that forks
    /// a grandchild (`/bin/sh -c '… &'`, `python runtests.py` spawning
    /// workers) leaves the grandchild alive holding the stdout pipe open,
    /// and the pump drain blocks until it exits on its own — defeating
    /// the timeout.
    ///
    /// `Inherit` is kept for the two cases that depend on sharing ral's
    /// pgid: a *pipeline* stage (it must join the pipeline group so the
    /// terminal foreground-handoff stays consistent), and an
    /// *interactive* background child (it shares ral's group so the
    /// kernel's terminal-driven SIGINT still reaches it).  Taking
    /// foreground is a separate decision ([`Self::want_fg`]).
    ///
    /// Every spawned external child also gets default disposition for
    /// SIGINT / SIGQUIT / SIGTSTP / SIGTTIN / SIGTTOU / SIGPIPE — that
    /// is universal and lives in `signal::spawn_with_pgid`'s pre_exec,
    /// not here.
    pub(super) fn pgid_policy(&self) -> PgidPolicy {
        if self.want_fg || self.own_group_when_background {
            PgidPolicy::NewLeader
        } else {
            PgidPolicy::Inherit
        }
    }

    /// True when this decision elected to take foreground.  Read only by
    /// the debug-only `trace_io_wiring`; the spawn path reads the
    /// `want_fg` field directly (see [`Self::acquire`] and
    /// [`Self::pgid_policy`]), so this accessor is gated to match its
    /// sole, debug-only caller.
    #[cfg(debug_assertions)]
    pub(super) fn want_fg(&self) -> bool {
        self.want_fg
    }

    /// True when a stop signal should park the child as a resumable job
    /// rather than kill-and-reap it (see the field doc).
    pub(super) fn park_on_stop(&self) -> bool {
        self.park_on_stop
    }

    /// Hand the controlling terminal to the freshly-spawned child.
    ///
    /// Returns the RAII [`ForegroundGuard`] (or `None` when this decision
    /// declined foreground or the platform handoff failed).  The guard's
    /// `Drop` restores ral's pgid no matter how the caller returns —
    /// making the restore RAII-managed is the only reliable way to plug
    /// every early-return path between spawn and `child.wait()`.
    pub(super) fn acquire(&self, child_id: u32, shell: &Shell) -> Option<ForegroundGuard> {
        if !self.want_fg {
            return None;
        }
        #[cfg(unix)]
        {
            ForegroundGuard::try_acquire(child_id as libc::pid_t, shell)
        }
        #[cfg(windows)]
        {
            ForegroundGuard::try_acquire(child_id as i32, shell)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (child_id, shell);
            None
        }
    }
}
