//! The session commands, run by the attend thread, which owns the context:
//! a [`Read`] at whichever boundary drains it, a [`Rewrite`] at the exchange
//! boundary alone.  View-only commands never reach here.

use std::ops::ControlFlow;

use crate::agent::Avatar;
use crate::bus::{Emitter, Read, Rewrite};
use crate::record::Transient;

impl Avatar {
    /// Survey or fork the context; it is left as it was.
    pub(crate) fn read(&self, cmd: &Read, emit: &Emitter) {
        match cmd {
            Read::Branch(name) => {
                match crate::agent::spawn::spawn_branch(self, name.as_deref(), emit) {
                    Ok(child) => self.note(format!(
                        "branch {} started (agent {})",
                        child.name, child.id
                    )),
                    Err(e) => self.note_error(&format!("could not start branch: {e}")),
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
                let result = self.clear();
                self.recorder().transient(Transient::Cleared);
                if let Err(error) = result {
                    self.note_error(&format!("clear failed: {error}"));
                }
            }
            Rewrite::Evict => {
                let provider = self.agent.current_provider();
                self.evict(&provider, true);
            }
            Rewrite::Rewind(anchor) => {
                if let Err(error) = self.rewind(*anchor, emit) {
                    self.note_error(&error);
                }
            }
            Rewrite::Quit => return ControlFlow::Break(()),
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests;
