//! Collect phase: fold a process-staged pipeline's per-stage observations
//! into one result.
//!
//! Stages are observed in launch order; the whole break policy lives in
//! `PipelineCollector`'s three `note_*` methods.

use super::super::command;
use super::launch::StageHandle;
use crate::evaluator::audit::observe_stamped;
use crate::types::{
    AuditFragment, AuditIo, Break, CommandOrigin, Error, Escape, Mooring, Observation, Observed,
    Settled, Shell, Value, epoch_us,
};

/// Wait on a direct-spawn external stage and reduce it to a [`StageObservation`].
///
/// Such a stage has no audit-emitting evaluator behind it, so its one command
/// node is synthesised here to keep it level with helper-routed stages.
#[allow(
    clippy::unnecessary_wraps,
    reason = "sibling of `StageHandle::observe` in the collect match; both arms must yield the same `Settled<StageObservation>` so the fold can `unwrap_or_else` uniformly."
)]
fn observe_external_stage(
    running: command::RunningChild,
    is_last: bool,
    shell: &Shell,
    started: std::time::Instant,
) -> Settled<StageObservation> {
    let name = running.name.clone();
    let (code, failure) = match running.observe(!is_last) {
        Ok(pair) => pair,
        Err(br) => return Ok(StageObservation::from_break(br)),
    };
    let effective = if failure.is_none() { 0 } else { code };

    let err = failure.map(|f| Error::from_command_failure(&name, f, shell));
    let audit = synth_external_stage_audit(shell, &name, err.as_ref(), effective);

    if let Some(err) = err {
        let err = super::augment_stage_failure(err, shell, started);
        Ok(StageObservation::failure(err).with_audit(audit))
    } else {
        Ok(StageObservation::ok(effective).with_audit(audit))
    }
}

/// The fragment is empty when audit is inactive at the parent.
fn synth_external_stage_audit(
    shell: &Shell,
    name: &str,
    err: Option<&Error>,
    status: i32,
) -> AuditFragment {
    if !shell.local.audit.active() {
        return AuditFragment::empty();
    }
    let site = shell.call_site();
    let principal = shell.mobile.context.principal();
    let (status, value, error) = match err {
        Some(e) => (e.exit_code(), Value::Unit, Some(e.message.clone())),
        None => (status, Value::Unit, None),
    };
    let now = epoch_us();
    let obs = Observation::spanning(
        site,
        now,
        now,
        principal,
        Observed::Command {
            argv: vec![name.to_string()],
            status,
            origin: CommandOrigin::External,
            io: AuditIo::default(),
            error,
            value,
        },
    );
    AuditFragment::from_observations(vec![obs])
}

/// Semantic outcome for one stage.
///
/// `Control` carries an [`Escape`], not a [`Break`], so no error — semantic
/// or protocol-layer — can be read as a control signal.
pub(super) enum StageOutcome {
    Ok,
    Failure(Error),
    Control(Escape),
}

/// One stage's observation, normalized across external children and ral
/// helpers.  `status` is the effective exit code for `last_status`, and
/// `final_value` is set only by the final value-typed ral stage.
pub(super) struct StageObservation {
    pub(super) status: i32,
    pub(super) outcome: StageOutcome,
    pub(super) final_value: Option<Value>,
    pub(super) audit: AuditFragment,
}

impl StageObservation {
    pub(super) fn ok(status: i32) -> Self {
        Self {
            status,
            outcome: StageOutcome::Ok,
            final_value: None,
            audit: AuditFragment::empty(),
        }
    }

    pub(super) fn failure(error: Error) -> Self {
        Self {
            status: error.exit_code(),
            outcome: StageOutcome::Failure(error),
            final_value: None,
            audit: AuditFragment::empty(),
        }
    }

    /// The zero `status` is never read: `fold` returns before the status branch.
    pub(super) fn control(escape: Escape) -> Self {
        Self {
            status: 0,
            outcome: StageOutcome::Control(escape),
            final_value: None,
            audit: AuditFragment::empty(),
        }
    }

