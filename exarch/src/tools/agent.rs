//! Sub-agent tools — fork a child session and launch a sub-agent.
//!
//! Launch-only and always asynchronous: every call forks a child, runs it on a
//! detached thread through the same [`Agent::drive`] loop, and returns a
//! start receipt immediately.  The child's single result is delivered later to
//! the parent's inbox at quiescence.
//!
//! The full prompt — every line — is rendered to the rail before spawn, so the
//! user can see what the child was asked to do.

use super::{INVALID_INPUT, Tool, invalid_input, u64_field};
use crate::agent::Agent;
use crate::agent_registry::{AgentRegistry, MessageError};
use crate::bus::{AgentId, AgentOutcome, AgentResult, Emitter, InboxMsg, Kind, Mailbox};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

pub(super) struct SpawnTool {
    kind: SpawnKind,
}

impl SpawnTool {
    pub(super) fn amnemon() -> Self {
        Self {
            kind: SpawnKind::Amnemon,
        }
    }

    pub(super) fn mnemon() -> Self {
        Self {
            kind: SpawnKind::Mnemon,
        }
    }
}

#[derive(Copy, Clone)]
enum SpawnKind {
    /// Tabula rasa: fresh model context, inherited shell values only.
    Amnemon,
    /// Remembrance: inherited model context, fresh prompt at the end.
    Mnemon,
}

impl SpawnKind {
    fn tool(self) -> &'static str {
        match self {
            Self::Amnemon => "amnemon",
            Self::Mnemon => "mnemon",
        }
    }

    fn desc(self) -> &'static str {
        match self {
            Self::Amnemon => {
                "Spawn an amnemon sub-agent: tabula rasa.  This is launch-only \
and always asynchronous: the call forks a child session and returns immediately \
with a start receipt `{id, title, status, log_dir}`.  The child runs the same \
agent loop off your critical path; its single reply is delivered to you LATER \
as its own marked turn when it finishes.  To fan out independent work, spawn \
several and combine their results when they land; poll live ones with `agents` \
and stop one with `agent_cancel`.  A sub-agent may itself spawn sub-agents, but \
delegation depth is finite: each child is handed one less unit of fuel than its \
spawner holds, and once a descendant's fuel reaches zero this tool disappears \
from its view — fuel bounds how deep a chain can recurse, not how many \
children you may start, so spawning several at once costs nothing extra — \
`agent_cancel` on any node stops its whole live subtree regardless of depth.\n\n\
This is expensive: the child is a fresh model session that re-pays the entire \
system prompt and does not share your conversation — it sees a value-snapshot \
of your shell (your `let` bindings, cwd, env) but none of your reasoning or \
message history.  Its own shell mutations (cd, env, new bindings) do NOT \
propagate back; you receive a string, not its bindings or intermediate \
findings.  File edits it makes to the working tree are real and persist.  \
Because each call costs a whole session boot plus a round-trip to relay the \
result back as text, delegate only a substantial, self-contained unit of work: \
a focused exploration whose intermediate detail you do not want to carry, or a \
task whose execution would otherwise flood your own context.  NEVER delegate a \
single grep/view/read/edit you can run inline — running it yourself is cheaper \
and keeps the result addressable (e.g. the witness `edit` needs).  \
`permissions` bounds the child to at most your own authority."
            }
            Self::Mnemon => {
                "Spawn a mnemon sub-agent: remembrance.  This is launch-only \
and always asynchronous: the call forks a child session and returns immediately \
with a start receipt `{id, title, status, log_dir}`.  The child reuses your \
current provider selection and inherits your model-visible context, with the \
tool call's `prompt` appended as a fresh final user prompt.  That makes it a \
good fit when the child needs the conversation you already built and the \
provider can hit its prompt cache.  Its single reply is delivered to you LATER \
as its own marked turn when it finishes.\n\n\
The shell is still forked: the child sees a value-snapshot of your shell (your \
`let` bindings, cwd, env), but its own shell mutations (cd, env, new bindings) \
do NOT propagate back; you receive a string, not its bindings or intermediate \
findings.  File edits it makes to the working tree are real and persist.  Use \
mnemon when context reuse matters; use `amnemon` when isolation and a blank \
conversation are better.  `permissions` bounds the child to at most your own \
authority; delegation depth is likewise finite — each child is handed one less \
unit of fuel than its spawner holds, so a chain cannot recurse forever (fan-out \
itself is unbounded)."
            }
        }
    }

    fn fork(self, session: &Agent, caps: ral_core::types::Capabilities) -> std::io::Result<Agent> {
        match self {
            Self::Amnemon => session.fork(caps),
            Self::Mnemon => session.fork_remembering(caps),
        }
    }
}

