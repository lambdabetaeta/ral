//! One shell call's enquiry desk, borrowing the owner's body through a loan.

use crate::agent::engine::EngineKind;
use crate::agent::fleet::Fleet;
use crate::agent::loan::Loan;
use crate::agent::roster::listing;
use crate::agent::{AgentHandle, Body};
use crate::bus::Emitter;
use crate::enquiry::{
    Agents, Context, Hits, Indexed, Material, Pins, Request, Schedules, Survey, Transcript,
};
use crate::shell_eval::surface::SurfaceApplier;
use atomic_refcell::AtomicRefCell;
use ral_core::carrier::Host;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::process::Scope;
use ral_core::protocol::EnquiryError;
use ral_core::shell::runtime::Error;
use std::sync::Arc;

/// The acts a desk verb can commit. One vocabulary for two readers: the rail's
/// `verb` column and the audit prose an unwind owes the model are both derived
/// from it, so a new act cannot reach one and miss the other.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum DeskAct {
    Spawn,
    Stop,
    Dismiss,
    Message,
    Schedule,
    Unschedule,
    Reply,
    ContextEvict,
}

impl DeskAct {
    /// The name the rail draws in its verb column.
    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Stop => "stop",
            Self::Dismiss => "dismiss",
            Self::Message => "message",
            Self::Schedule => "schedule",
            Self::Unschedule => "unschedule",
            Self::Reply => "reply",
            Self::ContextEvict => "evict",
        }
    }

    /// What the act did, past tense, for an audit read after the fact.
    fn done(self, subject: Option<&str>) -> String {
        let named = |what: &str| match subject {
            Some(s) => format!("{what} '{s}'"),
            None => what.to_string(),
        };
        match self {
            Self::Spawn => named("started agent"),
            Self::Stop => named("stopped agent"),
            Self::Dismiss => named("dismissed agent"),
            Self::Message => named("delivered a message to agent"),
            Self::Schedule => named("armed the wakeup"),
            Self::Unschedule => named("removed the wakeup"),
            // The one act with no addressee: a returning agent replies to its
            // parent and to nobody else.
            Self::Reply => "staged your reply".to_string(),
            Self::ContextEvict => named("evicted context"),
        }
    }
}

/// Plain values fixed for one shell call.
pub(crate) struct Call {
    pub(crate) handle: AgentHandle,
    pub(crate) life: Scope,
    pub(crate) fleet: Arc<Fleet>,
    pub(crate) kind: EngineKind,
    pub(crate) emit: Emitter,
    pub(crate) acts: AtomicRefCell<Vec<(DeskAct, Option<String>)>>,
}

impl Call {
    /// Committed acts survive a failed dispatch, even when its bindings do not.
    pub(crate) fn audit(&self) -> Option<String> {
        let acts = self.acts.borrow();
        if acts.is_empty() {
            return None;
        }
        let done = acts
            .iter()
            .map(|(act, subject)| act.done(subject.as_deref()))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "audit: this call had already {done}; that work stands; do not repeat it.\n"
        ))
    }

    /// Every attempt draws a row; only committed acts enter the audit.
    fn commit_act(
        &self,
        body: &Body,
        act: DeskAct,
        subject: Option<&str>,
        payload: String,
        refused: bool,
    ) {
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Display::HarnessCall {
                verb: act.verb().to_string(),
                subject: subject.map(str::to_string),
                payload,
                failed: refused,
            });
        if !refused {
            self.acts
                .borrow_mut()
                .push((act, subject.map(str::to_string)));
        }
    }
}

/// The loan is the only access to owner state; dispatch returns it.
pub(crate) struct ExarchDesk {
    pub(crate) loan: Arc<Loan<Body>>,
    pub(crate) call: Call,
}

fn no_desk() -> Error {
    EnquiryError::no_desk().into()
}

impl ExarchDesk {
    /// Decode one enquiry and answer it. The decode is the vocabulary's, so
    /// an ill-shaped request is refused here in the words its builtin would use.
    ///
    /// # Errors
    /// The decoder's refusal, or the addressed handler's.
    pub(crate) fn handle(&self, req: &FOValue) -> Result<FOValue, Error> {
        drop(self.loan.get().ok_or_else(no_desk)?);
        match Request::decode(req).map_err(Error::new)? {
            Request::Agents(Agents::List) => {
                Ok(listing(&self.call.handle, &self.call.fleet).encode())
            }
            Request::Agents(Agents::Start(start)) => self.launch(start),
            Request::Agents(Agents::Message(message)) => self.message(message),
            Request::Agents(Agents::Stop(name)) => self.agent_stop(&name),
            Request::Agents(Agents::Dismiss(name)) => self.agent_dismiss(&name),
            Request::Agents(Agents::Reply(value)) => self.agent_reply(value),
            Request::Agents(Agents::Read(name)) => self.agent_read(name),
            Request::Schedules(Schedules::List) => {
                self.require_schedule_grant("exarch-schedules `list")?;
                self.schedule_table()
            }
            Request::Schedules(Schedules::Add(add)) => self.schedule(add),
            Request::Schedules(Schedules::Remove(label)) => self.unschedule(&label),
            Request::Pins(Pins::Set(pin)) => self.apply_pin(pin.key, Some(pin.body)),
            Request::Pins(Pins::Clear(key)) => self.apply_pin(key, None),
            Request::Pins(Pins::Read(key)) => self.pin_read(&key),
            Request::Pins(Pins::List) => self.pin_list(),
            Request::Context(Context::Survey) => Ok(Survey::from(self.context_survey()?).encode()),
            Request::Context(Context::Evict(evict)) => self.context_evict(evict),
            Request::Transcript(Transcript::Index) => self.locate_then_read(
                |log| {
                    Ok(Vec::from_iter(
                        log.context()
                            .transcript_index()
                            .into_iter()
                            .map(Indexed::from),
                    ))
                },
                |index| Ok(index.encode()),
            ),
            Request::Transcript(Transcript::Read(read)) => self.locate_then_read(
                |log| log.context().locate_read(&read.turns),
                |read| {
                    read.turns()
                        .map(|turns| Vec::from_iter(turns.into_iter().map(Material::from)).encode())
                },
            ),
            Request::Transcript(Transcript::Grep(grep)) => self.locate_then_read(
                |log| log.context().locate_grep(grep.turns.only()),
                |read| {
                    read.grep(&grep.pattern)
                        .map(|hits| Hits::from(hits).encode())
                },
            ),
        }
    }
}

/// One `ral` call's whole host: the desk that answers its enquiries and
/// the applier that renders its surfaced values, built once per call and
/// shared by `Arc` between the run's dispatch and `shell_eval::run_shell`'s
/// own flush — so the two can never be handed different desks, or different
/// appliers, by mistake.
pub(crate) struct RunHost {
    pub(crate) desk: ExarchDesk,
    pub(crate) apply: SurfaceApplier,
}

impl Host for RunHost {
    fn surface(&self, val: &FOValue) {
        self.apply.surface(val);
    }

    fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
        self.desk.handle(&req).map_err(|e| EnquiryError {
            status: e.code(),
            message: e.message,
        })
    }
}

mod agents;
mod context;
mod pins;
mod schedules;

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
