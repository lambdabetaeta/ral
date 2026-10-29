//! Admission owns the meaning of inputs, including quiet and stale ones.

use crate::agent::Agent;
use crate::bus::{Input, notice, surface_notice};
use crate::record::{Display, Voice};
use crate::shell_eval::surface;
use ral_core::protocol::probe::WorkerState;
use ral_core::shell::runtime::DoneEvent;

pub(super) struct Prompt {
    pub(super) text: String,
    pub(super) voice: Voice,
    pub(super) continues: Option<u64>,
}

impl Agent {
    pub(super) fn admit(&mut self, input: Input) -> Option<Prompt> {
        let (text, voice) = match input {
            Input::Human(text) => {
                self.body.state.owed |= self.handle.0.role.returns();
                (text, Voice::Human)
            }
            Input::Message(message) => {
                self.body.state.owed |= message.from_parent;
                (message.render(), Voice::Peer)
            }
            Input::Child {
                id,
                name,
                status,
                elapsed,
            } => {
                if self.body.state.nursery.is_joined(id) {
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
            Input::Gone(id) => {
                let failures = self.body.state.nursery.join(id);
                self.body.state.failures.extend(failures);
                return None;
            }
            Input::Wakeup { epoch, text, .. } => {
                if epoch != self.body.state.schedules.epoch() {
                    return None;
                }
                (text, Voice::Wakeup)
            }
            Input::Surface { generation, values } => {
                if generation != self.generation {
                    return None;
                }
                surface::replay(&self.recorder(), &values);
                if let Some(done) = values.iter().rev().find_map(DoneEvent::from_surface) {
                    let workers = self.engine().read(|transport| transport.workers()).ok()?;
                    if !workers.iter().any(|row| {
                        row.id == done.id.0
                            && matches!(row.state, WorkerState::Ready(_) | WorkerState::Running)
                    }) {
                        return None;
                    }
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
