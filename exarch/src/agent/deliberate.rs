//! One prompt run to quiescence against the provider.
//!
//! [`Agent::deliberate`] steps the provider until it stops calling tools:
//! render the transcript, await a completion, admit the assistant message,
//! run the resulting tool-call batch ([`Agent::run_batch`], call by call
//! through [`Agent::invoke`]), and repeat — bounded by [`MAX_STEPS`] so a
//! headless or autonomous run with no Esc to hand still terminates. The one
//! auto-compaction check ([`Agent::compact`]) sits at `deliberate`'s entry,
//! the sole boundary guaranteed `ReadyForUser`; the policy behind it — the
//! pressure trigger, the summary cap, the suffix-keep budget — lives in
//! [`digest`](crate::agent::digest), this module only carries it out.
//! `deliberate`'s three exits, [`Agent::replied`], [`Agent::cancelled`], and
//! [`Agent::capped`], are the outcome constructors: each winds the session
//! log back to `ReadyForUser` its own way before handing back an
//! [`Outcome`].
//!
//! [`Agent::attend`] is the loop around this: it pulls one inbox item per
//! iteration and takes it up ([`Agent::take_up`]), which calls `deliberate`
//! once. This module is what happens *inside* that one call — never the loop
//! that repeats it.

use crate::agent::Agent;
use crate::agent::attend::announce;
use crate::agent::cancel;
use crate::agent::digest::{
    COMPACT_THRESHOLD, SUMMARY_CAP_FALLBACK_TOKENS, compaction_due, suffix_keep_budget,
    summary_cap_tokens,
};
use crate::agent::event::{QuiesceReason, ToolResult as SessionToolResult};
use crate::bus::{Emitter, Item, Kind};
use crate::provider::{CutShort, Provider, ProviderError, StepOut, StopReason, ToolCall};
use ral_core::serial::FOValue;
use std::sync::Arc;

/// Outcome of one [`Agent::deliberate`].  Degenerate cases (`Empty`,
/// `Stopped`) become nudges; `Cancelled` and `Capped` do not; hard
/// failures travel through [`ProviderError`].
#[derive(Debug)]
pub enum Outcome {
    Complete(String),
    /// A returning agent called `reply`: the carried payload is its deliberate
    /// return value, a faithful [`FOValue`] (`FOValue::Unit` when the
    /// argument was absent or unit).  Carried as a value, not pre-rendered
    /// text, so each consumer renders it at its own edge — prose for a model
    /// parent, the structure itself for the headless harness (via
    /// [`crate::shell_eval::user_json`]).  Distinct from [`Self::Complete`]
    /// precisely so the nudge layer can tell "already returned" from
    /// "stopped without returning" and not re-nudge an agent that replied.
    /// Terminal: it ends the attend loop.
    Replied(FOValue),
    Empty,
    Stopped {
        reason: String,
    },
    Cancelled,
    /// The round-trip loop hit [`MAX_STEPS`] without the model ever
    /// emitting a tool-call-free reply.  Terminal: it carries no nudge
    /// (re-attending would just spend another `MAX_STEPS`).
    Capped,
}

/// Hard ceiling on provider round-trips in one [`Agent::deliberate`].  The
/// interactive frontend has Esc to halt a runaway deliberation; headless and
/// autonomous sub-agent runs have nothing, so a model that keeps
/// emitting tool calls would loop until the token budget or the wall
/// runs out.  Bounding the step count keeps benchmark and headless runs
/// terminating.  Generous enough that no genuine interactive deliberation ever
/// reaches it.
const MAX_STEPS: u32 = 250;

