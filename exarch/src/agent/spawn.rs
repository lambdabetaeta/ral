//! Every agent attends on its own, joined thread.

use crate::agent::nursery::{Joined, Teardown};
use crate::agent::{Agent, AgentHandle};
use crate::bus::{Emitter, Input, WORKER_PANIC_PREFIX, panic_msg};
use crate::record::{AgentLog, Born, ChildStatus, Forensic, Memory, Transient, Unavailable};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::thread;

/// A thread that would not start; the agent was torn down without one.
#[derive(Debug)]
pub(crate) struct LaunchError {
    pub(crate) cause: io::Error,
    pub(crate) failures: Vec<String>,
}

/// Publishes `Died`, then posts `Gone` to the parent, when dropped.
struct Terminal {
    emit: Emitter,
    handle: AgentHandle,
    recorder: crate::record::Emitter,
    completion: Option<Vec<String>>,
}
impl Terminal {
    fn complete(&mut self, mut teardown: Teardown) -> Teardown {
        if let Err(failure) = self.recorder.check_available() {
            announce(&failure, &self.emit);
            teardown.failures.push(failure.to_string());
        }
        self.completion = Some(teardown.failures.clone());
        teardown
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.emit.fleet_sink().transient(Transient::Died {
            failures: self.completion.take().unwrap_or_else(|| {
                vec!["agent thread ended without a completed teardown result".into()]
            }),
        });
        if let Some(parent) = self.handle.parent() {
            let _posted = parent.0.address.post(Input::Gone(self.handle.id()));
        }
    }
}

impl Agent {
    /// Attend on a joined thread of its own, `first` queued first. The agent
    /// is handed over only once its thread runs: a failed spawn discards it.
    pub(crate) fn launch(
        self,
        born: Born,
        first: Option<String>,
        emit: &Emitter,
    ) -> Result<Joined<Teardown>, LaunchError> {
        let (handoff, receipt) = sync_channel::<(Self, Emitter)>(1);
        let spawned = thread::Builder::new()
            .name(format!("exarch-agent-{}", self.id()))
            .stack_size(8 * 1024 * 1024)
            .spawn(move || match receipt.recv() {
                Ok((agent, emit)) => live(agent, emit),
                // The launcher unwound before the handoff: nothing attended.
                Err(_) => Teardown {
                    failures: Vec::new(),
                },
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(cause) => {
                return Err(LaunchError {
                    cause,
                    failures: self.discard(),
                });
            }
        };
        let emit = emit.child(self.id(), born);
        if let Some(first) = first {
            let _posted = self.handle.steer(first);
        }
        handoff
            .send((self, emit))
            .expect("a spawned thread awaits its agent");
        Ok(Joined::new(thread))
    }

    /// Tear down an agent no thread attended: its failures, a failed last
    /// record among them.
    pub(crate) fn discard(mut self) -> Vec<String> {
        let recorder = self.recorder();
        let mut failures = self.teardown().failures;
        drop(self);
        failures.extend(recorder.check_available().err().map(|f| f.to_string()));
        failures
    }
}

/// A launched agent's thread: its life, then `Died` and `Gone`, also on unwind.
fn live(agent: Agent, emit: Emitter) -> Teardown {
    let mut terminal = Terminal {
        handle: agent.handle(),
        recorder: agent.recorder(),
        emit,
        completion: None,
    };
    let teardown =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| agent.run(&terminal.emit)))
            .unwrap_or_else(|payload| {
                let failure = format!("{WORKER_PANIC_PREFIX}{}", panic_msg(&payload));
                terminal.recorder.emit_or_end(Forensic::Error {
                    text: failure.clone(),
                });
                terminal.handle.report(ChildStatus::Failed(failure.clone()));
                Teardown {
                    failures: vec![failure],
                }
            });
    terminal.complete(teardown)
}

/// Announce a recording failure at its owner's end, on `emit`'s fleet
/// channel, which the failed writer cannot block.
pub(crate) fn announce(failure: &Unavailable, emit: &Emitter) {
    emit.fleet_sink().transient(Transient::Fault {
        text: failure.to_string(),
    });
}

/// Mints `root-{N}` names for an unnamed root; a name need only be
/// distinguishable from the live ones on screen, so a process-lifetime counter
/// suffices.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// A new root of `session`'s fleet under `name`, or under a minted `root-{N}`
/// where the human named none: a fresh engine from the launch's recipe, blank
/// for `Amnemon`, or importing `session`'s model-visible context for
/// `Mnemon`.
pub(crate) fn hatch_root(
    session: &Agent,
    memory: Memory,
    name: Option<String>,
) -> Result<Agent, String> {
    let name =
        name.unwrap_or_else(|| format!("root-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed)));
    let reserved = session
        .fleet
        .registry
        .name(&name)
        .map_err(|error| error.to_string())?;
    let engine = session
        .fleet
        .launch
        .engine()
        .map_err(|error| error.to_string())?;
    let inherited = (memory == Memory::Mnemon).then(|| session.body.log.inherited_context());
    Agent::hatch(
        &session.fleet,
        reserved,
        engine,
        session.body.state.provider.clone(),
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
    session: &mut Agent,
    memory: Memory,
    name: Option<String>,
    emit: &Emitter,
) -> Result<crate::agent::AgentHandle, String> {
    let root = hatch_root(session, memory, name)?;
    let name = root.handle.0.name.clone();
    let tool = match memory {
        Memory::Amnemon => "/new",
        Memory::Mnemon => "/branch",
    };
    session
        .recorder()
        .emit_or_end(crate::record::Display::ToolCall {
            tool: tool.to_string(),
            cmd: "(waiting for you)".to_string(),
            summary: Some(format!("Agent {name} spawned ({tool}).")),
        });
    let handle = root.handle();
    let born = Born {
        log_dir: root.log_dir().clone(),
        name,
        parent: None,
        resumed: None,
        reader: root.recorder().reader(),
    };
    let thread = match root.launch(born, None, emit) {
        Ok(thread) => thread,
        Err(error) => {
            session.body.state.failures.extend(error.failures);
            return Err(format!(
                "could not spawn a worker thread for agent {}: {}",
                handle.id(),
                error.cause
            ));
        }
    };
    session.fleet.adopt_root(thread);
    Ok(handle)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
