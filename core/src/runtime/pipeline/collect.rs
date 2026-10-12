//! Collect phase: every lifecycle edge arrives as one [`Event`] on one
//! channel.  [`step`] is pure over them — effects are returned, not performed
//! — and however the stages ended, the verdict folds in launch order.

use super::super::command;
use super::stage::StageHandle;
use crate::fact::Command;
use crate::process::{
    CancelCause, CancelWatch, Group, Pgid, TerminalLoan, WaitOutcome, Watch, watch_cancel,
};
use crate::types::{
    AuditFragment, AuditIo, Break, CommandOrigin, Error, Mooring, Observed, Settled, Shell, Value,
};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

/// A failed stage's error with the kernel's denial hint.  Best-effort:
/// siblings share the group, so the reader scopes deny lines to a descendant
/// sample taken at the failure, over the pipeline-wide window from `started`.
fn augment_stage_failure(err: Error, shell: &Shell, started: Instant) -> Error {
    let pids = || crate::process::sample_descendants(std::process::id());
    command::with_denials(err, shell, pids, started)
}

/// Built whether or not anyone is listening: this phase performs no effects,
/// and `fold` hands the fragment to the one emission door, which is where the
/// interest is judged.
fn synth_external_stage_audit(
    shell: &Shell,
    name: &str,
    args: Vec<String>,
    err: Option<&Error>,
) -> AuditFragment {
    let obs = shell.observation(Observed::Command(Command::new(
        name,
        args,
        err.map_or(0, Error::code),
        CommandOrigin::External,
        AuditIo::default(),
        err.map(|e| e.message.clone()),
    )));
    AuditFragment::from_observations(vec![obs])
}

/// The shell-touching finish the reaper's posting closure, holding no
/// `&Shell`, could not give an external's raw outcome.
fn finish_external_settlement(
    name: &str,
    args: Vec<String>,
    outcome: WaitOutcome,
    sent: Option<CancelCause>,
    shell: &Shell,
    started: Instant,
) -> StageObservation {
    let err = outcome
        .classify(sent)
        .map(|end| Error::of_child(name, end, shell));
    let audit = synth_external_stage_audit(shell, name, args, err.as_ref());
    let settled = match err {
        Some(err) => Err(Break::Error(augment_stage_failure(err, shell, started))),
        None => Ok(Value::Unit),
    };
    StageObservation { settled, audit }
}

/// One stage's terminal news, before the fold's shell-touching finish.
pub(super) enum StageEnd {
    Thread(StageObservation),
    External {
        name: String,
        args: Vec<String>,
        outcome: WaitOutcome,
        jail: Option<crate::process::jail::JailCgroup>,
        pumps: command::Pumps,
        sent: Option<CancelCause>,
    },
}

impl StageEnd {
    /// An external is settled under the cause sent to it or the key its
    /// group was `pressed`, whichever is stronger.
    fn settle(
        self,
        shell: &Shell,
        started: Instant,
        pressed: Option<CancelCause>,
    ) -> StageObservation {
        match self {
            // A `ReaderGone` break is minted only by a write to a dead edge or
            // by a `ReaderGone` scope cancel, both this collector's own doing,
            // so the break alone is the evidence.
            Self::Thread(obs) if obs.ended_by(CancelCause::ReaderGone) => obs.forgiven(),
            Self::Thread(obs) => obs,
            Self::External {
                name,
                args,
                outcome,
                jail,
                pumps,
                sent,
            } => {
                if let Some(jail) = &jail {
                    jail.finish();
                }
                if sent != Some(CancelCause::ReaderGone) {
                    pumps.join();
                }
                let cause = sent.max(pressed);
                finish_external_settlement(&name, args, outcome, cause, shell, started)
            }
        }
    }
}

/// One thing a teardown signals or kills: a whole process group, or a lone
/// external by pid.
enum Address<'a> {
    Group(Pgid),
    Pid(&'a Watch),
}

impl Address<'_> {
    /// `SIGCONT` after: a stopped member cannot act on the first until it runs.
    #[cfg(unix)]
    fn signal(&self, signal: crate::process::Signal) {
        match self {
            Self::Group(pgid) => {
                pgid.signal_group(signal);
                pgid.signal_group(crate::process::Signal::new(libc::SIGCONT));
            }
            Self::Pid(watch) => watch.signal(signal),
        }
    }

    /// Only a console group takes Ctrl-Break: a pid gets the kill alone.
    #[cfg(windows)]
    fn signal(&self, _: crate::process::Signal) {
        if let Self::Group(pgid) = self {
            crate::process::break_pipeline_group(*pgid);
        }
    }

    fn kill(&self) {
        match self {
            Self::Group(pgid) => pgid.kill(),
            Self::Pid(watch) => watch.kill(),
        }
    }
}

