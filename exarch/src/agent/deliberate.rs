//! One prompt run to quiescence against the provider.
//!
//! [`Avatar::deliberate`] drives the provider until it stops calling tools.
//! Auto-eviction is weighed at every turn boundary, against the thresholds in
//! [`gauge`]; the standing-condition warnings ride the
//! steering channel at a tool boundary.  [`Avatar::attend`] is the loop around
//! this, one call per inbox item.

use crate::agent::Avatar;
use crate::agent::attend::announce;
use crate::agent::gauge::{self, EVICT_THRESHOLD, Measure, suffix_keep_budget};
use crate::agent::tools;
use crate::bus::{Emitter, Next};
use crate::provider::{Delta, Provider, ProviderError, StepOut, StopReason, ToolCall};
use crate::record::AgentState;
use crate::record::Transient;
use crate::record::{EditAuthority, QuiesceReason, ToolResult as SessionToolResult};
use ral_core::carrier::Severed;
use ral_core::first_order::FOValue;
use std::io;
use std::sync::Arc;

/// Outcome of one [`Avatar::deliberate`].  [`Self::Empty`] and
/// [`Self::Stopped`] become nudges, [`Self::Cancelled`] does not.
#[derive(Debug)]
pub enum Outcome {
    /// The model spoke and called no tool.
    Complete,
    /// A returning agent called `reply`: the value is deposited on its agent
    /// for its consumer to read.  Distinct from [`Self::Complete`] so the
    /// nudge layer never re-nudges an agent that already answered.
    Replied,
    Empty,
    Stopped {
        reason: String,
    },
    Cancelled,
}

/// Why a deliberation ended short of an [`Outcome`].
#[derive(Debug)]
pub enum Fault {
    /// The provider's own failure, which the attend loop records and may
    /// nudge over.
    Provider(ProviderError),
    /// The engine is gone: no further tool call can dispatch, and the loop
    /// around this one ends too.
    Severed(Severed),
    /// The session log refused or failed an append; the context is left as
    /// it lay.
    Log(String),
}

