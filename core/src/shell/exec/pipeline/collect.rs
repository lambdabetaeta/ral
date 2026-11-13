//! Collect phase: every lifecycle edge arrives as one [`Event`] on one
//! channel.  [`step`] is pure over them — effects are returned, not performed,
//! bar the heard key's strike of the run's node — an abandoned edge is armed
//! from state before each wait, and however the stages ended, the verdict
//! folds in launch order.  A lone external is collected as the one stage of
//! its group, and settled as a command ends ([`CollectState::settle_lone`]).

use super::super::command::{self, Started};
use super::stage::{Drain, ExternalEnd, LiveExternal, StageFrame, StageHandle, StageKind};
use crate::fact::AuditIo;
use crate::ir::ValClos;
use crate::process::{
    CancelCause, Group, POLL_SLICE, Pgid, Scope, ScopeId, Spawned, TerminalHold, WaitOutcome, Watch,
};
use crate::shell::runtime::{AuditFragment, Break, End, Mooring, Settled, Shell, Stop, UNREPORTED};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// What a settlement reads of its collector: the window start for the
/// sandbox-denial reader, the scope, and the group it owns, if any.
#[derive(Clone, Copy)]
struct Window<'a> {
    started: Instant,
    scope: &'a Scope,
    owned: Option<Pgid>,
}

/// What an external's ending reads once its process and pumps are done.
pub(super) struct Settlement {
    pub(super) name: String,
    /// Absent for a lone command, whose own frame is still open to claim
    /// and record when it settles.
    pub(super) frame: Option<Box<StageFrame>>,
    /// The PIDs whose sandbox denials belong to it, taken at its start.
    pub(super) denial_pids: Option<HashSet<u32>>,
}

impl Settlement {
    /// How the run ended under `cause`; siblings share the window from
    /// `started`, so only denials by the stage's own PIDs are its.
    fn ending(
        &self,
        outcome: WaitOutcome,
        cause: Option<CancelCause>,
        window: Window<'_>,
    ) -> Settled<()> {
        command::ending(
            &self.name,
            outcome,
            cause,
            window.scope,
            self.denial_pids.as_ref(),
            window.started,
        )
    }

    /// The shell-touching finish the reaper's posting closure, holding no
    /// `&Shell`, could not give an external's raw outcome: the run's one
    /// ending, less a death by what this collector `sent`, whose stop is its
    /// own doing, consumed as a thread's is.
    fn observe(
        self,
        outcome: WaitOutcome,
        cause: Option<CancelCause>,
        sent: Option<CancelCause>,
        window: Window<'_>,
        shell: &Shell,
    ) -> StageObservation {
        let settled = match outcome.cancelled_by(cause) {
            Some(cause) if sent == Some(cause) => Ok(()),
            _ => self
                .ending(outcome, cause, window)
                .map_err(|broke| match &self.frame {
                    Some(frame) => broke.claimed_by(|| frame.prov.clone()),
                    None => broke,
                }),
        };
        let fragment = self
            .frame
            .and_then(|frame| {
                frame
                    .record
                    .end(settled.as_ref().err(), AuditIo::default(), shell)
            })
            .map_or_else(AuditFragment::empty, |observation| {
                AuditFragment::from_observations(vec![observation])
            });
        StageObservation {
            end: End::settled(settled.map(|()| ValClos::UNIT)),
            audit: fragment,
        }
    }
}

/// One stage's terminal news, before the fold's shell-touching finish.
pub(super) enum StageEnd {
    Thread {
        obs: StageObservation,
        stage: ScopeId,
    },
    External {
        end: ExternalEnd,
        outcome: WaitOutcome,
        sent: Option<CancelCause>,
    },
}

impl StageEnd {
    /// An external is settled under the cause [`drained`] reads; a key the
    /// tty delivered is attributed through the scope or not at all (ADR
    /// 261016_a-cleanup-is-shielded-within-its-grace).
    fn settle(self, shell: &Shell, window: Window<'_>) -> StageObservation {
        match self {
            Self::Thread { obs, stage } if obs.names(&stage) => obs.forgiven(),
            Self::Thread { obs, .. } => obs,
            Self::External { end, outcome, sent } => {
                let ExternalEnd { drain, settlement } = end;
                let cause = drained(drain, sent, window);
                settlement.observe(outcome, cause, sent, window, shell)
            }
        }
    }
}

