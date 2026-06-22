//! One continuous agent session: canonical event log, persistent shell,
//! capability set, and the shared turn driver every agent runs.
//!
//! [`Session::apply`] runs one round-trip against the provider;
//! [`Session::drive`] is the one loop — root and sub-agent alike — that pulls
//! turn-boundary messages from the session's own [`Inbox`], runs one `apply`
//! per message, and turns a nudge-worthy outcome into a self-posted
//! [`InboxMsg::Nudge`].  The only differences between agents are structural:
//! `is_root` (may spawn, mints the Esc token per turn), `park_when_idle` (an
//! interactive root parks for the human; a headless root and every peer
//! terminate at quiescence), `expect_action`, and `tools`.  A peer's single
//! result is delivered up its parent's mailbox by the spawn site, not here, so
//! `drive` itself is identical for both.

use crate::bootstrap::Scratch;
use crate::bus::{
    AgentOutcome, Emitter, Inbox, InboxMsg, Kind, Mailbox, SessionId, Turn, WORKER_PANIC_PREFIX,
};
use crate::cancel;
use crate::digest::{
    AGENT_REPLY_CAP, COMPACT_THRESHOLD, OPAQUE_CAP, SUMMARY_CAP_FALLBACK_TOKENS, clip,
    compaction_due, render, suffix_keep_budget, summary_cap_tokens,
};
use crate::event::{QuiesceReason, SessionLog, ToolResult as SessionToolResult};
use crate::nudge;
use crate::provider::{Provider, ProviderError, StepOut, StopReason, ToolCall};
use crate::shell_eval;
use crate::tools::ToolSet;
use ral_core::Shell;
use std::io;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Session {
    pub id: SessionId,
    pub(crate) system: String,
    log: SessionLog,
    shell: Shell,
    caps: ral_core::types::Capabilities,
    /// This session's own inbox — the consumer side of its message queue.
    /// [`Session::drive`] pulls turn-boundary deliverables from here;
    /// [`Session::apply`] drains tool-boundary steering from it mid-round-trip.
    /// Self-nudges and self-armed wakeups land here; a child's result lands in
    /// its *parent's* inbox, never reaching across into a sibling's.
    inbox: Inbox,
    /// Whether this session may spawn children — it holds the `agent` family
    /// and owns a populated [`Self::agents`].  The one structural bit `drive`
    /// branches on: only a root mints the process Esc token per turn; a peer
    /// holds one sticky token for life.
    is_root: bool,
    /// Whether `drive` parks on an empty inbox or terminates.  True only for
    /// an interactive root, whose human is an ever-present writer; a headless
    /// root and every peer terminate at quiescence (a peer still parks while a
    /// self-schedule is armed).
    park_when_idle: bool,
    /// When set (root only), turn completion is gated by two one-shot,
    /// budget-free nudges: a turn that used no tool is nudged to engage, one
    /// that did is nudged to verify.  Peers never carry it.
    expect_action: bool,
    /// Per-session nudge state — the retry budget and the one-shot completion
    /// gates.  Its latches reset on a genuine turn-boundary message, never on
    /// a self-[`InboxMsg::Nudge`], so a multi-step nudge sequence runs to
    /// completion within one turn.
    nudges: nudge::Registry,
    /// This session's cancellation token.  A peer owns one sticky token for
    /// life (registered, cancelled by `agent_cancel` / `/clear` / its
    /// ceiling).  The root re-mints a fresh *published* token at each turn
    /// boundary (the X5 reset), so this field holds the root's current turn
    /// token; an Esc on one turn never bleeds into the next.
    cancel: cancel::Token,
    /// Which tools this session advertises and may dispatch — the single
    /// source of truth for both, read by `provider.complete` (advertisement)
    /// and [`Self::stage`] (enforcement).  Full at the root; full minus the
    /// spawn family at a peer, so the spawn tree stays one level deep.
    tools: ToolSet,
    /// Whether the current turn has dispatched any tool call.  Reset on each
    /// genuine turn-boundary message; read by the completion-gate nudges to
    /// choose between the idle and verify prompts.
    acted: bool,
    /// A sub-agent's staged return value, set by the `reply` tool's dispatch
    /// (via [`Self::set_reply`]) and lifted by [`Self::apply`] into a
    /// [`TurnOutcome::Replied`] once the current tool-call batch finishes
    /// draining — never mid-batch, so the session reaches a clean boundary
    /// with every `call_id` answered.  Empty (or absent) renders to nothing
    /// and settles as [`AgentOutcome`](crate::bus::AgentOutcome)`::Empty`.
    /// Only a peer ever sets it; `reply` is withheld from the root.
    reply: Option<String>,
    /// Durable [`MobileSnapshot`](ral_core::types::MobileSnapshot) of the
    /// shell as of the last clean tool-call boundary.  Refreshed inside
    /// `run_shell` right before each dispatch, so it always holds the dynamic
    /// context completed calls left behind.  Read by [`Self::drive`] after a
    /// caught worker panic to roll the panicking call's dynamic-context
    /// effects back; it lives on `Session` precisely so it survives the
    /// `catch_unwind` boundary.
    durable: ral_core::types::MobileSnapshot,
    /// Live async `agent` workers spawned this session.  Only a root populates
    /// it (`agent` is in the root tool set); a peer carries an empty one it
    /// never uses.  Survives `/clear`: the generation is bumped and workers
    /// cancelled, so a worker that settles after the clear drops its result.
    pub(crate) agents: crate::agent_registry::AgentRegistry,
    /// Live scheduled wakeups (cron / after).  A peer may self-schedule (it
    /// posts wakeups into its own inbox), so this is no longer root-only; it
    /// is still gated by [`Self::schedule_authority`].
    pub(crate) schedules: crate::schedule::ScheduleRegistry,
    /// Whether self-scheduling is authorised this session.  Off by default;
    /// inherited by forks, so a `--allow-schedule` root grants its peers the
    /// same authority.
    pub(crate) schedule_authority: bool,
    /// Input tokens the model saw on the most recent completion — the live
    /// numerator for the context-pressure compaction trigger
    /// ([`Self::compact`]), the same signal the TUI's fidelity gauge reads.
    /// Set from each step's usage; reset to `0` on `/clear`.
    last_input: u64,
}

