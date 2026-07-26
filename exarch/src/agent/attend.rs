//! The one loop every node in the fleet runs: pull the next inbox item off
//! the node's own [`Inbox`](crate::bus::Inbox), take it up
//! ([`Agent::take_up`]), do the ready-boundary housekeeping
//! ([`Agent::reconcile_service_pins`], [`Agent::check_disk_warn`]), and turn
//! a nudge-worthy outcome into a self-posted [`Post::Nudge`].
//!
//! Stepping the model to quiescence over one item is [`super::deliberate`]'s
//! job ([`Agent::deliberate`]); this module is what calls it, over and over,
//! once per item, until the node parks or terminates.  No node is privileged
//! by special-case code; the distinctions reduce to *position*: the
//! parent-less **trunk** publishes its cancel token for the OS-signal path,
//! holding `reply` falls out of the construction-fixed `returns` bit
//! (`!interactive` at the trunk, `true` for every fork), and parking out of
//! `interactive`/the registry's engagement read via [`Agent::park_mode`].  A
//! child's single result is delivered up its parent's mailbox by the spawn
//! site, not here, so [`Agent::attend`] itself is identical for all.
//! [`Agent::attend_backlog`] is the one exception: a converse session's
//! bounded, per-exchange twin, sharing every per-item step with `attend`
//! ([`Agent::take_up`]) rather than looping the round-trip a second way.
//!
//! [`Control`] is the frontend hook `attend` calls out to for a
//! session-affecting slash command drained at the exchange boundary; off the
//! TUI, [`NoControl`] answers none.

use crate::agent::cancel;
use crate::agent::event::QuiesceReason;
use crate::agent::nudge;
use crate::agent::{Agent, deliberate, panic_msg, render_reply};
use crate::bus::{AgentOutcome, Emitter, Item, Kind, ParkMode, Post, WORKER_PANIC_PREFIX};
use crate::provider::ProviderError;
use crate::shell_eval;
use ral_core::serial::FOValue;

/// What [`Agent::take_up`] tells its caller's loop to do next: `Stop`
/// on a `/quit` or a terminal `reply`, `Continue` for everything else —
/// shared by [`Agent::attend`]'s forever loop and [`Agent::attend_backlog`]'s
/// bounded one, which differ only in how they pull the next [`Item`].
enum Flow {
    Continue,
    Stop,
}

/// How many ral calls elapse between disk-warn ceiling checks, once
/// `disk_warn_bytes` is configured — the walk's cost is real (a full scan of
/// the session log dir and scratch) but rare enough at this cadence to run
/// at the ready boundary rather than off a timer.
const DISK_WARN_CHECK_INTERVAL: u64 = 32;

/// What [`Agent::attend`] should do after handing a session-affecting slash
/// command to [`Control`].
pub enum Verdict {
    Continue,
    Quit,
}

/// The frontend hook the attend loop calls for a session-affecting slash
/// command ([`Item::Command`]) drained at the exchange boundary.
///
/// The attend thread owns the session the command mutates, so it cannot run on
/// the UI thread.
/// Only the non-interactive session commands route here (`/clear`, `/compact`,
/// `/quit`); `/model` swaps the [`ProviderHandle`](crate::agent::ProviderHandle)
/// directly on the UI thread, and view-only commands (`/help`, `/copy`, …) are
/// handled frontend-side.  Off the TUI there are no such commands, so
/// [`NoControl`] handles none.
pub trait Control {
    /// Run `raw` (a slash-command line) against the session the attend loop
    /// owns, returning whether the loop should quit.
    fn command(&mut self, raw: &str, session: &mut Agent, emit: &Emitter) -> Verdict;
}

/// The no-op control for drivers with no slash commands (headless, peers,
/// tests): an [`Item::Command`] should never reach them, so it is a no-op.
pub struct NoControl;

impl Control for NoControl {
    fn command(&mut self, _raw: &str, _session: &mut Agent, _emit: &Emitter) -> Verdict {
        Verdict::Continue
    }
}

