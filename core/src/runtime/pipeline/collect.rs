//! Collect phase: every lifecycle edge arrives as one [`Event`] on one
//! channel.  [`step`] is pure over them — effects are returned, not performed
//! — and however the stages ended, the verdict folds in launch order.

use super::super::command;
use super::stage::StageHandle;
use crate::evaluator::audit::{command_fact, observe_stamped};
use crate::ir::PipeYield;
use crate::process::{
    CancelCause, CancelWatch, Group, Pgid, TerminalLoan, WaitOutcome, Watch, watch_cancel,
};
use crate::types::{
    AuditFragment, AuditIo, Break, CommandOrigin, Error, Mooring, Observation, Settled, Shell,
    Value, epoch_us,
};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

/// Attach a kernel-denial diagnostic to a failed stage's error.  Best-effort:
/// siblings share the group, so the reader scopes deny lines to a descendant
/// sample taken now, over the pipeline-wide window from `started`.
fn augment_stage_failure(err: Error, shell: &Shell, started: Instant) -> Error {
    if shell.sandbox_projection().is_none() {
        return err;
    }
    let pids = crate::sandbox::sample_descendants(std::process::id());
    crate::sandbox::augment_failure(err, shell, &pids, started)
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
    let now = epoch_us();
    let obs = Observation::spanning(
        shell.call_site(),
        now,
        now,
        shell.context.principal(),
        command_fact(
            name,
            args,
            err.map_or(0, Error::exit_code),
            CommandOrigin::External,
            AuditIo::default(),
            err.map(|e| e.message.clone()),
        ),
    );
    AuditFragment::from_observations(vec![obs])
}

