//! Pipeline collect phase for process-staged pipelines.
//!
//! The collector reduces a sequence of [`StageObservation`]s to a single
//! pipeline result.  Two algebraic types carry the lifecycle: a
//! [`StageOutcome`] per stage (Ok / Failure / Control), and a
//! [`PipelineBreak`] for the *one* break the collector eventually
//! surfaces (an ordinary failure, or a control-flow signal).  The policy
//! ("first failure wins, control trumps failure, first control wins") is
//! one explicit fold.

use super::super::command;
use super::launch::StageHandle;
use crate::types::*;

/// Wait on a direct-spawn external stage and reduce to a [`StageObservation`].
///
/// Direct-spawn externals bypass the pipeline helper, so they have no
/// audit-emitting evaluator to lean on.  When audit is active at the
/// parent we synthesise a single command node here so a direct-spawn
/// stage appears in the tree on equal footing with helper-routed
/// stages.
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

    let audit = synth_external_stage_audit(shell, &name, &failure, effective);

    if let Some(failure) = failure {
        let loc = shell.turn.loc.source_loc(name.len());
        let err = Error::from_command_failure(&name, failure, loc, shell);
        let err = super::augment_stage_failure(err, shell, started);
        Ok(StageObservation::failure(err).with_audit(audit))
    } else {
        Ok(StageObservation::ok(effective).with_audit(audit))
    }
}

/// Synthesise an audit fragment for one direct-spawn external stage.
/// Empty when audit is inactive at the parent.
fn synth_external_stage_audit(
    shell: &Shell,
    name: &str,
    failure: &Option<crate::process::CommandFailure>,
    status: i32,
) -> AuditFragment {
    if !shell.local.audit.active() {
        return AuditFragment::empty();
    }
    let site = shell.turn.loc.audit_site();
    let principal = shell.mobile.context.principal();
    let stderr = match failure {
        Some(f) => f.message(name).into_bytes(),
        None => Vec::new(),
    };
    let now = epoch_us();
    let node = ExecNode::command(
        name,
        Vec::new(),
        status,
        site,
        AuditIo {
            stdout: Vec::new(),
            stderr,
        },
        Value::Unit,
        Vec::new(),
        AuditTime {
            start: now,
            end: now,
        },
        principal,
    );
    AuditFragment::from_nodes(vec![node])
}

/// Semantic outcome for one stage.
///
/// A stage either succeeded, failed as ordinary command/evaluator
/// failure, or produced an [`Escape`] that must leave the collector
/// directly (`exit`, stopped foreground job).  Control carries an
/// `Escape`, not a `Break`, so an error — semantic or protocol-layer —
/// can never be read as a control signal.
pub(super) enum StageOutcome {
    Ok,
    Failure(Error),
    Control(Escape),
}

/// One stage's observation, normalized across external children and ral
/// helpers.  `status` is the effective exit code for `last_status`;
/// `final_value` is present only for the final value-typed ral stage;
/// `audit` carries the fragment for the collector to merge in
/// observation order.  The fragment is opaque to this collector — it
/// holds whatever audit nodes the stage produced, packed in the order
/// the stage emitted them.
pub(super) struct StageObservation {
    pub(super) status: i32,
    pub(super) outcome: StageOutcome,
    pub(super) final_value: Option<Value>,
    pub(super) audit: AuditFragment,
}

impl StageObservation {
    /// A successful stage with its effective exit code.
    pub(super) fn ok(status: i32) -> Self {
        Self {
            status,
            outcome: StageOutcome::Ok,
            final_value: None,
            audit: AuditFragment::empty(),
        }
    }

    /// An ordinary failure; `status` is the error's exit code.
    pub(super) fn failure(error: Error) -> Self {
        Self {
            status: error.exit_code(),
            outcome: StageOutcome::Failure(error),
            final_value: None,
            audit: AuditFragment::empty(),
        }
    }

    /// A control-flow escape.  `status` is never read for control
    /// outcomes — `fold` returns before the status branch.
    pub(super) fn control(escape: Escape) -> Self {
        Self {
            status: 0,
            outcome: StageOutcome::Control(escape),
            final_value: None,
            audit: AuditFragment::empty(),
        }
    }

    /// Classify a [`Break`] observed while waiting on a stage: a
    /// catchable error — including a protocol-layer one (report pipe,
    /// frame decode, waitpid) — is an ordinary failure; only an
    /// [`Escape`] is control flow.
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

/// The break the collector surfaces from `finish` once observation is
/// complete.  Either an ordinary failure (the user sees `Break::Error`
/// with `last_status` set) or an [`Escape`] that propagates upward as
/// `Break::Escape` (Stop hands the pipeline to the REPL; Exit bubbles
/// out of the evaluator).
///
/// First failure wins among failures; any control-flow escape supersedes
/// a prior ordinary failure (a Ctrl-Z on stage 2 should not be hidden
/// behind stage 1's nonzero exit); first control wins among controls.
enum PipelineBreak {
    Failure(Error),
    Control(Escape),
}

pub(super) struct PipelineCollector {
    break_: Option<PipelineBreak>,
    final_value: Option<Value>,
    /// Set when a stage parks the pipeline pgid.  Once set, the collect
    /// loop SIGSTOPs the whole group and abandons remaining handles so
    /// their `Drop` doesn't SIGKILL the parked pgid out from under us.
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

    /// True once a stage has parked the pipeline pgid.  Used from
    /// `collect` to decide whether the parked-pipeline abandon branch
    /// needs to fire.
    #[cfg(unix)]
    fn parked(&self) -> bool {
        self.stopped_pgid.is_some()
    }

