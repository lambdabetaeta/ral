//! The one loop every node runs: draw the next item off the node's own inbox,
//! take it up, and turn a nudge-worthy outcome into a self-posted nudge.
//! Stepping the model to quiescence over one item is [`super::deliberate`]'s
//! job; nothing here special-cases a node's position, since a reply is
//! deposited on the node's own agent and a non-reply end is delivered up its
//! parent's mailbox by the spawn site.

use crate::agent::Avatar;
use crate::agent::deliberate::{Fault, Outcome};
use crate::agent::gauge::{Warning, ration};
use crate::agent::nudge;
use crate::agent::seat::EngineLost;
use crate::bus::{
    AgentOutcome, Emitter, Item, Next, ParkMode, Post, WORKER_PANIC_PREFIX, panic_msg,
};
use crate::clock;
use crate::provider::{Limit, Provider, ProviderError, Recovery};
use crate::record::AgentState;
use crate::record::QuiesceReason;
use crate::record::Transient;
use crate::schedule::Trigger;
use crate::shell_eval;
use ral_core::carrier::Severed;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

/// How an empty inbox ends a pass: parked on the verdict [`Avatar::park_mode`]
/// gives, or handed back to the caller at once.
#[derive(Clone, Copy)]
enum Idle {
    Park,
    Return,
}

/// What one drawn item leaves the loop with: the outcome an item settled on,
/// if one did — a command settles nothing — and whether the loop draws again;
/// `/quit` and a root's `reply` end it.
struct Step {
    settled: Option<AgentOutcome>,
    flow: ControlFlow<()>,
}

impl Step {
    fn command(flow: ControlFlow<()>) -> Self {
        Self {
            settled: None,
            flow,
        }
    }

    fn settled(outcome: AgentOutcome, flow: ControlFlow<()>) -> Self {
        Self {
            settled: Some(outcome),
            flow,
        }
    }
}

/// Between disk-ceiling walks: a full scan of the session log dir and
/// scratch, too costly to pay at every tool boundary.
const DISK_WARN_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// The label of the one-shot wakeup armed at a refusal's reset.
const RESUME_LABEL: &str = "provider-reset";

/// A finish without `reply` did not complete a returning agent's contract;
/// there is no scrape of its prose.
const NO_REPLY_REASON: &str = "ended without calling `reply`";

impl Avatar {
    /// The one attend loop, identical for every node: draw the next inbox
    /// item, take it up, and repeat until an empty inbox meets a
    /// [`Self::park_mode`] that does not park, or a cancel ends one that does.
    /// A `reply`'s value is deposited on the agent for its consumer to read.
    pub fn attend(&mut self, emit: &Emitter) -> AgentOutcome {
        self.attend_until(emit, Idle::Park)
    }

    /// [`Self::attend`]'s per-exchange half, for
    /// [`converse`](crate::headless::converse): drain the seeded message and
    /// any nudge continuation it raises, then return instead of blocking for
    /// the next one.  Converse posts no command, so a slash-shaped user
    /// message reaches the model as ordinary text.
    pub(crate) fn attend_backlog(&mut self, emit: &Emitter) -> AgentOutcome {
        self.attend_until(emit, Idle::Return)
    }

    fn attend_until(&mut self, emit: &Emitter, idle: Idle) -> AgentOutcome {
        self.couple(emit);
        let mut settled = AgentOutcome::Failed(NO_REPLY_REASON.into());
        let lost = loop {
            if let Some(s) = self.seat.severed() {
                break Some(s);
            }
            let Some(next) = self.draw(idle) else {
                break None;
            };
            self.agent.set_resting(false);
            match self.take_up(&next, emit) {
                Ok(step) => {
                    if let Some(outcome) = step.settled {
                        settled = outcome;
                    }
                    if step.flow.is_break() {
                        break None;
                    }
                }
                Err(s) => break Some(s),
            }
        };
        // A severance the loop broke on, or one a park quiesced behind, is
        // recorded exactly once: the engine's own account as a durable note,
        // the plain sentence as the failed outcome.
        if let Some(s) = lost.or_else(|| self.seat.severed()) {
            let lost = EngineLost::running(&s, self.agent.run_dir());
            self.note(lost.logged());
            settled = AgentOutcome::Failed(lost.to_string());
        }
        // `take_up` quiesces per item; this catches whichever path broke the
        // loop, so the agent is ReadyForUser however it ends.
        self.abort_unready();
        debug_assert!(
            self.log.borrow().context().is_ready(),
            "attend must leave the agent ReadyForUser"
        );
        settled
    }

