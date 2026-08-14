//! The one loop every node runs: pull the next item off the node's own inbox,
//! take it up, do the ready-boundary housekeeping, and turn a nudge-worthy
//! outcome into a self-posted nudge.  Stepping the model to quiescence over
//! one item is [`super::deliberate`]'s job; nothing here special-cases a
//! node's position, since a child's result is delivered up its parent's
//! mailbox by the spawn site rather than by this loop.

use crate::agent::cancel;
use crate::agent::digest::PRESSURE_THRESHOLD_FALLBACK;
use crate::agent::event::QuiesceReason;
use crate::agent::nudge;
use crate::agent::{Agent, deliberate, panic_msg, render_reply};
use crate::bus::{AgentOutcome, AgentState, Emitter, Item, ParkMode, Post, WORKER_PANIC_PREFIX};
use crate::provider::{Provider, ProviderError};
use crate::shell_eval;
use ral_core::serial::FOValue;

/// What the surrounding loop does next: `Stop` on `/quit` or a terminal `reply`.
enum Flow {
    Continue,
    Stop,
}

/// ral calls between disk-warn ceiling checks: the walk is a full scan of the
/// session log dir and scratch, too costly to pay at every ready boundary.
const DISK_WARN_CHECK_INTERVAL: u64 = 32;

/// What [`Agent::attend`] does once [`Control`] has run a slash command.
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
    fn command(&mut self, raw: &str, session: &mut Agent, emit: &Emitter) -> Verdict;
}

/// The control for drivers with no slash commands (headless, peers, tests),
/// where an [`Item::Command`] never arises.
pub struct NoControl;

impl Control for NoControl {
    fn command(&mut self, _raw: &str, _session: &mut Agent, _emit: &Emitter) -> Verdict {
        Verdict::Continue
    }
}