/// Outcome of one [`Session::apply`].  Degenerate cases (`Empty`,
/// `Stopped`) become nudges; `Cancelled` and `Capped` do not; hard
/// failures travel through [`ProviderError`].
#[derive(Debug)]
pub enum TurnOutcome {
    Complete(String),
    /// A sub-agent called `reply`: the carried payload is its deliberate
    /// return value (empty when the argument rendered to nothing).  Distinct
    /// from [`Self::Complete`] precisely so the nudge layer can tell "already
    /// returned" from "stopped without returning" and not re-nudge a child
    /// that replied.  Terminal: it ends the drive loop.
    Replied(String),
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

/// A shared, swappable handle to the active provider.  The drive loop reads
/// the current provider once per turn through [`Self::current`]; a `/model`
/// switch on the frontend's UI thread swaps it through [`Self::swap`].  Live
/// provider replacement across the UI / drive thread boundary needs a shared
/// cell rather than an owned `Arc` the worker would hold privately; a `Mutex`
/// suffices (the cell is touched at most once per turn and on the rare
/// switch).  A peer wraps a *snapshot* of the provider at spawn, so a later
/// root `/model` never disturbs an already-running child.
#[derive(Clone)]
pub struct ProviderHandle(Arc<Mutex<Arc<Provider>>>);

impl ProviderHandle {
    pub fn new(provider: Arc<Provider>) -> Self {
        Self(Arc::new(Mutex::new(provider)))
    }

    /// The provider in force for the next turn.
    pub fn current(&self) -> Arc<Provider> {
        self.0.lock().expect("provider handle poisoned").clone()
    }