    /// Note an ordinary failure.  First failure wins; a prior control
    /// keeps its place because control-flow always supersedes ordinary
    /// failure in `finish`.
    fn note_failure(&mut self, error: Error) {
        if self.break_.is_none() {
            self.break_ = Some(PipelineBreak::Failure(error));
        }
    }

    /// Note a control escape.  First control wins; a control supersedes
    /// any prior ordinary failure (so a Ctrl-Z reaching collect after a
    /// nonzero stage exit still reports as Stopped, not as the failure).
    fn note_control(&mut self, sig: Escape) {
        match self.break_ {
            Some(PipelineBreak::Control(_)) => {}
            _ => self.break_ = Some(PipelineBreak::Control(sig)),
        }
    }

    /// Note that a stage has parked the pipeline as a stopped job.  The
    /// group-wide SIGSTOP runs on the *first* stop only — it is
    /// idempotent in principle, but skipping it after the first stop
    /// keeps the unsafe call out of every later observation.
    #[cfg(unix)]
    fn note_stop(&mut self, signal: Escape) {
        if self.stopped_pgid.is_some() {
            return;
        }
        if let Escape::Stopped { pgid, .. } = &signal {
            self.stopped_pgid = Some(*pgid);
            // Park any still-running siblings so the whole pgid is in a
            // consistent stopped state before we hand it to the REPL.
            // Idempotent for stages already kernel-stopped via Ctrl-Z.
            pgid.signal_group(crate::process::Signal::new(libc::SIGSTOP));
        }
        self.note_control(signal);
    }

    /// Fold one [`StageObservation`] into the collector's accumulator
    /// state.  Every stage produces the same shape, so the policy ("first
    /// failure wins", "control trumps failure", "last value wins") lives
    /// only here.
    fn fold(&mut self, shell: &mut Shell, is_pipeline_final: bool, obs: StageObservation) {
        shell.local.audit.merge(obs.audit);
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
        shell: &mut Shell,
        started: std::time::Instant,
    ) -> PipelineCollector {
        let handles = std::mem::take(&mut self.handles);
        let mut collector = PipelineCollector::new();
        let last_idx = handles.len().saturating_sub(1);
        for (idx, handle) in handles.into_iter().enumerate() {
            // Once a sibling stage has parked the pipeline, abandon
            // remaining handles instead of waiting on them — calling
            // `wait` on a stopped child would block, and the default
            // `Drop` would SIGKILL the parked pgid out from under us.
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
            // An `Err` from observing a stage is a protocol-layer
            // failure (report pipe, frame decode, waitpid): classified
            // as an ordinary failure, so it can never supersede the
            // first genuine stage failure or read as control flow.
            let obs = result.unwrap_or_else(StageObservation::from_break);
            collector.fold(shell, is_pipeline_final, obs);
        }
        collector
    }
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use crate::process::{Pgid, Signal};

    /// A pgid the kernel will reject with ESRCH when we try to SIGSTOP it.
    /// `note_stop` invokes `kill(-pgid, SIGSTOP)` on the way through, so
    /// the test pgid must not match any real process group.
    const FAKE_PGID: i32 = 999_999_991;

    fn stopped_escape(pgid: i32) -> Escape {
        Escape::Stopped {
            pgid: Pgid(pgid),
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
        assert_eq!(c.stopped_pgid, Some(Pgid(FAKE_PGID)));
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
        // First Stopped wins; later calls do not overwrite the recorded pgid.
        assert_eq!(c.stopped_pgid, Some(Pgid(FAKE_PGID)));
    }

    #[test]
    fn note_stop_ignores_non_stopped_signals() {
        let mut c = PipelineCollector::new();
        c.note_stop(Escape::Exit(0));
        // The control bookkeeping still runs (records the break) but
        // stopped_pgid stays None — collect's park-aware branch hinges
        // on that field.
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
        // Control trumps failure: a Ctrl-Z / Exit observed after an
        // ordinary failure should still surface as control flow.
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
    fn helper_semantic_failure_is_failure_not_control() {
        // A ral helper reporting `WireOutcome::Error` reaches the
        // collector as `StageOutcome::Failure`, not `Control` — the
        // user gets the diagnostic with `last_status` set, not an
        // opaque `Break::Error` bubbling out as control flow.
        let mut c = PipelineCollector::new();
        let mut shell = Shell::default();
        c.fold(
            &mut shell,
            true,
            StageObservation::failure(make_error(5, "helper boom")),
        );
        assert!(matches!(c.break_, Some(PipelineBreak::Failure(_))));
    }

    #[test]
    fn final_status_recorded_on_success() {
        let mut c = PipelineCollector::new();
        let mut shell = Shell::default();
        c.fold(&mut shell, true, StageObservation::ok(42));
        assert_eq!(shell.mobile.control.last_status, 42);
        assert!(c.break_.is_none());
    }

    #[test]
    fn final_status_not_recorded_when_control_preempts() {
        let mut c = PipelineCollector::new();
        let mut shell = Shell::default();
        shell.mobile.control.last_status = 0;
        c.fold(
            &mut shell,
            true,
            StageObservation {
                status: 99,
                outcome: StageOutcome::Control(Escape::Exit(3)),
                final_value: None,
                audit: AuditFragment::empty(),
            },
        );
        // `is_pipeline_final` post-control branch is skipped, so the
        // status the stage reported never lands on the shell.
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
        // A protocol-layer error (report pipe, frame decode, waitpid)
        // folded after a genuine stage failure is an ordinary failure:
        // first failure still wins, and the break stays a Failure.
        let mut c = PipelineCollector::new();
        let mut shell = Shell::default();
        c.fold(
            &mut shell,
            false,
            StageObservation::failure(make_error(7, "stage one boom")),
        );
        c.fold(
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
