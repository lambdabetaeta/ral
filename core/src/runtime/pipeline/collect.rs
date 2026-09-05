//! Collect phase: fold a process-staged pipeline's per-stage observations
//! into one result.
//!
//! Every lifecycle edge — an external's exit from the reaper, a thread
//! stage's own report, a cancel, a signal the anchor witnessed — arrives as
//! one [`Event`] on one channel, and [`step`] is pure over them: effects are
//! returned, not performed.  Stages are observed in the order they end; the
//! verdict folds in launch order, the first failure winning and control
//! outranking failure.  A stage is cut at its first write to a dead edge and
//! nowhere else.

use super::super::command;
use super::group::PipelineGroup;
use super::launch::StageHandle;
use crate::evaluator::audit::observe_stamped;
use crate::process::{CancelCause, CancelWatch, CommandFailure, Pgid, watch_cancel};
use crate::types::{
    AuditFragment, AuditIo, Break, CommandOrigin, Error, Mooring, Observation, Observed, Settled,
    Shell, Value, epoch_us,
};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

/// Attach a kernel-denial diagnostic to a failed pipeline stage's error.
/// Best-effort: siblings share the group, so the reader scopes deny lines to a
/// descendant sample taken now, over the pipeline-wide window from `started`.
fn augment_stage_failure(err: Error, shell: &Shell, started: Instant) -> Error {
    if shell.sandbox_projection().is_none() {
        return err;
    }
    let pids = crate::sandbox::sample_descendants(std::process::id());
    crate::sandbox::augment_failure(err, shell, &pids, started)
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

/// The shell-touching finish an external stage's raw
/// [`crate::process::WaitOutcome`] still needs — attribution against `sent`,
/// audit, exit hints, denial augmentation — none of which the reaper's own
/// posting closure may do, holding no `&Shell`.
fn finish_external_settlement(
    name: &str,
    outcome: crate::process::WaitOutcome,
    sent: Option<CancelCause>,
    shell: &Shell,
    started: Instant,
) -> StageObservation {
    let err = CommandFailure::from_outcome(outcome, sent)
        .map(|f| Error::from_command_failure(name, f, shell));
    let audit = synth_external_stage_audit(shell, name, err.as_ref());
    match err {
        Some(err) => {
            StageObservation::failure(augment_stage_failure(err, shell, started)).with_audit(audit)
        }
        None => StageObservation::ok().with_audit(audit),
    }
}

/// One stage's terminal news, before the fold's shell-touching finish: a
/// thread stage's own [`StageObservation`] as-is; an external's raw outcome,
/// with the jail and the still-running pumps the fold settles by `sent`.
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
    /// A stage wrote to its dead outbound edge, heard by the sentinel; the
    /// read end travels back so the writer's own filing releases it.
    Wrote(usize, os_pipe::PipeReader),
    /// The mooring scope, or the anchor's own death — a cause nothing in the
    /// group has yet been told about, so teardown must deliver it.
    Cancelled(CancelCause),
    /// The anchor swallowed a signal the kernel had already delivered to
    /// every member: teardown sends no second copy of it.  Unix alone — the
    /// Windows anchor has no group-wide delivery to hear.
    #[cfg(unix)]
    Witnessed(CancelCause),
}

/// A stage's address on the collector's channel.
#[derive(Clone)]
pub(super) struct Slot {
    pub(super) ix: usize,
    pub(super) tx: Sender<Event>,
}

impl Slot {
    /// Post one event for this stage; a closed channel means the collector is
    /// already gone.
    pub(super) fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Hand `child` to the reaper, whose posting closure files this stage's
    /// own [`Event::Ended`] — no dedicated waiter thread anywhere.
    pub(super) fn watch(&self, child: crate::process::ChildHandle) -> crate::process::Watch {
        let ix = self.ix;
        child.into_watch(self.tx.clone(), move |o| Event::Ended(ix, o))
    }
}

