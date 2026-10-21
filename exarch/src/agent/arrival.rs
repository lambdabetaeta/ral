//! An inbox item entering context: booked against the park verdict, then
//! announced in the chrome its source shows.  The same whether it opens an
//! exchange or arrives mid-exchange as steering.

use crate::agent::Avatar;
use crate::bus::Item;
use crate::record::Display;
use crate::shell_eval::surface;
use ral_core::carrier::Severed;
use ral_core::protocol::probe::WorkerState;
use ral_core::types::DoneEvent;

impl Avatar {
    /// A direct child's result means it is no longer awaited, and
    /// [`Avatar::settle`] delivers before it retires, so a parked parent never
    /// sees "no live child" without the result already queued.  Every item
    /// here already survived the inbox's pop-time fence against a `/clear`, so
    /// the `Surface` arm replays cards unconditionally and then gates model
    /// delivery on whether a `DoneEvent`'s result is still unclaimed.
    ///
    /// # Errors
    /// The engine's severance, when the worker probe cannot be read.
    pub(super) fn arrive(&self, item: &Item) -> Result<bool, Severed> {
        let recorder = self.recorder();
        match item {
            Item::Agent(r) => {
                self.agent.heard(r.id);
                recorder.emit_or_report(Display::SubagentDone {
                    name: r.name.clone(),
                    outcome: r.outcome.settlement(),
                    elapsed_ms: u64::try_from(r.elapsed.as_millis()).unwrap_or(u64::MAX),
                });
                Ok(true)
            }
            // Stamped with and posted to this same session, so the recorder's
            // id already routes its cards to the right scrollback.
            Item::Surface { id, values } => {
                debug_assert_eq!(
                    *id, self.agent.id,
                    "a spawn's surface batch always drains in the session it was stamped with"
                );
                surface::replay(&recorder, values);
                let Some(done) = values.iter().rev().find_map(DoneEvent::from_surface) else {
                    return Ok(true);
                };
                let workers = self.seat.read(|t| t.workers())?;
                Ok(workers
                    .iter()
                    .find(|row| row.id == done.id.0)
                    .is_some_and(|row| {
                        matches!(row.state, WorkerState::Ready(_) | WorkerState::Running)
                    }))
            }
            // A prompt is drawn from its `Protocol` record; a nudge is quiet.
            Item::Human(_) | Item::Wakeup(_) | Item::Message(_) | Item::Nudge { .. } => Ok(true),
        }
    }
}

#[cfg(test)]
mod tests;