/// Monotonic dispatch counter, used to mint `sub-{N}` fallback titles
/// when the model omits one.  Lives as long as the process — the TUI
/// only needs each child's label to be distinguishable from its
/// siblings on screen.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// Parsed Agent tool input: the child's prompt, a validated tab label, and
/// the mandatory base to which the child's permissions are narrowed.
/// Splitting the parse from dispatch keeps validation testable in isolation.
#[cfg_attr(test, derive(Debug))]
struct Args {
    prompt: String,
    title: String,
    permissions: String,
}

/// True for titles that fit the tab-bar contract — non-empty, ≤24
/// chars, ASCII alphanumeric plus `-` / `_`.  Spaces and punctuation
/// are excluded because the tab bar lays them out token-by-token.
fn valid_title(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Read the JSON object the model emitted into [`Args`], or a reason
/// explaining why the input is malformed.  A missing or syntactically
/// invalid `title` is *not* an error — it falls back to `sub-{N}`.
fn parse_args(input: &Value) -> Result<Args, String> {
    let obj = input
        .as_object()
        .ok_or_else(|| "tool input is not a JSON object".to_string())?;
    let prompt = obj
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required string field `prompt`".to_string())?
        .to_string();
    let title = match obj.get("title").and_then(Value::as_str) {
        Some(t) if valid_title(t) => t.to_string(),
        _ => format!("sub-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed)),
    };
    // `permissions` is mandatory: every spawn states the child's ceiling
    // explicitly.  The base name is validated against the bake-ins at
    // dispatch, where the resolver's diagnostic can be surfaced.
    let permissions = obj
        .get("permissions")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required string field `permissions`".to_string())?
        .to_string();
    Ok(Args {
        prompt,
        title,
        permissions,
    })
}

impl Tool for SpawnTool {
    fn name(&self) -> &'static str {
        self.kind.tool()
    }

    fn desc(&self) -> &'static str {
        self.kind.desc()
    }

    fn gate(&self) -> super::Gate {
        super::Gate::Spawns
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The instruction to give the child agent.  \
            The child runs the same agent loop on its own snapshot of the shell.",
                    },
                    "title": {
                        "type": "string",
                        "description": "A short label (≤24 chars, ASCII letters/digits/`-`/`_`) \
            naming the child's tab in the TUI — e.g. `refactor-output`, `audit-deps`.  \
            Choose a phrase the user will recognise at a glance.  Omitted or invalid \
            titles fall back to `sub-{N}`.",
                    },
                    "permissions": {
                        "type": "string",
                        "enum": ["confined", "minimal", "read-only", "edit-only", "reasonable", "dangerous"],
                        "description": "Narrow agent's permissions: confined (offline, no home \
            reads), minimal (working tree + /tmp + network), read-only (writes only to scratch), \
            edit-only (edits working tree, no build tooling), \
            reasonable (everyday tooling), dangerous (no narrowing).",
                    },
                },
                "required": ["prompt", "title", "permissions"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        dispatch_spawn(self.kind, id, &input, session, emit)
    }
}

/// Fork the trunk's conversation into a new tab.  A branch converses (parks
/// for the human), so it registers without a ceiling and pushes no result
/// upward; `prompt` seeds a first turn, or `None` parks and waits.
pub(crate) fn spawn_branch(
    session: &Agent,
    prompt: Option<&str>,
    emit: &Emitter,
) -> Result<SpawnedChild, String> {
    let title = format!("branch-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed));
    let child = session.branch().map_err(|e| e.to_string())?;
    let spec = AsyncSpawn {
        tool: "/branch",
        title,
        prompt: prompt.map(str::to_string),
        commitment: None,
    };
    spawn_async(
        &session.agents,
        session.id,
        session.mailbox(),
        child,
        spec,
        emit,
    )
}

fn dispatch_spawn(
    kind: SpawnKind,
    id: String,
    input: &Value,
    session: &Agent,
    emit: &Emitter,
) -> SessionToolResult {
    let Args {
        prompt,
        title,
        permissions,
    } = match parse_args(input) {
        Ok(a) => a,
        Err(reason) => return invalid_input(id, kind.tool(), INVALID_INPUT, &reason, emit),
    };
    // The child's authority is the parent's narrowed to the requested base
    // (`parent ⊓ base`), frozen against the child's cwd.  Meet only ever
    // removes authority, so a spawn never escalates.
    let cwd = session.cwd();
    let child_caps =
        match crate::policy::narrow(session.caps(), &permissions, &cwd.to_string_lossy()) {
            Ok(c) => c,
            Err(reason) => return invalid_input(id, kind.tool(), INVALID_INPUT, &reason, emit),
        };
    let child = match kind.fork(session, child_caps) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("could not fork child session: {e}");
            session.note_error(msg.clone(), emit);
            return SessionToolResult { id, content: msg };
        }
    };
    let spec = AsyncSpawn {
        tool: kind.tool(),
        title,
        prompt: Some(prompt),
        commitment: None,
    };
    model_receipt(
        id,
        spawn_async(
            &session.agents,
            session.id,
            session.mailbox(),
            child,
            spec,
            emit,
        ),
    )
}