/// A stage's dead-man's switch, owned by its own producer thread: disarmed by
/// [`Self::send`], dropped armed it posts the observation itself, so no index
/// can go silent whoever else still holds a sender.
pub(super) struct SettleOnDrop(Option<Slot>);

impl SettleOnDrop {
    pub(super) fn new(slot: Slot) -> Self {
        Self(Some(slot))
    }

    pub(super) fn send(mut self, obs: StageObservation) {
        let slot = self.0.take().expect("armed until send or drop");
        slot.send(Event::Returned(slot.ix, obs));
    }
}

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        if let Some(slot) = self.0.take() {
            let msg = if std::thread::panicking() {
                "ral pipeline stage panicked"
            } else {
                "ral pipeline stage ended without reporting"
            };
            slot.send(Event::Returned(
                slot.ix,
                StageObservation::failure(Error::new(msg.to_string(), 1)),
            ));
        }
    }
}

/// [`step`]'s pure output: what the interpreter ([`CollectState::run`]) must
/// perform for one event's worth of consequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// This stage's outbound edge is dead: mark it, and set the sentinel
    /// listening on the parent's duplicate of its read end.
    ArmEdge(usize),
    /// The reader-gone kill, the sentinel having heard the first write to a
    /// dead edge: the one death forgiven.
    KillStage(usize),
    /// Tear the whole pipeline down.  `delivered` says the kernel already
    /// gave every member the signal this cause names, so teardown must not
    /// send a second copy.
    CancelAll { cause: CancelCause, delivered: bool },
    Done,
}

/// One stage's observation, normalized across external children and ral stage
/// threads.  `final_value` is set only by the final value-typed ral stage.
/// The break's own two constructors are the classification the fold needs:
/// only an `Escape` is control flow.
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

    /// Whether this stage's break is a `cause`-cancellation's own — the thread
    /// analogue of `WaitOutcome::is_stage_kill`.
    pub(super) fn ended_by(&self, cause: CancelCause) -> bool {
        matches!(&self.break_, Some(Break::Error(e)) if e.cancelled_by() == Some(cause))
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

    /// Ties go to the earlier stage, the fold being launch-ordered.
    fn note(&mut self, br: Break) {
        if self.break_.as_ref().map_or(0, rank) < rank(&br) {
            self.break_ = Some(br);
        }
    }

    /// The audit observations are broadcast before the break is ranked, so a
    /// stage that fails or escapes still contributes what it observed.
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
    /// What this collector itself sent stage `ix`, joined by `max`: it is the
    /// one party that knows what it did to whom.
    sent: Vec<Option<CancelCause>>,
    started: Instant,
    /// The pgid a forced end of this collector kills; `None` for a joining
    /// collector, whose owner's teardown does it.
    owned_group: Option<Pgid>,
    /// Every producer's [`Event`] arrives here: the reaper, a stage thread's
    /// own report, the anchor's witness, the cancel watch.
    rx: Receiver<Event>,
    /// This collector's own clone, held only while stages are still being
    /// launched: [`Self::all_stages_launched`] drops it, so the channel can
    /// close once every producer is gone.
    tx: Option<Sender<Event>>,
    /// The mooring scope, posting [`Event::Cancelled`] the instant it is
    /// cancelled; dropping this disarms it.
    _cancel: CancelWatch,
}

impl CollectState {
    pub(super) fn new(
        rx: Receiver<Event>,
        tx: Sender<Event>,
        group: &PipelineGroup,
        mooring: &Mooring,
        started: Instant,
    ) -> Self {
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
    /// anchor, no processes, and a root scope nothing ever cancels.
    #[cfg(test)]
    pub(super) fn for_step_test() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = watch_cancel(crate::process::CancelScope::root(), |_| {});
        Self {
            stages: Vec::new(),
            observed: Vec::new(),
            sent: Vec::new(),
            started: Instant::now(),
            owned_group: None,
            rx,
            tx: Some(tx),
            _cancel: cancel,
        }
    }

