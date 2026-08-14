//! One prompt run to quiescence against the provider.
//!
//! [`Agent::deliberate`] steps the provider until it stops calling tools,
//! bounded by [`MAX_STEPS`] since a headless run has no Esc to hand.
//! Auto-compaction is checked once, at entry — the sole boundary guaranteed
//! `ReadyForUser` — against the policy in [`digest`](crate::agent::digest).
//! [`Agent::attend`] is the loop around this, one call per inbox item.

use crate::agent::Agent;
use crate::agent::attend::announce;
use crate::agent::cancel;
use crate::agent::digest::{
    COMPACT_THRESHOLD, SUMMARY_CAP_FALLBACK_TOKENS, suffix_keep_budget, summary_cap_tokens,
};
use crate::agent::event::{
    ContextOp, EditAuthority, QuiesceReason, ToolResult as SessionToolResult,
};
use crate::bus::{AgentState, Emitter, Item, Kind};
use crate::provider::{CutShort, Provider, ProviderError, StepOut, StopReason, ToolCall};
use ral_core::serial::FOValue;
use std::sync::Arc;

/// Outcome of one [`Agent::deliberate`]; hard failures travel through
/// [`ProviderError`] instead.  [`Self::Empty`] and [`Self::Stopped`] become
/// nudges, [`Self::Cancelled`] and [`Self::Capped`] do not.
#[derive(Debug)]
pub enum Outcome {
    Complete(String),
    /// A returning agent called `reply`.  The payload stays a value so each
    /// consumer renders it at its own edge, and stays distinct from
    /// [`Self::Complete`] so the nudge layer never re-nudges an agent that
    /// already returned.  Terminal.
    Replied(FOValue),
    Empty,
    Stopped {
        reason: String,
    },
    Cancelled,
    /// Hit [`MAX_STEPS`] without a tool-call-free reply.  Terminal and
    /// nudge-free: re-attending would just spend another [`MAX_STEPS`].
    Capped,
}

/// Hard ceiling on provider round-trips in one [`Agent::deliberate`].  The
/// interactive frontend has Esc to halt a runaway; headless and autonomous runs
/// have nothing.  Generous enough that no genuine deliberation reaches it.
const MAX_STEPS: u32 = 250;