impl From<ProviderError> for Fault {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

impl From<Severed> for Fault {
    fn from(severed: Severed) -> Self {
        Self::Severed(severed)
    }
}

impl From<io::Error> for Fault {
    fn from(error: io::Error) -> Self {
        Self::Log(error.to_string())
    }
}

impl Avatar {
    /// Run one deliberation: optionally commit `prompt`, then drive the
    /// provider round-trip loop to quiescence over `provider`, read once by
    /// the caller so a `/model` swap lands on the next item rather than
    /// mid-item.
    ///
    /// # Errors
    /// A provider round-trip failed, the engine was lost, or a session-log
    /// mutation failed.
    ///
    /// # Panics
    /// Panics if a turn is truncated with no tool calls yet no cut-short cause
    /// was recorded.
    pub fn deliberate(
        &mut self,
        provider: &Arc<Provider>,
        prompt: Option<String>,
        continues: Option<u64>,
        emit: &Emitter,
    ) -> Result<Outcome, Fault> {
        self.couple(emit);
        // The commit producer's handle: answer paragraphs and reasoning runs
        // are recorded as they are decided, worker-side.
        let recorder = self.recorder();
        let token = self.agent.token.clone();
        // A reply staged by a batch a cancel or an error cut short must not
        // outlive it; entry is the one point every route into a deliberation
        // is guaranteed to cross.
        self.reply.take();
        if let Some(p) = prompt {
            self.log
                .borrow_mut()
                .append_user(p, continues)
                .map_err(Fault::Log)?;
        }
        loop {
            // Every turn boundary is weighed, this one included: the turn a
            // cut would take is never the one in hand, so there is no point in
            // the loop where the work at issue could leave.
            self.evict(provider, false);
            // The turn's live row derives from the published `Display::Turn`
            // record, which `record_turn_start` authors alongside the
            // protocol one — so this is the one authoring site.
            self.log.borrow_mut().record_turn_start(
                provider.tuning().clone(),
                provider.openrouter_route().map(str::to_owned),
            )?;
            let messages = self.log.borrow().render_messages().map_err(Fault::Log)?;
            recorder.transient(Transient::State(AgentState::AwaitingModel));
            // One producer per turn, sealed at whichever boundary ends the
            // stream below.  A streaming callback has no error channel of its
            // own, so the first failed commit is stashed and answered at that
            // boundary; nothing commits after it, a half-ordered scrollback
            // being worse than a short one.
            let mut stream = crate::record::commit::Stream::default();
            let mut unrecorded: Option<io::Error> = None;
            let taken = {
                let stream = &mut stream;
                let unrecorded = &mut unrecorded;
                provider.complete(
                    crate::provider::Request {
                        system: &self.agent.system,
                        transcript: &messages,
                        tools: self.fleet.launch.tools,
                        search: self.agent.search,
                    },
                    &mut |delta: Delta<'_>| {
                        match delta {
                            Delta::Say(t) => recorder.transient(Transient::Token(t.to_string())),
                            Delta::Think(r) => {
                                recorder.transient(Transient::Thinking(r.to_string()));
                            }
                        }
                        if unrecorded.is_none()
                            && let Err(error) = stream.push(&recorder, delta)
                        {
                            *unrecorded = Some(error);
                        }
                    },
                    &token,
                )
            };
            if token.is_cancelled() {
                abandon_turn(&mut stream, unrecorded, &recorder);
                return Ok(self.cancelled());
            }
            let StepOut {
                mut assistant_message,
                tool_calls,
                usage,
                stop_reason,
                cut_short,
            } = match taken {
                Ok(s) => s,
                Err(ProviderError::Cancelled(_)) => {
                    abandon_turn(&mut stream, unrecorded, &recorder);
                    return Ok(self.cancelled());
                }
                Err(e) => {
                    abandon_turn(&mut stream, unrecorded, &recorder);
                    return Err(e.into());
                }
            };
            close_turn(&mut stream, unrecorded, &recorder)?;
            // The committed message is the outcome's source of truth, not the
            // streaming accumulator: a provider that returns final text with
            // no `Say` deltas would otherwise read as an empty turn. Read
            // before `admit_assistant` below can replace empty content with
            // its stub.
            let spoke = assistant_message
                .content
                .first_text()
                .is_some_and(|text| !text.is_empty());
            // The live numerator the next `evict` weighs against the window.
            let measured_at = {
                let mut log = self.log.borrow_mut();
                log.record_usage(usage)?;
                log.context().log_len()
            };
            self.readings.measure = Some(Measure {
                tokens: usage.input,
                at: measured_at,
            });
            // Routine boundaries and `MaxTokens` (handled below) stay silent.
            if let Some(reason) = &stop_reason {
                match reason {
                    StopReason::Completed(_)
                    | StopReason::ToolCall(_)
                    | StopReason::MaxTokens(_) => {}
                    _ => recorder.transient(Transient::StopReason(reason.raw().to_string())),
                }
            }
            admit_assistant(&mut assistant_message);
            let tool_ids: Vec<String> = tool_calls.iter().map(|tc| tc.call_id.clone()).collect();
            self.log
                .borrow_mut()
                .append_assistant(
                    assistant_message,
                    tool_ids,
                    stop_reason.as_ref().map(|r| r.raw().to_string()),
                )
                .map_err(Fault::Log)?;
            let truncated = cut_short.is_some();
            // A cut-short turn with no tool calls is final; `Truncated` lets the
            // nudge re-drive it.  With tool calls the session now sits in
            // `AwaitingToolResults`, and returning would strand it there — the
            // nudge's `append_user` would fail on pending results — so fall
            // through and let the next round-trip resume with the results.
            if truncated && tool_calls.is_empty() {
                // The cut rides on whole, cause and all: the caller's own
                // `record_provider_error` is the one record of it, and the
                // renderers read the remedy — and, for a stall, the fact that
                // the exchange survives it — off the cause itself.
                let cause = cut_short.expect("truncated implies cut_short");
                return Err(ProviderError::Truncated {
                    cause: Box::new(cause),
                }
                .into());
            }
            if tool_calls.is_empty() {
                return Ok(match &stop_reason {
                    Some(r) if !matches!(r, StopReason::Completed(_) | StopReason::ToolCall(_)) => {
                        Outcome::Stopped {
                            reason: r.raw().to_string(),
                        }
                    }
                    _ if spoke => Outcome::Complete,
                    _ => Outcome::Empty,
                });
            }
            if truncated {
                self.note("[Truncated mid-tool-call; continuing]".into());
            }
            let results = self.run_batch(tool_calls, emit);
            self.log
                .borrow_mut()
                .append_tool_results(results)
                .map_err(Fault::Log)?;
            // A tool call in the batch just run may have found the engine
            // gone; no further dispatch can run once it has.
            if let Some(s) = self.seat.severed() {
                return Err(s.into());
            }
            // A cancelled batch admits nothing: a read run or a steer recorded
            // here would belong to the exchange being dropped.
            if token.is_cancelled() {
                return Ok(self.cancelled());
            }
            // The arrivals are taken in the order typed: a read runs against
            // the context as it stands at its place in the queue, a delivery
            // lands as a steering line of its own, `announce` drawing each
            // arrival's chrome.  The warnings trail them.
            for next in self.inbox.drain_mid_exchange() {
                match next {
                    Next::Read(read) => self.read(&read, emit),
                    Next::Item(item) => {
                        self.heard(&item);
                        announce(&item, None, &recorder);
                        self.log
                            .borrow_mut()
                            .append_steering(item.text())
                            .map_err(Fault::Log)?;
                    }
                    Next::Rewrite(_) => unreachable!("the mid-exchange drain holds at a rewrite"),
                }
            }
            let warnings = self.warnings(provider)?;
            if !warnings.is_empty() {
                self.log
                    .borrow_mut()
                    .append_steering(warnings.join("\n"))
                    .map_err(Fault::Log)?;
            }
            // A `reply` in the fully drained batch ends the run here, not at
            // another round-trip.
            if let Some(payload) = self.reply.take() {
                return Ok(self.replied(payload));
            }
        }
    }

