//! The exarch enquiry desk: the host half of `shell.enquire(class)`.
//!
//! [`HostServices`] is all `&Avatar` can lend a handler without lending
//! `&mut Avatar`/`&mut Shell`, since the reentrancy law bars a handler from
//! taking the session lock. [`ExarchDesk`] answers one enquiry against that
//! capture, paired with its [`SurfaceApplier`] in one [`RunHost`], which
//! implements [`ral_core::carrier::Host`] — the object every dispatch
//! rides, so a handler's chrome can never outrun the run's earlier surface
//! output.

use crate::agent::fleet::Fleet;
use crate::agent::roster::listing;
use crate::agent::seat::SeatKind;
use crate::agent::{Agent, Avatar, LogCell, ReplyCell};
use crate::bus::{Emitter, Stamp};
use crate::enquiry::{
    Agents, Context, Hits, Indexed, Material, Pins, Request, Schedules, Survey, Transcript,
};
use crate::shell_eval::{self, Surface};
use atomic_refcell::{AtomicRef, AtomicRefCell, AtomicRefMut};
use ral_core::carrier::Host;
use ral_core::fact::{Act, Worker};
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::protocol::EnquiryError;
use ral_core::sync::LockExt;
use ral_core::types::{Error, Observation, Observed};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

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

    /// The inverse of [`Self::verb`]: every string this fragment ever carries
    /// was minted from it, so this never meets a name it does not know.
    fn from_verb(verb: &str) -> Self {
        match verb {
            "spawn" => Self::Spawn,
            "cancel" => Self::Cancel,
            "message" => Self::Message,
            "schedule" => Self::Schedule,
            "unschedule" => Self::Unschedule,
            "reply" => Self::Reply,
            "evict" => Self::ContextEvict,
            other => unreachable!("desk act fragment carries an unknown verb `{other}`"),
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

/// What this `ral` call has committed, in the order it landed: one
/// [`Observation`] per attempt that landed, minted at [`HostServices::commit_act`]
/// — the door every acting handler funnels through — and read back once the
/// call has failed.
///
/// A call the wall cut short still started what it started and delivered what
/// it delivered, and no other in-band channel says so: the model's only other
/// signal — the diagnostic — speaks of the failure and not of the acts.
///
/// Only committed acts are recorded. A refused act changed nothing, so it
/// leaves no entry: this fragment answers *what stands*.
#[derive(Clone, Default)]
pub(crate) struct ActFragment(Arc<AtomicRefCell<Vec<Observation>>>);

impl ActFragment {
    /// Never waits: the only thread that could hold this borrow is the asker —
    /// a desk handler runs on the attend thread parked in `run_shell`, and the
    /// audit is read on that same thread once the run is back.
    fn borrow_mut(&self) -> AtomicRefMut<'_, Vec<Observation>> {
        self.0
            .try_borrow_mut()
            .unwrap_or_else(|_| Self::contended())
    }

    fn borrow(&self) -> AtomicRef<'_, Vec<Observation>> {
        self.0.try_borrow().unwrap_or_else(|_| Self::contended())
    }

    fn contended() -> ! {
        panic!(
            "act fragment contended: a desk handler may only run while the attend thread is \
             parked in run_shell"
        )
    }

    /// The sentence an unwind owes the model, or `None` when this call committed
    /// nothing at all — where silence is the whole truth.
    pub(crate) fn audit(&self) -> Option<String> {
        let done = {
            let acts = self.borrow();
            if acts.is_empty() {
                return None;
            }
            acts.iter()
                .filter_map(|obs| match &obs.what {
                    Observed::Act(act) => {
                        Some(DeskAct::from_verb(&act.verb).done(act.subject.as_deref()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        Some(format!(
            "audit: this call had already {done}; that work stands; do not repeat it.\n"
        ))
    }
}

/// Everything a desk handler may read off `&Avatar`, snapshotted fresh at every
/// [`crate::agent::Avatar::ral`] install so no capture goes stale mid-call.
pub(crate) struct HostServices {
    /// This run's own agent: parent of what it spawns, root of the descendant
    /// check `` `message ``/`` `cancel `` enforce, and the source every
    /// immutable-config read (`caps`, `fuel`, `returns`, …) is taken from.
    pub agent: Arc<Agent>,
    pub fleet: Arc<Fleet>,
    /// How this call's forks reach the desk: `` `start `` and `` `branch ``
    /// choose their arm on this fact.
    pub kind: SeatKind,
    pub emit: Emitter,
    /// Where the `reply` handler stages its value, holding no `&mut Avatar` to
    /// write it any other way.
    pub reply: ReplyCell,
    /// A `mnemon` spawn forks its inherited context off this.
    pub log: LogCell,
    /// The `/branch` this call serves, if it is the host's own.
    pub branch: Option<Arc<BranchOrder>>,
    /// The calling agent's own envelope, minted at install, so a desk older
    /// than *its* `/clear` refuses to spawn.
    pub stamp: Stamp,
    /// What this call has committed so far: minted per `ral` call, and read
    /// back by [`crate::shell_eval::report::tool_result`] when a raise discards
    /// the bindings but not the acts.
    pub acts: ActFragment,
    /// Who the acts are committed on behalf of, read once at install: the
    /// desk holds no `Shell` to ask, and a host act's principal is the host's.
    /// `None` where the host names nobody, the same fact `Context::principal`
    /// reports for an unbound `USER` in the same record.
    pub principal: Option<String>,
}

/// A `/branch` under way: the name the host chose, whether the fork reports
/// back, and the child the desk builds for it.
pub(crate) struct BranchOrder {
    pub name: String,
    pub returns: bool,
    pub child: std::sync::Mutex<Option<Avatar>>,
}

impl HostServices {
    /// The one door every acting handler commits an attempt through: one
    /// [`Observation`] built at the arm where the outcome is known, fanned to
    /// both its readers off that single datum — the rail row always, the
    /// fragment only when `refused` is false. A verb that reaches here cannot
    /// draw one reader's row and miss the other's, since there is only the
    /// one construction either draws from.
    fn commit_act(&self, act: DeskAct, subject: Option<&str>, payload: String, refused: bool) {
        let obs = Observation::instant(
            None,
            self.principal.clone(),
            Observed::Act(Act {
                verb: act.verb().to_string(),
                subject: subject.map(str::to_string),
                payload: payload.clone(),
                refused,
            }),
        );
        self.record_display(crate::record::Display::HarnessCall {
            verb: act.verb().to_string(),
            subject: subject.map(str::to_string),
            payload,
            failed: refused,
        });
        if !refused {
            self.acts.borrow_mut().push(obs);
        }
    }

    /// Author one display commit through the session's record seam.  A desk
    /// handler runs while the attend thread is parked in `run_shell`, so the
    /// log cell is free to lend the seam; the append failure a handler cannot
    /// propagate surfaces as its own error row instead of a shrug.
    fn record_display(&self, commit: crate::record::Display) {
        let recorder = self.log.borrow().record_emitter();
        if let Err(error) = recorder.emit(commit) {
            recorder.report_fault(&error);
        }
    }

    /// [`Self::record_display`] for the forensic class — the harness-result
    /// breadcrumbs that pair with an act's row.
    fn record_forensic(&self, fact: crate::record::Forensic) {
        let recorder = self.log.borrow().record_emitter();
        if let Err(error) = recorder.emit(fact) {
            recorder.report_fault(&error);
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
    /// an ill-shaped request is refused here in the words its door would use.
    ///
    /// # Errors
    /// The decoder's refusal, or the addressed handler's.
    pub(crate) fn handle(&self, req: &FOValue) -> Result<FOValue, Error> {
        match Request::decode(req).map_err(Error::new)? {
            Request::Agents(Agents::List) => Ok(listing(&self.services.agent).encode()),
            Request::Agents(Agents::Start(start)) => self.launch(start),
            Request::Agents(Agents::Message(message)) => self.message(message),
            Request::Agents(Agents::Cancel(name)) => self.agent_cancel(&name),
            Request::Agents(Agents::Reply(value)) => self.agent_reply(value),
            Request::Agents(Agents::Read(name)) => self.agent_read(name),
            Request::Agents(Agents::Branch(fork)) => self.agent_branch(fork),
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

/// Decodes a surfaced value straight into the record.
/// [`RunHost::apply`] is what every dispatch's drain loop
/// ([`ral_core::carrier::dispatch_to_report`]) reaches through the protocol,
/// so a call's surfaced values always render off the one applier it was built
/// with.
pub(crate) struct SurfaceApplier {
    pub(crate) recorder: crate::record::Emitter,
    /// Every `Observed::Worker` this call's drain has heard: the births the
    /// orphan sentence joins against the run-boundary `workers` probe.
    births: Mutex<HashSet<u64>>,
}

impl SurfaceApplier {
    pub(crate) fn new(recorder: crate::record::Emitter) -> Self {
        Self {
            recorder,
            births: Mutex::default(),
        }
    }

    pub(crate) fn births(&self) -> HashSet<u64> {
        self.births.lock_ignore_poison().clone()
    }

    /// Apply one live [`ral_core::protocol::Event::Surface`] value.
    ///
    /// A shape no surface class recognises at all is the extension law's
    /// loud case — recorded, not dropped silently; `landing`'s own rejection
    /// of a known observation stays silent, since that is a class this host
    /// chose not to render, not an unknown one.
    pub(crate) fn live(&self, val: &FOValue) {
        let shape = val.shape();
        let surface = match shell_eval::decode_surface(val) {
            shell_eval::Decoded::Surface(surface) => surface,
            shell_eval::Decoded::Landed => return,
            shell_eval::Decoded::Unknown => {
                if let Err(error) = self.recorder.emit(shell_eval::unknown_surface_note(shape)) {
                    self.recorder.report_fault(&error);
                }
                return;
            }
        };
        if let Surface::Observation(event) = &surface
            && let Observed::Worker(Worker { id, .. }) = &event.what
        {
            self.births.lock_ignore_poison().insert(id.0);
        }
        if let Err(error) = absorb_surface(&self.recorder, &surface) {
            self.recorder.report_fault(&error);
        }
    }
}

/// An applier alone is the mute host the bare harness runs under:
/// nothing answers.
impl Host for SurfaceApplier {
    fn surface(&self, val: &FOValue) {
        self.live(val);
    }

    fn enquire(&self, _req: FOValue) -> Result<FOValue, EnquiryError> {
        Err(EnquiryError::no_desk())
    }
}

/// Record one decoded [`Surface`] — shared by [`SurfaceApplier::live`] and
/// the deferred-batch path in `agent::attend::announce`, so the two cannot
/// drift on what records.
///
/// The record carries raw facts in arrival order: one
/// [`crate::record::Display::Observation`] per observation, one
/// [`crate::record::Display::Change`] per write or edit, one
/// [`crate::record::Display::Card`] per card.  Grouping a call's effects and
/// a run's changes belongs to the frontend, which derives them online and so
/// needs no coalesced log to rebuild from.
///
/// A pin publishes twice: [`crate::record::Forensic::Pin`]/`Unpin` is the
/// durable breadcrumb a resume replays, [`crate::record::Transient::Pin`]/
/// `Unpin` is the register the live process is holding, which a resume does
/// not restore.
pub(crate) fn absorb_surface(
    recorder: &crate::record::Emitter,
    surface: &Surface,
) -> std::io::Result<()> {
    match surface {
        Surface::Observation(event) => {
            let value = (**event).clone().encode();
            let _recorded = recorder.emit(crate::record::Display::Observation { value })?;
            Ok(())
        }
        Surface::Change(change) => {
            let _recorded = recorder.emit(crate::record::Display::Change {
                change: change.clone(),
            })?;
            Ok(())
        }
        Surface::Card(card) => {
            let _recorded = recorder.emit(crate::record::Display::Card { card: card.clone() })?;
            Ok(())
        }
        Surface::Done { cmd, outcome } => {
            let _recorded = recorder.emit(crate::record::Display::Done {
                cmd: cmd.clone(),
                outcome: outcome.clone(),
            })?;
            Ok(())
        }
        Surface::Notice(ral_core::types::Notice::Reap(reap)) => {
            let _recorded = recorder.emit(crate::record::Forensic::Reap {
                cmd: reap.cmd.clone(),
                cause: <&str>::from(reap.cause).to_string(),
            })?;
            Ok(())
        }
        Surface::Notice(ral_core::types::Notice::Prune(pruned)) => {
            let _recorded = recorder.emit(crate::record::Forensic::Prune {
                names: pruned.iter().map(|p| p.name.clone()).collect(),
                idle_calls: pruned.iter().map(|p| p.idle_calls).collect(),
            })?;
            Ok(())
        }
        // Register state, not scrollback: the history informs the resume note.
        Surface::Pin { key, card } => {
            let _recorded = recorder.emit(crate::record::Forensic::Pin { key: key.clone() })?;
            recorder.transient(crate::record::Transient::Pin {
                key: key.clone(),
                card: card.clone(),
            });
            Ok(())
        }
        Surface::Unpin { key } => {
            let _recorded = recorder.emit(crate::record::Forensic::Unpin { key: key.clone() })?;
            recorder.transient(crate::record::Transient::Unpin { key: key.clone() });
            Ok(())
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
        self.apply.live(val);
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
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests;

#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod wire_tests;