impl Agent {
    /// Run one deliberation: optionally commit `prompt`, then step the provider
    /// round-trip loop to quiescence.
    ///
    /// # Errors
    /// Returns `Err` if a provider round-trip fails, or if a session-log
    /// mutation does and is surfaced as `ProviderError::Other`.
    ///
    /// # Panics
    /// Panics if a step is truncated with no tool calls yet no cut-short cause
    /// was recorded.
    pub fn deliberate(
        &mut self,
        provider: &Arc<Provider>,
        prompt: Option<String>,
        continues: Option<u64>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> Result<Outcome, ProviderError> {
        self.couple(emit);
        // The commit producer's handle: answer chunks and the step's final
        // reasoning record through the seam as they are decided, worker-side.
        let recorder = self.recorder();
        // A staged reply must not outlive the batch that staged it when a cancel
        // or an error lands between `invoke` and the post-batch drain; entry is
        // the one point every route into a deliberation is guaranteed to cross.
        self.reply = None;
        // Entry is `ReadyForUser`, before the prompt is committed: the only
        // place `can_compact()` is guaranteed to hold, and every exchange and
        // nudge alike crosses it.
        self.compact(provider, emit, false, token, continues);
        let mut last_text = String::new();
        if let Some(p) = prompt {
            self.log
                .lock()
                .append_user(p, continues)
                .map_err(ProviderError::Other)?;
        }
        let mut n = 0u32;
        loop {
            n += 1;
            if n > MAX_STEPS {
                return Ok(self.capped(emit));
            }
            // The step's live row derives from the published record — the
            // retired `Kind::Step` twin — so this is the one authoring site.
            self.log
                .lock()
                .record_step(n, provider.tuning().clone())
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            last_text.clear();
            #[cfg(debug_assertions)]
            let t_render = std::time::Instant::now();
            let messages = self
                .log
                .lock()
                .render_messages()
                .map_err(ProviderError::Other)?;
            ral_core::dbg_trace!(
                "deliberate",
                "render_messages: {} msgs in {:?}",
                messages.len(),
                t_render.elapsed()
            );
            #[cfg(debug_assertions)]
            let t_req = std::time::Instant::now();
            #[cfg(debug_assertions)]
            let mut first_token: Option<std::time::Duration> = None;
            emit.emit(Kind::State(AgentState::AwaitingModel));
            // One chopper per step, flushed at whichever boundary seals the
            // stream below; a failed answer commit is stashed here, since a
            // streaming closure has no error channel of its own.
            let mut chopper = crate::record::commit::Chopper::default();
            let mut chop_error: Option<std::io::Error> = None;
            let step_out = {
                let token_emit = emit.clone();
                let reasoning_emit = emit.clone();
                let chopper = &mut chopper;
                let chop_error = &mut chop_error;
                provider.complete(
                    &self.system,
                    messages,
                    self.tool_enabled,
                    self.search,
                    &mut |t: &str| {
                        #[cfg(debug_assertions)]
                        if first_token.is_none() {
                            first_token = Some(t_req.elapsed());
                        }
                        last_text.push_str(t);
                        token_emit.emit(Kind::Token(t.to_string()));
                        if chop_error.is_none()
                            && let Err(error) = chopper.push(&recorder, t)
                        {
                            *chop_error = Some(error);
                        }
                    },
                    &mut |r: &str| {
                        reasoning_emit.emit(Kind::Thinking(r.to_string()));
                    },
                    token,
                )
            };
            ral_core::dbg_trace!(
                "deliberate",
                "provider.complete: first token {first_token:?}, full {:?}",
                t_req.elapsed()
            );
            if token.is_cancelled() {
                emit.emit(Kind::Boundary);
                seal_chopper(&mut chopper, &recorder);
                return Ok(self.cancelled(emit));
            }
            let StepOut {
                mut assistant_message,
                tool_calls,
                reasoning,
                usage,
                stop_reason,
                cut_short,
            } = match step_out {
                Ok(s) => s,
                Err(ProviderError::Cancelled(_)) => {
                    emit.emit(Kind::Boundary);
                    seal_chopper(&mut chopper, &recorder);
                    return Ok(self.cancelled(emit));
                }
                Err(e) => {
                    emit.emit(Kind::Boundary);
                    seal_chopper(&mut chopper, &recorder);
                    return Err(e);
                }
            };
            if let Some(error) = chop_error {
                emit.emit(Kind::Boundary);
                return Err(ProviderError::Other(format!(
                    "an answer commit was not recorded in record.jsonl: {error}"
                )));
            }
            // Before the boundary, so the TUI lands `∴` ahead of the answer's
            // separate markdown rail.
            if let Some(reasoning) = reasoning.as_deref()
                && !reasoning.trim().is_empty()
            {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "answer char count cannot approach u32::MAX"
                )]
                let answer_chars = last_text.chars().count() as u32;
                // The live row derives from the published record — the
                // retired `Kind::Reasoning` twin — so this is the one
                // authoring site.
                let _recorded = recorder
                    .emit(crate::record::Display::Thinking {
                        text: reasoning.to_string(),
                        answer_chars,
                    })
                    .map_err(|e| {
                        ProviderError::Other(format!(
                            "the step's reasoning commit was not recorded: {e}"
                        ))
                    })?;
            }
            emit.emit(Kind::Boundary);
            chopper.flush(&recorder).map_err(|e| {
                ProviderError::Other(format!(
                    "the answer's tail commit was not recorded: {e}"
                ))
            })?;
            // The live numerator the next `compact` weighs against the window.
            let input_tokens = usage.input;
            let measured_at = {
                let mut log = self.log.lock();
                log.record_usage(usage.into())
                    .map_err(|e| ProviderError::Other(e.to_string()))?;
                log.log_len()
            };
            self.last_input = (input_tokens, measured_at);
            // Routine boundaries and `MaxTokens` (handled below) stay silent.
            if let Some(reason) = &stop_reason {
                match reason {
                    StopReason::Completed(_)
                    | StopReason::ToolCall(_)
                    | StopReason::MaxTokens(_) => {}
                    _ => emit.emit(Kind::StopReason(reason.raw().to_string())),
                }
            }
            admit_assistant(&mut assistant_message);
            let tool_ids: Vec<String> = tool_calls.iter().map(|tc| tc.call_id.clone()).collect();
            self.log
                .lock()
                .append_assistant(
                    assistant_message,
                    tool_ids,
                    stop_reason.as_ref().map(|r| r.raw().to_string()),
                )
                .map_err(ProviderError::Other)?;
            let truncated = cut_short.is_some();
            // A cut-short turn with no tool calls is final; `Truncated` lets the
            // nudge re-drive it.  With tool calls the session now sits in
            // `AwaitingToolResults`, and returning would strand it there — the
            // nudge's `append_user` would fail on pending results — so fall
            // through and let the next round-trip resume with the results.
            if truncated && tool_calls.is_empty() {
                let reason = match cut_short.as_ref().expect("truncated implies cut_short") {
                    CutShort::OutputCap => {
                        let reason = stop_reason
                            .as_ref()
                            .map_or_else(|| "max_tokens".into(), |r| r.raw().to_string());
                        self.note_error(format!(
                            "turn truncated (stop_reason={reason}): output cap reached. \
                             re-run with `--max-tokens N` for a larger ceiling, \
                             or ask the agent to split the work into smaller turns.",
                        ));
                        reason
                    }
                    CutShort::Stalled(cause) => {
                        // Committing the streamed prefix salvages the turn; it does
                        // not make the provider's failure any less of one, and the
                        // cause — a refusal, a dropped connection — is the user's to
                        // read in full.  Its own record, not `ProviderError`: that
                        // one ends an exchange, and this one is survived.
                        let recorded = self.log.lock().record_stall(cause);
                        if let Err(error) = recorded {
                            eprintln!("exarch: a stream stall was not recorded: {error}");
                        }
                        // The block above carries the detail; what rides on as the
                        // truncation reason is the one-line spelling.
                        cause.summary()
                    }
                };
                return Err(ProviderError::Truncated { reason });
            }
            if tool_calls.is_empty() {
                return Ok(match &stop_reason {
                    Some(r) if !matches!(r, StopReason::Completed(_) | StopReason::ToolCall(_)) => {
                        Outcome::Stopped {
                            reason: r.raw().to_string(),
                        }
                    }
                    _ if last_text.is_empty() => Outcome::Empty,
                    _ => Outcome::Complete(last_text),
                });
            }
            if truncated {
                Self::note("[Truncated mid-tool-call; continuing]".into(), emit);
            }
            let (results, injected) = self.run_batch(tool_calls, token, emit);
            self.log
                .lock()
                .append_tool_results(results)
                .map_err(ProviderError::Other)?;
            // `announce` draws each arrival's own chrome; the texts coalesce
            // into the one steering message the protocol admits after a batch.
            if !injected.is_empty() {
                let mut text = String::new();
                for item in &injected {
                    announce(item, emit, &recorder);
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&item.text());
                }
                self.log
                    .lock()
                    .append_steering(text)
                    .map_err(ProviderError::Other)?;
            }
            if token.is_cancelled() {
                return Ok(self.cancelled(emit));
            }
            // A `reply` in the fully drained batch ends the run here, not at
            // another round-trip.
            if let Some(payload) = self.reply.take() {
                return Ok(self.replied(payload));
            }
        }
    }

    pub(crate) fn compact(
        &self,
        provider: &Arc<Provider>,
        emit: &Emitter,
        requested: bool,
        token: &cancel::Token,
        continues: Option<u64>,
    ) {
        if !self.log.lock().can_compact() {
            if requested {
                self.note_error("cannot compact while tool results are pending".into());
            }
            return;
        }
        // Auto-compaction tracks real context pressure: the tokens the model
        // last saw against its window, firing once they grow into the reserve.
        // An unknown window falls back to the byte heuristic; a manual
        // `/compact` overrides the trigger entirely.
        let window = provider.context_window();
        let (due, summary_cap) = match window {
            Some(w) if w > 0 => (self.token_compaction_due(w), summary_cap_tokens(w)),
            _ => {
                let bytes = self.log.lock().history_bytes();
                (bytes >= COMPACT_THRESHOLD, SUMMARY_CAP_FALLBACK_TOKENS)
            }
        };
        if !requested && !due {
            return;
        }
        // Never start a summarize request an exchange-boundary Esc has already
        // doomed.
        if token.is_cancelled() {
            return;
        }
        // Keep the recent half verbatim; summarise the older prefix.
        let keep = suffix_keep_budget(self.log.lock().history_bytes());
        let plan = match continues {
            Some(exchange) => self.log.lock().plan_compaction_before(keep, exchange),
            None => self.log.lock().plan_compaction(keep),
        };
        let Some(plan) = plan else {
            // No exchange old enough to summarise: a no-op, not an event.
            return;
        };
        emit.emit(Kind::State(AgentState::Compacting));
        match provider.summarize(&self.system, plan.prefix_messages, summary_cap, token) {
            Ok(summary) => {
                let recorded = self.log.lock().record_usage(summary.usage.into());
                if let Err(e) = recorded {
                    self.note_error(format!("compact failed: {e}"));
                    return;
                }
                let edited = self.log.lock().apply_edit(
                    ContextOp::Fold {
                        through_exchange: plan.through_exchange,
                        digest: summary.summary,
                    },
                    EditAuthority::Harness,
                );
                // `apply_edit` records `ContextEdited` through the seam, and
                // the live row derives from the published record — there is
                // no separate notification left to keep in step with it.
                if let Err(e) = edited {
                    self.note_error(format!("compact failed: {e}"));
                }
            }
            Err(e) => self.note_error(format!("compact failed: {e}")),
        }
    }

    /// Run a batch of tool calls in order, short-circuiting the rest to
    /// cancelled results the instant the token trips.  Every call answers
    /// synchronously — a `spawn` returns a start receipt, not a join handle —
    /// so this also collects whatever the tool boundary admitted meanwhile.
    fn run_batch(
        &mut self,
        tool_calls: Vec<ToolCall>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> (Vec<SessionToolResult>, Vec<Item>) {
        let mut results = Vec::with_capacity(tool_calls.len());
        let mut it = tool_calls.into_iter();
        for call in it.by_ref() {
            if token.is_cancelled() {
                results.push(cancelled_result(call.call_id));
                results.extend(it.map(|r| cancelled_result(r.call_id)));
                break;
            }
            results.push(self.invoke(call, emit));
        }
        // The tool-boundary drain, each message tagged with its source; a slash
        // command is the lone exception, held for the exchange boundary.  A
        // result that settled across a `/clear` is dropped, not injected.
        let injected = self
            .inbox
            .drain_steering()
            .into_iter()
            .filter(|t| self.admits(t))
            .collect();
        (results, injected)
    }

    fn invoke(&mut self, call: ToolCall, emit: &Emitter) -> SessionToolResult {
        // `ral` is the only name this agent recognises, and only when the
        // request advertised it (withheld for a `--chat` trunk).  Every harness
        // verb (`agent`, `reply`, `schedule`, …) is a builtin *inside* it.
        if self.tool_enabled && call.fn_name == crate::shell_eval::tools::ral::NAME {
            crate::shell_eval::tools::ral::dispatch(call.call_id, &call.fn_arguments, self, emit)
        } else {
            let msg = format!("unknown tool `{}`", call.fn_name);
            self.note_error(msg.clone());
            SessionToolResult {
                id: call.call_id,
                content: msg,
            }
        }
    }

    /// The batch carried a `reply`.  The round-trip never asked for a final
    /// assistant message, so the session sits in
    /// `AwaitingAssistantAfterToolResults` and is wound back with its own
    /// breadcrumb; live descendants are cancelled and reaped, and `attend` stops.
    fn replied(&self, payload: FOValue) -> Outcome {
        self.agents.cancel_descendants(self.id);
        self.log.lock().quiesce(QuiesceReason::Replied);
        Outcome::Replied(payload)
    }

    fn cancelled(&self, emit: &Emitter) -> Outcome {
        self.log.lock().quiesce(QuiesceReason::Cancelled);
        // The log already carries `Cancelled` from `quiesce`; this is only its
        // user-facing companion.
        emit.emit(Kind::Error("cancelled".into()));
        Outcome::Cancelled
    }

    /// The step count would exceed [`MAX_STEPS`].  History is left mid-protocol
    /// for the attend loop's per-item quiesce to wind back; the [`StopReason`]
    /// reaches the headless JSON so a harness can tell a cap from a completion.
    fn capped(&self, emit: &Emitter) -> Outcome {
        self.note_error(format!(
            "step cap reached ({MAX_STEPS} provider round-trips); ending the deliberation"
        ));
        emit.emit(Kind::StopReason("step_cap".into()));
        Outcome::Capped
    }
}

