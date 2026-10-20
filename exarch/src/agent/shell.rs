//! One `ral` call — [`Avatar::ral`] — and the [`SharedCell`]s a desk handler
//! writes the agent through: a handler answers mid-dispatch, on the attend
//! thread's own stack inside [`shell_eval::run_shell`], so it can never take
//! `&mut Avatar` and reaches them through the [`desk::HostServices`] capture
//! instead.

use crate::agent::Avatar;
use crate::agent::EngineLost;
use crate::agent::desk;
use crate::agent::digest::render;
use crate::bus::Emitter;
use crate::record::{AgentLog, EditAuthority, Span};
use crate::record::{AgentState, Forensic};
use crate::shell_eval;
use atomic_refcell::{AtomicRef, AtomicRefCell, AtomicRefMut};
use ral_core::first_order::FOValue;
use ral_core::protocol::Report;
use std::fmt::Write;
use std::io;
use std::sync::Arc;

/// State the attend thread shares with the desk handlers that run on its own
/// stack.  One thread, so a failed borrow is reentrancy, not contention: it
/// panics where a `Mutex` would deadlock.
pub(crate) struct SharedCell<T>(Arc<AtomicRefCell<T>>);

/// The session log, which the spawn spine forks a child's log off.
pub(crate) type LogCell = SharedCell<AgentLog>;

/// The desk's `reply` slot: within one batch the last write wins, and
/// [`Avatar::deliberate`] takes it once the batch drains.
pub(crate) type ReplyCell = SharedCell<Option<FOValue>>;

impl<T> SharedCell<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Arc::new(AtomicRefCell::new(value)))
    }

    pub(crate) fn borrow(&self) -> AtomicRef<'_, T> {
        self.0.try_borrow().unwrap_or_else(|_| Self::contended())
    }

    pub(crate) fn borrow_mut(&self) -> AtomicRefMut<'_, T> {
        self.0
            .try_borrow_mut()
            .unwrap_or_else(|_| Self::contended())
    }

    fn contended() -> ! {
        panic!(
            "{} contended: only the attend thread between calls, or a desk handler while it \
             is parked in run_shell, may borrow it; concurrent access is a scheduling bug, \
             not a wait",
            std::any::type_name::<T>()
        )
    }
}

impl<T> Clone for SharedCell<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Default> Default for SharedCell<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// What one `ral` call leaves the model: the text it reads, and whether the
/// run failed — rejected, raised, nonzero, or lost with its engine.
pub(crate) struct Evaluated {
    pub text: String,
    pub failed: bool,
}

impl Avatar {
    /// Point this session's recorder at the bus `emit` rides, so every fact
    /// the log authors is published live as it lands on disk.  Called at each
    /// entry where a session meets a bus — `attend`, `deliberate`,
    /// [`Self::ral`], `rewind` — because the recorder outlives any one bus:
    /// idempotent, and re-coupling over a dead per-exchange channel is how a
    /// headless session's next exchange comes back on air.
    pub(crate) fn couple(&self, emit: &Emitter) {
        self.log.borrow().record_emitter().attach(emit.fleet_sink());
    }

    /// This session's recorder, for whoever authors a display commit — the
    /// chopper, the surface buffer, a tool-call row.
    pub(crate) fn recorder(&self) -> crate::record::Emitter {
        self.log.borrow().record_emitter()
    }

    /// Evict on an authority no act row speaks for — a forced
    /// eviction — so it draws its own row.
    pub(crate) fn evict_unbidden(&self, span: Span, by: EditAuthority) -> Result<(), String> {
        self.log.borrow_mut().evict(span, None, by)
    }

    /// A log that cannot take the error is the one failure that cannot go
    /// through the log, so the line rides a fault transient instead — never
    /// lost silently.
    pub(crate) fn note_error(&self, msg: &str) {
        let recorder = self.recorder();
        if let Err(error) = recorder.emit(Forensic::Error {
            text: msg.to_string(),
        }) {
            recorder.report_fault(&io::Error::new(error.kind(), format!("{msg} ({error})")));
        }
    }

    /// An operational note: recorded as a [`crate::record::Forensic::SystemNote`], with no
    /// model-view twin, since the model never saw it.
    pub(crate) fn note(&self, text: String) {
        self.recorder()
            .emit_or_report(crate::record::Forensic::SystemNote { text });
    }

    /// Everything a desk handler may read off `&Avatar`, since the reentrancy
    /// law bars it from reaching back through `&mut Avatar`/`&mut Shell`.
    /// Built fresh at each [`Self::ral`] install, so no capture goes stale.
    pub(crate) fn host_services(&self, emit: &Emitter) -> desk::HostServices {
        desk::HostServices {
            fleet: self.fleet.clone(),
            kind: self.seat.kind(),
            agent: self.agent.clone(),
            emit: emit.clone(),
            reply: self.reply.clone(),
            log: self.log.clone(),
            stamp: self.agent.mailbox.stamp(),
            // Minted here, once per `ral` call: this is the one place a call's
            // whole desk capture is built, so the fragment's extent is the call's.
            acts: desk::ActFragment::default(),
        }
    }

    /// Evaluate one `ral` call.
    pub(crate) fn ral(&self, cmd: &str, timeout_secs: u64, emit: &Emitter) -> Evaluated {
        self.couple(emit);
        // The assistant turn is recorded before its results are built, so this
        // is the id of the turn this result closes — and the call's source name.
        let turn = self.log.borrow().context().current_turn();
        let source = turn.map_or_else(|| "tool call".to_string(), |turn| format!("turn {turn}"));
        let host = Arc::new(desk::RunHost {
            desk: desk::ExarchDesk {
                services: self.host_services(emit),
            },
            apply: shell_eval::surface::SurfaceApplier::new(self.recorder()),
        });
        // Stamped with this session's inbox epoch as read now, so a batch
        // from a worker that settles after a `/clear` is dropped.
        self.seat.install_deferred(shell_eval::deferred_sink(emit));
        self.recorder()
            .transient(crate::record::Transient::State(AgentState::Evaluating));
        let report = shell_eval::run_shell(
            self.seat.transport(),
            &self.agent.caps,
            &source,
            cmd,
            timeout_secs,
            host.clone() as Arc<dyn ral_core::carrier::Host>,
        );
        let lost = |s| EngineLost::running(&s, self.agent.run_dir()).to_string();
        // Only now, with the dispatch returned: the worker probe below is
        // legal at a run boundary and nowhere else.
        let (mut text, failed) = match report {
            Ok(Report::Ran {
                ending, captured, ..
            }) => match self.seat.read(|t| t.workers()) {
                Ok(workers) => {
                    let result = shell_eval::report::tool_result(
                        &ending,
                        captured,
                        &host.apply.births(),
                        host.desk.services.acts.audit().as_deref(),
                        &workers,
                        timeout_secs,
                    );
                    (render(&result), result.exit != 0)
                }
                Err(s) => (lost(s), true),
            },
            Ok(Report::Static { rendered, .. }) => (shell_eval::report::rejection(&rendered), true),
            Ok(Report::Faulted { rendered, .. }) => (
                shell_eval::report::fault(&rendered, host.desk.services.acts.audit().as_deref()),
                true,
            ),
            Err(s) => (lost(s), true),
        };
        if let Some(turn) = turn {
            let _ = write!(text, "\nTURN: {turn}");
        }
        Evaluated { text, failed }
    }
}

#[cfg(test)]
mod tests;
