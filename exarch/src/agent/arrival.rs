//! An inbox item entering context: booked against the park verdict, then
//! announced in the chrome its source shows.  The same whether it opens an
//! exchange or arrives mid-exchange as steering.

use crate::agent::Avatar;
use crate::bus::Item;
use crate::record::Display;
use crate::shell_eval::surface;

impl Avatar {
    /// `opens` is the turn a prompt opens ([`crate::record::AgentLog::opens`]),
    /// `None` for steering.
    ///
    /// A direct child's result means it is no longer awaited, and
    /// [`Avatar::settle`] delivers before it retires, so a parked parent never
    /// sees "no live child" without the result already queued.  Every item
    /// here already survived the inbox's pop-time fence against a `/clear`, so
    /// the `Surface` arm is a routing tripwire, not an admission.
    pub(super) fn arrive(&self, item: &Item, opens: Option<u64>) {
        let recorder = self.recorder();
        match item {
            Item::Human(_) | Item::Wakeup(_) | Item::Message(_) => {
                recorder.emit_or_report(Display::Prompt {
                    text: item.text(),
                    turn: opens,
                });
            }
            Item::Agent(r) => {
                self.agent.heard(r.id);
                recorder.emit_or_report(Display::SubagentDone {
                    name: r.name.clone(),
                    outcome: r.outcome.settlement(),
                    elapsed_ms: u64::try_from(r.elapsed.as_millis()).unwrap_or(u64::MAX),
                });
            }
            // Stamped with and posted to this same session, so the recorder's
            // id already routes its cards to the right scrollback.
            Item::Surface { id, values } => {
                debug_assert_eq!(
                    *id, self.agent.id,
                    "a spawn's surface batch always drains in the session it was stamped with"
                );
                surface::replay(&recorder, values);
            }
            // An internal continuation: quiet.
            Item::Nudge { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests;
