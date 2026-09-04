//! Collect phase: fold a process-staged pipeline's per-stage observations
//! into one result.
//!
//! Observation order is the order stages actually end, not launch order: a
//! non-final stage may outlive its reader (a child that stops itself, or one
//! that keeps writing after the reader is long gone), so collection reacts to
//! whichever stage ends next rather than blocking on one at a time. The kill
//! cascade is tail-driven — a stage is killed once its reader has settled —
//! and that kill is the one death forgiven. Verdict precedence — first
//! failure wins, control outranks failure — folds in launch order, over
//! observations buffered during the walk.  A stop never reaches this fold at
//! all, whether a member's or the anchor's own: the reaper answers every stop
//! with `SIGCONT` itself, the one rule in one place.
//!
//! Every event in the pipeline's lifecycle — an external's exit from the
//! reaper, a thread stage's own report, the mooring's or the anchor's own
//! cancel — arrives as one [`Event`] on one channel.  [`step`] is the pure
//! fold over it: it never blocks, never signals, never touches a process — it
//! inspects [`CollectState`] and returns the [`Effect`]s a thin interpreter
//! (`CollectState::run`) performs.  Attribution lives in the fold too:
//! `sent` records, for every stage, the strongest cause anything here ever
//! sent it, and [`CollectState::fold`] is the one place that reads it back
//! against what actually happened, since only there does `&Shell` reach an
//! external's settlement at all.

use super::super::command;
use super::group::PipelineGroup;
use super::launch::StageHandle;
use crate::evaluator::audit::observe_stamped;
use crate::process::{CancelCause, CancelWatch, CommandFailure, Pgid, watch_cancel};
use crate::types::{
    AuditFragment, AuditIo, Break, CommandOrigin, Error, Mooring, Observation, Observed, Settled,
    Shell, Value, epoch_us,
};
#[cfg(unix)]
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::{Receiver, Sender};

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

/// The shell-touching finish an external stage's raw [`crate::process::WaitOutcome`]
/// still needs — attribution against `sent`, audit synthesis, exit-hint
/// lookup, sandbox-denial augmentation — none of which the reaper's own
/// posting closure may do, holding no `&Shell` of its own.
fn finish_external_settlement(
    name: &str,
    outcome: crate::process::WaitOutcome,
    sent: Option<CancelCause>,
    shell: &Shell,
    started: std::time::Instant,
) -> StageObservation {
    let err = CommandFailure::from_outcome(outcome, sent)
        .map(|f| Error::from_command_failure(name, f, shell));
    let audit = synth_external_stage_audit(shell, name, err.as_ref());
    match err {
        Some(err) => {
            StageObservation::failure(super::augment_stage_failure(err, shell, started)).with_audit(audit)
        }
        None => StageObservation::ok().with_audit(audit),
    }
}

/// One stage's terminal news, before the fold's shell-touching finish: a
/// thread stage's own closure already computed its whole [`StageObservation`]
/// and this carries it as-is; an external stage carries only the raw
/// outcome the reaper posted, since [`finish_external_settlement`] is
/// [`CollectState::fold`]'s to run, with the `&Shell` nothing upstream of it
/// may hold — and its still-running pumps, whose join-or-detach the fold
/// decides by `sent` once the walk, and any teardown kill, is over.
pub(super) enum StageEnd {
    Thread(StageObservation),
    External {
        name: String,
        outcome: crate::process::WaitOutcome,
        jail: Option<crate::process::jail::JailCgroup>,
        pumps: command::Pumps,
    },
}

/// The collector's one wire type: every lifecycle edge, from whichever
/// producer sees it first.
pub(super) enum Event {
    /// An external's raw exit, from the reaper.
    Ended(usize, crate::process::WaitOutcome),
    /// A thread stage's own report: its own send, or its [`SettleOnDrop`]
    /// when the producer thread unwound first.
    Returned(usize, StageObservation),
    /// The mooring scope, the anchor's report pipe, or the anchor's own
    /// death — every one of them tears the whole pipeline down.
    Cancelled(CancelCause),
}

/// Owned by the one producer thread responsible for index `ix`: a stage
/// thread's closure.  Disarmed by [`Self::send`]; dropped armed — an unwind
/// anywhere on that thread, including before the closure body ran — it
/// posts the observation itself, so no index can go silent whoever else
/// still holds a sender.
pub(super) struct SettleOnDrop {
    ix: usize,
    tx: Option<Sender<Event>>,
}

