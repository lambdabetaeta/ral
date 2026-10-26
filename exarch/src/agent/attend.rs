//! The one loop every node runs: draw the next item off the node's own inbox,
//! take it up, and turn a nudge-worthy outcome into a self-posted nudge.
//! Stepping the model to quiescence over one item is [`super::deliberate`]'s
//! job; nothing here special-cases a node's position, since a reply is
//! deposited on the node's own agent and a non-reply end is delivered up its
//! parent's mailbox by the spawn site.

use crate::agent::EngineLost;
use crate::agent::nudge;
use crate::agent::outcome::{Fault, Outcome};
use crate::agent::{Avatar, Role};
use crate::bus::{
    AgentOutcome, Emitter, Item, Next, ParkMode, Post, WORKER_PANIC_PREFIX, panic_msg,
};
use crate::clock;
use crate::provider::{Limit, ProviderError, Recovery};
use crate::record::{AgentState, Forensic, QuiesceReason, Transient};
use crate::schedule::Trigger;
use ral_core::carrier::Severed;
use ral_core::sync::LockExt;
use std::ops::ControlFlow;

/// How an empty inbox ends a pass: parked on the verdict [`Avatar::park_mode`]
/// gives, or handed back to the caller at once.
#[derive(Clone, Copy)]
enum Idle {
    Park,
    Return,
}

/// What one drawn item leaves the loop with: the outcome an item settled on,
/// if one did — a command settles nothing — and whether the loop draws again;
/// `/close` and a root's `reply` end it.
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
        let mut settled = None;
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
                        settled = Some(outcome);
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
        // the plain sentence as the failed outcome.  Otherwise the last
        // settlement stands; a loop that settled nothing ends on why it
        // stopped drawing — a terminate cause in force before anything
        // settled (a lease reap, a `cancel`) is that cancellation, and
        // anything else a finish without `reply`.
        let settled = match lost.or_else(|| self.seat.severed()) {
            Some(s) => {
                let lost = EngineLost::running(&s, self.agent.run_dir());
                self.note(lost.logged());
                AgentOutcome::Failed(lost.to_string())
            }
            None => settled.unwrap_or_else(|| {
                if self.agent.token.terminated() {
                    AgentOutcome::Cancelled
                } else {
                    AgentOutcome::Failed(NO_REPLY_REASON.into())
                }
            }),
        };
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
        if !self.arrive(item)? {
            return Ok(Step::command(ControlFlow::Continue(())));
        }
        // Only a genuine boundary clears the latches, a prior exchange's Esc,
        // and any nudge still queued behind it — that nudge continued the
        // exchange this one closes.  A self-nudge is the same exchange
        // continuing.
        if item.opens_exchange() {
            self.readings.nudges.reset();
            self.agent.token.reset();
            self.inbox.drop_nudges();
        }
        let outcome = match self.attempt(item, emit) {
            Ok(outcome) => Ok(outcome),
            Err(Fault::Provider(error)) => {
                self.record_failure(&error);
                Err(error)
            }
            Err(Fault::Severed(s)) => return Err(s),
            // Recording and continuing keeps one host-side crash from sinking
            // the agent.
            Err(Fault::Log(why) | Fault::Panicked(why)) => {
                self.note_error(&why);
                return Ok(Step::settled(
                    AgentOutcome::Failed(why),
                    ControlFlow::Continue(()),
                ));
            }
        };
        self.follow_up(outcome.as_ref())?;
        // Only the headless root ends on `reply`; a child parks on its
        // deposit, to be messaged again.
        let flow = if matches!(
            (&self.agent.role, &outcome),
            (Role::Headless, Ok(Outcome::Replied))
        ) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        };
        Ok(Step::settled(agent_outcome(&outcome), flow))
    }

    /// Deliberate over `item`, a host-side panic caught as a fault; the log
    /// is left ready however it ends.
    fn attempt(&mut self, item: &Item, emit: &Emitter) -> Result<Outcome, Fault> {
        // Read once, so a `/model` swap on the UI thread lands on the next
        // item rather than mid-item.
        let active = self.agent.provider.current();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.deliberate(
                &active,
                Some((item.text(), item.voice())),
                item.continues(),
                emit,
            )
        }));
        // A provider error or an unwind can leave the session mid-protocol;
        // quiesce now so the next prompt — nudge or user — is admissible.
        self.abort_unready();
        attempt.unwrap_or_else(|payload| {
            Err(Fault::Panicked(format!(
                "{WORKER_PANIC_PREFIX}{}",
                panic_msg(&payload)
            )))
        })
    }

    /// Record a provider failure whether or not a nudge follows — a chat
    /// trunk keeps no registry, and its failures must still reach the human —
    /// and arm the resume a refusal with a known reset earns.
    fn record_failure(&self, error: &ProviderError) {
        self.recorder().emit_or_report(Forensic::ProviderError {
            error: error.clone(),
        });
        if let ProviderError::Refused(refusal) = error
            && self.resumes_at_reset()
            && let Recovery::Deferred(at) = refusal.recovery()
        {
            self.resume_at(at, refusal.limit);
        }
    }

    /// Self-post the nudge `outcome` earns, if any.
    ///
    /// # Errors
    /// The engine's severance, from the worker probe.
    fn follow_up(&mut self, outcome: Result<&Outcome, &ProviderError>) -> Result<(), Severed> {
        // A boundary read, legal here: the batch has fully drained and no
        // dispatch is in flight.
        let workers_idle = self.seat.read(|t| t.workers())?.is_empty();
        let facts = nudge::Facts {
            must_reply: self.agent.role.returns(),
            pinned: self.pinned_digest(),
            // Nudged only when nothing else is already carrying this agent
            // forward: no reply standing for a consumer to fetch, no worker
            // still running, no busy children.
            quiet: !self.agent.has_reply() && workers_idle && !self.agent.has_busy_children(),
        };
        let nudged = self
            .readings
            .nudges
            .react(outcome, &facts, &self.log.borrow());
        let Some(text) = nudged else {
            return Ok(());
        };
        let prompt = self.log.borrow().context().current_prompt();
        match prompt {
            Some(prompt) => self.inbox.push(Post::Nudge { prompt, text }),
            // Unreachable: every outcome `react` answers followed a
            // deliberation whose prompt `append_user` committed.
            None => self.note_error(&format!(
                "a nudge was decided with no prompt in hand to continue: dropping it: {text}"
            )),
        }
        Ok(())
    }

    /// Every pinned slot's summary joined onto one line, `None` when nothing
    /// is pinned.  Rendered through the rail's own `summary_line`, so the
    /// reminder reads as the user sees it.
    fn pinned_digest(&self) -> Option<String> {
        let lines: Vec<String> = self
            .agent
            .pins
            .lock_ignore_poison()
            .values()
            .map(|pin| crate::card::summary_line(&pin.card))
            .collect();
        (!lines.is_empty()).then(|| lines.join("; "))
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
        if matches!(agent.role, Role::Converses) && self.fleet.launch.trunk.attended() {
            // `next_or_idle` lets a terminate cause end every park but `Held`,
            // so a conversing agent asks here instead: a terminate-cause cancel
            // (SIGTERM) stamps this token, and parking Held past it would be a zombie.
            return if agent.token.terminated() {
                ParkMode::Quiesce
            } else {
                ParkMode::Held
            };
        }
        // Only a child waits to be messaged; a headless root returns and ends.
        if matches!(agent.role, Role::Child { .. }) && (engaged || agent.has_reply()) {
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