/// Finish a filed external's jail, join its pumps under `scope`, and read
/// the cause it ended under.  The jail goes first, its finish killing a
/// straggler that holds a pump's pipe; a cause ending the join kills an
/// `owned` group once; a cut for a dead write detaches the pumps.  The cause
/// is the one `sent` or the one in force once the pumps are done, whichever
/// is stronger: a reader-gone stop this collector did not send attributes
/// nothing.
fn drained(
    Drain { jail, drains }: Drain,
    sent: Option<CancelCause>,
    Window { scope, owned, .. }: Window<'_>,
) -> Option<CancelCause> {
    drop(jail);
    if sent != Some(CancelCause::ReaderGone) {
        drains.join_under(scope, || owned.into_iter().for_each(Pgid::kill));
    }
    sent.max(scope.cause().filter(|c| *c != CancelCause::ReaderGone))
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

    /// Only a console group takes Ctrl-Break: a pid, which no event reaches,
    /// gets the kill as its opening.
    #[cfg(windows)]
    fn signal(&self, _: crate::process::Signal) {
        match self {
            Self::Group(pgid) => crate::process::break_pipeline_group(*pgid),
            Self::Pid(watch) => watch.kill(),
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
    /// [`Event::Ended`]: no dedicated waiter thread anywhere.  Shared, so a
    /// group's pin may hold the zombie past the stage's own end.
    pub(super) fn watch(&self, child: crate::process::ChildHandle) -> Arc<Watch> {
        let ix = self.ix;
        Arc::new(child.into_watch(self.tx.clone(), move |o| Event::Ended(ix, o)))
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
            slot.send(Event::Returned(slot.ix, StageObservation::unreported()));
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
    pub(super) end: End,
    pub(super) audit: AuditFragment,
}

impl StageObservation {
    #[cfg(test)]
    pub(super) fn ok() -> Self {
        Self {
            end: End::Ok(ValClos::UNIT),
            audit: AuditFragment::empty(),
        }
    }

    /// A stage that never reported: a panic, which the fold re-raises.
    fn unreported() -> Self {
        Self {
            end: End::unreported(),
            audit: AuditFragment::empty(),
        }
    }

    /// A stop this collector owns: one naming `stage`, whatever its cause (a
    /// reader-gone stop, or the teardown this collector ran).
    fn names(&self, stage: &ScopeId) -> bool {
        matches!(
            &self.end,
            End::Stopped(Stop { owner: Some(owner), .. }) if owner == stage
        )
    }

    /// The teardown an escape out of `stage` calls for: `Cancelled` for an
    /// `exit`; a stop's own cause unless it names `stage` (the collector's own
    /// doing: a reader-gone stop, or a teardown already running).
    fn tears_down_for(&self, stage: &ScopeId) -> Option<CancelCause> {
        match &self.end {
            End::Exited(_) => Some(CancelCause::Cancelled),
            End::Stopped(Stop { cause, .. }) if !self.names(stage) => Some(*cause),
            _ => None,
        }
    }

    fn panicked(&self) -> bool {
        matches!(self.end, End::Panic(_))
    }

    /// A stop this collector owns is dropped; what the stage observed still
    /// happened and is kept.
    pub(super) fn forgiven(self) -> Self {
        Self {
            end: End::Ok(ValClos::UNIT),
            audit: self.audit,
        }
    }
}

/// By [`Break::rank`] (law 5); ties go to the earlier stage.
fn stronger(held: Break, new: Break) -> Break {
    if new.rank() > held.rank() { new } else { held }
}

/// Live collector state: every stage still running or already observed.
pub(super) struct CollectState {
    stages: Vec<Option<StageHandle>>,
    observed: Vec<Option<StageEnd>>,
    /// Window start for the sandbox-denial reader.
    started: Instant,
    /// An owned pgid is signalled and killed whole; a joined one is its
    /// owner's, and ral's own is never signalled whole, so in either this
    /// collector addresses its own externals by pid.
    group: Group,
    /// The process whose pid is a lone leader's group id, held, alive and
    /// then a zombie past its stage's end, until this collector's last kill
    /// of the group.  A pipeline's anchor pins its own group; an envelope's
    /// payload group, nothing once its monitor has ended.
    pin: Option<Arc<Watch>>,
    /// Terminal restoration responsibility and, for a tenant, the one ear
    /// that strikes reported keys on the run's node.
    terminal: TerminalHold,
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
        terminal: TerminalHold,
        scope: Scope,
        cleanup_grace: Duration,
        started: Instant,
    ) -> Self {
        Self {
            stages: Vec::new(),
            observed: Vec::new(),
            started,
            group,
            pin: None,
            terminal,
            rx,
            scope,
            cleanup_grace,
            teardown: None,
        }
    }

    /// A collector over a standalone command's one child ([`Started::collect`]),
    /// in the group it landed in: one it leads, with no anchor; its enclosing
    /// stage's; or ral's own.  It has no thread stage to grant a grace.
    pub(super) fn lone(mut started: Started, since: Instant, shell: &Shell) -> Self {
        let terminal = std::mem::take(&mut started.spawned.terminal);
        let group = started.spawned.group.clone();
        // A lone leader's zombie pins its group through settlement. An
        // envelope's payload, not its monitor, leads the addressed group.
        let leads = group
            .owned()
            .is_some_and(|owned| owned.as_raw() == started.spawned.child.id().cast_signed());
        let (tx, rx) = std::sync::mpsc::channel();
        let slot = Slot { ix: 0, tx };
        let stage = started.register(&slot, group.owned());
        let pin = leads.then(|| Arc::clone(&stage.watch));
        let mut state = Self::new(
            rx,
            group,
            terminal,
            shell.scope.clone(),
            Duration::ZERO,
            since,
        );
        state.pin = pin;
        state.push(StageHandle::new(StageKind::External(stage), None, slot));
        state
    }

    /// The key `signal` is, if the terminal was lent: struck on the node, and
    /// read by the scope at the next slice; inert otherwise.
    #[cfg(unix)]
    fn hear(&mut self, signal: crate::process::Signal) {
        self.terminal.hear(signal);
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
        self.terminal.pressed()
    }

    /// A `CollectState` for `step`'s own transition-table tests — no group, no
    /// anchor, no processes — beside the sender its stages' slots address.
    #[cfg(test)]
    pub(super) fn for_step_test() -> (Self, Sender<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let anchor = Scope::clone(&crate::process::DurableRoot::default().child());
        let state = Self {
            stages: Vec::new(),
            observed: Vec::new(),
            started: Instant::now(),
            // Never `Owns`: a forced end would `kill(-pgid)` a pgid nobody owns.
            group: Group::Joins(crate::process::Membership::new(
                Pgid::from_raw(1).expect("1 is positive"),
                anchor.clone(),
            )),
            pin: None,
            terminal: TerminalHold::default(),
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

    /// Everything a teardown addresses: the group this collector owns, else
    /// each external by pid — a thread stage has neither, its cancel being
    /// all teardown owes it — and every envelope, whose payload leads a
    /// session of its own that no signal to the collector's group reaches.
    /// `own: false` leaves the collector's own out: the kernel, a joined
    /// group's owner, or the tty to ral's own group, delivers it.
    fn addresses(&self, own: bool) -> impl Iterator<Item = Address<'_>> {
        let owned = self.group.owned();
        let group = owned.filter(|_| own).map(Address::Group);
        let pids = (own && owned.is_none()).then(|| {
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
            .filter_map(StageHandle::own_group)
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
            let own = self.pressed() < Some(cause) && self.group.owes(cause);
            for address in self.addresses(own) {
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
        // Even the first failed start may leave a descendant in our group.
        // The anchor still pins its ID, whether any stage registered or not.
        self.cancel_all(self.scope.cause().unwrap_or(CancelCause::Cancelled));
    }

    /// Take what the stages left to settle, once the anchor has ended: every
    /// key it reported is struck first, then the terminal comes back from
    /// tenants all dead, before any pump is joined, as a lone command's does.
    fn vacate(&mut self) -> Vec<Option<StageEnd>> {
        #[cfg(unix)]
        self.hear_out();
        if !self.live() {
            std::mem::take(&mut self.terminal).reclaim(None);
        }
        std::mem::take(&mut self.observed)
    }

    /// The audit and verdict fold, in launch order — the one place `&Shell`
    /// reaches an external's settlement, and why `last` is the final stage's
    /// value whenever that stage is `Ok` and unread otherwise.  The audit is
    /// merged before the verdict is ranked, so a failing stage still
    /// contributes what it observed.
    pub(super) fn fold(mut self, mooring: &Mooring, shell: &mut Shell) -> Settled<ValClos> {
        let observed = self.vacate();
        let mut verdict: Option<Break> = None;
        let mut panic: Option<String> = None;
        let mut last = ValClos::UNIT;
        for end in observed {
            // Thread stages already published their facts.
            let surface = matches!(&end, Some(StageEnd::External { .. })).then_some(mooring);
            let StageObservation { end, audit } = match end {
                Some(end) => end.settle(shell, self.window()),
                None => StageObservation::unreported(),
            };
            for observation in audit.into_observations() {
                shell.observe_stamped(surface, observation);
            }
            match end {
                End::Panic(text) => {
                    panic.get_or_insert(text);
                }
                ending => match ending.into_settled() {
                    Err(br) => {
                        verdict = Some(match verdict {
                            Some(held) => stronger(held, br),
                            None => br,
                        });
                    }
                    Ok(v) => last = v,
                },
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
                .filter(|c| c.cause == cause)
                .map(|c| c.owner);
            verdict = verdict.or_else(|| Some(Break::stopped(cause, owner)));
        }
        verdict.map_or(Ok(last), Err)
    }

    /// Settle the one external a lone collector observed, as a command ends:
    /// the terminal back the instant its tenant is dead, hearing the key it
    /// died of, as no anchor reported one; then its pumps, after which no
    /// kill addresses its group and the pin goes; then its own ending, whose
    /// stop is the run's.
    pub(super) fn settle_lone(mut self) -> Settled<()> {
        let Some(Some(StageEnd::External { end, outcome, sent })) = self.observed.pop() else {
            // Re-raised, as a panicked stage is.
            std::panic::resume_unwind(Box::new(UNREPORTED.to_owned()));
        };
        std::mem::take(&mut self.terminal).reclaim(outcome.death());
        let ExternalEnd { drain, settlement } = end;
        let cause = drained(drain, sent, self.window());
        drop(self.pin.take());
        settlement.ending(outcome, cause, self.window())
    }

    fn window(&self) -> Window<'_> {
        Window {
            started: self.started,
            scope: &self.scope,
            owned: self.group.owned(),
        }
    }
}

impl Started {
    /// Register any external, lone or pipelined, with the same reaper.
    /// Address an envelope separately only if its payload is outside the
    /// collector's owned group; otherwise a cancellation would signal it twice.
    pub(super) fn register(self, slot: &Slot, collector_owned: Option<Pgid>) -> LiveExternal {
        let Self {
            shown,
            spawned:
                Spawned {
                    child,
                    group,
                    jail,
                    terminal,
                },
            denial_pids,
            drains,
        } = self;
        // Lone collection takes the hold before registering; pipeline stages
        // join their owner and therefore arrive with an empty hold.
        drop(terminal);
        LiveExternal {
            watch: slot.watch(child),
            payload: group
                .enveloped()
                .filter(|payload| Some(*payload) != collector_owned),
            end: ExternalEnd {
                drain: Drain { jail, drains },
                settlement: Settlement {
                    name: shown,
                    frame: None,
                    denial_pids,
                },
            },
        }
    }

    /// Collect the child with its launch's opaque terminal responsibility.
    /// The terminal returns before jail settlement and pump joins.
    pub(crate) fn collect(self, since: Instant, shell: &Shell) -> Settled<()> {
        // Declared first, so an unwind kills before it releases.
        #[cfg(windows)]
        let job = match self.spawned.group {
            Group::Owns(leader) => Some(super::group::Job::owning(leader)),
            Group::Joins(_) | Group::Shares => None,
        };
        let mut collect = CollectState::lone(self, since, shell);
        collect.drive();
        #[cfg(windows)]
        if let Some(job) = job {
            job.drain(&shell.scope);
        }
        collect.settle_lone()
    }
}

/// What the collector waits a lone external out to before it settles it: its
/// raw end, and the cause it sent.
#[cfg(all(test, unix))]
pub(crate) fn lone_end(
    spawned: Spawned<Group>,
    shell: &Shell,
) -> (WaitOutcome, Option<CancelCause>) {
    let started = Started {
        shown: "test".into(),
        spawned,
        denial_pids: None,
        drains: crate::io::child::Drains::default(),
    };
    let mut lone = CollectState::lone(started, Instant::now(), shell);
    lone.drive();
    match lone.observed.pop().flatten() {
        Some(StageEnd::External { outcome, sent, .. }) => (outcome, sent),
        _ => panic!("a lone collector files its one external"),
    }
}

/// Settle a process test's launch through the production terminal-return boundary.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn collect_spawned(
    spawned: Spawned<Group>,
    since: Instant,
    shell: &Shell,
) -> Settled<()> {
    Started {
        shown: "test".into(),
        spawned,
        denial_pids: None,
        drains: crate::io::child::Drains::default(),
    }
    .collect(since, shell)
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
