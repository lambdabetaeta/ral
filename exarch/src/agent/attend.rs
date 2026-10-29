//! One life, attending exchanges until teardown joins its helpers.

use crate::agent::arrival::Prompt;
use crate::agent::engine::Engine;
use crate::agent::nursery::Teardown;
use crate::agent::outcome::{Fault, Outcome};
use crate::agent::{Agent, EngineLost, nudge};
use crate::bus::{
    AgentOutcome, ENDED_BEFORE_REPORTING, Emitter, Next, Rest, WORKER_PANIC_PREFIX, panic_msg,
};
use crate::clock;
use crate::provider::{Limit, ProviderError, Recovery};
use crate::record::{AgentState, ChildStatus, Forensic, QuiesceReason, Transient, Unavailable};
use crate::schedule::Trigger;
use ral_core::process::Scope;

const RESUME_LABEL: &str = "provider-reset";
const NO_REPLY_REASON: &str = "ended without calling `reply`";

impl Agent {
    pub(crate) fn run(mut self, emit: &Emitter) -> Teardown {
        #[cfg(test)]
        hooks::before_run(self.id());
        self.couple(emit);
        let mut last = None;
        loop {
            let rest = self.rest();
            if self.handle.is_live() {
                self.recorder().transient(Transient::State(if rest.pending {
                    AgentState::WaitingOnAgents
                } else {
                    AgentState::Ready
                }));
            }
            match self.mailbox.next(rest) {
                Next::Ended => break,
                Next::Settled { stopped } => self.settled(stopped, last.take()),
                Next::Command(command) => {
                    if self.command(command, emit).is_break() {
                        break;
                    }
                }
                Next::Take { input, exchange } => {
                    let Some(prompt) = self.admit(input) else {
                        continue;
                    };
                    if !self.mailbox.begin() {
                        break;
                    }
                    self.body.state.readings.nudges.reset();
                    last = Some(self.exchange(&exchange, prompt, emit));
                }
            }
        }
        self.teardown()
    }

    pub(super) fn rest(&self) -> Rest {
        let nursery = &self.body.state.nursery;
        Rest {
            pending: nursery.owing(),
            schedules: self.handle.0.role.returns() && self.body.state.schedules.armed(),
        }
    }

    fn exchange(&mut self, exchange: &Scope, mut prompt: Prompt, emit: &Emitter) -> AgentOutcome {
        loop {
            let outcome = match self.attempt(exchange, prompt, emit) {
                Ok(outcome) => Ok(outcome),
                Err(Fault::Provider(error)) => {
                    self.record_failure(&error);
                    Err(error)
                }
                Err(Fault::Severed(cause)) => {
                    return AgentOutcome::Failed(
                        EngineLost::running(&cause, self.run_dir()).to_string(),
                    );
                }
                Err(Fault::Unavailable(failure)) => {
                    return AgentOutcome::Failed(failure.to_string());
                }
                Err(Fault::Refused(why) | Fault::Panicked(why)) => {
                    self.note_error(&why);
                    return AgentOutcome::Failed(why);
                }
            };
            let final_outcome = agent_outcome(&outcome);
            // A12: read debts before checking the queue.
            let rest = self.rest();
            if exchange.is_cancelled() || self.mailbox.has_input() {
                return final_outcome;
            }
            let workers_idle = match self.engine().read(|transport| transport.workers()) {
                Ok(workers) => workers.is_empty(),
                Err(cause) => {
                    return AgentOutcome::Failed(
                        EngineLost::running(&cause, self.run_dir()).to_string(),
                    );
                }
            };
            let facts = nudge::Facts {
                must_reply: self.body.state.owed,
                pinned: self.pinned_digest(),
                quiet: workers_idle && !rest.pending && !rest.schedules,
            };
            let body = &mut *self.body;
            let Some(text) = body
                .state
                .readings
                .nudges
                .react(outcome.as_ref(), &facts, &body.log)
            else {
                return final_outcome;
            };
            let Some(continues) = body.log.context().current_prompt() else {
                self.note_error(&format!(
                    "a nudge has no prompt to continue: dropping it: {text}"
                ));
                return final_outcome;
            };
            prompt = Prompt {
                text,
                voice: crate::record::Voice::Harness,
                continues: Some(continues),
            };
        }
    }

