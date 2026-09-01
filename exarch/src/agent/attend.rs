//! The one loop every node runs: pull the next item off the node's own inbox,
//! take it up, do the ready-boundary housekeeping, and turn a nudge-worthy
//! outcome into a self-posted nudge.  Stepping the model to quiescence over
//! one item is [`super::deliberate`]'s job; nothing here special-cases a
//! node's position, since a child's reply is deposited on its own agent and a
//! non-reply end is delivered up its parent's mailbox by the spawn site.

use crate::agent::digest::PRESSURE_THRESHOLD_FALLBACK;
use crate::agent::event::QuiesceReason;
use crate::agent::nudge;
use crate::agent::seat::engine_gone;
use crate::agent::{Avatar, deliberate, panic_msg};
use crate::bus::{AgentOutcome, AgentState, Emitter, Item, ParkMode, Post, WORKER_PANIC_PREFIX};
use crate::provider::{Provider, ProviderError};
use crate::shell_eval;
use ral_core::serial::FOValue;
use ral_core::protocol::Severed;

/// What the surrounding loop does next: `Stop` on `/quit` or a headless root's `reply`.
enum Flow {
    Continue,
    Stop,
}

/// ral calls between disk-warn ceiling checks: the walk is a full scan of the
/// session log dir and scratch, too costly to pay at every ready boundary.
const DISK_WARN_CHECK_INTERVAL: u64 = 32;

/// What [`Avatar::attend`] does once [`Control`] has run a slash command.
pub enum Verdict {
    Continue,
    Quit,
}

/// The frontend hook the attend loop calls for an [`Item::Command`] drained at
/// the exchange boundary.
///
/// A command routes here rather than staying frontend-side only when it
/// mutates the session, which the attend thread owns; view-only ones and
/// `/model` (a swap of the shared
/// [`ProviderHandle`](crate::agent::ProviderHandle)) stay on the UI thread.
pub trait Control {
    /// Run the slash-command line `raw` against the agent the loop owns.
    fn command(&mut self, raw: &str, session: &mut Avatar, emit: &Emitter) -> Verdict;
}

/// The control for drivers with no slash commands (headless, peers, tests),
/// where an [`Item::Command`] never arises.
pub struct NoControl;

impl Control for NoControl {
    fn command(&mut self, _raw: &str, _session: &mut Avatar, _emit: &Emitter) -> Verdict {
        Verdict::Continue
    }
}

impl Avatar {
    /// The one attend loop, identical for every node: pull the next inbox
    /// item, take it up, and repeat until an empty inbox meets a
    /// [`Self::park_mode`] that does not park, or a cancel ends one that does.
    /// The returned `payload` is the faithful [`FOValue`] a `reply` carried,
    /// left for the consuming edge to render.
    pub fn attend(
        &mut self,
        control: &mut dyn Control,
        emit: &Emitter,
    ) -> (AgentOutcome, Option<FOValue>) {
        self.attend_with(control, emit, |_, mode| mode)
    }

    /// [`Self::attend`] with the park verdict reshaped by `policy` before it
    /// reaches either of its two readers — the idle-state emission just below
    /// and `next_or_idle`'s own parking argument — so a frontend's status and
    /// the loop's actual wait always agree on what an empty inbox means.
    /// `attend`'s public behavior is this under the identity policy; the only
    /// other one is [`quiesce_when_childless`], which
    /// [`crate::headless::converse_settled`] runs under instead.
    pub(crate) fn attend_with(
        &mut self,
        control: &mut dyn Control,
        emit: &Emitter,
        policy: impl Fn(&Self, ParkMode) -> ParkMode,
    ) -> (AgentOutcome, Option<FOValue>) {
        self.couple(emit);
        let mut final_outcome = (AgentOutcome::Failed(NO_REPLY_REASON.into()), None);
        loop {
            if let Some(s) = self.seat.severed() {
                self.severed(&s, &mut final_outcome);
                break;
            }
            // Every pass is a settled ready boundary.  A lease-chain reap is
            // deliberately not drained here: core pushes its `` `notice `` on
            // the surface stream of the run that observes it, so a reap during
            // a long idle surfaces once an item next runs.
            if let Err(s) = self.check_disk_warn()
                && matches!(self.severed(&s, &mut final_outcome), Flow::Stop)
            {
                break;
            }
            // The state a frontend shows over the coming silence is this park's
            // own verdict.  Only on an empty queue: with an item already in
            // hand the agent is not idle for any observable moment, and the
            // deliberation's own transitions are the truth.
            if self.inbox.is_empty() {
                let mode = policy(self, self.park_mode(self.agent.engaged()));
                self.agent.set_resting(mode == ParkMode::Engaged);
                self.recorder()
                    .transient(crate::record::Transient::State(idle_state(mode)));
            }
            // `next_or_idle` recomputes the park verdict on every wake.  A
            // returning agent that quiesces breaks here, and its outcome
            // reaches its parent through the worker epilogue, not this break.
            let Some(item) = self.inbox.next_or_idle(
                |engaged| policy(self, self.park_mode(engaged)),
                &self.agent.cancel,
            ) else {
                break;
            };
            self.agent.set_resting(false);
            if matches!(
                self.take_up(&item, control, emit, &mut final_outcome),
                Flow::Stop
            ) {
                break;
            }
        }
        // A park that quiesced on severance must report `engine_gone`, not
        // the generic `NO_REPLY_REASON` an empty inbox otherwise leaves.
        if let Some(s) = self.seat.severed() {
            self.severed(&s, &mut final_outcome);
        }
        // `take_up` quiesces per item; this catches whichever path breaks the
        // loop, so the agent is ReadyForUser however it ends.
        if !self.log.lock().is_ready() {
            self.log.lock().quiesce(QuiesceReason::Aborted);
        }
        debug_assert!(
            self.log.lock().is_ready(),
            "attend must leave the agent ReadyForUser"
        );
        final_outcome
    }