/// The collector's one wire type: every lifecycle edge, from whichever
/// producer sees it first.
pub(super) enum Event {
    /// An external's raw exit, from the reaper.
    Ended(usize, WaitOutcome),
    /// A thread stage's own report, or its [`SettleOnDrop`]'s.
    Returned(usize, StageObservation),
    /// The sentinel heard a write to this stage's dead outbound edge.
    Wrote(usize),
    /// A cause nothing in the group has been told about; teardown delivers it.
    Cancelled(CancelCause),
    /// A signal the anchor swallowed, the kernel having already delivered it to
    /// every member: a key only if the terminal was lent, and then teardown
    /// sends no second copy.  Unix alone — the Windows anchor has no
    /// group-wide delivery to hear.
    #[cfg(unix)]
    Heard(crate::process::Signal),
}

/// A stage's address on the collector's channel.
#[derive(Clone)]
pub(super) struct Slot {
    pub(super) ix: usize,
    pub(super) tx: Sender<Event>,
}

impl Slot {
    /// A closed channel means the collector is already gone.
    pub(super) fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Hand `child` to the reaper, whose posting closure files this stage's own
    /// [`Event::Ended`]: no dedicated waiter thread anywhere.
    pub(super) fn watch(&self, child: crate::process::ChildHandle) -> Watch {
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
        if let Some(slot) = self.0.take() {
            slot.send(Event::Returned(slot.ix, obs));
        }
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
                StageObservation::failure(Error::new(msg.to_string())),
            ));
        }
    }
}

/// What [`step`] asks [`CollectState::run`] to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// Mark this stage's outbound edge dead and set the sentinel on it.
    ArmEdge(usize),
    /// Cut this stage, whose write to a dead edge the sentinel heard.
    KillStage(usize),
    /// `delivered`: the kernel already gave every member this cause's signal.
    CancelAll { cause: CancelCause, delivered: bool },
}

/// One stage's observation, normalized across external children and ral stage
/// threads.
pub(super) struct StageObservation {
    pub(super) settled: Settled<Value>,
    pub(super) audit: AuditFragment,
}

impl StageObservation {
    #[cfg(test)]
    pub(super) fn ok() -> Self {
        Self {
            settled: Ok(Value::Unit),
            audit: AuditFragment::empty(),
        }
    }

    pub(super) fn failure(error: Error) -> Self {
        Self {
            settled: Err(Break::Error(error)),
            audit: AuditFragment::empty(),
        }
    }

    /// A killed stage's verdict is the collector's own doing and is dropped;
    /// what the stage observed still happened and is kept.
    pub(super) fn forgiven(self) -> Self {
        Self {
            settled: Ok(Value::Unit),
            audit: self.audit,
        }
    }

    /// Whether this stage's break is a `cause`-cancellation's own — the thread
    /// analogue of `WaitOutcome::classify`'s forgiveness.
    pub(super) fn ended_by(&self, cause: CancelCause) -> bool {
        matches!(&self.settled, Err(Break::Error(e)) if e.cancelled_by() == Some(cause))
    }
}

fn rank(br: &Break) -> u8 {
    match br {
        Break::Error(_) => 1,
        Break::Escape(_) => 2,
    }
}

/// An escape outranks an error; ties go to the earlier stage.
fn stronger(held: Break, new: Break) -> Break {
    if rank(&new) > rank(&held) { new } else { held }
}

/// Live collector state: every stage still running or already observed.
pub(super) struct CollectState {
    stages: Vec<Option<StageHandle>>,
    observed: Vec<Option<StageEnd>>,
    /// Window start for the sandbox-denial reader.
    started: Instant,
    /// An owned pgid is signalled and killed whole; a joined one is its
    /// owner's, so this collector addresses its own externals by pid.
    group: Group,
    /// The terminal lent to the group, if any: the one ear for a key, which
    /// returns it and strikes what it heard when this collector drops.
    loan: Option<TerminalLoan>,
    rx: Receiver<Event>,
    /// The mooring scope, posting [`Event::Cancelled`] the instant it is
    /// cancelled; dropping this disarms it.
    _cancel: CancelWatch,
}