impl SettleOnDrop {
    pub(super) fn new(ix: usize, tx: Sender<Event>) -> Self {
        Self { ix, tx: Some(tx) }
    }

    pub(super) fn send(mut self, obs: StageObservation) {
        let tx = self.tx.take().expect("armed until send or drop");
        let _ = tx.send(Event::Returned(self.ix, obs));
    }
}

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let msg = if std::thread::panicking() {
                "ral pipeline stage panicked"
            } else {
                "ral pipeline stage ended without reporting"
            };
            let _ = tx.send(Event::Returned(
                self.ix,
                StageObservation::failure(Error::new(msg.to_string(), 1)),
            ));
        }
    }
}

/// [`step`]'s pure output: what the interpreter ([`CollectState::run`]) must
/// perform for one event's worth of consequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// The reader-gone cascade's own kill: the one death forgiven.  Never
    /// reused for any other kill — see [`StageHandle::kill_now`]'s doc.
    KillStage(usize),
    CancelAll(CancelCause),
    Done,
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

/// Live collector state: every stage still running or already observed.
pub(super) struct CollectState {
    stages: Vec<Option<StageHandle>>,
    observed: Vec<Option<StageEnd>>,
    /// What this collector has itself sent stage `ix`, joined by `max` where
    /// two causes land on the same stage.  Recorded here, not on the stage or
    /// its child, since the collector is the one party that knows what it did
    /// to whom.
    sent: Vec<Option<CancelCause>>,
    started: std::time::Instant,
    /// The pgid a forced end of this collector kills; `None` for a joining
    /// collector, whose owner's teardown does it.
    owned_group: Option<Pgid>,
    /// Every producer's [`Event`] arrives here: the reaper (through a stage's
    /// own watch), a stage thread's own report, the anchor's witness, the
    /// cancel watch.
    rx: Receiver<Event>,
    /// This collector's own clone, held only while stages are still being
    /// launched: [`Self::all_stages_launched`] drops it, so the channel can
    /// close once every producer is gone.  A thread stage's own end always
    /// arrives as `Returned` — by its own send, or by its [`SettleOnDrop`]
    /// when the producer thread unwinds first.
    tx: Option<Sender<Event>>,
    /// The mooring scope, posting [`Event::Cancelled`] the instant it is
    /// cancelled; dropping this disarms it.
    _cancel: CancelWatch,
}