impl Agent {
    /// The one attend loop, identical for every node: pull the next inbox
    /// item, take it up, and repeat until an empty inbox meets a
    /// [`Self::park_mode`] that does not park, or a cancel ends one that does.
    /// The returned `payload` is the faithful [`FOValue`] a `reply` carried,
    /// left for the consuming edge to render.
    ///
    /// # Panics
    /// Panics if the inbox rejects a reactive nudge, which the idempotent
    /// nudge protocol makes impossible.
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
        // Only the trunk publishes to the OS-signal slot; a sub-agent's token
        // is reached through the registry cascade instead.
        let _slot = self.parent.is_none().then(|| cancel::publish(&self.cancel));
        let mut final_outcome = (AgentOutcome::Empty, None);
        loop {
            // Every pass is a settled ready boundary.  A lease-chain reap is
            // deliberately not drained here: core pushes its `` `notice `` on
            // the surface stream of the run that observes it, so a reap during
            // a long idle surfaces once an item next runs.
            self.reconcile_service_pins();
            self.check_disk_warn(emit);
            // The state a frontend shows over the coming silence is this park's
            // own verdict.  Only on an empty queue: with an item already in
            // hand the agent is not idle for any observable moment, and the
            // deliberation's own transitions are the truth.
            if self.inbox.is_empty() {
                self.recorder()
                    .transient(crate::record::Transient::State(idle_state(policy(
                        self,
                        self.park_mode(),
                    ))));
            }
            // `next_or_idle` recomputes the park verdict on every wake.  An
            // un-engaged idle returning agent breaks here, yet its outcome
            // reaches its parent through the worker epilogue, not this break.
            let Some(item) = self
                .inbox
                .next_or_idle(|| policy(self, self.park_mode()), &self.cancel)
            else {
                break;
            };
            if matches!(
                self.take_up(&item, control, emit, &mut final_outcome),
                Flow::Stop
            ) {
                break;
            }
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
        // A child is removed by its spawn site through `settle`; the trunk has
        // nobody to do it, so it removes itself and the fleet empties.
        if self.parent.is_none() {
            self.agents.deregister(self.id);
        }
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
        let mut final_outcome = (AgentOutcome::Empty, None);
        loop {
            self.reconcile_service_pins();
            self.check_disk_warn(emit);
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
        if !self.log.lock().is_ready() {
            self.log.lock().quiesce(QuiesceReason::Aborted);
        }
        final_outcome
    }

    /// Take up one item already drawn from the inbox — the per-item step
    /// [`Self::attend`] and [`Self::attend_backlog`] share.  `final_outcome`
    /// is the running `(outcome, payload)` the caller reports once its own
    /// loop ends; an item that never reaches the deliberation (an inadmissible
    /// generation, a command) leaves it as it was.
    fn take_up(
        &mut self,
        item: &Item,
        control: &mut dyn Control,
        emit: &Emitter,
        final_outcome: &mut (AgentOutcome, Option<FOValue>),
    ) -> Flow {
        // Refused before it can reset the latches below or enter the log.
        if !self.admits(item) {
            return Flow::Continue;
        }
        // Only a genuine boundary clears the latches and a prior exchange's
        // Esc; a self-nudge is the same exchange continuing.
        if item.opens_exchange() {
            if let Some(nudges) = &mut self.nudges {
                nudges.reset();
            }
            self.cancel.reset();
        }
        if let Item::Command(raw) = item {
            return match control.command(raw, self, emit) {
                Verdict::Quit => Flow::Stop,
                Verdict::Continue => Flow::Continue,
            };
        }
        announce(item, emit, &self.recorder());
        // Read once, so a `/model` swap on the UI thread lands on the next
        // item rather than mid-item.
        let active = self.provider.current();
        let token = self.cancel.clone();
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
        *final_outcome = agent_outcome(&outcome);
        // Before any nudge decision, and whether or not one follows: a chat
        // trunk keeps no registry, and its failures must still reach the human.
        if let Err(e) = &outcome
            && let Err(error) = self.log.lock().record_provider_error(e)
        {
            eprintln!("exarch: a provider error was not recorded: {error}");
        }
        let waiting_on_children = self.has_live_children();
        // `last_input` is fresh off `deliberate`, so the gauge reads this
        // completion's own pressure, not the one it was called with.
        let pressure = self.context_pressure(&active);
        let ctx = nudge::NudgeCtx {
            // Exempt while children are live: an un-replied finish there is a
            // legitimate wait on the fleet, not a dropped return, and nagging
            // would push the agent to return before its agents land.
            must_reply: self.returns() && !waiting_on_children,
            pinned: self.pinned_digest(),
            waiting_on_children,
            is_headless_root: self.parent.is_none() && !self.interactive,
            pressure,
        };
        let replied = matches!(outcome, Ok(deliberate::Outcome::Replied(_)));
        let nudge_msg = match &mut self.nudges {
            Some(nudges) => nudges.react(&outcome, &ctx, &mut self.log.lock()),
            None => None,
        };
        if let Some(msg) = &nudge_msg
            && let Some(exchange) = self.log.lock().current_exchange()
        {
            self.inbox
                .push(Post::Nudge {
                    exchange,
                    text: msg.clone(),
                })
                .expect("Nudge is idempotent and never rejects");
        }
        // Latch only once the warning actually rode a message out: on the
        // must-reply budget-exhausted path `react` returns `None` despite
        // `ctx.pressure` being `Some`, and that warning went undelivered, so
        // it must stay unlatched to retry on the very next completion.
        if ctx.pressure.is_some()
            && nudge_msg.is_some()
            && matches!(outcome, Ok(deliberate::Outcome::Complete(_)))
        {
            self.context_warn_latched = true;
        }
        // `reply` hard-terminates, engagement and armed schedules
        // notwithstanding — unless the nudge layer just turned this very one
        // back for self-verification, which the loop must stay alive to carry.
        if replied && nudge_msg.is_none() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    /// Whether a drawn item still belongs to the live context.  A worker posts
    /// its result *before* retiring its registry entry (deliver-then-retire,
    /// so a parked parent never sees "no live child" without the result
    /// already queued), and a deferred `spawn`'s surface batch composes and
    /// pushes in two steps a `/clear` can fall between: neither can judge its
    /// own staleness, so both stamp a birth generation for this consuming edge
    /// to check.  A `ScheduledWakeup` is settled earlier, at the inbox's own
    /// pop boundary against that inbox's clear-epoch; every other source is
    /// generation-free.  The `Surface` id is a debug assertion, not an
    /// admission rule — the deferred sink stamps and posts to the very session
    /// that drains it, so a mismatch could only be a routing bug.
    pub(super) fn admits(&self, item: &Item) -> bool {
        match item {
            Item::Agent(r) => r.generation == self.agents.generation(),
            Item::Surface { id, generation, .. } => {
                debug_assert_eq!(
                    *id, self.id,
                    "a spawn's surface batch always drains in the session it was stamped with"
                );
                *generation == self.agents.generation()
            }
            _ => true,
        }
    }

    /// How an empty inbox should be treated, recomputed on every wake.  A
    /// conversing agent parks [`ParkMode::Held`], immune to cancellation; a
    /// returning agent a human has exchanged with parks [`ParkMode::Engaged`]
    /// — the same wait, but a terminate-cause cancel still ends it, and the
    /// registry's idle lease rather than this predicate bounds it.
    fn park_mode(&self) -> ParkMode {
        let conversing = self.interactive && !self.returns();
        if conversing {
            // `/clear` reaps the entry, and an unlisted agent is unreachable —
            // its mailbox resolves to None — so Held would be a zombie.
            if !self.agents.is_live(self.id) {
                return ParkMode::Quiesce;
            }
            return ParkMode::Held;
        }
        // No `is_live` guard, unlike `Held` above: every entry-removal path for
        // a parented agent terminate-stamps its token first
        // (`cancel_and_remove`), and `next_or_idle` checks `terminated()` on
        // every wake.
        if self.parent.is_some() && self.returns() && self.agents.engaged(self.id) {
            return ParkMode::Engaged;
        }
        if self.has_live_children() {
            return ParkMode::HeldByChildren;
        }
        if self.schedules.armed() {
            return ParkMode::UntilCancelled;
        }
        ParkMode::Quiesce
    }

    /// Whether an async child launched from here has yet to settle its result
    /// up this inbox.
    fn has_live_children(&self) -> bool {
        self.agents.has_children(self.id)
    }

    /// Re-pin the host-owned `services` card against the shell's live worker
    /// registry, dropping it the moment no durable service remains.  The sole
    /// writer: `shell_eval::reject_protected_pin` refuses the key to the model.
    fn reconcile_service_pins(&self) {
        let recorder = self.recorder();
        let key = || shell_eval::SERVICES_PIN_KEY.to_string();
        let live: Vec<crate::agent::ProbedWorker> = self
            .probe_workers()
            .into_iter()
            .filter(|entry| entry.class == ral_core::types::LeaseClass::Durable)
            .collect();

        if live.is_empty() {
            let had_one = self
                .pins
                .lock()
                .expect("pin register poisoned")
                .remove(shell_eval::SERVICES_PIN_KEY)
                .is_some();
            if had_one {
                if let Err(error) = recorder.emit(crate::record::Forensic::Unpin { key: key() }) {
                    recorder.report_fault(&error);
                }
                recorder.transient(crate::record::Transient::Unpin { key: key() });
            }
            return;
        }

        let card = crate::bus::card::services_pin_card(&live);
        self.pins.lock().expect("pin register poisoned").insert(
            shell_eval::SERVICES_PIN_KEY.to_string(),
            shell_eval::PinDigest::new(card.clone()),
        );
        if let Err(error) = recorder.emit(crate::record::Forensic::Pin { key: key() }) {
            recorder.report_fault(&error);
        }
        recorder.transient(crate::record::Transient::Pin { key: key(), card });
    }

    /// The rendered pressure-gauge detail once the soft line ahead of
    /// auto-compaction ([`crate::agent::digest::pressure_due`]) is crossed, or
    /// `None` while the excursion is already latched or the line has not been
    /// reached.  Mirrors [`Self::compact`]'s own two-armed match, one reserve
    /// earlier: a known window weighs `last_input` against it, an unknown one
    /// falls back to the byte heuristic.  A clean reading re-arms the next
    /// pressure crossing — but only once the measure is known: a stale
    /// measure relieves nothing, so it must not unlatch a warning still owed.
    fn context_pressure(&mut self, provider: &Provider) -> Option<String> {
        let pressure = match provider.context_window() {
            Some(w) if w > 0 => {
                let pressure = self.token_pressure(w);
                if pressure.is_none() && self.measured_input().is_some() {
                    self.context_warn_latched = false;
                }
                pressure
            }
            _ => {
                let bytes = self.log.lock().history_bytes();
                let pressure =
                    (bytes >= PRESSURE_THRESHOLD_FALLBACK).then(|| format!("{} KB", bytes / 1024));
                if pressure.is_none() {
                    self.context_warn_latched = false;
                }
                pressure
            }
        };
        if self.context_warn_latched {
            None
        } else {
            pressure
        }
    }

    /// Warn once per excursion above the operator's disk-warn ceiling, as an
    /// operational note ([`Kind::SystemNote`]) — nothing is ever rotated or
    /// deleted.  Unconfigured it walks nothing at all; otherwise it walks on
    /// the [`Self::ral_epoch`] cadence of [`DISK_WARN_CHECK_INTERVAL`].
    fn check_disk_warn(&mut self, emit: &Emitter) {
        let Some(ceiling) = self.disk_warn_bytes else {
            return;
        };
        if self.ral_epoch < self.disk_check_epoch {
            return;
        }
        self.disk_check_epoch = self.ral_epoch + DISK_WARN_CHECK_INTERVAL;
        let mut total = crate::agent::resources::dir_size(self.log.lock().dir());
        if let Some(scratch) = self.probe_env_var("EXARCH_SCRATCH") {
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
                    emit,
                );
            }
        } else {
            self.disk_warn_latched = false;
        }
    }
}

