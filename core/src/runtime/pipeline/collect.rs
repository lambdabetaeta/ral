//! Collect phase: every lifecycle edge arrives as one [`Event`] on one
//! channel.  [`step`] is pure over them — effects are returned, not performed,
//! bar the heard key's strike of the run's node — an abandoned edge is armed
//! from state before each wait, and however the stages ended, the verdict
//! folds in launch order.

use super::super::command;
use super::stage::StageHandle;
use crate::fact::Command;
use crate::process::{
    CancelCause, ChildEnd, Group, POLL_SLICE, Pgid, Scope, ScopeId, TerminalLoan, WaitOutcome,
    Watch,
};
use crate::types::{
    AuditFragment, AuditIo, Break, CommandOrigin, Error, Escape, Mooring, Observed, Settled, Shell,
    ThreadEnd, Value,
};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// A failed stage's error with the kernel's denial hint.  Best-effort:
/// siblings share the group, so the reader scopes deny lines to a descendant
/// sample taken at the failure, over the pipeline-wide window from `started`.
fn augment_stage_failure(err: Error, shell: &Shell, started: Instant) -> Error {
    let pids = || crate::process::sample_descendants(std::process::id());
    command::with_denials(err, shell, pids, started)
}

/// Built whether or not anyone is listening: this phase performs no effects,
/// and `fold` hands the fragment to the one emission point, which is where the
/// interest is judged.
fn synth_external_stage_audit(
    shell: &Shell,
    name: &str,
    args: Vec<String>,
    broke: Option<&Break>,
) -> AuditFragment {
    let obs = shell.observation(Observed::Command(Command::new(
        name,
        args,
        broke.map_or(0, Break::code),
        CommandOrigin::External,
        AuditIo::default(),
        broke.map(Break::to_string),
    )));
    AuditFragment::from_observations(vec![obs])
}

/// The shell-touching finish the reaper's posting closure, holding no
/// `&Shell`, could not give an external's raw outcome.  A death by what this
/// collector `sent` is the external's stop naming its stage, consumed as a
/// thread's is.
#[allow(
    clippy::too_many_arguments,
    reason = "the one shell-touching finish; each argument is a distinct fact of the settlement"
)]
fn finish_external_settlement(
    name: &str,
    args: Vec<String>,
    outcome: WaitOutcome,
    cause: Option<CancelCause>,
    sent: Option<CancelCause>,
    scope: &Scope,
    shell: &Shell,
    started: Instant,
) -> StageObservation {
    let broke = match outcome.classify(cause) {
        Some(ChildEnd::Cancelled(c)) if sent == Some(c) => None,
        end => end.map(|end| Break::of_child(name, end, scope, shell)),
    };
    let audit = synth_external_stage_audit(shell, name, args, broke.as_ref());
    let settled = match broke {
        Some(Break::Error(err)) => Err(Break::Error(augment_stage_failure(err, shell, started))),
        Some(stop) => Err(stop),
        None => Ok(Value::Unit),
    };
    StageObservation {
        end: ThreadEnd::Settled(settled),
        audit,
    }
}