    /// Replace the active provider (a `/model` switch).  Takes effect on the
    /// drive loop's next turn; an in-flight turn finishes on the provider it
    /// started with, and any running peer keeps its own snapshot.
    pub fn swap(&self, provider: Arc<Provider>) {
        *self.0.lock().expect("provider handle poisoned") = provider;
    }
}

/// What [`Session::drive`] should do after handing a session-affecting slash
/// command to [`Control`].
pub enum ControlFlow {
    Continue,
    Quit,
}

/// The frontend hook the drive loop calls for a session-affecting slash
/// command ([`Turn::Command`]) drained at the turn boundary — the drive thread
/// owns the session the command mutates, so it cannot run on the UI thread.
/// Only the non-interactive session commands route here (`/clear`, `/compact`,
/// `/quit`); `/model` swaps the [`ProviderHandle`] directly on the UI thread,
/// and view-only commands (`/help`, `/copy`, …) are handled frontend-side.
/// Off the TUI there are no such commands, so [`NoControl`] handles none.
pub trait Control {
    /// Run `raw` (a slash-command line) against the session the drive loop
    /// owns, returning whether the loop should quit.
    fn command(&mut self, raw: &str, session: &mut Session, emit: &Emitter) -> ControlFlow;
}

/// The no-op control for drivers with no slash commands (headless, peers,
/// tests): a [`Turn::Command`] should never reach them, so it is a no-op.
pub struct NoControl;

impl Control for NoControl {
    fn command(&mut self, _raw: &str, _session: &mut Session, _emit: &Emitter) -> ControlFlow {
        ControlFlow::Continue
    }
}

/// The launch configuration threaded into [`Session::assemble`] — bundled so
/// the one constructor reads at the call site rather than as a wall of bare
/// booleans.
struct Build {
    system: String,
    caps: ral_core::types::Capabilities,
    shell: Shell,
    log: SessionLog,
    is_root: bool,
    park_when_idle: bool,
    expect_action: bool,
    schedule_authority: bool,
    tools: ToolSet,
}

impl Session {
    fn assemble(b: Build) -> Self {
        let Build {
            system,
            caps,
            mut shell,
            log,
            is_root,
            park_when_idle,
            expect_action,
            schedule_authority,
            tools,
        } = b;
        seed_session_dir(&mut shell, &log);
        let durable = shell.mobile_snapshot();
        Self {
            id: log.id(),
            system,
            log,
            shell,
            caps,
            inbox: Inbox::new(),
            is_root,
            park_when_idle,
            expect_action,
            nudges: nudge::Registry::new(),
            cancel: cancel::Token::new(),
            tools,
            acted: false,
            reply: None,
            durable,
            agents: crate::agent_registry::AgentRegistry::new(),
            schedules: crate::schedule::ScheduleRegistry::new(),
            schedule_authority,
            last_input: 0,
        }
    }

    fn replace_shell(&mut self, mut shell: Shell) {
        seed_session_dir(&mut shell, &self.log);
        self.durable = shell.mobile_snapshot();
        self.shell = shell;
    }

    #[allow(clippy::too_many_arguments)] // launch config, threaded once at boot
    pub(crate) fn root(
        system: String,
        caps: ral_core::types::Capabilities,
        scratch: &Scratch,
        run_dir: &std::path::Path,
        model: &str,
        provider_label: &str,
        expect_action: bool,
        allow_schedule: bool,
        interactive: bool,
    ) -> io::Result<Self> {
        let shell = boot_root_shell(scratch);
        let sessions_root = run_dir.join("sessions");
        let id = fresh_id();
        let log = SessionLog::root(&sessions_root, id, model, provider_label, system.len())?;
        Ok(Self::assemble(Build {
            system,
            caps,
            shell,
            log,
            is_root: true,
            // Only an interactive root has an ever-present human writer to park
            // for; a headless root terminates once its seeded work is idle.
            park_when_idle: interactive,
            expect_action,
            schedule_authority: allow_schedule,
            tools: ToolSet::All,
        }))
    }

    pub(crate) fn clear(&mut self, scratch: &Scratch) -> io::Result<()> {
        // `boot_root_shell` owns the signal ceremony: stale ral interrupts
        // are discarded before embedded library evaluation, and the exarch
        // cancel chain is restored over ral's freshly installed handlers.
        let shell = boot_root_shell(scratch);
        self.log.clear(self.system.len())?;
        self.replace_shell(shell);
        // A rebuilt context starts empty: drop the stale pressure reading so
        // the next turn's usage sets it afresh.
        self.last_input = 0;
        // Drop every queued message: a rebuilt context carries neither stale
        // user steering nor non-human deliveries across the clear.
        self.inbox.clear();
        // Cancel every live async agent and advance the generation so any
        // worker that settles after this clear drops its result rather than
        // delivering it into the rebuilt context.
        self.agents.clear();
        // Drop every schedule too: a fresh root carries no pending wakeups.
        self.schedules.clear();
        Ok(())
    }

    pub(crate) fn fork(&self) -> io::Result<Session> {
        // The child is an independent fork of the parent session: it snapshots
        // the parent's scope (prelude, agent library, accumulated bindings),
        // dynamic context (cwd, env, grants), and installed builtin table (the
        // host's `window-hash`/`grep-files`/`edit` and the rest), and starts
        // fresh in control counters and session state — its own inbox, its own
        // (fresh) cancellation token, no terminal authority, no flow-back.
        // Core owns the flow matrix, so the builtin table can't be silently
        // dropped here the way a hand-copied field once was.
        let shell = self.shell.fork_session();
        let child_id = fresh_id();
        let log = self.log.fork(child_id, self.system.len())?;
        Ok(Self::assemble(Build {
            system: self.system.clone(),
            caps: self.caps.clone(),
            shell,
            log,
            is_root: false,
            // A peer has no human; it terminates at quiescence (parking only
            // while a self-schedule it armed is still live).
            park_when_idle: false,
            expect_action: false,
            // Self-scheduling authority is inherited: a `--allow-schedule` root
            // grants its peers the same right to wake themselves.
            schedule_authority: self.schedule_authority,
            // A peer may not spawn — withholding the spawn family from its tool
            // set bounds recursion to depth 1, advertised and enforced.
            tools: ToolSet::NoSpawn,
        }))
    }

