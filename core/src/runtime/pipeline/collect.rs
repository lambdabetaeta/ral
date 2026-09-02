//! Collect phase: fold a process-staged pipeline's per-stage observations
//! into one result.
//!
//! Observation order is the order stages actually end, not launch order: a
//! non-final stage may outlive its reader (a child that stops itself, or one
//! that keeps writing after the reader is long gone), so collection probes
//! every still-running stage rather than blocking on one at a time. The kill
//! cascade is tail-driven — a stage is killed once its reader has settled —
//! and that kill is the one death forgiven. Verdict precedence — first
//! failure wins, control outranks failure, a stop parks — folds in launch
//! order, over observations buffered during the walk.

use super::super::command;
#[cfg(unix)]
use super::group::Witnessed;
use super::group::{GroupRole, PipelineGroup};
use super::launch::{Probe, StageHandle};
use crate::evaluator::audit::observe_stamped;
#[cfg(unix)]
use crate::process::Signal;
use crate::process::{CancelCause, Pgid, StageGate};
use crate::types::{
    AuditFragment, AuditIo, Break, CommandOrigin, Error, Mooring, Observation, Observed, Settled,
    Shell, Value, epoch_us,
};

/// Wait on a direct-spawn external stage and reduce it to a [`StageObservation`].
///
/// Such a stage has no audit-emitting evaluator behind it, so its one command
/// node is synthesised here to keep it level with thread-routed stages.
pub(super) fn observe_external_stage(
    running: command::RunningChild,
    shell: &Shell,
    started: std::time::Instant,
) -> StageObservation {
    let name = running.name.clone();
    let failure = match running.observe() {
        Ok(failure) => failure,
        Err(br) => return StageObservation::from_break(br),
    };
    let err = failure.map(|f| Error::from_command_failure(&name, f, shell));
    let audit = synth_external_stage_audit(shell, &name, err.as_ref());
    match err {
        Some(err) => StageObservation::failure(super::augment_stage_failure(err, shell, started))
            .with_audit(audit),
        None => StageObservation::ok().with_audit(audit),
    }
}

/// The fragment is empty when audit is inactive at the parent.
fn synth_external_stage_audit(shell: &Shell, name: &str, err: Option<&Error>) -> AuditFragment {
    if !shell.local.audit.active() {
        return AuditFragment::empty();
    }
    let site = shell.call_site();
    let principal = shell.context.principal();
    let now = epoch_us();
    let obs = Observation::spanning(
        site,
        now,
        now,
        principal,
        Observed::Command {
            argv: vec![name.to_string()],
            status: err.map_or(0, Error::exit_code),
            origin: CommandOrigin::External,
            io: AuditIo::default(),
            error: err.map(|e| e.message.clone()),
            value: Value::Unit,
        },
    );
    AuditFragment::from_observations(vec![obs])
}

/// One stage's observation, normalized across external children and ral
/// stage threads.  `final_value` is set only by the final value-typed ral
/// stage, so a stage that broke carries none.
///
/// The break is a [`Break`], whose own two constructors are already the
/// classification the fold needs: a stage-boundary failure (a panicked
/// thread, `waitpid`) arrives as `Error` like any other, and only an
/// `Escape` is control flow.
pub(super) struct StageObservation {
    pub(super) break_: Option<Break>,
    final_value: Option<Value>,
    audit: AuditFragment,
}

impl StageObservation {
    pub(super) fn ok() -> Self {
        Self {
            break_: None,
            final_value: None,
            audit: AuditFragment::empty(),
        }
    }

    pub(super) fn failure(error: Error) -> Self {
        Self::from_break(Break::Error(error))
    }

    pub(super) fn from_break(br: Break) -> Self {
        Self {
            break_: Some(br),
            final_value: None,
            audit: AuditFragment::empty(),
        }
    }

    pub(super) fn with_audit(mut self, audit: AuditFragment) -> Self {
        self.audit = audit;
        self
    }

    pub(super) fn with_value(mut self, value: Option<Value>) -> Self {
        self.final_value = value;
        self
    }

    /// A killed stage's verdict is the collector's own doing and is dropped;
    /// what the stage observed still happened and is kept.
    pub(super) fn forgiven(self) -> Self {
        Self::ok().with_audit(self.audit)
    }
}