/// Emit the chrome an item's source shows as it enters context.  A nudge is an
/// internal continuation and a command never reaches the model, so both are quiet.
pub(super) fn announce(item: &Item, emit: &Emitter, recorder: &crate::record::Emitter) {
    match item {
        Item::Human(_) | Item::Wakeup(_) | Item::Message(_) => {
            // The live row derives from the published record — the retired
            // `Kind::UserPromptEcho` twin — so this is the one authoring site.
            record_commit(recorder, crate::record::Display::Prompt { text: item.text() });
        }
        Item::Agent(r) => {
            // The record carries the breadcrumb-reduced pair the scrollback
            // block is built from, not the raw outcome enum.
            let (text, error) = r.outcome.breadcrumb(&r.text);
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
                if let Some(surface) = shell_eval::accepted_surface(v, emit) {
                    if let Err(error) =
                        crate::fleet::desk::absorb_surface(&mut buf, recorder, *id, &surface)
                    {
                        recorder.report_fault(&error);
                    }
                    emit.emit(surface.into_kind());
                }
            }
            if let Err(error) = buf.flush_surfaces(recorder) {
                recorder.report_fault(&error);
            }
        }
        Item::Nudge { .. } | Item::Command(_) => {}
    }
}

/// Record one display commit beside its legacy `Kind` emission, surfacing a
/// failed append as a `Transient::Fault` exactly as the surface path does.
fn record_commit(recorder: &crate::record::Emitter, commit: crate::record::Display) {
    if let Err(error) = recorder.emit(commit) {
        recorder.report_fault(&error);
    }
}

