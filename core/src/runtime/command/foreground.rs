//! Foreground-job decision for a standalone external command.
//!
//! Two correlated outputs depend on the same predicate: the [`PgidPolicy`]
//! to spawn under (`NewLeader` for a foreground leader, `NewSession` for a
//! detached background worker, else `Inherit`) and the post-spawn handoff
//! via [`ForegroundGuard::try_acquire`].  Bundling them
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
    /// Foreground requires *all three*: this caller is the top-level
    /// orchestrator ([`LaunchRole::is_top_level`](crate::io::LaunchRole::is_top_level),
    /// not a pipeline stage), the installed turn holds the terminal lease
    /// ([`Shell::terminal_lease`] is `Some` — the authority the old
    /// `startup_foreground` predicate stood in for), and stdout is terminal-
    /// bound with no shell-side pump.  The lease — not `interactive` — is the
    /// terminal-ownership oracle: a non-interactive script launched at a
    /// terminal holds a `Leased` turn exactly like the REPL and must foreground
    /// its interactive children, or they raise SIGTTOU on their first
    /// `tcsetattr` from a background pgroup.  An internal pipeline stage runs
    /// with [`LaunchRole::PipelineStage`](crate::io::LaunchRole), so even when
    /// its `shell.turn.io` appears to satisfy the conditions it cannot take
    /// foreground; an exarch tool turn installs `Denied`, so its lease borrow
    /// is unavailable and the handoff cannot be constructed at all.
    pub(super) fn for_standalone(shell: &Shell, needs_pump: bool) -> Self {
        let want_fg = shell.turn.io.launch_role.is_top_level()
            && shell.terminal_lease().is_some()
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
            own_group_when_background: shell.turn.io.launch_role.is_top_level()
                && !shell.turn.io.interactive,
            // Park only a foreground child of an interactive REPL — see
            // the field doc.
            park_on_stop: want_fg && shell.turn.io.interactive,
        }
    }

    /// `PgidPolicy` for the spawn: `NewLeader` when the child takes the
    /// foreground (a foreground job leader that needs the controlling
    /// terminal to take it), `NewSession` for a detached non-interactive
    /// top-level background child, `Inherit` otherwise.
    ///
    /// A non-interactive top-level standalone external (exarch's tool
    /// eval, `ral -c`, a script) leads its own group so a watchdog cancel
    /// (exarch's tool timeout) can `kill(-pgid, …)` the *whole subtree* —
    /// not just the direct child.  Without its own group an `Inherit`
    /// child's only killable handle is its pid, so a launcher that forks
    /// a grandchild (`/bin/sh -c '… &'`, `python runtests.py` spawning
    /// workers) leaves the grandchild alive holding the stdout pipe open,
    /// and the pump drain blocks until it exits on its own — defeating
    /// the timeout.  Such a child never takes the foreground, so it has no
    /// use for the controlling terminal: `NewSession` (`setsid`) gives it
    /// its own session, severing the terminal so the job — or anything it
    /// spawns, e.g. a `cargo test` exercising signal code — cannot signal
    /// the process that owns the tty.  The session's pgid still equals the
    /// child's pid, so the subtree `kill(-pgid, …)` is unchanged.
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
        if self.want_fg {
            PgidPolicy::NewLeader
        } else if self.own_group_when_background {
            PgidPolicy::NewSession
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
        // `want_fg` already required `terminal_lease().is_some()`, so the
        // borrow is present; it is the unforgeable proof `try_acquire` demands.
        let lease = shell.terminal_lease()?;
        #[cfg(unix)]
        {
            ForegroundGuard::try_acquire(child_id as libc::pid_t, lease)
        }
        #[cfg(windows)]
        {
            ForegroundGuard::try_acquire(child_id as i32, lease)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (child_id, lease);
            None
        }
    }
}