    /// The next item, or `None` once an empty inbox ends the pass.  Every
    /// draw is a settled ready boundary.  A lease-chain reap is deliberately
    /// not drained here: core pushes its `` `notice `` on the surface stream
    /// of the run that observes it, so a reap during a long idle surfaces once
    /// an item next runs.
    fn draw(&self, idle: Idle) -> Option<Next> {
        match idle {
            Idle::Return => self.inbox.next_item(),
            Idle::Park => {
                // The state a frontend shows over the coming silence is this
                // park's own verdict.  Only on an empty queue: with an item
                // already in hand the agent is not idle for any observable
                // moment, and the deliberation's own transitions are the truth.
                if self.inbox.is_empty() {
                    let mode = self.park_mode(self.agent.engaged());
                    self.agent.set_resting(mode == ParkMode::Engaged);
                    self.recorder()
                        .transient(Transient::State(idle_state(mode)));
                }
                // The verdict is recomputed on every wake.
                self.inbox
                    .next_or_idle(|engaged| self.park_mode(engaged), &self.agent.token)
            }
        }
    }

    /// Take up one drawn item.  A drawn item is always admissible: staleness
    /// is settled at the inbox's own pop, against that inbox's clear-epoch.
    ///
    /// # Errors
    /// The engine's severance, after which no item can run.
    fn take_up(&mut self, next: &Next, emit: &Emitter) -> Result<Step, Severed> {
        let item = match next {
            Next::Read(cmd) => {
                self.read(cmd, emit);
                return Ok(Step::command(ControlFlow::Continue(())));
            }
            Next::Rewrite(cmd) => return Ok(Step::command(self.rewrite(cmd, emit))),
            Next::Item(item) => item,
        };
        self.heard(item);
        // Only a genuine boundary clears the latches, a prior exchange's Esc,
        // and any nudge still queued behind it — that nudge continued the
        // exchange this one closes.  A self-nudge is the same exchange
        // continuing.
        if item.opens_exchange() {
            self.readings.nudges.reset();
            self.agent.token.reset();
            self.inbox.drop_nudges();
        }
        let opens = self.log.borrow().opens(item.continues());
        announce(item, opens, &self.recorder());
        // Read once, so a `/model` swap on the UI thread lands on the next
        // item rather than mid-item.
        let active = self.agent.provider.current();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.deliberate(&active, Some(item.text()), item.continues(), emit)
        }));
        // A provider error or an unwind can leave the session mid-protocol;
        // quiesce now so the next prompt — nudge or user — is admissible.
        self.abort_unready();
        let outcome: Result<Outcome, ProviderError> = match attempt {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(Fault::Provider(error))) => Err(error),
            Ok(Err(Fault::Severed(s))) => return Err(s),
            Ok(Err(Fault::Log(why))) => {
                self.note_error(&why);
                return Ok(Step::settled(
                    AgentOutcome::Failed(why),
                    ControlFlow::Continue(()),
                ));
            }
            // Host-side only — transport, surface decode, render.  An
            // eval-side panic is caught at `Shell::run`, which rolls the
            // dynamic state back, so it never unwinds this far.  Recording
            // and continuing keeps one crash from sinking the agent.
            Err(payload) => {
                let msg = format!("{WORKER_PANIC_PREFIX}{}", panic_msg(&payload));
                self.note_error(&msg);
                return Ok(Step::settled(
                    AgentOutcome::Failed(msg),
                    ControlFlow::Continue(()),
                ));
            }
        };
        // Before any nudge decision, and whether or not one follows: a chat
        // trunk keeps no registry, and its failures must still reach the human.
        if let Err(error) = &outcome {
            let recorded = self.log.borrow_mut().record_provider_error(error);
            if let Err(unrecorded) = recorded {
                self.recorder().report_fault(&unrecorded);
            }
            if let ProviderError::Refused(refusal) = error
                && self.resumes_at_reset()
                && let Recovery::Deferred(at) = refusal.recovery()
            {
                self.resume_at(at, refusal.limit);
            }
        }
        // A boundary read, legal here: the batch has fully drained and no
        // dispatch is in flight.
        let workers_idle = self.seat.read(|t| t.workers())?.is_empty();
        let facts = nudge::Facts {
            must_reply: self.agent.returns,
            pinned: self.pinned_digest(),
            // Nudged only when nothing else is already carrying this agent
            // forward: no reply standing for a consumer to fetch, no detached
            // shell work, no busy children.
            quiet: !self.agent.has_reply() && workers_idle && !self.agent.has_busy_children(),
        };
        let nudged =
            self.readings
                .nudges
                .react(outcome.as_ref(), &facts, &mut self.log.borrow_mut());
        if let Some(text) = nudged {
            let prompt = self.log.borrow().context().current_prompt();
            match prompt {
                Some(prompt) => self.inbox.push(Post::Nudge { prompt, text }),
                // Unreachable: every outcome `react` answers followed a
                // deliberation whose prompt `append_user` committed.
                None => self.note_error(&format!(
                    "a nudge was decided with no prompt in hand to continue: dropping it: {text}"
                )),
            }
        }
        // Only a parentless returning agent — the headless root — ends on
        // `reply`; a child parks on its deposit, to be messaged again.
        let flow = if self.agent.parent.is_none() && matches!(outcome, Ok(Outcome::Replied)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        };
        Ok(Step::settled(agent_outcome(&outcome), flow))
    }

    /// Book a drawn item against the park verdict: a direct child's
    /// result means it is no longer awaited.  [`Avatar::settle`] makes
    /// deliver-then-retire structural, so a parked parent never sees "no live
    /// child" without the result already queued.  Every item reaching here
    /// already survived the inbox's own pop-time fence against a `/clear`
    /// (`crate::bus::inbox`'s clear-epoch), so there is nothing left to
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
    /// mutex — and this agent's own status.  A conversing agent with a human
    /// attached parks [`ParkMode::Held`], immune to cancellation; one driven
    /// one exchange at a time waits on nothing but its fleet; a returning
    /// agent a human has exchanged with parks [`ParkMode::Engaged`] — the same
    /// wait, but a terminate-cause cancel still ends it, and the fleet's idle
    /// lease rather than this predicate bounds it.
    fn park_mode(&self, engaged: bool) -> ParkMode {
        // Nothing is left to wait for once the engine is gone.
        if self.seat.severed().is_some() {
            return ParkMode::Quiesce;
        }
        let agent = &self.agent;
        if !agent.returns && self.fleet.launch.attended {
            // `next_or_idle` lets a terminate cause end every park but `Held`,
            // so a conversing agent asks here instead: `/close` stamps this
            // token, and parking Held past it would be a zombie.
            return if agent.token.terminated() {
                ParkMode::Quiesce
            } else {
                ParkMode::Held
            };
        }
        // Only an agent with somewhere to report waits to be messaged; a
        // headless root returns and ends.
        if agent.parent.is_some() && agent.returns && (engaged || agent.has_reply()) {
            return ParkMode::Engaged;
        }
        if agent.has_busy_children() {
            return ParkMode::HeldByChildren;
        }
        if agent.schedules.armed() {
            return ParkMode::UntilCancelled;
        }
        ParkMode::Quiesce
    }

    /// The disk-warn ceiling's verdict, as an operational note once per
    /// excursion — nothing is ever rotated or deleted.  Unconfigured it walks
    /// nothing at all; otherwise it walks at most once per
    /// [`DISK_WARN_CHECK_INTERVAL`].
    ///
    /// # Errors
    /// The engine's severance, from the `EXARCH_SCRATCH` probe.
    fn disk_warning(&mut self) -> Result<Option<Warning>, Severed> {
        let Some(ceiling) = self.fleet.launch.disk_warn_bytes else {
            return Ok(None);
        };
        if self
            .disk_checked
            .is_some_and(|at| at.elapsed() < DISK_WARN_CHECK_INTERVAL)
        {
            return Ok(None);
        }
        self.disk_checked = Some(Instant::now());
        let mut total = crate::agent::resources::dir_size(self.log.borrow().dir());
        if let Some((_, bytes)) = self.scratch_bytes()? {
            total += bytes;
        }
        Ok(self.readings.gauges.disk(total, ceiling))
    }

    /// Every standing condition newly climbed, told: the user's lines noted,
    /// the model's reminders returned as the steering that trails a tool
    /// batch's arrivals.
    ///
    /// # Errors
    /// The engine's severance, from the disk probe.
    pub(super) fn warnings(&mut self, provider: &Provider) -> Result<Vec<String>, Severed> {
        let pressure = self.pressure_gauge(provider);
        let mut told: Vec<Warning> = self
            .readings
            .gauges
            .pressure(pressure)
            .into_iter()
            .collect();
        told.extend(self.disk_warning()?);
        let account = provider.account();
        told.extend(ration::told(
            account,
            &provider.climb_allowances(ration::USER_HEARS),
        ));
        told.extend(self.readings.gauges.ration.climb(
            account,
            &provider.allowances(),
            self.resumes_at_reset(),
        ));
        Ok(self.tell(told))
    }

    /// Note each user line; word each model one as a reminder — none for a
    /// trunk that steers nothing.
    fn tell(&self, warnings: Vec<Warning>) -> Vec<String> {
        warnings
            .into_iter()
            .filter_map(|warning| match warning {
                Warning::User(line) => {
                    self.note(line);
                    None
                }
                Warning::Model(reminder) => self
                    .readings
                    .nudges
                    .remind(&reminder, &mut self.log.borrow_mut()),
            })
            .collect()
    }

    /// Arm the one-shot wakeup that resumes the task at a refusal's reset,
    /// replacing any earlier one.
    fn resume_at(&self, resets_at: jiff::Timestamp, limit: Limit) {
        let now = jiff::Timestamp::now();
        let prompt = format!(
            "The provider refused requests at {} until {}; that time has passed. \
             Carry on where you left off.",
            clock::local(now),
            clock::local(resets_at)
        );
        let schedules = &self.agent.schedules;
        schedules.unschedule(RESUME_LABEL);
        match schedules.schedule(
            Trigger::At(resets_at),
            prompt,
            RESUME_LABEL.into(),
            &self.agent.mailbox,
        ) {
            Ok(_) => self.note(format!(
                "{}: resuming {}, in {}",
                limit.label(),
                clock::local(resets_at),
                clock::hms(clock::until(resets_at, now).as_secs())
            )),
            Err(refusal) => self.note_error(&format!(
                "{}: the resume could not be scheduled: {refusal}",
                limit.label()
            )),
        }
    }

    /// Quiesce a log a deliberation left mid-protocol.
    fn abort_unready(&self) {
        let mut log = self.log.borrow_mut();
        if !log.context().is_ready() {
            log.quiesce(QuiesceReason::Aborted);
        }
    }
}

