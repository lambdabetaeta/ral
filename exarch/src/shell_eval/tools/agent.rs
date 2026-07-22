//! Sub-agent spawn machinery — the fork-detach-register spine shared by
//! `/branch` and the desk's `agent-start` arm.
//!
//! Launch-only and always asynchronous: every call forks a child, runs it on
//! a detached thread through the same [`Agent::drive`] loop, and returns a
//! start receipt immediately.  The child's single result is delivered later
//! to the parent's inbox at quiescence.
//!
//! The full prompt — every line — is rendered to the rail before spawn, so
//! the user can see what the child was asked to do.

use crate::agent::Agent;
use crate::bus::{AgentId, AgentOutcome, AgentResult, Emitter, InboxMsg, Kind, Mailbox};
use crate::fleet::registry::{AGENT_LEASE_IDLE, AgentRegistry, RegisterError, Registration};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

/// Monotonic dispatch counter, used to mint `branch-{N}` labels for a
/// `/branch` spawn.  Lives as long as the process — the TUI only needs each
/// child's label to be distinguishable from its siblings on screen.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// Fork the trunk's conversation into a new tab.  A branch converses (parks
/// for the human), so it registers without a ceiling and pushes no result
/// upward; `prompt` seeds a first turn, or `None` parks and waits.
pub(crate) fn spawn_branch(
    session: &Agent,
    prompt: Option<&str>,
    emit: &Emitter,
) -> Result<SpawnedChild, String> {
    let name = format!("branch-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed));
    let child = session.branch().map_err(|e| e.to_string())?;
    let spec = AsyncSpawn {
        verb: "/branch",
        name,
        prompt: prompt.map(str::to_string),
        harness: false,
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

/// The tool-specific half of an async spawn: everything that varies between
/// the `agent` builtin's two `type`s (`amnemon`/`mnemon`) and `/branch`, as
/// opposed to the fork-detach-register mechanics every one of them shares
/// ([`spawn_async`]).
pub(crate) struct AsyncSpawn {
    /// The acting name the rail shows: the bare imperative on the harness
    /// path (`spawn`), the slash command on `/branch`.
    pub verb: &'static str,
    pub name: String,
    /// The child's launch prompt, or `None` to park and wait — a branch, which
    /// converses for the human rather than running a seeded turn.
    pub prompt: Option<String>,
    /// Whether this spawn was launched by a desk harness verb (`amnemon`/
    /// `mnemon` via `agent-start`) rather than `/branch`, a human slash
    /// command with no desk verb behind it at all. Decides which rail chrome
    /// the spawn line wears: a harness verb never crossed the provider
    /// boundary, so it renders as [`Kind::HarnessCall`]; `/branch` renders as
    /// an ordinary [`Kind::ToolCall`].
    pub harness: bool,
}

/// What a spawn hands back: the child's identity as the host and the
/// model each render in their own vocabulary.  Only ever built for a child
/// that is genuinely running — a spawn that never got a worker thread, or
/// whose registration was refused, returns `Err(String)` instead
/// ([`spawn_async`]).
pub(crate) struct SpawnedChild {
    pub id: crate::bus::AgentId,
    pub name: String,
    pub log_dir: String,
}

/// Fork-then-detach: hand an already-forked, already-capped `child` to a
/// worker thread that drives it to completion off the parent's critical
/// path, and return the child's identity immediately.  This is the one
/// launch-only, always-asynchronous shape every `agent` spawn and `/branch`
/// share; each caller differs only in how it built `child` and `spec`.
///
/// The registration itself can refuse for two reasons, closed by
/// [`AgentRegistry::register`] under its own lock: this agent's own entry
/// vanished between the tool call starting and the registry lock (an
/// `agent-cancel`/`/clear` racing the spawn), or `name` is already borne by
/// another live agent (two same-name spawns racing in one turn). Either way
/// there is no registry entry to unwind — the registration never happened —
/// so the call simply returns `Err`. If instead the OS refuses to spawn the
/// worker thread *after* a successful registration, that entry is unwound
/// through [`AgentRegistry::settle`] before returning `Err`. Every caller's
/// receipt collapses to that plain error instead of a start receipt.
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
        verb,
        name,
        prompt,
        harness,
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
    // The child's eval-layer cancel reach, registered so the cascade can
    // unwind a `ral` eval already in flight when a terminate-class cancel
    // lands, and a per-tab interrupt can unwind just the in-flight turn
    // without touching the root a later turn would inherit.
    let reach = child.seat.eval_reach();
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
    let generation = match registry.register(Registration {
        id: agent_id,
        parent: Some(parent),
        lease: delivers.then_some(AGENT_LEASE_IDLE), // a returning worker is reaped if abandoned; a branch keeps no lease
        name: name.clone(),
        log_dir: log_dir.clone(),
        cancel,
        reach: Some(reach),
        mailbox: child_mailbox,
        provider: child_provider,
    }) {
        Ok(generation) => generation,
        Err(RegisterError::SessionDead) => {
            return Err(format!(
                "could not start agent {agent_id}: this session is no longer live"
            ));
        }
        Err(RegisterError::NameTaken(name)) => {
            return Err(format!(
                "could not start agent '{name}': a live agent already bears this name — pick \
                 another, or wait for it to settle"
            ));
        }
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
    let born_name = name.clone();
    let born_parent = parent;
    let worker_name = name.clone();
    // Seed the launch turn into the child's own inbox: the only downward
    // edge is this one write.  A branch has no prompt — it parks for the human.
    if let Some(p) = &prompt {
        child.seed(p.clone());
    }
    // The dispatch shows on the rail before the spawn, so the user can see
    // exactly what the child was asked to do. A harness verb never crossed
    // the provider boundary and changes the world outside the turn, so it
    // renders as an act — verb, subject, payload; `/branch` is a human slash
    // command, rendered as an ordinary `ToolCall`.
    let cmd = prompt.unwrap_or_else(|| "(waiting for you)".to_string());
    if harness {
        emit.emit(Kind::HarnessCall {
            verb,
            subject: Some(name.clone()),
            payload: cmd,
            failed: false,
        });
    } else {
        emit.emit(Kind::ToolCall {
            tool: verb,
            cmd,
            summary: Some(format!("Agent {name} spawned ({verb}).")),
        });
    }
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
                name: born_name,
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
                    name: worker_name,
                    outcome,
                    text,
                    log_dir,
                    elapsed: started.elapsed(),
                    generation,
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
            name,
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