/// [`Agent::attend_with`]'s park policy for
/// [`crate::headless::converse_settled`]: a conversing trunk's `park_mode`
/// always answers [`ParkMode::Held`], blind to its fleet, since a chat trunk
/// never used to have one worth waiting on. This reshapes exactly that
/// answer — live children hold as [`ParkMode::HeldByChildren`] instead, and a
/// childless trunk quiesces at once rather than parking on a human who is not
/// there to type. Every other verdict passes through unchanged: `Engaged`
/// requires a `steer` synod never offers, and `UntilCancelled` requires an
/// armed schedule `converse_settled` already refused at construction.
pub(crate) fn quiesce_when_childless(agent: &Agent, mode: ParkMode) -> ParkMode {
    match mode {
        ParkMode::Held if agent.has_live_children() => ParkMode::HeldByChildren,
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
fn agent_outcome(
    r: &Result<deliberate::Outcome, ProviderError>,
) -> (AgentOutcome, Option<FOValue>) {
    match r {
        // A unit or empty-string reply renders to nothing: a deliberate empty
        // return rather than a missing one.
        Ok(deliberate::Outcome::Replied(v)) if !render_reply(v).is_empty() => {
            (AgentOutcome::Complete, Some(v.clone()))
        }
        Ok(deliberate::Outcome::Replied(_)) => (AgentOutcome::Empty, None),
        // Re-nudged within budget by `nudge` before it ever reaches here.
        Ok(deliberate::Outcome::Complete(_) | deliberate::Outcome::Empty) => (
            AgentOutcome::Failed("ended without calling `reply`".into()),
            None,
        ),
        Ok(deliberate::Outcome::Stopped { reason }) => {
            (AgentOutcome::Stopped(reason.clone()), None)
        }
        Ok(deliberate::Outcome::Cancelled) => (AgentOutcome::Cancelled, None),
        Ok(deliberate::Outcome::Capped) => (AgentOutcome::Stopped("step cap reached".into()), None),
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
    use crate::agent::ProviderHandle;
    use crate::agent::testkit::*;
    use crate::bus::Kind;
    use crate::record::{Forensic, Record};
    use crate::fleet::registry::{AGENT_LEASE_IDLE, Registration};
    use crate::provider::scripted::{Reply, Script};
    use ral_core::Value;

    /// Every item `announce` draws records its display commit through the
    /// seam, with no direct legacy `Kind` twin of its own: a prompt's live
    /// row is `record_kind`'s derivation of `Display::Prompt`, and a
    /// subagent's breadcrumb is `Display::SubagentDone` alone — the TUI's
    /// fact fold routes it to root without any `Kind::SubagentDone` bridge.
    #[test]
    fn announce_records_display_facts_with_no_legacy_kind_twin() {
        use crate::bus::{AgentResult, Signal};
        use crate::record::Display;

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx.clone(), 0);
        let recorder = crate::record::Emitter::none();
        recorder.attach(crate::record::FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });

        announce(&Item::Human("hello".into()), &emit, &recorder);
        announce(
            &Item::Agent(AgentResult {
                name: "helper".into(),
                outcome: AgentOutcome::Failed("boom".into()),
                text: String::new(),
                elapsed: std::time::Duration::from_millis(1500),
                generation: 0,
            }),
            &emit,
            &recorder,
        );

        let mut facts: Vec<&'static str> = Vec::new();
        let mut derived: Vec<&'static str> = Vec::new();
        while let Ok(sig) = rx.try_recv() {
            match sig {
                Signal::Event(_) => panic!("neither class carries a direct legacy Kind"),
                Signal::Fact(id, fact) => {
                    match fact.value() {
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
                            assert_eq!(*elapsed_ms, 1500);
                            facts.push("subagent");
                        }
                        _ => {}
                    }
                    let is_prompt = matches!(fact.value(), Record::Display(Display::Prompt { .. }));
                    if is_prompt {
                        let event = Signal::Fact(id, fact)
                            .into_event()
                            .expect("record_kind derives a prompt fact into Kind::UserPromptEcho");
                        match event.kind {
                            Kind::UserPromptEcho(text) => {
                                assert_eq!(text, "hello");
                                derived.push("prompt");
                            }
                            _ => panic!("expected Kind::UserPromptEcho"),
                        }
                    }
                }
                Signal::Transient(..) => {}
            }
        }
        assert_eq!(facts, ["prompt", "subagent"], "both commits reach the seam");
        assert_eq!(derived, ["prompt"], "the prompt's live row derives from its fact");
    }

    /// The park verdict reads engagement from the registry, never from the
    /// TUI's focus cursor.
    #[test]
    fn park_mode_reads_engagement_from_the_registry() {
        let dir = tmp("park-engaged");
        let held = trunk(true);
        assert_eq!(held.park_mode(), ParkMode::Held);

        let parent = Agent::for_test("system").unwrap();
        let child = parent.fork(parent.caps().clone()).expect("fork child");
        child
            .agents
            .register(Registration {
                id: child.id,
                parent: Some(parent.id),
                lease: Some(AGENT_LEASE_IDLE),
                name: "child".into(),
                log_dir: dir.path().join("child"),
                cancel: child.cancel_token().clone(),
                reach: child.seat.eval_reach(),
                mailbox: child.mailbox(),
                provider: child.provider_handle(),
            })
            .expect("child registration must succeed: its parent is live");
        assert_eq!(
            child.park_mode(),
            ParkMode::Quiesce,
            "un-engaged, no live children, no schedule: idle quiesce delivers the outcome"
        );

        child.agents.renew(child.id);
        assert_eq!(
            child.park_mode(),
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
        let emit = Emitter::new(tx, root.id);
        let root_result = root.run_shell("c1".into(), "reply 1", 5, &emit);
        let refusal = "you converse with the user; you do not return";
        assert!(
            root_result.content.contains(refusal),
            "got: {}",
            root_result.content
        );

        let mut branch = root.branch().expect("branch a conversing child");
        // A distinct name: `root` already holds `TRUNK_NAME` in this same
        // shared registry, and names are unique among live entries.
        branch.register_self_named("branch");
        let branch_result = branch.run_shell("c2".into(), "reply 1", 5, &emit);
        assert!(
            branch_result.content.contains(refusal),
            "a /branch child must be refused with the same text, got: {}",
            branch_result.content
        );
        assert_eq!(
            branch.park_mode(),
            ParkMode::Held,
            "a /branch child still parks Held after a refused reply"
        );
    }

    /// An un-replied finish is re-nudged within budget, then settles `Failed`
    /// — the final prose is never scraped as the answer.
    #[test]
    fn sub_agent_without_reply_is_re_nudged_then_fails() {
        let parent = Agent::for_test("system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("do the thing".into());
        // More prose-only replies than the budget will consume, so the test
        // does not couple to the exact budget.
        let mut script = Script::new();
        for _ in 0..8 {
            script = script.then(Reply::text("here is prose, but no reply"));
        }
        let (outcome, text) = drive_peer(&mut child, scripted("test-model", script));
        assert!(
            matches!(outcome, AgentOutcome::Failed(_)),
            "an un-replied finish settles Failed, got {outcome:?}"
        );
        assert!(
            text.is_empty(),
            "the final prose must not be scraped: {text:?}"
        );
        assert!(child.is_ready());
    }

    /// A host-side unwind — transport, decode, render — is recorded, fails the
    /// item, and leaves the log ready, so the next prompt still deliberates.
    /// The eval-side panics in `agent/shell.rs` never reach this arm:
    /// `Shell::run` rolls those back long before the loop sees them.
    #[test]
    fn host_panic_is_recorded_and_the_next_prompt_still_deliberates() {
        let mut session = Agent::for_test("system").unwrap();
        // The first prompt unwinds; the rest answer the second and the no-reply
        // nudges it draws.
        let mut script = Script::new().then(Reply::panicking());
        for _ in 0..8 {
            script = script.then(Reply::text("recovered"));
        }
        session.provider = ProviderHandle::new(scripted("test-model", script));

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
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
    fn dispatch_with_lease(session: &Agent, cmd: &str, lease: ral_core::types::WorkerLease) {
        use ral_core::transport::{DispatchId, Program, Run};
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
        );
    }

    /// A reap the lease chain performs between runs sits queued: core only
    /// pushes a `` `notice `` from inside a run's own surface stream, so it
    /// surfaces at the next run, and exactly once.
    #[test]
    fn ready_boundary_reap_notice_surfaces_at_the_next_run() {
        let mut session = Agent::for_test("system").unwrap();
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
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        session.run_shell("c0".into(), "return 1", 5, &emit);

        let mut notices = 0;
        while let Ok(event) = rx.try_next_event() {
            let Kind::Notice {
                notice: crate::bus::card::Notice::Reap { cmd, cause },
                ..
            } = event.kind
            else {
                continue;
            };
            notices += 1;
            assert_eq!(cmd, "<block>", "the reap must name the spawned body");
            assert_eq!(
                cause,
                ral_core::types::ReapCause::Idle,
                "an unpolled worker past its idle bound reaps as Idle"
            );
        }
        assert_eq!(notices, 1, "the queued reap surfaces exactly once");

        session.run_shell("c1".into(), "return 1", 5, &emit);
        while let Ok(event) = rx.try_next_event() {
            assert!(
                !matches!(event.kind, Kind::Notice { .. }),
                "a further run with nothing new queued must surface no second notice"
            );
        }
    }

    // ── the `services` pin ledger ─────────────────────────────────────────

    /// The `services` pin is born carrying the service's description, and
    /// retired the moment no durable service remains.
    #[test]
    fn reconcile_service_pins_births_and_retires_the_services_pin() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.run_shell(
            "c1".into(),
            r#"service "watch the thing" { test-clear-block-forever }"#,
            5,
            &emit,
        );

        session.reconcile_service_pins();
        {
            let pin = session
                .pins
                .lock()
                .unwrap()
                .get(shell_eval::SERVICES_PIN_KEY)
                .expect("the services pin must be born")
                .clone();
            let line = crate::bus::card::summary_line(&pin.card);
            assert!(
                line.contains("watch the thing"),
                "the description must render: {line}"
            );
        }
        let mut saw_pin = false;
        while let Ok(sig) = rx.try_recv() {
            if let crate::bus::Signal::Transient(_, crate::record::Transient::Pin { key, .. }) = sig
            {
                assert_eq!(key, shell_eval::SERVICES_PIN_KEY);
                saw_pin = true;
            }
        }
        assert!(saw_pin, "the birth must publish a Transient::Pin");

        // Through the ordinary `cancel` builtin — the same edge a model reaches.
        let entry = session
            .seat
            .shell_mut()
            .shell
            .workers()
            .pop()
            .expect("the service registered");
        let cancel_fn = session
            .seat
            .shell_mut()
            .shell
            .lookup_builtin("cancel")
            .expect("core registers cancel");
        cancel_fn
            .run(
                &[Value::Handle(entry.handle)],
                &ral_core::types::Mooring::adrift(),
                &mut session.seat.shell_mut().shell,
            )
            .expect("cancel must succeed");

        session.reconcile_service_pins();
        assert!(
            session
                .pins
                .lock()
                .unwrap()
                .get(shell_eval::SERVICES_PIN_KEY)
                .is_none(),
            "the services pin must be retired once no durable service remains"
        );
        let mut saw_unpin = false;
        while let Ok(sig) = rx.try_recv() {
            if let crate::bus::Signal::Transient(_, crate::record::Transient::Unpin { key }) = sig {
                assert_eq!(key, shell_eval::SERVICES_PIN_KEY);
                saw_unpin = true;
            }
        }
        assert!(saw_unpin, "retirement must publish a Transient::Unpin");
    }

    /// A forged `unpin` is refused with a diagnostic and leaves the register
    /// untouched — proof `run_shell`'s applier routes through the guard.  The
    /// write direction is covered at `shell_eval::reject_protected_pin`.
    #[test]
    fn program_cannot_write_the_services_pin() {
        let mut session = Agent::for_test("system").unwrap();
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        session.run_shell("c1".into(), r#"surface `unpin [key: "services"]"#, 5, &emit);

        let mut saw_error = false;
        while let Ok(event) = rx.try_next_event() {
            if let Kind::Error(msg) = event.kind {
                assert!(msg.contains("protected service-ledger pin"));
                saw_error = true;
            }
        }
        assert!(
            saw_error,
            "the forged pin must be rejected with a diagnostic"
        );
    }

    /// The reminder is built from the register the model actually wrote, and it
    /// becomes the next committed prompt: `pinned_digest` → `NudgeCtx` →
    /// `Post::Nudge` → the user turn the next deliberation sends.  The nudge
    /// unit tests hand `react` a hand-written digest; only this joins it to the
    /// live register.
    #[test]
    fn pinned_state_reminder_reads_the_live_register_and_becomes_the_next_prompt() {
        let mut session = Agent::for_test("system").unwrap();
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
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

        // A finish with nothing returned draws the reminder; the two `reply`
        // turns then satisfy the headless root's verification and end the loop.
        session.provider = ProviderHandle::new(scripted(
            "test-model",
            Script::new()
                .then(Reply::text("done"))
                .then(Reply::tool_calls(vec![ral_call("r1", "reply 'x'")]))
                .then(Reply::tool_calls(vec![ral_call("r2", "reply 'x'")])),
        ));
        session.seed("get on with it".into());
        session.attend(&mut NoControl, &emit);

        assert!(
            std::iter::from_fn(|| rx.try_next_event().ok()).any(
                |e| matches!(e.kind, Kind::Nudge { cause, .. } if cause == "pinned-state reminder")
            ),
            "the live register must raise a pinned-state nudge"
        );
        let view = serde_json::to_string(&session.rendered_messages()).unwrap();
        assert!(
            view.contains("There is pinned state: ship the reminder"),
            "the reminder must be committed as the next prompt, not dropped: {view}"
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
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.provider =
            ProviderHandle::new(scripted("test-model", Script::new().then(Reply::empty())));
        session.seed("hello".into());
        session.attend_backlog(&emit);

        assert!(
            !std::iter::from_fn(|| rx.try_next_event().ok())
                .any(|e| matches!(e.kind, Kind::Nudge { .. })),
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

    /// A boundary prune's `Kind::Notice` rides the surface stream of the call
    /// it fires inside — one notice naming the binding, none once nothing is
    /// left idle.  The tiny re-armed bound is for speed only.
    #[test]
    fn boundary_prune_notice_rides_the_runs_own_stream() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 2,
                large_binding_bytes: u64::MAX,
            });

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.run_shell("c0".into(), "let reap_me = 1", 5, &emit);
        session.run_shell("c1".into(), "$[0]", 5, &emit);
        session.run_shell("c2".into(), "$[0]", 5, &emit);

        let mut prunes = Vec::new();
        while let Ok(event) = rx.try_next_event() {
            if let Kind::Notice {
                notice: crate::bus::card::Notice::Prune { names, idle_calls },
                ..
            } = event.kind
            {
                prunes.push((names, idle_calls));
            }
        }
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
        while let Ok(event) = rx.try_next_event() {
            assert!(
                !matches!(
                    event.kind,
                    Kind::Notice {
                        notice: crate::bus::card::Notice::Prune { .. },
                        ..
                    }
                ),
                "nothing left idle — no second prune notice"
            );
        }
    }

    /// Unconfigured, the check returns before the epoch bookkeeping: no walk,
    /// no emission, no cost.
    #[test]
    fn check_disk_warn_unconfigured_never_walks_or_warns() {
        let mut session = Agent::for_test("system").unwrap();
        assert!(session.disk_warn_bytes.is_none());

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.check_disk_warn(&emit);

        assert_eq!(
            session.disk_check_epoch, 0,
            "the early return never advances the check epoch"
        );
        assert!(rx.try_recv().is_err(), "unconfigured: never emits, ever");
    }

    /// The latch suppresses a repeat until the figure falls back under.
    #[test]
    fn check_disk_warn_still_above_does_not_repeat() {
        let mut session = Agent::for_test("system").unwrap();
        let baseline = crate::agent::resources::dir_size(&session.log_dir());
        std::fs::write(session.log_dir().join("big.txt"), vec![0u8; 4096]).unwrap();
        session.disk_warn_bytes = Some(baseline + 100);

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.check_disk_warn(&emit);
        assert!(rx.try_recv().is_ok(), "the first crossing warns");

        // Force the amortization window open without driving real ral calls.
        session.disk_check_epoch = session.ral_epoch;
        session.check_disk_warn(&emit);
        assert!(
            rx.try_recv().is_err(),
            "still above the ceiling: the latch suppresses a repeat"
        );
    }

    /// Falling back under clears the latch, so a re-crossing warns again.
    #[test]
    fn check_disk_warn_falling_below_rearms_the_latch() {
        let mut session = Agent::for_test("system").unwrap();
        // The ceiling sits just above the session's own baseline, so the big
        // file alone decides whether it is crossed.
        let baseline = crate::agent::resources::dir_size(&session.log_dir());
        let big = session.log_dir().join("big.txt");
        std::fs::write(&big, vec![0u8; 4096]).unwrap();
        session.disk_warn_bytes = Some(baseline + 100);

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.check_disk_warn(&emit);
        let event = rx.try_next_event().expect("the first crossing warns");
        match event.kind {
            Kind::SystemNote(text) => assert!(text.contains("disk"), "{text}"),
            _ => panic!("expected Kind::SystemNote"),
        }

        std::fs::remove_file(&big).unwrap();
        session.disk_check_epoch = session.ral_epoch;
        session.check_disk_warn(&emit);
        assert!(
            rx.try_recv().is_err(),
            "back under the ceiling: no warning, just the latch clearing"
        );
        assert!(!session.disk_warn_latched, "the latch is cleared");

        std::fs::write(&big, vec![0u8; 4096]).unwrap();
        session.disk_check_epoch = session.ral_epoch;
        session.check_disk_warn(&emit);
        assert!(
            rx.try_recv().is_ok(),
            "re-crossing after falling below warns again"
        );
    }

    /// A binding prune is transcript/TUI-only: it must never grow the
    /// model-view `record.jsonl`.
    #[test]
    fn prune_does_not_add_model_events() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 1,
                large_binding_bytes: u64::MAX,
            });

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        session.run_shell("c0".into(), "let events_json_x = 1", 5, &emit);
        let after_bind = session.log.lock().event_count();

        // The prune fires at this call's own ready boundary (idle bound 1).
        session.run_shell("c1".into(), "$[0]", 5, &emit);
        let after_prune_call = session.log.lock().event_count();
        let pruned = std::iter::from_fn(|| rx.try_next_event().ok()).any(|event| {
            matches!(
                event.kind,
                Kind::Notice {
                    notice: crate::bus::card::Notice::Prune { .. },
                    ..
                }
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
