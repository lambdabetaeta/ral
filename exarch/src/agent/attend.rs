//! Draw, take up, deliberate, and continue until the park verdict ends attendance.

use crate::agent::outcome::{Fault, Outcome};
use crate::agent::{Agent, EngineLost, Role, nudge};
use crate::bus::{
    AgentOutcome, Drawn, Emitter, Item, Next, ParkMode, State, WORKER_PANIC_PREFIX, panic_msg,
};
use crate::clock;
use crate::provider::{Limit, ProviderError, Recovery};
use crate::record::{AgentState, Forensic, QuiesceReason, Transient};
use crate::schedule::Trigger;
use ral_core::carrier::Severed;
use ral_core::process::Scope;
use ral_core::sync::LockExt;
use std::ops::ControlFlow;

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
const RESUME_LABEL: &str = "provider-reset";
const NO_REPLY_REASON: &str = "ended without calling `reply`";
impl Agent {
    pub fn attend(&mut self, emit: &Emitter) -> AgentOutcome {
        self.couple(emit);
        let mut settled = None;
        let lost = loop {
            if self.check_available().is_err() {
                break None;
            }
            if let Some(s) = self.engine.severed() {
                break Some(s);
            }
            let Some(drawn) = self.draw() else {
                break None;
            };
            self.inbox.set_resting(false);
            match self.take_up(drawn, emit) {
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
        // Only a child's reply is leased, so only a child's can be superseded.
        let settled = settled.filter(|outcome| {
            !matches!(self.handle.0.role, Role::Child { .. })
                || !matches!(outcome, AgentOutcome::Replied)
                || self.handle.reply_since().is_some()
        });
        let settled = match lost.or_else(|| self.engine.severed()) {
            Some(s) => {
                let lost = EngineLost::running(&s, self.run_dir());
                self.note(lost.logged());
                AgentOutcome::Failed(lost.to_string())
            }
            None => settled.unwrap_or_else(|| {
                if self.handle.is_live() {
                    AgentOutcome::Failed(NO_REPLY_REASON.into())
                } else {
                    AgentOutcome::Cancelled
                }
            }),
        };
        if let Err(error) = self.abort_unready() {
            self.end_unavailable(&error);
            return AgentOutcome::Failed(error.to_string());
        }
        debug_assert!(
            self.log.borrow().context().is_ready(),
            "a usable attended session must admit a fresh prompt"
        );
        settled
    }
    fn draw(&self) -> Option<Drawn> {
        if self.check_available().is_err() {
            return None;
        }
        if self.inbox.is_empty() {
            let mode = self.park_snapshot();
            self.inbox.set_resting(mode == ParkMode::Engaged);
            self.recorder()
                .transient(Transient::State(idle_state(mode)));
        }
        self.inbox.draw(|state| self.park_mode(state))
    }
    fn take_up(&mut self, drawn: Drawn, emit: &Emitter) -> Result<Step, Severed> {
        if let Err(error) = self.check_available() {
            return Ok(Step::settled(
                AgentOutcome::Failed(error.to_string()),
                ControlFlow::Break(()),
            ));
        }
        let Drawn { next, opening } = drawn;
        let item = match &next {
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
        if let Some(opening) = opening {
            opening.open();
            self.readings.nudges.reset();
        }
        let exchange = self.inbox.exchange();
        let outcome = match self.attempt(&exchange, item, emit) {
            Ok(outcome) => Ok(outcome),
            Err(Fault::Provider(error)) => {
                self.record_failure(&error);
                Err(error)
            }
            Err(Fault::Severed(s)) => return Err(s),
            Err(Fault::Unavailable(why)) => {
                return Ok(Step::settled(
                    AgentOutcome::Failed(why),
                    ControlFlow::Break(()),
                ));
            }
            Err(Fault::Log(why) | Fault::Panicked(why)) => {
                self.note_error(&why);
                return Ok(Step::settled(
                    AgentOutcome::Failed(why),
                    ControlFlow::Continue(()),
                ));
            }
        };
        if self.check_available().is_err() {
            return Ok(Step::settled(
                agent_outcome(&outcome),
                ControlFlow::Break(()),
            ));
        }
        self.follow_up(outcome.as_ref())?;
        let flow = if matches!(
            (&self.handle.0.role, &outcome),
            (Role::Headless, Ok(Outcome::Replied))
        ) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        };
        Ok(Step::settled(agent_outcome(&outcome), flow))
    }
    fn attempt(&mut self, exchange: &Scope, item: &Item, emit: &Emitter) -> Result<Outcome, Fault> {
        let active = self.handle.current_provider();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.deliberate(
                exchange,
                &active,
                Some((item.text(), item.voice())),
                item.continues(),
                emit,
            )
        }));
        self.abort_unready()
            .map_err(|error| Fault::Unavailable(error.to_string()))?;
        attempt.unwrap_or_else(|payload| {
            Err(Fault::Panicked(format!(
                "{WORKER_PANIC_PREFIX}{}",
                panic_msg(&payload)
            )))
        })
    }
    fn record_failure(&self, error: &ProviderError) {
        self.recorder().emit_or_report(Forensic::ProviderError {
            error: error.clone(),
        });
        if let ProviderError::Refused(refusal) = error
            && self.check_available().is_ok()
            && self.resumes_at_reset()
            && let Recovery::Deferred(at) = refusal.recovery()
        {
            self.resume_at(at, refusal.limit);
        }
    }
    fn follow_up(&mut self, outcome: Result<&Outcome, &ProviderError>) -> Result<(), Severed> {
        let workers_idle = self.engine.read(|t| t.workers())?.is_empty();
        let facts = nudge::Facts {
            must_reply: self.handle.0.role.returns(),
            pinned: self.pinned_digest(),
            quiet: workers_idle && !self.handle.has_busy_children(),
        };
        let Some(text) = self
            .readings
            .nudges
            .react(outcome, &facts, &self.log.borrow())
        else {
            return Ok(());
        };
        if self.check_available().is_err() {
            return Ok(());
        }
        match self.log.borrow().context().current_prompt() {
            Some(prompt) => self.inbox.nudge(prompt, text),
            None => self.note_error(&format!(
                "a nudge was decided with no prompt in hand to continue: dropping it: {text}"
            )),
        }
        Ok(())
    }
    fn pinned_digest(&self) -> Option<String> {
        let lines: Vec<_> = self
            .pins
            .lock_ignore_poison()
            .values()
            .map(|pin| crate::card::summary_line(&pin.card))
            .collect();
        (!lines.is_empty()).then(|| lines.join("; "))
    }
    fn park_snapshot(&self) -> ParkMode {
        let status = self.handle.status();
        self.residency(status.reply_since.is_some(), status.busy)
    }
    fn park_mode(&self, state: &State) -> ParkMode {
        self.residency(state.leased(), state.has_busy_children())
    }
    fn residency(&self, leased: bool, busy: bool) -> ParkMode {
        if self.engine.severed().is_some() || self.check_available().is_err() {
            return ParkMode::Quiesce;
        }
        if matches!(self.handle.0.role, Role::Converses) && self.fleet.launch.trunk.attended() {
            return ParkMode::Held;
        }
        if matches!(self.handle.0.role, Role::Child { .. }) && leased {
            return ParkMode::Engaged;
        }
        if busy {
            return ParkMode::HeldByChildren;
        }
        if self.schedules.armed() {
            return ParkMode::UntilCancelled;
        }
        ParkMode::Quiesce
    }
    fn resume_at(&self, resets_at: jiff::Timestamp, limit: Limit) {
        let now = jiff::Timestamp::now();
        let prompt = format!(
            "The provider refused requests at {} until {}; that time has passed. Carry on where you left off.",
            clock::local(now),
            clock::local(resets_at)
        );
        self.schedules.unschedule(RESUME_LABEL);
        match self.schedules.schedule(
            Trigger::At(resets_at),
            prompt,
            RESUME_LABEL.into(),
            &self.inbox.mailbox(),
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
    pub(super) fn abort_unready(&self) -> std::io::Result<()> {
        let mut log = self.log.borrow_mut();
        log.check_available()?;
        if !log.context().is_ready() {
            log.quiesce(QuiesceReason::Aborted)?;
        }
        Ok(())
    }
}
fn idle_state(mode: ParkMode) -> AgentState {
    match mode {
        ParkMode::HeldByChildren => AgentState::WaitingOnAgents,
        ParkMode::Held | ParkMode::Engaged | ParkMode::UntilCancelled | ParkMode::Quiesce => {
            AgentState::Ready
        }
    }
}
fn agent_outcome(r: &Result<Outcome, ProviderError>) -> AgentOutcome {
    match r {
        Ok(Outcome::Replied) => AgentOutcome::Replied,
        Ok(Outcome::Complete | Outcome::Empty) => AgentOutcome::Failed(NO_REPLY_REASON.into()),
        Ok(Outcome::Stopped { reason }) => AgentOutcome::Stopped(reason.clone()),
        Ok(Outcome::Cancelled) => AgentOutcome::Cancelled,
        Err(e) => AgentOutcome::Failed(e.summary()),
    }
}
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
