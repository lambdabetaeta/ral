//! Foreground-job decision for a standalone external command.
//!
//! One predicate drives two correlated outputs — the [`PgidPolicy`] to spawn
//! under and the post-spawn terminal handoff — so they are bundled in one
//! witness and cannot drift apart.  A pipeline stage itself is spawned via
//! `PipelineGroup` in `core/src/runtime/pipeline/group.rs`, which gates
//! foreground on the pipeline's frozen `TerminalPlan` instead; an external
//! spawned *from inside* a stage still comes through here and joins the
//! stage's group.

use crate::process::{ForegroundGuard, Pgid, PgidPolicy, StopPolicy};
use crate::types::{Mooring, Shell};

/// Whether a freshly-spawned standalone external takes the controlling
/// terminal, and whether it leads its own process group.
pub(super) struct ForegroundDecision {
    want_fg: bool,
    /// Top-level and non-interactive: no session whose terminal-foreground
    /// group the child must stay consistent with, so it may lead its own
    /// group and let a cancel tree-kill it.
    own_group_when_background: bool,
    /// Whether a stop signal should surface as a resumable job — an
    /// interactive REPL foreground child, which has a job table to `fg` it
    /// back — rather than being killed and reaped.  Fed into
    /// [`StopPolicy::for_external`] via [`Self::stop_policy`], which also
    /// parks on a pipeline's gate when this run is a stage thread.
    escapes: bool,
    /// The pipeline group this run's stage thread belongs to, if any.
    stage_group: Option<Pgid>,
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
            escapes: want_fg && shell.io.interactive,
            stage_group: shell.io.launch_role.stage_group(),
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
    /// A run inside a pipeline stage joins the stage's group instead, so its
    /// externals stay under the one pgid the pipeline signals as a whole.
    /// `Inherit` is what remains: an interactive background child, which
    /// relies on the kernel's terminal-driven SIGINT reaching it.
    pub(super) fn pgid_policy(&self) -> PgidPolicy {
        if self.want_fg {
            PgidPolicy::NewLeader
        } else if self.own_group_when_background {
            PgidPolicy::NewSession
        } else if let Some(g) = self.stage_group {
            PgidPolicy::Join(g)
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

    /// The [`StopPolicy`] a spawn under this decision should wait with.
    pub(super) fn stop_policy(&self, mooring: &Mooring) -> StopPolicy {
        StopPolicy::for_external(mooring, self.escapes)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgid_policy_for_a_stage_group() {
        let decision = ForegroundDecision {
            want_fg: false,
            own_group_when_background: false,
            escapes: false,
            stage_group: Some(Pgid::from_raw(1).expect("1 is positive")),
        };
        assert!(matches!(decision.pgid_policy(), PgidPolicy::Join(_)));
    }
}