    /// [`Self::attend`]'s per-exchange half, for
    /// [`converse`](crate::headless::converse): drain the seeded message and
    /// any nudge continuation it raises, then return instead of blocking for
    /// the next one.  Only the pull differs — an empty queue ends the exchange
    /// rather than waiting on anything still in flight — so the per-item step
    /// stays the shared [`Self::take_up`].  [`NoControl`] is right and not an
    /// oversight: converse posts no [`Item::Command`], so a slash-shaped user
    /// message reaches the model as ordinary text.
    pub(crate) fn attend_backlog(&mut self, emit: &Emitter) -> (AgentOutcome, Option<FOValue>) {
        self.couple(emit);
        let mut control = NoControl;
        let mut final_outcome = (AgentOutcome::Failed(NO_REPLY_REASON.into()), None);
        loop {
            if let Some(s) = self.seat.severed() {
                self.severed(&s, &mut final_outcome);
                break;
            }
            if let Err(s) = self.check_disk_warn()
                && matches!(self.severed(&s, &mut final_outcome), Flow::Stop)
            {
                break;
            }
            let Some(item) = self.inbox.next_item() else {
                break;
            };
            if matches!(
                self.take_up(&item, &mut control, emit, &mut final_outcome),
                Flow::Stop
            ) {
                break;
            }
        }
        // A park that quiesced on severance must report `engine_gone`, not
        // the generic `NO_REPLY_REASON` an empty inbox otherwise leaves.
        if let Some(s) = self.seat.severed() {
            self.severed(&s, &mut final_outcome);
        }
        if !self.log.lock().is_ready() {
            self.log.lock().quiesce(QuiesceReason::Aborted);
        }
        final_outcome
    }

