//! Foreground-job decision for a standalone external command.
//!
//! One predicate drives two correlated outputs — the [`PgidPolicy`] to spawn
//! under and the post-spawn terminal handoff — so they are bundled in one
//! witness and cannot drift apart.  A pipeline stage itself is spawned via
//! `PipelineGroup` in `core/src/shell/exec/pipeline/group.rs`, which gates
//! foreground on the pipeline's frozen `TerminalPlan` instead; an external
//! spawned *from inside* a stage still comes through here and joins the
//! stage's group.

use crate::process::{Membership, Pgid, PgidPolicy, PreparedTerminalLoan, TerminalLoan};
use crate::shell::runtime::{Mooring, Shell};

/// Whether a freshly-spawned standalone external takes the controlling
/// terminal, and whether it leads its own process group: one decision, held as
/// the [`PgidPolicy`] it entails, since `NewLeader` is exactly the foreground.
pub(super) struct ForegroundDecision(PgidPolicy);

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
    ///
    /// An `enveloped` child never takes it: bwrap's `--new-session` puts the
    /// payload in a session `tcsetpgrp` cannot name.
    pub(super) fn for_standalone(
        shell: &Shell,
        needs_pump: bool,
        enveloped: bool,
        mooring: &Mooring,
    ) -> Self {
        let want_fg = shell.terminal_lease(mooring).is_some()
            && !needs_pump
            && !enveloped
            && matches!(shell.io.stdout, crate::io::Sink::Terminal);
        Self(policy(
            shell.io.stage.as_ref().map(Membership::group),
            want_fg,
            shell.io.interactive,
        ))
    }

    pub(super) fn pgid_policy(&self) -> PgidPolicy {
        self.0
    }

    /// True when this decision elected to take foreground.  Gated to match
    /// its sole caller, the debug-only `trace_io_wiring`.
    #[cfg(debug_assertions)]
    pub(super) fn want_fg(&self) -> bool {
        matches!(self.0, PgidPolicy::NewLeader)
    }

    /// Capture terminal state before spawning; lend it once the pid is known.
    pub(super) fn prepare(&self, shell: &Shell, mooring: &Mooring) -> Option<PreparedTerminalLoan> {
        if !matches!(self.0, PgidPolicy::NewLeader) {
            return None;
        }
        TerminalLoan::prepare(shell.terminal_lease(mooring)?, &mooring.home)
    }
}

/// `NewLeader` when the child takes the foreground, `NewSession` for a
/// headless (non-interactive top-level) child, `Inherit` otherwise.
///
/// The headless child needs a group of its own so a watchdog cancel can
/// `kill(-pgid, …)` the whole subtree: an `Inherit` child's only killable
/// handle is its pid, so a grandchild it forks (`/bin/sh -c '… &'`)
/// survives holding the stdout pipe open and the pump drain outlasts the
/// timeout.  `NewSession` rather than `NewLeader` because such a child
/// never takes the terminal, and severing the session stops it — or
/// anything it spawns — from signalling whatever owns the tty; the new
/// session's pgid still equals its pid, so the tree-kill is unchanged.
///
/// A run inside a pipeline stage (`stage` is its group) joins that group
/// instead, so its externals stay under the one pgid the pipeline signals as
/// a whole, and never takes the foreground.  `Inherit` is what remains: an
/// interactive background child, which relies on the kernel's terminal-driven
/// SIGINT reaching it.
fn policy(stage: Option<Pgid>, want_fg: bool, interactive: bool) -> PgidPolicy {
    match stage {
        Some(group) => PgidPolicy::Join(group),
        None if want_fg => PgidPolicy::NewLeader,
        None if !interactive => PgidPolicy::NewSession,
        None => PgidPolicy::Inherit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stage_joins_its_group_and_a_top_level_run_leads_by_circumstance() {
        let group = Pgid::from_raw(1).expect("1 is positive");
        assert!(matches!(
            policy(Some(group), true, false),
            PgidPolicy::Join(_)
        ));
        assert!(matches!(policy(None, true, true), PgidPolicy::NewLeader));
        assert!(matches!(policy(None, false, false), PgidPolicy::NewSession));
        assert!(matches!(policy(None, false, true), PgidPolicy::Inherit));
    }
}