pub(super) struct PipelineCollector {
    break_: Option<Break>,
    final_value: Option<Value>,
}

/// An escape outranks an error outranks success: the one ranking law of the
/// fold.
fn rank(br: &Break) -> u8 {
    match br {
        Break::Error(_) => 1,
        Break::Escape(_) => 2,
    }
}

impl PipelineCollector {
    fn new() -> Self {
        Self {
            break_: None,
            final_value: None,
        }
    }

    /// Ties go to the earlier stage, the fold being launch-ordered, so a
    /// Ctrl-Z on stage 2 displaces stage 1's nonzero exit while a second
    /// failure never displaces the first.
    fn note(&mut self, br: Break) {
        if self.break_.as_ref().map_or(0, rank) < rank(&br) {
            self.break_ = Some(br);
        }
    }

    /// The audit observations broadcast before the break is ranked, so a stage
    /// that fails or escapes still contributes what it observed.  Reporting
    /// each rather than merging them in puts a stage thread's writes and
    /// execs on the rail — though only where the parent already holds a
    /// trail, since that is what makes a stage collect at all.
    fn fold(
        &mut self,
        mooring: &Mooring,
        shell: &mut Shell,
        is_pipeline_final: bool,
        obs: StageObservation,
    ) {
        for observation in obs.audit.into_observations() {
            observe_stamped(shell, mooring, observation);
        }
        if let Some(br) = obs.break_ {
            self.note(br);
        }
        // A stage that broke carries no value, so this needs no guard.
        if is_pipeline_final {
            self.final_value = obs.final_value;
        }
    }

    pub(super) fn finish(self, yields: crate::ir::PipeYield) -> Settled<Value> {
        if let Some(br) = self.break_ {
            return Err(br);
        }
        match yields {
            crate::ir::PipeYield::Unit => Ok(Value::Unit),
            crate::ir::PipeYield::Last => Ok(self.final_value.unwrap_or(Value::Unit)),
        }
    }
}

/// One pass's outcome: whether it made progress, found nothing new, finished
/// every stage, or hit a stop.  Shared by [`CollectState::drive`]'s blocking
/// loop and [`CollectState::pass`]'s single non-blocking use by
/// `ParkedPipeline::poll`.
pub(super) enum Pass {
    Advanced,
    Idle,
    Done,
    #[cfg(unix)]
    Parked(Signal),
}

/// `drive`'s two outcomes: every stage observed, or a stop parked the group.
/// `Parked` is `cfg(unix)`: there is no stop to park from on Windows.
pub(super) enum Drive {
    Done,
    #[cfg(unix)]
    Parked(Signal),
}

/// Live collector state: every stage still running or already observed.
/// Re-entrant — `drive` may be called again after a park to resume the same
/// walk — so both vectors, not just the pending count, survive between calls.
pub(super) struct CollectState {
    stages: Vec<Option<StageHandle>>,
    observed: Vec<Option<StageObservation>>,
    started: std::time::Instant,
    /// The pgid a forced end of this collector kills; `None` for a joining
    /// collector, whose owner's teardown does it.
    owned_group: Option<Pgid>,
}

#[cfg(unix)]
fn stop_the_group(group: &PipelineGroup, gate: &StageGate) {
    gate.pause();
    group.leader_pgid().signal_group(Signal::new(libc::SIGSTOP));
}

/// This collector's own scope, once `check` has also parked a joining
/// collector alongside its owner.
fn scope_cancelled(mooring: &Mooring) -> Option<CancelCause> {
    crate::process::check(mooring).err()?;
    mooring.cancel.cause()
}

impl CollectState {
    pub(super) fn new(group: &PipelineGroup, started: std::time::Instant) -> Self {
        Self {
            stages: Vec::new(),
            observed: Vec::new(),
            started,
            owned_group: group.owned().then(|| group.leader_pgid()),
        }
    }

    pub(super) fn push(&mut self, handle: StageHandle) {
        self.stages.push(Some(handle));
        self.observed.push(None);
    }