    fn attempt(
        &mut self,
        exchange: &Scope,
        prompt: Prompt,
        emit: &Emitter,
    ) -> Result<Outcome, Fault> {
        let active = self.body.state.provider.clone();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.deliberate(
                exchange,
                &active,
                Some((prompt.text, prompt.voice)),
                prompt.continues,
                emit,
            )
        }))
        .unwrap_or_else(|payload| {
            self.abort_unready()?;
            Err(Fault::Panicked(format!(
                "{WORKER_PANIC_PREFIX}{}",
                panic_msg(&payload)
            )))
        })
    }

    pub(super) fn report(&mut self, status: ChildStatus) {
        self.recorder().emit_or_end(Forensic::Status {
            status: status.clone(),
        });
        self.body.state.owed = false;
        self.body.state.schedules.clear();
        self.handle.report(status);
    }

    fn settled(&mut self, stopped: bool, last: Option<AgentOutcome>) {
        if self.body.state.owed {
            self.report(if stopped {
                ChildStatus::Stopped
            } else {
                ChildStatus::Failed(
                    last.map_or_else(|| NO_REPLY_REASON.into(), AgentOutcome::reason),
                )
            });
        }
    }

    /// A8, once: a later call finds the engine retired and does nothing.
    pub(crate) fn teardown(&mut self) -> Teardown {
        // Read before the engine ends: its detach's EOF severs it.
        let Some(severed) = self.engine.as_ref().map(Engine::severed) else {
            return Teardown {
                failures: Vec::new(),
            };
        };
        self.body.state.schedules.clear();
        self.handle.end();
        if self.body.state.owed || self.handle.status().owes_parent {
            let reason = severed.as_ref().map_or_else(
                || ENDED_BEFORE_REPORTING.to_string(),
                |cause| EngineLost::running(cause, self.run_dir()).to_string(),
            );
            self.report(ChildStatus::Failed(reason));
        }
        let failures = self.body.state.nursery.dismiss_all();
        self.body.state.failures.extend(failures);
        if let Some(cause) = severed {
            self.recorder()
                .emit_or_end(Forensic::EngineSevered { cause });
        }
        let engine = self.engine.take().expect("teardown found the engine");
        self.body.retire_engine(&self.handle.0.name, &engine);
        Teardown {
            failures: std::mem::take(&mut self.body.state.failures),
        }
    }

    fn pinned_digest(&self) -> Option<String> {
        let lines: Vec<_> = self
            .body
            .state
            .pins
            .values()
            .map(|pin| crate::card::summary_line(&pin.card))
            .collect();
        (!lines.is_empty()).then(|| lines.join("; "))
    }

    fn record_failure(&self, error: &ProviderError) {
        self.recorder().emit_or_end(Forensic::ProviderError {
            error: error.clone(),
        });
        if let ProviderError::Refused(refusal) = error
            && self.resumes_at_reset()
            && let Recovery::Deferred(at) = refusal.recovery()
        {
            self.resume_at(at, refusal.limit);
        }
    }

    fn resume_at(&self, resets_at: jiff::Timestamp, limit: Limit) {
        let now = jiff::Timestamp::now();
        let prompt = format!(
            "The provider refused requests at {} until {}; that time has passed. Carry on where you left off.",
            clock::local(now),
            clock::local(resets_at)
        );
        self.body.state.schedules.unschedule(RESUME_LABEL);
        match self.body.state.schedules.schedule(
            Trigger::At(resets_at),
            prompt,
            RESUME_LABEL.into(),
            &self.mailbox.address(),
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

    pub(super) fn abort_unready(&mut self) -> Result<(), Unavailable> {
        if !self.body.log.context().is_ready() {
            self.body.log.quiesce(QuiesceReason::Aborted)?;
        }
        Ok(())
    }
}

fn agent_outcome(result: &Result<Outcome, ProviderError>) -> AgentOutcome {
    match result {
        Ok(Outcome::Replied) => AgentOutcome::Replied,
        Ok(Outcome::Complete | Outcome::Empty) => AgentOutcome::Failed(NO_REPLY_REASON.into()),
        Ok(Outcome::Stopped { reason }) => AgentOutcome::Stopped(reason.clone()),
        Ok(Outcome::Cancelled) => AgentOutcome::Cancelled,
        Err(error) => AgentOutcome::Failed(error.summary()),
    }
}

#[cfg(test)]
pub(crate) mod hooks {
    use crate::record::AgentId;
    use ral_core::sync::LockExt;
    use std::sync::Mutex;

    static PANICS: Mutex<Vec<AgentId>> = Mutex::new(Vec::new());

    /// `id`'s `run` panics before attending, outside every recovery boundary.
    pub(crate) fn panic_on_run(id: AgentId) {
        PANICS.lock_ignore_poison().push(id);
    }

    pub(super) fn before_run(id: AgentId) {
        assert!(
            !PANICS.lock_ignore_poison().contains(&id),
            "injected panic in run"
        );
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