impl CollectState {
    pub(super) fn new(group: &mut PipelineGroup, mooring: &Mooring, started: std::time::Instant) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        #[cfg(unix)]
        if group.owned() {
            group.start_witness(tx.clone());
        }
        let cancel_tx = tx.clone();
        let cancel = watch_cancel(mooring.cancel.as_scope().clone(), move |cause| {
            let _ = cancel_tx.send(Event::Cancelled(cause));
        });
        Self {
            stages: Vec::new(),
            observed: Vec::new(),
            sent: Vec::new(),
            started,
            owned_group: group.owned().then(|| group.leader_pgid()),
            rx,
            tx: Some(tx),
            _cancel: cancel,
        }
    }

    /// A `CollectState` for `step`'s own transition-table tests: no group, no
    /// anchor, no processes — just the state `step` folds over, with a real
    /// channel so `push`ed [`StageHandle`]s (built with
    /// [`StageHandle::fake_external_for_step_test`]) still have somewhere to
    /// send from, unused by the tests that construct events by hand instead.
    /// The watch is held on a fresh root scope nothing ever cancels.
    #[cfg(test)]
    pub(super) fn for_step_test() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = watch_cancel(crate::process::CancelScope::root(), |_| {});
        Self {
            stages: Vec::new(),
            observed: Vec::new(),
            sent: Vec::new(),
            started: std::time::Instant::now(),
            owned_group: None,
            rx,
            tx: Some(tx),
            _cancel: cancel,
        }
    }

    /// The index a stage about to be launched will occupy once pushed, for
    /// [`super::thread::launch_thread_stage`]'s closure (and a direct
    /// external's own watch) to name itself by in its own reports.
    pub(super) fn next_index(&self) -> usize {
        self.stages.len()
    }

    /// A clone for a stage's own producer — see [`Event`] and
    /// [`Self::all_stages_launched`].
    pub(super) fn sender(&self) -> Sender<Event> {
        self.tx
            .as_ref()
            .expect("a stage is still launching, so the collector's own sender is alive")
            .clone()
    }

    /// Every stage is now launched, so the last clone this collector itself
    /// held is dropped.
    pub(super) fn all_stages_launched(&mut self) {
        self.tx = None;
    }

    pub(super) fn push(&mut self, handle: StageHandle) {
        self.stages.push(Some(handle));
        self.observed.push(None);
        self.sent.push(None);
    }

    /// Block for the next event.  `None` once every producer is gone —
    /// a defect elsewhere, since a stage's own end always arrives first —
    /// treated the same as `Done` itself: there is nothing further
    /// collection can do.
    fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    /// An external's own terminal event: file it as a [`StageEnd::External`]
    /// — [`StageHandle::file_external_end`] reaps the watch, carrying the
    /// jail and the pumps out for [`Self::fold`] to finish and settle by
    /// this stage's own `sent` — then the reader-gone cascade.
    fn on_ended(&mut self, ix: usize, outcome: crate::process::WaitOutcome) -> Vec<Effect> {
        let handle = self.stages[ix]
            .take()
            .expect("an Ended event arrives once, for a live external stage");
        let end = handle.file_external_end(outcome);
        self.file_end(ix, end)
    }

    /// A thread stage's own terminal event: file it as a
    /// [`StageEnd::Thread`], then the reader-gone cascade.
    fn on_returned(&mut self, ix: usize, obs: StageObservation) -> Vec<Effect> {
        let handle = self.stages[ix]
            .take()
            .expect("a Returned event arrives once, for a live thread stage");
        let end = handle.file_thread_end(obs);
        self.file_end(ix, end)
    }

    /// The common tail once a stage's [`StageEnd`] is known: file it, then
    /// the reader-gone cascade — identical in content to the old tail-first
    /// rescan, fired now by the event that justifies it — and whether every
    /// stage is now observed.  The cascade's own kill is recorded here, in
    /// `sent`, the instant `step` decides to emit it: attribution is the
    /// fold's, not the interpreter's.
    fn file_end(&mut self, ix: usize, end: StageEnd) -> Vec<Effect> {
        self.observed[ix] = Some(end);

        let mut effects = Vec::new();
        // A stage that already finished keeps its outcome — `!{ echo a;
        // exit 3 } | head -1` must stay honest — so the writer is only
        // killed while still running (`Some`).
        if ix > 0
            && let Some(writer) = self.stages[ix - 1].as_ref()
            && writer.feeds_pipe()
        {
            self.sent[ix - 1] = self.sent[ix - 1].max(Some(CancelCause::ReaderGone));
            effects.push(Effect::KillStage(ix - 1));
        }
        if self.stages.iter().all(Option::is_none) {
            effects.push(Effect::Done);
        }
        effects
    }

    /// Perform one [`Effect`]: a kill, or the whole group's cancel.  `Some`
    /// when it decides `drive`'s outcome; the caller returns at once rather
    /// than folding whatever else the same event's effect list still holds.
    fn run(&mut self, effect: Effect, group: &PipelineGroup) -> Option<()> {
        match effect {
            Effect::KillStage(ix) => {
                if let Some(handle) = self.stages[ix].as_mut() {
                    handle.kill_now();
                }
                None
            }
            Effect::CancelAll(cause) => {
                self.cancel_all(group, cause);
                Some(())
            }
            Effect::Done => Some(()),
        }
    }

    /// Fold events until every stage is observed —
    /// `loop { for e in step(&mut st, rx.recv()?) { run(e) } }`, no interval,
    /// no backoff, exact latency, zero idle CPU.
    pub(super) fn drive(&mut self, group: &PipelineGroup) {
        loop {
            let Some(ev) = self.recv() else {
                return;
            };
            for effect in step(self, ev) {
                if self.run(effect, group).is_some() {
                    return;
                }
            }
        }
    }

    /// Tear the whole pipeline down.
    ///
    /// Signal, bounded grace, kill, drain — in that order, for both group
    /// kinds.  The kill precedes every pump join: a stage's own kill reaches
    /// its pid alone, so a pumped descendant outlives it holding the pump's
    /// pipe, and no join on that pump returns while it does — which is why
    /// filing a stage's end here never joins, and `fold` does, after.  A
    /// collector dropped short of this is ended by `Drop`, kill first.
    ///
    /// A joining group has no pgid of its own to signal or kill: the signal
    /// is per pid, via `cancel_stages`, and the kill per live external, via
    /// [`Self::kill_live_externals`] — descendants of a killed external are
    /// the group owner's.
    ///
    /// The grace is a bounded blocking `recv_timeout` on the very deadline,
    /// not a probe loop; only a stage's own terminal event is filed during
    /// teardown — the reader-gone cascade and further cancellations are moot
    /// once the whole group is already dying, so every other event is simply
    /// discarded.  Idempotent: an event already mid-flight may cause this to
    /// run again (a second cancel racing the first), and every step here —
    /// re-cancelling, re-signalling, re-killing — is safe to repeat.
    pub(super) fn cancel_all(&mut self, group: &PipelineGroup, cause: CancelCause) {
        self.cancel_stages(cause);
        group.signal(cause); // a no-op for a joining group
        #[cfg(unix)]
        {
            let deadline = std::time::Instant::now() + crate::process::TEARDOWN_GRACE;
            while self.stages.iter().any(Option::is_some) {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let event = match self.rx.recv_timeout(remaining) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                };
                self.file_terminal_only(event);
            }
        }
        if group.owned() {
            group.kill();
        } else {
            self.kill_live_externals();
        }
        while self.stages.iter().any(Option::is_some) {
            let Some(event) = self.rx.recv().ok() else {
                break;
            };
            self.file_terminal_only(event);
        }
    }

    /// `cancel_all`'s own drain: file a stage's own terminal event and
    /// discard anything else — a further cancel, mid-teardown, is moot.
    fn file_terminal_only(&mut self, event: Event) {
        match event {
            Event::Ended(ix, outcome) => {
                let _ = self.on_ended(ix, outcome);
            }
            Event::Returned(ix, obs) => {
                let _ = self.on_returned(ix, obs);
            }
            Event::Cancelled(_) => {}
        }
    }

    /// Kill every still-live external's pid directly — a joining group's own
    /// half of teardown, with no pgid of its own to kill; descendants of a
    /// killed external are the group owner's.
    fn kill_live_externals(&self) {
        for handle in self.stages.iter().flatten() {
            handle.kill_by_pid();
        }
    }

    /// Cancel and wake every unobserved stage, recording the cause against
    /// each in `sent`.  Explicit per stage rather than through the mooring:
    /// a cancel the anchor witnessed has no cancelled ancestor scope to
    /// propagate from.
    pub(super) fn cancel_stages(&mut self, cause: CancelCause) {
        let Self { stages, sent, .. } = self;
        for (ix, handle) in stages.iter_mut().enumerate() {
            if let Some(handle) = handle {
                sent[ix] = sent[ix].max(Some(cause));
                handle.cancel(cause);
            }
        }
    }

    /// The audit observations and verdict fold, in launch order, over
    /// whatever this walk observed — the one place `&Shell` reaches an
    /// external's settlement, and the one place `sent` is read back against
    /// what happened: an external's pumps are joined, or detached when this
    /// collector's own reader-gone kill ended it, and its outcome folds
    /// through [`finish_external_settlement`]; a thread's own observation is
    /// [`StageObservation::forgiven`] under that same kill.  The pumps join,
    /// and the jail's cgroup comes down, here and not when the stage's end
    /// was filed: a join blocks on whatever still holds the pipe, and the
    /// jail's `rmdir` polls while descendants are still dying — both wait on
    /// `cancel_all`'s group kill, which precedes this.
    pub(super) fn fold(&mut self, mooring: &Mooring, shell: &mut Shell) -> PipelineCollector {
        let n = self.observed.len();
        let sent = std::mem::take(&mut self.sent);
        let mut collector = PipelineCollector::new();
        for (ix, end) in std::mem::take(&mut self.observed).into_iter().enumerate() {
            let Some(end) = end else { continue };
            let obs = match end {
                StageEnd::Thread(obs) if sent[ix] == Some(CancelCause::ReaderGone) => obs.forgiven(),
                StageEnd::Thread(obs) => obs,
                StageEnd::External {
                    name,
                    outcome,
                    jail,
                    pumps,
                } => {
                    #[cfg(target_os = "linux")]
                    if let Some(jail) = &jail {
                        jail.finish();
                    }
                    #[cfg(not(target_os = "linux"))]
                    let _ = jail;
                    pumps.settle(sent[ix] == Some(CancelCause::ReaderGone));
                    finish_external_settlement(&name, outcome, sent[ix], shell, self.started)
                }
            };
            collector.fold(mooring, shell, ix + 1 == n, obs);
        }
        collector
    }
}