/// Emit the chrome an item's source shows as it enters context, `opens` being
/// the turn a prompt opens ([`crate::record::AgentLog::opens`]).  A nudge is an internal
/// continuation and a command never reaches the model, so both are quiet.
pub(super) fn announce(item: &Item, opens: Option<u64>, recorder: &crate::record::Emitter) {
    match item {
        Item::Human(_) | Item::Wakeup(_) | Item::Message(_) => {
            // The live row derives from the published `Display::Prompt`
            // record, so this is the one authoring site.
            record_commit(
                recorder,
                crate::record::Display::Prompt {
                    text: item.text(),
                    turn: opens,
                },
            );
        }
        Item::Agent(r) => {
            // The record carries the reduced fault the scrollback block is
            // built from, not the raw outcome enum.
            record_commit(
                recorder,
                crate::record::Display::SubagentDone {
                    name: r.name.clone(),
                    error: r.outcome.fault(),
                    elapsed_ms: u64::try_from(r.elapsed.as_millis()).unwrap_or(u64::MAX),
                },
            );
        }
        // A detached `spawn`'s deferred batch, decoded as the live foreground
        // decode would.  It was stamped with and posted to this same session,
        // so the emitter's id already routes its cards to the right scrollback.
        Item::Surface { values, .. } => {
            for v in values {
                match shell_eval::decode_surface(v) {
                    shell_eval::Decoded::Surface(surface) => {
                        if let Err(error) = crate::agent::desk::absorb_surface(recorder, &surface) {
                            recorder.report_fault(&error);
                        }
                    }
                    shell_eval::Decoded::Landed => {}
                    shell_eval::Decoded::Unknown => {
                        if let Err(error) =
                            recorder.emit(shell_eval::unknown_surface_note(v.shape()))
                        {
                            recorder.report_fault(&error);
                        }
                    }
                }
            }
        }
        Item::Nudge { .. } => {}
    }
}

/// Record one display commit, surfacing a failed append as a
/// `Transient::Fault` exactly as the surface path does.
fn record_commit(recorder: &crate::record::Emitter, commit: crate::record::Display) {
    if let Err(error) = recorder.emit(commit) {
        recorder.report_fault(&error);
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

/// Reduce a finished deliberation to the result a returning agent's consumer
/// is told.  Only `reply` completes the contract — its value is already
/// deposited on the agent — so a finish without one settles
/// [`AgentOutcome::Failed`].
fn agent_outcome(r: &Result<Outcome, ProviderError>) -> AgentOutcome {
    match r {
        Ok(Outcome::Replied) => AgentOutcome::Replied,
        // Re-nudged within budget by `nudge` before it ever reaches here.
        Ok(Outcome::Complete | Outcome::Empty) => AgentOutcome::Failed(NO_REPLY_REASON.into()),
        Ok(Outcome::Stopped { reason }) => AgentOutcome::Stopped(reason.clone()),
        Ok(Outcome::Cancelled) => AgentOutcome::Cancelled,
        Err(e) => AgentOutcome::Failed(e.summary()),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests;