/// The tool-specific half of an async spawn: everything that varies between
/// `amnemon`/`mnemon` and `commit`/`verify_commitment`, as opposed to the
/// fork-detach-register mechanics every one of them shares ([`spawn_async`]).
pub(crate) struct AsyncSpawn {
    pub tool: &'static str,
    pub title: String,
    /// The child's launch prompt, or `None` to park and wait — a branch, which
    /// converses for the human rather than running a seeded turn.
    pub prompt: Option<String>,
    /// Set only for a `commit`/`verify_commitment` spawn. `None` for every
    /// ordinary spawn, which can never tag a result for the pin register.
    pub commitment: Option<super::commitment::CommitmentIntent>,
}

/// What a spawn hands back: the child's identity as the host and the
/// model each render in their own vocabulary.  Only ever built for a child
/// that is genuinely running — a spawn that never got a worker thread, or
/// whose registration was refused, returns `Err(String)` instead
/// ([`spawn_async`]).
pub(crate) struct SpawnedChild {
    pub id: crate::bus::AgentId,
    pub title: String,
    pub log_dir: String,
}

/// Fork-then-detach: hand an already-forked, already-capped `child` to a
/// worker thread that drives it to completion off the parent's critical
/// path, and return the child's identity immediately.  This is the one
/// launch-only, always-asynchronous shape `amnemon`, `mnemon`, `commit`,
/// and `verify_commitment` share; each caller differs only in how it built
/// `child` and `spec`.
///
/// `spec.commitment`, when `Some`, marks this spawn as a `commit`/
/// `verify_commitment` check: the worker computes — while it still holds the
/// child's raw reply, before that detail is discarded — what the structured
/// reply decides, and tags the settled [`AgentResult`] so the parent applies
/// it to the protected pin register when the result drains
/// ([`Agent::drive`](crate::agent::Agent::drive)).
///
/// If the OS refuses to spawn the worker thread, or the registration itself
/// is refused (this agent's own entry vanished between the tool call
/// starting and the registry lock — an `agent_cancel`/`/clear` racing the
/// spawn), the just-created registry entry (if any) is unwound and the call
/// returns `Err` — every caller's receipt collapses to that plain error
/// instead of a start receipt.
///
/// # Errors
/// Returns `Err(String)` if the registration is refused or the worker
/// thread could not be spawned.
pub(crate) fn spawn_async(
    registry: &AgentRegistry,
    parent: AgentId,
    parent_mailbox: Mailbox,
    mut child: Agent,
    spec: AsyncSpawn,
    emit: &Emitter,
) -> Result<SpawnedChild, String> {
    let AsyncSpawn {
        tool,
        title,
        prompt,
        commitment,
    } = spec;
    // Capture everything off the child before it moves into the worker
    // thread: its identity, log directory, own cancellation token and
    // provider handle, the registry entry's generation, and the parent's
    // mailbox (the child's upward result edge).  The child is registered
    // under *this* agent as its parent, so the subtree cascade reaches it.
    let agent_id = child.id;
    let log_dir = child.log_dir();
    let log_dir_str = log_dir.display().to_string();
    let cancel = child.cancel_token().clone();
    // The child's eval-layer cancel handle, registered so the cascade can
    // unwind a `ral` eval already in flight when a terminate-class cancel
    // lands; its turn-scope cell, registered so a per-tab interrupt can
    // unwind just the in-flight turn without touching that root.
    let eval_root = child.eval_root();
    let turn_scope = child.turn_scope();
    // The child's inbox sender, registered so the frontend can steer or
    // wake this tab.  Cheap-clone, taken off `child` before it moves into
    // the worker thread (alongside the one the streaming `child_emit`
    // carries).
    let child_mailbox = child.mailbox();
    // The child's own provider handle (seeded at `fork` from this agent's
    // current provider), registered so a `/model` on this tab swaps the
    // child's provider alone.
    let child_provider = child.provider_handle();
    // A returning sub-agent holds `reply`; a conversing branch does not.  This
    // one fact gates the reaper ceiling, the tab kind, and the settle epilogue.
    let delivers = child.returns();
    let Some(generation) = registry.register(crate::agent_registry::Registration {
        id: agent_id,
        parent: Some(parent),
        ceiling: delivers, // a returning worker is reaped if abandoned; a branch keeps no ceiling
        title: title.clone(),
        log_dir: log_dir.clone(),
        cancel,
        eval_root: Some(eval_root),
        turn_scope: Some(turn_scope),
        mailbox: child_mailbox,
        provider: child_provider,
    }) else {
        return Err(format!(
            "could not start agent {agent_id}: this session is no longer live"
        ));
    };
    // An owned, `'static` clone for the worker thread; `registry` itself
    // stays free for the unwind path below if the thread never spawns.
    let worker_registry = AgentRegistry::clone(registry);
    // A live tab whenever the bus outlives the turn (the TUI): a real
    // emitter cloned off the session sender, stamped with the child's id
    // and carrying the child's own mailbox.  Off a per-turn bus (headless)
    // the child is muted *on the display* — an emitter whose receiver is
    // already dropped — so it never streams; but either way it carries the
    // child's own `Transcript`, so its operational trace is recorded
    // regardless of whether anyone is watching.  Its model view returns
    // through its forked `AgentLog`, its reply through the inbox.
    let child_emit = if emit.is_session_lived() {
        emit.child(agent_id, child.mailbox(), child.transcript())
    } else {
        emit.muted_child(agent_id, child.transcript())
    };
    let started = Instant::now();
    let born_title = title.clone();
    let born_parent = parent;
    let worker_title = title.clone();
    // Seed the launch turn into the child's own inbox: the only downward
    // edge is this one write.  A branch has no prompt — it parks for the human.
    if let Some(p) = &prompt {
        child.seed(p.clone());
    }
    // The dispatch shows on the rail before the spawn, so the user can see
    // exactly what the child was asked to do.
    emit.emit(Kind::ToolCall {
        tool,
        cmd: prompt.unwrap_or_else(|| "(waiting for you)".to_string()),
        summary: Some(format!("Agent {title} spawned ({tool}).")),
    });
    let worker = thread::Builder::new()
        .name(format!("exarch-agent-{agent_id}"))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            // registered before the first token and ages out after the
            // last; on a muted emitter both are no-ops.  The id routes
            // every event to the child's own tab through the TUI's existing
            // draw path.
            child_emit.emit(Kind::Born {
                log_dir: log_dir.clone(),
                title: born_title,
                parent: born_parent,
                branch: !delivers,
            });
            let (outcome, payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // The child drives on its own provider handle, seeded at
                // `fork` from this agent's current provider, so a later
                // `/model` on either never disturbs the other.
                child.drive(&mut crate::agent::NoControl, &child_emit)
            }))
            .unwrap_or_else(|_| (AgentOutcome::Failed("sub-agent panicked".into()), None));
            child_emit.emit(Kind::Died);
            // A returning sub-agent delivers its one result upward and then
            // self-settles; a branch converses, pushes nothing, and leaves its
            // entry to outlive this worker (dropped by `/clear`/`/close`).
            if delivers {
                // A model parent reads the reply as prose in its context, so
                // the peer edge renders the faithful payload to text here.
                let text = payload
                    .as_ref()
                    .map(crate::agent::render_reply)
                    .unwrap_or_default();
                // A commitment writer/verifier's structured reply decides here,
                // on the worker thread, while the raw payload is still in hand —
                // the parent only ever sees the tag, never the payload itself.
                let commitment_settle = commitment
                    .and_then(|i| super::commitment::commitment_settle(i, &outcome, payload.as_ref()));
                // Deliver, then retire.  The parent's park verdict reads child
                // liveness (the registry) and delivery (its inbox) under two
                // different locks, and pops its queue only after the verdict —
                // so the push must come first: a parent that observes this
                // entry gone is then guaranteed to find the result already
                // queued, and cannot quiesce between the two facts and drop it.
                // Staleness (this worker settling across a `/clear`) is decided
                // at the consuming edge instead: the drive loop's generation
                // admission reads the birth `generation` stamped here.
                let rejected = parent_mailbox.push(InboxMsg::AgentResult(AgentResult {
                    id: agent_id,
                    title: worker_title,
                    outcome,
                    text,
                    log_dir,
                    elapsed: started.elapsed(),
                    generation,
                    commitment_settle,
                }));
                // No synchronous caller to return this to — the spawn's own
                // tool_result already returned the "started" receipt — so the
                // drop is reported through the child's own error vocabulary
                // instead of silently vanishing
                // (`decisions/260705_leases-and-budgets`).
                if let Err(reject) = rejected {
                    child_emit.emit(Kind::Error(format!(
                        "the parent's inbox rejected this result: {reject}"
                    )));
                }
                worker_registry.settle(agent_id, generation);
            }
        });
    match worker {
        Ok(_) => Ok(SpawnedChild {
            id: agent_id,
            title,
            log_dir: log_dir_str,
        }),
        Err(e) => {
            // The registration above ran before the OS thread ever existed —
            // register-after-spawn would race a conversing child's own first
            // `park_mode` read of `is_live(self.id)` against the parent's
            // post-spawn `register` call, so the entry must be unwound here
            // instead of never having been created.  No worker will ever call
            // `settle`, so this is the one place that can: the registry ends
            // up exactly as if the spawn had never been attempted.
            registry.settle(agent_id, generation);
            let msg = format!("could not spawn a worker thread for agent {agent_id}: {e}");
            emit.emit(Kind::Error(msg.clone()));
            Err(msg)
        }
    }
}

