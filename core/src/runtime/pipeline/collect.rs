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
use super::launch::StageHandle;
use crate::evaluator::audit::observe_stamped;
use crate::process::StageKill;
use crate::types::{
    AuditFragment, AuditIo, Break, CommandOrigin, Error, Mooring, Observation, Observed, Settled,
    Shell, Value, epoch_us,
};
/// Only the park has a use for the escape's payload, and only Unix parks.
#[cfg(unix)]
use crate::types::Escape;

/// Wait on a direct-spawn external stage and reduce it to a [`StageObservation`].
///
/// Such a stage has no audit-emitting evaluator behind it, so its one command
/// node is synthesised here to keep it level with helper-routed stages.
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
/// helpers.  `final_value` is set only by the final value-typed ral stage,
/// so a stage that broke carries none.
///
/// The break is a [`Break`], whose own two constructors are already the
/// classification the fold needs: a protocol-layer failure (report pipe,
/// frame decode, waitpid) arrives as `Error` like any other, and only an
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
    /// each rather than merging them in puts a helper stage's writes and execs
    /// on the rail — though only where the parent already holds a trail, since
    /// that is what makes a stage collect at all.
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

    /// The event loop.  Stages are observed as they end, in whatever order
    /// that happens — a non-blocking probe per still-running stage, not a
    /// blocking wait on one at a time — so a stage stuck reading from a dead
    /// writer never wedges the collector against a producer that has merely
    /// stopped itself.  A stage whose reader has settled is killed, and that
    /// kill is the one death forgiven.  A stop parks the group at once.
    /// Verdicts fold in launch order, over observations buffered during the
    /// walk.
    pub(super) fn collect(
        mut self,
        mooring: &Mooring,
        shell: &mut Shell,
        started: std::time::Instant,
    ) -> PipelineCollector {
        let handles = std::mem::take(&mut self.handles);
        let n = handles.len();
        let mut stages: Vec<Option<StageHandle>> = handles.into_iter().map(Some).collect();
        let mut observed: Vec<Option<StageObservation>> = (0..n).map(|_| None).collect();
        #[cfg(unix)]
        let mut parked = false;
        let mut interval = std::time::Duration::from_millis(5);
        let cap = std::time::Duration::from_millis(100);

        while stages.iter().any(Option::is_some) {
            // On cancellation, stop probing and observe everything blocking,
            // tail-first: the first `wait` performs the group teardown and
            // attribution, and the rest reap what it felled.
            let cancelled = crate::process::check(mooring).is_err();
            let mut progress = false;
            for ix in (0..n).rev() {
                let Some(handle) = stages[ix].as_mut() else {
                    continue;
                };
                if observed.get(ix + 1).is_some_and(Option::is_some) {
                    handle.kill_for_dead_reader();
                }
                if !cancelled && !handle.try_settle() {
                    continue;
                }
                let obs = stages[ix]
                    .take()
                    .expect("probed above")
                    .observe(shell, ix + 1 == n, started);
                #[cfg(unix)]
                if let Some(Break::Escape(Escape::Stopped { pgid, .. })) = &obs.break_ {
                    parked = true;
                    pgid.signal_group(crate::process::Signal::new(libc::SIGSTOP));
                }
                observed[ix] = Some(obs);
                progress = true;
                #[cfg(unix)]
                if parked {
                    break;
                }
            }
            // `wait` on a stopped child would block, so stop probing at once.
            #[cfg(unix)]
            if parked {
                break;
            }
            if progress {
                interval = std::time::Duration::from_millis(5);
            } else if !cancelled {
                std::thread::sleep(interval);
                interval = (interval * 2).min(cap);
            }
        }

        // Only a parked pipeline leaves a stage unobserved, and `Drop` would
        // SIGKILL the pgid the kernel is holding stopped.
        for handle in stages.into_iter().flatten() {
            handle.abandon();
        }

        let mut collector = PipelineCollector::new();
        for (ix, obs) in observed.into_iter().enumerate() {
            if let Some(obs) = obs {
                collector.fold(mooring, shell, ix + 1 == n, obs);
            }
        }
        collector
    }
}

#[cfg(test)]
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
}