impl CollectState {
    pub(super) fn new(
        rx: Receiver<Event>,
        tx: &Sender<Event>,
        group: Group,
        loan: Option<TerminalLoan>,
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
            started,
            group,
            loan,
            rx,
            _cancel: cancel,
        }
    }

    /// Whether the group holds the terminal — the loan actually made, never
    /// the plan it was launched under.  Settled before any stage exists.
    pub(super) fn holds_terminal(&self) -> bool {
        self.loan.is_some()
    }

    /// The key `signal` is, if the terminal was lent; inert otherwise.
    #[cfg(unix)]
    fn hear(&mut self, signal: crate::process::Signal) -> Option<CancelCause> {
        self.loan.as_mut().and_then(|loan| loan.hear(signal))
    }

    /// Hear every report still queued: once the anchor has ended, a key its
    /// stages all survived or died of before `drive` read it is here.
    #[cfg(unix)]
    fn hear_out(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            if let Event::Heard(signal) = ev {
                self.hear(signal);
            }
        }
    }

    fn pressed(&self) -> Option<CancelCause> {
        self.loan.as_ref().and_then(TerminalLoan::pressed)
    }

    /// A `CollectState` for `step`'s own transition-table tests — no group, no
    /// anchor, no processes — beside the sender its stages' slots address.
    #[cfg(test)]
    pub(super) fn for_step_test() -> (Self, Sender<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = watch_cancel(crate::process::CancelScope::root(), |_| {});
        let state = Self {
            stages: Vec::new(),
            observed: Vec::new(),
            started: Instant::now(),
            // Never `Owns`: a forced end would `kill(-pgid)` a pgid nobody owns.
            group: Group::Joins(crate::process::Membership::new(
                Pgid::from_raw(1).expect("1 is positive"),
                crate::process::CancelScope::root(),
            )),
            loan: None,
            rx,
            _cancel: cancel,
        };
        (state, tx)
    }

    pub(super) fn push(&mut self, handle: StageHandle) {
        self.stages.push(Some(handle));
        self.observed.push(None);
    }

    fn live(&self) -> bool {
        self.stages.iter().any(Option::is_some)
    }

    /// `None` back means every producer is gone.
    fn recv(&self, deadline: Option<Instant>) -> Option<Event> {
        match deadline {
            Some(deadline) => self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok(),
            None => self.rx.recv().ok(),
        }
    }

    /// File one stage's own terminal event, then arm the edge its reader has
    /// just abandoned.  Only a live writer is armed against: one that already
    /// finished keeps its outcome, so `!{ echo a; exit 3 } | head -1` stays
    /// honest.
    fn file(&mut self, ix: usize, end: impl FnOnce(StageHandle) -> StageEnd) -> Option<Effect> {
        let handle = self.stages[ix]
            .take()
            .expect("a stage's own end arrives once, for a live stage");
        self.observed[ix] = Some(end(handle));
        let writer = ix.checked_sub(1)?;
        self.stages[writer]
            .is_some()
            .then_some(Effect::ArmEdge(writer))
    }

    /// A stage already filed keeps its outcome, so the news is dropped.
    fn on_wrote(&self, ix: usize) -> Option<Effect> {
        self.stages[ix].is_some().then_some(Effect::KillStage(ix))
    }

    fn run(&mut self, effect: Effect) {
        match effect {
            Effect::ArmEdge(ix) => {
                if let Some(handle) = self.stages[ix].as_ref() {
                    handle.arm();
                }
            }
            Effect::KillStage(ix) => {
                if let Some(handle) = self.stages[ix].as_mut() {
                    handle.cut();
                }
            }
            Effect::CancelAll { cause, delivered } => self.cancel_all(cause, delivered),
        }
    }

    /// Fold events until every stage is observed.
    pub(super) fn drive(&mut self) {
        while self.live() {
            let Some(ev) = self.recv(None) else { return };
            if let Some(effect) = step(self, ev) {
                self.run(effect);
            }
        }
    }

    /// The same, discarding the effects: during teardown the group is already
    /// dying, so only the filing matters.
    fn drain(&mut self, deadline: Option<Instant>) {
        while self.live() {
            let Some(ev) = self.recv(deadline) else {
                return;
            };
            let _ = step(self, ev);
        }
    }

    /// Everything a teardown addresses: the pipeline's group when this
    /// collector owns it, else each external by pid — a thread stage has
    /// neither, its cancel and wake being all teardown owes it — and every
    /// envelope, whose payload leads a session of its own that no
    /// signal to the pipeline's group reaches.  `pipeline: false` leaves the
    /// pipeline's own out: the kernel, or a joined group's owner, delivers it.
    fn addresses(&self, pipeline: bool) -> impl Iterator<Item = Address<'_>> {
        let (group, pids) = match &self.group {
            Group::Owns(g) => (pipeline.then_some(Address::Group(*g)), false),
            Group::Joins(_) => (None, pipeline),
        };
        let pids = pids.then(|| {
            self.stages
                .iter()
                .flatten()
                .filter_map(StageHandle::watch)
                .map(Address::Pid)
        });
        let envelopes = self
            .stages
            .iter()
            .flatten()
            .filter_map(StageHandle::envelope)
            .map(Address::Group);
        group
            .into_iter()
            .chain(pids.into_iter().flatten())
            .chain(envelopes)
    }

    fn kill_live(&self) {
        for address in self.addresses(true) {
            address.kill();
        }
    }

    /// Cancel, grace-signal unless `delivered`, kill, drain.  Idempotent, a
    /// second cancel being free to race the first.  `delivered` speaks for
    /// the pipeline's group alone: the kernel's signal to the foreground
    /// group never reached an envelope's session.  A joining collector grace-
    /// signals only a cause its owner does not deliver.
    ///
    /// The kill precedes every pump join: a descendant that outlives its stage
    /// holds the pump's pipe, and no join on that pump returns while it does —
    /// so filing a stage's end never joins, and `fold` does, after.
    pub(super) fn cancel_all(&mut self, cause: CancelCause, delivered: bool) {
        // Explicit per stage rather than through the mooring: a cancel the
        // anchor heard has no cancelled ancestor scope to propagate from.
        for handle in self.stages.iter_mut().flatten() {
            handle.cancel(cause);
        }
        if let Some(signal) = crate::process::grace_signal(cause) {
            let pipeline = !delivered && self.group.owes(cause);
            for address in self.addresses(pipeline) {
                address.signal(signal);
            }
            self.drain(Some(Instant::now() + crate::process::TEARDOWN_GRACE));
        }
        self.kill_live();
        self.drain(None);
    }

    /// The audit and verdict fold, in launch order — the one place `&Shell`
    /// reaches an external's settlement, and why `last` is the final stage's
    /// value whenever that stage is `Ok` and unread otherwise.  The audit is
    /// merged before the verdict is ranked, so a failing stage still
    /// contributes what it observed.  The loan drops with `self`, returning
    /// the terminal and striking the frame whatever the verdict.
    pub(super) fn fold(mut self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        #[cfg(unix)]
        self.hear_out();
        let pressed = self.pressed();
        let observed = std::mem::take(&mut self.observed);
        let mut verdict: Option<Break> = None;
        let mut last = Value::Unit;
        for end in observed {
            // A stage nobody reported is a failure, not an absence.
            let end = end.unwrap_or_else(|| {
                StageEnd::Thread(StageObservation::failure(Error::new(
                    "ral pipeline stage vanished without reporting".to_string(),
                )))
            });
            // Thread stages already published their facts.
            let surface = matches!(&end, StageEnd::External { .. }).then_some(mooring);
            let StageObservation { settled, audit } = end.settle(shell, self.started, pressed);
            for observation in audit.into_observations() {
                shell.observe_stamped(surface, observation);
            }
            match settled {
                Err(br) => {
                    verdict = Some(match verdict {
                        Some(held) => stronger(held, br),
                        None => br,
                    });
                }
                Ok(v) => last = v,
            }
        }
        verdict.map_or(Ok(last), Err)
    }
}

/// Pure over [`CollectState`]: filing an observation happens here too, being
/// in-memory bookkeeping rather than the kill or cancel an [`Effect`] stands
/// for.
pub(super) fn step(state: &mut CollectState, ev: Event) -> Option<Effect> {
    match ev {
        Event::Ended(ix, outcome) => state.file(ix, |h| h.file_external_end(outcome)),
        Event::Returned(ix, obs) => state.file(ix, |h| h.file_thread_end(obs)),
        Event::Wrote(ix) => state.on_wrote(ix),
        Event::Cancelled(cause) => Some(Effect::CancelAll {
            cause,
            delivered: false,
        }),
        #[cfg(unix)]
        Event::Heard(signal) => state.hear(signal).map(|cause| Effect::CancelAll {
            cause,
            delivered: true,
        }),
    }
}

impl Drop for CollectState {
    /// A collector dropped with a stage unobserved is a forced end, so
    /// something dies.  Nothing joins here — a descendant the kill missed could
    /// hold a pump's pipe open for ever.  Every stage observed is the ordinary
    /// end, and a descendant a stage forked outlives it.
    fn drop(&mut self) {
        if self.live() {
            self.kill_live();
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests;