    /// Take up one item already drawn from the inbox — the per-item step
    /// [`Self::attend`] and [`Self::attend_backlog`] share.  `final_outcome`
    /// is the running `(outcome, payload)` the caller reports once its own
    /// loop ends; an item that never reaches the deliberation (a command)
    /// leaves it as it was.  A drawn item is always admissible: staleness is
    /// settled at the inbox's own pop, against that inbox's clear-epoch.
    fn take_up(
        &mut self,
        item: &Item,
        control: &mut dyn Control,
        emit: &Emitter,
        final_outcome: &mut (AgentOutcome, Option<FOValue>),
    ) -> Flow {
        self.heard(item);
        // Only a genuine boundary clears the latches and a prior exchange's
        // Esc; a self-nudge is the same exchange continuing.
        if item.opens_exchange() {
            if let Some(nudges) = &mut self.nudges {
                nudges.reset();
            }
            self.agent.cancel.reset();
        }
        if let Item::Command(raw) = item {
            return match control.command(raw, self, emit) {
                Verdict::Quit => Flow::Stop,
                Verdict::Continue => Flow::Continue,
            };
        }
        announce(item, &self.recorder());
        // Read once, so a `/model` swap on the UI thread lands on the next
        // item rather than mid-item.
        let active = self.agent.provider.current();
        let token = self.agent.cancel.clone();
        let prompt = Some(item.text());
        let continues = item.continues();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.deliberate(&active, prompt, continues, &token, emit)
        }));
        let outcome = match attempt {
            Ok(o) => o,
            Err(p) => {
                // Host-side only — transport, surface decode, render.  An
                // eval-side panic is caught at `Shell::run`, which rolls the
                // dynamic state back, so it never unwinds this far.  Recording
                // and continuing keeps one crash from sinking the agent.
                let msg = panic_msg(&p);
                self.note_error(format!("{WORKER_PANIC_PREFIX}{msg}"));
                *final_outcome = (
                    AgentOutcome::Failed(format!("{WORKER_PANIC_PREFIX}{msg}")),
                    None,
                );
                if !self.log.lock().is_ready() {
                    self.log.lock().quiesce(QuiesceReason::Aborted);
                }
                return Flow::Continue;
            }
        };
        // A provider error or step cap can leave the session mid-protocol.
        // The caller's guard would only fire on loop exit; quiesce now so the
        // next prompt — nudge or user — is admissible.
        if !self.log.lock().is_ready() {
            self.log.lock().quiesce(QuiesceReason::Aborted);
        }
        // A root's reply goes to whoever drives its loop, not to a deposit: it
        // has no parent to fetch it, and staging one would read as a standing
        // reply the nudge layer must fall silent for.
        if let Ok(deliberate::Outcome::Replied(v)) = &outcome
            && self.agent.parent.is_some()
        {
            self.agent.deposit_reply(v.clone());
        }
        *final_outcome = agent_outcome(&outcome);
        // Before any nudge decision, and whether or not one follows: a chat
        // trunk keeps no registry, and its failures must still reach the human.
        if let Err(e) = &outcome
            && let Err(error) = self.log.lock().record_provider_error(e)
        {
            eprintln!("exarch: a provider error was not recorded: {error}");
        }
        if let Ok(deliberate::Outcome::Severed(s)) = &outcome {
            return self.severed(s, final_outcome);
        }
        // A boundary read, legal here: the batch has fully drained and no
        // dispatch is in flight.
        let workers_idle = match self.probe_workers() {
            Ok(workers) => workers.is_empty(),
            Err(s) => return self.severed(&s, final_outcome),
        };
        // `last_input` is fresh off `deliberate`, so the gauge reads this
        // completion's own pressure, not the one it was called with.
        let facts = nudge::Facts {
            must_reply: self.returns(),
            pinned: self.pinned_digest(),
            pressure: self.pressure_gauge(&active),
            // Nudged only when nothing else is already carrying this agent
            // forward: no reply standing for a parent to fetch, no detached
            // shell work, no busy children.
            quiet: !self.agent.has_reply() && workers_idle && !self.agent.has_busy_children(),
        };
        let nudge_msg = match &mut self.nudges {
            Some(nudges) => nudges.react(&outcome, &facts, &mut self.log.lock()),
            None => None,
        };
        if let Some(text) = nudge_msg {
            let exchange = self.log.lock().current_exchange();
            if let Some(exchange) = exchange {
                self.inbox.push(Post::Nudge { exchange, text });
            } else {
                // Unreachable: every outcome `react` answers followed a
                // deliberation whose prompt `append_user` committed, which
                // opened an exchange.
                let recorded = self.log.lock().record_error(format!(
                    "a nudge was decided with no exchange open to continue — \
                     dropping it: {text}"
                ));
                if let Err(error) = recorded {
                    eprintln!("exarch: a dropped nudge was not recorded: {error}");
                }
            }
        }
        // Only the headless root ends on `reply` — a child parks on its deposit.
        if self.agent.parent.is_none() && matches!(outcome, Ok(deliberate::Outcome::Replied(_))) {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    /// Book a drawn item against the park verdict: a direct child's
    /// result means it is no longer awaited.  [`Avatar::settle`] makes
    /// deliver-then-retire structural, so a parked parent never sees "no live
    /// child" without the result already queued.  Every item reaching here
    /// already survived the inbox's own pop-time fence against a `/clear`
    /// ([`crate::bus::inbox`]'s clear-epoch), so there is nothing left to
    /// admit — the `Surface` arm is a routing tripwire, not an admission.
    pub(super) fn heard(&self, item: &Item) {
        match item {
            Item::Agent(r) => self.agent.heard(r.id),
            Item::Surface { id, .. } => debug_assert_eq!(
                *id, self.agent.id,
                "a spawn's surface batch always drains in the session it was stamped with"
            ),
            _ => {}
        }
    }

    /// How an empty inbox should be treated, recomputed on every wake, from
    /// `engaged` — the exchange clock, read by `next_or_idle` under the queue
    /// mutex — and this agent's own status.  A
    /// conversing agent parks [`ParkMode::Held`], immune to cancellation; a
    /// returning agent a human has exchanged with parks [`ParkMode::Engaged`]
    /// — the same wait, but a terminate-cause cancel still ends it, and the
    /// fleet's idle lease rather than this predicate bounds it.
    fn park_mode(&self, engaged: bool) -> ParkMode {
        // Nothing is left to wait for once the engine is gone.
        if self.seat.severed().is_some() {
            return ParkMode::Quiesce;
        }
        let conversing = self.agent.interactive && !self.returns();
        if conversing {
            // `next_or_idle` lets a terminate cause end every park but `Held`,
            // so a conversing agent asks here instead: `/close` stamps this
            // token, and parking Held past it would be a zombie.
            if self.agent.cancel.terminated() {
                return ParkMode::Quiesce;
            }
            return ParkMode::Held;
        }
        // Only an agent with somewhere to report waits to be messaged; a
        // headless root returns and ends.
        if self.agent.parent.is_some() && self.returns() && (engaged || self.agent.has_reply()) {
            return ParkMode::Engaged;
        }
        if self.agent.has_busy_children() {
            return ParkMode::HeldByChildren;
        }
        if self.agent.schedules.armed() {
            return ParkMode::UntilCancelled;
        }
        ParkMode::Quiesce
    }

    /// One reading of the pressure gauge, no state.  The soft line is
    /// [`crate::agent::digest::pressure_due`], one reserve ahead of
    /// auto-compaction; an unknown window falls back to the byte heuristic
    /// against [`PRESSURE_THRESHOLD_FALLBACK`].
    fn pressure_gauge(&self, provider: &Provider) -> nudge::Pressure {
        match provider.context_window() {
            Some(w) if w > 0 => match self.token_pressure(w) {
                Some(detail) => nudge::Pressure::Over(detail),
                None if self.measured_input().is_some() => nudge::Pressure::Under,
                None => nudge::Pressure::Unknown,
            },
            _ => {
                let bytes = self.log.lock().history_bytes();
                if bytes >= PRESSURE_THRESHOLD_FALLBACK {
                    nudge::Pressure::Over(format!("{} KB", bytes / 1024))
                } else {
                    nudge::Pressure::Under
                }
            }
        }
    }

    /// Warn once per excursion above the operator's disk-warn ceiling, as an
    /// operational note ([`Forensic::SystemNote`](crate::record::Forensic::SystemNote))
    /// — nothing is ever rotated or deleted.  Unconfigured it walks nothing at
    /// all; otherwise it walks on the [`Self::ral_epoch`] cadence of
    /// [`DISK_WARN_CHECK_INTERVAL`].
    ///
    /// # Errors
    /// The engine's severance, from the `EXARCH_SCRATCH` probe.
    fn check_disk_warn(&mut self) -> Result<(), Severed> {
        let Some(ceiling) = self.agent.disk_warn_bytes else {
            return Ok(());
        };
        if self.ral_epoch < self.disk_check_epoch {
            return Ok(());
        }
        self.disk_check_epoch = self.ral_epoch + DISK_WARN_CHECK_INTERVAL;
        let mut total = crate::agent::resources::dir_size(self.log.lock().dir());
        if let Some(scratch) = self.probe_env_var("EXARCH_SCRATCH")? {
            total += crate::agent::resources::dir_size(&std::path::PathBuf::from(&scratch));
        }
        if total > ceiling {
            if !self.disk_warn_latched {
                self.disk_warn_latched = true;
                Self::note(
                    format!(
                        "disk: session log + scratch is {} KiB, over the {} KiB warn ceiling \
                         — forensic records are never rotated or deleted automatically; \
                         clean up by hand",
                        total / 1024,
                        ceiling / 1024
                    ),
                    self,
                );
            }
        } else {
            self.disk_warn_latched = false;
        }
        Ok(())
    }

    /// The one edge every caller ends on when it learns its engine is
    /// severed: record the sentence, fail the outcome, quiesce the log if a
    /// deliberation left it mid-protocol, and stop the loop that called this.
    fn severed(&self, s: &Severed, final_outcome: &mut (AgentOutcome, Option<FOValue>)) -> Flow {
        self.note_error(engine_gone(s));
        *final_outcome = (AgentOutcome::Failed(engine_gone(s)), None);
        if !self.log.lock().is_ready() {
            self.log.lock().quiesce(QuiesceReason::Aborted);
        }
        Flow::Stop
    }
}