    pub(crate) fn log_dir(&self) -> &std::path::Path {
        self.log.dir()
    }

    /// This session's mailbox — for the `schedule` tool to arm wakeups into
    /// its *own* inbox, and the spawn site to capture the parent's box as a
    /// child's upward result edge.
    pub(crate) fn mailbox(&self) -> Mailbox {
        self.inbox.mailbox()
    }

    /// A handle to this session's inbox — for a frontend to build the bus
    /// (whose emitters mint mailboxes onto this same queue) over the session's
    /// own queue, so producers and the drive loop share one inbox.
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// This session's cancellation token (read by the spawn site to register
    /// a peer's sticky token in the parent's [`Self::agents`]).
    pub(crate) fn cancel_token(&self) -> &cancel::Token {
        &self.cancel
    }

    /// Seed this session's inbox with its launch prompt — the spawn site calls
    /// it once on a freshly forked child, then drops its handle, so the only
    /// downward edge is this one write.
    pub(crate) fn seed(&self, prompt: String) {
        self.inbox.push_user(prompt);
    }

    /// Build a root session against a throwaway sessions root under
    /// `dir`, with default (unrestricted) capabilities and a baked
    /// shell.  The harness in `tests/` uses this to drive [`Self::apply`]
    /// and [`Self::drive`] through a [`Provider::scripted`] backend.
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
        Ok(Self::assemble(Build {
            system: system.to_string(),
            caps: ral_core::types::Capabilities::default(),
            shell,
            log,
            is_root: true,
            // Tests drive `apply`/`drive` directly and must never block; a test
            // root terminates at quiescence like a peer.
            park_when_idle: false,
            expect_action: false,
            schedule_authority: false,
            tools: ToolSet::All,
        }))
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

