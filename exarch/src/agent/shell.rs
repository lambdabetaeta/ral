//! One shell call borrows the body and returns it before reading the report.

use crate::agent::desk::{Call, ExarchDesk, RunHost};
use crate::agent::digest::render;
use crate::agent::{Agent, EngineLost};
use crate::bus::Emitter;
use crate::record::{AgentState, EditAuthority, Forensic, Span, Transient};
use crate::shell_eval;
use atomic_refcell::AtomicRefCell;
use ral_core::process::Scope;
use ral_core::protocol::Report;
use std::fmt::Write;
use std::sync::Arc;

pub(crate) struct Evaluated {
    pub text: String,
    pub failed: bool,
}

impl Agent {
    pub(crate) fn couple(&self, emit: &Emitter) {
        self.body.log.record_emitter().attach(emit.fleet_sink());
    }

    pub(crate) fn recorder(&self) -> crate::record::Emitter {
        self.body.log.record_emitter()
    }

    pub(crate) fn evict_unbidden(&mut self, span: Span, by: EditAuthority) -> Result<(), String> {
        self.body
            .log
            .evict(span, None, by)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn note_error(&self, message: &str) {
        self.recorder().emit_or_end(Forensic::Error {
            text: message.to_string(),
        });
    }

    pub(crate) fn note(&self, text: String) {
        self.recorder().emit_or_end(Forensic::SystemNote { text });
    }

    pub(crate) fn ral(
        &mut self,
        exchange: &Scope,
        cmd: &str,
        timeout_secs: u64,
        emit: &Emitter,
    ) -> Evaluated {
        self.couple(emit);
        let recorder = self.recorder();
        // Recorded before its results, so this is the turn this result closes.
        let turn = self.body.log.context().current_turn();
        let source = turn.map_or_else(|| "tool call".to_string(), |turn| format!("turn {turn}"));
        let call = Call {
            handle: self.handle.clone(),
            life: self.mailbox.life(),
            fleet: self.fleet.clone(),
            kind: self.engine().kind(),
            emit: emit.clone(),
            acts: AtomicRefCell::new(Vec::new()),
        };
        recorder.transient(Transient::State(AgentState::Evaluating));
        let engine = self.engine.as_ref().expect("a shell call owns its engine");
        let (report, host) = self.body.lend(|loan| {
            let host = Arc::new(RunHost {
                desk: ExarchDesk {
                    loan: loan.clone(),
                    call,
                },
                apply: shell_eval::surface::SurfaceApplier::new(recorder),
            });
            let report = shell_eval::run_shell(
                engine.transport(),
                &source,
                cmd,
                timeout_secs,
                host.clone() as Arc<dyn ral_core::carrier::Host>,
                Some(exchange),
            );
            (report, host)
        });
        // The worker probe is legal only at a run boundary, i.e. now.
        let lost = |cause| EngineLost::running(&cause, self.run_dir()).to_string();
        let (mut text, failed) = match report {
            Ok(Report::Ran {
                ending, captured, ..
            }) => match self.engine().read(|transport| transport.workers()) {
                Ok(workers) => {
                    let result = shell_eval::report::tool_result(
                        &ending,
                        captured,
                        &host.apply.births(),
                        host.desk.call.audit().as_deref(),
                        &workers,
                        timeout_secs,
                    );
                    (render(&result), result.exit != 0)
                }
                Err(cause) => (lost(cause), true),
            },
            Ok(Report::Static { rendered, .. }) => (shell_eval::report::rejection(&rendered), true),
            Ok(Report::Faulted { rendered, .. }) => (
                shell_eval::report::fault(&rendered, host.desk.call.audit().as_deref()),
                true,
            ),
            Err(cause) => (lost(cause), true),
        };
        if let Some(turn) = turn {
            let _ = write!(text, "\nTURN: {turn}");
        }
        Evaluated { text, failed }
    }
}

#[cfg(test)]
mod tests;
