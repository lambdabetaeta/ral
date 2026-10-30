//! The session commands, run by the attend thread, which owns the context:
//! a [`Read`] at whichever boundary drains it, a [`Rewrite`] at the exchange
//! boundary alone.  View-only commands never reach here.

use std::ops::ControlFlow;

use crate::agent::Agent;
use crate::bus::{Command, Emitter, Read, Rewrite};
use crate::provider::Pick;
use crate::record::Forensic;

impl Agent {
    pub(super) fn command(&mut self, command: Command, emit: &Emitter) -> ControlFlow<()> {
        match command {
            Command::Read(read) => {
                self.read(&read, emit);
                ControlFlow::Continue(())
            }
            Command::Rewrite(rewrite) => self.rewrite(&rewrite, emit),
            Command::Model(pick) => {
                self.switch_model(pick);
                ControlFlow::Continue(())
            }
        }
    }

    /// Build the provider, record the switch, then run on it and publish it.
    pub(crate) fn switch_model(&mut self, pick: Pick) {
        let bureau = &self.fleet.launch.bureau;
        let Pick { choice, tuning } = pick;
        let model = choice.model.clone();
        let built = match bureau.build(choice, &tuning, self.fleet.launch.max_tokens) {
            Ok(built) => built,
            Err(e) => {
                self.note_error(&format!("could not switch to {model}: {e}"));
                return;
            }
        };
        if self
            .mailbox
            .try_transact(|txn| txn.change_model(built.selected().clone()))
            .is_ok()
        {
            self.body.client = built;
        }
    }

    /// Survey the context or open a session from it; it is left as it was.
    pub(crate) fn read(&mut self, cmd: &Read, emit: &Emitter) {
        match cmd {
            Read::Hatch { memory, name } => {
                match crate::agent::spawn::human_child(self, *memory, name.clone(), emit) {
                    Ok(root) => {
                        self.note(format!(
                            "session {} opened (agent {})",
                            root.0.name,
                            root.id()
                        ));
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
                if self.body.log.context().can_evict() {
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
                self.recorder().emit_or_end(Forensic::SessionClosed);
                self.handle.end();
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests;
