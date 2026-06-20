//! One continuous agent session: canonical event log, persistent shell,
//! capability set, turn driver.  [`Session::apply`] runs one round-trip
//! against the provider; [`Session::run_turn`] wraps it with the
//! nudge-retry policy.  Sub-sessions fork on the model's `agent` tool;
//! the call tree lives on the Rust call stack and mirrors as
//! [`Kind::Born`] / [`Kind::Died`] on the bus.

use crate::bootstrap::Scratch;
use crate::bus::{Emitter, Kind, SessionId, Sink, pump};
use crate::cancel;
use crate::digest::{AGENT_REPLY_CAP, COMPACT_THRESHOLD, OPAQUE_CAP, clip, render};
use crate::event::{QuiesceReason, SessionLog, ToolResult as SessionToolResult};
use crate::nudge;
use crate::provider::{Provider, ProviderError, StepOut, StopReason, ToolCall};
use crate::shell_eval;
use ral_core::Shell;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

pub struct Session {
    pub id: SessionId,
    pub(crate) system: String,
    log: SessionLog,
    shell: Shell,
    caps: ral_core::types::Capabilities,
    /// A forked child. Sub-agents are neither advertised nor permitted
    /// root-only tools — chiefly `agent`, so the spawn tree stays one
    /// level deep (see [`crate::tools::Tool::root_only`]).
    is_subagent: bool,
    /// When set (headless, root only), turn completion is gated by two
    /// one-shot nudges: a turn that never used a tool is nudged to engage,
    /// and a turn that did is nudged to verify its output against the task.
    /// Forks never inherit it.
    expect_action: bool,
    /// Whether the current turn has dispatched any tool call. Reset at the
    /// top of [`Session::run_turn`]; read by the completion-gate nudges to
    /// choose between the idle and verify prompts.
    acted: bool,
    /// Durable snapshot of `shell.mobile` as of the last clean tool-call
    /// boundary.  Refreshed inside the worker (`run_shell`) right before
    /// each `run_shell` dispatches, so it always holds the dynamic context
    /// that completed calls left behind, never the one a call is mid-way
    /// through.  Read by [`Session::run_turn`] after a caught worker panic
    /// (`pump` → `Ok(None)`) to roll the panicking call's dynamic-context
    /// effects back: the field lives on `Session` precisely so it survives
    /// `pump`'s `catch_unwind` boundary (written by the worker, read by the
    /// driver).  See `ral_core`'s `run_turn` frame guard for the IO half
    /// of the same panic-recovery contract.
    durable: ral_core::types::Mobile,
    /// Live async `agent` workers spawned this session.  Only the root
    /// populates it (`agent` is root-only); a forked child carries an empty
    /// one it never uses.  Survives `/clear`: `clear` bumps its generation
    /// and cancels its workers rather than replacing it, so a worker that
    /// settles after the clear (holding a clone of this same registry) finds
    /// its generation stale and drops its result.
    pub(crate) agents: crate::agent_registry::AgentRegistry,
}

/// Outcome of one [`Session::apply`].  Degenerate cases (`Empty`,
/// `Stopped`) become nudges; `Cancelled` and `Capped` do not; hard
/// failures travel through [`ProviderError`].
#[derive(Debug)]
pub enum TurnOutcome {
    Complete(String),
    Empty,
    Stopped {
        reason: String,
    },
    Cancelled,
    /// The round-trip loop hit [`MAX_STEPS`] without the model ever
    /// emitting a tool-call-free reply.  Terminal: it carries no nudge
    /// (re-driving would just spend another `MAX_STEPS`).
    Capped,
}

/// Hard ceiling on provider round-trips in one [`Session::apply`].  The
/// interactive frontend has Esc to halt a runaway turn; headless and
/// autonomous sub-agent runs have nothing, so a model that keeps
/// emitting tool calls would loop until the token budget or the wall
/// runs out.  Bounding the step count keeps benchmark and headless runs
/// terminating.  Generous enough that no genuine interactive turn ever
/// reaches it.
const MAX_STEPS: u32 = 250;

