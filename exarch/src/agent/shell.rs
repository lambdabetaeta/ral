//! One `ral` call — [`Avatar::ral`] — and the two `Arc`-shared cells a
//! desk handler writes the agent through: a handler answers mid-dispatch, on
//! the attend thread's own stack inside [`shell_eval::run_shell`], so it can
//! never take `&mut Avatar` and reaches [`ReplyCell`] and [`LogCell`] through
//! the [`desk::HostServices`] capture instead.  One thread means a failed
//! borrow conflict is reentrancy, not contention, so both cells panic where a
//! `Mutex` would deadlock.

use crate::agent::Avatar;
use crate::agent::desk;
use crate::agent::digest::{OPAQUE_CAP, clip, render};
use crate::agent::seat::EngineLost;
use crate::bus::Emitter;
use crate::record::AgentState;
use crate::record::{AgentLog, EditAuthority};
use crate::shell_eval;
use atomic_refcell::{AtomicRef, AtomicRefCell, AtomicRefMut};
use ral_core::first_order::FOValue;
use ral_core::protocol::Report;
use ral_core::sync::LockExt;
use std::fmt::Write;
use std::io;
use std::sync::Arc;

/// The desk's `reply` slot: within one batch the last write wins, and
/// [`Avatar::deliberate`] takes it once the batch drains.
#[derive(Clone, Default)]
pub(crate) struct ReplyCell(Arc<AtomicRefCell<Option<FOValue>>>);

impl ReplyCell {
    /// Never waits: the one thread that could hold this borrow is the asker.
    fn borrow_mut(&self) -> AtomicRefMut<'_, Option<FOValue>> {
        self.0.try_borrow_mut().unwrap_or_else(|_| {
            panic!(
                "reply cell contended: a desk handler may only run while the attend thread is \
                 parked in run_shell"
            )
        })
    }

    pub(crate) fn set(&self, value: FOValue) {
        *self.borrow_mut() = Some(value);
    }

    pub(crate) fn take(&self) -> Option<FOValue> {
        self.borrow_mut().take()
    }
}

/// [`Avatar::log`] behind its own cell, so a desk handler can be handed the log
/// off `&Avatar` — the spawn spine forks a child's log through it — instead of
/// reaching back through `&mut Avatar`.
#[derive(Clone)]
pub(crate) struct LogCell(Arc<AtomicRefCell<AgentLog>>);

impl LogCell {
    pub(crate) fn new(log: AgentLog) -> Self {
        Self(Arc::new(AtomicRefCell::new(log)))
    }

    /// The crate's one way onto the log for writing, and it never waits: a
    /// failed borrow means a handler asked for a borrow its own attend thread
    /// already holds.
    pub(crate) fn borrow_mut(&self) -> AtomicRefMut<'_, AgentLog> {
        self.0
            .try_borrow_mut()
            .unwrap_or_else(|_| Self::contended())
    }

    /// The read-only way onto the log; conflicts exactly as [`Self::borrow_mut`].
    pub(crate) fn borrow(&self) -> AtomicRef<'_, AgentLog> {
        self.0.try_borrow().unwrap_or_else(|_| Self::contended())
    }

    fn contended() -> ! {
        panic!(
            "log cell contended: the log may only be borrowed by the attend thread between \
             calls or by a desk handler while the attend thread is parked in run_shell: \
             concurrent access is a scheduling bug, not a wait"
        )
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

    /// Evict on an authority no act row speaks for — the harness's pressure
    /// cut, the user's `/rewind` — so the eviction draws its own row.
    pub(crate) fn evict_unbidden(&self, turns: &[u64], by: EditAuthority) -> Result<(), String> {
        let cut = self.log.borrow_mut().evict(turns, None, by)?;
        self.recorder()
            .emit(crate::record::Display::Evicted { cut, by })
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// `record_error` is durable and published in one call, so there is no
    /// second write to keep in step with it.  A log that cannot take it is
    /// the one failure that cannot go through the log, so the line rides a
    /// fault transient instead — never lost silently.
    pub(crate) fn note_error(&self, msg: &str) {
        let recorded = self.log.borrow_mut().record_error(msg.to_string());
        if let Err(error) = recorded {
            self.recorder()
                .report_fault(&io::Error::new(error.kind(), format!("{msg} ({error})")));
        }
    }

    /// An operational note: recorded as a [`crate::record::Forensic::SystemNote`], with no
    /// model-view twin, since the model never saw it.
    pub(crate) fn note(&self, text: String) {
        let recorder = self.recorder();
        if let Err(error) = recorder.emit(crate::record::Forensic::SystemNote { text }) {
            recorder.report_fault(&error);
        }
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
            branch: None,
            stamp: self.agent.mailbox.stamp(),
            // Minted here, once per `ral` call: this is the one place a call's
            // whole desk capture is built, so the fragment's extent is the call's.
            acts: desk::ActFragment::default(),
            principal: ral_core::host::user(),
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
            apply: desk::SurfaceApplier::new(self.recorder()),
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
            Ok(Report::Static { rendered, .. }) => (clip(&rendered, OPAQUE_CAP), true),
            Err(s) => (lost(s), true),
        };
        if let Some(turn) = turn {
            let _ = write!(text, "\nTURN: {turn}");
        }
        Evaluated { text, failed }
    }

    /// Every pinned slot's summary joined onto one line, for the periodic
    /// nudge reminder; `None` when nothing is pinned.  Rendered through the
    /// rail's own `summary_line`, so the reminder reads as the user sees it.
    pub(super) fn pinned_digest(&self) -> Option<String> {
        let lines: Vec<String> = self
            .agent
            .pins
            .lock_ignore_poison()
            .values()
            .map(|pin| crate::card::summary_line(&pin.card))
            .collect();
        (!lines.is_empty()).then(|| lines.join("; "))
    }
}

#[cfg(test)]
mod tests;