/// The shell-touching finish the reaper's posting closure, holding no
/// `&Shell`, could not give an external's raw outcome.
fn finish_external_settlement(
    name: &str,
    args: Vec<String>,
    outcome: WaitOutcome,
    sent: Option<CancelCause>,
    enveloped: bool,
    shell: &Shell,
    started: Instant,
) -> StageObservation {
    let err = outcome
        .classify(sent, enveloped)
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
        enveloped: bool,
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
                enveloped,
            } => {
                if let Some(jail) = &jail {
                    jail.finish();
                }
                pumps.settle(sent == Some(CancelCause::ReaderGone));
                let cause = sent.max(pressed);
                finish_external_settlement(&name, args, outcome, cause, enveloped, shell, started)
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
                StageObservation::failure(Error::new(msg.to_string(), 1)),
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
    /// envelope, whose payload leads a session of its own (§3.2) that no
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
    /// broadcast before the verdict is ranked, so a failing stage still
    /// contributes what it observed.  The loan drops with `self`, returning
    /// the terminal and striking the frame whatever the verdict.
    pub(super) fn fold(
        mut self,
        mooring: &Mooring,
        shell: &mut Shell,
        yields: PipeYield,
    ) -> Settled<Value> {
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
                    1,
                )))
            });
            let StageObservation { settled, audit } = end.settle(shell, self.started, pressed);
            for observation in audit.into_observations() {
                observe_stamped(shell, mooring, observation);
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
        verdict.map_or_else(
            || match yields {
                PipeYield::Last => Ok(last),
                PipeYield::Unit if matches!(last, Value::Unit) => Ok(Value::Unit),
                PipeYield::Unit => Err(Break::Error(
                    crate::evaluator::machine::bytes_promise_broken(&last),
                )),
            },
            Err,
        )
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
mod tests {
    use super::super::group::PipelineGroup;
    use super::*;
    use crate::types::Escape;

    fn error_break(status: i32, msg: &str) -> Break {
        Break::Error(Error::new(msg.to_string(), status))
    }

    /// The four laws of the join: an escape outranks an error, and within a
    /// rank the earlier stage wins.
    #[test]
    fn breaks_join_by_rank_earlier_stage_breaking_ties() {
        match stronger(error_break(7, "first"), error_break(9, "second")) {
            Break::Error(error) => assert_eq!(error.message, "first"),
            Break::Escape(_) => panic!("an error must not displace an earlier error"),
        }
        assert!(matches!(
            stronger(
                Break::Escape(Escape::Exit(1)),
                Break::Escape(Escape::Exit(2))
            ),
            Break::Escape(Escape::Exit(1))
        ));
        assert!(matches!(
            stronger(
                error_break(7, "early failure"),
                Break::Escape(Escape::Exit(3))
            ),
            Break::Escape(Escape::Exit(3))
        ));
        assert!(matches!(
            stronger(
                Break::Escape(Escape::Exit(3)),
                error_break(7, "later failure")
            ),
            Break::Escape(Escape::Exit(3))
        ));
    }

    #[test]
    fn protocol_error_does_not_supersede_earlier_failure() {
        match stronger(
            error_break(7, "stage one boom"),
            error_break(1, "report pipe: broken"),
        ) {
            Break::Error(error) => assert_eq!(error.message, "stage one boom"),
            Break::Escape(_) => panic!("expected the first stage failure to win"),
        }
    }

    /// An owning group and a collector wired onto the same channel, as
    /// `PipeNode::launch` builds them.
    fn owning_pipeline(
        shell: &Shell,
        mooring: &Mooring,
    ) -> (PipelineGroup, CollectState, Sender<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let group = PipelineGroup::prepare(shell, tx.clone()).expect("anchor spawns");
        let collect = CollectState::new(rx, &tx, group.group(), None, mooring, Instant::now());
        (group, collect, tx)
    }

    /// `drive` over two real children folds what they settled.
    #[test]
    fn drive_folds_two_settling_stages_to_done() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let (group, mut collect, tx) = owning_pipeline(&shell, &mooring);

        for (ix, code) in [0, 1].into_iter().enumerate() {
            let slot = Slot { ix, tx: tx.clone() };
            collect.push(StageHandle::for_test(slot, spawn_exiting(code)));
        }
        drop(tx);

        collect.drive();
        match collect.fold(&mooring, &mut shell, PipeYield::Unit) {
            Err(Break::Error(error)) => assert_ne!(error.exit_code(), 0),
            other => panic!("expected the final stage's exit to fold in, got {other:?}"),
        }
        drop(group);
    }

    /// An external that exits with `code`: the host's shell, asked for
    /// nothing but an exit status.
    ///
    /// `/bin/sh` on Unix rather than `true`/`false`, which are not in `/bin`
    /// on macOS; `cmd.exe` on Windows, where neither `/bin` nor those two
    /// exist at all.  What the test needs is a real child that settles with
    /// the code it was given, and both spell that.
    #[cfg(unix)]
    const EXIT_SHELL: (&str, &str) = ("/bin/sh", "-c");
    #[cfg(windows)]
    const EXIT_SHELL: (&str, &str) = ("cmd.exe", "/c");

    /// See [`EXIT_SHELL`].
    fn spawn_exiting(code: u8) -> crate::process::ChildHandle {
        let (shell, flag) = EXIT_SHELL;
        let child = std::process::Command::new(shell)
            .args([flag, &format!("exit {code}")])
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {shell}: {e}"));
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
            stdout
                .read_exact(&mut byte)
                .expect("read the grandchild pid");
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
        let (group, mut collect, tx) = owning_pipeline(&shell, &mooring);
        let (child, pid) = spawn_stage_with_grandchild(&group, true);

        collect.push(StageHandle::for_test(Slot { ix: 0, tx }, child));
        drop(collect);

        assert_dead_within_2s(pid);
        drop(group);
    }

    /// The ordinary end kills nothing: a member that outlives its stage — a
    /// descendant the stage forked — outlives the pipeline too.
    #[cfg(unix)]
    #[test]
    fn a_collector_that_observed_every_stage_kills_nothing() {
        let shell = Shell::default();
        let mooring = Mooring::adrift();
        let (group, mut collect, tx) = owning_pipeline(&shell, &mooring);
        let (child, pid) = spawn_stage_with_grandchild(&group, false);

        collect.push(StageHandle::for_test(Slot { ix: 0, tx }, child));
        collect.drive();
        drop(collect);

        assert!(
            unsafe { libc::kill(pid, 0) } == 0,
            "the orphaned grandchild must survive the collector's ordinary end"
        );
        unsafe { libc::kill(pid, libc::SIGKILL) };
        drop(group);
    }

    /// A joining collector has no pgid to kill, so `cancel_all` must reach its
    /// external's pid itself rather than hang in the final drain.
    #[cfg(unix)]
    #[test]
    fn a_joining_collector_cancel_all_kills_its_externals_and_returns() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let owner =
            PipelineGroup::prepare(&shell, std::sync::mpsc::channel().0).expect("anchor spawns");
        let pgid = owner.leader_pgid();
        let joining =
            PipelineGroup::joining(owner.membership(&crate::process::CancelScope::root()));

        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let (child, _pgid) =
            crate::process::spawn_with_pgid(&mut cmd, crate::process::PgidPolicy::Join(pgid))
                .expect("spawn /bin/sh under the owner's pgid");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut collect =
            CollectState::new(rx, &tx, joining.group(), None, &mooring, Instant::now());
        collect.push(StageHandle::for_test(
            Slot {
                ix: 0,
                tx: tx.clone(),
            },
            crate::process::ChildHandle::from_std(child),
        ));
        drop(tx);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                collect.cancel_all(CancelCause::Deadline, false);
                let _ = done_tx.send(());
            });
            done_rx
                .recv_timeout(std::time::Duration::from_secs(3))
                .expect("cancel_all must not hang on a joining group's own external");
        });

        assert!(
            matches!(
                collect.fold(&mooring, &mut shell, PipeYield::Unit),
                Err(Break::Error(_))
            ),
            "a killed stage must fold in an error, not a quiet ok"
        );
        drop(owner);
    }

    // ── step: the transition table ─────────────────────────────────────────
    //
    // Every case is a sequence of `Event`s fed to `step` and an assertion on
    // the `Effect` it returns: no sleeps, no retry loops.

    fn state_with(n: usize) -> CollectState {
        let (mut state, tx) = CollectState::for_step_test();
        for ix in 0..n {
            state.push(StageHandle::fake_external_for_step_test(Slot {
                ix,
                tx: tx.clone(),
            }));
        }
        state
    }

    fn threads_with(n: usize) -> CollectState {
        let (mut state, tx) = CollectState::for_step_test();
        for ix in 0..n {
            state.push(StageHandle::fake_thread_for_step_test(Slot {
                ix,
                tx: tx.clone(),
            }));
        }
        state
    }

    fn sent_to(state: &CollectState, ix: usize) -> Option<CancelCause> {
        state.stages[ix].as_ref().expect("a live stage").sent
    }

    /// A settled reader arms the edge its still-running writer holds — and an
    /// already-finished writer has none left to arm, exactly the honesty
    /// `!{ echo a; exit 3 } | head -1` needs.
    #[test]
    fn settling_a_stage_arms_its_still_running_writers_edge_but_spares_a_finished_one() {
        let mut state = state_with(4);
        // Stage 0 settles first — nothing upstream of it to arm.
        assert_eq!(
            step(&mut state, Event::Ended(0, WaitOutcome::Exited(0))),
            None
        );
        assert!(
            state.live(),
            "one of four stages settling must not end the pipeline"
        );
        // Stage 1 settles: its writer (0) already settled, so it keeps its
        // outcome and nothing is armed against it.
        let effect = step(&mut state, Event::Ended(1, WaitOutcome::Exited(0)));
        assert_eq!(
            effect, None,
            "a writer that already settled must keep its outcome, not be armed against"
        );
        // Stage 3 settles while its writer (2) is still running: that edge is
        // armed — and stage 2 has not settled, so the pipeline is not done.
        let effect = step(&mut state, Event::Ended(3, WaitOutcome::Exited(0)));
        assert_eq!(effect, Some(Effect::ArmEdge(2)));
        assert!(
            state.live(),
            "an unobserved writer means the pipeline is not done"
        );
        // Stage 2 now settles too: every stage is observed.
        let _ = step(&mut state, Event::Ended(2, WaitOutcome::Exited(0)));
        assert!(
            !state.live(),
            "the last stage settling must end the pipeline"
        );
    }

    /// The sentinel's news is what cuts a stage: `Wrote` emits the one kill,
    /// which records the cause against it.
    #[test]
    fn a_write_to_a_dead_edge_kills_its_writer() {
        let mut state = state_with(2);
        assert_eq!(
            step(&mut state, Event::Wrote(0)),
            Some(Effect::KillStage(0))
        );
        state.run(Effect::KillStage(0));
        assert_eq!(sent_to(&state, 0), Some(CancelCause::ReaderGone));
    }

    /// A stage that ended on its own account before the sentinel was heard
    /// keeps its outcome: the news is dropped.
    #[test]
    fn a_write_heard_after_its_writer_was_filed_is_ignored() {
        let mut state = state_with(2);
        let _ = step(&mut state, Event::Ended(0, WaitOutcome::Exited(3)));
        assert_eq!(
            step(&mut state, Event::Wrote(0)),
            None,
            "a filed stage must not be cut"
        );
    }

    /// A cancel always tears down, whatever the cause or its source, and
    /// teardown must deliver it, nothing else having.
    #[test]
    fn a_cancelled_event_cancels_all() {
        let mut state = state_with(1);
        assert_eq!(
            step(&mut state, Event::Cancelled(CancelCause::Deadline)),
            Some(Effect::CancelAll {
                cause: CancelCause::Deadline,
                delivered: false
            })
        );
    }

    /// A key the anchor heard reached every member of the group by the
    /// kernel's own hand, so teardown must not send a second copy of it; a
    /// signal no terminal sends is inert.
    #[cfg(unix)]
    #[test]
    fn a_heard_key_tears_down_without_resending_it() {
        let frame = crate::process::DurableRoot::new().worker();
        let mut state = state_with(1);
        state.loan = Some(TerminalLoan::for_test(&frame));
        let heard = |n| Event::Heard(crate::process::Signal::new(n));
        assert_eq!(step(&mut state, heard(libc::SIGTERM)), None);
        assert_eq!(
            step(&mut state, heard(libc::SIGINT)),
            Some(Effect::CancelAll {
                cause: CancelCause::Interrupt,
                delivered: true
            })
        );
    }

    /// With no terminal lent, nothing the anchor hears is a key.
    #[cfg(unix)]
    #[test]
    fn with_no_loan_a_heard_signal_is_inert() {
        let mut state = state_with(1);
        assert_eq!(
            step(
                &mut state,
                Event::Heard(crate::process::Signal::new(libc::SIGINT))
            ),
            None
        );
    }

    /// A stage dead of SIGINT nobody sent is the key's cancellation only if
    /// the key was pressed on the lent terminal, and its own signal otherwise.
    #[cfg(unix)]
    #[test]
    fn a_stage_dead_of_the_key_is_cancelled_only_if_it_was_pressed() {
        let shell = Shell::default();
        let end = || StageEnd::External {
            name: "sleep".to_string(),
            args: Vec::new(),
            outcome: WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGINT)),
            jail: None,
            pumps: command::Pumps::default(),
            sent: None,
            enveloped: false,
        };
        let settled = |pressed| end().settle(&shell, Instant::now(), pressed).settled;
        match settled(Some(CancelCause::Interrupt)) {
            Err(Break::Error(e)) => assert_eq!(e.cancelled_by(), Some(CancelCause::Interrupt)),
            other => panic!("expected the key's cancellation, got {other:?}"),
        }
        match settled(None) {
            Err(Break::Error(e)) => assert!(
                matches!(
                    e.status,
                    crate::types::Status::Process(crate::process::CommandFailure::Signal(s))
                        if s == crate::process::Signal::new(libc::SIGINT)
                ),
                "{e:?}"
            ),
            other => panic!("expected the stage's own signal death, got {other:?}"),
        }
    }

    /// A stage this collector tore down reports the cause it sent, not the
    /// signal that carried it.
    #[cfg(unix)]
    #[test]
    fn a_torn_down_externals_death_names_the_cause_it_was_sent() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let mut state = state_with(1);
        state.stages[0]
            .as_mut()
            .expect("a live stage")
            .cancel(CancelCause::Explicit);
        let _ = step(
            &mut state,
            Event::Ended(
                0,
                WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGTERM)),
            ),
        );

        match state.fold(&mooring, &mut shell, PipeYield::Unit) {
            Err(Break::Error(e)) => assert_eq!(
                e.cancelled_by(),
                Some(CancelCause::Explicit),
                "expected the cause, not the signal: {e:?}"
            ),
            other => panic!("expected the teardown death to fold in as an error, got {other:?}"),
        }
    }

    /// A `try` inside a stage body its reader cut sees why: the record reads
    /// `` `cancelled `reader-gone `` at 141, which the handler re-raises as
    /// its own failure for the run to report.
    #[cfg(unix)]
    #[test]
    fn a_try_in_a_stage_its_reader_cut_sees_reader_gone() {
        let src = "let spew = { |n| to-line y\n spew $n }\n\
                   !{ try { spew 0 } { |err| fail [status: $err[status], message: !{str $err[reason]}] } } | head -n 1";
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let report = shell.run(crate::run::RunRequest {
            run: crate::engine::testkit::run(src),
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
        });
        match report {
            crate::run::RunReport::Ran {
                ending: crate::run::Ending::Raised { error, .. },
                ..
            } => {
                assert_eq!(error.exit_code(), 141, "{error:?}");
                assert!(
                    error.message.contains("`cancelled `reader-gone"),
                    "{error:?}"
                );
            }
            _ => panic!("the handler's re-raise must end the run"),
        }
    }

    /// `Ended(ix, Signaled(KILL))` folds as the collector's own forgiven kill
    /// exactly when the stage was sent one, and as an ordinary failure when
    /// nothing here ever did.
    #[cfg(unix)]
    #[test]
    fn a_stage_kill_is_forgiven_by_sent_and_kept_without_it() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let kill = WaitOutcome::Signaled(crate::process::Signal::new(libc::SIGKILL));

        let mut forgiven = state_with(1);
        forgiven.stages[0]
            .as_mut()
            .expect("a live stage")
            .cancel(CancelCause::ReaderGone);
        let _ = step(&mut forgiven, Event::Ended(0, kill));
        assert!(
            forgiven.fold(&mooring, &mut shell, PipeYield::Unit).is_ok(),
            "a reader-gone kill must be forgiven, not folded in as a failure"
        );

        let mut kept = state_with(1);
        let _ = step(&mut kept, Event::Ended(0, kill));
        assert!(
            matches!(
                kept.fold(&mooring, &mut shell, PipeYield::Unit),
                Err(Break::Error(_))
            ),
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
        let mut state = threads_with(2);

        assert_eq!(
            step(&mut state, Event::Returned(1, StageObservation::ok())),
            Some(Effect::ArmEdge(0)),
            "the reader settling first must arm its writer's edge"
        );

        let _ = step(
            &mut state,
            Event::Returned(0, StageObservation::failure(Error::new("boom", 3))),
        );

        match state.fold(&mooring, &mut shell, PipeYield::Unit) {
            Err(Break::Error(e)) => assert_eq!(e.exit_code(), 3),
            other => panic!("expected the writer's own exit 3 to survive, got {other:?}"),
        }
    }

    /// A `ReaderGone` break — as a dead edge's sink or a `ReaderGone` scope
    /// cancel mints it — is forgiven on its own evidence, nothing having been
    /// sent to the stage.
    #[test]
    fn a_thread_writer_the_cancel_ended_is_forgiven() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let mut state = threads_with(1);
        assert_eq!(sent_to(&state, 0), None, "nothing was sent to this stage");

        let _ = step(
            &mut state,
            Event::Returned(
                0,
                StageObservation::failure(Error::cancelled(CancelCause::ReaderGone)),
            ),
        );

        let folded = state.fold(&mooring, &mut shell, PipeYield::Unit);
        assert!(
            folded.is_ok(),
            "a writer the reader-gone cancel actually ended must be forgiven: {folded:?}"
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
            Event::Returned(0, obs) => match obs.settled {
                Err(Break::Error(err)) => assert!(
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
            Event::Returned(0, obs) => match obs.settled {
                Err(Break::Error(err)) => assert!(
                    err.message.contains("without reporting"),
                    "expected the silent-end message: {}",
                    err.message
                ),
                other => panic!("expected an Error break, got {other:?}"),
            },
            _ => panic!("expected Returned(0, _)"),
        }
    }

    /// `drive` over a producer that unwinds still returns rather than blocking
    /// in `recv` forever, and the panic folds in as an `Error`.
    #[test]
    fn drive_settles_a_stage_whose_producer_unwound() {
        let mut shell = Shell::default();
        let mooring = Mooring::adrift();
        let (group, mut collect, tx) = owning_pipeline(&shell, &mooring);

        let slot = Slot { ix: 0, tx };
        collect.push(StageHandle::fake_thread_for_step_test(slot.clone()));

        let handle = std::thread::spawn(move || {
            let _guard = SettleOnDrop::new(slot);
            panic!("boom");
        });

        collect.drive();
        let _ = handle.join();

        match collect.fold(&mooring, &mut shell, PipeYield::Unit) {
            Err(Break::Error(_)) => {}
            other => panic!("expected the panic to fold in as an Error, got {other:?}"),
        }
        drop(group);
    }
}