/// Render a spawn receipt for a model caller — the JSON tool result the
/// spawn tools answer with.
pub(super) fn model_receipt(id: String, spawned: Result<SpawnedChild, String>) -> SessionToolResult {
    let child = match spawned {
        Ok(child) => child,
        Err(reason) => return SessionToolResult { id, content: reason },
    };
    let receipt = json!({
        "id": child.id,
        "title": child.title,
        "status": "started",
        "log_dir": child.log_dir,
        "note": "If you have nothing to do before the agent returns, please yield to the user.",
    });
    SessionToolResult {
        id,
        content: receipt.to_string(),
    }
}

/// `agents` — list the live async agents this session started.
pub(super) struct AgentsTool;

impl Tool for AgentsTool {
    fn name(&self) -> &'static str {
        "agents"
    }

    fn desc(&self) -> &'static str {
        "List the async agents you started this session that are still \
running — by id, with title, elapsed seconds, and log directory.  Use it to \
recover ids after a context compaction, then `agent_cancel` to stop a \
straggler.  Settled agents are not listed: their replies arrive on their own \
as marked turns."
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {},
                "required": [],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        _input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        _emit: &Emitter,
    ) -> SessionToolResult {
        // A silent tool: an argument-less recovery poll the orchestrator runs
        // to recover ids after a compaction, not a user-facing action.  It
        // emits no `ToolCall`/`ToolResult`, so it leaves no trace on the rail
        // (or the headless frontend, or `transcript.jsonl`) — the model still
        // receives the listing through the returned result below.
        let live = session.agents.list(session.id);
        let content = if live.is_empty() {
            "no live async agents".to_string()
        } else {
            live.iter()
                .map(|a| {
                    format!(
                        "{}  {}  {}s  {}",
                        a.id,
                        a.title,
                        a.elapsed.as_secs(),
                        a.log_dir.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        SessionToolResult { id, content }
    }
}

/// An agent's display label: its title when known, else its numeric id.
fn label(title: Option<&str>, id: u64) -> String {
    title.map_or_else(|| id.to_string(), str::to_string)
}

/// `message` — send a marked model-visible note to another live agent.
pub(super) struct MessageTool;

impl Tool for MessageTool {
    fn name(&self) -> &'static str {
        "message"
    }

    fn desc(&self) -> &'static str {
        "Send a short message to a live agent *you started* by id.  The \
recipient sees it as a marked turn, not as human input, at its next tool \
boundary.  Use ids from `agents`, spawn receipts, or ids you deliberately \
included in a child's prompt.  Only a descendant of yours may receive a \
message — never a sibling, an ancestor, or yourself.  This is for \
coordination only: it does not return the recipient's answer; use `reply` \
for a returning agent's final result."
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The id of the live agent to receive the message.",
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to deliver to that agent.",
                    },
                },
                "required": ["id", "message"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let Some(recipient_id) = u64_field(&input, "id") else {
            return invalid_input(
                id,
                "message",
                INVALID_INPUT,
                "missing required integer field `id`",
                emit,
            );
        };
        let Some(message) = input
            .as_object()
            .and_then(|o| o.get("message"))
            .and_then(Value::as_str)
        else {
            return invalid_input(
                id,
                "message",
                INVALID_INPUT,
                "missing required string field `message`",
                emit,
            );
        };
        let who = label(session.agents.title_for(recipient_id).as_deref(), recipient_id);
        emit.emit(Kind::ToolCall {
            tool: "message",
            cmd: message.to_string(),
            summary: Some(format!("Messaged agent {who}.")),
        });
        let content = match session
            .agents
            .message(session.id, recipient_id, message.to_string())
        {
            Ok(()) => format!("sent message to agent {who}"),
            Err(MessageError::UnknownRecipient(n)) => {
                format!("no live agent with id {n}; did it finish already?")
            }
            Err(MessageError::UnknownSender(n)) => {
                format!("cannot send from agent {n}: it is no longer live")
            }
            Err(MessageError::NotADescendant(n)) => {
                format!(
                    "agent {n} is not an agent you started; message may only reach a descendant of yours"
                )
            }
            Err(MessageError::RecipientInboxFull(n, reject)) => {
                format!("agent {n} did not receive the message: {reject}")
            }
        };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}

/// `agent_cancel` — stop one live async agent by id.
pub(super) struct AgentCancelTool;

impl Tool for AgentCancelTool {
    fn name(&self) -> &'static str {
        "agent_cancel"
    }

    fn desc(&self) -> &'static str {
        "Cancel a live async agent *you started*, by its id (from `agents`).  \
The worker is asked to stop at its next checkpoint; it then delivers a \
cancelled result.  A no-op if no live agent has that id.  Only a descendant \
of yours may be cancelled — never a sibling, an ancestor, or yourself."
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The id of the live async agent to cancel, as shown by `agents`.",
                    },
                },
                "required": ["id"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let Some(agent_id) = u64_field(&input, "id") else {
            return invalid_input(
                id,
                "agent_cancel",
                INVALID_INPUT,
                "missing required integer field `id`",
                emit,
            );
        };
        let who = label(session.agents.title_for(agent_id).as_deref(), agent_id);
        emit.emit(Kind::ToolCall {
            tool: "agent_cancel",
            cmd: format!("cancelling agent {agent_id}"),
            summary: Some(format!("Agent {who} cancelled.")),
        });
        let content = match session.agents.cancel_scoped(session.id, agent_id) {
            Ok(true) => format!("cancelling agent {who}"),
            Ok(false) => format!("no live agent with id {agent_id}"),
            Err(crate::agent_registry::NotADescendant(n)) => format!(
                "agent {n} is not an agent you started; agent_cancel may only reach a descendant of yours"
            ),
        };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_title_boundaries() {
        assert!(valid_title("a"));
        assert!(valid_title("refactor-output"));
        assert!(valid_title("audit_deps"));
        assert!(valid_title(&"x".repeat(24)));
        assert!(!valid_title(""));
        assert!(!valid_title(&"x".repeat(25)));
        assert!(!valid_title("has space"));
        assert!(!valid_title("non-ascii-é"));
    }

    /// A spawn that never got a worker thread must render as a plain error,
    /// never as a start receipt the model would mistake for a live child.
    #[test]
    fn model_receipt_renders_a_spawn_error_as_plain_content() {
        let result = model_receipt(
            "call-1".to_string(),
            Err("could not spawn a worker thread for agent 7: oom".to_string()),
        );
        assert_eq!(
            result.content, "could not spawn a worker thread for agent 7: oom",
            "the error text is the whole tool result, not wrapped in a receipt"
        );
        assert!(
            !result.content.contains("\"status\""),
            "a failed spawn never renders the started-receipt JSON"
        );
    }

    #[test]
    fn model_receipt_renders_the_start_receipt_when_spawn_succeeded() {
        let child = SpawnedChild {
            id: 7,
            title: "sub-1".to_string(),
            log_dir: "/tmp/sub-1".to_string(),
        };
        let result = model_receipt("call-1".to_string(), Ok(child));
        assert!(result.content.contains("\"status\":\"started\""));
        assert!(result.content.contains("\"id\":7"));
    }
}
