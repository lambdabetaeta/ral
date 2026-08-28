//! The fork-detach spine behind `/branch` and the desk's `` agents `start ``.
//! Every spawn is launch-only: the child runs the same [`Avatar::attend`] loop
//! on a detached thread; a reply notice reaches the parent's inbox when the
//! child replies, a non-reply end when it quiesces.

use crate::agent::Avatar;
use crate::bus::{AgentOutcome, Emitter};
use crate::record::Transient;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

/// Mints `branch-{N}` labels for an unnamed branch; a label need only be
/// distinguishable from the siblings on screen, so a process-lifetime counter
/// suffices.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// Fork the trunk's conversation into a new tab under `name`, or under a minted
/// `branch-{N}` where the human named none.  A branch converses: it roots its
/// own tree, pushes no result upward, and parks for the human rather than
/// seeding a first exchange — the fork *is* the whole of the command, and what
/// to say next is said to the new tab.
pub(crate) fn spawn_branch(
    session: &Avatar,
    name: Option<&str>,
    emit: &Emitter,
) -> Result<SpawnedChild, String> {
    let name = name.map_or_else(
        || format!("branch-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed)),
        str::to_string,
    );
    let child = session.branch(name.clone()).map_err(|e| e.to_string())?;
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
    spawn_async(child, spec, emit)
}

/// The half of an async spawn that varies between `` agents `start ``'s two
/// kinds (`amnemon`/`mnemon`) and `/branch`; [`spawn_async`] holds the half
/// they share.
pub(crate) struct AsyncSpawn {
    pub name: String,
    /// `None` parks for the human rather than seeding a first exchange.
    pub prompt: Option<String>,
}

/// A start receipt, built only for a child genuinely running: an unspawnable
/// thread yields `Err(String)` instead.
pub(crate) struct SpawnedChild {
    pub id: crate::bus::AgentId,
    pub name: String,
}

/// Hand an already-forked, already-capped `child` to a worker thread that
/// attends it off the parent's critical path, and return its identity at once.
/// Callers differ only in how they built `child` and `spec`.
///
/// `child` is already in its parent's subtree and holds its name — that is
/// what construction did — so there is no window here in which an unenrolled
/// child could be attended, and nothing to unwind if the thread fails.
///
/// # Errors
/// Returns `Err(String)` if the worker thread could not be spawned.
pub(crate) fn spawn_async(
    mut child: Avatar,
    spec: AsyncSpawn,
    emit: &Emitter,
) -> Result<SpawnedChild, String> {
    let AsyncSpawn { name, prompt } = spec;
    // Everything below is taken off `child` before it moves into the worker.
    let agent_id = child.agent.id;
    let log_dir = child.log_dir();
    let spawner = child.agent.parent_id();
    // Off a bus whose children are muted (headless's per-exchange default)
    // the child's receiver is already dropped, so it streams nowhere; either
    // way its own record seam is recorded through whether or not anyone
    // watches.
    let child_emit = if emit.spawns_live_children() {
        emit.child(agent_id, child.mailbox())
    } else {
        emit.muted_child(agent_id)
    };
    let born_name = name.clone();
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
            // A replied child's parent was notified at deposit time; `settle`
            // skips a second report for it.  A branch reports to nobody and
            // only `/close` on its own tab ever ends it.  Delivery precedes
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
            if let Err(error) =
                spawn_recorder.emit(crate::record::Forensic::Error { text: msg.clone() })
            {
                spawn_recorder.report_fault(&error);
            }
            Err(msg)
        }
    }
}
