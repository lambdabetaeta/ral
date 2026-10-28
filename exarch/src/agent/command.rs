//! The session commands, run by the attend thread, which owns the context:
//! a [`Read`] at whichever boundary drains it, a [`Rewrite`] at the exchange
//! boundary alone.  View-only commands never reach here.

use std::ops::ControlFlow;

use crate::agent::Agent;
use crate::bus::{Emitter, Read, Rewrite};
use crate::record::Forensic;

impl Agent {
    /// Survey the context or open a session from it; it is left as it was.
    pub(crate) fn read(&self, cmd: &Read, emit: &Emitter) {
        match cmd {
            Read::Hatch { memory, name } => {
                match crate::agent::spawn::spawn_root(self, *memory, name.clone(), emit) {
                    Ok(root) => {
                        self.note(format!("session {} opened (agent {})", root.name, root.id));
                    }
                    Err(e) => self.note_error(&format!("could not open a session: {e}")),
                }
            }
            Read::Context => self.emit_context_survey(),
            Read::Resources => self.emit_resources(&self.recorder()),
        }
    }

    /// Rewrite or end the context; `Break` ends the attend loop.
    pub(crate) fn rewrite(&mut self, cmd: &Rewrite, emit: &Emitter) -> ControlFlow<()> {
        match cmd {
            Rewrite::Clear => {
                if let Err(error) = self.clear() {
                    self.note_error(&format!("clear failed: {error}"));
                }
            }
            Rewrite::Evict => {
                if self.log.borrow().context().can_evict() {
                    self.force_eviction();
                } else {
                    self.note_error("cannot evict while tool results are pending");
                }
            }
            Rewrite::Rewind(anchor) => {
                if let Err(error) = self.rewind(*anchor, emit) {
                    self.note_error(&error);
                }
            }
            Rewrite::Close => {
                self.handle.end();
                self.recorder().emit_or_end(Forensic::SessionClosed);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests;