    /// A protocol-layer break (report pipe, frame decode, waitpid) lands here
    /// as an ordinary failure; only an escape is control flow.
    pub(super) fn from_break(br: Break) -> Self {
        match br {
            Break::Error(err) => Self::failure(err),
            Break::Escape(esc) => Self::control(esc),
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

/// The one break `finish` surfaces once every stage has been observed.
enum PipelineBreak {
    Failure(Error),
    Control(Escape),
}

pub(super) struct PipelineCollector {
    break_: Option<PipelineBreak>,
    final_value: Option<Value>,
    /// Set when a stage parks the pipeline pgid.  While set, `collect`
    /// abandons the remaining handles rather than let their `Drop` SIGKILL
    /// the parked pgid out from under us.
    #[cfg(unix)]
    stopped_pgid: Option<crate::process::Pgid>,
}

impl PipelineCollector {
    fn new() -> Self {
        Self {
            break_: None,
            final_value: None,
            #[cfg(unix)]
            stopped_pgid: None,
        }
    }

    #[cfg(unix)]
    fn parked(&self) -> bool {
        self.stopped_pgid.is_some()
    }

    /// First failure wins.  A prior control blocks this one too, harmlessly:
    /// control outranks failure anyway.
    fn note_failure(&mut self, error: Error) {
        if self.break_.is_none() {
            self.break_ = Some(PipelineBreak::Failure(error));
        }
    }

    /// First control wins, but any control displaces a prior failure: a Ctrl-Z
    /// on stage 2 must not hide behind stage 1's nonzero exit.
    fn note_control(&mut self, sig: Escape) {
        match self.break_ {
            Some(PipelineBreak::Control(_)) => {}
            _ => self.break_ = Some(PipelineBreak::Control(sig)),
        }
    }

    /// Park the whole group, but only when this stop will be the surfaced
    /// control: not once the pipeline is already parked, and not when an
    /// earlier escape holds the break — that escape wins in `finish`, so
    /// parking would stop a pipeline that is really exiting.
    #[cfg(unix)]
    fn note_stop(&mut self, signal: Escape) {
        let already_controlled = matches!(self.break_, Some(PipelineBreak::Control(_)));
        if self.stopped_pgid.is_none()
            && !already_controlled
            && let Escape::Stopped { pgid, .. } = &signal
        {
            self.stopped_pgid = Some(*pgid);
            // A no-op for the siblings the Ctrl-Z already stopped.
            pgid.signal_group(crate::process::Signal::new(libc::SIGSTOP));
        }
        self.note_control(signal);
    }

    /// The audit observations broadcast before the outcome is classified, so
    /// a stage that fails or escapes still contributes what it observed.
    /// Reporting each rather than merging them in puts a helper stage's writes
    /// and execs on the rail — though only where the parent already holds a
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
        match obs.outcome {
            StageOutcome::Ok => {}
            StageOutcome::Failure(err) => {
                self.note_failure(err);
            }
            #[cfg(unix)]
            StageOutcome::Control(esc @ Escape::Stopped { .. }) => {
                self.note_stop(esc);
                return;
            }
            StageOutcome::Control(esc) => {
                self.note_control(esc);
                return;
            }
        }
        if is_pipeline_final {
            shell.mobile.control.last_status = obs.status;
            self.final_value = obs.final_value;
        }
    }

    pub(super) fn finish(
        self,
        shell: &mut Shell,
        last_output: crate::mode::PipeMode,
    ) -> Settled<Value> {
        if let Some(br) = self.break_ {
            return Err(match br {
                PipelineBreak::Control(esc) => Break::Escape(esc),
                PipelineBreak::Failure(error) => {
                    shell.mobile.control.last_status = error.exit_code();
                    Break::Error(error)
                }
            });
        }
        match last_output {
            crate::mode::PipeMode::Bytes => Ok(Value::Unit),
            _ => Ok(self.final_value.unwrap_or(Value::Unit)),
        }
    }
}

pub(super) struct RunningPipeline {
    handles: Vec<StageHandle>,
}

impl RunningPipeline {
    pub(super) fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    pub(super) fn add(&mut self, handle: StageHandle) {
        self.handles.push(handle);
    }

    pub(super) fn collect(
        mut self,
        mooring: &Mooring,
        shell: &mut Shell,
        started: std::time::Instant,
    ) -> PipelineCollector {
        let handles = std::mem::take(&mut self.handles);
        let mut collector = PipelineCollector::new();
        let last_idx = handles.len().saturating_sub(1);
        for (idx, handle) in handles.into_iter().enumerate() {
            // `wait` on a stopped child would block, and `Drop` would SIGKILL
            // the parked pgid, so abandon what is left instead.
            #[cfg(unix)]
            if collector.parked() {
                match handle {
                    StageHandle::External(h) => {
                        h.abandon();
                    }
                    StageHandle::Helper(h) => h.abandon(),
                }
                continue;
            }
            let is_pipeline_final = idx == last_idx;
            let result = match handle {
                StageHandle::External(h) => {
                    observe_external_stage(h, is_pipeline_final, shell, started)
                }
                StageHandle::Helper(h) => h.observe(shell, is_pipeline_final, started),
            };
            let obs = result.unwrap_or_else(StageObservation::from_break);
            collector.fold(mooring, shell, is_pipeline_final, obs);
        }
        collector
    }
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use crate::process::{Pgid, Signal};

    /// `note_stop` does a real `kill(-pgid, SIGSTOP)` on the way through, so
    /// this must match no live process group and earn an ESRCH.
    const FAKE_PGID: i32 = 999_999_991;

    fn stopped_escape(pgid: i32) -> Escape {
        Escape::Stopped {
            pgid: Pgid::from_raw(pgid).expect("a child pid is positive"),
            signal: Signal::new(libc::SIGTSTP),
            cmd: "test-stage".into(),
        }
    }

    fn make_error(status: i32, msg: &str) -> Error {
        Error::new(msg.to_string(), status)
    }

    #[test]
    fn note_stop_records_pgid_and_marks_break() {
        let mut c = PipelineCollector::new();
        c.note_stop(stopped_escape(FAKE_PGID));
        assert_eq!(
            c.stopped_pgid,
            Some(Pgid::from_raw(FAKE_PGID).expect("the fake pgid is positive"))
        );
        assert!(matches!(
            c.break_,
            Some(PipelineBreak::Control(Escape::Stopped { .. }))
        ));
    }

    #[test]
    fn note_stop_is_idempotent_first_stop_wins() {
        let mut c = PipelineCollector::new();
        c.note_stop(stopped_escape(FAKE_PGID));
        c.note_stop(stopped_escape(FAKE_PGID + 1));
        assert_eq!(
            c.stopped_pgid,
            Some(Pgid::from_raw(FAKE_PGID).expect("the fake pgid is positive"))
        );
    }

    #[test]
    fn note_stop_skips_park_when_control_already_held() {
        let mut c = PipelineCollector::new();
        c.note_control(Escape::Exit(3));
        c.note_stop(stopped_escape(FAKE_PGID));
        assert!(c.stopped_pgid.is_none());
        assert!(matches!(
            c.break_,
            Some(PipelineBreak::Control(Escape::Exit(3)))
        ));
    }

    #[test]
    fn note_stop_ignores_non_stopped_signals() {
        let mut c = PipelineCollector::new();
        c.note_stop(Escape::Exit(0));
        // The control bookkeeping still runs; only the parking is skipped.
        assert!(c.stopped_pgid.is_none());
        assert!(matches!(
            c.break_,
            Some(PipelineBreak::Control(Escape::Exit(0)))
        ));
    }

    #[test]
    fn first_failure_wins_among_failures() {
        let mut c = PipelineCollector::new();
        c.note_failure(make_error(7, "first"));
        c.note_failure(make_error(9, "second"));
        match c.break_ {
            Some(PipelineBreak::Failure(error)) => {
                assert_eq!(error.exit_code(), 7);
                assert_eq!(error.message, "first");
            }
            _ => panic!("expected Failure(7,'first'); got {:?}", c.break_.is_some()),
        }
    }

    #[test]
    fn control_supersedes_prior_failure() {
        let mut c = PipelineCollector::new();
        c.note_failure(make_error(7, "early failure"));
        c.note_control(Escape::Exit(3));
        assert!(matches!(
            c.break_,
            Some(PipelineBreak::Control(Escape::Exit(3)))
        ));
    }

    #[test]
    fn first_control_wins_among_controls() {
        let mut c = PipelineCollector::new();
        c.note_control(Escape::Exit(1));
        c.note_control(Escape::Exit(2));
        assert!(matches!(
            c.break_,
            Some(PipelineBreak::Control(Escape::Exit(1)))
        ));
    }

    #[test]
    fn final_status_recorded_on_success() {
        let mut c = PipelineCollector::new();
        let mut shell = Shell::default();
        c.fold(
            &Mooring::adrift(),
            &mut shell,
            true,
            StageObservation::ok(42),
        );
        assert_eq!(shell.mobile.control.last_status, 42);
        assert!(c.break_.is_none());
    }

    #[test]
    fn final_status_not_recorded_when_control_preempts() {
        let mut c = PipelineCollector::new();
        let mut shell = Shell::default();
        shell.mobile.control.last_status = 0;
        c.fold(
            &Mooring::adrift(),
            &mut shell,
            true,
            StageObservation {
                status: 99,
                outcome: StageOutcome::Control(Escape::Exit(3)),
                final_value: None,
                audit: AuditFragment::empty(),
            },
        );
        assert_eq!(shell.mobile.control.last_status, 0);
    }

    #[test]
    fn from_break_classifies_error_as_failure() {
        let obs = StageObservation::from_break(Break::Error(make_error(4, "report pipe: boom")));
        assert_eq!(obs.status, 4);
        assert!(matches!(obs.outcome, StageOutcome::Failure(_)));
    }

    #[test]
    fn from_break_classifies_escape_as_control() {
        let obs = StageObservation::from_break(Break::Escape(Escape::Exit(3)));
        assert!(matches!(
            obs.outcome,
            StageOutcome::Control(Escape::Exit(3))
        ));
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
            Some(PipelineBreak::Failure(error)) => assert_eq!(error.message, "stage one boom"),
            _ => panic!("expected the first stage failure to win"),
        }
    }
}
