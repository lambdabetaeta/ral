//! Admission owns the meaning of inputs, including quiet and stale ones.

use crate::agent::Agent;
use crate::bus::{Input, notice, surface_notice};
use crate::record::{Display, Voice};
use crate::shell_eval::surface;
use ral_core::shell::runtime::DoneEvent;

pub(super) struct Prompt {
    pub(super) text: String,
    pub(super) voice: Voice,
    pub(super) continues: Option<u64>,
}

impl Agent {
    pub(super) fn gone(&mut self, id: crate::record::AgentId) {
        let failures = self.body.nursery.join(id);
        self.body.failures.extend(failures);
    }

    pub(super) fn admit(&self, input: Input) -> Option<Prompt> {
        let (text, voice) = match input {
            Input::Human(text) => (text, Voice::Human),
            Input::Task(message) => (message.text, Voice::Peer),
            Input::Message(message) => (message.render(), Voice::Peer),
            Input::Child {
                id,
                name,
                status,
                elapsed,
            } => {
                if !self.body.nursery.holds(id) {
                    return None;
                }
                let settled = status.settlement();
                self.recorder().emit_or_end(Display::SubagentDone {
                    name: name.to_string(),
                    outcome: settled.clone(),
                    elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                });
                (notice(&name, &settled, elapsed), Voice::Harness)
            }
            Input::Wakeup { text, .. } => (text, Voice::Wakeup),
            Input::Surface { generation, values } => {
                if generation != self.generation {
                    return None;
                }
                let done = values.iter().rev().find_map(DoneEvent::from_surface);
                let announced = done.as_ref().is_none_or(|done| {
                    self.engine()
                        .read(ral_core::fact::ProbeSubject::Workers, |transport| {
                            transport.workers()
                        })
                        .is_ok_and(|rows| done.unclaimed_in(&rows))
                });
                let values = if announced {
                    values
                } else {
                    values
                        .into_iter()
                        .filter(|v| DoneEvent::from_surface(v).is_none())
                        .collect()
                };
                surface::replay(&self.recorder(), &values);
                if !announced {
                    return None;
                }
                (surface_notice(&values), Voice::Harness)
            }
        };
        Some(Prompt {
            text,
            voice,
            continues: None,
        })
    }
}

#[cfg(test)]
mod tests;