/// Emit the chrome an item's source shows as it enters context.  A nudge is an
/// internal continuation and a command never reaches the model, so both are quiet.
pub(super) fn announce(item: &Item, recorder: &crate::record::Emitter) {
    match item {
        Item::Human(_) | Item::Wakeup(_) | Item::Message(_) => {
            // The live row derives from the published `Display::Prompt`
            // record, so this is the one authoring site.
            record_commit(
                recorder,
                crate::record::Display::Prompt { text: item.text() },
            );
        }
        Item::Agent(r) => {
            // The record carries the breadcrumb-reduced pair the scrollback
            // block is built from, not the raw outcome enum.
            let (text, error) = r.outcome.breadcrumb();
            record_commit(
                recorder,
                crate::record::Display::SubagentDone {
                    name: r.name.clone(),
                    text,
                    error,
                    elapsed_ms: u64::try_from(r.elapsed.as_millis()).unwrap_or(u64::MAX),
                },
            );
        }
        // A detached `spawn`'s deferred batch, decoded as the live foreground
        // decode would — its io and diff surfaces through the same commit
        // producer, one buffer per batch.  It was stamped with and posted to
        // this same session, so the emitter's id already routes its cards to
        // the right viewport.
        Item::Surface { id, values, .. } => {
            let mut buf = crate::record::commit::SurfaceBuffer::new();
            for v in values {
                match shell_eval::decode_surface(v) {
                    shell_eval::Decoded::Surface(surface) => {
                        if let Err(error) =
                            crate::fleet::desk::absorb_surface(&mut buf, recorder, *id, &surface)
                        {
                            recorder.report_fault(&error);
                        }
                    }
                    shell_eval::Decoded::Landed => {}
                    shell_eval::Decoded::Unknown => {
                        if let Err(error) =
                            recorder.emit(shell_eval::unknown_surface_note(v.type_name()))
                        {
                            recorder.report_fault(&error);
                        }
                    }
                }
            }
            if let Err(error) = buf.flush_surfaces(recorder) {
                recorder.report_fault(&error);
            }
        }
        Item::Nudge { .. } | Item::Command(_) => {}
    }
}

/// Record one display commit, surfacing a failed append as a
/// `Transient::Fault` exactly as the surface path does.
fn record_commit(recorder: &crate::record::Emitter, commit: crate::record::Display) {
    if let Err(error) = recorder.emit(commit) {
        recorder.report_fault(&error);
    }
}

/// [`Avatar::attend_with`]'s park policy for
/// [`crate::headless::converse_settled`]: a conversing trunk's `park_mode`
/// always answers [`ParkMode::Held`], blind to its fleet, since a chat trunk
/// never used to have one worth waiting on. This reshapes exactly that
/// answer — live children hold as [`ParkMode::HeldByChildren`] instead, and a
/// childless trunk quiesces at once rather than parking on a human who is not
/// there to type. Every other verdict passes through unchanged: `Engaged`
/// requires a `steer` synod never offers, and `UntilCancelled` requires an
/// armed schedule `converse_settled` already refused at construction.
pub(crate) fn quiesce_when_childless(avatar: &Avatar, mode: ParkMode) -> ParkMode {
    match mode {
        ParkMode::Held if avatar.agent.has_busy_children() => ParkMode::HeldByChildren,
        ParkMode::Held => ParkMode::Quiesce,
        other => other,
    }
}

/// The state an idle agent is in, read off the park verdict that is about to
/// hold it there: a wait on the fleet is the one idleness that is not the
/// human's turn, and every other park — including the [`ParkMode::Quiesce`]
/// that ends the loop — leaves nothing outstanding.
fn idle_state(mode: ParkMode) -> AgentState {
    match mode {
        ParkMode::HeldByChildren => AgentState::WaitingOnAgents,
        ParkMode::Held | ParkMode::Engaged | ParkMode::UntilCancelled | ParkMode::Quiesce => {
            AgentState::Ready
        }
    }
}

/// Reduce a finished deliberation to the `(tag, payload)` a returning agent's
/// result carries.  Only `reply` carries a value up, as the faithful
/// [`FOValue`] the model passed; there is no scrape, so a finish without one
/// did not complete the contract and settles [`AgentOutcome::Failed`].
const NO_REPLY_REASON: &str = "ended without calling `reply`";

