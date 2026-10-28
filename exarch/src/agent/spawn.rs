//! The detach spine behind every attended root and the desk's `` exarch-agents `start ``.
//! Every spawn is launch-only: the node runs the same [`Agent::attend`] loop
//! on a detached thread; a reply notice reaches the parent's inbox when a
//! child replies, a non-reply end when it quiesces.

use crate::agent::Agent;
use crate::bus::{AgentOutcome, Emitter};
use crate::record::{AgentLog, Born, Memory, Resumed, Transient};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

impl Agent {
    pub(crate) fn settle(mut self, outcome: AgentOutcome) {
        let replied = self.handle.0.mailbox.retire();
        match outcome {
            AgentOutcome::Replied if replied => {}
            AgentOutcome::Replied => self.report(AgentOutcome::Cancelled),
            outcome => self.report(outcome),
        }
        self.report.take();
    }
}

/// Mints `root-{N}` names for an unnamed root; a name need only be
/// distinguishable from the live ones on screen, so a process-lifetime counter
/// suffices.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// A new root of `session`'s fleet under `name`, or under a minted `root-{N}`
/// where the human named none: a fresh engine from the launch's genesis, blank
/// for `Amnemon`, or importing `session`'s model-visible context for
/// `Mnemon`.
pub(crate) fn hatch_root(
    session: &Agent,
    memory: Memory,
    name: Option<String>,
) -> Result<Agent, String> {
    let name =
        name.unwrap_or_else(|| format!("root-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed)));
    let claim = session.fleet.claim(&name).map_err(|e| e.to_string())?;
    let engine = session.fleet.launch.engine().map_err(|e| e.to_string())?;
    let inherited = (memory == Memory::Mnemon).then(|| session.log.borrow().inherited_context());
    Agent::hatch(
        &session.fleet,
        claim,
        engine,
        session.handle.current_provider(),
        |sessions, model, account, system| {
            let id = crate::agent::fresh_id();
            AgentLog::root(sessions, id, &name, model, account, system, inherited)
                .map(|log| (log, ()))
        },
    )
    .map(|(root, ())| root)
    .map_err(|e| e.to_string())
}

/// Open a new root session as a tab ([`hatch_root`]).  A root converses and
/// parks for the human rather than seeding a first exchange: what to say next
/// is said to the new tab.
pub(crate) fn spawn_root(
    session: &Agent,
    memory: Memory,
    name: Option<String>,
    emit: &Emitter,
) -> Result<SpawnedChild, String> {
    let root = hatch_root(session, memory, name)?;
    let name = root.handle.0.name.clone();
    let tool = match memory {
        Memory::Amnemon => "/new",
        Memory::Mnemon => "/branch",
    };
    // The row's display commit, authored beside the rail row `spawn_async`
    // emits from the same inputs; a harness spawn's recorded row is the
    // desk's own HarnessCall commit instead.
    session
        .recorder()
        .emit_or_report(crate::record::Display::ToolCall {
            tool: tool.to_string(),
            cmd: "(waiting for you)".to_string(),
            summary: Some(format!("Agent {name} spawned ({tool}).")),
        });
    let spec = AsyncSpawn {
        name,
        prompt: None,
        resumed: None,
    };
    spawn_async(root, spec, emit)
}

/// The half of an async spawn that varies between `` exarch-agents `start ``,
/// a new root, and a resumed one; [`spawn_async`] holds the half they share.
pub(crate) struct AsyncSpawn {
    pub name: String,
    /// `None` parks for the human rather than seeding a first exchange.
    pub prompt: Option<String>,
    /// What a resumed root picked up, for its tab to replay.
    pub resumed: Option<Resumed>,
}

/// A start receipt, built only for a child genuinely running: an unspawnable
/// thread yields `Err(String)` instead.
pub(crate) struct SpawnedChild {
    pub id: crate::record::AgentId,
    pub name: String,
}

/// Hand an already-built `child` to a worker thread that
/// attends it off the parent's critical path, and return its identity at once.
/// Callers differ only in how they built `child` and `spec`.
///
/// Construction already adopted the child; a failed thread start drops it.
///
/// # Errors
/// Returns `Err(String)` if the worker thread could not be spawned.
pub(crate) fn spawn_async(
    mut child: Agent,
    spec: AsyncSpawn,
    emit: &Emitter,
) -> Result<SpawnedChild, String> {
    let AsyncSpawn {
        name,
        prompt,
        resumed,
    } = spec;
    let agent_id = child.id();
    // Off a bus whose children are muted (headless's per-exchange default)
    // the child's receiver is already dropped, so it streams nowhere; either
    // way its own record seam is recorded through whether or not anyone
    // watches.
    let child_emit = if emit.spawns_live_children() {
        let born = Born {
            log_dir: child.log_dir().clone(),
            name: name.clone(),
            parent: child
                .handle
                .0
                .role
                .parent()
                .map(crate::agent::AgentHandle::id),
            resumed,
            recorder: child.recorder(),
        };
        emit.child(agent_id, born)
    } else {
        emit.muted_child(agent_id)
    };
    // The only downward edge into the child is this one write.
    if let Some(p) = &prompt {
        child.seed(p.clone());
    }
    // The dispatch's own row is authored at each caller with session access —
    // `spawn_root`'s `Display::ToolCall` beside this call, a harness spawn's
    // at the desk's own commitment arm — so this spine mints no row of its own.
    // Taken before the move below, so a spawn that never runs can still
    // report through them, and end the birth already announced.
    let spawn_recorder = child.recorder();
    let mourner = child_emit.fleet_sink();
    let worker = thread::Builder::new()
        .name(format!("exarch-agent-{agent_id}"))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let recorder = child.recorder();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                child.attend(&child_emit)
            }))
            .unwrap_or_else(|_| {
                recorder.emit_or_report(crate::record::Forensic::Error {
                    text: "sub-agent panicked".into(),
                });
                AgentOutcome::Failed("sub-agent panicked".into())
            });
            child.recorder().transient(Transient::Died);
            // A replied child's parent was notified at deposit time; `settle`
            // skips a second report for it.  A root reports to nobody and
            // only `/close` or `/quit` on its own tab ever ends it.  Delivery precedes
            // retirement: a parent that finds this child gone always finds
            // the result already queued.
            child.settle(outcome);
        });
    match worker {
        Ok(_) => Ok(SpawnedChild { id: agent_id, name }),
        Err(e) => {
            // The closure never ran, so `child` fell with it, and that drop is
            // the whole of the unwind.
            let msg = format!("could not spawn a worker thread for agent {agent_id}: {e}");
            spawn_recorder.emit_or_report(crate::record::Forensic::Error { text: msg.clone() });
            mourner.transient(Transient::Died);
            Err(msg)
        }
    }
}