/// `step` is pure over [`CollectState`]: it inspects the observation vector
/// and returns the [`Effect`]s that answer one [`Event`].  Filing an
/// observation happens here too — in-memory bookkeeping this collector
/// already owns, not the kill/cancel an `Effect` stands for.
pub(super) fn step(state: &mut CollectState, ev: Event) -> Vec<Effect> {
    match ev {
        Event::Ended(ix, outcome) => state.on_ended(ix, outcome),
        Event::Returned(ix, obs) => state.on_returned(ix, obs),
        Event::Cancelled(cause) => vec![Effect::CancelAll(cause)],
    }
}

impl Drop for CollectState {
    /// A collector dropped with a stage unobserved is a pipeline ending by
    /// force — an aborted launch, an unwind — so something dies: an owning
    /// collector kills its group; a joining collector has no pgid of its own
    /// and reaches its own externals by pid instead — the owner's group kill
    /// takes the rest.  Nothing joins here: the pumps of any end already
    /// filed drop detached, since a descendant a pid-addressed kill missed
    /// could hold their pipes open for ever.  Every stage observed is the
    /// ordinary end, which a `spawn` worker that joined the pgid outlives.
    fn drop(&mut self) {
        if self.stages.iter().any(Option::is_some) {
            match self.owned_group {
                Some(pgid) => pgid.kill(),
                None => self.kill_live_externals(),
            }
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
    use crate::process::WaitOutcome;
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

    /// `drive` over two real children reaches `Done` and folds what they
    /// settled.  The *final* stage carries the failure: a writer's own exit
    /// is not assertable here, because a reader that settles first kills it
    /// where it stands and that death is the one forgiven — precedence
    /// between two surviving verdicts is
    /// [`breaks_join_by_rank_earlier_stage_breaking_ties`]'s to state, over
    /// no processes at all.
    #[test]
    fn drive_folds_two_settling_stages_to_done() {
        let mut shell = Shell::default();
        let mut group = PipelineGroup::prepare(&shell).expect("anchor spawns");
        let mooring = Mooring::adrift();

        let mut collect = CollectState::new(&mut group, &mooring, std::time::Instant::now());
        collect.push(StageHandle::for_test(&collect, spawn_exiting(0)));
        collect.push(StageHandle::for_test(&collect, spawn_exiting(1)));

        collect.drive(&group);
        let folded = collect.fold(&mooring, &mut shell);
        match folded.break_ {
            Some(Break::Error(error)) => assert_ne!(error.exit_code(), 0),
            other => panic!("expected the final stage's exit to fold in, got {other:?}"),
        }
    }

    /// An external that exits with `code`, wrapped exactly as a direct
    /// pipeline stage.  `/bin/sh` because `true` and `false` are not in
    /// `/bin` on macOS.
    fn spawn_exiting(code: u8) -> crate::process::ChildHandle {
        let name = format!("exit {code}");
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", &name])
            .spawn()
            .unwrap_or_else(|e| panic!("spawn /bin/sh: {e}"));
        crate::process::ChildHandle::from_std(child)
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
    ) -> (crate::process::ChildHandle, i32) {
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
        (crate::process::ChildHandle::from_std(child), pid)
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
        let mut group = PipelineGroup::prepare(&shell).expect("anchor spawns");
        let (child, pid) = spawn_stage_with_grandchild(&group, true);

        let mooring = Mooring::adrift();
        let mut collect = CollectState::new(&mut group, &mooring, std::time::Instant::now());
        collect.push(StageHandle::for_test(&collect, child));
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
        let mut group = PipelineGroup::prepare(&shell).expect("anchor spawns");
        let mooring = Mooring::adrift();
        let (child, pid) = spawn_stage_with_grandchild(&group, false);

        let mut collect = CollectState::new(&mut group, &mooring, std::time::Instant::now());
        collect.push(StageHandle::for_test(&collect, child));
        collect.drive(&group);
        drop(collect);

        assert!(
            unsafe { libc::kill(pid, 0) } == 0,
            "the orphaned grandchild must survive the collector's ordinary end"
        );
        unsafe { libc::kill(pid, libc::SIGKILL) };
        drop(group);
    }

    /// A nested pipeline's own `timeout`-shaped teardown: no pgid of its own
    /// to signal or kill, so `cancel_all` must reach its direct external's
    /// pid itself, or it hangs in the final drain until that external exits
    /// on its own — the defect this collector's joining branch used to have.
    #[cfg(unix)]
    #[test]
    fn a_joining_collector_cancel_all_kills_its_externals_and_returns() {
        let mut shell = Shell::default();
        let owner = PipelineGroup::prepare(&shell).expect("anchor spawns");
        let pgid = owner.leader_pgid();
        let mut joining = PipelineGroup::joining(pgid);

        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let (child, _pgid) =
            crate::process::spawn_with_pgid(&mut cmd, crate::process::PgidPolicy::Join(pgid))
                .expect("spawn /bin/sh under the owner's pgid");

        let mooring = Mooring::adrift();
        let mut collect = CollectState::new(&mut joining, &mooring, std::time::Instant::now());
        collect.push(StageHandle::for_test(
            &collect,
            crate::process::ChildHandle::from_std(child),
        ));
        collect.all_stages_launched();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                collect.cancel_all(&joining, CancelCause::Deadline);
                let _ = done_tx.send(());
            });
            done_rx
                .recv_timeout(std::time::Duration::from_secs(3))
                .expect("cancel_all must not hang on a joining group's own external");
        });

        let folded = collect.fold(&mooring, &mut shell);
        assert!(
            matches!(folded.break_, Some(Break::Error(_))),
            "a killed stage must fold in an error, not a quiet ok"
        );
        drop(owner);
    }

    // ── step: the transition table ─────────────────────────────────────────
    //
    // No processes started for their own sake, no sleeps, no retry loops:
    // every corner case here is a sequence of `Event`s fed to `step`, and an
    // assertion on the `Effect`s it returns.

    fn state_with(n: usize) -> CollectState {
        let mut state = CollectState::for_step_test();
        for _ in 0..n {
            state.push(StageHandle::fake_external_for_step_test());
        }
        state
    }

    /// The reader-gone cascade: a settled reader kills its still-running
    /// writer, once — and an already-finished writer keeps its outcome,
    /// exactly the honesty `!{ echo a; exit 3 } | head -1` needs.
    #[test]
    fn settling_a_stage_kills_its_still_running_writer_but_spares_a_finished_one() {
        let mut state = state_with(4);
        // Stage 0 settles first — nothing upstream of it to cascade into.
        let effects = step(&mut state, Event::Ended(0, WaitOutcome::Exited(0)));
        assert!(
            !effects.contains(&Effect::Done),
            "one of four stages settling must not end the pipeline"
        );
        // Stage 1 settles: its writer (0) already settled, so it keeps its
        // outcome rather than being killed again.
        let effects = step(&mut state, Event::Ended(1, WaitOutcome::Exited(0)));
        assert!(
            !effects.contains(&Effect::KillStage(0)),
            "a writer that already settled must keep its outcome, not be killed again: {effects:?}"
        );
        // Stage 3 settles while its writer (2) is still running: killed —
        // and stage 2 has not itself settled yet, so the pipeline is not done.
        let effects = step(&mut state, Event::Ended(3, WaitOutcome::Exited(0)));
        assert!(
            effects.contains(&Effect::KillStage(2)),
            "a still-running writer whose reader just settled must be killed: {effects:?}"
        );
        assert!(
            !effects.contains(&Effect::Done),
            "a killed writer is not yet observed, so the pipeline is not done: {effects:?}"
        );
        // Stage 2, killed by the cascade above, now settles too: every stage
        // is observed, so the pipeline ends.
        let effects = step(&mut state, Event::Ended(2, WaitOutcome::Exited(0)));
        assert!(
            effects.contains(&Effect::Done),
            "the last stage settling must end the pipeline: {effects:?}"
        );
    }

    /// A cancel always tears down, whatever the cause or its source — the
    /// mooring's own scope, the anchor's report pipe, or the anchor's own
    /// death.
    #[test]
    fn a_cancelled_event_cancels_all() {
        let mut state = state_with(1);
        let effects = step(&mut state, Event::Cancelled(CancelCause::Deadline));
        assert_eq!(effects, vec![Effect::CancelAll(CancelCause::Deadline)]);
    }

    /// `Ended(ix, Signaled(KILL))` folds as the collector's own forgiven
    /// kill exactly when `sent[ix]` says this collector is the one that sent
    /// it, and as an ordinary failure when nothing here ever did.
    #[cfg(unix)]
    #[test]
    fn a_stage_kill_is_forgiven_by_sent_and_kept_without_it() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let kill = WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGKILL));

        let mut forgiven = state_with(1);
        forgiven.sent[0] = Some(CancelCause::ReaderGone);
        let _ = step(&mut forgiven, Event::Ended(0, kill));
        let folded = forgiven.fold(&mooring, &mut shell);
        assert!(
            folded.break_.is_none(),
            "a reader-gone kill must be forgiven, not folded in as a failure"
        );

        let mut kept = state_with(1);
        let _ = step(&mut kept, Event::Ended(0, kill));
        let folded = kept.fold(&mooring, &mut shell);
        assert!(
            matches!(folded.break_, Some(Break::Error(_))),
            "the very same death, unsent, must be kept as a real failure"
        );
    }

    // ── SettleOnDrop: the dead-man's switch ─────────────────────────────────

    /// Dropped mid-unwind, the guard settles its index itself: the message
    /// names the panic, not a silent disconnect.
    #[test]
    fn a_guard_dropped_while_panicking_settles_its_index() {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _guard = SettleOnDrop::new(0, tx);
            panic!("boom");
        });
        let event = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("the guard settles on unwind");
        match event {
            Event::Returned(0, obs) => match obs.break_ {
                Some(Break::Error(err)) => assert!(
                    err.message.contains("panicked"),
                    "expected the panic in the message: {}",
                    err.message
                ),
                other => panic!("expected an Error break, got {other:?}"),
            },
            _ => panic!("expected Returned(0, _)"),
        }
        let _ = handle.join();
    }

    /// A guard that already sent must not send again on drop: exactly one
    /// event reaches the channel.
    #[test]
    fn a_guard_that_sent_does_not_settle_twice() {
        let (tx, rx) = std::sync::mpsc::channel();
        let guard = SettleOnDrop::new(0, tx);
        guard.send(StageObservation::ok());
        assert!(matches!(rx.recv(), Ok(Event::Returned(0, _))));
        // The send consumed the guard's sender, so the channel is now closed
        // rather than merely empty: no second event can ever arrive.
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    /// A guard dropped with no panic in flight and no send made — a stage
    /// thread that returned without filing its own `Returned` — reports a
    /// silent end, distinct in message from a panic.
    #[test]
    fn a_guard_dropped_without_a_panic_reports_a_silent_end() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(SettleOnDrop::new(0, tx));
        match rx.recv().expect("the guard settles on drop") {
            Event::Returned(0, obs) => match obs.break_ {
                Some(Break::Error(err)) => assert!(
                    err.message.contains("without reporting"),
                    "expected the silent-end message: {}",
                    err.message
                ),
                other => panic!("expected an Error break, got {other:?}"),
            },
            _ => panic!("expected Returned(0, _)"),
        }
    }

    /// `drive` over a producer that unwinds holding its `SettleOnDrop` still
    /// reaches `Done` rather than blocking in `recv` forever, and the panic
    /// folds in as an `Error` — the whole point of the switch.
    #[test]
    fn drive_settles_a_stage_whose_producer_unwound() {
        let mut shell = Shell::default();
        let mut group = PipelineGroup::prepare(&shell).expect("anchor spawns");
        let mooring = Mooring::adrift();
        let mut collect = CollectState::new(&mut group, &mooring, std::time::Instant::now());

        let ix = collect.next_index();
        let tx = collect.sender();
        collect.push(StageHandle::fake_thread_for_step_test());
        collect.all_stages_launched();

        let handle = std::thread::spawn(move || {
            let _guard = SettleOnDrop::new(ix, tx);
            panic!("boom");
        });

        collect.drive(&group);
        let _ = handle.join();

        let folded = collect.fold(&mooring, &mut shell);
        match folded.break_ {
            Some(Break::Error(_)) => {}
            other => panic!("expected the panic to fold in as an Error, got {other:?}"),
        }
    }
}