impl Agent {
    /// The one shared attend loop — every node alike.  Pull the next inbox
    /// item, take it up ([`Agent::take_up`]), let the nudge registry post any
    /// retry as a self-[`Post::Nudge`], and repeat.  An empty inbox parks or
    /// terminates by [`Self::park_mode`]: a present human (the conversing
    /// trunk or an engaged returning agent) holds it; a live schedule holds it
    /// until cancelled; otherwise it terminates at quiescence.
    ///
    /// Per-item panic recovery lives here — the [`std::panic::catch_unwind`] records the
    /// failure and continues the loop, so one crashing deliberation never
    /// sinks the agent; eval-side state is already safe, rolled back at the
    /// engine's own run door ([`ral_core::Shell::run`]).  Returns the final
    /// `(outcome, payload)`, where `payload` is the faithful [`FOValue`] a
    /// `reply` carried (else `None`); the interactive trunk ignores it, a
    /// child's spawn site renders it to text, and the headless sink projects
    /// it through [`shell_eval::user_json`].
    ///
    /// The provider is the agent's own [`ProviderHandle`](crate::agent::ProviderHandle),
    /// read once per item, so a `/model` swap on this agent takes effect on
    /// the next one; `control` handles agent-affecting slash commands
    /// ([`NoControl`] off the TUI).
    ///
    /// # Panics
    /// Panics if pushing a reactive nudge onto the inbox is rejected, which
    /// the idempotent nudge protocol guarantees cannot occur.
    pub fn attend(
        &mut self,
        control: &mut dyn Control,
        emit: &Emitter,
    ) -> (AgentOutcome, Option<FOValue>) {
        // The trunk publishes its sticky token for the OS-signal path, held for
        // the whole attend; a sub-agent's token is reached through the registry
        // cascade, never the slot, so it publishes nothing.
        let _slot = self.parent.is_none().then(|| cancel::publish(&self.cancel));
        let mut final_outcome = (AgentOutcome::Empty, None);
        loop {
            // Every pass back to the loop top is a settled ready boundary
            // (the per-iteration quiesce below, or the invariant `attend` is
            // entered under). A lease-chain reap between items is not drained
            // here: core's own engine pushes it as a `` `notice `` surface
            // class at the ready boundary of the run that produced it, so it
            // becomes visible only at the next dispatched run, not on every
            // idle pass through this loop — a worker reaped while this agent
            // sits fully idle surfaces once an item next runs, not the
            // instant the reap happens.
            //
            // The service ledger's sibling boundary drain: the `services`
            // pin is (re-)born or dies at this pass.
            self.reconcile_service_pins(emit);
            // The disk-warn check's own sibling: amortized on the same ral
            // epoch, a no-op walk-wise when unconfigured.
            self.check_disk_warn(emit);
            // The park verdict ([`Self::park_mode`]) is recomputed on every
            // wake inside `next_or_idle`: an un-engaged idle returning agent
            // reads `Quiesce` here and the loop breaks — its outcome still
            // reaches its parent through the worker epilogue's
            // deliver-then-retire, never through this break itself.
            let Some(item) = self.inbox.next_or_idle(|| self.park_mode(), &self.cancel) else {
                break;
            };
            if matches!(
                self.take_up(&item, control, emit, &mut final_outcome),
                Flow::Stop
            ) {
                break;
            }
        }
        // Safety net: per-iteration quiescing (inside `take_up`) handles
        // mid-loop errors, but a `break` from `Replied` or a future exit path
        // could still bypass it.  This catch-all ensures the exchange-ends-ready
        // invariant holds regardless of how the loop exits.
        if !self.log.lock().is_ready() {
            self.log.lock().quiesce(QuiesceReason::Aborted);
        }
        debug_assert!(
            self.log.lock().is_ready(),
            "attend must leave the agent ReadyForUser"
        );
        // The trunk removes itself at termination (a child is removed by its
        // spawn site through `settle`), so the fleet empties when the last
        // agent is gone.
        if self.parent.is_none() {
            self.agents.deregister(self.id);
        }
        final_outcome
    }

