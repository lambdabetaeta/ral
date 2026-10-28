//! One prompt run to quiescence against the provider.
//!
//! [`Avatar::deliberate`] drives one round-trip at a time until the model
//! stops calling tools.  A forced eviction is weighed at every turn boundary,
//! against the thresholds in [`gauge`](super::gauge); the standing-condition
//! warnings ride the steering channel at a tool boundary.
//! [`Avatar::attend`] is the loop around this, one call per inbox item.

use crate::agent::Avatar;
use crate::agent::gauge::Warning;
use crate::agent::outcome::{Fault, Outcome};
use crate::agent::tools;
use crate::bus::{Emitter, Next};
use crate::provider::{Delta, Provider, ProviderError, StepOut, StopReason, ToolCall};
use crate::record::ToolResult as SessionToolResult;
use crate::record::commit::Stream;
use crate::record::{AgentState, EditAuthority, Forensic, QuiesceReason, Transient, Voice};
use ral_core::data::FOValue;
use ral_core::process::CancelCause;
use std::ops::ControlFlow;
use std::sync::Arc;

/// What one round-trip leaves the deliberation with.
enum Turn {
    Ends(Outcome),
    /// Results are owed to these before the next round-trip.
    Calls(Vec<ToolCall>),
    /// An overflow an eviction relieved: the round-trip is taken again.
    Retry,
}

impl Avatar {
    /// Run one deliberation: optionally commit `prompt` and its voice, then drive
    /// round-trips to quiescence over `provider`, read once by the caller so a
    /// `/model` swap lands on the next item rather than mid-item.
    ///
    /// # Errors
    /// A provider round-trip failed, the engine was lost, or a session-log
    /// mutation failed.
    pub fn deliberate(
        &mut self,
        provider: &Arc<Provider>,
        prompt: Option<(String, Voice)>,
        continues: Option<u64>,
        emit: &Emitter,
    ) -> Result<Outcome, Fault> {
        self.couple(emit);
        self.agent.disarm_reply_lease();
        // Entry is the one point every route into a deliberation crosses, so a
        // reply staged by a batch a cancel or an error cut short dies here.
        self.reply.borrow_mut().take();
        if let Some((text, voice)) = prompt {
            self.log
                .borrow_mut()
                .append_user(text, continues, voice)
                .map_err(Fault::Log)?;
        }
        let mut retried = false;
        loop {
            // Every turn boundary is weighed, this one included: the turn a
            // cut would take is never the one in hand.
            if self.must_force_evict(provider) {
                self.force_eviction();
            }
            match self.round_trip(provider, retried)? {
                Turn::Ends(outcome) => return Ok(outcome),
                Turn::Retry => retried = true,
                Turn::Calls(calls) => {
                    retried = false;
                    if let ControlFlow::Break(outcome) = self.answer(calls, provider, emit)? {
                        return Ok(outcome);
                    }
                }
            }
        }
    }