/// One stage's terminal news, before the fold's shell-touching finish.
pub(super) enum StageEnd {
    Thread {
        obs: StageObservation,
        stage: ScopeId,
    },
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
    /// An external is settled under the cause sent to it or the one in force
    /// on the current `scope`, whichever is stronger; a key the tty delivered
    /// is attributed through the scope or not at all (ADR 261016_a-cleanup-is-shielded-within-its-grace).  A reader-gone
    /// stop this collector did not send attributes nothing.
    fn settle(
        self,
        shell: &Shell,
        started: Instant,
        scope: &Scope,
        group: &Group,
    ) -> StageObservation {
        match self {
            Self::Thread { obs, stage } if obs.names(&stage) => obs.forgiven(),
            Self::Thread { obs, .. } => obs,
            Self::External {
                name,
                args,
                outcome,
                jail,
                pumps,
                sent,
            } => {
                // Before the join: the finish kills a straggler holding a pump's pipe.
                drop(jail);
                if sent != Some(CancelCause::ReaderGone) {
                    pumps.join_under(scope, || {
                        if let Group::Owns(g) = group {
                            g.kill();
                        }
                    });
                }
                let cause = sent.max(scope.cause().filter(|c| *c != CancelCause::ReaderGone));
                finish_external_settlement(&name, args, outcome, cause, sent, scope, shell, started)
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
    /// The sentinel heard a write past what this stage's outbound edge owed
    /// its departed reader.
    Wrote(usize),
    /// A cause on the current scope stronger than the teardown so far; minted
    /// by [`CollectState::recv`] alone.
    Cancelled(CancelCause),
    /// The anchor ended: on its own, that ends the pipeline.  Unix alone: the
    /// Windows anchor has no watch.
    #[cfg(unix)]
    AnchorDied,
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
            slot.send(Event::Returned(
                slot.ix,
                StageObservation::failure(Error::new(
                    "ral pipeline stage ended without reporting".to_string(),
                )),
            ));
        }
    }
}

/// What [`step`] asks [`CollectState::run`] to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// Cut this stage, whose write to a dead edge the sentinel heard.
    KillStage(usize),
    CancelAll {
        cause: CancelCause,
    },
}

/// One stage's observation, normalized across external children and ral stage
/// threads.
pub(super) struct StageObservation {
    pub(super) end: ThreadEnd,
    pub(super) audit: AuditFragment,
}

impl StageObservation {
    #[cfg(test)]
    pub(super) fn ok() -> Self {
        Self {
            end: ThreadEnd::Settled(Ok(Value::Unit)),
            audit: AuditFragment::empty(),
        }
    }

    pub(super) fn failure(error: Error) -> Self {
        Self {
            end: ThreadEnd::Settled(Err(Break::Error(error))),
            audit: AuditFragment::empty(),
        }
    }

    /// A stop this collector owns: one naming `stage`, whatever its cause (a
    /// reader-gone stop, or the teardown this collector ran).
    fn names(&self, stage: &ScopeId) -> bool {
        matches!(
            &self.end,
            ThreadEnd::Settled(Err(Break::Escape(Escape::Stopped { owner: Some(o), .. }))) if o == stage
        )
    }

    /// The teardown an escape out of `stage` calls for: `Cancelled` for an
    /// `exit`; a stop's own cause unless it names `stage` (the collector's own
    /// doing: a reader-gone stop, or a teardown already running).
    fn tears_down_for(&self, stage: &ScopeId) -> Option<CancelCause> {
        match &self.end {
            ThreadEnd::Settled(Err(Break::Escape(Escape::Exit(_)))) => Some(CancelCause::Cancelled),
            ThreadEnd::Settled(Err(Break::Escape(Escape::Stopped { cause, .. })))
                if !self.names(stage) =>
            {
                Some(*cause)
            }
            _ => None,
        }
    }

    fn panicked(&self) -> bool {
        matches!(self.end, ThreadEnd::Panicked(_))
    }

    /// A stop this collector owns is dropped; what the stage observed still
    /// happened and is kept.
    pub(super) fn forgiven(self) -> Self {
        Self {
            end: ThreadEnd::Settled(Ok(Value::Unit)),
            audit: self.audit,
        }
    }
}

/// A stage's reader-gone stop is some collector's above to consume, so it
/// contributes nothing here (law 5); the closed-stdout stop is a stop like any
/// other.
fn rank(br: &Break) -> u8 {
    match br {
        Break::Escape(Escape::Stopped {
            cause: CancelCause::ReaderGone,
            owner: Some(_),
            ..
        }) => 0,
        Break::Error(_) => 1,
        Break::Escape(Escape::Exit(_)) => 2,
        Break::Escape(Escape::Stopped { cause, .. }) => 3 + *cause as u8,
    }
}

/// `Error 1 < Exit 2 < Stopped c (3 + c)`; two stops rank by cause; ties go to
/// the earlier stage.
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
    /// strikes it on the run's node as it hears.
    loan: Option<TerminalLoan>,
    rx: Receiver<Event>,
    /// The launching computation's current scope, read between slices.
    scope: Scope,
    cleanup_grace: Duration,
    /// The strongest cause this teardown has written on the stages; `None`
    /// before any teardown.  Only rises (`escalate`); the slice still
    /// synthesises `Cancelled` for a cause above it.
    teardown: Option<CancelCause>,
}