    /// The turns an eviction would take were it to run now, `None` when
    /// nothing is old enough to shed.
    /// [`crate::record::model::Context::plan_eviction`] never names the
    /// newest turn, so this never answers with the work in hand either.
    pub(crate) fn planned_eviction(&self) -> Option<Vec<u64>> {
        let log = self.log.borrow();
        let context = log.context();
        let plan = context
            .can_evict()
            .then(|| context.plan_eviction(suffix_keep_budget(context.history_bytes())));
        drop(log);
        plan.flatten()
    }

    /// Shed the older half of the context, the harness writing no note of its
    /// own: the model's own `` exarch-context `evict `` is where a note comes from.
    pub(crate) fn evict(&self, provider: &Arc<Provider>, requested: bool) {
        if !self.log.borrow().context().can_evict() {
            if requested {
                self.note_error("cannot evict while tool results are pending");
            }
            return;
        }
        // Auto-eviction tracks real context pressure: the tokens the model
        // last saw against its window, firing once they grow into the reserve.
        // An unknown window falls back to the byte heuristic; a manual
        // `/evict` overrides the trigger entirely.
        let due = match provider.context_window() {
            Some(window) if window > 0 => self
                .measured_input()
                .is_some_and(|tokens| gauge::eviction_due(tokens, window)),
            _ => self.log.borrow().context().history_bytes() >= EVICT_THRESHOLD,
        };
        if !requested && !due {
            return;
        }
        // A boundary Esc leaves the context as it lies; the next boundary
        // weighs it again.
        if self.agent.token.is_cancelled() {
            return;
        }
        // No turn old enough to shed is a no-op, not an event.
        let Some(turns) = self.planned_eviction() else {
            return;
        };
        self.recorder()
            .transient(Transient::State(AgentState::Evicting));
        if let Err(e) = self.evict_unbidden(&turns, EditAuthority::Harness) {
            self.note_error(&format!("evict failed: {e}"));
        }
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
                let lost =
                    crate::agent::seat::EngineLost::running(&s, self.agent.run_dir()).to_string();
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
        self.agent
            .cancel_descendants(ral_core::process::CancelCause::Cancelled);
        self.agent.deposit_reply(payload);
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

fn cancelled_result(id: String) -> SessionToolResult {
    SessionToolResult {
        id,
        content: "[EXARCH // Request interrupted by user, before tool execution.]".into(),
    }
}

/// Close a streaming turn: every commit it still owes, then the boundary
/// that lets a printer retire its live edge.  The boundary follows the seal
/// whether or not it held — a live edge outliving its turn is the one
/// failure the printer cannot recover from on its own.
///
/// A commit the streaming callback could not make is answered here, since a
/// callback has no error channel of its own; nothing commits on top of it,
/// the order being broken already.
///
/// # Errors
/// The first commit that failed anywhere in the turn.
fn close_turn(
    stream: &mut crate::record::commit::Stream,
    unrecorded: Option<io::Error>,
    recorder: &crate::record::Emitter,
) -> io::Result<()> {
    let sealed = match unrecorded {
        Some(error) => Err(error),
        None => stream.seal(recorder),
    };
    recorder.transient(Transient::Boundary);
    sealed
}

/// [`close_turn`] on an exit path that is already cancelling or erroring:
/// the streamed prefix is what the user saw, so it still commits,
/// best-effort — the exit in flight is the error being reported, and this
/// one must not mask it.
fn abandon_turn(
    stream: &mut crate::record::commit::Stream,
    unrecorded: Option<io::Error>,
    recorder: &crate::record::Emitter,
) {
    if let Err(error) = close_turn(stream, unrecorded, recorder) {
        recorder.report_fault(&error);
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