    /// The converse primitive's per-exchange half of [`Self::attend`]:
    /// process every item already queued — the message
    /// [`exarch::converse`](crate::headless::converse) just seeded, and
    /// whatever nudge continuations it raises — to the next park, then
    /// return, instead of blocking for the next message the way `attend`
    /// does.  Shares [`Agent::take_up`] with `attend` rather than looping
    /// the round-trip a second way; the only difference is the non-blocking
    /// pull ([`Inbox::next_item`](crate::bus::Inbox::next_item)) in place
    /// of `attend`'s parking one.  Safe because a converse session's trunk
    /// starts no children (`fuel: 0`) and arms no self-schedule
    /// (`allow_schedule: false`), so an empty queue always means *this*
    /// exchange is answered, never a wait on something still to arrive. No
    /// [`Control`]: a slash-shaped user message still reaches the model as
    /// ordinary text ([`Item::Command`] only ever arises from a frontend
    /// that posts one itself, which converse never does), so [`NoControl`]
    /// is correct here, not a placeholder for one the caller forgot to
    /// supply.
    pub(crate) fn attend_backlog(&mut self, emit: &Emitter) -> (AgentOutcome, Option<FOValue>) {
        let mut control = NoControl;
        let mut final_outcome = (AgentOutcome::Empty, None);
        loop {
            self.reconcile_service_pins(emit);
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

    /// Process one item already drawn from the inbox: generation
    /// admission, the exchange-boundary latch reset, a session command's
    /// dispatch to `control`, the deliberation itself, and the nudge
    /// reaction. The one per-item step both [`Self::attend`]'s forever loop
    /// and [`Self::attend_backlog`]'s bounded one run — the two never repeat
    /// this logic independently. `final_outcome` carries the running
    /// `(outcome, payload)` the caller reports once its own loop ends; an
    /// item this returns [`Flow::Continue`] for without reaching the
    /// deliberation (an inadmissible generation, a command) leaves it
    /// untouched.
    fn take_up(
        &mut self,
        item: &Item,
        control: &mut dyn Control,
        emit: &Emitter,
        final_outcome: &mut (AgentOutcome, Option<FOValue>),
    ) -> Flow {
        // Generation admission: a worker delivers before it retires, so a
        // result that settled across a `/clear` can reach this queue; it
        // is dropped here, before it can reset latches or enter the log.
        if !self.admits(item) {
            return Flow::Continue;
        }
        // A genuine exchange boundary resets the nudge latches and clears the
        // sticky cancel token, so a prior exchange's Esc cannot carry into
        // this one.  A self-nudge is the same exchange continuing and resets
        // neither.
        if item.opens_exchange() {
            self.nudges.reset();
            self.cancel.reset();
        }
        // Agent-affecting slash commands run against the owned agent,
        // never the model.
        if let Item::Command(raw) = item {
            return match control.command(raw, self, emit) {
                Verdict::Quit => Flow::Stop,
                Verdict::Continue => Flow::Continue,
            };
        }
        announce(item, emit);
        // Read the provider in force for this item; a `/model` swap on this
        // agent lands here on the next item, never mid-item.
        let active = self.provider.current();
        let token = self.cancel.clone();
        let prompt = Some(item.text());
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.deliberate(&active, prompt, &token, emit)
        }));
        let outcome = match attempt {
            Ok(o) => o,
            Err(p) => {
                // A host-side panic — provider transport, surface decode,
                // render, outcome.  The shell needs no attention here: an
                // eval-side panic is caught at the engine's own run door
                // (`Shell::run`), which rolls the dynamic state back
                // and reports a failed run, so it never unwinds this far.
                let msg = panic_msg(&p);
                self.note_error(format!("{WORKER_PANIC_PREFIX}{msg}"), emit);
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
        // A provider error or step cap can leave the session
        // mid-protocol (e.g. AwaitingAssistantAfterToolResults).
        // The caller's post-loop guard does the same but only on loop exit;
        // quiesce per-item so the next prompt — nudge or user — is
        // admissible (exchange-ends-ready invariant, X12).
        if !self.log.lock().is_ready() {
            self.log.lock().quiesce(QuiesceReason::Aborted);
        }
        *final_outcome = agent_outcome(&outcome);
        let waiting_on_children = self.has_live_children();
        let ctx = nudge::NudgeCtx {
            // Every returning agent (a sub-agent or a headless trunk) hands
            // its value back through `reply`; an un-replied finish is
            // re-nudged within budget, then fails.  Only the interactive
            // trunk, which converses and never returns, is exempt — and so,
            // transiently, is any agent with live children: an un-replied
            // finish there is not a dropped return but a legitimate wait on
            // its fleet, and `park_mode` holds it (`HeldByChildren`) until a
            // child's result wakes it.  Nagging it toward `reply` now would
            // push it to return before its agents land; once they have all
            // settled the nudge resumes and insists on `reply` as before.
            must_reply: self.returns() && !waiting_on_children,
            pinned: self.pinned_digest(),
            waiting_on_children,
            is_headless_root: self.parent.is_none() && !self.interactive,
        };
        let replied = matches!(outcome, Ok(deliberate::Outcome::Replied(_)));
        let nudge_msg = self
            .nudges
            .react(&outcome, &ctx, emit, &mut self.log.lock());
        if let Some(msg) = &nudge_msg {
            self.inbox
                .push(Post::Nudge(msg.clone()))
                .expect("Nudge is idempotent and never rejects");
        }
        // `reply` hard-terminates: end the loop regardless of engagement or
        // a self-armed schedule — the agent returns its value and is gone.
        // Unless the nudge layer just turned this very `reply` back for
        // self-verification (a headless root's first call), in which case
        // `nudge_msg` carries that reminder and the loop must keep running
        // to deliver it.
        if replied && nudge_msg.is_none() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    /// Generation admission for a drawn item.  An
    /// [`AgentResult`](crate::bus::AgentResult) is stamped with its worker's
    /// birth generation, and the worker posts it *before* retiring its
    /// registry entry (deliver-then-retire, so a parked parent can never
    /// observe "no live child" without the result already queued) — which
    /// means the worker cannot check staleness itself.  A deferred `spawn`'s
    /// surface batch ([`crate::shell_eval::InboxDeferred`]) carries the same
    /// birth generation for the identical reason: its compose and its push are
    /// two separate steps a `/clear` can fall between, so it cannot decide its
    /// own staleness either.  The consuming edge decides both: a generation
    /// that predates the live registry generation settled across a `/clear`
    /// and belongs to a context that no longer exists.  A `ScheduledWakeup`
    /// is checked earlier, at the inbox's own pop boundary
    /// ([`crate::bus::pop_item`]/[`crate::bus::to_item`]), against that inbox's local
    /// clear-epoch rather than this fleet-wide registry generation — its only
    /// producer (the schedule reaper) never has a handle to this registry, so
    /// staleness is settled before an [`Item`] is even minted; by the time one
    /// reaches here it is already current.  Every other item source is
    /// generation-free and admitted unconditionally.
    ///
    /// A `Surface`'s stamped `id` is checked here too, as a debug assertion
    /// rather than an admission rule: the deferred sink always stamps it
    /// with, and posts it to, the very session that goes on to drain it, so
    /// the id and `self.id` can never disagree without a routing bug — this
    /// only catches a future one.
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

    /// The `park_mode` predicate ([`ParkMode`]), recomputed on every wake.  A
    /// *conversing* agent (interactive and holding no `reply`, so it never
    /// returns) parks [`ParkMode::Held`], immune to cancellation; a returning
    /// agent a human has exchanged a message with instead parks
    /// [`ParkMode::Engaged`] — the same wait, but a terminate-cause cancel
    /// still ends it, since an exchange is not a conversation and the
    /// registry's idle lease, not this predicate, bounds how long it lasts.
    /// Live children it launched hold it until they settle
    /// ([`ParkMode::HeldByChildren`]) so a headless root waiting on its fleet
    /// stays alive to receive their results; a live self-schedule holds it
    /// until cancelled; otherwise it terminates at quiescence.
    fn park_mode(&self) -> ParkMode {
        let conversing = self.interactive && !self.returns();
        if conversing {
            // A conversing agent lives exactly as long as the fleet lists it:
            // /clear reaps its entry, and an unlisted agent is unreachable (its
            // mailbox resolves to None), so parking Held would be a zombie.
            if !self.agents.is_live(self.id) {
                return ParkMode::Quiesce;
            }
            return ParkMode::Held;
        }
        // No `is_live` guard, unlike `Held` above: every entry-removal path
        // for a parented agent terminate-stamps its token first
        // (`cancel_and_remove`), and `next_or_idle` checks `terminated()` on
        // every wake, so an unlisted engaged agent still exits at once.
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

    /// Whether this agent has a live child still running — an async child it
    /// launched that has not yet settled its result up this inbox.  Read by
    /// [`Self::park_mode`] (so a root waiting on its fleet parks rather than
    /// quiescing) and by [`Self::attend`] (so the `reply` nudge is not raised
    /// against a finish that is a legitimate wait, not a dropped return).
    fn has_live_children(&self) -> bool {
        self.agents.has_children(self.id)
    }

    /// Reconcile the host-owned `services` pin against the shell's live
    /// worker registry at the attend loop's ready-boundary pass: one card lists every currently-running durable
    /// service, re-pinned whenever at least one is alive; the pin drops the
    /// moment none remain (cancelled, or settled and reaped). The model
    /// cannot write or clear this pin itself —
    /// [`shell_eval::reject_protected_pin`] refuses that key — so this is the one writer.
    fn reconcile_service_pins(&self, emit: &Emitter) {
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
                emit.emit(Kind::Unpin {
                    key: shell_eval::SERVICES_PIN_KEY.to_string(),
                });
            }
            return;
        }

        let card = crate::bus::card::services_pin_card(&live);
        self.pins.lock().expect("pin register poisoned").insert(
            shell_eval::SERVICES_PIN_KEY.to_string(),
            shell_eval::PinDigest::new(card.clone()),
        );
        emit.emit(Kind::Pin {
            key: shell_eval::SERVICES_PIN_KEY.to_string(),
            card,
        });
    }

    /// Warn once per excursion above the operator's disk-warn ceiling,
    /// through the operational-note vocabulary ([`Kind::SystemNote`]) — never
    /// rotates or deletes. A no-op by construction when unconfigured: no
    /// walk, no warning, ever. Amortized to once every
    /// [`DISK_WARN_CHECK_INTERVAL`] ral calls (tracked on the same
    /// [`Self::ral_epoch`] the settled-worker and binding-lease sweeps read)
    /// so the walk's cost — a full scan of the session log dir and scratch —
    /// is paid rarely, at the ready boundary both frontends share
    /// ([`Self::attend`]).
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

/// The model-facing chrome an item's source emits as it enters context —
/// whether it opens a fresh exchange at the boundary or lands mid-exchange at
/// a tool boundary: a human prompt and a wakeup echo as their (possibly
/// marked) text, an agent result renders as the dialable `↘` subagent block
/// the sink already knows.  A self-nudge and a command are quiet — a nudge is
/// an internal continuation, a command never reaches the model.
pub(super) fn announce(item: &Item, emit: &Emitter) {
    match item {
        Item::Human(_) | Item::Wakeup(_) | Item::Message(_) => {
            emit.emit(Kind::UserPromptEcho(item.text()));
        }
        Item::Agent(r) => emit.emit(Kind::SubagentDone {
            name: r.name.clone(),
            outcome: r.outcome.clone(),
            text: r.text.clone(),
            elapsed: r.elapsed,
        }),
        // A detached `spawn` worker's deferred surface batch: decode each value
        // and emit it as the live foreground decode would, so its cards/io land
        // on the rail.  The emitter's id is this session's, which is exactly the
        // id the batch was stamped with (a session's boundary sink stamps with
        // its own id and posts to its own inbox), so the cards reach the right
        // viewport.  The model is then woken with `item.text()`'s notice.
        Item::Surface { values, .. } => {
            for v in values {
                if let Some(kind) = shell_eval::accepted_surface(v, emit) {
                    emit.emit(kind);
                }
            }
        }
        // A self-nudge is a quiet continuation; a command never reaches the model.
        Item::Nudge(_) | Item::Command(_) => {}
    }
}

/// Reduce a finished deliberation's outcome to the `(tag, payload)` a
/// returning agent's result carries — the one place the reduction happens.
/// Only a deliberate `reply` carries a value up, and it travels as the
/// faithful [`FOValue`] the model passed; the consuming edge renders it (a
/// unit or empty-string value settles [`AgentOutcome::Empty`]).  A tool-call-free prose
/// finish is **not** harvested — there is no scrape — and because `reply` is
/// the sole return path, a returning agent that never replied did not
/// complete its contract: it (like a genuinely empty finish) settles
/// [`AgentOutcome::Failed`].  Every other shape carries only its tag.
fn agent_outcome(
    r: &Result<deliberate::Outcome, ProviderError>,
) -> (AgentOutcome, Option<FOValue>) {
    match r {
        // Any value that renders to text carries content; a unit or
        // empty-string reply (renders to nothing) is a deliberate empty
        // return.
        Ok(deliberate::Outcome::Replied(v)) if !render_reply(v).is_empty() => {
            (AgentOutcome::Complete, Some(v.clone()))
        }
        Ok(deliberate::Outcome::Replied(_)) => (AgentOutcome::Empty, None),
        // A finish without `reply`.  There is no scrape: a returning agent that
        // never replied did not complete its contract, so it fails honestly
        // rather than handing up a trailing fragment that masquerades as the
        // answer.  Re-nudged within budget first (see [`nudge`]).
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
    use crate::agent::testkit::*;
    use crate::fleet::registry::{AGENT_LEASE_IDLE, Registration};
    use crate::provider::scripted::{Reply, Script};
    use ral_core::Value;

    /// The park verdict reads engagement from the registry, never from focus:
    /// a conversing trunk parks `Held` regardless; an un-engaged parented
    /// returning agent quiesces once idle (the un-replied-worker outcome
    /// path, untouched); the same agent parks `Engaged` the instant a human
    /// exchange renews it (`bus.rs` pins the terminate-cause half of that
    /// contract).
    #[test]
    fn park_mode_reads_engagement_from_the_registry() {
        let dir = tmp("park-engaged");
        let held = trunk(&dir, true);
        assert_eq!(held.park_mode(), ParkMode::Held);

        let parent = Agent::for_test(&dir, "system").unwrap();
        let child = parent.fork(parent.caps().clone()).expect("fork child");
        child
            .agents
            .register(Registration {
                id: child.id,
                parent: Some(parent.id),
                lease: Some(AGENT_LEASE_IDLE),
                name: "child".into(),
                log_dir: dir.join("child"),
                cancel: child.cancel_token().clone(),
                reach: Some(child.seat.eval_reach()),
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

    /// The `reply` builtin's refusal on a non-returning agent is keyed on
    /// the captured `returns` bit, never on trunk-ness: the interactive
    /// trunk and a `/branch` child of it get the identical text, and the
    /// branch still parks `Held` afterward — the refusal is an ordinary
    /// call error, not a termination.
    #[test]
    fn reply_refused_identically_for_trunk_and_branch_conversing_agents() {
        let dir = tmp("reply-refused-conversing");
        let mut root = trunk(&dir, true);
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

    /// A returning agent that finishes without calling `reply` is re-nudged
    /// within budget, and if it still does not reply its run settles `Failed`
    /// — `reply` is the sole return path, so a finish without it did not
    /// complete the contract, and the final prose is **not** harvested (no
    /// scrape).
    #[test]
    fn sub_agent_without_reply_is_re_nudged_then_fails() {
        let dir = tmp("reply-missing");
        let parent = Agent::for_test(&dir, "system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("do the thing".into());
        // More prose-only replies than the nudge budget will consume: once it
        // is spent the un-replied finish is accepted and the surplus is never
        // popped (so the test does not couple to the exact budget).
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

    /// Run `cmd` as one dispatched run under a millisecond-scale
    /// `deferred_lease`, bypassing `shell_eval::run_shell`'s hardcoded 1 h/24 h
    /// constants so a reap test doesn't need to wait out the real policy.
    /// No deferred sink: a lease reap never tries to deliver anything to the
    /// inbox, so the tests using this only care about the registry side
    /// effect.
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
            },
        );
    }

    /// Seed one lease-chain reap (an unpolled worker under a millisecond
    /// idle bound) with no run running to push it — core's engine only
    /// pushes a `` `notice `` from *inside* a run's own surface stream, so a
    /// reap the background lease chain performs between runs sits queued
    /// until one runs. The next dispatched run surfaces it as
    /// `Kind::Notice`, ordered before that run's own `Kind::ToolResult`; a
    /// further run with nothing new queued surfaces no second notice.
    #[test]
    fn ready_boundary_reap_notice_surfaces_at_the_next_run() {
        let dir = tmp("drain-worker-reaps");
        let mut session = Agent::for_test(&dir, "system").unwrap();
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
        while let Ok(event) = rx.try_recv() {
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
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event.kind, Kind::Notice { .. }),
                "a further run with nothing new queued must surface no second notice"
            );
        }
    }

    // ── the `services` pin ledger ─────────────────────────────────────────

    /// The `services` pin is the host's own write: born with the service's
    /// description as soon as a durable birth is reconciled, and retired
    /// the moment cancelling it leaves no durable service running — the
    /// same ready-boundary pass in [`Agent::attend`].
    #[test]
    fn reconcile_service_pins_births_and_retires_the_services_pin() {
        let dir = tmp("reconcile-service-pins");
        let mut session = Agent::for_test(&dir, "system").unwrap();
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

        session.reconcile_service_pins(&emit);
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
        while let Ok(event) = rx.try_recv() {
            if let Kind::Pin { key, .. } = event.kind {
                assert_eq!(key, shell_eval::SERVICES_PIN_KEY);
                saw_pin = true;
            }
        }
        assert!(saw_pin, "the birth must emit a Pin event");

        // Cancel the service through the ordinary `cancel` builtin — the
        // same edge a model reaches — and reconcile again: the row leaves.
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
            .body
            .call(
                &[Value::Handle(entry.handle)],
                &mut session.seat.shell_mut().shell,
            )
            .expect("cancel must succeed");

        session.reconcile_service_pins(&emit);
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
        while let Ok(event) = rx.try_recv() {
            if let Kind::Unpin { key } = event.kind {
                assert_eq!(key, shell_eval::SERVICES_PIN_KEY);
                saw_unpin = true;
            }
        }
        assert!(saw_unpin, "retirement must emit an Unpin event");
    }

    /// A model's own `surface` call cannot clear the host-owned `services`
    /// pin: `run_shell` decodes the forged `unpin`, `reject_protected_pin`
    /// refuses it with a diagnostic, and the register is left untouched —
    /// proof that `run_shell`'s applier routes through the guard. The forge
    /// direction (writing over the pin) is covered at the rule's own site,
    /// `shell_eval::reject_protected_pin`, not here.
    #[test]
    fn program_cannot_write_the_services_pin() {
        let dir = tmp("services-pin-protected");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        session.run_shell("c1".into(), r#"surface `unpin [key: "services"]"#, 5, &emit);

        let mut saw_error = false;
        while let Ok(event) = rx.try_recv() {
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

    /// Seed one idle binding (a tiny re-armed bound, for speed — every
    /// `Agent` is already armed with the production `BINDING_IDLE_CALLS` by
    /// `assemble`): the boundary prune fires inside a later call's own run
    /// and its `Kind::Notice` rides that run's surface stream — one notice
    /// naming the pruned binding, and no second notice once nothing is left
    /// idle.
    #[test]
    fn boundary_prune_notice_rides_the_runs_own_stream() {
        let dir = tmp("reap-bindings");
        let mut session = Agent::for_test(&dir, "system").unwrap();
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
        while let Ok(event) = rx.try_recv() {
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
        while let Ok(event) = rx.try_recv() {
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

    /// Unconfigured (`disk_warn_bytes: None`) is a no-op by construction:
    /// the check returns before touching the epoch bookkeeping or walking
    /// anything, so it never emits and never costs.
    #[test]
    fn check_disk_warn_unconfigured_never_walks_or_warns() {
        let dir = tmp("disk-warn-unconfigured");
        let mut session = Agent::for_test(&dir, "system").unwrap();
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

    /// Still above the ceiling on a later (amortized) check does not repeat
    /// the warning — the latch stays set until the figure falls back under.
    #[test]
    fn check_disk_warn_still_above_does_not_repeat() {
        let dir = tmp("disk-warn-still-above");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let baseline = crate::agent::resources::dir_size(&session.log_dir());
        std::fs::write(session.log_dir().join("big.txt"), vec![0u8; 4096]).unwrap();
        session.disk_warn_bytes = Some(baseline + 100);

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.check_disk_warn(&emit);
        assert!(rx.try_recv().is_ok(), "the first crossing warns");

        // Force the amortization window to have elapsed, as if
        // `DISK_WARN_CHECK_INTERVAL` more ral calls had passed, without
        // actually driving any.
        session.disk_check_epoch = session.ral_epoch;
        session.check_disk_warn(&emit);
        assert!(
            rx.try_recv().is_err(),
            "still above the ceiling: the latch suppresses a repeat"
        );
    }

    /// Falling back under the ceiling clears the latch, so a later
    /// re-crossing warns again rather than staying suppressed forever.
    #[test]
    fn check_disk_warn_falling_below_rearms_the_latch() {
        let dir = tmp("disk-warn-rearm");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        // The ceiling sits just above the session's own baseline footprint
        // (its freshly-written `events.json`), so the big file alone decides
        // whether the ceiling is crossed.
        let baseline = crate::agent::resources::dir_size(&session.log_dir());
        let big = session.log_dir().join("big.txt");
        std::fs::write(&big, vec![0u8; 4096]).unwrap();
        session.disk_warn_bytes = Some(baseline + 100);

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.check_disk_warn(&emit);
        let event = rx.try_recv().expect("the first crossing warns");
        match event.kind {
            Kind::SystemNote(text) => assert!(text.contains("disk"), "{text}"),
            _ => panic!("expected Kind::SystemNote"),
        }

        // Fall back under the ceiling and force a re-check.
        std::fs::remove_file(&big).unwrap();
        session.disk_check_epoch = session.ral_epoch;
        session.check_disk_warn(&emit);
        assert!(
            rx.try_recv().is_err(),
            "back under the ceiling: no warning, just the latch clearing"
        );
        assert!(!session.disk_warn_latched, "the latch is cleared");

        // Cross again: the latch must be re-armed, not stuck cleared forever.
        std::fs::write(&big, vec![0u8; 4096]).unwrap();
        session.disk_check_epoch = session.ral_epoch;
        session.check_disk_warn(&emit);
        assert!(
            rx.try_recv().is_ok(),
            "re-crossing after falling below warns again"
        );
    }

    /// A binding prune is transcript/TUI-only: it must never grow the
    /// model-view `events.json` the same attend loop pass writes tool
    /// results into.
    #[test]
    fn prune_event_is_absent_from_events_json() {
        let dir = tmp("prune-events-json");
        let mut session = Agent::for_test(&dir, "system").unwrap();
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
        let pruned = std::iter::from_fn(|| rx.try_recv().ok()).any(|event| {
            matches!(
                event.kind,
                Kind::Notice {
                    notice: crate::bus::card::Notice::Prune { .. },
                    ..
                }
            )
        });
        assert!(pruned, "the prune notice must have fired on the bus");

        // A further identical call with nothing to prune grows the model
        // view by exactly the same amount: the prune contributed nothing.
        session.run_shell("c2".into(), "$[0]", 5, &emit);
        let after_plain_call = session.log.lock().event_count();
        assert_eq!(
            after_prune_call - after_bind,
            after_plain_call - after_prune_call,
            "a binding prune must never write a model-view events.json entry"
        );
    }
}