pub(crate) fn fresh_id() -> SessionId {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

impl Session {
    fn assemble(
        system: String,
        caps: ral_core::types::Capabilities,
        mut shell: Shell,
        log: SessionLog,
        is_subagent: bool,
        expect_action: bool,
    ) -> Self {
        seed_session_dir(&mut shell, &log);
        let durable = shell.mobile.clone();
        Self {
            id: log.id(),
            system,
            log,
            shell,
            caps,
            is_subagent,
            expect_action,
            acted: false,
            durable,
            agents: crate::agent_registry::AgentRegistry::new(),
        }
    }

    fn replace_shell(&mut self, mut shell: Shell) {
        seed_session_dir(&mut shell, &self.log);
        self.durable = shell.mobile.clone();
        self.shell = shell;
    }

    pub(crate) fn root(
        system: String,
        caps: ral_core::types::Capabilities,
        scratch: &Scratch,
        run_dir: &std::path::Path,
        model: &str,
        provider_label: &str,
        expect_action: bool,
    ) -> io::Result<Self> {
        let shell = boot_root_shell(scratch);
        let sessions_root = run_dir.join("sessions");
        let id = fresh_id();
        let log = SessionLog::root(&sessions_root, id, model, provider_label, system.len())?;
        Ok(Self::assemble(
            system,
            caps,
            shell,
            log,
            false,
            expect_action,
        ))
    }

    pub(crate) fn clear(&mut self, scratch: &Scratch) -> io::Result<()> {
        // `boot_root_shell` owns the signal ceremony: stale ral interrupts
        // are discarded before embedded library evaluation, and the exarch
        // cancel chain is restored over ral's freshly installed handlers.
        let shell = boot_root_shell(scratch);
        self.log.clear(self.system.len())?;
        self.replace_shell(shell);
        // Cancel every live async agent and advance the generation so any
        // worker that settles after this clear drops its result rather than
        // delivering it into the rebuilt context.
        self.agents.clear();
        Ok(())
    }

    pub(crate) fn fork(&self) -> io::Result<Session> {
        // Snapshot the parent's whole shell into the child: `scope` (the
        // baked ral prelude, the agent helper library, and any bindings
        // the parent has accumulated — none of which survive a bare
        // `Shell::new`) and `context` (cwd, env, dynamic context). Only
        // the evaluator's control counters start fresh: the child is a new
        // session, not a continuation of the parent's call stack.
        let mut shell = Shell::new(crate::bootstrap::probe_terminal());
        shell.mobile.scope = self.shell.mobile.scope.clone();
        shell.mobile.context = self.shell.mobile.context.clone();
        let child_id = fresh_id();
        let log = self.log.fork(child_id, self.system.len())?;
        Ok(Self::assemble(
            self.system.clone(),
            self.caps.clone(),
            shell,
            log,
            true,
            false,
        ))
    }

    pub(crate) fn log_dir(&self) -> &std::path::Path {
        self.log.dir()
    }

    /// Build a root session against a throwaway sessions root under
    /// `dir`, with default (unrestricted) capabilities and a baked
    /// shell.  The harness in `tests/` uses this to drive [`Self::apply`]
    /// and [`Self::run_turn`] through a [`Provider::scripted`] backend.
    pub fn for_test(dir: &std::path::Path, system: &str) -> io::Result<Self> {
        let shell = crate::bootstrap::boot_shell();
        let id = fresh_id();
        let log = SessionLog::root(
            &dir.join("sessions"),
            id,
            "test-model",
            "test",
            system.len(),
        )?;
        Ok(Self::assemble(
            system.to_string(),
            ral_core::types::Capabilities::default(),
            shell,
            log,
            false,
            false,
        ))
    }

    /// Whether the session is at a settled turn boundary — every turn
    /// must hand it back here.  Exposed for the harness to assert the
    /// transcript-admission invariant after a turn.
    pub fn is_ready(&self) -> bool {
        self.log.is_ready()
    }

    /// The model-view messages the next request would carry.  Exposed
    /// for the harness to assert no malformed / empty message survives
    /// into the committed transcript.
    pub fn rendered_messages(&self) -> Vec<genai::chat::ChatMessage> {
        self.log.history_messages()
    }

    /// Serialised model-view byte count — the compaction-threshold
    /// input.  Exposed so the harness can assert compaction fired.
    pub fn history_bytes(&self) -> usize {
        self.log.history_bytes()
    }

    pub fn apply(
        &mut self,
        provider: &Arc<Provider>,
        prompt: Option<String>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> Result<TurnOutcome, ProviderError> {
        // No clear here: the root turn mints a fresh token (see
        // `run_turn`), and a sub-agent shares the parent's.  Clearing here
        // would erase a just-pressed Esc the moment a sub-agent's `apply`
        // begins (X5).
        let mut last_text = String::new();
        // Auto-compaction runs here, at the one boundary where
        // `can_compact()` actually holds — `apply` is entered `ReadyForUser`
        // (the turn-ends-ready invariant), before the prompt is committed.
        // The prior mid-turn call sat in `AwaitingAssistantAfterToolResults`,
        // where `can_compact()` is always false, so it never fired (X1).
        // Every provider round-trip — each user turn, each nudge iteration —
        // passes through here, so long autonomous and headless runs stay
        // bounded.
        self.compact(provider, emit, false, token);
        if let Some(p) = prompt {
            self.log
                .append_user(p)
                .map_err(|e| ProviderError::Other(e.to_string()))?;
        }
        let mut n = 0u32;
        loop {
            n += 1;
            if n > MAX_STEPS {
                return self.capped(emit);
            }
            self.log
                .record_step(n)
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            emit.emit(Kind::Step(n));
            last_text.clear();
            emit.emit(Kind::Phase("rendering context".into()));
            #[cfg(debug_assertions)]
            let t_render = std::time::Instant::now();
            let messages = self
                .log
                .render_messages()
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            ral_core::dbg_trace!(
                "turn",
                "render_messages: {} msgs in {:?}",
                messages.len(),
                t_render.elapsed()
            );
            #[cfg(debug_assertions)]
            let t_req = std::time::Instant::now();
            #[cfg(debug_assertions)]
            let mut first_token: Option<std::time::Duration> = None;
            emit.emit(Kind::Phase("waiting for model".into()));
            let step_out = {
                let token_emit = emit.clone();
                provider.complete(
                    &self.system,
                    messages,
                    !self.is_subagent,
                    &mut |t: &str| {
                        #[cfg(debug_assertions)]
                        if first_token.is_none() {
                            first_token = Some(t_req.elapsed());
                        }
                        last_text.push_str(t);
                        token_emit.emit(Kind::Token(t.to_string()));
                    },
                    token,
                )
            };
            ral_core::dbg_trace!(
                "turn",
                "provider.complete: first token {first_token:?}, full {:?}",
                t_req.elapsed()
            );
            emit.emit(Kind::Boundary);
            if token.is_cancelled() {
                return self.cancelled(emit);
            }
            let StepOut {
                mut assistant_message,
                tool_calls,
                usage,
                stop_reason,
            } = match step_out {
                Ok(s) => s,
                Err(ProviderError::Cancelled(_)) => {
                    return self.cancelled(emit);
                }
                Err(e) => return Err(e),
            };
            self.log
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
                .append_assistant(
                    assistant_message,
                    tool_ids,
                    stop_reason.as_ref().map(|r| r.raw().to_string()),
                )
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            let truncated = matches!(stop_reason, Some(StopReason::MaxTokens(_)));
            // `MaxTokens` truncated the assistant.  With no captured tool
            // call there is nothing to dispatch and the assistant turn is
            // final, so surface the truncation and nudge from the boundary.
            // With captured tool calls, the assistant message carries
            // `tool_ids` and the session is now `AwaitingToolResults`;
            // returning here would strand it there and the nudge's
            // `append_user` would fail "tool results pending" (X6).  Instead
            // fall through to dispatch the calls and continue the loop — the
            // next round-trip resumes the truncated turn with the results in
            // hand.
            if truncated && tool_calls.is_empty() {
                let reason = stop_reason
                    .as_ref()
                    .map(|r| r.raw().to_string())
                    .unwrap_or_else(|| "max_tokens".into());
                self.note_error(
                    format!(
                        "turn truncated (stop_reason={reason}): output cap reached. \
                         re-run with `--max-tokens N` for a larger ceiling, \
                         or ask the agent to split the work into smaller turns.",
                    ),
                    emit,
                );
                return Err(ProviderError::Truncated { reason });
            }
            if tool_calls.is_empty() {
                return Ok(match &stop_reason {
                    Some(r) if !matches!(r, StopReason::Completed(_) | StopReason::ToolCall(_)) => {
                        TurnOutcome::Stopped {
                            reason: r.raw().to_string(),
                        }
                    }
                    _ if last_text.is_empty() => TurnOutcome::Empty,
                    _ => TurnOutcome::Complete(last_text),
                });
            }
            if truncated {
                self.note_dim(
                    "[turn truncated by the output cap mid-tool-call; dispatching the \
                     captured calls and continuing]"
                        .into(),
                    emit,
                );
            }
            self.acted = true;
            let Dispatch { results, steering } =
                self.dispatch(provider, tool_calls, token, emit)?;
            self.log
                .append_tool_results(results)
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            if let Some(text) = steering {
                self.log
                    .append_steering(text.clone())
                    .map_err(|e| ProviderError::Other(e.to_string()))?;
                emit.emit(Kind::UserPromptEcho(text));
            }
            if token.is_cancelled() {
                return self.cancelled(emit);
            }
        }
    }

    /// One user turn.  Each iteration runs one [`Session::apply`] and
    /// hands the outcome to [`nudge::Registry::react`], which decides
    /// whether to stop or to loop with a (possibly synthetic) next
    /// prompt.
    pub fn run_turn<S: Sink>(
        &mut self,
        sink: &mut S,
        provider: &Arc<Provider>,
        prompt: Option<String>,
    ) -> Result<(), String> {
        let id = self.id;
        let mut pending = prompt;
        let mut nudges = nudge::Registry::new();
        self.acted = false;
        // Mint this root turn's cancellation token.  Minting publishes a
        // fresh, un-cancelled token for the signal handler and is itself
        // the reset; the guard retires it when the turn ends.  Every
        // `apply` this turn — including a sub-agent's, which shares this
        // token rather than minting — reads it, so one Esc halts the tree.
        let root = cancel::mint_root();
        let outcome = loop {
            // Reborrow per iteration so the worker's closure owns only
            // a short-lived `&mut Session`; the outer loop keeps the
            // original for the next pass + post-attempt chrome.
            let attempt = {
                let s: &mut Session = &mut *self;
                let p = pending.take();
                let token = root.token().clone();
                match pump(sink, id, move |emit| s.apply(provider, p, &token, emit)) {
                    Ok(a) => a,
                    Err(e) => break Err(e.to_string()),
                }
            };
            // `None` is a worker panic — the error line is already out.
            // `run_turn`'s frame guard has self-healed the IO frame
            // on unwind; here we resume from the last committed `Mobile`
            // snapshot, rolling the whole dynamic state — grant frames,
            // env/cwd overrides, the handler stack, and any bindings the
            // panicking call half-applied — back to the last clean
            // tool-call boundary.  Completed calls' bindings and cwd live
            // in the snapshot and survive; the panicking call's do not.
            let Some(attempt) = attempt else {
                self.shell.mobile = self.durable.clone();
                break Ok(());
            };
            let ctx = nudge::NudgeCtx {
                expect_action: self.expect_action && !self.is_subagent,
                acted: self.acted,
            };
            match nudges.react(&attempt, ctx, sink, &mut self.log, id) {
                nudge::Step::Stop => break Ok(()),
                nudge::Step::Continue(s) => pending = Some(s),
            }
        };
        // A turn always hands the session back ready for the next user
        // prompt.  A surfaced error leaves the committed prompt stranded
        // mid-protocol — wind it back here, through the single exit, so
        // the driver's next `append_user` is always admissible.
        if !self.log.is_ready() {
            self.log.quiesce(QuiesceReason::Aborted);
        }
        debug_assert!(
            self.log.is_ready(),
            "run_turn must leave the session ReadyForUser"
        );
        outcome
    }

    pub(crate) fn compact(
        &mut self,
        provider: &Arc<Provider>,
        emit: &Emitter,
        requested: bool,
        token: &cancel::Token,
    ) {
        if !self.log.can_compact() {
            if requested {
                self.note_error("cannot compact while tool results are pending".into(), emit);
            }
            return;
        }
        let bytes = self.log.history_bytes();
        if bytes < COMPACT_THRESHOLD {
            return;
        }
        // A turn-boundary Esc must not kick off a summarize request we'd
        // only instantly cancel; bail before the work and let `apply`'s
        // post-compact check return to the prompt.
        if token.is_cancelled() {
            return;
        }
        self.note_dim(
            format!("[compacting history: {} KB → summary]", bytes / 1024),
            emit,
        );
        emit.emit(Kind::Phase("compacting history".into()));
        match provider.summarize(&self.system, self.log.history_messages(), token) {
            Ok(summary) => {
                if let Err(e) = self.log.record_usage(summary.usage.into()) {
                    self.note_error(format!("compact failed: {e}"), emit);
                    return;
                }
                emit.emit(Kind::Usage(summary.usage));
                if let Err(e) = self.log.replace_with_summary(summary.summary) {
                    self.note_error(format!("compact failed: {e}"), emit);
                    return;
                }
                self.note_dim(
                    format!("[compacted: now {} KB]", self.log.history_bytes() / 1024),
                    emit,
                );
            }
            Err(e) => self.note_error(format!("compact failed: {e}"), emit),
        }
    }

    // --- private helpers ---

    fn dispatch(
        &mut self,
        provider: &Arc<Provider>,
        tool_calls: Vec<ToolCall>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> Result<Dispatch, ProviderError> {
        thread::scope(|scope| -> Result<Dispatch, ProviderError> {
            let mut staged: Vec<Staged<'_>> = Vec::with_capacity(tool_calls.len());
            let mut it = tool_calls.into_iter();
            while let Some(call) = it.next() {
                if token.is_cancelled() {
                    staged.push(Staged::Done(cancelled_result(call.call_id)));
                    staged.extend(it.map(|r| Staged::Done(cancelled_result(r.call_id))));
                    break;
                }
                staged.push(self.stage(provider, call, token, emit, scope));
            }
            let results = staged.into_iter().map(Staged::finish).collect();
            Ok(Dispatch {
                results,
                steering: self.take_steering(emit),
            })
        })
    }

    fn take_steering(&self, emit: &Emitter) -> Option<String> {
        if self.is_subagent {
            None
        } else {
            emit.drain_tool_steering()
        }
    }

    fn stage<'scope, 'env: 'scope>(
        &mut self,
        provider: &'env Arc<Provider>,
        call: ToolCall,
        token: &'env cancel::Token,
        emit: &Emitter,
        scope: &'scope thread::Scope<'scope, 'env>,
    ) -> Staged<'scope> {
        match crate::tools::find(&call.fn_name) {
            Some(t) if t.root_only() && self.is_subagent => {
                let msg = format!(
                    "tool `{}` is not available to sub-agents — do this work yourself",
                    call.fn_name
                );
                self.note_error(msg.clone(), emit);
                Staged::Done(SessionToolResult {
                    id: call.call_id,
                    content: msg,
                })
            }
            Some(t) => t.dispatch(
                call.call_id,
                call.fn_arguments,
                self,
                provider,
                token,
                emit,
                scope,
            ),
            None => {
                let msg = format!("unknown tool `{}`", call.fn_name);
                self.note_error(msg.clone(), emit);
                Staged::Done(SessionToolResult {
                    id: call.call_id,
                    content: msg,
                })
            }
        }
    }

    pub(crate) fn cwd(&self) -> std::path::PathBuf {
        self.shell.cwd()
    }

    /// Best-effort dual-write: log the chrome line, then forward it
    /// through `emit`.  A log write-failure must not block the user line.
    pub(crate) fn note_error(&mut self, msg: String, emit: &Emitter) {
        let _ = self.log.record_error(msg.clone());
        emit.emit(Kind::Error(msg));
    }

    pub(crate) fn note_dim(&mut self, text: String, emit: &Emitter) {
        let _ = self.log.record_dim(text.clone());
        emit.emit(Kind::Dim(text));
    }

    pub(crate) fn run_shell(
        &mut self,
        id: String,
        cmd: &str,
        timeout_secs: u64,
        emit: &Emitter,
    ) -> SessionToolResult {
        // Refresh the durable snapshot at this clean boundary: the
        // dynamic context here reflects every prior tool call that
        // returned, and none that this one is about to mutate.  If the
        // eval below panics, `run_turn` rebuilds the live context from
        // this snapshot, rolling the panicking call's effects back.
        self.durable = self.shell.mobile.clone();
        let content =
            match shell_eval::run_shell(&mut self.shell, &self.caps, cmd, timeout_secs, emit) {
                shell_eval::Outcome::Ran(r) => render(&r),
                shell_eval::Outcome::Static(s) => clip(&s, OPAQUE_CAP),
            };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }

    fn cancelled(&mut self, emit: &Emitter) -> Result<TurnOutcome, ProviderError> {
        // No clear: a cancelled turn is `Stop`ped by the nudge registry,
        // so `run_turn` returns and the next root turn mints a fresh,
        // un-cancelled token (the reset).  Clearing the live token here
        // would race a cancel still in flight to a sibling sub-agent.
        self.log.quiesce(QuiesceReason::Cancelled);
        // The canonical log already carries `Cancelled` from quiesce;
        // this is the user-facing companion only.
        emit.emit(Kind::Error("cancelled".into()));
        Ok(TurnOutcome::Cancelled)
    }

    /// Reached at the top of the round-trip loop once the step count
    /// would exceed [`MAX_STEPS`].  The history is mid-protocol (the last
    /// step appended its tool results); `run_turn`'s single exit winds it
    /// back to `ReadyForUser`.  `note_error` is the user-facing line and
    /// the forensic breadcrumb; the `StopReason` surfaces in the headless
    /// JSON result so a benchmark harness can tell a capped run from a
    /// completed one.
    fn capped(&mut self, emit: &Emitter) -> Result<TurnOutcome, ProviderError> {
        self.note_error(
            format!("step cap reached ({MAX_STEPS} provider round-trips); ending turn"),
            emit,
        );
        emit.emit(Kind::StopReason("step_cap".into()));
        Ok(TurnOutcome::Capped)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.log.record_session_ended();
    }
}

struct Dispatch {
    results: Vec<SessionToolResult>,
    steering: Option<String>,
}

pub(crate) enum Staged<'scope> {
    Done(SessionToolResult),
    /// `agent` collapses the child's outcome to a reply string at the spawn
    /// site; dispatch joins every same-batch child before the next provider
    /// request, so same-batch agents may run concurrently but never outlive
    /// the parent turn.
    Spawned {
        id: String,
        handle: thread::ScopedJoinHandle<'scope, Result<String, ProviderError>>,
    },
}