    /// The channel address of the stage about to be launched — the index it
    /// will occupy once pushed, and a sender clone for its own producers.
    pub(super) fn slot(&self) -> Slot {
        Slot {
            ix: self.stages.len(),
            tx: self
                .tx
                .as_ref()
                .expect("a stage is still launching, so the collector's own sender is alive")
                .clone(),
        }
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

    fn live(&self) -> bool {
        self.stages.iter().any(Option::is_some)
    }

    /// Block for the next event; `None` once every producer is gone, which
    /// leaves nothing further to collect and so reads as `Done`.
    fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    /// File one stage's own terminal event: take its handle out, let `end`
    /// turn it into a [`StageEnd`], then arm the edge its reader has just
    /// abandoned and say whether every stage is now observed.
    fn file(&mut self, ix: usize, end: impl FnOnce(StageHandle) -> StageEnd) -> Vec<Effect> {
        let handle = self.stages[ix]
            .take()
            .expect("a stage's own end arrives once, for a live stage");
        self.observed[ix] = Some(end(handle));

        let mut effects = Vec::new();
        // A stage that already finished keeps its outcome — `!{ echo a;
        // exit 3 } | head -1` must stay honest — so only a writer still
        // running (`Some`) has an edge left to arm.
        if ix > 0 && self.stages[ix - 1].is_some() {
            effects.push(Effect::ArmEdge(ix - 1));
        }
        if !self.live() {
            effects.push(Effect::Done);
        }
        effects
    }

    /// The sentinel heard this stage write to its dead edge: take the read
    /// end back, record the cause against the stage, and cut it.  A stage
    /// that has already been filed keeps its outcome, and the returned reader
    /// simply drops.
    fn on_wrote(&mut self, ix: usize, reader: os_pipe::PipeReader) -> Vec<Effect> {
        let Some(handle) = self.stages[ix].as_mut() else {
            return Vec::new();
        };
        handle.regain(reader);
        self.sent[ix] = self.sent[ix].max(Some(CancelCause::ReaderGone));
        vec![Effect::KillStage(ix)]
    }

    /// Perform one [`Effect`]: arming an edge, a kill, or the whole group's
    /// cancel.  `Some` when it decides `drive`'s outcome; the caller returns
    /// at once rather than folding whatever else the same event's effect list
    /// still holds.
    fn run(&mut self, effect: Effect, group: &PipelineGroup) -> Option<()> {
        match effect {
            Effect::ArmEdge(ix) => {
                if let Some(handle) = self.stages[ix].as_mut() {
                    handle.arm();
                }
                None
            }
            Effect::KillStage(ix) => {
                if let Some(handle) = self.stages[ix].as_mut() {
                    handle.cut();
                }
                None
            }
            Effect::CancelAll { cause, delivered } => {
                self.cancel_all(group, cause, delivered);
                Some(())
            }
            Effect::Done => Some(()),
        }
    }

    /// Fold events until every stage is observed: a blocking `recv` per
    /// event, no interval and no backoff.
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

    /// Fold events until every stage is observed, discarding the effects:
    /// during teardown the group is already dying, so only the filing
    /// matters.  A bounded blocking `recv_timeout` on the very `deadline`,
    /// not a probe loop; `None` waits without one.
    fn drain(&mut self, deadline: Option<Instant>) {
        while self.live() {
            let received = match deadline {
                Some(deadline) => self
                    .rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .ok(),
                None => self.rx.recv().ok(),
            };
            let Some(event) = received else { return };
            let _ = step(self, event);
        }
    }

    /// Every still-live external's wait handle; a thread stage has none, its
    /// own cancel and wake being all teardown owes it.
    fn live_externals(&self) -> impl Iterator<Item = &crate::process::Watch> {
        self.stages.iter().flatten().filter_map(StageHandle::watch)
    }

    fn kill_live_externals(&self) {
        for watch in self.live_externals() {
            watch.kill();
        }
    }

    /// Tear the whole pipeline down: cancel every live stage, open with the
    /// cause's grace signal unless `delivered` says the kernel already sent
    /// it, wait out the grace, kill, drain.  Idempotent, a second cancel
    /// being free to race the first.  A joining group has no pgid of its own,
    /// so both the signal and the kill reach its externals by pid.
    ///
    /// The kill precedes every pump join: a descendant that outlives its
    /// stage holds the pump's pipe, and no join on that pump returns while it
    /// does — so filing a stage's end never joins, and `fold` does, after.
    pub(super) fn cancel_all(&mut self, group: &PipelineGroup, cause: CancelCause, delivered: bool) {
        // Explicit per stage rather than through the mooring: a cancel the
        // anchor witnessed has no cancelled ancestor scope to propagate from.
        let Self { stages, sent, .. } = self;
        for (ix, handle) in stages.iter_mut().enumerate() {
            if let Some(handle) = handle {
                sent[ix] = sent[ix].max(Some(cause));
                handle.cancel(cause);
            }
        }
        #[cfg(unix)]
        if let Some(signal) = crate::process::grace_signal(cause) {
            if !delivered {
                if group.owned() {
                    group.signal(signal);
                } else {
                    for watch in self.live_externals() {
                        watch.signal(signal);
                    }
                }
            }
            self.drain(Some(Instant::now() + crate::process::TEARDOWN_GRACE));
        }
        #[cfg(not(unix))]
        let _ = delivered;
        if group.owned() {
            group.kill();
        } else {
            self.kill_live_externals();
        }
        self.drain(None);
    }

    /// The audit and verdict fold, in launch order — the one place `&Shell`
    /// reaches an external's settlement, and the one place `sent` is read
    /// back against what happened: a stage this collector's own reader-gone
    /// kill ended is forgiven, and its pumps detached rather than joined.
    ///
    /// The pumps join and the jail comes down here rather than at filing:
    /// both wait on `cancel_all`'s group kill, which precedes this.
    pub(super) fn fold(&mut self, mooring: &Mooring, shell: &mut Shell) -> PipelineCollector {
        let n = self.observed.len();
        let sent = std::mem::take(&mut self.sent);
        let mut collector = PipelineCollector::new();
        for (ix, end) in std::mem::take(&mut self.observed).into_iter().enumerate() {
            let Some(end) = end else { continue };
            let obs = match end {
                // A `ReaderGone` break is minted only by a write to a dead
                // edge or by a `ReaderGone` scope cancel, both this
                // collector's own doing, so the break alone is the evidence.
                StageEnd::Thread(obs) if obs.ended_by(CancelCause::ReaderGone) => obs.forgiven(),
                StageEnd::Thread(obs) => obs,
                StageEnd::External {
                    name,
                    outcome,
                    jail,
                    pumps,
                } => {
                    if let Some(jail) = &jail {
                        jail.finish();
                    }
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
        Event::Ended(ix, outcome) => state.file(ix, |h| h.file_external_end(outcome)),
        Event::Returned(ix, obs) => state.file(ix, |h| h.file_thread_end(obs)),
        Event::Wrote(ix, reader) => state.on_wrote(ix, reader),
        Event::Cancelled(cause) => vec![Effect::CancelAll {
            cause,
            delivered: false,
        }],
        #[cfg(unix)]
        Event::Witnessed(cause) => vec![Effect::CancelAll {
            cause,
            delivered: true,
        }],
    }
}

impl Drop for CollectState {
    /// A collector dropped with a stage unobserved is a forced end, so
    /// something dies: an owning collector kills its group, a joining one its
    /// own externals by pid.  Nothing joins here — a descendant the kill
    /// missed could hold a pump's pipe open for ever.  Every stage observed
    /// is the ordinary end, which a `spawn` worker that joined the pgid
    /// outlives.
    fn drop(&mut self) {
        if self.live() {
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

    /// The four laws of the join: an escape outranks an error, and within a
    /// rank the earlier stage wins.
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

    /// An owning group and a collector wired onto the same channel, as
    /// `PipeNode::launch` builds them.
    fn owning_pipeline(shell: &Shell, mooring: &Mooring) -> (PipelineGroup, CollectState) {
        let (tx, rx) = std::sync::mpsc::channel();
        let group = PipelineGroup::prepare(shell, tx.clone()).expect("anchor spawns");
        let collect = CollectState::new(rx, tx, &group, mooring, Instant::now());
        (group, collect)
    }

    /// `drive` over two real children reaches `Done` and folds what they
    /// settled.
    #[test]
    fn drive_folds_two_settling_stages_to_done() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let (group, mut collect) = owning_pipeline(&shell, &mooring);

        for code in [0, 1] {
            let slot = collect.slot();
            collect.push(StageHandle::for_test(slot, spawn_exiting(code)));
        }

        collect.drive(&group);
        let folded = collect.fold(&mooring, &mut shell);
        match folded.break_ {
            Some(Break::Error(error)) => assert_ne!(error.exit_code(), 0),
            other => panic!("expected the final stage's exit to fold in, got {other:?}"),
        }
    }

    /// An external that exits with `code`.  `/bin/sh` because `true` and
    /// `false` are not in `/bin` on macOS.
    fn spawn_exiting(code: u8) -> crate::process::ChildHandle {
        let name = format!("exit {code}");
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", &name])
            .spawn()
            .unwrap_or_else(|e| panic!("spawn /bin/sh: {e}"));
        crate::process::ChildHandle::from_std(child)
    }

    /// A stage in the group's pgid over a background `sleep`, whose pid comes
    /// back on the stage's stdout.  `wait_on_child` says whether the stage
    /// outlives the grandchild or orphans it into the group.
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

    /// A grandchild no per-pid kill reaches dies of the group kill a forced
    /// end fires.
    #[cfg(unix)]
    #[test]
    fn a_collector_dropped_short_of_observation_kills_the_group() {
        let shell = Shell::default();
        let mooring = Mooring::adrift();
        let (group, mut collect) = owning_pipeline(&shell, &mooring);
        let (child, pid) = spawn_stage_with_grandchild(&group, true);

        let slot = collect.slot();
        collect.push(StageHandle::for_test(slot, child));
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
        let mooring = Mooring::adrift();
        let (group, mut collect) = owning_pipeline(&shell, &mooring);
        let (child, pid) = spawn_stage_with_grandchild(&group, false);

        let slot = collect.slot();
        collect.push(StageHandle::for_test(slot, child));
        collect.drive(&group);
        drop(collect);

        assert!(
            unsafe { libc::kill(pid, 0) } == 0,
            "the orphaned grandchild must survive the collector's ordinary end"
        );
        unsafe { libc::kill(pid, libc::SIGKILL) };
        drop(group);
    }

    /// A joining collector has no pgid to kill, so `cancel_all` must reach
    /// its external's pid itself rather than hang in the final drain.
    #[cfg(unix)]
    #[test]
    fn a_joining_collector_cancel_all_kills_its_externals_and_returns() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let owner = PipelineGroup::prepare(&shell, std::sync::mpsc::channel().0)
            .expect("anchor spawns");
        let pgid = owner.leader_pgid();
        let joining = PipelineGroup::joining(pgid);

        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let (child, _pgid) =
            crate::process::spawn_with_pgid(&mut cmd, crate::process::PgidPolicy::Join(pgid))
                .expect("spawn /bin/sh under the owner's pgid");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut collect = CollectState::new(rx, tx, &joining, &mooring, Instant::now());
        let slot = collect.slot();
        collect.push(StageHandle::for_test(
            slot,
            crate::process::ChildHandle::from_std(child),
        ));
        collect.all_stages_launched();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                collect.cancel_all(&joining, CancelCause::Deadline, false);
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
    // Every case is a sequence of `Event`s fed to `step` and an assertion on
    // the `Effect`s it returns: no sleeps, no retry loops.

    fn state_with(n: usize) -> CollectState {
        let mut state = CollectState::for_step_test();
        for _ in 0..n {
            let slot = state.slot();
            state.push(StageHandle::fake_external_for_step_test(slot));
        }
        state
    }

    fn push_fake_thread(state: &mut CollectState) {
        let slot = state.slot();
        state.push(StageHandle::fake_thread_for_step_test(slot));
    }

    /// A settled reader arms the edge its still-running writer holds — and an
    /// already-finished writer has none left to arm, exactly the honesty
    /// `!{ echo a; exit 3 } | head -1` needs.
    #[test]
    fn settling_a_stage_arms_its_still_running_writers_edge_but_spares_a_finished_one() {
        let mut state = state_with(4);
        // Stage 0 settles first — nothing upstream of it to arm.
        let effects = step(&mut state, Event::Ended(0, WaitOutcome::Exited(0)));
        assert!(
            !effects.contains(&Effect::Done),
            "one of four stages settling must not end the pipeline"
        );
        // Stage 1 settles: its writer (0) already settled, so it keeps its
        // outcome and nothing is armed against it.
        let effects = step(&mut state, Event::Ended(1, WaitOutcome::Exited(0)));
        assert!(
            !effects.contains(&Effect::ArmEdge(0)),
            "a writer that already settled must keep its outcome, not be armed against: {effects:?}"
        );
        // Stage 3 settles while its writer (2) is still running: that edge is
        // armed — and stage 2 has not settled, so the pipeline is not done.
        let effects = step(&mut state, Event::Ended(3, WaitOutcome::Exited(0)));
        assert!(
            effects.contains(&Effect::ArmEdge(2)),
            "a still-running writer whose reader just settled must have its edge armed: {effects:?}"
        );
        assert!(
            !effects.contains(&Effect::Done),
            "an unobserved writer means the pipeline is not done: {effects:?}"
        );
        // Stage 2 now settles too: every stage is observed, so the pipeline
        // ends.
        let effects = step(&mut state, Event::Ended(2, WaitOutcome::Exited(0)));
        assert!(
            effects.contains(&Effect::Done),
            "the last stage settling must end the pipeline: {effects:?}"
        );
    }

    /// The sentinel's news is what cuts a stage: `Wrote` records the cause
    /// against it and emits the one kill.
    #[test]
    fn a_write_to_a_dead_edge_kills_its_writer() {
        let mut state = state_with(2);
        let (reader, _writer) = crate::process::cloexec_pipe().expect("pipe");
        let effects = step(&mut state, Event::Wrote(0, reader));
        assert_eq!(effects, vec![Effect::KillStage(0)]);
        assert_eq!(state.sent[0], Some(CancelCause::ReaderGone));
    }

    /// A stage that ended on its own account before the sentinel was heard
    /// keeps its outcome: the news is dropped, and the reader with it.
    #[test]
    fn a_write_heard_after_its_writer_was_filed_is_ignored() {
        let mut state = state_with(2);
        let _ = step(&mut state, Event::Ended(0, WaitOutcome::Exited(3)));
        let (reader, _writer) = crate::process::cloexec_pipe().expect("pipe");
        let effects = step(&mut state, Event::Wrote(0, reader));
        assert!(
            effects.is_empty(),
            "a filed stage must not be cut: {effects:?}"
        );
        assert_eq!(state.sent[0], None);
    }

    /// A cancel always tears down, whatever the cause or its source — the
    /// mooring's own scope or the anchor's own death — and teardown must
    /// deliver it, nothing else having.
    #[test]
    fn a_cancelled_event_cancels_all() {
        let mut state = state_with(1);
        let effects = step(&mut state, Event::Cancelled(CancelCause::Deadline));
        assert_eq!(
            effects,
            vec![Effect::CancelAll {
                cause: CancelCause::Deadline,
                delivered: false
            }]
        );
    }

    /// A signal the anchor witnessed reached every member of the group by the
    /// kernel's own hand, so teardown must not send a second copy of it.
    #[cfg(unix)]
    #[test]
    fn a_witnessed_signal_tears_down_without_resending_it() {
        let mut state = state_with(1);
        let effects = step(&mut state, Event::Witnessed(CancelCause::Interrupt));
        assert_eq!(
            effects,
            vec![Effect::CancelAll {
                cause: CancelCause::Interrupt,
                delivered: true
            }]
        );
    }

    /// A stage this collector tore down names the cause it sent, not the
    /// signal number that carried it: the attribution reads `sent` back
    /// against the wait status, wherever the teardown ran.
    #[cfg(unix)]
    #[test]
    fn a_torn_down_externals_death_names_the_cause_it_was_sent() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let mut state = state_with(1);
        state.sent[0] = Some(CancelCause::Explicit);
        let _ = step(
            &mut state,
            Event::Ended(
                0,
                WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGTERM)),
            ),
        );

        let folded = state.fold(&mooring, &mut shell);
        match folded.break_ {
            Some(Break::Error(e)) => assert!(
                e.message.contains("stopped because the call was cancelled"),
                "expected the cause, not the signal number: {}",
                e.message
            ),
            other => panic!("expected the teardown death to fold in as an error, got {other:?}"),
        }
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

    /// The reader's `Returned` reaches the collector before the writer's own
    /// honest exit does, so its edge is armed — but the writer never wrote
    /// again, and its break is its own `exit 3`, which `fold` must keep.
    #[test]
    fn a_thread_writers_honest_exit_survives_its_readers_earlier_end() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let mut state = CollectState::for_step_test();
        push_fake_thread(&mut state);
        push_fake_thread(&mut state);

        let effects = step(&mut state, Event::Returned(1, StageObservation::ok()));
        assert!(
            effects.contains(&Effect::ArmEdge(0)),
            "the reader settling first must arm its writer's edge: {effects:?}"
        );

        let _ = step(
            &mut state,
            Event::Returned(0, StageObservation::failure(Error::new("boom", 3))),
        );

        let folded = state.fold(&mooring, &mut shell);
        match folded.break_ {
            Some(Break::Error(e)) => assert_eq!(e.exit_code(), 3),
            other => panic!("expected the writer's own exit 3 to survive, got {other:?}"),
        }
    }

    /// A `ReaderGone` break — as a dead edge's sink or a `ReaderGone` scope
    /// cancel mints it — is forgiven on its own evidence, with nothing
    /// recorded in `sent`.
    #[test]
    fn a_thread_writer_the_cancel_ended_is_forgiven() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let mut state = CollectState::for_step_test();
        push_fake_thread(&mut state);

        let _ = step(
            &mut state,
            Event::Returned(
                0,
                StageObservation::failure(Error::cancelled(CancelCause::ReaderGone)),
            ),
        );
        assert_eq!(state.sent[0], None, "nothing was sent to this stage");

        let folded = state.fold(&mooring, &mut shell);
        assert!(
            folded.break_.is_none(),
            "a writer the reader-gone cancel actually ended must be forgiven: {:?}",
            folded.break_
        );
    }

    // ── SettleOnDrop: the dead-man's switch ─────────────────────────────────

    /// Dropped mid-unwind, the guard settles its index itself: the message
    /// names the panic, not a silent disconnect.
    #[test]
    fn a_guard_dropped_while_panicking_settles_its_index() {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _guard = SettleOnDrop::new(Slot { ix: 0, tx });
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
        let guard = SettleOnDrop::new(Slot { ix: 0, tx });
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
        drop(SettleOnDrop::new(Slot { ix: 0, tx }));
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
        let mooring = Mooring::adrift();
        let (group, mut collect) = owning_pipeline(&shell, &mooring);

        let slot = collect.slot();
        push_fake_thread(&mut collect);
        collect.all_stages_launched();

        let handle = std::thread::spawn(move || {
            let _guard = SettleOnDrop::new(slot);
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