    /// The one shared turn loop — root and peer alike.  Pull a turn-boundary
    /// deliverable from the inbox, run one [`Self::apply`], let the nudge
    /// registry post any retry as a self-[`InboxMsg::Nudge`], and repeat.  An
    /// empty inbox either parks (an interactive root, or any agent with a live
    /// self-schedule) or terminates (a peer / headless root at quiescence).
    ///
    /// Per-`apply` panic recovery lives here — the `catch_unwind` rolls the
    /// dynamic context back to the last clean tool-call boundary and continues
    /// the loop, so one crashing turn never sinks the session.  Returns the
    /// final turn's `(outcome, text)` digest; the root ignores it, a peer's
    /// spawn site delivers it up the parent's mailbox.
    ///
    /// `provider` is the shared [`ProviderHandle`] the loop reads once per
    /// turn, so a `/model` swap on the UI thread takes effect next turn;
    /// `control` handles session-affecting slash commands ([`NoControl`] off
    /// the TUI).
    pub fn drive(
        &mut self,
        provider: ProviderHandle,
        control: &mut dyn Control,
        emit: &Emitter,
    ) -> (AgentOutcome, String) {
        // The root's published Esc token, re-minted per turn boundary; held
        // here so the slot stays valid for the whole turn and is retired when
        // the next mint (or this loop's end) drops it.  A peer mints nothing.
        let mut guard: Option<cancel::RootGuard> = None;
        let mut digest = (AgentOutcome::Empty, String::new());
        loop {
            let armed = self.schedules.armed();
            let Some(turn) = self
                .inbox
                .next_or_idle(self.park_when_idle, armed, &self.cancel)
            else {
                break;
            };
            // A genuine turn boundary resets the nudge latches and, at the
            // root, re-mints the cancellation token (minting is itself the
            // reset, so a prior turn's Esc cannot carry into this one).  A
            // self-nudge is the same turn continuing and resets neither.
            if turn.resets_turn() {
                self.nudges.reset();
                self.acted = false;
                if self.is_root {
                    let g = cancel::mint_root();
                    self.cancel = g.token().clone();
                    guard = Some(g);
                }
            }
            // Session-affecting slash commands run against the owned session,
            // never the model.
            if let Turn::Command(raw) = &turn {
                match control.command(raw, self, emit) {
                    ControlFlow::Quit => break,
                    ControlFlow::Continue => continue,
                }
            }
            announce(&turn, emit);
            // Read the provider in force for this turn; a `/model` swap lands
            // here next turn, never mid-turn.
            let active = provider.current();
            let token = self.cancel.clone();
            let prompt = Some(turn.text());
            let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.apply(&active, prompt, &token, emit)
            }));
            let outcome = match attempt {
                Ok(o) => o,
                Err(p) => {
                    // A worker panic mid-eval: roll the dynamic state — grant
                    // frames, env/cwd overrides, the handler stack, half-applied
                    // bindings — back to the last clean tool-call boundary.
                    // Completed calls' effects live in the snapshot and survive;
                    // the panicking call's do not.  The IO frame self-heals in
                    // ral_core's own run_turn guard; this is the dynamic half.
                    self.shell.restore_mobile(self.durable.clone());
                    let msg = panic_msg(&p);
                    self.note_error(format!("{WORKER_PANIC_PREFIX}{msg}"), emit);
                    digest = (
                        AgentOutcome::Failed(format!("worker panicked: {msg}")),
                        String::new(),
                    );
                    continue;
                }
            };
            digest = agent_digest(&outcome);
            let ctx = nudge::NudgeCtx {
                expect_action: self.expect_action,
                acted: self.acted,
                // A peer returns through `reply`; an un-replied finish earns
                // one reminder.  The root never returns, so it is exempt.
                must_reply: !self.is_root,
            };
            if let Some(msg) = self.nudges.react(&outcome, ctx, emit, &mut self.log) {
                self.inbox.push(InboxMsg::Nudge(msg));
            }
            // `reply` hard-terminates: end the drive loop even if a self-armed
            // schedule would otherwise keep the peer parked.
            if matches!(outcome, Ok(TurnOutcome::Replied(_))) {
                break;
            }
        }
        // The loop always hands the session back ready for the next prompt: a
        // surfaced error or caught panic can leave the committed prompt
        // stranded mid-protocol; wind it back here through the single exit.
        if !self.log.is_ready() {
            self.log.quiesce(QuiesceReason::Aborted);
        }
        debug_assert!(self.log.is_ready(), "drive must leave the session ReadyForUser");
        drop(guard);
        digest
    }

    pub fn apply(
        &mut self,
        provider: &Arc<Provider>,
        prompt: Option<String>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> Result<TurnOutcome, ProviderError> {
        // Auto-compaction runs here, at the one boundary where `can_compact()`
        // actually holds — `apply` is entered `ReadyForUser` (the
        // turn-ends-ready invariant), before the prompt is committed.  Every
        // provider round-trip — each user turn, each nudge iteration — passes
        // through here, so long autonomous and headless runs stay bounded.
        self.compact(provider, emit, false, token);
        let mut last_text = String::new();
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
                    &self.tools,
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
            // The tokens the model just saw — the live numerator for the
            // context-pressure compaction trigger at this turn's boundary.
            self.last_input = usage.input;
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
            let Dispatch { results, steering } = self.dispatch(provider, tool_calls, token, emit);
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
            // The batch has fully drained — every call dispatched, every
            // `call_id` answered.  If one of them was `reply`, end the run now
            // with its payload rather than looping for another round-trip.
            if let Some(payload) = self.reply.take() {
                return self.replied(payload);
            }
        }
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
                let bytes = self.log.history_bytes();
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
        // A turn-boundary Esc must not kick off a summarize request we'd
        // only instantly cancel; bail before the work and let `apply`'s
        // post-compact check return to the prompt.
        if token.is_cancelled() {
            return;
        }
        // Keep the recent half verbatim; summarise the older prefix.
        let keep = suffix_keep_budget(self.log.history_bytes());
        let Some(plan) = self.log.plan_compaction(keep) else {
            if requested {
                self.note_dim("[nothing to compact]".into(), emit);
            }
            return;
        };
        self.note_dim(format!("[compacting history: {detail} → summary]"), emit);
        emit.emit(Kind::Phase("compacting history".into()));
        match provider.summarize(&self.system, plan.prefix_messages, summary_cap, token) {
            Ok(summary) => {
                if let Err(e) = self.log.record_usage(summary.usage.into()) {
                    self.note_error(format!("compact failed: {e}"), emit);
                    return;
                }
                emit.emit(Kind::Usage(summary.usage));
                if let Err(e) = self.log.apply_compaction(summary.summary, plan.suffix_start) {
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

    /// Dispatch a batch of tool calls in order, short-circuiting the rest to
    /// cancelled results the instant the token trips.  Every tool returns its
    /// result synchronously now — the `agent` tool launches a detached peer
    /// and returns a start receipt — so there is no join phase and no
    /// `thread::scope`.
    fn dispatch(
        &mut self,
        provider: &Arc<Provider>,
        tool_calls: Vec<ToolCall>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> Dispatch {
        let mut results = Vec::with_capacity(tool_calls.len());
        let mut it = tool_calls.into_iter();
        for call in it.by_ref() {
            if token.is_cancelled() {
                results.push(cancelled_result(call.call_id));
                results.extend(it.map(|r| cancelled_result(r.call_id)));
                break;
            }
            results.push(self.stage(provider, call, emit));
        }
        Dispatch {
            results,
            steering: self.take_steering(),
        }
    }

    /// The tool-boundary steering drain: a non-slash user prompt that barged
    /// in mid-turn, taken from this session's own inbox.  A peer has no human
    /// writer, so its inbox holds no user steering and this is always `None`.
    fn take_steering(&self) -> Option<String> {
        self.inbox.drain_tool()
    }

    fn stage(&mut self, provider: &Arc<Provider>, call: ToolCall, emit: &Emitter) -> SessionToolResult {
        match crate::tools::find(&call.fn_name) {
            Some(t) if !self.tools.allows(t) => {
                // The allow-check is the backstop for two unadvertised cases:
                // a peer reaching for the spawn family, or the root reaching
                // for `reply`.  Name the right one.
                let msg = if t.spawns() {
                    format!(
                        "tool `{}` is not available to sub-agents — do this work yourself",
                        call.fn_name
                    )
                } else {
                    format!(
                        "tool `{}` is for sub-agents only — you finish a turn by replying to the user directly",
                        call.fn_name
                    )
                };
                self.note_error(msg.clone(), emit);
                SessionToolResult {
                    id: call.call_id,
                    content: msg,
                }
            }
            Some(t) => t.dispatch(call.call_id, call.fn_arguments, self, provider, emit),
            None => {
                let msg = format!("unknown tool `{}`", call.fn_name);
                self.note_error(msg.clone(), emit);
                SessionToolResult {
                    id: call.call_id,
                    content: msg,
                }
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
        // eval below panics, `drive` rebuilds the live context from
        // this snapshot, rolling the panicking call's effects back.
        self.durable = self.shell.mobile_snapshot();
        // The deferred-surface destination: a detached `spawn` worker flushes
        // its buffered batch here at completion, posted into this session's
        // own inbox (via `emit`'s mailbox) and guarded by the agent registry's
        // generation (so a `/clear` drops a stale batch).
        let boundary = shell_eval::boundary_sink(emit, self.id, &self.agents);
        let content = match shell_eval::run_shell(
            &mut self.shell,
            &self.caps,
            cmd,
            timeout_secs,
            emit,
            Some(boundary),
        ) {
            shell_eval::Outcome::Ran(r) => render(&r),
            shell_eval::Outcome::Static(s) => clip(&s, OPAQUE_CAP),
        };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }

    /// Stash a sub-agent's deliberate return value — called from the `reply`
    /// tool's dispatch.  [`Self::apply`] lifts it into a
    /// [`TurnOutcome::Replied`] once the tool-call batch drains.
    pub(crate) fn set_reply(&mut self, payload: String) {
        self.reply = Some(payload);
    }

    /// Reached when the batch carried a `reply`: wind the session back to
    /// `ReadyForUser` with the dedicated breadcrumb (the last round-trip
    /// dispatched the reply but never asked for a final assistant message, so
    /// the protocol sits in `AwaitingAssistantAfterToolResults`), and return
    /// the payload.  `drive` then breaks the loop — `reply` hard-terminates.
    fn replied(&mut self, payload: String) -> Result<TurnOutcome, ProviderError> {
        self.log.quiesce(QuiesceReason::Replied);
        Ok(TurnOutcome::Replied(payload))
    }

    fn cancelled(&mut self, emit: &Emitter) -> Result<TurnOutcome, ProviderError> {
        self.log.quiesce(QuiesceReason::Cancelled);
        // The canonical log already carries `Cancelled` from quiesce;
        // this is the user-facing companion only.
        emit.emit(Kind::Error("cancelled".into()));
        Ok(TurnOutcome::Cancelled)
    }

    /// Reached at the top of the round-trip loop once the step count
    /// would exceed [`MAX_STEPS`].  The history is mid-protocol (the last
    /// step appended its tool results); `drive`'s single exit winds it
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

/// The model-facing chrome a turn's source emits before its `apply`: a human
/// prompt and a wakeup echo as their (possibly marked) text, an agent result
/// renders as the dialable `↘` subagent block the sink already knows.  A
/// self-nudge and a command are quiet — a nudge is an internal continuation, a
/// command never reaches the model.
fn announce(turn: &Turn, emit: &Emitter) {
    match turn {
        Turn::Human(s) | Turn::Wakeup(s) => emit.emit(Kind::UserPromptEcho(s.clone())),
        Turn::Agent(r) => emit.emit(Kind::SubagentDone {
            title: r.title.clone(),
            outcome: r.outcome.clone(),
            text: r.text.clone(),
            elapsed: r.elapsed,
        }),
        // A detached `spawn` worker's deferred surface batch: decode each value
        // and emit it as the live foreground decode would, so its cards/io land
        // on the rail.  The emitter's id is this session's, which is exactly the
        // id the batch was stamped with (a session's boundary sink stamps with
        // its own id and posts to its own inbox), so the cards reach the right
        // viewport.  The model is then woken with `turn.text()`'s notice.
        Turn::Surface { values, .. } => {
            for v in values {
                if let Some(kind) = shell_eval::decode_surface(v) {
                    emit.emit(kind);
                }
            }
        }
        // A self-nudge is a quiet continuation; a command never reaches the model.
        Turn::Nudge(_) | Turn::Command(_) => {}
    }
}

/// Reduce a finished turn's outcome to the `(tag, text)` digest a peer's
/// result carries — the one place the reduction happens.  Only a deliberate
/// `reply` carries text up: its payload is clipped to the reply cap (an empty
/// payload settles `Empty`).  A tool-call-free prose finish is **not**
/// harvested — there is no scrape — so it, like a genuinely empty turn, settles
/// `Empty`; the child returns through `reply` or returns nothing.  Every other
/// shape carries only its tag.
fn agent_digest(r: &Result<TurnOutcome, ProviderError>) -> (AgentOutcome, String) {
    match r {
        Ok(TurnOutcome::Replied(payload)) if payload.is_empty() => {
            (AgentOutcome::Empty, String::new())
        }
        Ok(TurnOutcome::Replied(payload)) => {
            (AgentOutcome::Complete, clip(payload, AGENT_REPLY_CAP))
        }
        Ok(TurnOutcome::Complete(_)) | Ok(TurnOutcome::Empty) => {
            (AgentOutcome::Empty, String::new())
        }
        Ok(TurnOutcome::Stopped { reason }) => (AgentOutcome::Stopped(reason.clone()), String::new()),
        Ok(TurnOutcome::Cancelled) => (AgentOutcome::Cancelled, String::new()),
        Ok(TurnOutcome::Capped) => (
            AgentOutcome::Stopped("step cap reached".into()),
            String::new(),
        ),
        Err(e) => (AgentOutcome::Failed(e.to_string()), String::new()),
    }
}

/// The message of a recovered panic payload, for either string shape.
fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    //! Panic-recovery integrity (A4): a worker panic mid-tool-eval must
    //! preserve the bindings completed tool calls left behind and leave the
    //! dynamic context clean.  Driven through the scripted provider and the
    //! shared `drive` loop — the real path: `drive` catches the unwind,
    //! ral_core's own frame guard self-heals the IO frame, and `drive`
    //! rebuilds the live context from the durable snapshot.

    use super::*;
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
        session.durable = session.shell.mobile_snapshot();
        let baseline_grant_depth = session.shell.grant_depth();

        // 1st call binds `a4_x` (completes); 2nd call panics mid-eval.  Both
        // round-trips happen inside one `apply`; `drive` catches the unwind.
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let a4_x = 7")]))
                .then(Reply::tool_calls(vec![ral_call("c2", "a4-panic-now")])),
        );
        session.seed("compute then crash".into());
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let _ = session.drive(ProviderHandle::new(provider), &mut NoControl, &emit);

        // The completed call's binding survives the panic.
        assert!(
            session.shell.scope_lookup("a4_x").is_some(),
            "a binding from a completed tool call must survive a later call's panic"
        );
        // The dynamic context is rolled back to the clean boundary: no
        // leaked grant frame from the panicking call's `with_capabilities`.
        assert_eq!(
            session.shell.grant_depth(),
            baseline_grant_depth,
            "the panicking call's grant frame must not leak into the next turn"
        );
        // The drive loop handed the session back ready for a fresh prompt.
        assert!(
            session.is_ready(),
            "drive must leave the session ReadyForUser even after a worker panic"
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

    /// A forked child inherits the parent's installed builtin surface, not
    /// just the core set a bare `Shell::new` seeds, and is denied the spawn
    /// family (so the tree stays one level deep).
    #[test]
    fn fork_inherits_host_builtins_and_cannot_spawn() {
        let dir = tmp("fork-builtins");
        let session = Session::for_test(&dir, "system").unwrap();
        assert!(
            session.shell.lookup_builtin("window-hash").is_some(),
            "the parent boot shell must carry the exarch host builtins"
        );
        let child = session.fork().expect("fork child session");
        for name in ["window-hash", "grep-files", "edit", "explore-dir", "line-hash"] {
            assert!(
                child.shell.lookup_builtin(name).is_some(),
                "the forked child must inherit the host builtin `{name}`"
            );
        }
        // The child's tool set withholds the spawn family, so a peer cannot
        // spawn its own children and the tree stays one level deep.
        let agent = crate::tools::find("agent").expect("agent tool registered");
        assert!(
            !child.tools.allows(agent),
            "a peer must not be permitted the `agent` spawn tool"
        );
    }

    /// A `reply` tool call carrying `result`.
    fn reply_call(id: &str, result: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: id.into(),
            fn_name: "reply".into(),
            fn_arguments: serde_json::json!({ "result": result }),
            thought_signatures: None,
        }
    }

    /// Drive a forked peer to quiescence through `provider`, returning the
    /// `(outcome, text)` digest its parent's spawn site would deliver.
    fn drive_peer(child: &mut Session, provider: Arc<Provider>) -> (AgentOutcome, String) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, child.id);
        child.drive(ProviderHandle::new(provider), &mut NoControl, &emit)
    }

    /// A sub-agent returns through `reply`: its markdown payload is the value
    /// the parent receives (raw, newlines intact), it settles `Complete`, and
    /// the run hard-terminates leaving the session `ReadyForUser`.
    #[test]
    fn sub_agent_returns_through_reply() {
        let dir = tmp("reply-terminal");
        let parent = Session::for_test(&dir, "system").unwrap();
        let mut child = parent.fork().expect("fork child");
        child.seed("write a report".into());
        let provider = scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![reply_call(
                "r1",
                serde_json::json!("# Report\nline one\nline two"),
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
            "a replied turn must leave the session ReadyForUser"
        );
    }

    /// A structured `reply` argument (object/array) is pretty-printed, so the
    /// shape stays legible in what the parent receives.
    #[test]
    fn sub_agent_reply_renders_a_structured_payload() {
        let dir = tmp("reply-structured");
        let parent = Session::for_test(&dir, "system").unwrap();
        let mut child = parent.fork().expect("fork child");
        child.seed("list the files".into());
        let provider = scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![reply_call(
                "r1",
                serde_json::json!({ "files": ["a.rs", "b.rs"] }),
            )])),
        );
        let (outcome, text) = drive_peer(&mut child, provider);
        assert!(matches!(outcome, AgentOutcome::Complete));
        assert!(
            text.contains("\"files\""),
            "a structured reply is pretty-printed: {text}"
        );
    }

    /// A sub-agent that finishes without calling `reply` is reminded once, and
    /// if it still does not reply its run settles `Empty` — the final prose is
    /// **not** harvested (no scrape).
    #[test]
    fn sub_agent_without_reply_is_nudged_then_returns_empty() {
        let dir = tmp("reply-missing");
        let parent = Session::for_test(&dir, "system").unwrap();
        let mut child = parent.fork().expect("fork child");
        child.seed("do the thing".into());
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::text("here is prose, but no reply"))
                .then(Reply::text("still prose, still no reply")),
        );
        let (outcome, text) = drive_peer(&mut child, provider);
        assert!(
            matches!(outcome, AgentOutcome::Empty),
            "an un-replied finish settles Empty, got {outcome:?}"
        );
        assert!(text.is_empty(), "the final prose must not be scraped: {text:?}");
        assert!(child.is_ready());
    }

    /// `reply` is the peer's way of returning and is withheld from the root,
    /// the mirror of how the spawn family is withheld from a peer.
    #[test]
    fn reply_is_offered_to_a_peer_and_withheld_from_the_root() {
        let dir = tmp("reply-gating");
        let root = Session::for_test(&dir, "system").unwrap();
        let reply = crate::tools::find("reply").expect("reply tool registered");
        assert!(
            !root.tools.allows(reply),
            "the root never returns, so it must not be offered `reply`"
        );
        let child = root.fork().expect("fork child");
        assert!(
            child.tools.allows(reply),
            "a peer returns through `reply`, so it must hold it"
        );
    }
}
