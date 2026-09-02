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
use super::group::PipelineGroup;
use super::launch::{Probe, StageHandle};
use crate::evaluator::audit::observe_stamped;
#[cfg(unix)]
use crate::process::Signal;
use crate::process::{CancelCause, StageGate, StageKill, StageState};
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
    kill: StageKill,
    shell: &Shell,
    started: std::time::Instant,
) -> StageObservation {
    let name = running.name.clone();
    let failure = match running.observe(kill) {
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

impl PipelineCollector {
    fn new() -> Self {
        Self {
            break_: None,
            final_value: None,
        }
    }

    /// Breaks join by rank — an escape outranks an error outranks success —
    /// and ties go to the earlier stage, the fold being launch-ordered.  So a
    /// Ctrl-Z on stage 2 displaces stage 1's nonzero exit, while a second
    /// failure never displaces the first.
    fn note(&mut self, br: Break) {
        if matches!(
            (&self.break_, &br),
            (None, _) | (Some(Break::Error(_)), Break::Escape(_))
        ) {
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

pub(super) struct Running {
    handles: Vec<StageHandle>,
}

impl Running {
    pub(super) fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    pub(super) fn add(&mut self, handle: StageHandle) {
        self.handles.push(handle);
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
}

/// What a stop does to this collector.  Only a tty-owning group has a job
/// table to resume it, so only it parks; any other owned group is cancelled,
/// as `StopPolicy::KillAndReap` does for a lone external; a joining collector
/// forwards the stop into its own stage's status — the report its owner
/// reads — and forgets it here, never parking, signalling, or escaping.
#[derive(Debug, PartialEq, Eq)]
enum OnStop {
    Park,
    Cancel,
    Forward,
}

fn on_stop(group: &PipelineGroup) -> OnStop {
    if group.owns_tty() {
        OnStop::Park
    } else if group.owned() {
        OnStop::Cancel
    } else {
        OnStop::Forward
    }
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
    pub(super) fn new(running: Running, started: std::time::Instant) -> Self {
        let n = running.handles.len();
        Self {
            stages: running.handles.into_iter().map(Some).collect(),
            observed: (0..n).map(|_| None).collect(),
            started,
        }
    }

    /// One non-blocking pass over every unobserved stage, tail-first: kill a
    /// stage whose reader has already settled, observe whichever finished,
    /// and detect a stop — through the anchor or a stage's own status — and
    /// answer it per [`on_stop`].
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
            Some(Witnessed::Stopped(sig)) => match on_stop(group) {
                OnStop::Park => {
                    stop_the_group(group, gate);
                    return Pass::Parked(sig);
                }
                OnStop::Cancel => Some(CancelCause::Terminate),
                OnStop::Forward => unreachable!("a joining group has no anchor to witness"),
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
            if self.observed.get(ix + 1).is_some_and(Option::is_some) {
                handle.kill_for_dead_reader();
            }
            match handle.probe() {
                Probe::Running => {}
                Probe::Ready => {
                    let obs = self.stages[ix]
                        .take()
                        .expect("probed above")
                        .observe(shell, ix + 1 == n, self.started);
                    self.observed[ix] = Some(obs);
                    progress = true;
                }
                Probe::Stopped(sig) => match on_stop(group) {
                    #[cfg(unix)]
                    OnStop::Park => {
                        stop_the_group(group, gate);
                        return Pass::Parked(sig);
                    }
                    #[cfg(not(unix))]
                    OnStop::Park => unreachable!("nothing stops on Windows"),
                    OnStop::Cancel => {
                        return self.cancel_all(group, CancelCause::Terminate, shell);
                    }
                    OnStop::Forward => {
                        if let Some(park) = &mooring.park {
                            park.status.set(StageState::Stopped(sig));
                        }
                        handle.resume();
                    }
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

    /// Signal the group once, cancel every stage, then observe them
    /// tail-first, blocking: the final stage leaves first and drops its
    /// reader end, which `EPIPE`s the stage before it, and so on up the
    /// pipeline.
    fn cancel_all(&mut self, group: &mut PipelineGroup, cause: CancelCause, shell: &Shell) -> Pass {
        group.signal(cause);
        self.cancel_stages(cause);
        let n = self.stages.len();
        for ix in (0..n).rev() {
            if let Some(handle) = self.stages[ix].take() {
                let obs = handle.observe(shell, ix + 1 == n, self.started);
                self.observed[ix] = Some(obs);
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
    pub(super) fn cancel_stages(&self, cause: CancelCause) {
        for handle in self.stages.iter().flatten() {
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

        let mut running = Running::new();
        running.add(StageHandle::for_test(spawn_exiting("false")));
        running.add(StageHandle::for_test(spawn_exiting("true")));
        let mut collect = CollectState::new(running, std::time::Instant::now());

        match collect.drive(&mut group, &gate, &mooring, &shell) {
            Drive::Done => {}
            #[cfg(unix)]
            Drive::Parked(_) => panic!("two ordinary exits must not park"),
        }
        let folded = collect.fold(&mooring, &mut shell);
        match folded.break_ {
            Some(Break::Error(error)) => assert_ne!(error.exit_code(), 0),
            other => panic!("expected `false`'s nonzero exit to fold in, got {other:?}"),
        }
    }

    /// `/bin/false` or `/bin/true`, wrapped exactly as a direct external
    /// pipeline stage: `GroupOwner::None` never parks (`RunningChild::parks`),
    /// so `StopPolicy` is irrelevant here.
    fn spawn_exiting(name: &str) -> command::RunningChild {
        let mut cmd = std::process::Command::new(format!("/bin/{name}"));
        let child = cmd.spawn().unwrap_or_else(|e| panic!("spawn /bin/{name}: {e}"));
        command::RunningChild::assemble_with_owner(
            crate::process::ChildHandle::from_std(child),
            name.to_string(),
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

    /// `/bin/sleep 30` under its own pgid, real-`SIGSTOP`'d — the stop a
    /// direct external pipeline stage sees, without a whole pipeline launch to
    /// set one up.  `Drop` (via `RunningChild`'s) kills it regardless of the
    /// stop, so nothing survives the test.
    #[cfg(unix)]
    fn spawn_stopped_sleep(stop: crate::process::StopPolicy) -> command::RunningChild {
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg("30");
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
            stop,
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
        assert_eq!(on_stop(&tty), OnStop::Park);
        assert_eq!(on_stop(&batch), OnStop::Cancel);
        assert_eq!(on_stop(&joining), OnStop::Forward);
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
        assert!(group.owns_tty());
        let gate = StageGate::new();
        let mooring = Mooring::adrift();

        let mut running = Running::new();
        running.add(StageHandle::for_test(spawn_stopped_sleep(
            crate::process::StopPolicy::Escape,
        )));
        let mut collect = CollectState::new(running, std::time::Instant::now());

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
            status: crate::process::StageStatus::new(),
        };
        let mooring = Mooring {
            park: Some(park.clone()),
            ..Mooring::adrift()
        };
        let shell = Shell::default();

        let mut running = Running::new();
        running.add(StageHandle::for_test(spawn_stopped_sleep(
            crate::process::StopPolicy::Escape,
        )));
        let mut collect = CollectState::new(running, std::time::Instant::now());

        for _ in 0..100 {
            match collect.pass(&mut group, &gate, &mooring, &shell) {
                Pass::Parked(_) => panic!("a joining collector must never park"),
                Pass::Done => panic!("a mere stop must not be observed as done"),
                Pass::Advanced | Pass::Idle => {}
            }
            if matches!(park.status.get(), StageState::Stopped(_)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the stop was never forwarded to the joining collector's own stage status");
    }
}