fn cancelled_result(id: String) -> SessionToolResult {
    SessionToolResult {
        id,
        content: "cancelled before tool execution".into(),
    }
}

/// Seal a streaming step's chopper on an exit path that is already
/// cancelling or erroring: the streamed prefix is what the user saw, so it
/// still commits, best-effort — the exit in flight is the error being
/// reported, and this one must not mask it.
fn seal_chopper(chopper: &mut crate::record::commit::Chopper, recorder: &crate::record::Emitter) {
    if let Err(error) = chopper.flush(recorder) {
        eprintln!("exarch: a cancelled step's streamed prefix was not recorded: {error}");
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::testkit::*;
    use crate::agent::{NoControl, ProviderHandle, fresh_id};
    use crate::bus::{AgentOutcome, Inbox, Post};
    use crate::fleet::registry::{AGENT_LEASE_IDLE, EvalReach, InterruptTarget, Registration};
    use crate::provider::scripted::{Reply, Script};
    use genai::chat::ChatRole;
    use ral_core::Shell;
    use ral_core::Value;
    use ral_core::typecheck::builtins::{BuiltinTypeRule, mk_scheme, pure, thunk};
    use ral_core::typecheck::{Scheme, Ty, Unifier};
    use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled};
    use std::borrow::Cow;

    /// A sub-agent returns through `reply`: the payload reaches the parent raw,
    /// it settles `Complete`, and the run ends `ReadyForUser`.
    #[test]
    fn sub_agent_returns_through_reply() {
        let parent = Agent::for_test("system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("write a report".into());
        let provider = scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "reply #'# Report\nline one\nline two'#",
            )])),
        );
        let (outcome, text) = drive_peer(&mut child, provider);
        assert!(
            matches!(outcome, AgentOutcome::Complete),
            "a reply settles Complete, got {outcome:?}"
        );
        assert_eq!(
            text, "# Report\nline one\nline two",
            "the markdown payload passes through raw, newlines intact"
        );
        assert!(
            child.is_ready(),
            "a replied deliberation must leave the session ReadyForUser"
        );
    }

    /// `reply` settles the whole owned subtree: children still running when the
    /// parent returns are cancelled and reaped, siblings left untouched.
    #[test]
    fn reply_cancels_live_descendants() {
        let dir = tmp("reply-cancels-children");
        let parent = Agent::for_test("system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("return early".into());

        let direct = fresh_id();
        let grandchild = fresh_id();
        let sibling = fresh_id();
        let direct_token = cancel::Token::new();
        let grandchild_token = cancel::Token::new();
        let sibling_token = cancel::Token::new();
        let direct_root = ral_core::process::DurableRoot::default();
        // `register` refuses a child whose declared parent is not live at this
        // instant, so `child` must be registered before `direct` can be.
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
        let _ = child.agents.register(Registration {
            id: direct,
            parent: Some(child.id),
            lease: Some(AGENT_LEASE_IDLE),
            name: "direct".into(),
            log_dir: dir.path().join("direct"),
            cancel: direct_token.clone(),
            reach: EvalReach::Identity {
                eval_root: Some(direct_root.clone()),
                interrupt_target: InterruptTarget::default(),
            },
            mailbox: Inbox::new().mailbox(),
            provider: child.provider.clone(),
        });
        let _ = child.agents.register(Registration {
            id: grandchild,
            parent: Some(direct),
            lease: Some(AGENT_LEASE_IDLE),
            name: "grandchild".into(),
            log_dir: dir.path().join("grandchild"),
            cancel: grandchild_token.clone(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: InterruptTarget::default(),
            },
            mailbox: Inbox::new().mailbox(),
            provider: child.provider.clone(),
        });
        let sibling_generation = child
            .agents
            .register(Registration {
                id: sibling,
                parent: Some(parent.id),
                lease: Some(AGENT_LEASE_IDLE),
                name: "sibling".into(),
                log_dir: dir.path().join("sibling"),
                cancel: sibling_token.clone(),
                reach: EvalReach::Identity {
                    eval_root: Some(ral_core::process::DurableRoot::default()),
                    interrupt_target: InterruptTarget::default(),
                },
                mailbox: Inbox::new().mailbox(),
                provider: child.provider.clone(),
            })
            .expect("sibling registration must succeed: its parent is live");

        let provider = scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call("r1", "reply 'done'")])),
        );
        let (outcome, text) = drive_peer(&mut child, provider);

        assert!(matches!(outcome, AgentOutcome::Complete));
        assert_eq!(text, "done");
        assert!(direct_token.is_cancelled(), "direct child is cancelled");
        assert!(
            direct_root.as_scope().is_cancelled(),
            "the reap cancels the abandoned child's eval layer too"
        );
        assert!(
            grandchild_token.is_cancelled(),
            "grandchild is cancelled recursively"
        );
        assert!(
            child.agents.list(child.id).is_empty(),
            "reply reaps the abandoned subtree"
        );
        assert!(
            !sibling_token.is_cancelled(),
            "a sibling outside the replying subtree is untouched"
        );
        assert!(
            child.agents.settle(sibling, sibling_generation),
            "reply must not bump the global generation and poison siblings"
        );
    }

    /// A prompt typed while a tool batch runs is committed as a steering turn
    /// between the results and the next assistant turn — the one mid-exchange
    /// user ingress there is.  Several drained at once coalesce into one
    /// message, blank-line joined.
    #[test]
    fn steering_typed_during_a_tool_batch_is_committed_between_results_and_reply() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .inbox
            .push(Post::UserSteering("actually, stop after this".into()))
            .unwrap();
        session
            .inbox
            .push(Post::UserSteering("and report".into()))
            .unwrap();

        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let steer_x = 1")]))
                .then(Reply::text("ok")),
        );
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        match session.deliberate(
            &provider,
            Some("go".into()),
            None,
            &cancel::Token::new(),
            &emit,
        ) {
            Ok(Outcome::Complete(s)) => assert_eq!(s, "ok"),
            other => panic!("the steering prompt must be admitted mid-exchange, got {other:?}"),
        }

        let ms = session.rendered_messages();
        assert_eq!(
            ms.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
            vec![
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::Tool,
                ChatRole::User,
                ChatRole::Assistant,
            ],
            "the steering turn sits between the tool results and the next reply"
        );
        assert_eq!(
            ms[3].content.first_text(),
            Some("actually, stop after this\n\nand report"),
            "both drained prompts coalesce into the one admitted message"
        );
        assert!(
            std::iter::from_fn(|| rx.try_next_event().ok())
                .any(|e| matches!(e.kind, Kind::UserPromptEcho(t) if t.contains("and report"))),
            "the arrival must be announced as it enters context"
        );
        assert!(session.is_ready());
    }

    /// A provider error mid-deliberation strands the session in
    /// `AwaitingAssistantAfterToolResults`; the attend loop's per-iteration
    /// quiesce must leave the next prompt admissible.
    #[test]
    fn provider_error_mid_deliberation_does_not_wedge_session() {
        let mut session = Agent::for_test("system").unwrap();
        // A tool call that completes, then a stream error mid-protocol, then
        // clean replies for the second exchange and its nudges.
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let x12_a = 7")]))
                .then(Reply::error(ProviderError::Other(
                    "stream ended without End event".into(),
                )))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok")),
        );
        session.provider = ProviderHandle::new(provider);
        session.seed("first exchange".into());
        session.seed("second exchange after error".into());
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let (outcome, _) = session.attend(&mut NoControl, &emit);
        assert!(
            session.is_ready(),
            "session must be ReadyForUser after a mid-deliberation provider error"
        );
        assert!(
            scope_has(&mut session, "x12_a"),
            "a binding from a completed tool call must survive a later provider error"
        );
        match outcome {
            AgentOutcome::Failed(msg) => {
                assert!(
                    !msg.contains("tool results are pending"),
                    "second exchange must not be rejected; got: {msg}"
                );
            }
            other => panic!("expected Failed from no-reply nudges, got {other:?}"),
        }
    }

    #[test]
    fn stale_token_measure_is_unknown_until_the_next_completion() {
        let mut session = Agent::for_test("system").unwrap();
        {
            let mut log = session.log.lock();
            log.append_user("old context".into(), None).unwrap();
            log.append_assistant(genai::chat::ChatMessage::assistant("answer"), vec![], None)
                .unwrap();
        }
        let measured_at = session.log.lock().log_len();
        session.last_input = (99_000, measured_at);
        session
            .log
            .lock()
            .apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::Model)
            .unwrap();

        assert!(
            !session.token_compaction_due(100_000),
            "a stale token measure must not trigger compaction"
        );
        assert_eq!(
            session.token_pressure(100_000),
            None,
            "a stale token measure must not warn as high pressure"
        );

        let provider = scripted(
            "test-model",
            Script::new().then(Reply::text("fresh completion")),
        );
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let outcome = session.deliberate(
            &provider,
            Some("new context".into()),
            None,
            &cancel::Token::new(),
            &emit,
        );
        assert!(matches!(outcome, Ok(Outcome::Complete(text)) if text == "fresh completion"));
        assert_eq!(session.measured_input(), Some(0));
    }

    thread_local! {
        /// The token [`builtin_t2_cancel_now`] cancels — nothing else lets a
        /// bare builtin reach the token `deliberate` is watching.
        static T2_CANCEL_TOKEN: std::cell::RefCell<Option<cancel::Token>> =
            const { std::cell::RefCell::new(None) };
    }

    /// test-only: cancels whatever token is staged in [`T2_CANCEL_TOKEN`].
    #[allow(
        clippy::unnecessary_wraps,
        reason = "fixed BuiltinBody::Static signature"
    )]
    fn builtin_t2_cancel_now(
        _args: &[Value],
        _mooring: &Mooring,
        _shell: &mut Shell,
    ) -> Settled<Value> {
        T2_CANCEL_TOKEN.with(|cell| {
            if let Some(token) = cell.borrow().as_ref() {
                token.cancel(ral_core::process::CancelCause::Explicit);
            }
        });
        Ok(Value::Unit)
    }

    fn scheme_t2_cancel_now(_u: &mut Unifier) -> Scheme {
        mk_scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
    }

    static T2_CANCEL_BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
        Cow::Borrowed("t2-cancel-now"),
        BuiltinTypeRule::Scheme(scheme_t2_cancel_now),
        "test-only: cancel the token staged in T2_CANCEL_TOKEN.",
        BuiltinBody::Static(builtin_t2_cancel_now),
    )];
    static T2_CANCEL_BUILTINS: &[BuiltinEntry] = &T2_CANCEL_BUILTINS_ARR;

    /// A `reply` staged mid-batch and then overtaken by a cancellation must not
    /// survive into the next deliberation: the batch replies, then cancels the
    /// token `deliberate` watches, landing between `run_batch` and the drain.
    #[test]
    fn cancel_between_run_batch_and_drain_does_not_leak_reply_into_next_deliberation() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(T2_CANCEL_BUILTINS);

        let token = cancel::Token::new();
        T2_CANCEL_TOKEN.with(|cell| *cell.borrow_mut() = Some(token.clone()));

        let provider = scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![
                ral_call("r1", "reply 'stale'"),
                ral_call("c2", "t2-cancel-now"),
            ])),
        );
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        match session.deliberate(&provider, Some("go".into()), None, &token, &emit) {
            Ok(Outcome::Cancelled) => {}
            other => panic!("expected the cancel to win before the reply drains, got {other:?}"),
        }
        T2_CANCEL_TOKEN.with(|cell| *cell.borrow_mut() = None);

        let token2 = cancel::Token::new();
        // The next deliberation's first batch must itself reach the reply-drain
        // check: a leaked payload hard-terminates exactly there, on `c3`.
        let provider2 = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c3", "1")]))
                .then(Reply::text("done")),
        );
        match session.deliberate(&provider2, Some("continue".into()), None, &token2, &emit) {
            Ok(Outcome::Complete(s)) => assert_eq!(s, "done"),
            other => panic!(
                "a reply staged in a cancelled batch must not leak into the next deliberation, got {other:?}"
            ),
        }
    }

    /// A worker delivers before it retires, so a result that settled across a
    /// `/clear` still reaches the inbox and must be dropped here.
    #[test]
    fn stale_agent_result_is_dropped_by_generation_admission() {
        let mut session = Agent::for_test("system").unwrap();
        let stale = session.agents.generation();
        session.agents.clear_subtree(session.id);
        session
            .inbox
            .push(Post::AgentResult(crate::bus::AgentResult {
                name: "late".into(),
                outcome: AgentOutcome::Complete,
                text: "settled across the clear".into(),
                elapsed: std::time::Duration::ZERO,
                generation: stale,
            }))
            .unwrap();
        session.provider = ProviderHandle::new(scripted("test-model", Script::new()));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let (outcome, payload) = session.attend(&mut NoControl, &emit);
        assert!(
            matches!(outcome, AgentOutcome::Empty),
            "a stale result must be dropped, not attended to; got {outcome:?}"
        );
        assert!(payload.is_none());
        assert!(session.is_ready());
    }

    /// The positive half: a live-generation result is delivered and drives the
    /// provider.
    #[test]
    fn current_generation_agent_result_is_delivered() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .inbox
            .push(Post::AgentResult(crate::bus::AgentResult {
                name: "worker".into(),
                outcome: AgentOutcome::Complete,
                text: "found it".into(),
                elapsed: std::time::Duration::ZERO,
                generation: session.agents.generation(),
            }))
            .unwrap();
        // A headless root's first `reply` is turned back for self-verification.
        session.provider = ProviderHandle::new(scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("r1", "reply 'done'")]))
                .then(Reply::tool_calls(vec![ral_call("r2", "reply 'done'")])),
        ));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let (outcome, payload) = session.attend(&mut NoControl, &emit);
        assert!(
            matches!(outcome, AgentOutcome::Complete),
            "a live-generation result must be delivered; got {outcome:?}"
        );
        assert_eq!(
            payload,
            Some(FOValue::String {
                value: "done".into()
            })
        );
    }

    /// The same admission over a deferred `spawn` batch: composed off the
    /// attend thread, so the producer cannot judge its own staleness.
    #[test]
    fn stale_surface_batch_is_dropped_by_generation_admission() {
        let mut session = Agent::for_test("system").unwrap();
        let stale = session.agents.generation();
        session.agents.clear_subtree(session.id);
        session
            .inbox
            .push(Post::Surface {
                id: session.id,
                values: Vec::new(),
                generation: stale,
            })
            .unwrap();
        session.provider = ProviderHandle::new(scripted("test-model", Script::new()));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let (outcome, payload) = session.attend(&mut NoControl, &emit);
        assert!(
            matches!(outcome, AgentOutcome::Empty),
            "a stale surface batch must be dropped, not attended to; got {outcome:?}"
        );
        assert!(payload.is_none());
        assert!(session.is_ready());
    }

    /// The positive half: a live-generation surface batch is delivered and
    /// drives the provider.
    #[test]
    fn current_generation_surface_batch_is_delivered() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .inbox
            .push(Post::Surface {
                id: session.id,
                values: Vec::new(),
                generation: session.agents.generation(),
            })
            .unwrap();
        // Same self-verification turn-back as the agent-result twin above.
        session.provider = ProviderHandle::new(scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("r1", "reply 'done'")]))
                .then(Reply::tool_calls(vec![ral_call("r2", "reply 'done'")])),
        ));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let (outcome, payload) = session.attend(&mut NoControl, &emit);
        assert!(
            matches!(outcome, AgentOutcome::Complete),
            "a live-generation surface batch must be delivered; got {outcome:?}"
        );
        assert_eq!(
            payload,
            Some(FOValue::String {
                value: "done".into()
            })
        );
    }

    /// `AgentRegistry::cancel` never touches a shell's worker registry — it
    /// only cancels the entry's `eval_root`.  That suffices: a worker's cancel
    /// scope is a child of that root and `is_cancelled` walks ancestors.
    /// Pinned because it must keep holding with no wiring of its own.
    #[test]
    fn cancel_cascade_reaches_a_cancelled_sub_agents_workers() {
        let parent = Agent::for_test("system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        let _ = parent.agents.register(Registration {
            id: child.id,
            parent: Some(parent.id),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: child.log_dir(),
            cancel: child.cancel_token().clone(),
            reach: child.seat.eval_reach(),
            mailbox: child.mailbox(),
            provider: child.provider_handle(),
        });

        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, child.id, child.inbox.mailbox());
        let _ = child.run_shell("c1".into(), "spawn { test-clear-block-forever }", 30, &emit);

        let entries = child.seat.shell_mut().shell.workers();
        assert_eq!(entries.len(), 1, "the child's own spawn must register");
        assert!(!entries[0].handle.cancel.is_cancelled(), "freshly spawned");

        assert!(
            parent.agents.cancel(child.id),
            "the child must still be live"
        );

        assert!(
            entries[0].handle.cancel.is_cancelled(),
            "the subtree cascade must reach a cancelled sub-agent's own \
             workers through its shell's durable root"
        );
    }
}