impl CollectState {
    pub(super) fn new(
        rx: Receiver<Event>,
        group: Group,
        loan: Option<TerminalLoan>,
        scope: Scope,
        cleanup_grace: Duration,
        started: Instant,
    ) -> Self {
        Self {
            stages: Vec::new(),
            observed: Vec::new(),
            started,
            group,
            loan,
            rx,
            scope,
            cleanup_grace,
            teardown: None,
        }
    }

    /// Whether the group holds the terminal — the loan actually made, never
    /// the plan it was launched under.  Settled before any stage exists.
    pub(super) fn holds_terminal(&self) -> bool {
        self.loan.is_some()
    }

    /// The key `signal` is, if the terminal was lent: struck on the node, and
    /// read by the scope at the next slice; inert otherwise.
    #[cfg(unix)]
    fn hear(&mut self, signal: crate::process::Signal) {
        if let Some(loan) = self.loan.as_mut() {
            loan.hear(signal);
        }
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

    /// Only to skip the duplicate grace signal; attributes no death.
    fn pressed(&self) -> Option<CancelCause> {
        self.loan.as_ref().and_then(TerminalLoan::pressed)
    }

    /// A `CollectState` for `step`'s own transition-table tests — no group, no
    /// anchor, no processes — beside the sender its stages' slots address.
    #[cfg(test)]
    pub(super) fn for_step_test() -> (Self, Sender<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let anchor = Scope::clone(&crate::process::DurableRoot::default().worker());
        let state = Self {
            stages: Vec::new(),
            observed: Vec::new(),
            started: Instant::now(),
            // Never `Owns`: a forced end would `kill(-pgid)` a pgid nobody owns.
            group: Group::Joins(crate::process::Membership::new(
                Pgid::from_raw(1).expect("1 is positive"),
                anchor.clone(),
            )),
            loan: None,
            rx,
            scope: anchor,
            cleanup_grace: Duration::ZERO,
            teardown: None,
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

    fn live_threads(&self) -> impl Iterator<Item = &super::thread::ThreadStage> {
        self.stages.iter().flatten().filter_map(StageHandle::thread)
    }

    fn threads_live(&self) -> bool {
        self.live_threads().next().is_some()
    }

    /// One event, or `None` at `deadline` or when every producer is gone.
    /// Every edge a reader has left is swept first.  The deadline and scope
    /// are read before each receive, which waits at most a slice; this alone
    /// mints `Event::Cancelled`, for a cause above `teardown`.
    fn recv(&self, deadline: Option<Instant>) -> Option<Event> {
        self.sweep_edges();
        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return None;
            }
            if let Some(cause) = self.scope.cause()
                && self.teardown.is_none_or(|t| cause > t)
            {
                return Some(Event::Cancelled(cause));
            }
            #[cfg(windows)]
            for t in self.live_threads() {
                if t.is_cancelled() {
                    t.interrupt();
                }
            }
            let slice = deadline.map_or(POLL_SLICE, |d| {
                d.saturating_duration_since(Instant::now()).min(POLL_SLICE)
            });
            match self.rx.recv_timeout(slice) {
                Ok(ev) => return Some(ev),
                Err(RecvTimeoutError::Disconnected) => return None,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// File one stage's own terminal event.
    fn file(&mut self, ix: usize, end: impl FnOnce(StageHandle) -> StageEnd) {
        let handle = self.stages[ix]
            .take()
            .expect("a stage's own end arrives once, for a live stage");
        self.observed[ix] = Some(end(handle));
    }

    /// Every live stage whose reader is gone: filed, or never pushed.  The
    /// last stage pushed is always one: the final stage (no edge, so a no-op)
    /// or, after a launch that failed part-way, a writer whose reader never
    /// came.  A writer already filed keeps its outcome, so
    /// `!{ echo a; exit 3 } | head -1` stays honest.
    fn abandoned(&self) -> impl Iterator<Item = usize> {
        let live = |ix: usize| matches!(self.stages.get(ix), Some(Some(_)));
        (0..self.stages.len()).filter(move |&ix| live(ix) && !live(ix + 1))
    }

    /// Set the sentinel on every abandoned edge, marking it dead unless a
    /// teardown, which cuts nothing at an edge, is under way.
    fn sweep_edges(&self) {
        let cut = self.teardown.is_none();
        for writer in self.abandoned().filter_map(|ix| self.stages[ix].as_ref()) {
            writer.listen(cut);
        }
    }

    /// A stage already filed keeps its outcome, so the news is dropped.
    fn on_wrote(&self, ix: usize) -> Option<Effect> {
        self.stages[ix].is_some().then_some(Effect::KillStage(ix))
    }

    fn run(&mut self, effect: Effect) {
        match effect {
            Effect::KillStage(ix) => {
                if let Some(handle) = self.stages[ix].as_mut() {
                    handle.cut();
                }
            }
            Effect::CancelAll { cause } => self.cancel_all(cause),
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

    /// Raise `teardown` to `cause` and write it on every live stage in one
    /// sweep; `false` if already covered.  Monotone; with `cut`, the one
    /// writer of a stage's node.
    fn escalate(&mut self, cause: CancelCause) -> bool {
        if self.teardown.is_some_and(|t| t >= cause) {
            return false;
        }
        self.teardown = Some(cause);
        for handle in self.stages.iter_mut().flatten() {
            handle.cancel(cause);
        }
        true
    }

    /// File events while `more`, escalating on a panic (to `Aborted`) or a
    /// stronger cause.  A bounded wait ends at once under `Aborted`; other
    /// effects are discarded.
    fn drain_while(&mut self, deadline: Option<Instant>, more: fn(&Self) -> bool) {
        let done = |s: &Self| deadline.is_some() && s.teardown == Some(CancelCause::Aborted);
        while more(self) && !done(self) {
            let Some(ev) = self.recv(deadline) else {
                return;
            };
            if let Some(Effect::CancelAll { cause }) = step(self, ev) {
                self.escalate(cause);
            }
        }
    }

    fn drain_until(&mut self, deadline: Instant) {
        self.drain_while(Some(deadline), Self::live);
    }

    fn drain_threads_until(&mut self, deadline: Instant) {
        self.drain_while(Some(deadline), Self::threads_live);
    }

    fn drain(&mut self) {
        self.drain_while(None, Self::live);
    }

    /// Everything a teardown addresses: the pipeline's group when this
    /// collector owns it, else each external by pid — a thread stage has
    /// neither, its cancel being all teardown owes it — and every
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

    /// Every live address, plus each live stage's jail for a member that
    /// `setsid()`'d out of the group.
    fn kill_live(&self) {
        for address in self.addresses(true) {
            address.kill();
        }
        for jail in self.stages.iter().flatten().filter_map(StageHandle::jail) {
            jail.kill();
        }
    }

    /// Tear down for `cause`: write it on every stage, grace-signal unless the
    /// terminal delivered it, wait out shielded thread stages up to the
    /// grace, then kill and drain.  `Aborted` cuts every wait short.
    ///
    /// The kill precedes every pump join: a descendant that outlives its stage
    /// holds the pump's pipe, and no join on that pump returns while it does —
    /// so filing a stage's end never joins, and `fold` does, after.
    pub(super) fn cancel_all(&mut self, cause: CancelCause) {
        if !self.escalate(cause) {
            return;
        }
        if let Some(signal) = crate::process::grace_signal(cause) {
            let pipeline = self.pressed() < Some(cause) && self.group.owes(cause);
            for address in self.addresses(pipeline) {
                address.signal(signal);
            }
            self.drain_until(Instant::now() + crate::process::TEARDOWN_GRACE);
        }
        // A live thread stage may be in a shielded cleanup whose children
        // share the group: it is waited for, up to the grace, before the
        // kill.  No shield masks `Aborted`, so that wait ends on its own.
        if self.teardown < Some(CancelCause::Aborted) {
            self.drain_threads_until(Instant::now() + self.cleanup_grace);
        }
        self.kill_live();
        self.drain();
    }

    /// End what a launch that failed part-way started: under the cause in
    /// force, as `recv` would mint it, else as an `exit` in a stage would.
    pub(super) fn roll_back(&mut self) {
        if self.live() {
            self.cancel_all(self.scope.cause().unwrap_or(CancelCause::Cancelled));
        }
    }

    /// The audit and verdict fold, in launch order — the one place `&Shell`
    /// reaches an external's settlement, and why `last` is the final stage's
    /// value whenever that stage is `Ok` and unread otherwise.  The audit is
    /// merged before the verdict is ranked, so a failing stage still
    /// contributes what it observed.
    pub(super) fn fold(mut self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        #[cfg(unix)]
        self.hear_out();
        let observed = std::mem::take(&mut self.observed);
        let mut verdict: Option<Break> = None;
        let mut panic: Option<String> = None;
        let mut last = Value::Unit;
        for end in observed {
            // Thread stages already published their facts.
            let surface = matches!(&end, Some(StageEnd::External { .. })).then_some(mooring);
            // A stage nobody reported is a failure, not an absence.
            let StageObservation { end, audit } = match end {
                Some(end) => end.settle(shell, self.started, &self.scope, &self.group),
                None => StageObservation::failure(Error::new(
                    "ral pipeline stage vanished without reporting".to_string(),
                )),
            };
            for observation in audit.into_observations() {
                shell.observe_stamped(surface, observation);
            }
            match end {
                ThreadEnd::Panicked(text) => {
                    panic.get_or_insert(text);
                }
                ThreadEnd::Settled(Err(br)) => {
                    verdict = Some(match verdict {
                        Some(held) => stronger(held, br),
                        None => br,
                    });
                }
                ThreadEnd::Settled(Ok(v)) => last = v,
            }
        }
        // On the run thread: the machine's `catch_unwind` abandons its frames
        // and the top restores its checkpoint.
        if let Some(text) = panic {
            std::panic::resume_unwind(Box::new(text));
        }
        if let Some(cause) = self.teardown {
            // The verdict only when no stage halted (law 5): the cause lives on
            // the stages' nodes, which die with them, so it displaces no halt.
            let owner = self
                .scope
                .in_force()
                .filter(|(c, _)| *c == cause)
                .map(|(_, id)| id);
            verdict = verdict.or_else(|| Some(Break::stopped(cause, owner)));
        }
        verdict.map_or(Ok(last), Err)
    }
}

/// Pure over [`CollectState`]: filing an observation happens here too, being
/// in-memory bookkeeping rather than the kill or cancel an [`Effect`] stands
/// for.
pub(super) fn step(state: &mut CollectState, ev: Event) -> Option<Effect> {
    match ev {
        Event::Ended(ix, outcome) => {
            state.file(ix, |h| h.file_external_end(outcome));
            None
        }
        Event::Returned(ix, obs) => {
            state.file(ix, |h| h.file_thread_end(obs));
            let Some(StageEnd::Thread { obs, stage }) = &state.observed[ix] else {
                unreachable!("a thread stage's end was just filed");
            };
            if obs.panicked() {
                Some(Effect::CancelAll {
                    cause: CancelCause::Aborted,
                })
            } else {
                obs.tears_down_for(stage)
                    .map(|cause| Effect::CancelAll { cause })
            }
        }
        Event::Wrote(ix) => state.on_wrote(ix),
        Event::Cancelled(cause) => Some(Effect::CancelAll { cause }),
        #[cfg(unix)]
        Event::AnchorDied => state.teardown.is_none().then_some(Effect::CancelAll {
            cause: CancelCause::Terminated,
        }),
        #[cfg(unix)]
        Event::Heard(signal) => {
            state.hear(signal);
            None
        }
    }
}

impl Drop for CollectState {
    /// A collector dropped with a stage unobserved is a forced end, so
    /// something dies.  `Terminated` is written on every live stage in one
    /// sweep before the kill, so the strikes are one instant and the joins in
    /// the stages' own drops share one grace.  Nothing joins here — a
    /// descendant the kill missed could hold a pump's pipe open for ever.
    fn drop(&mut self) {
        if self.live() {
            self.escalate(CancelCause::Terminated);
            self.kill_live();
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
