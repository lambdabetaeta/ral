//! The exarch enquiry desk: the host half of `shell.enquire(class)`.
//!
//! [`HostServices`] captures plain values from the owner, since reentrancy bars a handler from
//! taking the session lock. [`ExarchDesk`] answers one enquiry against that
//! capture, paired with its [`SurfaceApplier`] in one [`RunHost`], which
//! implements [`ral_core::carrier::Host`] — the object every dispatch
//! rides, so a handler's chrome can never outrun the run's earlier surface
//! output.

use crate::agent::engine::EngineKind;
use crate::agent::fleet::Fleet;
use crate::agent::roster::listing;
use crate::agent::{AgentHandle, LogCell, ReplyCell, SharedCell};
use crate::bus::{Emitter, Mailbox};
use crate::enquiry::{
    Agents, Context, Hits, Indexed, Material, Pins, Request, Schedules, Survey, Transcript,
};
use crate::schedule::ScheduleRegistry;
use crate::shell_eval::PinDigests;
use crate::shell_eval::surface::SurfaceApplier;
use ral_core::carrier::Host;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::protocol::EnquiryError;
use ral_core::shell::runtime::Error;
use std::sync::Arc;

/// The acts a desk verb can commit. One vocabulary for two readers: the rail's
/// `verb` column and the audit prose an unwind owes the model are both derived
/// from it, so a new act cannot reach one and miss the other.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum DeskAct {
    Spawn,
    Cancel,
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
            Self::Cancel => "cancel",
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
            Self::Cancel => named("cancelled agent"),
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

/// What this `ral` call has committed, in the order it landed: one entry per
/// act that landed, minted at [`HostServices::commit_act`] — the one place every
/// acting handler funnels through — and read back once the call has failed.
///
/// A call the wall cut short still started what it started and delivered what
/// it delivered, and no other in-band channel says so: the model's only other
/// signal — the diagnostic — speaks of the failure and not of the acts.
///
/// Only committed acts are recorded. A refused act changed nothing, so it
/// leaves no entry: this fragment answers *what stands*.
#[derive(Clone, Default)]
pub(crate) struct ActFragment(SharedCell<Vec<(DeskAct, Option<String>)>>);

impl ActFragment {
    /// The sentence an unwind owes the model, or `None` when this call committed
    /// nothing at all — where silence is the whole truth.
    pub(crate) fn audit(&self) -> Option<String> {
        let acts = self.0.borrow();
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
}

/// A call's local services, retaining its originating work scope.
pub(crate) struct HostServices {
    /// This run's own agent: parent of what it spawns, root of the descendant
    /// check `` `message ``/`` `cancel `` enforce, and the source every
    /// immutable-config read (`fuel`, `role`, …) is taken from.
    pub handle: AgentHandle,
    pub schedules: ScheduleRegistry,
    pub pins: PinDigests,
    pub fleet: Arc<Fleet>,
    /// How this call's forks reach the desk: `` `start `` chooses its arm on
    /// this fact.
    pub kind: EngineKind,
    pub emit: Emitter,
    /// Where the `reply` handler stages its value, holding no `&mut Agent` to
    /// write it any other way.
    pub reply: ReplyCell,
    /// A `mnemon` spawn forks its inherited context off this.
    pub log: LogCell,
    /// The originating work's envelope; never refreshed at installation.
    pub mailbox: Mailbox,
    /// What this call has committed so far: minted per `ral` call, and read
    /// back by [`crate::shell_eval::report::tool_result`] when a raise discards
    /// the bindings but not the acts.
    pub acts: ActFragment,
}

impl HostServices {
    /// The one place every acting handler commits an attempt through: the
    /// rail row always, the fragment only when `refused` is false, so a verb
    /// cannot draw one and miss the other.
    fn commit_act(&self, act: DeskAct, subject: Option<&str>, payload: String, refused: bool) {
        self.log
            .borrow()
            .record_emitter()
            .emit_or_report(crate::record::Display::HarnessCall {
                verb: act.verb().to_string(),
                subject: subject.map(str::to_string),
                payload,
                failed: refused,
            });
        if !refused {
            self.acts
                .0
                .borrow_mut()
                .push((act, subject.map(str::to_string)));
        }
    }
}

/// Answers one [`FOValue`] enquiry against a captured [`HostServices`]. Fresh
/// per `ral` call; wrapped in [`RunHost`], the [`Host`] every dispatch rides.
pub(crate) struct ExarchDesk {
    pub(crate) services: HostServices,
}

impl ExarchDesk {
    /// Decode one enquiry and answer it. The decode is the vocabulary's, so
    /// an ill-shaped request is refused here in the words its builtin would use.
    ///
    /// # Errors
    /// The decoder's refusal, or the addressed handler's.
    pub(crate) fn handle(&self, req: &FOValue) -> Result<FOValue, Error> {
        match Request::decode(req).map_err(Error::new)? {
            Request::Agents(Agents::List) => {
                Ok(listing(&self.services.handle, &self.services.fleet).encode())
            }
            Request::Agents(Agents::Start(start)) => self.launch(start),
            Request::Agents(Agents::Message(message)) => self.message(message),
            Request::Agents(Agents::Cancel(name)) => self.agent_cancel(&name),
            Request::Agents(Agents::Reply(value)) => self.agent_reply(value),
            Request::Agents(Agents::Read(name)) => self.agent_read(name),
            Request::Schedules(Schedules::List) => {
                self.require_schedule_grant("exarch-schedules `list")?;
                Ok(self.schedule_table())
            }
            Request::Schedules(Schedules::Add(add)) => self.schedule(add),
            Request::Schedules(Schedules::Remove(label)) => self.unschedule(&label),
            Request::Pins(Pins::Set(pin)) => Ok(self.apply_pin(pin.key, Some(pin.body))),
            Request::Pins(Pins::Clear(key)) => Ok(self.apply_pin(key, None)),
            Request::Pins(Pins::Read(key)) => Ok(self.pin_read(&key)),
            Request::Pins(Pins::List) => Ok(self.pin_list()),
            Request::Context(Context::Survey) => Ok(Survey::from(self.context_survey()).encode()),
            Request::Context(Context::Evict(evict)) => self.context_evict(evict),
            // The index stays whole under the lock: it is a projection of
            // rows the structure already holds, touching no file.
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