impl Agent {
    /// Run one deliberation: optionally commit `prompt`, then step the
    /// provider round-trip loop to quiescence, returning the deliberation's
    /// outcome.
    ///
    /// # Errors
    /// Returns `Err` if a provider round-trip fails, or if a session-log
    /// mutation fails and is surfaced as `ProviderError::Other` (committing
    /// the prompt, recording a step or usage, rendering the request, or
    /// appending the reply).
    ///
    /// # Panics
    /// Panics if a step is truncated with no tool calls yet no cut-short
    /// cause was recorded — an internal invariant of the round-trip loop.
    pub fn deliberate(
        &mut self,
        provider: &Arc<Provider>,
        prompt: Option<String>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> Result<Outcome, ProviderError> {
        // A reply only ever belongs to the batch that staged it: a cancel, a
        // log-append error, or a panic between the `invoke` call that set it
        // and the drain that takes it (all recovered by returning from this
        // `deliberate` without reaching that take) must not let it outlive
        // this call.  Entering fresh here — every route into `deliberate` is
        // a new deliberation — is the one place that is structurally
        // guaranteed to run, so it is the one place this reset needs to live.
        self.reply = None;
        // Auto-compaction runs here, at the one boundary where `can_compact()`
        // actually holds — `deliberate` is entered `ReadyForUser` (the
        // exchange-ends-ready invariant), before the prompt is committed.
        // Every provider round-trip — each user exchange, each nudge
        // iteration — passes through here, so long autonomous and headless
        // runs stay bounded.
        self.compact(provider, emit, false, token);
        let mut last_text = String::new();
        if let Some(p) = prompt {
            self.log
                .lock()
                .append_user(p)
                .map_err(ProviderError::Other)?;
        }
        let mut n = 0u32;
        loop {
            n += 1;
            if n > MAX_STEPS {
                return Ok(self.capped(emit));
            }
            self.log
                .lock()
                .record_step(n, provider.tuning().clone())
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            emit.emit(Kind::Step {
                n,
                tuning: provider.tuning().clone(),
            });
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
            emit.emit(Kind::Phase("awaiting model".into()));
            let step_out = {
                let token_emit = emit.clone();
                let reasoning_emit = emit.clone();
                provider.complete(
                    &self.system,
                    messages,
                    self.tool_enabled,
                    &mut |t: &str| {
                        #[cfg(debug_assertions)]
                        if first_token.is_none() {
                            first_token = Some(t_req.elapsed());
                        }
                        last_text.push_str(t);
                        token_emit.emit(Kind::Token(t.to_string()));
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
                    return Ok(self.cancelled(emit));
                }
                Err(e) => {
                    emit.emit(Kind::Boundary);
                    return Err(e);
                }
            };
            // Commit reasoning before the boundary so the TUI can land the
            // `∴` block ahead of the answer's separate markdown rail. A pure
            // tool-call step may still show a thinking block; the captured
            // reasoning also round-trips on `assistant_message` below.
            if let Some(reasoning) = reasoning.as_deref()
                && !reasoning.trim().is_empty()
            {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "answer char count cannot approach u32::MAX"
                )]
                let answer_chars = last_text.chars().count() as u32;
                emit.emit(Kind::Reasoning {
                    text: reasoning.to_string(),
                    answer_chars,
                });
            }
            emit.emit(Kind::Boundary);
            // The tokens the model just saw — the live numerator for the
            // context-pressure compaction trigger at this step's boundary.
            self.last_input = usage.input;
            self.log
                .lock()
                .record_usage(usage.into())
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            emit.emit(Kind::Usage(usage));
            // Surface non-trivial stop reasons; the routine boundaries
            // and `MaxTokens` (handled below) stay silent.
            if let Some(reason) = &stop_reason {
                match reason {
                    StopReason::Completed(_)
                    | StopReason::ToolCall(_)
                    | StopReason::MaxTokens(_) => {}
                    _ => emit.emit(Kind::StopReason(reason.raw().to_string())),
                }
            }
            // Admission boundary: repair non-object tool-call arguments
            // (X2) and never commit an empty assistant message (X7) before
            // it enters the transcript.
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
            // The assistant turn was cut short — the output cap or a
            // mid-stream stall.  With no captured tool call there is nothing
            // to run and the assistant turn is final, so commit the
            // partial reply (done above) and surface a `Truncated` so the
            // nudge re-drives the exchange with `continue`.  With captured
            // tool calls (only the output cap leaves any) the assistant
            // message carries `tool_ids` and the session is now
            // `AwaitingToolResults`; returning here would strand it there and
            // the nudge's `append_user` would fail "tool results pending"
            // (X6).  Instead fall through to run the calls and continue the
            // loop — the next round-trip resumes the truncated turn with the
            // results in hand.
            if truncated && tool_calls.is_empty() {
                let reason = match cut_short.as_ref().expect("truncated implies cut_short") {
                    CutShort::OutputCap => {
                        let reason = stop_reason
                            .as_ref()
                            .map_or_else(|| "max_tokens".into(), |r| r.raw().to_string());
                        self.note_error(
                            format!(
                                "turn truncated (stop_reason={reason}): output cap reached. \
                                 re-run with `--max-tokens N` for a larger ceiling, \
                                 or ask the agent to split the work into smaller turns.",
                            ),
                            emit,
                        );
                        reason
                    }
                    CutShort::Stalled(cause) => {
                        // A transient transport hiccup we recovered from, not
                        // a misconfiguration: an operational note, not an error.
                        Self::note(
                            format!("[Stream stalled: {}]", cause.replace('\n', " | ")),
                            emit,
                        );
                        cause.clone()
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
            // Everything that arrived during the batch lands now, mid-step:
            // each source renders its own chrome (a `↘` block for a subagent, a
            // marked wakeup, a `spawn`'s cards), and their texts coalesce into
            // the single steering message the protocol admits after a batch.
            if !injected.is_empty() {
                let mut text = String::new();
                for item in &injected {
                    announce(item, emit);
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
            // The batch has fully drained — every call dispatched, every
            // `call_id` answered.  If one of them was `reply`, end the run now
            // with its payload rather than looping for another round-trip.
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
    ) {
        if !self.log.lock().can_compact() {
            if requested {
                self.note_error("cannot compact while tool results are pending".into(), emit);
            }
            return;
        }
        // Auto-compaction tracks real context pressure: `last_input` (the
        // tokens the model last saw) against the model's context window,
        // firing once it grows into the reserve (oh-my-pi's trigger).  An
        // unknown window (native provider, or the catalog not yet fetched)
        // falls back to the absolute byte heuristic.  A manual `/compact`
        // (`requested`) overrides the gate: the user is compacting on
        // purpose.  `summary_cap` scales the summary with the window —
        // exarch keeps a recent suffix verbatim and summarises only the
        // older prefix, so the summary stays concise either way.
        let used = self.last_input;
        let window = provider.context_window();
        let (due, detail, summary_cap) = match window {
            Some(w) if w > 0 => (
                compaction_due(used, w),
                format!("{used} of {w} tokens"),
                summary_cap_tokens(w),
            ),
            _ => {
                let bytes = self.log.lock().history_bytes();
                (
                    bytes >= COMPACT_THRESHOLD,
                    format!("{} KB", bytes / 1024),
                    SUMMARY_CAP_FALLBACK_TOKENS,
                )
            }
        };
        if !requested && !due {
            return;
        }
        // An exchange-boundary Esc must not kick off a summarize request
        // we'd only instantly cancel; bail before the work and let
        // `deliberate`'s post-compact check return to the prompt.
        if token.is_cancelled() {
            return;
        }
        // Keep the recent half verbatim; summarise the older prefix.
        let keep = suffix_keep_budget(self.log.lock().history_bytes());
        let Some(plan) = self.log.lock().plan_compaction(keep) else {
            // No exchange old enough to summarise.  This is a no-op, not an
            // event: the absence of a `compacted` note already says nothing
            // happened, and the worker has no honest way to draw view-only
            // chrome.
            return;
        };
        Self::note(format!("[Compacting history: {detail} → summary]"), emit);
        emit.emit(Kind::Phase("compacting".into()));
        match provider.summarize(&self.system, plan.prefix_messages, summary_cap, token) {
            Ok(summary) => {
                let recorded = self.log.lock().record_usage(summary.usage.into());
                if let Err(e) = recorded {
                    self.note_error(format!("compact failed: {e}"), emit);
                    return;
                }
                emit.emit(Kind::Usage(summary.usage));
                let compacted = self
                    .log
                    .lock()
                    .apply_compaction(summary.summary, plan.suffix_start);
                if let Err(e) = compacted {
                    self.note_error(format!("compact failed: {e}"), emit);
                    return;
                }
                Self::note(
                    format!(
                        "[Compacted: now {} KB]",
                        self.log.lock().history_bytes() / 1024
                    ),
                    emit,
                );
            }
            Err(e) => self.note_error(format!("compact failed: {e}"), emit),
        }
    }

    // --- private helpers ---

    /// Run a batch of tool calls in order, short-circuiting the rest to
    /// cancelled results the instant the token trips.  Every call returns its
    /// result synchronously — a spawn inside the `ral` eval launches a
    /// detached peer and answers with a start receipt — so there is no join
    /// phase and no `thread::scope`.  Answers with the batch's results and the
    /// items admitted at the tool boundary while it ran.
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
        // The tool-boundary drain: every message that arrived during the
        // batch — barged-in user steering, a settled subagent's result, a
        // fired wakeup, a `spawn`'s surface — tagged with its source. A
        // slash command is the lone exception, held for the exchange
        // boundary.  Generation admission applies here as at the exchange
        // boundary: a result that settled across a `/clear` is dropped, not
        // injected.
        let injected = self
            .inbox
            .drain_steering()
            .into_iter()
            .filter(|t| self.admits(t))
            .collect();
        (results, injected)
    }

    fn invoke(&mut self, call: ToolCall, emit: &Emitter) -> SessionToolResult {
        // `ral` is the only name this agent ever recognises, and only when
        // its provider requests actually advertised it (withheld only for a
        // `--chat` trunk) — so a well-behaved model never names anything
        // else here. Every harness verb (`agent`, `reply`, `schedule`, …)
        // is a builtin *inside* a `ral` call, not a name `invoke` matches.
        if self.tool_enabled && call.fn_name == crate::shell_eval::tools::ral::NAME {
            crate::shell_eval::tools::ral::dispatch(call.call_id, &call.fn_arguments, self, emit)
        } else {
            let msg = format!("unknown tool `{}`", call.fn_name);
            self.note_error(msg.clone(), emit);
            SessionToolResult {
                id: call.call_id,
                content: msg,
            }
        }
    }

    /// Reached when the batch carried a `reply`: wind the session back to
    /// `ReadyForUser` with the dedicated breadcrumb (the last round-trip
    /// dispatched the reply but never asked for a final assistant message, so
    /// the protocol sits in `AwaitingAssistantAfterToolResults`), cancel any
    /// live descendants, and return the payload.  `attend` then breaks the
    /// loop — `reply` hard-terminates.
    fn replied(&self, payload: FOValue) -> Outcome {
        self.agents.cancel_descendants(self.id);
        self.log.lock().quiesce(QuiesceReason::Replied);
        Outcome::Replied(payload)
    }

    fn cancelled(&self, emit: &Emitter) -> Outcome {
        self.log.lock().quiesce(QuiesceReason::Cancelled);
        // The canonical log already carries `Cancelled` from quiesce;
        // this is the user-facing companion only.
        emit.emit(Kind::Error("cancelled".into()));
        Outcome::Cancelled
    }

    /// Reached at the top of the round-trip loop once the step count
    /// would exceed [`MAX_STEPS`].  The history is mid-protocol (the last
    /// step appended its tool results); `attend`'s single exit winds it
    /// back to `ReadyForUser`.  `note_error` is the user-facing line and
    /// the forensic breadcrumb; the `StopReason` surfaces in the headless
    /// JSON result so a benchmark harness can tell a capped run from a
    /// completed one.
    fn capped(&self, emit: &Emitter) -> Outcome {
        self.note_error(
            format!("step cap reached ({MAX_STEPS} provider round-trips); ending the deliberation"),
            emit,
        );
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

/// Substituted for an assistant message that would serialise to no
/// substantive content.  Anthropic rejects an assistant turn whose
/// `content` is empty (`[]` or only an empty text block) once a later
/// message makes it non-final, so the empty nudge — which appends a
/// user prompt right after — would poison every subsequent request.  A
/// short stub keeps the turn renderable; the nudge still recovers the
/// empty reply by re-prompting.
const EMPTY_ASSISTANT_STUB: &str = "(no content)";

/// Normalise an assistant message to the transcript-admission invariant
/// at the `deliberate` commit boundary: **every committed message serialises
/// to a request every supported provider accepts.**
///
/// - X2: a tool call whose `fn_arguments` is not a JSON object is repaired
///   to `{}`.  genai's Anthropic adapter only repairs a `null` argument, so
///   a non-object (a bare string, number, array) re-serialises verbatim and
///   strict backends 400 every later request that carries it.
/// - X7: a message with no substantive part — no tool call, no non-empty
///   text or binary — has the stub substituted, so it is never committed
///   empty.
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
        // Reasoning / thought signatures alone are not a renderable turn on
        // a strict backend; they ride alongside real content, never as it.
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
    use crate::fleet::registry::{AGENT_LEASE_IDLE, EvalReach, Registration, RunScope};
    use crate::provider::scripted::{Reply, Script};
    use ral_core::Shell;
    use ral_core::Value;
    use ral_core::typecheck::builtins::{BuiltinTypeRule, mk_scheme, pure, thunk};
    use ral_core::typecheck::{Scheme, Ty, Unifier};
    use ral_core::types::{BuiltinBody, BuiltinEntry, Settled};
    use std::borrow::Cow;

    /// A sub-agent returns through `reply`: its markdown payload is the value
    /// the parent receives (raw, newlines intact), it settles `Complete`, and
    /// the run hard-terminates leaving the session `ReadyForUser`.
    #[test]
    fn sub_agent_returns_through_reply() {
        let dir = tmp("reply-terminal");
        let parent = Agent::for_test(&dir, "system").unwrap();
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

    /// `reply` is a settling edge for the whole owned subtree: if a parent
    /// returns before children finish, those children are cancelled and reaped
    /// rather than left registered under a dead parent.
    #[test]
    fn reply_cancels_live_descendants() {
        let dir = tmp("reply-cancels-children");
        let parent = Agent::for_test(&dir, "system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("return early".into());

        let direct = fresh_id();
        let grandchild = fresh_id();
        let sibling = fresh_id();
        let direct_token = cancel::Token::new();
        let grandchild_token = cancel::Token::new();
        let sibling_token = cancel::Token::new();
        let direct_root = ral_core::process::DurableRoot::default();
        // `child` itself must be a live entry before `direct` can register
        // under it as parent — `register` refuses a child whose declared
        // parent is not, at this instant, live.
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
        let _ = child.agents.register(Registration {
            id: direct,
            parent: Some(child.id),
            lease: Some(AGENT_LEASE_IDLE),
            name: "direct".into(),
            log_dir: dir.join("direct"),
            cancel: direct_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: direct_root.clone(),
                run_scope: RunScope::default(),
            }),
            mailbox: Inbox::new().mailbox(),
            provider: child.provider.clone(),
        });
        let _ = child.agents.register(Registration {
            id: grandchild,
            parent: Some(direct),
            lease: Some(AGENT_LEASE_IDLE),
            name: "grandchild".into(),
            log_dir: dir.join("grandchild"),
            cancel: grandchild_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: ral_core::process::DurableRoot::default(),
                run_scope: RunScope::default(),
            }),
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
                log_dir: dir.join("sibling"),
                cancel: sibling_token.clone(),
                reach: Some(EvalReach::Identity {
                    eval_root: ral_core::process::DurableRoot::default(),
                    run_scope: RunScope::default(),
                }),
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

    /// X12: a provider error mid-deliberation (e.g. "stream ended without End
    /// event") must not wedge the session.  When `deliberate` returns `Err`
    /// with the session stranded in `AwaitingAssistantAfterToolResults`,
    /// the `attend` loop quiesces per-iteration so the next prompt is
    /// admitted — not rejected with "tool results are pending".
    #[test]
    fn provider_error_mid_deliberation_does_not_wedge_session() {
        let dir = tmp("x12-provider-error");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        // 1st round-trip: model requests a tool call (runs to completion,
        // leaving the session `AwaitingAssistantAfterToolResults`);
        // 2nd round-trip: stream error mid-protocol;
        // 3rd–6th: clean replies for the second exchange and its nudges.
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
        // The session is ready for the next prompt.
        assert!(
            session.is_ready(),
            "session must be ReadyForUser after a mid-deliberation provider error"
        );
        // The binding from the completed tool call survived.
        assert!(
            scope_has(&mut session, "x12_a"),
            "a binding from a completed tool call must survive a later provider error"
        );
        // The second exchange ran: the final outcome is the no-reply failure
        // from the second exchange's nudges, NOT a "tool results are pending"
        // rejection from a wedged session.
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

    thread_local! {
        /// The token this process's [`builtin_t2_cancel_now`] cancels, staged
        /// by the test that installs it — a same-thread, test-only side
        /// channel standing in for a cancellation racing in mid-batch, since
        /// nothing else lets a bare builtin reach the token `deliberate` is
        /// watching.
        static T2_CANCEL_TOKEN: std::cell::RefCell<Option<cancel::Token>> =
            const { std::cell::RefCell::new(None) };
    }

    /// test-only: cancels whatever token is staged in [`T2_CANCEL_TOKEN`].
    #[allow(
        clippy::unnecessary_wraps,
        reason = "fixed BuiltinBody::Static signature"
    )]
    fn builtin_t2_cancel_now(_args: &[Value], _shell: &mut Shell) -> Settled<Value> {
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

    static T2_CANCEL_BUILTINS: &[BuiltinEntry] = &[BuiltinEntry {
        name: Cow::Borrowed("t2-cancel-now"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_t2_cancel_now),
        doc: "test-only: cancel the token staged in T2_CANCEL_TOKEN.",
        body: BuiltinBody::Static(builtin_t2_cancel_now),
    }];

    /// T2: a `reply` staged mid-batch that is then overtaken by a
    /// cancellation before the batch fully drains must not survive into the
    /// next deliberation.  The scripted batch carries `reply` first — staging
    /// `self.reply` — then a builtin that cancels the very token `deliberate`
    /// is watching, landing the cancel exactly where `run_batch` already ran
    /// the reply but the post-batch drain has not yet taken it.  Without the
    /// reset at `deliberate`'s entry, the next deliberation's first tool
    /// batch would hard-terminate on this stale payload instead of
    /// completing.
    #[test]
    fn cancel_between_run_batch_and_drain_does_not_leak_reply_into_next_deliberation() {
        let dir = tmp("t2-cancel-mid-batch");
        let mut session = Agent::for_test(&dir, "system").unwrap();
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
        match session.deliberate(&provider, Some("go".into()), &token, &emit) {
            Ok(Outcome::Cancelled) => {}
            other => panic!("expected the cancel to win before the reply drains, got {other:?}"),
        }
        T2_CANCEL_TOKEN.with(|cell| *cell.borrow_mut() = None);

        let token2 = cancel::Token::new();
        // The next deliberation's first batch must itself run a tool call and
        // reach the reply-drain check — a leaked `self.reply` hard-terminates
        // exactly there, on `c3`'s batch, before the second round-trip that
        // actually completes the deliberation ever runs.
        let provider2 = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c3", "1")]))
                .then(Reply::text("done")),
        );
        match session.deliberate(&provider2, Some("continue".into()), &token2, &emit) {
            Ok(Outcome::Complete(s)) => assert_eq!(s, "done"),
            other => panic!(
                "a reply staged in a cancelled batch must not leak into the next deliberation, got {other:?}"
            ),
        }
    }

    /// Generation admission: a worker delivers before it retires, so a
    /// result that settled across a `/clear` can reach the inbox — the attend
    /// loop must drop it before it becomes a deliberation.  The script is
    /// empty, so any admitted item would consult the provider and fail the
    /// run; a clean `Empty` quiescence proves the result never got that far.
    #[test]
    fn stale_agent_result_is_dropped_by_generation_admission() {
        let dir = tmp("generation-admission-stale");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let stale = session.agents.generation();
        session.agents.clear_subtree(session.id);
        session
            .inbox
            .push(Post::AgentResult(crate::bus::AgentResult {
                id: fresh_id(),
                name: "late".into(),
                outcome: AgentOutcome::Complete,
                text: "settled across the clear".into(),
                log_dir: dir,
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

    /// The admission control's positive half: a result stamped with the live
    /// generation is delivered as an item and drives the provider.
    #[test]
    fn current_generation_agent_result_is_delivered() {
        let dir = tmp("generation-admission-live");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .inbox
            .push(Post::AgentResult(crate::bus::AgentResult {
                id: fresh_id(),
                name: "worker".into(),
                outcome: AgentOutcome::Complete,
                text: "found it".into(),
                log_dir: dir,
                elapsed: std::time::Duration::ZERO,
                generation: session.agents.generation(),
            }))
            .unwrap();
        // This root session is a headless root (`parent: None`, non-interactive),
        // so its first `reply` is turned back once for self-verification; the
        // second is what actually delivers the result.
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

    /// The same admission control's `Surface` half: a deferred `spawn`
    /// batch's birth generation (`InboxDeferred`, `shell_eval.rs`) is checked
    /// here exactly like an `AgentResult`'s, since neither producer can decide
    /// its own staleness (each composes on a thread other than the attend
    /// loop's, and its push can land arbitrarily long after). The script is
    /// empty, so any admitted item would consult the provider and fail the
    /// run.
    #[test]
    fn stale_surface_batch_is_dropped_by_generation_admission() {
        let dir = tmp("generation-admission-stale-surface");
        let mut session = Agent::for_test(&dir, "system").unwrap();
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

    /// The positive half: a surface batch stamped with the live generation is
    /// delivered as an item and drives the provider.
    #[test]
    fn current_generation_surface_batch_is_delivered() {
        let dir = tmp("generation-admission-live-surface");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .inbox
            .push(Post::Surface {
                id: session.id,
                values: Vec::new(),
                generation: session.agents.generation(),
            })
            .unwrap();
        // Same self-verification quirk as `current_generation_agent_result_is_delivered`.
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

    /// The generation-and-cascade audit's cascade edge: `AgentRegistry::cancel`
    /// (the primitive behind `agent-cancel` and the subtree cascade) never
    /// touches a shell's worker registry directly — it only cancels the
    /// entry's `eval_root`. That is already enough: a worker's own cancel
    /// scope is a child of that same root, and every
    /// `CancelScope::is_cancelled` walks its ancestors, so cancelling a
    /// sub-agent reaches its own still-running workers with no extra edge.
    /// Pinned as a regression — this must keep holding with no wiring of
    /// its own.
    #[test]
    fn cancel_cascade_reaches_a_cancelled_sub_agents_workers() {
        let dir = tmp("cascade-cancels-sub-agent-workers");
        let parent = Agent::for_test(&dir, "system").unwrap();
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
            reach: Some(child.seat.eval_reach()),
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