    /// One request to the provider, streamed onto the record as it arrives.
    /// `retried` is whether this round-trip already follows an overflow's
    /// eviction, which is never taken twice running.
    fn round_trip(&mut self, provider: &Provider, retried: bool) -> Result<Turn, Fault> {
        let recorder = self.recorder();
        let turn = self.log.borrow().context().next_id();
        recorder
            .emit(Forensic::TurnStarted {
                turn,
                tuning: provider.tuning().clone(),
                via: provider.via().cloned(),
            })
            .map(drop)?;
        let messages = self.log.borrow().render_messages().map_err(Fault::Log)?;
        let system = self.log.borrow().context().system().clone();
        recorder.transient(Transient::State(AgentState::AwaitingModel));
        let mut stream = Stream::default();
        let token = &self.agent.token;
        let taken = provider.complete(
            crate::provider::Request {
                system: &system,
                transcript: &messages,
                tools: self.fleet.launch.tools,
                search: self.agent.search,
            },
            &mut |delta: Delta<'_>| {
                match delta {
                    Delta::Say(t) => recorder.transient(Transient::Token(t.to_string())),
                    Delta::Think(r) => recorder.transient(Transient::Thinking(r.to_string())),
                    Delta::Retry { .. } => {}
                }
                stream.push(&recorder, delta);
                if let Delta::Retry { cause, lost } = delta {
                    recorder.transient(Transient::Boundary);
                    if let Some(usage) = lost {
                        recorder.emit_or_report(Forensic::UsageDelta {
                            usage,
                            generation: None,
                            provider: None,
                        });
                    }
                    recorder.emit_or_report(Forensic::Retried {
                        error: cause.clone(),
                    });
                }
            },
            token,
        );
        let cancelled = token.is_cancelled() || matches!(taken, Err(ProviderError::Cancelled(_)));
        let closed = stream.close(&recorder);
        match taken {
            _ if cancelled => {
                abandoned(&recorder, closed);
                Ok(Turn::Ends(self.cancelled()))
            }
            Ok(step) => {
                closed?;
                self.settle_turn(step)
            }
            Err(error) => {
                abandoned(&recorder, closed);
                self.relieve(error, provider, retried)
            }
        }
    }

    /// A provider failure, unless it is an overflow an eviction relieves.
    fn relieve(
        &self,
        error: ProviderError,
        provider: &Provider,
        retried: bool,
    ) -> Result<Turn, Fault> {
        let ProviderError::Overflow { listed, .. } = &error else {
            return Err(error.into());
        };
        if let Some(window) = provider.context_window()
            && *listed != Some(window)
        {
            self.recorder().emit_or_report(Forensic::WindowNarrowed {
                context_window: window,
            });
        }
        if retried || !self.force_eviction() {
            return Err(error.into());
        }
        self.note(format!(
            "[{}; evicted the older turns and retrying]",
            error.summary()
        ));
        Ok(Turn::Retry)
    }

    /// Commit a completed round-trip, and read off what it leaves.
    fn settle_turn(&mut self, step: StepOut) -> Result<Turn, Fault> {
        let StepOut {
            mut assistant_message,
            tool_calls,
            usage,
            stop_reason,
            cut_short,
            served,
        } = step;
        // The committed message, not the streaming accumulator, is the
        // outcome's source of truth: a provider may return final text with no
        // `Say` deltas.  Read before `admit_assistant` can stub empty content.
        let spoke = assistant_message
            .content
            .first_text()
            .is_some_and(|text| !text.is_empty());
        self.measure(usage, served)?;
        // Routine boundaries and `MaxTokens` (handled below) stay silent.
        if let Some(reason) = &stop_reason
            && !matches!(
                reason,
                StopReason::Completed(_) | StopReason::ToolCall(_) | StopReason::MaxTokens(_)
            )
        {
            self.recorder()
                .transient(Transient::StopReason(reason.raw().to_string()));
        }
        admit_assistant(&mut assistant_message);
        let tool_ids = tool_calls.iter().map(|tc| tc.call_id.clone()).collect();
        self.log
            .borrow_mut()
            .append_assistant(
                assistant_message,
                tool_ids,
                stop_reason.as_ref().map(|r| r.raw().to_string()),
            )
            .map_err(Fault::Log)?;
        match (cut_short, tool_calls.is_empty()) {
            // The cut rides on whole, cause and all: the caller's own
            // `Forensic::ProviderError` is its one record, and the renderers
            // read the remedy off the cause itself.
            (Some(cause), true) => Err(ProviderError::Truncated {
                cause: Box::new(cause),
            }
            .into()),
            // The session now awaits tool results, so returning would strand
            // it there: the next round-trip resumes with them instead.
            (cut, false) => {
                if cut.is_some() {
                    self.note("[Truncated mid-tool-call; continuing]".into());
                }
                Ok(Turn::Calls(tool_calls))
            }
            (None, true) => Ok(Turn::Ends(match stop_reason {
                None | Some(StopReason::Completed(_) | StopReason::ToolCall(_)) => {
                    if spoke {
                        Outcome::Complete
                    } else {
                        Outcome::Empty
                    }
                }
                Some(reason) => Outcome::Stopped {
                    reason: reason.raw().to_string(),
                },
            })),
        }
    }

    /// Answer the model's calls, then take in what arrived meanwhile and
    /// what has newly climbed.  `Break` ends the deliberation.
    fn answer(
        &mut self,
        calls: Vec<ToolCall>,
        provider: &Provider,
        emit: &Emitter,
    ) -> Result<ControlFlow<Outcome>, Fault> {
        let results = self.run_batch(calls, emit);
        self.log
            .borrow_mut()
            .append_tool_results(results)
            .map_err(Fault::Log)?;
        self.readings.nudges.reset();
        // A call in the batch may have found the engine gone.
        if let Some(s) = self.seat.severed() {
            return Err(s.into());
        }
        // A cancelled batch admits nothing: a read run or a steer recorded
        // here would belong to the exchange being dropped.
        if self.agent.token.is_cancelled() {
            return Ok(ControlFlow::Break(self.cancelled()));
        }
        // Reply closes admission; arrivals stay queued for the next draw.
        if let Some(payload) = self.reply.borrow_mut().take() {
            return Ok(ControlFlow::Break(self.replied(payload)));
        }
        // Arrivals are taken in the order typed: a read runs against the
        // context as it stands at its place in the queue, a delivery lands as
        // a steering line of its own.  The warnings trail them.
        for drawn in self.inbox.drain_drawn(&self.work, &self.agent.token) {
            #[cfg(test)]
            tests::after_drain_draw(self);
            if drawn.stamp.admit(&self.agent.token, || ()).is_none() {
                if let Next::Item(crate::bus::Item::Human(text)) = drawn.next {
                    self.log
                        .borrow_mut()
                        .append_steering(text, Voice::Human)
                        .map_err(Fault::Log)?;
                }
                continue;
            }
            match drawn.next {
                Next::Read(read) => self.read(&read, emit),
                Next::Item(item) => {
                    if self.arrive(&item)? {
                        self.log
                            .borrow_mut()
                            .append_steering(item.text(), item.voice())
                            .map_err(Fault::Log)?;
                    }
                }
                Next::Rewrite(_) => unreachable!("the mid-exchange drain holds at a rewrite"),
            }
        }
        if self.agent.token.is_cancelled() || !self.work.current(&self.agent.token) {
            return Ok(ControlFlow::Break(self.cancelled()));
        }
        let climbed = self.climbed(provider)?;
        let steering = self.tell(climbed);
        if !steering.is_empty() {
            self.log
                .borrow_mut()
                .append_steering(steering.join("\n"), Voice::Harness)
                .map_err(Fault::Log)?;
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Note each user line; word each model one as a reminder, returned as
    /// steering.
    fn tell(&self, warnings: Vec<Warning>) -> Vec<String> {
        warnings
            .into_iter()
            .filter_map(|warning| match warning {
                Warning::User(line) => {
                    self.note(line);
                    None
                }
                Warning::Model(reminder) => {
                    self.readings.nudges.remind(&reminder, &self.log.borrow())
                }
            })
            .collect()
    }

    /// Force-evict the older half of the context, the harness writing no note of its
    /// own: the model's own `` exarch-context `evict `` is where a note comes
    /// from. Whether anything was evicted.
    pub(crate) fn force_eviction(&self) -> bool {
        if !self.log.borrow().context().can_evict() {
            return false;
        }
        // A boundary Esc leaves the context as it lies; the next boundary
        // weighs it again.
        if self.agent.token.is_cancelled() {
            return false;
        }
        // No turn old enough to shed is a no-op, not an event.
        let Some(span) = self.planned_forced_eviction() else {
            return false;
        };
        self.recorder()
            .transient(Transient::State(AgentState::Evicting));
        if let Err(e) = self.evict_unbidden(span, EditAuthority::Harness) {
            self.note_error(&format!("evict failed: {e}"));
            return false;
        }
        true
    }

    /// Run a batch of tool calls in order, short-circuiting the rest to
    /// cancelled results the instant the token trips.  Every call answers
    /// synchronously — a `spawn` returns a start receipt, not a join handle.
    fn run_batch(&self, tool_calls: Vec<ToolCall>, emit: &Emitter) -> Vec<SessionToolResult> {
        let mut results = Vec::with_capacity(tool_calls.len());
        let mut it = tool_calls.into_iter();
        for call in it.by_ref() {
            if self.agent.token.is_cancelled() {
                results.push(cancelled_result(call.call_id));
                results.extend(it.map(|r| cancelled_result(r.call_id)));
                break;
            }
            // A severed seat runs nothing more, so the rest answer its loss.
            if let Some(s) = self.seat.severed() {
                let lost = crate::agent::EngineLost::running(&s, self.agent.run_dir()).to_string();
                results.extend(std::iter::once(call).chain(it).map(|r| SessionToolResult {
                    id: r.call_id,
                    content: lost.clone(),
                }));
                break;
            }
            results.push(self.invoke(call, emit));
        }
        results
    }

    fn invoke(&self, call: ToolCall, emit: &Emitter) -> SessionToolResult {
        // The names this agent recognises are exactly the ones its request
        // advertised.  Every harness verb (`agent`, `reply`, `schedule`, …)
        // is a builtin *inside* `ral`, not a tool of its own.
        let ToolCall {
            call_id,
            fn_name,
            fn_arguments,
            ..
        } = call;
        let offered = self.fleet.launch.tools.contains(&fn_name);
        let answered = offered
            .then(|| tools::dispatch(&fn_name, call_id.clone(), &fn_arguments, self, emit))
            .flatten();
        answered.unwrap_or_else(|| {
            let msg = format!("unknown tool `{fn_name}`");
            self.note_error(&msg);
            SessionToolResult {
                id: call_id,
                content: msg,
            }
        })
    }

    /// The batch carried a `reply`.  Live descendants are cancelled and
    /// reaped, the value is deposited for this agent's consumer, and the
    /// round-trip — which never asked for a final assistant message, so the
    /// session sits in `AwaitingAssistantAfterToolResults` — is wound back
    /// with its own breadcrumb.  A child then parks, deposit and all; only a
    /// parentless agent's `attend` loop stops here.
    fn replied(&self, payload: FOValue) -> Outcome {
        self.agent.cancel_descendants(CancelCause::Cancelled);
        self.recorder()
            .transient(Transient::State(AgentState::Ready));
        self.agent.deposit_reply(&self.fleet, payload);
        self.log.borrow_mut().quiesce(QuiesceReason::Replied);
        Outcome::Replied
    }

    /// The log already carries `Cancelled` through `quiesce`'s own
    /// `Forensic::Cancelled` record; the view fold draws it, so there is no
    /// separate user-facing companion left to emit.
    fn cancelled(&self) -> Outcome {
        self.log.borrow_mut().quiesce(QuiesceReason::Cancelled);
        Outcome::Cancelled
    }
}

/// A turn's close on an exit already cancelling or failing: the streamed
/// prefix is what the user saw, so it still commits, best-effort, and the
/// exit in flight stays the one reported.
fn abandoned(recorder: &crate::record::Emitter, closed: std::io::Result<()>) {
    if let Err(error) = closed {
        recorder.report_fault(&error);
    }
}

fn cancelled_result(id: String) -> SessionToolResult {
    SessionToolResult {
        id,
        content: "[EXARCH // Request interrupted by user, before tool execution.]".into(),
    }
}

/// Stands in for an assistant message that would serialise to no substantive
/// content.  Anthropic rejects an empty assistant turn once a later message
/// makes it non-final, and the empty nudge appends a user prompt right after.
const EMPTY_ASSISTANT_STUB: &str = "(no content)";

/// Normalise an assistant message to the commit-boundary invariant: every
/// committed message serialises to a request every supported provider accepts.
/// A tool call whose `fn_arguments` is not a JSON object is repaired to `{}`,
/// since genai's Anthropic adapter repairs only a `null` and a bare string or
/// array re-serialises verbatim, 400ing every later request that carries it.
fn admit_assistant(msg: &mut genai::chat::ChatMessage) {
    use genai::chat::ContentPart;
    for part in &mut msg.content {
        if let ContentPart::ToolCall(tc) = part
            && !tc.fn_arguments.is_object()
        {
            tc.fn_arguments = serde_json::json!({});
        }
    }
    let substantive = msg.content.iter().any(|p| match p {
        ContentPart::Text(t) => !t.trim().is_empty(),
        ContentPart::ToolCall(_) | ContentPart::Binary(_) | ContentPart::ToolResponse(_) => true,
        // Reasoning and thought signatures ride alongside real content,
        // never as it: alone they are not a renderable turn.
        ContentPart::ThoughtSignature(_)
        | ContentPart::ReasoningContent(_)
        | ContentPart::Custom(_) => false,
    });
    if !substantive {
        msg.content = genai::chat::MessageContent::from_text(EMPTY_ASSISTANT_STUB);
    }
}

#[cfg(test)]
mod tests;
