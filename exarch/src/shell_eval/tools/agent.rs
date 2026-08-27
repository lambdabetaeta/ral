//! The fork-detach-register spine behind `/branch` and the desk's
//! `` agents `start ``.  Every spawn is launch-only: the child runs the same
//! [`Agent::attend`] loop on a detached thread; a reply notice reaches the
//! parent's inbox when the child replies, a non-reply end when it quiesces.

use crate::agent::Agent;
use crate::bus::{AgentId, AgentOutcome, AgentResult, Emitter, Mailbox, Post};
use crate::fleet::registry::{AgentRegistry, RegisterError, Registration};
use crate::record::Transient;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

/// Mints `branch-{N}` labels for an unnamed branch; a label need only be
/// distinguishable from the siblings on screen, so a process-lifetime counter
/// suffices.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// Fork the trunk's conversation into a new tab under `name`, or under a minted
/// `branch-{N}` where the human named none.  A branch converses: it registers
/// without a lease, pushes no result upward, and parks for the human rather
/// than seeding a first exchange — the fork *is* the whole of the command, and
/// what to say next is said to the new tab.
pub(crate) fn spawn_branch(
    session: &Agent,
    name: Option<&str>,
    emit: &Emitter,
) -> Result<SpawnedChild, String> {
    let name = name.map_or_else(
        || format!("branch-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed)),
        str::to_string,
    );
    let child = session.branch().map_err(|e| e.to_string())?;
    // The branch row's display commit, authored beside the legacy rail row
    // `spawn_async` emits from the same inputs; a harness spawn's recorded
    // row is the desk's own HarnessCall commit instead.
    if let Err(error) = session.recorder().emit(crate::record::Display::ToolCall {
        tool: "/branch".to_string(),
        cmd: "(waiting for you)".to_string(),
        summary: Some(format!("Agent {name} spawned (/branch).")),
    }) {
        session.recorder().report_fault(&error);
    }
    let spec = AsyncSpawn { name, prompt: None };
    spawn_async(
        &session.agents,
        session.id,
        session.mailbox(),
        child,
        spec,
        emit,
    )
}

/// The half of an async spawn that varies between `` agents `start ``'s two
/// kinds (`amnemon`/`mnemon`) and `/branch`; [`spawn_async`] holds the half
/// they share.
pub(crate) struct AsyncSpawn {
    pub name: String,
    /// `None` parks for the human rather than seeding a first exchange.
    pub prompt: Option<String>,
}

/// A start receipt, built only for a child genuinely running: a refused
/// registration or an unspawnable thread yields `Err(String)` instead.
pub(crate) struct SpawnedChild {
    pub id: crate::bus::AgentId,
    pub name: String,
}

/// Hand an already-forked, already-capped `child` to a worker thread that
/// attends it off the parent's critical path, and return its identity at once.
/// Callers differ only in how they built `child` and `spec`.
///
/// [`AgentRegistry::register`] refuses, under its own lock, when this agent's
/// entry vanished between the tool call and that lock (an `` agents `cancel `` or
/// `/clear` racing the spawn), or when `name` is already borne by a live agent
/// (two same-name spawns racing in one batch); neither leaves an entry to
/// unwind.  A thread that fails to spawn *after* a successful registration
/// does, and is unwound through [`AgentRegistry::settle`] below.
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
    let AsyncSpawn { name, prompt } = spec;
    // Everything below is taken off `child` before it moves into the worker.
    let agent_id = child.id;
    let log_dir = child.log_dir();
    let cancel = child.cancel_token().clone();
    // Registered so a terminate-class cancel can unwind a `ral` eval already in
    // flight, and a per-tab interrupt can unwind just the in-flight exchange
    // without touching the root a later exchange would inherit.
    let reach = child.seat.eval_reach();
    // Registered so the frontend can steer or wake this tab.
    let child_mailbox = child.mailbox();
    // Seeded at `fork` from this agent's current provider and registered apart
    // from it, so a later `/model` on either never disturbs the other.
    let child_provider = child.provider_handle();
    // A returning sub-agent holds `reply`; a conversing branch does not.  This
    // one fact gates the parent edge and the settle epilogue.
    let delivers = child.returns();
    let spawner = delivers.then_some(parent);
    let generation = match registry.register(Registration {
        id: agent_id,
        parent: spawner,
        name: name.clone(),
        log_dir: log_dir.clone(),
        cancel,
        reach,
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
        Err(RegisterError::NameMalformed(why)) => {
            return Err(format!("could not start agent {agent_id}: {why}"));
        }
    };
    // `registry` itself stays free for the unwind path below.
    let worker_registry = AgentRegistry::clone(registry);
    // Off a bus whose children are muted (headless's per-exchange default)
    // the child's receiver is already dropped, so it streams nowhere; either
    // way its own record seam is recorded through whether or not anyone
    // watches.
    let child_emit = if emit.spawns_live_children() {
        emit.child(agent_id, child.mailbox())
    } else {
        emit.muted_child(agent_id)
    };
    let started = Instant::now();
    let born_name = name.clone();
    let worker_name = name.clone();
    // The only downward edge into the child is this one write.
    if let Some(p) = &prompt {
        child.seed(p.clone());
    }
    // The dispatch's own row is authored at each caller with session access —
    // `spawn_branch`'s `Display::ToolCall` beside this call, a harness spawn's
    // at the desk's own commitment arm — so this spine mints no row of its own.
    // Taken before the move below, so a spawn that never runs can still
    // report through it.
    let spawn_recorder = child.recorder();
    let worker = thread::Builder::new()
        .name(format!("exarch-agent-{agent_id}"))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            // Opens the child's tab before its first token, and the stamped id
            // routes every later event to it; a no-op on a muted emitter. The
            // child's own recorder, coupled to this bus ahead of `attend`'s
            // own coupling, so the birth is on air before the first token.
            let recorder = child.recorder();
            child.couple(&child_emit);
            recorder.transient(Transient::Born {
                log_dir: log_dir.clone(),
                name: born_name,
                parent: spawner,
            });
            let (outcome, _payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    child.attend(&mut crate::agent::NoControl, &child_emit)
                }))
                .unwrap_or_else(|_| {
                    if let Err(error) = recorder.emit(crate::record::Forensic::Error {
                        text: "sub-agent panicked".into(),
                    }) {
                        recorder.report_fault(&error);
                    }
                    (AgentOutcome::Failed("sub-agent panicked".into()), None)
                });
            child.recorder().transient(Transient::Died);
            // A branch pushes nothing and leaves its entry to outlive this
            // worker; holding no parent edge, only `/close` on its own tab
            // ever drops it.
            if !delivers {
                return;
            }
            // A replied child's parent was notified at deposit time; any other
            // end is delivered, then retired.  The parent's park verdict reads
            // child liveness (the registry) and delivery (its inbox) under
            // two different locks, and pops its queue only after the verdict,
            // so the push must come first: a parent that observes this entry
            // gone then always finds the result already queued.  Staleness
            // across a `/clear` is decided at the consuming edge, by the
            // attend loop's admission of the birth `generation` stamped here.
            if !matches!(outcome, AgentOutcome::Replied) {
                let result = AgentResult {
                    name: worker_name,
                    outcome,
                    elapsed: started.elapsed(),
                    generation,
                };
                if let Err(reject) = parent_mailbox.push(Post::AgentResult(result)) {
                    let text = format!("the parent's inbox rejected this result: {reject}");
                    if let Err(error) = recorder.emit(crate::record::Forensic::Error { text }) {
                        recorder.report_fault(&error);
                    }
                }
            }
            worker_registry.settle(agent_id);
        });
    match worker {
        Ok(_) => Ok(SpawnedChild { id: agent_id, name }),
        Err(e) => {
            // Registration must precede the thread: registering after it would
            // race a conversing child's first `park_mode` read of `is_live`
            // against this call.  So the entry exists and must be unwound, and
            // with no worker to ever `settle`, this is the one place that can.
            registry.settle(agent_id);
            let msg = format!("could not spawn a worker thread for agent {agent_id}: {e}");
            if let Err(error) =
                spawn_recorder.emit(crate::record::Forensic::Error { text: msg.clone() })
            {
                spawn_recorder.report_fault(&error);
            }
            Err(msg)
        }
    }
}