impl<'scope> Staged<'scope> {
    fn finish(self) -> SessionToolResult {
        match self {
            Staged::Done(r) => r,
            Staged::Spawned { id, handle } => {
                let content = match handle.join() {
                    Ok(Ok(reply)) => clip(&reply, AGENT_REPLY_CAP),
                    Ok(Err(e)) => format!("call error: {e}"),
                    Err(_) => "call panicked".into(),
                };
                SessionToolResult { id, content }
            }
        }
    }
}

fn boot_root_shell(scratch: &Scratch) -> Shell {
    let mut shell = crate::bootstrap::boot_shell();
    scratch.install_into(&mut shell);
    shell
}

/// Seed `EXARCH_SESSION_DIR` into `shell` from `log`'s directory.  Run
/// at construction and again after [`Session::clear`] rebuilds the
/// shell, so the name always points at the live session's event-log
/// directory.
fn seed_session_dir(shell: &mut Shell, log: &SessionLog) {
    let dir = log.dir().to_string_lossy().into_owned();
    crate::bootstrap::seed_var(shell, "EXARCH_SESSION_DIR", &dir);
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
/// message makes it non-final, so the empty-turn nudge — which appends a
/// user prompt right after — would poison every subsequent request.  A
/// short stub keeps the turn renderable; the nudge still recovers the
/// empty reply by re-prompting.
const EMPTY_ASSISTANT_STUB: &str = "(no content)";

/// Normalise an assistant message to the transcript-admission invariant
/// at the `apply` commit boundary: **every committed message serialises
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
    for part in msg.content.iter_mut() {
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
#[allow(clippy::disallowed_methods, reason = "[io-door:test] test fs/process scaffolding")]
mod tests {
    //! Panic-recovery integrity (A4): a worker panic mid-tool-eval must
    //! preserve the bindings completed tool calls left behind and leave
    //! the dynamic context clean for the next turn.  Driven through the
    //! scripted provider and `run_turn` — the real path: `pump` catches
    //! the unwind, `run_turn`'s frame guard self-heals the IO frame, and
    //! `run_turn` rebuilds the live context from the durable snapshot.

    use super::*;
    use crate::bus::Event;
    use crate::provider::scripted::{Reply, Script};
    use ral_core::Value;
    use ral_core::typecheck::builtins::{BuiltinTypeRule, mk_scheme, pure, thunk};
    use ral_core::typecheck::{Scheme, Ty, Unifier};
    use ral_core::types::{BuiltinBody, BuiltinEntry, Settled};
    use std::borrow::Cow;

    /// A scripted provider behind the `Arc` the turn driver threads.
    fn scripted(model: &str, script: Script) -> Arc<Provider> {
        Arc::new(Provider::scripted(model, script))
    }

    /// A nullary builtin whose body panics — stands in for any Rust panic
    /// the evaluator can raise mid-tool-eval.
    fn builtin_panic_now(_args: &[Value], _shell: &mut Shell) -> Settled<Value> {
        panic!("a4 test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut Unifier) -> Scheme {
        mk_scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
    }

    static PANIC_BUILTINS: &[BuiltinEntry] = &[BuiltinEntry {
        name: Cow::Borrowed("a4-panic-now"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_panic_now),
        doc: "test-only: panic the evaluator mid-eval.",
        body: BuiltinBody::Static(builtin_panic_now),
    }];

    /// Discards every event; the assertions read session state directly.
    struct NullSink;
    impl Sink for NullSink {
        fn handle(&mut self, _e: Event) {}
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("exarch-a4-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ral_call(id: &str, cmd: &str) -> ToolCall {
        ToolCall {
            call_id: id.into(),
            fn_name: "ral".into(),
            fn_arguments: serde_json::json!({
                "cmd": cmd,
                "description": "a4 test command",
            }),
            thought_signatures: None,
        }
    }

    #[test]
    fn worker_panic_preserves_completed_bindings_and_clean_context() {
        ral_core::builtins::register_builtins(PANIC_BUILTINS);
        let dir = tmp("panic-recovery");
        let mut session = Session::for_test(&dir, "system").unwrap();
        session.shell.install_builtins(PANIC_BUILTINS);
        // Refresh `durable` so the snapshot reflects the just-installed
        // builtin frame, matching the production boundary where the
        // baseline is the booted shell.
        session.durable = session.shell.mobile.clone();
        let baseline_grant_depth = session.shell.mobile.context.grants.iter().count();

        // 1st call binds `a4_x` (completes); 2nd call panics mid-eval.
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let a4_x = 7")]))
                .then(Reply::tool_calls(vec![ral_call("c2", "a4-panic-now")])),
        );

        session
            .run_turn(&mut NullSink, &provider, Some("compute then crash".into()))
            .expect("run_turn absorbs the worker panic and returns Ok");

        // The completed call's binding survives the panic.
        assert!(
            session.shell.mobile.scope.get("a4_x").is_some(),
            "a binding from a completed tool call must survive a later call's panic"
        );
        // The dynamic context is rolled back to the clean boundary: no
        // leaked grant frame from the panicking call's `with_capabilities`.
        assert_eq!(
            session.shell.mobile.context.grants.iter().count(),
            baseline_grant_depth,
            "the panicking call's grant frame must not leak into the next turn"
        );
        // The turn handed the session back ready for a fresh prompt.
        assert!(
            session.is_ready(),
            "run_turn must leave the session ReadyForUser even after a worker panic"
        );

        // The next turn is admissible and runs to completion on the
        // healed shell.
        let provider2 = scripted("test-model", Script::new().then(Reply::text("ok")));
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let root = cancel::mint_root();
        match session.apply(&provider2, Some("continue".into()), root.token(), &emit) {
            Ok(TurnOutcome::Complete(s)) => assert_eq!(s, "ok"),
            other => panic!("next turn on the healed shell must complete, got {other:?}"),
        }
    }

    /// A11/X5: a sub-agent shares the root turn's cancellation token, so an
    /// Esc that lands on the parent's token before the child enters its own
    /// `apply` still cancels the child.  Modelled deterministically: the
    /// parent mints the root token (the `agent` tool would hand a clone to
    /// the child), it is cancelled, and the child's `apply` — driven on the
    /// shared token — short-circuits to `Cancelled` rather than running the
    /// scripted turn.  A pre-fix child cleared the flag at the top of its
    /// own `apply` and ran on regardless.
    #[test]
    fn subagent_apply_honours_a_shared_cancelled_token() {
        let dir = tmp("subagent-cancel");
        let session = Session::for_test(&dir, "system").unwrap();
        let mut child = session.fork().expect("fork child session");

        // The parent's root token, shared with the child as the `agent`
        // tool would.  An Esc lands before the child runs.
        let root = cancel::mint_root();
        let child_token = root.token().clone();
        root.token().cancel();

        // The child would complete with "leaked" on the scripted provider;
        // honouring the shared token, it must cancel instead.
        let provider = scripted("test-model", Script::new().then(Reply::text("leaked")));
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, child.id);
        match child.apply(&provider, Some("do work".into()), &child_token, &emit) {
            Ok(TurnOutcome::Cancelled) => {}
            other => panic!("a sub-agent on a cancelled shared token must cancel, got {other:?}"),
        }
    }
}