    /// One non-blocking pass over every unobserved stage, tail-first: end a
    /// running stage whose reader has already settled, observe whichever
    /// finished, and detect a stop — through the anchor or a stage's own
    /// probe — and answer it by the group's role.  Only a foreground group
    /// has a job table to resume it, so only it parks; any other owned group
    /// is cancelled; a joining collector forwards the stop into its own
    /// stage's slot — the report its owner reads — and forgets it here,
    /// never parking, signalling, or escaping.  A stop with no park above it
    /// — a detached worker, deaf to job control — is instead resumed by this
    /// collector itself.
    #[cfg_attr(
        not(unix),
        allow(unused_variables, reason = "the Ctrl-Z gate has no Windows use")
    )]
    pub(super) fn pass(
        &mut self,
        group: &mut PipelineGroup,
        gate: &StageGate,
        mooring: &Mooring,
        shell: &Shell,
    ) -> Pass {
        #[cfg(unix)]
        let witnessed = match group.witness() {
            Some(Witnessed::Stopped(sig)) => match group.role() {
                GroupRole::Foreground => {
                    stop_the_group(group, gate);
                    return Pass::Parked(sig);
                }
                GroupRole::Background => Some(CancelCause::Terminate),
                GroupRole::Joining => unreachable!("a joining group has no anchor to witness"),
            },
            Some(Witnessed::Cancelled(cause)) => Some(cause),
            None => None,
        };
        #[cfg(not(unix))]
        let witnessed = None;

        if let Some(cause) = witnessed.or_else(|| scope_cancelled(mooring)) {
            return self.cancel_all(group, cause, shell);
        }

        let n = self.stages.len();
        let mut progress = false;
        for ix in (0..n).rev() {
            let Some(handle) = self.stages[ix].as_mut() else {
                continue;
            };
            match handle.probe() {
                // Ended only while still running: a stage that already
                // finished keeps its outcome, so `!{ echo a; exit 3 } | head -1`
                // stays honest.  Observed on the next pass.
                Probe::Running if self.observed.get(ix + 1).is_some_and(Option::is_some) => {
                    handle.reader_gone();
                }
                Probe::Running => {}
                Probe::Ready => {
                    let obs = self.stages[ix]
                        .take()
                        .expect("probed above")
                        .observe(shell, ix + 1 == n, self.started);
                    self.observed[ix] = Some(obs);
                    progress = true;
                }
                Probe::Stopped(sig) => match group.role() {
                    #[cfg(unix)]
                    GroupRole::Foreground => {
                        stop_the_group(group, gate);
                        return Pass::Parked(sig);
                    }
                    #[cfg(not(unix))]
                    GroupRole::Foreground => unreachable!("nothing stops on Windows"),
                    GroupRole::Background => {
                        // Settled before the group teardown: its SIGCONT would
                        // resume the stopped child, and the verdict would then
                        // name whatever it did next instead of the stop.
                        handle.end_stopped(sig);
                        return self.cancel_all(group, CancelCause::Terminate, shell);
                    }
                    GroupRole::Joining => match &mooring.park {
                        Some(park) => {
                            park.stop.set(Some(sig));
                            handle.resume();
                        }
                        None => handle.resume_unowned(),
                    },
                },
            }
        }

        if !self.stages.iter().any(Option::is_some) {
            Pass::Done
        } else if progress {
            Pass::Advanced
        } else {
            Pass::Idle
        }
    }

    /// Tear the whole pipeline down, and only then observe it.
    ///
    /// Signal, grace, kill, join — in that order, because a join can only
    /// terminate once nothing that holds a pipe end is alive, and a stage's
    /// own kill reaches its pid alone.  A pumped descendant of a killed stage
    /// outlives the stage, and no wait on that pump returns while it does.
    /// This is the one path that observes on purpose, so the kill is explicit
    /// and precedes the observation; a collector dropped short of that is
    /// ended by `Drop`, kill first.
    ///
    /// Tail-first once the tree is dead, so each edge's held read end is
    /// released behind its writer.
    pub(super) fn cancel_all(
        &mut self,
        group: &PipelineGroup,
        cause: CancelCause,
        shell: &Shell,
    ) -> Pass {
        self.cancel_stages(cause);
        // A joining group has no signal to grace and no pgid to kill: its
        // stages die of their own cancel, and the owner's teardown does the rest.
        if group.owned() {
            group.signal(cause);
            #[cfg(unix)]
            {
                let deadline = std::time::Instant::now() + crate::process::TEARDOWN_GRACE;
                loop {
                    // Every stage each round, not up to the first unsettled
                    // one: a probe is also what reaps a direct external.
                    let mut pending = 0_usize;
                    for handle in self.stages.iter_mut().flatten() {
                        if !matches!(handle.probe(), Probe::Ready) {
                            pending += 1;
                        }
                    }
                    if pending == 0 || std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            group.kill();
        }
        let n = self.stages.len();
        for ix in (0..n).rev() {
            if let Some(handle) = self.stages[ix].take() {
                self.observed[ix] = Some(handle.observe(shell, ix + 1 == n, self.started));
            }
        }
        Pass::Done
    }

    /// Pass until every stage is observed or a stop parks the group.
    /// Re-entrant: `fg` resumes the same walk.
    pub(super) fn drive(
        &mut self,
        group: &mut PipelineGroup,
        gate: &StageGate,
        mooring: &Mooring,
        shell: &Shell,
    ) -> Drive {
        let mut interval = std::time::Duration::from_millis(5);
        let cap = std::time::Duration::from_millis(100);
        loop {
            match self.pass(group, gate, mooring, shell) {
                Pass::Done => return Drive::Done,
                #[cfg(unix)]
                Pass::Parked(sig) => return Drive::Parked(sig),
                Pass::Advanced => interval = std::time::Duration::from_millis(5),
                Pass::Idle => {
                    std::thread::sleep(interval);
                    interval = (interval * 2).min(cap);
                }
            }
        }
    }

    /// `Stopped` → `Running` on every stage, for `fg`/`bg`.
    #[cfg(unix)]
    pub(super) fn resume_all(&mut self) {
        for handle in self.stages.iter_mut().flatten() {
            handle.resume();
        }
    }

    /// Cancel and wake every unobserved stage.  Explicit per stage rather than
    /// through the mooring: a cancel the anchor witnessed, or a parked
    /// pipeline's, has no cancelled ancestor scope to propagate from.
    pub(super) fn cancel_stages(&mut self, cause: CancelCause) {
        for handle in self.stages.iter_mut().flatten() {
            handle.cancel(cause);
        }
    }

    /// The audit observations and verdict fold, in launch order, over
    /// whatever this walk observed.
    pub(super) fn fold(&mut self, mooring: &Mooring, shell: &mut Shell) -> PipelineCollector {
        let n = self.observed.len();
        let mut collector = PipelineCollector::new();
        for (ix, obs) in std::mem::take(&mut self.observed).into_iter().enumerate() {
            if let Some(obs) = obs {
                collector.fold(mooring, shell, ix + 1 == n, obs);
            }
        }
        collector
    }
}

impl Drop for CollectState {
    /// A collector dropped with a stage unobserved is a pipeline ending by
    /// force — an aborted launch, an unwind, a park nobody resumed — so the
    /// group dies before the handles below join: a stage's own kill reaches
    /// its pid alone, and a pumped descendant that survived it would hold the
    /// pump's pipe open for ever.  Every stage observed is the ordinary end,
    /// which a `spawn` worker that joined the pgid outlives.
    fn drop(&mut self) {
        if self.stages.iter().any(Option::is_some)
            && let Some(pgid) = self.owned_group
        {
            pgid.kill();
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::types::Escape;

    fn make_error(status: i32, msg: &str) -> Error {
        Error::new(msg.to_string(), status)
    }

    /// The four laws of the join, one per pair of ranks.  An escape outranks
    /// an error; within a rank the earlier stage wins, the fold being
    /// launch-ordered.
    #[test]
    fn breaks_join_by_rank_earlier_stage_breaking_ties() {
        let mut errors = PipelineCollector::new();
        errors.note(Break::Error(make_error(7, "first")));
        errors.note(Break::Error(make_error(9, "second")));
        match errors.break_ {
            Some(Break::Error(error)) => assert_eq!(error.message, "first"),
            _ => panic!("an error must not displace an earlier error"),
        }

        let mut escapes = PipelineCollector::new();
        escapes.note(Break::Escape(Escape::Exit(1)));
        escapes.note(Break::Escape(Escape::Exit(2)));
        assert!(matches!(
            escapes.break_,
            Some(Break::Escape(Escape::Exit(1)))
        ));

        let mut displaced = PipelineCollector::new();
        displaced.note(Break::Error(make_error(7, "early failure")));
        displaced.note(Break::Escape(Escape::Exit(3)));
        assert!(matches!(
            displaced.break_,
            Some(Break::Escape(Escape::Exit(3)))
        ));

        let mut held = PipelineCollector::new();
        held.note(Break::Escape(Escape::Exit(3)));
        held.note(Break::Error(make_error(7, "later failure")));
        assert!(matches!(held.break_, Some(Break::Escape(Escape::Exit(3)))));
    }

    #[test]
    fn protocol_error_does_not_supersede_earlier_failure() {
        let mut c = PipelineCollector::new();
        let mut shell = Shell::default();
        c.fold(
            &Mooring::adrift(),
            &mut shell,
            false,
            StageObservation::failure(make_error(7, "stage one boom")),
        );
        c.fold(
            &Mooring::adrift(),
            &mut shell,
            true,
            StageObservation::from_break(Break::Error(make_error(1, "report pipe: broken"))),
        );
        match c.break_ {
            Some(Break::Error(error)) => assert_eq!(error.message, "stage one boom"),
            _ => panic!("expected the first stage failure to win"),
        }
    }

    #[test]
    fn drive_folds_two_ready_stages_in_launch_order() {
        let mut shell = Shell::default();
        let mut group =
            PipelineGroup::prepare(super::super::resolve::TerminalPlan::NoTerminal, &shell)
                .expect("anchor spawns");
        let gate = StageGate::new();
        let mooring = Mooring::adrift();

        let mut collect = CollectState::new(&group, std::time::Instant::now());
        collect.push(StageHandle::for_test(spawn_exiting(1)));
        collect.push(StageHandle::for_test(spawn_exiting(0)));

        match collect.drive(&mut group, &gate, &mooring, &shell) {
            Drive::Done => {}
            #[cfg(unix)]
            Drive::Parked(_) => panic!("two ordinary exits must not park"),
        }
        let folded = collect.fold(&mooring, &mut shell);
        match folded.break_ {
            Some(Break::Error(error)) => assert_ne!(error.exit_code(), 0),
            other => panic!("expected the failing stage's exit to fold in, got {other:?}"),
        }
    }

    /// An external that exits with `code`, wrapped exactly as a direct
    /// pipeline stage: `GroupOwner::None` gives it no group to be a job under,
    /// so its `StopPolicy` never decides anything.  `/bin/sh` because `true`
    /// and `false` are not in `/bin` on macOS.
    fn spawn_exiting(code: u8) -> command::RunningChild {
        let name = format!("exit {code}");
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", &name])
            .spawn()
            .unwrap_or_else(|e| panic!("spawn /bin/sh: {e}"));
        command::RunningChild::assemble_with_owner(
            crate::process::ChildHandle::from_std(child),
            name,
            command::ExternalPlumbing {
                stdout_pump: None,
                stderr_pump: None,
            },
            crate::process::StopPolicy::KillAndReap,
            command::GroupOwner::None,
            crate::process::CancelScope::root(),
            None,
        )
    }

    /// `/bin/sleep secs` under its own pgid, real-`SIGSTOP`'d — the stop a
    /// direct external pipeline stage sees, without a whole pipeline launch to
    /// set one up.  `Drop` (via `RunningChild`'s) kills it regardless of the
    /// stop, so nothing survives the test.
    #[cfg(unix)]
    fn spawn_stopped_sleep(secs: &str) -> command::RunningChild {
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg(secs);
        let (child, pgid) = crate::process::spawn_with_pgid(&mut cmd, crate::process::PgidPolicy::NewLeader)
            .expect("spawn /bin/sleep under a pgid");
        let pgid = pgid.expect("NewLeader yields a tracked pgid");
        rustix::process::kill_process(pgid.as_pid(), rustix::process::Signal::STOP)
            .expect("SIGSTOP the sleep");
        command::RunningChild::assemble_with_owner(
            crate::process::ChildHandle::from_std(child),
            "sleep".to_string(),
            command::ExternalPlumbing {
                stdout_pump: None,
                stderr_pump: None,
            },
            crate::process::StopPolicy::KillAndReap,
            command::GroupOwner::Standalone(pgid),
            crate::process::CancelScope::root(),
            None,
        )
    }

    /// Only a group with a job table behind it parks; the rest is cancelled
    /// or, in a joining collector, forwarded to the owner.
    #[test]
    fn a_stop_parks_only_a_tty_owning_group() {
        use super::super::resolve::TerminalPlan;
        let shell = Shell::default();
        let tty = PipelineGroup::prepare(TerminalPlan::ForegroundExternalGroup, &shell)
            .expect("anchor spawns");
        let batch =
            PipelineGroup::prepare(TerminalPlan::NoTerminal, &shell).expect("anchor spawns");
        let joining = PipelineGroup::joining(tty.leader_pgid());
        assert_eq!(tty.role(), GroupRole::Foreground);
        assert_eq!(batch.role(), GroupRole::Background);
        assert_eq!(joining.role(), GroupRole::Joining);
    }

    #[cfg(unix)]
    #[test]
    fn stopped_probe_on_a_tty_owning_group_pauses_the_gate_and_parks() {
        let shell = Shell::default();
        let mut group = PipelineGroup::prepare(
            super::super::resolve::TerminalPlan::ForegroundExternalGroup,
            &shell,
        )
        .expect("anchor spawns");
        assert_eq!(group.role(), GroupRole::Foreground);
        let gate = StageGate::new();
        let mooring = Mooring::adrift();

        let mut collect = CollectState::new(&group, std::time::Instant::now());
        collect.push(StageHandle::for_test(spawn_stopped_sleep("30")));

        let signal = 'wait: {
            for _ in 0..100 {
                if let Pass::Parked(sig) = collect.pass(&mut group, &gate, &mooring, &shell) {
                    break 'wait sig;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("an owned group's stop must eventually park");
        };
        assert_eq!(signal, crate::process::Signal::new(libc::SIGSTOP));
        assert!(
            gate.is_paused(),
            "pausing the gate is what makes every other stage block"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stopped_probe_on_a_joining_group_forwards_to_its_own_stage_and_never_parks() {
        let mut group = PipelineGroup::joining(
            crate::process::Pgid::from_raw(std::process::id().cast_signed()).expect("our own pid"),
        );
        let gate = StageGate::new();
        let park = crate::process::StagePark {
            gate: StageGate::new(),
            stop: crate::process::StageStop::new(),
        };
        let mooring = Mooring {
            park: Some(park.clone()),
            ..Mooring::adrift()
        };
        let shell = Shell::default();

        let mut collect = CollectState::new(&group, std::time::Instant::now());
        collect.push(StageHandle::for_test(spawn_stopped_sleep("30")));

        for _ in 0..100 {
            match collect.pass(&mut group, &gate, &mooring, &shell) {
                Pass::Parked(_) => panic!("a joining collector must never park"),
                Pass::Done => panic!("a mere stop must not be observed as done"),
                Pass::Advanced | Pass::Idle => {}
            }
            if park.stop.get().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the stop was never forwarded to the joining collector's own stage status");
    }

    /// A joining group whose mooring carries no park — a `spawn` worker's
    /// pipeline — is deaf to job control: nothing above it will ever `SIGCONT`
    /// its stopped stage, so the collector must revive it itself and walk on
    /// to `Done` rather than spin forever on `Idle`.
    #[cfg(unix)]
    #[test]
    fn stopped_probe_on_a_joining_group_with_no_park_resumes_its_own_stage() {
        let mut group = PipelineGroup::joining(
            crate::process::Pgid::from_raw(std::process::id().cast_signed()).expect("our own pid"),
        );
        let gate = StageGate::new();
        let mooring = Mooring::adrift();
        let shell = Shell::default();

        let mut collect = CollectState::new(&group, std::time::Instant::now());
        collect.push(StageHandle::for_test(spawn_stopped_sleep("0.2")));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match collect.pass(&mut group, &gate, &mooring, &shell) {
                Pass::Parked(_) => panic!("a joining collector must never park"),
                Pass::Done => break,
                Pass::Advanced | Pass::Idle => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "an ownerless stop must be resumed rather than livelocked"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A stage spawned into the group's own pgid via `PgidPolicy::Join`, as
    /// `launch.rs` wires a direct external — a background `sleep` forked from
    /// under `/bin/sh`, whose pid comes back over the stage's own stdout
    /// before the stage is wrapped for the collector. `wait_on_child` decides
    /// whether the wrapped `sh` waits on it (so the grandchild is still
    /// running when the stage is observed) or exits at once (so the
    /// grandchild is orphaned into the group, still alive, before the stage
    /// even ends).
    #[cfg(unix)]
    fn spawn_stage_with_grandchild(
        group: &PipelineGroup,
        wait_on_child: bool,
    ) -> (command::RunningChild, i32) {
        use std::io::Read;
        let script = if wait_on_child {
            "sleep 30 & echo $!; wait"
        } else {
            "sleep 30 & echo $!"
        };
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd.stdout(std::process::Stdio::piped());
        let (mut child, _pgid) = crate::process::spawn_with_pgid(
            &mut cmd,
            crate::process::PgidPolicy::Join(group.leader_pgid()),
        )
        .expect("spawn /bin/sh under the group's pgid");
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut pid_line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stdout.read_exact(&mut byte).expect("read the grandchild pid");
            if byte[0] == b'\n' {
                break;
            }
            pid_line.push(byte[0]);
        }
        let pid: i32 = String::from_utf8(pid_line)
            .expect("ascii pid")
            .trim()
            .parse()
            .expect("a pid");
        let rc = command::RunningChild::assemble_with_owner(
            crate::process::ChildHandle::from_std(child),
            "sh".to_string(),
            command::ExternalPlumbing {
                stdout_pump: None,
                stderr_pump: None,
            },
            crate::process::StopPolicy::KillAndReap,
            command::GroupOwner::BorrowedByPipeline(group.leader_pgid()),
            crate::process::CancelScope::root(),
            None,
        );
        (rc, pid)
    }

    #[cfg(unix)]
    fn assert_dead_within_2s(pid: i32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let dead = unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if dead {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("grandchild pid {pid} outlived the collector's forced end");
    }

    /// A grandchild in the pipeline's pgid that its stage's own pid kill
    /// cannot reach.  Dropping the collector with the stage unobserved is a
    /// forced end, and the group kill it fires is what ends the grandchild.
    #[cfg(unix)]
    #[test]
    fn a_collector_dropped_short_of_observation_kills_the_group() {
        let shell = Shell::default();
        let group = PipelineGroup::prepare(super::super::resolve::TerminalPlan::NoTerminal, &shell)
            .expect("anchor spawns");
        let (rc, pid) = spawn_stage_with_grandchild(&group, true);

        let mut collect = CollectState::new(&group, std::time::Instant::now());
        collect.push(StageHandle::for_test(rc));
        drop(collect);

        assert_dead_within_2s(pid);
        drop(group);
    }

    /// The ordinary end kills nothing: a member that outlives its stage — a
    /// `spawn` worker that joined the pgid — outlives the pipeline too.
    #[cfg(unix)]
    #[test]
    fn a_collector_that_observed_every_stage_kills_nothing() {
        let shell = Shell::default();
        let mut group =
            PipelineGroup::prepare(super::super::resolve::TerminalPlan::NoTerminal, &shell)
                .expect("anchor spawns");
        let gate = StageGate::new();
        let mooring = Mooring::adrift();
        let (rc, pid) = spawn_stage_with_grandchild(&group, false);

        let mut collect = CollectState::new(&group, std::time::Instant::now());
        collect.push(StageHandle::for_test(rc));
        match collect.drive(&mut group, &gate, &mooring, &shell) {
            Drive::Done => {}
            Drive::Parked(_) => panic!("an ordinary exit must not park"),
        }
        drop(collect);

        assert!(
            unsafe { libc::kill(pid, 0) } == 0,
            "the orphaned grandchild must survive the collector's ordinary end"
        );
        unsafe { libc::kill(pid, libc::SIGKILL) };
        drop(group);
    }
}