fn agent_outcome(
    r: &Result<deliberate::Outcome, ProviderError>,
) -> (AgentOutcome, Option<FOValue>) {
    match r {
        // The headless root's own `reply` reaches the epilogue directly — a
        // child's is deposited on its own agent instead, and never
        // drives `take_up`'s call here.
        Ok(deliberate::Outcome::Replied(v)) => (AgentOutcome::Replied, Some(v.clone())),
        // Re-nudged within budget by `nudge` before it ever reaches here.
        Ok(deliberate::Outcome::Complete(_) | deliberate::Outcome::Empty) => {
            (AgentOutcome::Failed(NO_REPLY_REASON.into()), None)
        }
        Ok(deliberate::Outcome::Stopped { reason }) => {
            (AgentOutcome::Stopped(reason.clone()), None)
        }
        Ok(deliberate::Outcome::Cancelled) => (AgentOutcome::Cancelled, None),
        Ok(deliberate::Outcome::Capped) => (AgentOutcome::Stopped("step cap reached".into()), None),
        Ok(deliberate::Outcome::Severed(s)) => (AgentOutcome::Failed(engine_gone(s)), None),
        Err(e) => (AgentOutcome::Failed(e.summary()), None),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::testkit::*;
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{Display, Forensic, Record};

    /// Every item `announce` draws records its display commit through the
    /// seam: a prompt commits `Display::Prompt`, and a subagent's breadcrumb
    /// commits `Display::SubagentDone`.
    #[test]
    fn announce_records_display_facts() {
        use crate::bus::AgentResult;

        let (tx, rx) = crate::bus::channel();
        let recorder = crate::record::Emitter::none();
        recorder.attach(crate::record::FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });

        announce(&Item::Human("hello".into()), &recorder);
        announce(
            &Item::Agent(AgentResult {
                id: 7,
                name: "helper".into(),
                outcome: AgentOutcome::Failed("boom".into()),
                elapsed: std::time::Duration::from_millis(1500),
            }),
            &recorder,
        );

        let mut facts: Vec<&'static str> = Vec::new();
        for record in crate::bus::drain_records(&rx) {
            match record {
                Record::Display(Display::Prompt { text }) => {
                    assert_eq!(text, "hello");
                    facts.push("prompt");
                }
                Record::Display(Display::SubagentDone {
                    name,
                    error,
                    elapsed_ms,
                    ..
                }) => {
                    assert_eq!(name, "helper");
                    assert_eq!(error.as_deref(), Some("boom"));
                    assert_eq!(elapsed_ms, 1500);
                    facts.push("subagent");
                }
                _ => {}
            }
        }
        assert_eq!(facts, ["prompt", "subagent"], "both commits reach the seam");
    }

    /// The park verdict reads engagement off the agent's own exchange clock,
    /// never off the TUI's focus cursor.
    #[test]
    fn park_mode_reads_engagement_from_the_exchange_clock() {
        let held = trunk(true);
        assert_eq!(held.park_mode(held.agent.engaged()), ParkMode::Held);

        let parent = Avatar::for_test("system").unwrap();
        let child = parent.fork(parent.caps().clone()).expect("fork child");
        assert_eq!(
            child.park_mode(child.agent.engaged()),
            ParkMode::Quiesce,
            "un-engaged, no live children, no schedule: idle quiesce delivers the outcome"
        );

        child.agent.mailbox().steer("hi".into());
        assert_eq!(
            child.park_mode(child.agent.engaged()),
            ParkMode::Engaged,
            "a human exchange engages the child, which now parks messageable"
        );
    }

    /// The `reply` refusal keys on the captured `returns` bit, not on
    /// trunk-ness, and is an ordinary call error rather than a termination.
    #[test]
    fn reply_refused_identically_for_trunk_and_branch_conversing_agents() {
        let mut root = trunk(true);
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, root.agent.id);
        let root_result = root.run_shell("c1".into(), "agents `reply 1", 5, &emit);
        let refusal = "you converse with the user; you do not return";
        assert!(
            root_result.content.contains(refusal),
            "got: {}",
            root_result.content
        );

        // A distinct name: `root` already holds `TRUNK_NAME` in this same
        // fleet, and names are unique among the live.
        let mut branch = root
            .branch("branch".into())
            .expect("branch a conversing child");
        let branch_result = branch.run_shell("c2".into(), "agents `reply 1", 5, &emit);
        assert!(
            branch_result.content.contains(refusal),
            "a /branch child must be refused with the same text, got: {}",
            branch_result.content
        );
        assert_eq!(
            branch.park_mode(branch.agent.engaged()),
            ParkMode::Held,
            "a /branch child still parks Held after a refused reply"
        );
    }

    /// An un-replied finish is re-nudged within budget, then settles `Failed`
    /// — the final prose is never scraped as the answer.
    #[test]
    fn sub_agent_without_reply_is_re_nudged_then_fails() {
        let parent = Avatar::for_test("system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("do the thing".into());
        // More prose-only replies than the budget will consume, so the test
        // does not couple to the exact budget.
        let mut script = Script::new();
        for _ in 0..8 {
            script = script.then(Reply::text("here is prose, but no reply"));
        }
        let (outcome, payload) = drive_peer(&mut child, scripted("test-model", script));
        assert!(
            matches!(outcome, AgentOutcome::Failed(_)),
            "an un-replied finish settles Failed, got {outcome:?}"
        );
        assert!(
            payload.is_none(),
            "the final prose must not be scraped: {payload:?}"
        );
        assert!(child.is_ready());
    }

    /// A standing deposited reply gates every nudge kind at once — the single
    /// `quiet` test, not three separate flags.  A registered child holding a
    /// reply from an earlier turn draws no empty-turn nudge on this one.
    #[test]
    fn deposited_reply_suppresses_every_nudge() {
        let parent = Avatar::for_test("system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.agent.deposit_reply(FOValue::String {
            value: "already replied".into(),
        });

        child
            .agent
            .provider
            .swap(scripted("test-model", Script::new().then(Reply::empty())));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, child.agent.id);
        child.couple(&emit);
        child.seed("do more".into());
        let item = child.inbox.next_item().expect("the seeded item");
        let mut control = NoControl;
        let mut final_outcome = (AgentOutcome::Failed(NO_REPLY_REASON.into()), None);
        child.take_up(&item, &mut control, &emit, &mut final_outcome);

        assert!(
            child.inbox.next_item().is_none(),
            "a standing reply must suppress even the empty-turn nudge, budget-free rules included"
        );
    }

    /// A host-side unwind — transport, decode, render — is recorded, fails the
    /// item, and leaves the log ready, so the next prompt still deliberates.
    /// The eval-side panics in `agent/shell.rs` never reach this arm:
    /// `Shell::run` rolls those back long before the loop sees them.
    #[test]
    fn host_panic_is_recorded_and_the_next_prompt_still_deliberates() {
        let mut session = Avatar::for_test("system").unwrap();
        // The first prompt unwinds; the rest answer the second and the no-reply
        // nudges it draws.
        let mut script = Script::new().then(Reply::panicking());
        for _ in 0..8 {
            script = script.then(Reply::text("recovered"));
        }
        session.agent.provider.swap(scripted("test-model", script));

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);
        session.seed("crash on this one".into());
        let (panicked, _) = session.attend(&mut NoControl, &emit);
        assert!(
            matches!(&panicked, AgentOutcome::Failed(m) if m.starts_with(WORKER_PANIC_PREFIX)),
            "the unwind must fail the item, not sink the attend thread: {panicked:?}"
        );
        assert!(
            session.is_ready(),
            "the panicked exchange must be wound back"
        );

        // Seeded only now: consecutive prompts coalesce into one inbox entry.
        session.seed("but answer this one".into());
        let (outcome, _) = session.attend(&mut NoControl, &emit);

        let signals: Vec<crate::bus::Signal> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            signals.iter().any(|s| matches!(
                s,
                crate::bus::Signal::Fact(_, fact)
                    if matches!(fact.value(), Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX))
            )),
            "the unwind must reach the user as an error too"
        );
        assert!(
            signals.iter().any(|s| matches!(
                s,
                crate::bus::Signal::Transient(_, crate::record::Transient::Token(t)) if t == "recovered"
            )),
            "the prompt after the panic must still deliberate"
        );
        assert!(
            matches!(outcome, AgentOutcome::Failed(_)),
            "an un-replied run settles Failed, got {outcome:?}"
        );
        assert!(session.is_ready());

        let parsed = crate::record::read_records(&session.log_dir().join("record.jsonl"))
            .expect("the panicked exchange must leave a parseable log");
        assert!(
            parsed.iter().any(|r| matches!(
                r,
                Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX)
            )),
            "the panic is recorded in the one seam's log too"
        );
    }

    /// Run `cmd` as one dispatched run under a millisecond-scale
    /// `deferred_lease`, so a reap test need not wait out `run_shell`'s real
    /// 1 h/24 h policy.
    fn dispatch_with_lease(session: &Avatar, cmd: &str, lease: ral_core::types::WorkerLease) {
        use ral_core::protocol::{DispatchId, Host, Program, Run};
        use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
        session.seat.transport().dispatch(
            DispatchId(0),
            Run {
                program: Program::Source(cmd.to_string()),
                script_name: "<test>".to_string(),
                caps: ral_core::types::Capabilities::root(),
                wall: Some(std::time::Duration::from_secs(5)),
                deferred_lease: Some(lease),
                worker_cap: None,
                io: RunIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: RunStdin::Empty,
                trail: None,
            },
            &(std::sync::Arc::new(()) as std::sync::Arc<dyn Host>),
        );
    }

    /// A reap the lease chain performs between runs sits queued: core only
    /// pushes a `` `notice `` from inside a run's own surface stream, so it
    /// surfaces at the next run, and exactly once.
    #[test]
    fn ready_boundary_reap_notice_surfaces_at_the_next_run() {
        let mut session = Avatar::for_test("system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        dispatch_with_lease(
            &session,
            "spawn { test-clear-block-forever }",
            ral_core::types::WorkerLease {
                idle: std::time::Duration::from_millis(40),
                backstop: std::time::Duration::from_secs(10),
            },
        );

        // Past the idle bound, unpolled: the lease chain reaps it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while probe_int(&session, "worker-count") > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the unpolled worker must be reaped within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

        session.run_shell("c0".into(), "return 1", 5, &emit);

        let reaps: Vec<(String, String)> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|record| match record {
                Record::Display(Display::Notice {
                    notice: crate::record::NoticeFact::Reap { cmd, cause },
                }) => Some((cmd, cause)),
                _ => None,
            })
            .collect();
        assert_eq!(reaps.len(), 1, "the queued reap surfaces exactly once");
        let (cmd, cause) = &reaps[0];
        assert_eq!(cmd, "<block>", "the reap must name the spawned body");
        assert_eq!(
            cause, "idle",
            "an unpolled worker past its idle bound reaps as Idle"
        );

        session.run_shell("c1".into(), "return 1", 5, &emit);
        assert!(
            crate::bus::drain_records(&rx)
                .into_iter()
                .all(|record| !matches!(record, Record::Display(Display::Notice { .. }))),
            "a further run with nothing new queued must surface no second notice"
        );
    }

    /// The reminder is built from the register the model actually wrote, and it
    /// becomes the next committed prompt: `pinned_digest` → `Facts` →
    /// `Post::Nudge` → the user turn the next deliberation sends.  The nudge
    /// unit tests hand `react` a hand-written digest; only this joins it to the
    /// live register.
    #[test]
    fn pinned_state_reminder_reads_the_live_register_and_becomes_the_next_prompt() {
        let mut session = Avatar::for_test("system").unwrap();
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session.run_shell(
            "c0".into(),
            r#"surface `pin [key: "goal", body: `text [spans: [[text: "ship the reminder"]]]]"#,
            5,
            &emit,
        );
        let digest = session
            .pinned_digest()
            .expect("the model's own pin must reach the register");
        assert!(digest.contains("ship the reminder"), "got: {digest}");

        // A finish with nothing returned draws the reminder; the single
        // `reply` that follows ends the loop, with no verification round.
        session.agent.provider.swap(scripted(
            "test-model",
            Script::new()
                .then(Reply::text("done"))
                .then(Reply::tool_calls(vec![ral_call("r1", "agents `reply 'x'")])),
        ));
        session.seed("get on with it".into());
        let (outcome, _) = session.attend(&mut NoControl, &emit);

        assert!(
            matches!(outcome, AgentOutcome::Replied),
            "the loop must terminate on the first reply, got {outcome:?}"
        );
        assert!(
            crate::bus::drain_records(&rx).into_iter().any(|record| matches!(
                record,
                Record::Forensic(Forensic::Nudge { cause, .. }) if cause == "pinned-state reminder"
            )),
            "the live register must raise a pinned-state nudge"
        );
        let view = serde_json::to_string(&session.rendered_messages()).unwrap();
        assert!(
            view.contains("There is pinned state: ship the reminder"),
            "the reminder must be committed as the next prompt, not dropped: {view}"
        );
    }

    /// A returning agent has no reply-triggered nudge round to relive; the
    /// livelock regression instead lives here, for the interactive root: a
    /// stationary model-written pin queues at most one nudge, never a
    /// perpetual `Complete → nudge → Complete` cycle that would never let the
    /// loop park.
    #[test]
    fn pin_reminder_does_not_relivelock_the_interactive_root() {
        let mut session = trunk(true);
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session.run_shell(
            "c0".into(),
            r#"surface `pin [key: "goal", body: `text [spans: [[text: "keep going"]]]]"#,
            5,
            &emit,
        );

        session.agent.provider.swap(scripted(
            "test-model",
            Script::new().then(Reply::text("working on it")),
        ));
        session.seed("go".into());
        let item = session.inbox.next_item().expect("the seeded item");
        let mut control = NoControl;
        let mut final_outcome = (AgentOutcome::Failed(NO_REPLY_REASON.into()), None);
        session.take_up(&item, &mut control, &emit, &mut final_outcome);

        let nudge = session
            .inbox
            .next_item()
            .expect("the first quiet completion must queue one pin reminder");
        assert!(
            matches!(&nudge, Item::Nudge { text, .. } if text.contains("There is pinned state")),
            "expected a pin reminder, got {nudge:?}"
        );

        session.agent.provider.swap(scripted(
            "test-model",
            Script::new().then(Reply::text("still working")),
        ));
        session.take_up(&nudge, &mut control, &emit, &mut final_outcome);

        assert!(
            session.inbox.next_item().is_none(),
            "a stationary pin must queue no second nudge — the loop would park, not relivelock"
        );
    }

    /// `--chat` withholds the tool, so nothing is left to steer the model
    /// toward: the empty turn that
    /// `nudge::tests::empty_turn_nudges_and_consumes_budget` nudges over here
    /// stands as the model left it, and no synthetic prompt is committed.
    #[test]
    fn chat_trunk_never_nudges() {
        let mut session = chat_trunk();
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session
            .agent
            .provider
            .swap(scripted("test-model", Script::new().then(Reply::empty())));
        session.seed("hello".into());
        session.attend_backlog(&emit);

        assert!(
            !crate::bus::drain_records(&rx)
                .into_iter()
                .any(|record| matches!(record, Record::Forensic(Forensic::Nudge { .. }))),
            "a chat trunk raises no nudge"
        );
        let view = serde_json::to_string(&session.rendered_messages()).unwrap();
        assert!(
            view.contains("hello"),
            "the exchange must have happened at all"
        );
        assert!(
            !view.contains("EXARCH_REMINDER"),
            "nothing synthetic may join a chat conversation: {view}"
        );
    }

    /// A boundary prune's `Display::Notice` rides the surface stream of the
    /// call it fires inside — one notice naming the binding, none once
    /// nothing is left idle.  The tiny re-armed bound is for speed only.
    #[test]
    fn boundary_prune_notice_rides_the_runs_own_stream() {
        let mut session = Avatar::for_test("system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 2,
                large_binding_bytes: u64::MAX,
            });

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        session.run_shell("c0".into(), "let reap_me = 1", 5, &emit);
        session.run_shell("c1".into(), "$[0]", 5, &emit);
        session.run_shell("c2".into(), "$[0]", 5, &emit);

        let prunes: Vec<(Vec<String>, Vec<u64>)> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|record| match record {
                Record::Display(Display::Notice {
                    notice: crate::record::NoticeFact::Prune { names, idle_calls },
                }) => Some((names, idle_calls)),
                _ => None,
            })
            .collect();
        assert_eq!(
            prunes.len(),
            1,
            "exactly one prune notice rides a run's surface stream"
        );
        let (names, idle_calls) = &prunes[0];
        assert_eq!(names, &vec!["reap_me".to_string()]);
        assert_eq!(idle_calls.len(), 1);
        assert!(idle_calls[0] >= 2, "idle at least the armed bound");

        session.run_shell("c3".into(), "$[0]", 5, &emit);
        assert!(
            crate::bus::drain_records(&rx).into_iter().all(|record| {
                !matches!(
                    record,
                    Record::Display(Display::Notice {
                        notice: crate::record::NoticeFact::Prune { .. },
                    })
                )
            }),
            "nothing left idle — no second prune notice"
        );
    }

    /// The disk-warn ceiling a fixture is built under, comfortably above a
    /// fresh session's own footprint, and a file that alone crosses it.
    const WARN_CEILING: u64 = 64 * 1024;
    const OVER_CEILING: usize = 1024 * 1024;

    fn warned_at(ceiling: u64) -> Avatar {
        Avatar::for_test_with(crate::agent::TestTrunk {
            disk_warn_bytes: Some(ceiling),
            ..crate::agent::TestTrunk::new("system")
        })
        .expect("a trunk under a disk-warn ceiling")
    }

    /// Unconfigured, the check returns before the epoch bookkeeping: no walk,
    /// no emission, no cost.
    #[test]
    fn check_disk_warn_unconfigured_never_walks_or_warns() {
        let mut session = Avatar::for_test("system").unwrap();
        assert!(session.agent.disk_warn_bytes.is_none());

        let (tx, rx) = crate::bus::channel();
        session.recorder().attach(crate::record::FleetSink {
            id: session.agent.id,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        session
            .check_disk_warn()
            .expect("an identity seat never severs");

        assert_eq!(
            session.disk_check_epoch, 0,
            "the early return never advances the check epoch"
        );
        assert!(
            crate::bus::drain_records(&rx).is_empty(),
            "unconfigured: never emits, ever"
        );
    }

    /// The latch suppresses a repeat until the figure falls back under.
    #[test]
    fn check_disk_warn_still_above_does_not_repeat() {
        let mut session = warned_at(WARN_CEILING);
        std::fs::write(session.log_dir().join("big.txt"), vec![0u8; OVER_CEILING]).unwrap();

        let (tx, rx) = crate::bus::channel();
        session.recorder().attach(crate::record::FleetSink {
            id: session.agent.id,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        session
            .check_disk_warn()
            .expect("an identity seat never severs");
        assert!(
            !crate::bus::drain_records(&rx).is_empty(),
            "the first crossing warns"
        );

        // Force the amortization window open without driving real ral calls.
        session.disk_check_epoch = session.ral_epoch;
        session
            .check_disk_warn()
            .expect("an identity seat never severs");
        assert!(
            crate::bus::drain_records(&rx).is_empty(),
            "still above the ceiling: the latch suppresses a repeat"
        );
    }

    /// Falling back under clears the latch, so a re-crossing warns again.
    #[test]
    fn check_disk_warn_falling_below_rearms_the_latch() {
        let mut session = warned_at(WARN_CEILING);
        // The ceiling sits comfortably above the session's own baseline plus
        // one durable warning note's own footprint, so the big file alone —
        // not the warning's own record — decides whether it is crossed.
        let big = session.log_dir().join("big.txt");
        std::fs::write(&big, vec![0u8; OVER_CEILING]).unwrap();

        let (tx, rx) = crate::bus::channel();
        session.recorder().attach(crate::record::FleetSink {
            id: session.agent.id,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        session
            .check_disk_warn()
            .expect("an identity seat never severs");
        let fact = crate::bus::drain_records(&rx)
            .into_iter()
            .next()
            .expect("the first crossing warns");
        match fact {
            Record::Forensic(Forensic::SystemNote { text }) => {
                assert!(text.contains("disk"), "{text}");
            }
            other => panic!("expected Forensic::SystemNote, got {other:?}"),
        }

        std::fs::remove_file(&big).unwrap();
        session.disk_check_epoch = session.ral_epoch;
        session
            .check_disk_warn()
            .expect("an identity seat never severs");
        assert!(
            crate::bus::drain_records(&rx).is_empty(),
            "back under the ceiling: no warning, just the latch clearing"
        );
        assert!(!session.disk_warn_latched, "the latch is cleared");

        std::fs::write(&big, vec![0u8; OVER_CEILING]).unwrap();
        session.disk_check_epoch = session.ral_epoch;
        session
            .check_disk_warn()
            .expect("an identity seat never severs");
        assert!(
            !crate::bus::drain_records(&rx).is_empty(),
            "re-crossing after falling below warns again"
        );
    }

    /// A binding prune is transcript/TUI-only: it must never grow the
    /// model-view `record.jsonl`.
    #[test]
    fn prune_does_not_add_model_events() {
        let mut session = Avatar::for_test("system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 1,
                large_binding_bytes: u64::MAX,
            });

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);
        session.run_shell("c0".into(), "let events_json_x = 1", 5, &emit);
        let after_bind = session.log.lock().event_count();

        // The prune fires at this call's own ready boundary (idle bound 1).
        session.run_shell("c1".into(), "$[0]", 5, &emit);
        let after_prune_call = session.log.lock().event_count();
        let pruned = crate::bus::drain_records(&rx).into_iter().any(|record| {
            matches!(
                record,
                Record::Display(Display::Notice {
                    notice: crate::record::NoticeFact::Prune { .. },
                })
            )
        });
        assert!(pruned, "the prune notice must have fired on the bus");

        session.run_shell("c2".into(), "$[0]", 5, &emit);
        let after_plain_call = session.log.lock().event_count();
        assert_eq!(
            after_prune_call - after_bind,
            after_plain_call - after_prune_call,
            "a binding prune must never write a model-view record.jsonl entry"
        );
    }
}
