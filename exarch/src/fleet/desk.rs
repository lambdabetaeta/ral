//! The exarch enquiry desk: the host half of `shell.enquire(class)`.
//!
//! [`HostServices`] is all `&Avatar` can lend a handler without lending
//! `&mut Avatar`/`&mut Shell`, since the reentrancy law bars a handler from
//! taking the session lock. [`ExarchDesk`] answers one enquiry against that
//! capture, paired with its [`SurfaceApplier`] in one [`RunHost`], which
//! implements [`ral_core::protocol::Host`] — the object every dispatch
//! rides, so a handler's chrome can never outrun the run's earlier surface
//! output.

use crate::agent::event::{AgentLog, ContextSurvey, EditAuthority};
use crate::agent::seat::{Seat, SeatKind};
use crate::agent::{Agent, Avatar, Build, LogCell, ProviderHandle, ReplyCell};
use crate::bus::card::{Card, encode_card};
use crate::bus::{Emitter, Stamp};
use crate::fleet::Fleet;
use crate::fleet::enquiry::{
    Add, Agents, Context, Deposit, Evict, ForkClaim, Hits, Indexed, Material, Memory, Message,
    Name, Note, Pins, Request, Schedules, Selection, Start, Survey, Transcript,
};
use crate::fleet::roster::{listing, summary};
use crate::provider::Provider;
use crate::shell_eval::{self, Surface};
use ral_core::SpawnGrant;
use ral_core::protocol::{EnquiryError, Host};
use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum;
use ral_core::sync::LockExt;
use ral_core::types::{Error, Observation, Observed};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The two words that bracket one hatch, in opposite directions: the host
/// writes the eight token bytes the guest's listener is waiting for, and reads
/// back the single byte that listener writes only once the child's `spawn` has
/// returned. Both precede the first frame; neither is one.
///
/// The clock is the transport's own, so a dial carries no second deadline —
/// and it is lifted again before the stream is adopted, since the reader
/// thread must then park in `read_frame` for as long as the child lives.
fn greet_hatch(stream: &mut ral_core::wire::WireStream, token: u64) -> Result<(), String> {
    use std::io::{Read, Write};
    let patience = ral_core::protocol::Liveness::default().deadline;
    stream
        .set_write_timeout(Some(patience))
        .and_then(|()| stream.set_read_timeout(Some(patience)))
        .map_err(|e| format!("could not put the hatch deadline on the dialled wire: {e}"))?;
    stream
        .write_all(&token.to_le_bytes())
        .map_err(|e| format!("could not send the hatch token to the guest's listener: {e}"))?;
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            "the guest closed the connection before acknowledging the hatch".to_string()
        }
        _ => format!("could not read the guest's hatch acknowledgement: {e}"),
    })?;
    if ack[0] != ral_core::protocol::HATCH_ACK {
        return Err(format!(
            "the guest answered the hatch with byte {:#04x} rather than the acknowledgement — \
             whatever is listening on that port is not a ral engine waiting to be hatched",
            ack[0]
        ));
    }
    stream
        .set_read_timeout(None)
        .map_err(|e| format!("could not lift the hatch deadline from the wire: {e}"))
}

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
pub(crate) struct ActFragment(Arc<Mutex<Vec<Observation>>>);

impl ActFragment {
    /// A fragment seeded with `acts` directly, bypassing `commit_act` — for a
    /// renderer test that needs a settled fragment with no live desk behind it.
    #[cfg(test)]
    pub(crate) fn from_acts(acts: Vec<Observation>) -> Self {
        Self(Arc::new(Mutex::new(acts)))
    }

    /// Never waits: the only thread that could hold this guard is the asker —
    /// a desk handler runs on the attend thread parked in `run_shell`, and the
    /// audit is read on that same thread once the run is back.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Observation>> {
        match self.0.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => panic!(
                "act fragment contended: a desk handler may only run while the attend thread is \
                 parked in run_shell"
            ),
            Err(std::sync::TryLockError::Poisoned(_)) => panic!("act fragment poisoned"),
        }
    }

    /// The sentence an unwind owes the model, or `None` when this call committed
    /// nothing at all — where silence is the whole truth.
    pub(crate) fn audit(&self) -> Option<String> {
        let done = {
            let acts = self.lock();
            if acts.is_empty() {
                return None;
            }
            acts.iter()
                .filter_map(|obs| match &obs.what {
                    Observed::Act { verb, subject, .. } => {
                        Some(DeskAct::from_verb(verb).done(subject.as_deref()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        Some(format!(
            "audit: this call had already {done}; that work stands — do not repeat it.\n"
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
    /// Frozen at install: the path root `narrow` and every handler resolves
    /// from, and a hatched child's cwd.
    pub cwd: PathBuf,
    /// The engine's `$HOME`, frozen at install: a hatched child's home.
    pub home: Option<PathBuf>,
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
    /// reports for an unbound `$USER` in the same record.
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
            Observed::Act {
                verb: act.verb().to_string(),
                subject: subject.map(str::to_string),
                payload: payload.clone(),
                refused,
            },
        );
        self.record_display(crate::record::Display::HarnessCall {
            verb: act.verb().to_string(),
            subject: subject.map(str::to_string),
            payload,
            failed: refused,
        });
        if !refused {
            self.acts.lock().push(obs);
        }
    }

    /// Author one display commit through the session's record seam.  A desk
    /// handler runs while the attend thread is parked in `run_shell`, so the
    /// log cell is free to lend the seam; the append failure a handler cannot
    /// propagate surfaces as its own error row instead of a shrug.
    fn record_display(&self, commit: crate::record::Display) {
        let recorder = self.log.lock().record_emitter();
        if let Err(error) = recorder.emit(commit) {
            recorder.report_fault(&error);
        }
    }

    /// [`Self::record_display`] for the forensic class — the harness-result
    /// breadcrumbs that pair with an act's row.
    fn record_forensic(&self, fact: crate::record::Forensic) {
        let recorder = self.log.lock().record_emitter();
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

/// The rail subject a cut mints from a turn address, as runs: `"turns 41–43,
/// 50"`. Sorted here, since the model's own list need not be.
fn turns_subject(turns: &[u64]) -> String {
    let mut sorted = turns.to_vec();
    sorted.sort_unstable();
    format!("turns {}", crate::record::model::runs(&sorted))
}

impl ExarchDesk {
    /// Decode one enquiry and answer it. The decode is the vocabulary's, so
    /// an ill-shaped request is refused here in the words its door would use.
    ///
    /// # Errors
    /// The decoder's refusal, or the addressed handler's.
    pub(crate) fn handle(&self, req: &FOValue) -> Result<FOValue, Error> {
        match Request::decode(req).map_err(|why| Error::new(why, 1))? {
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
                        log.transcript_index().into_iter().map(Indexed::from),
                    ))
                },
                |index| Ok(index.encode()),
            ),
            Request::Transcript(Transcript::Read(read)) => self.locate_then_read(
                |log| log.locate_read(&read.turns),
                |read| {
                    read.turns()
                        .map(|turns| Vec::from_iter(turns.into_iter().map(Material::from)).encode())
                },
            ),
            Request::Transcript(Transcript::Grep(grep)) => self.locate_then_read(
                |log| log.locate_grep(grep.turns.as_deref()),
                |read| {
                    read.grep(&grep.pattern)
                        .map(|hits| Hits::from(hits).encode())
                },
            ),
        }
    }

    /// The spawn spine behind `` `start ``: take up the fork, fork its log off
    /// the parent's, assemble it at one less unit of fuel, hand it to
    /// `spawn_async`. Every cheap guard runs before the fork is taken up; a
    /// refusal after it simply drops the child's engine.
    fn launch(&self, Start { spec, fork }: Start) -> Result<FOValue, Error> {
        let s = &self.services;
        let Name(name) = spec.name;

        // The captured fuel, caps, and grant are only as fresh as the
        // envelope they were snapshotted under — this caller's own, so a
        // `/clear` in another tab does not refuse a spawn here.
        if s.stamp.is_stale() {
            return Err(Error::new(
                "`exarch-agents `start` refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale — \
                 issue agent again on your next turn",
                1,
            ));
        }

        // Fuel bounds depth, not fan-out: the parent's own is never debited, so
        // siblings are free and only a deep enough chain bottoms out.
        if s.agent.fuel() == 0 {
            return Err(Error::new(
                "`exarch-agents `start` refused: no spawn fuel remains at this depth, so you cannot \
                 delegate any further here. Fuel bounds how deep a chain of spawns may \
                 recurse, never how many children you may start at any one depth — starting \
                 several agents here costs nothing extra. `exarch-agents `cancel` on any node stops \
                 its whole live subtree regardless of depth.",
                1,
            ));
        }

        // Didactic, not race-free: `register` below re-checks under its own
        // lock, and that is what closes a same-name race.
        if s.fleet.name_live(&name) {
            return Err(Error::new(
                format!(
                    "`exarch-agents `start` refused: a live agent already bears the name '{name}' — pick \
                     another, or wait for it to settle. Names identify live agents; agents \
                     lists yours."
                ),
                1,
            ));
        }

        // Before the seat split, so both arms share one resolution and a
        // refusal unwinds nothing: no adopted shell, no forked log, no dial.
        let provider = self.child_provider(&spec.provider, &spec.model)?;

        let seat = self.fork_seat("start", fork, &spec.grant.0)?;
        let child = self.child(
            "start",
            name.clone(),
            seat,
            provider,
            true,
            // A child may narrow its parent's search reach, never widen it.
            s.agent.search() && spec.search,
            spec.memory == Memory::Mnemon,
        )?;
        self.spawn_child(child, name, spec.prompt)
    }

    /// The child's provider: the parent's own `Arc` verbatim when neither
    /// half names a selection, and a freshly minted one otherwise.
    ///
    /// No catalog and no network: `` `inherit `` *states* which account the
    /// child is on, so a named model never has to be attributed to one, and a
    /// spawn can never block the fleet on a model-list round trip.
    fn child_provider(
        &self,
        provider: &Selection,
        model: &Selection,
    ) -> Result<Arc<Provider>, Error> {
        let s = &self.services;
        let current = s.agent.current_provider();
        if matches!((provider, model), (Selection::Inherit, Selection::Inherit)) {
            return Ok(current);
        }
        let refused = |why: String| Error::new(format!("`exarch-agents `start` refused: {why}"), 1);
        let bureau = s.agent.bureau();
        let available = bureau.available();
        let account = match provider {
            Selection::Inherit => current.account().clone(),
            Selection::Named(name) => {
                crate::provider::models::resolve_pinned_provider(name, &available)
                    .map_err(refused)?
            }
        };
        let model = match model {
            Selection::Named(model) => model.clone(),
            Selection::Inherit if account.id == current.account().id => current.model().to_string(),
            Selection::Inherit => account.service.default_model.clone().ok_or_else(|| {
                refused(format!(
                    "'{}' publishes no default model, so a child sent to it must be told which \
                     one to run — write `model: `named '<model>'` rather than `` `inherit ``",
                    crate::provider::identity::label(&account, &available)
                ))
            })?,
        };
        bureau.reselect(&current, &account, model).map_err(refused)
    }
    /// Take up the fork a builtin body left for this desk: adopt it out of
    /// the parent's own transport, narrowed there by `grant`, or dial the
    /// listener it opened across the wire, whose engine narrows itself by the
    /// seed it carries.
    fn fork_seat(&self, verb: &str, fork: ForkClaim, grant: &SpawnGrant) -> Result<Seat, Error> {
        let refused =
            |why: String| Error::new(format!("`exarch-agents `{verb}` refused: {why}"), 1);
        match (&self.services.kind, fork) {
            (SeatKind::Identity(parent), ForkClaim::Parked(id)) => parent
                .adopt_parked(id, grant)
                .map(Seat::adopted)
                .map_err(refused),
            (SeatKind::Wire, ForkClaim::Listening { port, token }) => {
                self.dial_seat(port, token).map_err(refused)
            }
            (SeatKind::Identity(_), ForkClaim::Listening { .. }) => Err(refused(
                "this host runs its children in its own process, so the fork must be `parked \
                 <nursery id>` — a session that says it is listening for a dial is describing a \
                 wire this desk does not have"
                    .into(),
            )),
            (SeatKind::Wire, ForkClaim::Parked(_)) => Err(refused(
                "this host reaches its children across a wire, so the fork must be `listening \
                 [port, token]` — a session that says it is parked in process is naming a \
                 nursery this desk does not have"
                    .into(),
            )),
        }
    }

    /// The wire arm: dial the listener the guest opened for this one fork,
    /// write the token it is waiting for, and seat the child once the guest
    /// acknowledges — which it does only once the child process exists.
    fn dial_seat(&self, port: u32, token: u64) -> Result<Seat, String> {
        let s = &self.services;
        let dial = s.agent.dial().ok_or(
            "this wire session has no dialler installed to reach a helper engine's listener — a \
             construction bug, since a fuelled wire trunk is refused at Avatar::root without one",
        )?;
        let home = s.home.clone().ok_or(
            "this engine has no `$HOME` for its child to inherit — is `HOME` unset in it?",
        )?;
        let mut stream = dial.dial(port).map_err(|reason| {
            format!("could not dial the helper engine's listener on guest port {port} — {reason}")
        })?;
        greet_hatch(&mut stream, token)?;
        // Past the ack the child is alive, so a refusal from here on simply
        // drops the stream: the child reads EOF on fd 3 and the guest's own
        // table reaps it.
        let transport = ral_core::protocol::WireTransport::adopt(
            stream,
            ral_core::protocol::Liveness::default(),
        )
        .map_err(|e| format!("could not adopt the hatched wire: {e}"))?;
        // The parent engine's own two paths, as read at this call's install.
        // A hatched helper's logs live under the run that hatched it.
        Seat::wire(transport, s.cwd.clone(), home).map_err(|lost| {
            crate::agent::seat::EngineLost::starting(&lost, s.agent.run_dir()).to_string()
        })
    }

    /// A child of this call's agent, seated on `seat`: its fuel one less, and
    /// its log forked off the parent's. A returning child reports to its
    /// parent; one that does not roots its own tree and converses.
    #[allow(
        clippy::too_many_arguments,
        reason = "each is one axis a spawn kind decides; a struct would only rename them"
    )]
    fn child(
        &self,
        verb: &str,
        name: String,
        seat: Seat,
        provider: Arc<Provider>,
        returns: bool,
        search: bool,
        inherit_context: bool,
    ) -> Result<Avatar, Error> {
        let s = &self.services;
        let fuel = s.agent.fuel().saturating_sub(1);
        // Against the *child's* grants, never this agent's, so the opening
        // bookend records the child's real system length.
        let system_prompt = s.agent.index().apply(
            s.agent.system_base(),
            &crate::prompt::Grants {
                returns,
                allow_schedule: s.agent.allow_schedule,
                spawns: fuel > 0,
            },
            &name,
        );
        let account =
            crate::agent::RecordedAccount::of(provider.account(), &s.agent.bureau().available());
        let log = {
            let parent_log = s.log.lock();
            let mut log = parent_log
                .fork(
                    crate::agent::fresh_id(),
                    system_prompt.len(),
                    provider.model(),
                    &account,
                )
                .map_err(|e| Error::new(format!("could not fork child session log: {e}"), 1))?;
            let inherited = inherit_context.then(|| parent_log.inherited_context());
            drop(parent_log);
            if let Some(inherited) = inherited {
                log.import_context(inherited)
                    .map_err(|e| Error::new(e, 1))?;
            }
            log
        };
        Avatar::assemble(Build {
            name,
            system: s.agent.system_base().clone(),
            system_prompt,
            index: s.agent.index().clone(),
            // The child's engine holds its own layer; the stack it runs under
            // is its parent's.
            caps: s.agent.caps().clone(),
            seat,
            log,
            parent: returns.then(|| s.agent.clone()),
            fuel,
            provider: ProviderHandle::new(provider),
            interactive: s.agent.interactive(),
            returns,
            allow_schedule: s.agent.allow_schedule,
            tools: s.agent.tools(),
            search,
            fleet: s.fleet.clone(),
            run_lock: None,
            resume_summary: None,
            disk_warn_bytes: s.agent.disk_warn_bytes(),
            egress: s.agent.egress().clone(),
            dial: s.agent.dial().cloned(),
            bureau: s.agent.bureau().clone(),
        })
        .map_err(|why| Error::new(format!("`exarch-agents `{verb}` refused: {why}"), 1))
    }

    /// Hand `child` to `spawn_async` and answer the roster it now appears in —
    /// the state, not a receipt, and the same answer from either arm.
    fn spawn_child(&self, child: Avatar, name: String, prompt: String) -> Result<FOValue, Error> {
        let s = &self.services;
        // Held past the move, so the commitment arm can still name the act.
        let acted_name = name.clone();
        let acted_prompt = prompt.clone();
        let spawned = crate::shell_eval::tools::agent::spawn_async(
            child,
            crate::shell_eval::tools::agent::AsyncSpawn {
                name,
                prompt: Some(prompt),
            },
            &s.emit,
        );
        s.commit_act(
            DeskAct::Spawn,
            Some(&acted_name),
            acted_prompt,
            spawned.is_err(),
        );
        match spawned {
            Ok(_) => Ok(self.summary()),
            Err(reason) => Err(Error::new(reason, 1)),
        }
    }

    /// `` `branch `` — the desk half of the host's `/branch`: the fork takes
    /// the parent's whole authority and context, and waits for the host to
    /// take it up. Refused on any call that is not the host's own `/branch`.
    fn agent_branch(&self, fork: ForkClaim) -> Result<FOValue, Error> {
        let s = &self.services;
        let Some(order) = &s.branch else {
            return Err(Error::new(
                "`exarch-agents `branch` is the host's own `/branch` door, and no /branch is \
                 under way on this call",
                1,
            ));
        };
        let seat = self.fork_seat("branch", fork, &SpawnGrant::Inherit)?;
        let child = self.child(
            "branch",
            order.name.clone(),
            seat,
            s.agent.current_provider(),
            order.returns,
            s.agent.search(),
            !order.returns,
        )?;
        *order.child.lock_ignore_poison() = Some(child);
        Ok(FOValue::Unit)
    }

    /// The world after a transition — what every tag but `` `list `` and
    /// `` `read `` answers. A roster here would cost O(fleet) on every spawn
    /// of a fan-out to restate what the caller mostly knew; these two
    /// integers are what it could not have derived.
    fn summary(&self) -> FOValue {
        summary(&self.services.agent).encode()
    }

    /// `` `cancel `` — resolve a live descendant by name and cancel its whole
    /// subtree, scoped as [`Agent::descendant`] enforces. A real cancel and a
    /// miss are both successful calls answering the summary; only a scope
    /// violation raises.
    fn agent_cancel(&self, name: &str) -> Result<FOValue, Error> {
        let s = &self.services;
        // The row is derived after the call: one claiming "cancelled" ahead of
        // it would assert an effect the world never saw. `cancel` takes no
        // argument, so its payload column carries the outcome instead.
        let cancelled = match s.fleet.resolve(name) {
            None => Ok(false),
            Some(found) => match s.agent.descendant(&found) {
                Some(target) => {
                    target.cancel_tree(ral_core::process::CancelCause::Explicit);
                    Ok(true)
                }
                None => Err(()),
            },
        };
        let (payload, content, refused) = match cancelled {
            Ok(true) => (String::new(), format!("cancelling agent '{name}'"), false),
            Ok(false) => (
                "no live agent by that name".to_string(),
                format!("no live agent named '{name}'"),
                false,
            ),
            Err(()) => (
                "refused: not a descendant".to_string(),
                format!(
                    "agent '{name}' is not an agent you started; `exarch-agents `cancel` may only reach a descendant of yours"
                ),
                true,
            ),
        };
        s.commit_act(DeskAct::Cancel, Some(name), payload, refused);
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        // The raise is the model's only copy of a scope violation.
        if refused {
            Err(Error::new(content, 1))
        } else {
            Ok(self.summary())
        }
    }

    /// `` `message `` — resolve any live agent by name and send it a note.
    /// Unscoped, unlike `` `cancel ``: a note is the fleet's one way for a
    /// child to reach an ancestor or a sibling, and it only ever queues a turn.
    fn message(&self, Message { to: name, text }: Message) -> Result<FOValue, Error> {
        let s = &self.services;
        // Unlike `cancel`, an unresolved name refuses rather than no-ops:
        // `message` promises delivery and there is nothing to deliver to. A
        // target settling between the name resolving and the send lands in the
        // same arm, one step later. The row goes up after the send, since a
        // delivery's payload is its text but a refusal's is why.
        let sent = text.clone();
        let (payload, content, ok) = match s.fleet.resolve(&name) {
            None => (
                "refused: no live agent by that name".to_string(),
                format!("no live agent named '{name}'; did it finish already?"),
                false,
            ),
            Some(to) if to.id == s.agent.id => (
                "refused: that is you".to_string(),
                format!(
                    "agent '{name}' is you; `exarch-agents `message` reaches another agent — to wake yourself, arm a `exarch-schedules` fire"
                ),
                false,
            ),
            Some(to) => {
                s.agent.message(&to, text);
                (sent, format!("sent message to agent '{name}'"), true)
            }
        };
        s.commit_act(DeskAct::Message, Some(&name), payload, !ok);
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        // A delivery answers the roster like every other tag; a refusal raises,
        // and the raise is the model's only copy of it.
        if ok {
            Ok(self.summary())
        } else {
            Err(Error::new(content, 1))
        }
    }

    /// The self-wakeup guard the whole schedule family runs first. It is
    /// handed the tag the model typed, so the refusal names that and not a
    /// vocabulary the model was never taught.
    fn require_schedule_grant(&self, verb: &str) -> Result<(), Error> {
        if self.services.agent.allow_schedule {
            return Ok(());
        }
        Err(Error::new(
            format!(
                "`{verb}` refused: this agent does not hold the self-wakeup grant. An agent \
                 that can wake itself indefinitely holds real authority, so the grant is off \
                 by default; relaunch with `--allow-schedule` if this session genuinely needs \
                 to schedule its own wakeups."
            ),
            1,
        ))
    }

    /// `` `add `` — arm a self-wakeup through
    /// [`ScheduleRegistry::schedule`](crate::fleet::schedule::ScheduleRegistry::schedule).
    /// The answer is the table it now appears in; its `next-s` column already
    /// says everything a receipt could.
    fn schedule(
        &self,
        Add {
            trigger,
            label,
            prompt,
        }: Add,
    ) -> Result<FOValue, Error> {
        self.require_schedule_grant("exarch-schedules `add")?;
        let s = &self.services;

        // The row goes up after the registry call, so a refusal tiers instead
        // of reading as one that landed.
        let described = trigger.describe();
        let result = s
            .agent
            .schedules
            .schedule(trigger, prompt, label.clone(), s.agent.mailbox());
        let (payload, content) = match &result {
            Ok(receipt) => (
                described,
                format!(
                    "scheduled '{}' ({}s to first fire)",
                    receipt.label,
                    receipt.next_in.as_secs()
                ),
            ),
            Err(e) => (format!("refused: {e}"), format!("could not schedule: {e}")),
        };
        s.commit_act(DeskAct::Schedule, Some(&label), payload, result.is_err());
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        match result {
            Ok(_) => Ok(self.schedule_table()),
            Err(_) => Err(Error::new(content, 1)),
        }
    }

    /// A snapshot of this agent's live wakeups — every `` `exarch-schedules ``
    /// tag's answer.
    fn schedule_table(&self) -> FOValue {
        self.services.agent.schedules.list().encode()
    }

    /// `` `remove `` — take one scheduled wakeup off the table by label. Only
    /// the grant refusal raises; a label that was never there is a successful
    /// call answering a table that does not carry it.
    fn unschedule(&self, label: &str) -> Result<FOValue, Error> {
        self.require_schedule_grant("exarch-schedules `remove")?;
        let s = &self.services;
        // The rail's payload column spells out the miss the table only implies,
        // since the verb has no argument of its own to show there.
        let removed = s.agent.schedules.unschedule(label);
        let (payload, content) = if removed {
            (String::new(), format!("unscheduled '{label}'"))
        } else {
            (
                "no live schedule by that label".to_string(),
                format!("no live schedule labelled '{label}'"),
            )
        };
        s.commit_act(DeskAct::Unschedule, Some(label), payload, !removed);
        s.record_forensic(crate::record::Forensic::HarnessResult { text: content });
        Ok(self.schedule_table())
    }

    /// `` `reply `` — stage the payload into the cell [`Avatar::deliberate`] lifts
    /// into a deposit on this agent's own status once the batch drains:
    /// it parks the agent and hands the value to the parent's `` exarch-agents `read ``,
    /// rather than ending the run. Refused on every non-returning agent, keyed
    /// on `returns` and never on trunk-ness.
    fn agent_reply(&self, value: FOValue) -> Result<FOValue, Error> {
        let s = &self.services;
        if !s.agent.returns() {
            return Err(Error::new(
                "exarch-agents `reply` refused: you converse with the user; you do not return. \
                 `reply` parks you and hands your value to your parent's exarch-agents `read — \
                 the interactive trunk and every /branch child instead keep talking, turn after \
                 turn, and hold no `reply` to call.",
                1,
            ));
        }
        let display = shell_eval::ral_value_to_text(&value).unwrap_or_default();
        let payload = if display.is_empty() {
            "(empty reply)".into()
        } else {
            display
        };
        // No subject: the parent is the only recipient a returning agent has.
        s.commit_act(DeskAct::Reply, None, payload, false);
        s.reply.set(value);
        Ok(self.summary())
    }

    /// `` `read `` — fetch the value the live descendant named by the payload
    /// last handed to `` `reply ``, scoped as `` `message `` is. The
    /// one tag that answers neither summary nor roster, but the fetched record
    /// instead, since that is the whole point of the call. A read is an
    /// observation, not an act, so nothing is committed to [`DeskAct`] — it
    /// changes nothing, exactly like `` `list ``.
    fn agent_read(&self, name: String) -> Result<FOValue, Error> {
        let s = &self.services;

        let content = match s.fleet.resolve(&name) {
            None => Err(format!(
                "no live agent named '{name}'; did it finish, or was it never started?"
            )),
            Some(found) => match s.agent.descendant(&found) {
                None => Err(format!(
                    "agent '{name}' is not an agent you started; `exarch-agents `read` may only reach \
                     a descendant of yours"
                )),
                // The deposit is left in place, so the fetch is idempotent
                // until the descendant replies again.
                Some(to) => to.reply().ok_or_else(|| {
                    format!(
                        "agent '{name}' has not replied yet — it is still working; wait for its \
                         notice instead of polling"
                    )
                }),
            },
        };
        match content {
            Ok(reply) => {
                s.record_forensic(crate::record::Forensic::HarnessResult {
                    text: format!("read agent '{name}'s reply"),
                });
                Ok(Deposit { name, reply }.encode())
            }
            Err(text) => {
                s.record_forensic(crate::record::Forensic::HarnessResult { text: text.clone() });
                Err(Error::new(text, 1))
            }
        }
    }

    /// `` `set ``/`` `clear `` — write or empty the register slot under
    /// `key`: update the mirror `` `read ``/`` `list `` answer from, then draw
    /// the forensic row and transient through [`absorb_surface`]. Answers
    /// `()`.
    fn apply_pin(&self, key: String, card: Option<Card>) -> FOValue {
        {
            let mut m = self.services.agent.pins.lock_ignore_poison();
            match &card {
                Some(card) => {
                    m.insert(key.clone(), shell_eval::PinDigest::new(card.clone()));
                }
                None => {
                    m.remove(&key);
                }
            }
        }
        let recorder = self.services.log.lock().record_emitter();
        let surface = match card {
            Some(card) => Surface::Pin { key, card },
            None => Surface::Unpin { key },
        };
        if let Err(error) = absorb_surface(&recorder, &surface) {
            recorder.report_fault(&error);
        }
        FOValue::Unit
    }

    /// `` `read `` — the card stored under `key` on this agent's own
    /// register, canonically re-encoded, or `()` on a miss. Read-after-write
    /// within one run is sound: `` `set ``/`` `clear `` write the mirror
    /// synchronously, on this same enquiry desk.
    fn pin_read(&self, key: &str) -> FOValue {
        let m = self.services.agent.pins.lock_ignore_poison();
        m.get(key)
            .map_or(FOValue::Unit, |digest| encode_card(&digest.card))
    }

    /// `` `list `` — the keys currently occupied on this agent's own
    /// register, in `BTreeMap` order.
    fn pin_list(&self) -> FOValue {
        let pins = self.services.agent.pins.lock_ignore_poison();
        Vec::from_iter(pins.keys().cloned()).encode()
    }

    /// The context as the log holds it. Silent, like the roster: a survey
    /// commits no act, and every edit tag answers one of these too.
    fn context_survey(&self) -> ContextSurvey {
        self.services.log.lock().context_survey()
    }

    /// The tail every `` `exarch-transcript `` tag shares: `locate` under the
    /// session lock, `read` once it is gone.  The family draws no row: a read
    /// is a listing, and a listing's telling is the record it answers with.
    fn locate_then_read<P>(
        &self,
        locate: impl FnOnce(&mut AgentLog) -> Result<P, String>,
        read: impl FnOnce(P) -> Result<FOValue, String>,
    ) -> Result<FOValue, Error> {
        // Its own statement: the guard dies at this semicolon, so a long read
        // never holds the seam, the bus and `/resources` behind it.
        let located = locate(&mut self.services.log.lock());
        located.and_then(read).map_err(|error| Error::new(error, 1))
    }

    /// `` `exarch-context `evict `` — the turns the address names leave the
    /// context at once, wherever they lie, but for a user turn whose answers
    /// still have a resident turn outside the set; the model's optional
    /// `note` stands in the marker left where they were. Commits the act
    /// under `refused` on either outcome, and mirrors the surviving weight or
    /// the refusal onto the trace before answering the survey.
    fn context_evict(&self, Evict { turns, note }: Evict) -> Result<FOValue, Error> {
        let note = note.map(|Note(note)| note);
        let payload = note
            .as_ref()
            .map_or_else(String::new, |note| format!("note {note}"));
        let subject = turns_subject(&turns);
        let result = self
            .services
            .log
            .lock()
            .evict(&turns, note, EditAuthority::Model);
        match result {
            // `evict` records `Evicted` through the seam, and the live row
            // derives from that published record: nothing separate to emit
            // here.
            Ok(()) => {
                self.services
                    .commit_act(DeskAct::ContextEvict, Some(&subject), payload, false);
                let survey = self.context_survey();
                let text = format!("context is now {} serialized bytes", survey.total_bytes);
                self.services
                    .record_forensic(crate::record::Forensic::HarnessResult { text });
                Ok(Survey::from(survey).encode())
            }
            Err(error) => {
                self.services
                    .commit_act(DeskAct::ContextEvict, Some(&subject), payload, true);
                self.services
                    .record_forensic(crate::record::Forensic::HarnessResult {
                        text: error.clone(),
                    });
                Err(Error::new(error, 1))
            }
        }
    }
}

/// Decodes a surfaced value straight into the record.
/// [`RunHost::apply`] is what every dispatch's drain loop
/// ([`ral_core::protocol::dispatch_to_report`]) reaches through the protocol,
/// so a call's surfaced values always render off the one applier it was built
/// with.
pub(crate) struct SurfaceApplier {
    pub(crate) recorder: crate::record::Emitter,
}

impl SurfaceApplier {
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
/// [`crate::record::Display::Card`] per card.  Grouping a call's effects and
/// merging a file's consecutive hunks belong to the frontend, which derives
/// them online and so needs no coalesced log to rebuild from.
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
            let value = event.to_wire();
            let _recorded = recorder.emit(crate::record::Display::Observation { value })?;
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
        Surface::Notice(notice) => {
            let _recorded = recorder.emit(crate::record::Display::Notice {
                notice: notice_fact(notice),
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

/// The data half of a `` `notice `` card, the reap cause carried as the three
/// spellings the record's serde surface names.
fn notice_fact(notice: &crate::bus::card::Notice) -> crate::record::NoticeFact {
    match notice {
        crate::bus::card::Notice::Reap { cmd, cause } => crate::record::NoticeFact::Reap {
            cmd: cmd.clone(),
            cause: match cause {
                ral_core::types::ReapCause::Idle => "idle",
                ral_core::types::ReapCause::Backstop => "backstop",
                ral_core::types::ReapCause::Retention => "retention",
            }
            .to_string(),
        },
        crate::bus::card::Notice::Prune { names, idle_calls } => crate::record::NoticeFact::Prune {
            names: names.clone(),
            idle_calls: idle_calls.clone(),
        },
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
            status: e.exit_code(),
            message: e.message,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::event::AgentLog;
    use crate::agent::testkit::ral_call;
    use crate::bus::{Inbox, Signal, channel};
    use crate::fleet::enquiry::{Grant, Grep, Launch, Pin, Reading};
    use crate::fleet::roster::AgentInfo;
    use crate::fleet::schedule::{Trigger, parse_duration};
    use crate::provider::{
        Provider,
        scripted::{Reply, Script},
    };
    use crate::record::{Display, FleetSink, Record, Transient};
    use ral_core::serial::datum::tag;
    use ral_core::types::NurseryId;
    use regex::Regex;
    use std::time::Duration;

    fn fresh_log() -> AgentLog {
        AgentLog::for_test(0, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    /// A fresh fleet's trunk and the [`HostServices`] a desk running as it
    /// captures, with `configure` free to narrow the trunk's own config
    /// before it is born — `search: false` for the one case that must be
    /// narrowed rather than granted, a custom `system_base`/`index` for the
    /// bookend test, `returns: false` for the one refusal that keys on it.
    /// Returns the fleet and the trunk's own inbox too, since a spawn test
    /// needs both to drive `` `start `` end to end.
    fn services_with(
        fuel: u32,
        configure: impl FnOnce(&mut crate::agent::testkit::TestAgentSpec),
    ) -> (HostServices, Arc<Fleet>, Inbox) {
        let parent_inbox = Inbox::new();
        let fleet = Fleet::new();
        let mut spec = crate::agent::testkit::TestAgentSpec::new("parent");
        spec.mailbox = parent_inbox.mailbox();
        spec.fuel = fuel;
        spec.returns = true;
        spec.search = true;
        configure(&mut spec);
        let agent = crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh fleet's trunk");
        let (emit, _rx) = crate::bus::dummy_emitter();
        let services = HostServices {
            fleet: fleet.clone(),
            kind: SeatKind::Identity(Arc::new(crate::bootstrap::test_transport())),
            stamp: agent.mailbox().stamp(),
            agent,
            emit,
            cwd: PathBuf::from("/"),
            home: Some(PathBuf::from("/tmp")),
            reply: ReplyCell::default(),
            log: LogCell::new(fresh_log()),
            branch: None,
            acts: ActFragment::default(),
            principal: ral_core::host::user(),
        };
        (services, fleet, parent_inbox)
    }

    /// The base capture every desk below builds on, so growing [`HostServices`]
    /// means touching one literal, not three.
    fn base_services() -> HostServices {
        services_with(3, |_| {}).0
    }

    fn desk() -> ExarchDesk {
        ExarchDesk {
            services: base_services(),
        }
    }

    fn append_prompt_and_answer(log: &mut AgentLog, prompt: &str, answer: &str) {
        log.append_user(prompt.to_string(), None).expect("prompt");
        log.append_assistant(
            genai::chat::ChatMessage::assistant(answer),
            Vec::new(),
            None,
        )
        .expect("answer");
    }

    impl ExarchDesk {
        /// `request` as its door sends it: encoded by the vocabulary, and
        /// decoded again on arrival.
        pub(super) fn ask(&self, request: Request) -> Result<FOValue, Error> {
            self.handle(&request.encode())
        }
    }

    fn context_evict_request(turns: &[u64], note: Option<&str>) -> Request {
        Request::Context(Context::Evict(Evict {
            turns: turns.to_vec(),
            note: note.map(|note| Note(note.to_string())),
        }))
    }

    /// An empty address is well-typed, so a read naming no turn is the shape
    /// the fold refuses rather than one the encoder cannot build.
    fn transcript_read_request(turns: &[u64]) -> Request {
        Request::Transcript(Transcript::Read(Reading {
            turns: turns.to_vec(),
        }))
    }

    fn transcript_grep_request(pattern: &str, turns: Option<&[u64]>) -> Request {
        Request::Transcript(Transcript::Grep(Grep {
            pattern: Regex::new(pattern).expect("a test pattern compiles"),
            turns: turns.map(<[u64]>::to_vec),
        }))
    }

    fn int_field(value: &FOValue, key: &str) -> i64 {
        value
            .field(key)
            .and_then(FOValue::as_int)
            .unwrap_or_else(|| panic!("record has no Int field `{key}`"))
    }

    fn str_field<'a>(row: &'a FOValue, key: &str) -> Option<&'a str> {
        row.field(key).and_then(FOValue::as_str)
    }

    fn text(value: &str) -> FOValue {
        value.to_string().encode()
    }

    /// A malformed request, spelt by hand because the vocabulary will not
    /// build it: the family names the class, the tag what to do.
    fn family_req(family: &str, label: &str, payload: Option<FOValue>) -> FOValue {
        tag(family, Some(tag(label, payload)))
    }

    /// The model's plainest spawn record: `` `amnemon ``, inheriting
    /// provider and model.
    pub(super) fn spec(prompt: &str, name: &str, grant: SpawnGrant, search: bool) -> Launch {
        Launch {
            prompt: prompt.to_string(),
            name: Name(name.to_string()),
            memory: Memory::Amnemon,
            grant: Grant(grant),
            search,
            provider: Selection::Inherit,
            model: Selection::Inherit,
        }
    }

    pub(super) fn confined() -> SpawnGrant {
        SpawnGrant::Base("confined".to_string())
    }

    pub(super) fn start(fork: ForkClaim, spec: Launch) -> Request {
        Request::Agents(Agents::Start(Start { spec, fork }))
    }

    /// The in-process shape: the fork waits in this host's own nursery.
    pub(super) fn start_req(session: NurseryId, prompt: &str, name: &str, search: bool) -> Request {
        start(
            ForkClaim::Parked(session),
            spec(prompt, name, confined(), search),
        )
    }

    pub(super) fn message_req(to: &str, text: &str) -> Request {
        Request::Agents(Agents::Message(Message {
            to: to.to_string(),
            text: text.to_string(),
        }))
    }

    fn reply_req(value: FOValue) -> Request {
        Request::Agents(Agents::Reply(value))
    }

    /// `` `exarch-schedules `add `` with an `` `after `` trigger — the only
    /// kind these tests arm, since a cron's first fire is not a fixed delay.
    fn add_req(after: &str, label: &str, prompt: &str) -> Request {
        Request::Schedules(Schedules::Add(Add {
            trigger: Trigger::After(parse_duration(after).expect("a test duration parses")),
            label: label.to_string(),
            prompt: prompt.to_string(),
        }))
    }

    fn remove_req(label: &str) -> Request {
        Request::Schedules(Schedules::Remove(label.to_string()))
    }

    /// Unwrap a summary answer into `(live, replied)`.
    pub(super) fn summary_counts(answer: &FOValue) -> (usize, usize) {
        let counts = crate::fleet::roster::Summary::decode(answer).expect("a summary answer");
        (counts.live, counts.replied)
    }

    /// The rows `` `list `` answers, for a test whose transition no longer
    /// carries them.
    pub(super) fn listed(desk: &ExarchDesk) -> Vec<AgentInfo> {
        let rows = desk
            .ask(Request::Agents(Agents::List))
            .expect("`list answers the rows");
        Vec::decode(&rows).expect("`list answers roster rows")
    }

    /// Unwrap a `` `exarch-schedules `` answer into its rows.
    fn table(answer: FOValue) -> Vec<FOValue> {
        let FOValue::List { items } = answer else {
            panic!("every `exarch-schedules tag answers the bare table")
        };
        items
    }

    /// [`desk`] holding the self-wakeup grant, so a schedule test reaches past it.
    fn granted_desk() -> ExarchDesk {
        ExarchDesk {
            services: services_with(3, |spec| spec.allow_schedule = true).0,
        }
    }

    /// A desk whose parent holds the very inbox this returns, so
    /// `` `start ``/`` `cancel ``/`` `message `` run end to end and a child's
    /// result is observable — unlike [`desk`].
    fn spawnable_desk(fuel: u32) -> (Arc<ExarchDesk>, Arc<Fleet>, Inbox) {
        let (services, fleet, parent_inbox) = services_with(fuel, |_| {});
        (Arc::new(ExarchDesk { services }), fleet, parent_inbox)
    }

    /// Poll `inbox` for the next exchange-boundary item — a spawned child's
    /// settled [`crate::bus::AgentResult`] lands here.
    fn wait_for_settle(inbox: &Inbox) -> crate::bus::Item {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(item) = inbox.next_item() {
                return item;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child did not settle within the timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    type Parked<R> = Box<dyn FnOnce(&ExarchDesk, NurseryId) -> R + Send>;

    /// Answers the one `` `branch `` a real run enquires by running its `f`
    /// against the fork that run parked, while the fork is still in its pen.
    struct Parking<R> {
        desk: Arc<ExarchDesk>,
        f: Mutex<Option<Parked<R>>>,
        out: Mutex<Option<R>>,
    }

    impl<R: Send> Host for Parking<R> {
        fn surface(&self, _val: &FOValue) {}

        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
            let Ok(Request::Agents(Agents::Branch(ForkClaim::Parked(id)))) = Request::decode(&req)
            else {
                panic!("an identity run's `branch parks its fork")
            };
            let f = self.f.lock().unwrap().take().expect("one enquiry per run");
            *self.out.lock().unwrap() = Some(f(&self.desk, id));
            Ok(FOValue::Unit)
        }
    }

    /// `f`'s answer about a fork a real run parked in the desk's own parent
    /// transport — the one door an identity fork reaches a desk by.
    fn with_parked<R: Send + 'static>(
        desk: &Arc<ExarchDesk>,
        f: impl FnOnce(&ExarchDesk, NurseryId) -> R + Send + 'static,
    ) -> R {
        let SeatKind::Identity(parent) = &desk.services.kind else {
            panic!("an identity desk parks in process")
        };
        let host = Arc::new(Parking {
            desk: desk.clone(),
            f: Mutex::new(Some(Box::new(f))),
            out: Mutex::default(),
        });
        let report = ral_core::protocol::dispatch_to_report(
            &**parent,
            crate::agent::testkit::source_run("_exarch-branch"),
            host.clone(),
        )
        .expect("an identity engine never severs");
        host.out
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| panic!("the run never enquired: {report:?}"))
    }

    /// Whether the fork `id` still waits in the desk's parent's pen.
    fn still_parked(desk: &ExarchDesk, id: NurseryId) -> bool {
        let SeatKind::Identity(parent) = &desk.services.kind else {
            panic!("an identity desk parks in process")
        };
        parent.adopt_parked(id, &SpawnGrant::Inherit).is_ok()
    }

    #[test]
    fn unknown_class_answers_the_extension_error() {
        let err = desk()
            .handle(&FOValue::Variant {
                label: "no-such-class".into(),
                payload: None,
            })
            .expect_err("an unrecognised class must not answer Ok");
        assert_eq!(err.message, "unrecognised enquiry class `no-such-class`");
    }

    /// Nesting the tag under the family must not open a silent hole one level
    /// down: a tag extends a family the way a class extends the desk.
    #[test]
    fn unknown_tag_answers_the_extension_error_too() {
        for class in ["agents", "schedules", "pins", "context", "transcript"] {
            let err = desk()
                .handle(&family_req(class, "no-such-tag", None))
                .expect_err("an unrecognised tag must not answer Ok");
            assert!(
                err.message.starts_with(&format!(
                    "unrecognised tag in `exarch-{class} `no-such-tag` — "
                )),
                "got: {}",
                err.message
            );
        }
    }

    /// Names the expected shape rather than panicking or silently defaulting.
    #[test]
    fn non_variant_request_errors_didactically() {
        let err = desk()
            .handle(&FOValue::Unit)
            .expect_err("a non-variant request must not answer Ok");
        assert!(
            err.message.contains("must be a variant"),
            "error must name the expected shape, got: {}",
            err.message
        );
    }

    #[test]
    fn context_survey_rows_every_resident_turn() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "closed\nturn", "answer");
            append_prompt_and_answer(&mut log, "evicted", "answer");
            log.evict(&[1, 2], None, EditAuthority::Model)
                .expect("evict");
            log.import_note(genai::chat::ChatMessage::user("inherited\ncontext"))
                .expect("import");
            log.append_user("live".into(), None).expect("live prompt");
        }
        let expected_bytes = desk.services.log.lock().history_bytes();

        let answer = desk
            .ask(Request::Context(Context::Survey))
            .expect("context survey");
        let kinds = survey_rows(&answer)
            .iter()
            .map(|row| str_field(row, "kind").expect("survey kind"))
            .collect::<Vec<_>>();
        assert_eq!(kinds, vec!["own", "own", "import", "own"]);
        assert_eq!(
            int_field(&answer, "total-bytes"),
            i64::try_from(expected_bytes).unwrap()
        );
    }

    /// One record per turn the read named, not one concatenated blob: the
    /// list is the shape the doc's own "read in slices" advice needs to be
    /// sayable.  A read is a listing, so it commits no act and draws no row.
    #[test]
    fn transcript_answers_one_record_per_turn_without_committing_an_act() {
        let mut desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "first prompt", "first answer");
        }
        let (tx, rx) = channel();
        desk.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        desk.services.emit = Emitter::new(tx, 0);

        let FOValue::List { items } = desk
            .ask(transcript_read_request(&[1, 2]))
            .expect("exarch-transcript `read")
        else {
            panic!("exarch-transcript `read must answer a list, one record per turn")
        };
        let [first, second] = items.as_slice() else {
            panic!("two named turns must answer exactly two records, got {items:?}")
        };
        assert_eq!(int_field(first, "turn"), 1);
        assert_eq!(str_field(first, "role"), Some("user"));
        let messages = first
            .field("messages")
            .and_then(FOValue::as_list)
            .unwrap_or_else(|| panic!("a read's messages are a list, got {first:?}"));
        assert_eq!(
            messages.len(),
            1,
            "the prompt turn holds one message, got {messages:?}"
        );
        assert_eq!(
            int_field(second, "turn"),
            2,
            "the second record is the turn asked for after it"
        );
        assert!(desk.services.acts.audit().is_none(), "a read has no act");

        desk.ask(transcript_read_request(&[]))
            .expect_err("a read that names no turn is not meaningful");
        let drawn = crate::bus::drain_records(&rx)
            .into_iter()
            .filter(|record| matches!(record, Record::Display(Display::HarnessCall { .. })))
            .count();
        assert_eq!(
            drawn, 0,
            "neither the read nor its refusal draws an act row: a listing's telling is its answer"
        );
    }

    /// A read answers the turns it named wherever they lie, each naming the
    /// role it bears; a turn past what is recorded, and the turn
    /// being written, are refused by the state they meet.
    #[test]
    fn transcript_read_names_the_role_of_every_turn_it_answers() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "first prompt", "first answer");
            append_prompt_and_answer(&mut log, "second prompt", "second answer");
        }
        let FOValue::List { items } = desk
            .ask(transcript_read_request(&[2, 3]))
            .expect("closed turns are readable")
        else {
            panic!("exarch-transcript `read must answer a list, one record per turn")
        };
        let reached = items
            .iter()
            .map(|item| {
                (
                    int_field(item, "turn"),
                    str_field(item, "role").expect("a read names the turn's role"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reached,
            vec![(2, "assistant"), (3, "user")],
            "an answer and the prompt after it, each naming its own role"
        );

        let error = desk
            .ask(transcript_read_request(&[9]))
            .expect_err("a read must not reach past what is recorded");
        assert_eq!(error.message, "turn 9 is not recorded — the latest is 4");

        desk.services
            .log
            .lock()
            .append_user("live".into(), None)
            .expect("live prompt");
        let error = desk
            .ask(transcript_read_request(&[5]))
            .expect_err("the turn being written is not readable");
        assert_eq!(
            error.message,
            "turn 5 is being written now — it is the one turn the transcript cannot read back yet"
        );
    }

    /// The pattern is compiled on decode, so an invalid one is refused in the
    /// regex crate's own words rather than in a paraphrase of them.
    #[test]
    #[expect(
        clippy::invalid_regex,
        reason = "the pattern that will not compile is this test's subject"
    )]
    fn grep_refuses_a_bad_regex_with_the_crate_message() {
        const UNCLOSED: &str = "(unclosed";
        let desk = desk();
        let expected = Regex::new(UNCLOSED)
            .expect_err("an unclosed group is not a regex")
            .to_string();
        let error = desk
            .handle(&family_req(
                "transcript",
                "grep",
                Some(FOValue::Map {
                    entries: vec![("pattern".to_string(), text(UNCLOSED))],
                }),
            ))
            .expect_err("an invalid regex is not searchable");
        assert!(error.message.ends_with(&expected), "got: {}", error.message);
    }

    /// `turn` for `turns` once searched the whole transcript in silence: the
    /// reader accepted the payload and no one ever asked for that field.
    #[test]
    fn transcript_grep_refuses_a_field_it_does_not_read() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "first prompt", "first answer");
        }
        let err = desk
            .handle(&family_req(
                "transcript",
                "grep",
                Some(FOValue::Map {
                    entries: vec![
                        ("pattern".to_string(), text("answer")),
                        (
                            "turn".to_string(),
                            FOValue::List {
                                items: vec![FOValue::Int { value: 1 }],
                            },
                        ),
                    ],
                }),
            ))
            .expect_err("`turn` is not a field `grep` reads");
        assert_eq!(
            err.message,
            "`exarch-transcript `grep`: unknown field `turn — did you mean `turns?"
        );
        desk.ask(transcript_grep_request("answer", Some(&[1, 2])))
            .expect("the spelt field still narrows the search");
    }

    /// The desk hands the fold's refusal to the model verbatim.
    #[test]
    fn context_edit_refusals_surface_the_admissibility_sentence() {
        let live_desk = desk();
        {
            let mut live = live_desk.services.log.lock();
            live.append_user("live".into(), None).expect("live prompt");
        }
        let err = live_desk
            .ask(context_evict_request(&[1], None))
            .expect_err("the turn being written is not editable");
        assert_eq!(
            err.message,
            "turn 1 is being written now — an eviction keeps the work in hand"
        );

        let unknown_desk = desk();
        {
            let mut log = unknown_desk.services.log.lock();
            append_prompt_and_answer(&mut log, "one", "answer");
        }
        let err = unknown_desk
            .ask(context_evict_request(&[7], None))
            .expect_err("an unrecorded turn is not editable");
        assert_eq!(err.message, "turn 7 is not recorded — the latest is 2");

        let evicted_desk = desk();
        {
            let mut log = evicted_desk.services.log.lock();
            append_prompt_and_answer(&mut log, "one", "answer");
            append_prompt_and_answer(&mut log, "two", "answer");
            append_prompt_and_answer(&mut log, "three", "answer");
        }
        evicted_desk
            .ask(context_evict_request(&[1, 2], None))
            .expect("evict");
        let err = evicted_desk
            .ask(context_evict_request(&[1], None))
            .expect_err("a turn that has left is not addressable");
        assert_eq!(
            err.message,
            "turn 1 has already left your context — the earliest still in it is 3"
        );

        evicted_desk
            .ask(context_evict_request(&[], None))
            .expect_err("an empty address is not an edit");
    }

    /// The edit's answer is the survey the transition leaves behind, not a
    /// receipt for the transition: the number that decides the next edit is
    /// `total-bytes` now, against the budget. The act it commits is
    /// [`DeskAct::ContextEvict`], and the audit sentence names it.
    #[test]
    fn context_evict_answers_the_survey_it_leaves_behind_and_commits_the_act() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "one", "a longer answer");
            append_prompt_and_answer(&mut log, "two", "another answer");
        }
        let answer = desk
            .ask(context_evict_request(&[1, 2], Some("the parser is fixed")))
            .expect("context evict");
        assert_eq!(
            int_field(&answer, "total-bytes"),
            i64::try_from(desk.services.log.lock().history_bytes()).unwrap(),
            "the survey's total is the context's own weight"
        );
        let rows = survey_rows(&answer);
        assert_eq!(
            rows.iter()
                .map(|row| (
                    int_field(row, "id"),
                    str_field(row, "role").expect("a survey row names its role")
                ))
                .collect::<Vec<_>>(),
            vec![(3, "user"), (4, "assistant")],
            "the evicted turns are gone from the answer"
        );
        assert_eq!(str_field(&rows[0], "kind"), Some("own"));
        let audit = desk
            .services
            .acts
            .audit()
            .expect("a landed eviction leaves an act");
        assert!(
            audit.contains("evicted context"),
            "the audit sentence must name the act, got: {audit}"
        );
    }

    /// The survey rows an answer carries, in id order.
    fn survey_rows(answer: &FOValue) -> &[FOValue] {
        answer
            .field("rows")
            .and_then(FOValue::as_list)
            .expect("a context answer carries its survey rows")
    }

    /// An empty note would render `Your note at eviction: ""` in the marker,
    /// so its decode refuses it rather than the shape allowing it.
    #[test]
    fn context_evict_refuses_an_empty_note() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "one", "answer");
            append_prompt_and_answer(&mut log, "two", "answer");
        }
        let err = desk
            .ask(context_evict_request(&[1], Some("")))
            .expect_err("an empty note is not a note");
        assert_eq!(
            err.message,
            "`exarch-context `evict`: `note: must not be empty — omit it to leave none"
        );
    }

    /// The marker keeps the note for the rest of the session, so the cap is
    /// what stands between one short line and a summary.
    #[test]
    fn context_evict_refuses_a_note_over_the_cap() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "one", "answer");
            append_prompt_and_answer(&mut log, "two", "answer");
        }
        let note = "x".repeat(241);
        let err = desk
            .ask(context_evict_request(&[1], Some(&note)))
            .expect_err("241 bytes is over the 240-byte cap");
        assert_eq!(
            err.message,
            "`exarch-context `evict`: `note: is 241 bytes; the marker keeps one short line — 240 at most. What is the one thing your future self needs to know?"
        );
    }

    /// A line break in a note would draw an extra row in the marker, read as
    /// one of the harness's own — the door refuses it rather than the
    /// renderer alone standing between the two.
    #[test]
    fn context_evict_refuses_a_multiline_note() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "one", "answer");
            append_prompt_and_answer(&mut log, "two", "answer");
        }
        for note in ["line one\nline two", "line one\rline two"] {
            let err = desk
                .ask(context_evict_request(&[1], Some(note)))
                .expect_err("a note that breaks a line is not one row");
            assert_eq!(
                err.message,
                "`exarch-context `evict`: `note: must be a single line — the marker draws one row per turn the cut takes, and a line break in a note reads as one of them."
            );
        }
    }

    /// A field no tag reads is refused, not dropped: an eviction that silently
    /// ignored `through` would cut turns the model never named.
    #[test]
    fn context_evict_refuses_a_field_it_does_not_read() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            append_prompt_and_answer(&mut log, "one", "answer");
            append_prompt_and_answer(&mut log, "two", "answer");
        }
        let err = desk
            .handle(&family_req(
                "context",
                "evict",
                Some(FOValue::Map {
                    entries: vec![
                        (
                            "turns".to_string(),
                            FOValue::List {
                                items: vec![FOValue::Int { value: 1 }],
                            },
                        ),
                        ("through".to_string(), FOValue::Int { value: 2 }),
                    ],
                }),
            ))
            .expect_err("`through` is not a field `evict` reads");
        assert_eq!(
            err.message,
            "`exarch-context `evict`: unknown field `through — expected `turns, `note"
        );
        desk.ask(context_evict_request(&[1, 2], Some("the parser is fixed")))
            .expect("the fields the tag does read still evict");
    }

    /// `` `exarch-pins `set ``: a one-span text card under `key`.
    fn pin_set_req(key: &str, text: &str) -> Request {
        let body = FOValue::Map {
            entries: vec![(
                "spans".into(),
                FOValue::List {
                    items: vec![FOValue::Map {
                        entries: vec![("text".into(), text.to_string().encode())],
                    }],
                },
            )],
        };
        let body = Card::decode(&tag("text", Some(body))).expect("a one-span text card");
        Request::Pins(Pins::Set(Pin {
            key: key.to_string(),
            body,
        }))
    }

    fn pin_clear_req(key: &str) -> Request {
        Request::Pins(Pins::Clear(key.to_string()))
    }

    fn pin_read_req(key: &str) -> Request {
        Request::Pins(Pins::Read(key.to_string()))
    }

    /// A pin written through `` `exarch-pins `set `` comes back from
    /// `` `exarch-pins `read `` as the canonical card — the readback and the
    /// pinned mark agree on shape, which is the whole point of a readable
    /// register.
    #[test]
    fn pin_read_returns_the_canonical_card() {
        let d = desk();
        d.ask(pin_set_req("tasks", "hi"))
            .expect("`exarch-pins `set` must answer Ok");

        let answer = d.ask(pin_read_req("tasks")).expect("a hit must answer Ok");
        let card =
            crate::bus::card::value_to_card(&answer).expect("the readback must decode as a card");
        assert!(
            matches!(
                card.marks(),
                [crate::bus::card::Mark::Text { spans }]
                    if spans.len() == 1 && spans[0].role.is_none() && spans[0].text == "hi"
            ),
            "the canonical card must round-trip the pinned text mark, got {card:?}"
        );
    }

    /// `` `set ``/`` `clear `` write and empty the register mirror
    /// `` `read ``/`` `list `` answer from.
    #[test]
    fn pin_set_and_clear_round_trip_through_the_desk() {
        let d = desk();

        assert!(
            matches!(d.ask(pin_read_req("tasks")), Ok(FOValue::Unit)),
            "an unset key must answer unit"
        );

        d.ask(pin_set_req("tasks", "hi"))
            .expect("`exarch-pins `set` must answer Ok");
        let answer = d
            .ask(pin_read_req("tasks"))
            .expect("a set key must read back");
        let card =
            crate::bus::card::value_to_card(&answer).expect("the readback must decode as a card");
        assert!(
            matches!(
                card.marks(),
                [crate::bus::card::Mark::Text { spans }]
                    if spans.len() == 1 && spans[0].text == "hi"
            ),
            "`set` must write the canonical card, got {card:?}"
        );

        d.ask(pin_clear_req("tasks"))
            .expect("`exarch-pins `clear` must answer Ok");
        assert!(
            matches!(d.ask(pin_read_req("tasks")), Ok(FOValue::Unit)),
            "`clear` must empty the slot `set` wrote"
        );
    }

    /// A key never pinned, and a key unpinned after being pinned, both answer
    /// `()` — a miss and a clear are the same absence to `` `exarch-pins `read ``.
    #[test]
    fn pin_read_answers_unit_on_miss_and_after_unpin() {
        let d = desk();

        assert!(
            matches!(d.ask(pin_read_req("tasks")), Ok(FOValue::Unit)),
            "a key never pinned must answer unit"
        );

        d.ask(pin_set_req("tasks", "hi"))
            .expect("`exarch-pins `set` must answer Ok");
        d.ask(pin_clear_req("tasks"))
            .expect("`exarch-pins `clear` must answer Ok");
        assert!(
            matches!(d.ask(pin_read_req("tasks")), Ok(FOValue::Unit)),
            "an unpinned key must answer unit"
        );
    }

    /// `` `exarch-pins `list `` names exactly the occupied keys, in
    /// `BTreeMap` order, and tracks a set/clear pair.
    #[test]
    fn pin_list_tracks_set_and_clear() {
        let d = desk();
        let keys = |d: &ExarchDesk| match d
            .ask(Request::Pins(Pins::List))
            .expect("`exarch-pins `list` must answer Ok")
        {
            FOValue::List { items } => items
                .into_iter()
                .map(|v| match v {
                    FOValue::String { value } => value,
                    other => panic!("`exarch-pins `list` must answer strings, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("`exarch-pins `list` must answer a list, got {other:?}"),
        };

        assert!(keys(&d).is_empty(), "an empty register lists no keys");

        d.ask(pin_set_req("b", "one"))
            .expect("`exarch-pins `set` must answer Ok");
        d.ask(pin_set_req("a", "two"))
            .expect("`exarch-pins `set` must answer Ok");
        assert_eq!(
            keys(&d),
            vec!["a", "b"],
            "keys list in BTreeMap (lexicographic) order"
        );

        d.ask(pin_clear_req("b"))
            .expect("`exarch-pins `clear` must answer Ok");
        assert_eq!(keys(&d), vec!["a"], "a clear drops its key from the list");
    }

    /// Every surface class the live applier renders also records through the
    /// seam — a `Display` record (or, for the pin register, `Forensic`) — in
    /// the order it arrived.  A pin/unpin additionally publishes a `Transient`
    /// beside its `Forensic` twin: the durable breadcrumb and the live
    /// register the process is holding, two records for one act.
    #[test]
    fn live_surfaces_record_their_seam_twins() {
        use crate::bus::Signal;
        use crate::record::{Display, Forensic, Record, Transient};

        let d = desk();
        let (tx, rx) = channel();
        d.services
            .log
            .lock()
            .record_emitter()
            .attach(crate::record::FleetSink {
                id: 0,
                tx: tx.downgrade(),
                meter: crate::bus::UsageMeter::default(),
            });
        let applier = SurfaceApplier {
            recorder: d.services.log.lock().record_emitter(),
        };

        let read = ral_core::types::Observation::instant(
            None,
            None,
            ral_core::types::Observed::Read {
                path: "a.rs".into(),
            },
        );
        applier.live(&read.to_wire());
        applier.live(&FOValue::Variant {
            label: "card".into(),
            payload: Some(Box::new(FOValue::List { items: vec![] })),
        });
        applier.live(&FOValue::Variant {
            label: "done".into(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![
                    (
                        "cmd".into(),
                        FOValue::String {
                            value: "<block>".into(),
                        },
                    ),
                    (
                        "outcome".into(),
                        FOValue::Variant {
                            label: "ok".into(),
                            payload: Some(Box::new(FOValue::Unit)),
                        },
                    ),
                ],
            })),
        });
        applier.live(&FOValue::Variant {
            label: "notice".into(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![
                    (
                        "kind".into(),
                        FOValue::Variant {
                            label: "reap".into(),
                            payload: None,
                        },
                    ),
                    (
                        "cmd".into(),
                        FOValue::String {
                            value: "sleep 10".into(),
                        },
                    ),
                    (
                        "cause".into(),
                        FOValue::String {
                            value: "idle".into(),
                        },
                    ),
                ],
            })),
        });
        d.ask(pin_set_req("tasks", "hi"))
            .expect("`exarch-pins `set` must answer Ok");
        d.ask(pin_clear_req("tasks"))
            .expect("`exarch-pins `clear` must answer Ok");

        let mut facts: Vec<&'static str> = Vec::new();
        let mut transients: Vec<&'static str> = Vec::new();
        while let Ok(sig) = rx.try_recv() {
            match sig {
                Signal::Fact(_, fact) => facts.push(match fact.value() {
                    Record::Display(Display::Observation { .. }) => "io",
                    Record::Display(Display::Card { .. }) => "card",
                    Record::Display(Display::Done {
                        outcome: crate::record::DoneOutcome::Ok,
                        ..
                    }) => "done",
                    Record::Display(Display::Notice {
                        notice: crate::record::NoticeFact::Reap { cause, .. },
                    }) if cause == "idle" => "notice",
                    Record::Forensic(Forensic::Pin { key }) if key == "tasks" => "pin",
                    Record::Forensic(Forensic::Unpin { key }) if key == "tasks" => "unpin",
                    _ => continue,
                }),
                Signal::Transient(_, t) => transients.push(match t {
                    Transient::Pin { key, .. } if key == "tasks" => "pin",
                    Transient::Unpin { key } if key == "tasks" => "unpin",
                    _ => continue,
                }),
            }
        }
        assert_eq!(
            transients,
            ["pin", "unpin"],
            "the pin register's live copy also publishes as a Transient beside its Forensic twin"
        );
        assert_eq!(
            facts,
            ["io", "card", "done", "notice", "pin", "unpin"],
            "every class records its twin, in the order it surfaced"
        );
    }

    /// The receipt names the child, and its reply notice lands in the parent's
    /// inbox once it settles — the value itself is fetched with `` `read ``.
    #[test]
    fn agent_start_spawns_and_delivers_result_to_parent_inbox() {
        let (desk, fleet, parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'hi from child'",
            )])),
        ));
        desk.services.agent.provider_handle().swap(provider);

        let answer = with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "say hi", "helper", true))
        })
        .expect("a valid `start must succeed");

        let (live, _) = summary_counts(&answer);
        assert_eq!(live, 1, "the child is the one other agent alive");
        let rows = listed(&desk);
        let child = rows
            .iter()
            .find(|row| row.name == "helper")
            .expect("the child stands on the listing");
        assert!(
            !child.log_dir.as_os_str().is_empty(),
            "a roster row carries the agent's log directory"
        );

        match wait_for_settle(&parent_inbox) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                    "the child's reply notice must reach the parent's inbox, got: {:?}",
                    result.outcome
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
        let helper = fleet.resolve("helper").expect("the child is still live");
        assert_eq!(
            desk.services
                .agent
                .descendant(&helper)
                .and_then(|child| child.reply()),
            Some(FOValue::String {
                value: "hi from child".into()
            }),
            "the deposited reply must be fetchable off the child"
        );
    }

    /// `` `read `` is the one tag that answers a record rather than the
    /// roster: after a scripted child replies, the parent fetches exactly
    /// what it deposited.
    #[test]
    fn agent_read_answers_the_childs_deposited_reply() {
        let (desk, _fleet, parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'read me'",
            )])),
        ));
        desk.services.agent.provider_handle().swap(provider);

        with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "say hi", "helper", true))
        })
        .expect("a valid `start must succeed");
        let _ = wait_for_settle(&parent_inbox);

        let answer = desk
            .ask(Request::Agents(Agents::Read("helper".into())))
            .expect("a descendant that has replied must answer its reply");
        assert_eq!(str_field(&answer, "name"), Some("helper"));
        assert_eq!(
            str_field(&answer, "reply"),
            Some("read me"),
            "`` `read `` must answer the very value the child handed to `reply"
        );
    }

    /// The state *is* the answer, not a receipt about the child just started:
    /// the roster a spawn answers with carries a sibling that spawn never
    /// touched, so nothing has to ask again to see what the fleet now is.
    #[test]
    fn start_answers_the_fleets_state_not_a_receipt() {
        let (desk, fleet, parent_inbox) = spawnable_desk(3);
        desk.services
            .agent
            .provider_handle()
            .swap(Arc::new(Provider::scripted(
                "test-model",
                Script::new().then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "exarch-agents `reply 'a'",
                )])),
            )));
        // Held for the whole test, and with no worker behind it, so it never
        // settles out from under the assertion below.
        let mut sibling = crate::agent::testkit::TestAgentSpec::new("already-there");
        sibling.parent = Some(desk.services.agent.clone());
        let _already_there = crate::agent::testkit::test_agent(&fleet, sibling)
            .expect("a fresh child of a live parent");

        let (live, _) = summary_counts(
            &with_parked(&desk, |desk, session| {
                desk.ask(start_req(session, "go", "helper", false))
            })
            .expect("the spawn must succeed"),
        );
        assert_eq!(
            live, 2,
            "the spawn counts the fleet's state, the sibling it did not start included"
        );
        let rows = listed(&desk);
        let mut names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["already-there", "helper", "parent"],
            "and `list names them, the reader among them"
        );

        let _ = wait_for_settle(&parent_inbox);
    }

    /// Naming neither half is an inheritance, not a decision: the child gets
    /// the parent's own `Arc<Provider>`, allocating nothing and asking the
    /// bureau nothing.
    #[test]
    fn an_inheriting_spawn_hands_the_child_the_parents_own_provider() {
        let desk = desk();
        let parent = desk.services.agent.current_provider();
        let child = desk
            .child_provider(&Selection::Inherit, &Selection::Inherit)
            .expect("an inheriting spawn resolves nothing");
        assert!(
            Arc::ptr_eq(&parent, &child),
            "the child must share the parent's provider, not a rebuild of it"
        );
    }

    /// A scripted session mints nothing, so a spawn that names a selection is
    /// refused saying so rather than silently inheriting.
    #[test]
    fn a_named_selection_under_a_scripted_bureau_is_refused() {
        let desk = desk();
        let Err(err) =
            desk.child_provider(&Selection::Inherit, &Selection::Named("other-model".into()))
        else {
            panic!("a scripted bureau must refuse to mint");
        };
        assert!(
            err.message.contains("scripted") && err.message.contains("mints no others"),
            "the refusal must say this session mints nothing, got: {}",
            err.message
        );
    }

    /// And it is refused before anything is adopted: no child is registered,
    /// exactly as a missing spec field leaves none.
    #[test]
    fn a_refused_selection_never_registers_a_child() {
        let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
        // Refused before any fork is taken up, so none need be parked.
        let err = desk
            .ask(start(
                ForkClaim::Parked(NurseryId(0)),
                Launch {
                    model: Selection::Named("other-model".into()),
                    ..spec("go", "helper", confined(), false)
                },
            ))
            .expect_err("a scripted bureau must refuse to mint");
        assert!(
            err.message.contains("`exarch-agents `start` refused"),
            "the refusal must name the tag it refused, got: {}",
            err.message
        );
        assert_eq!(
            summary(&desk.services.agent).live,
            0,
            "a refused selection must never register a child"
        );
    }

    /// A `` `start `` whose spec record `edit` has spoilt, as no door sends it.
    fn spoilt_start(edit: impl FnOnce(&mut Vec<(String, FOValue)>)) -> FOValue {
        let mut spec = spec("go", "scout", confined(), false).encode();
        let FOValue::Map { entries } = &mut spec else {
            unreachable!("a spec is a record")
        };
        edit(entries);
        let fork = ForkClaim::Parked(NurseryId(0)).encode();
        family_req(
            "agents",
            "start",
            Some(FOValue::Map {
                entries: vec![("spec".into(), spec), ("fork".into(), fork)],
            }),
        )
    }

    /// The desk reads the model's record by field name, so a missing field is
    /// named — never a position the model never wrote.
    #[test]
    fn start_refuses_a_spec_missing_a_field_by_name() {
        let err = desk()
            .handle(&spoilt_start(|spec| spec.retain(|(key, _)| key != "name")))
            .expect_err("a spec missing `name` must be refused");
        assert_eq!(
            err.message,
            "`exarch-agents `start`: `spec: no `name field in a record of 6 fields"
        );
    }

    /// A misspelt spec field is refused with the field it most likely meant.
    #[test]
    fn start_refuses_a_misspelt_spec_field() {
        let err = desk()
            .handle(&spoilt_start(|spec| spec[1].0 = "nmae".into()))
            .expect_err("`nmae` is not a field the spec carries");
        assert_eq!(
            err.message,
            "`exarch-agents `start`: `spec: unknown field `nmae — did you mean `name?"
        );
    }

    /// A request above the parent's ceiling is narrowed, never refused — the
    /// child's stack gains one more layer from [`crate::policy::base_layer`].
    /// Only that half is visible here: the clamped bit lands in a private
    /// `Agent` field, so `agent::build`'s fork test asserts the narrowing
    /// itself.
    #[test]
    fn agent_start_admits_a_search_request_above_the_parents_ceiling() {
        let (services, _fleet, parent_inbox) = services_with(3, |spec| spec.search = false);
        let desk = Arc::new(ExarchDesk { services });
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'done'",
            )])),
        ));
        desk.services.agent.provider_handle().swap(provider);

        let answer = with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "go", "searcher", true))
        });
        assert!(
            answer.is_ok(),
            "a spawn asking for more search reach than its parent holds is narrowed, not refused"
        );
        let _ = wait_for_settle(&parent_inbox);
    }

    /// A `` `restrict `` carrying anything but a record is refused by the
    /// desk's own decoder, before a base is named or a path is frozen.
    #[test]
    fn start_refuses_a_restriction_that_is_not_a_record() {
        let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
        let err = desk
            .ask(start(
                ForkClaim::Parked(NurseryId(0)),
                spec(
                    "go",
                    "helper",
                    SpawnGrant::Restrict(text("everything")),
                    false,
                ),
            ))
            .expect_err("a `restrict that carries no record at all");
        assert_eq!(
            err.message,
            "`exarch-agents `start`: `spec: `grant: `restrict must carry a capability record \
             [exec, fs, net, detach, editor, shell], got a Str"
        );
    }

    /// Read `system_prompt_bytes` off a session's opening bookend — the first
    /// record in its `record.jsonl`.
    fn recorded_system_prompt_bytes(log_dir: &std::path::Path) -> usize {
        let records = crate::record::read_records(&log_dir.join("record.jsonl")).unwrap();
        let first = records
            .into_iter()
            .next()
            .expect("record.jsonl must have at least one record");
        match first {
            crate::record::Record::Forensic(crate::record::Forensic::SessionStarted {
                system_prompt_bytes,
                ..
            }) => system_prompt_bytes,
            other => panic!("first record must be SessionStarted, got {other:?}"),
        }
    }

    /// The recorded length is the child's own resolved system prompt, not the
    /// raw `system_template` [`ExarchDesk::launch`] forks its log from.
    #[test]
    fn agent_start_bookend_records_the_childs_resolved_length() {
        let template = format!(
            "persona\n\n# Builtins\n\n{}",
            crate::prompt::BUILTIN_INDEX_PLACEHOLDER
        );
        // The production seam: the index table resolves from the same booted
        // surface the parked child shells fork from.
        let index = crate::prompt::BuiltinIndex::resolve(
            crate::bootstrap::test_shell()
                .builtin_names()
                .map(str::to_string)
                .collect(),
        );
        let (services, _fleet, parent_inbox) = services_with(3, |spec| {
            spec.system_base = template.clone();
            spec.index = index.clone();
        });
        let desk = Arc::new(ExarchDesk { services });
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'hi from child'",
            )])),
        ));
        desk.services.agent.provider_handle().swap(provider);

        let answer = with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "say hi", "helper", true))
        })
        .expect("a valid `start must succeed");

        let _ = summary_counts(&answer);
        let rows = listed(&desk);
        let child = rows
            .iter()
            .find(|row| row.name == "helper")
            .expect("the child stands on the listing");

        let expected = desk
            .services
            .agent
            .index()
            .apply(
                &template,
                &crate::prompt::Grants {
                    returns: true,
                    allow_schedule: desk.services.agent.allow_schedule,
                    spawns: desk.services.agent.fuel().saturating_sub(1) > 0,
                },
                "helper",
            )
            .len();
        assert_eq!(
            recorded_system_prompt_bytes(&child.log_dir),
            expected,
            "the bookend must record the spawned child's own resolved \
             system, not the unresolved template HostServices captured"
        );

        // Drain the settle, so no background thread outlives this test.
        let _ = wait_for_settle(&parent_inbox);
    }

    #[test]
    fn agent_start_refuses_at_zero_fuel_with_the_exhaustion_text() {
        let (desk, _fleet, _parent_inbox) = spawnable_desk(0);
        let (err, left_parked) = with_parked(&desk, |desk, session| {
            let answer = desk.ask(start_req(session, "hi", "helper", true));
            (answer, still_parked(desk, session))
        });
        let err = err.expect_err("zero fuel must refuse");
        assert!(
            err.message.contains("no spawn fuel remains"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("Fuel bounds how deep"),
            "must state that fuel bounds depth, not fan-out, got: {}",
            err.message
        );
        assert!(
            left_parked,
            "a fuel refusal happens before adopt, so the parked fork must \
             stay for the run guard to reap, never claimed by a refused call"
        );
    }

    /// [`Fleet::name_live`] catches the ordinary case before the parked
    /// fork is ever adopted, so the refused spawn registers no child.
    #[test]
    fn agent_start_refuses_a_name_already_borne_by_a_live_agent() {
        let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'a'",
            )])),
        ));
        desk.services.agent.provider_handle().swap(provider);

        let answer = with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "go", "helper", true))
        });
        assert!(answer.is_ok(), "the first spawn must succeed");

        let (err, left_parked) = with_parked(&desk, |desk, session| {
            let answer = desk.ask(start_req(session, "go again", "helper", true));
            (answer, still_parked(desk, session))
        });
        let err = err.expect_err("a second spawn naming a live agent must be refused");
        assert!(
            err.message.contains("already bears the name 'helper'"),
            "got: {}",
            err.message
        );
        assert!(
            left_parked,
            "a name-collision refusal happens before adopt, so the parked \
             fork must stay for the run guard to reap, never claimed by a \
             refused call"
        );
        assert_eq!(
            summary(&desk.services.agent).live,
            1,
            "the second, refused spawn must leave no child behind"
        );
    }

    /// The in-process door checks a name, but a wire peer need not have come
    /// through one — so the spawn spine checks it too, before a log is forked
    /// or a listener dialled, rather than leaving it all to the enrolment.
    #[test]
    fn agent_start_refuses_a_malformed_name_before_it_adopts_the_fork() {
        let (desk, _fleet, _parent_inbox) = spawnable_desk(3);

        let (err, left_parked) = with_parked(&desk, |desk, session| {
            let answer = desk.ask(start_req(session, "go", "help/er", true));
            (answer, still_parked(desk, session))
        });
        let err = err.expect_err("a malformed name must be refused");
        assert!(
            err.message.contains("ASCII letters"),
            "the refusal must carry the name rule; got: {}",
            err.message
        );
        assert!(
            left_parked,
            "the name is refused before adopt, so the parked fork must stay \
             for the run guard to reap"
        );
        assert_eq!(
            summary(&desk.services.agent).live,
            0,
            "a refused spawn leaves no child behind"
        );
    }

    /// Siblings in one turn all succeed off the same captured fuel, since the
    /// parent's own is never debited.
    #[test]
    fn fuel_bounds_depth_not_fanout() {
        let (desk, _fleet, parent_inbox) = spawnable_desk(1);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "exarch-agents `reply 'a'",
                )]))
                .then(Reply::tool_calls(vec![ral_call(
                    "r2",
                    "exarch-agents `reply 'b'",
                )]))
                .then(Reply::tool_calls(vec![ral_call(
                    "r3",
                    "exarch-agents `reply 'c'",
                )])),
        ));
        desk.services.agent.provider_handle().swap(provider);

        for i in 0..3 {
            let answer = with_parked(&desk, move |desk, session| {
                desk.ask(start_req(session, "go", &format!("t{i}"), true))
            });
            assert!(
                answer.is_ok(),
                "sibling {i} must not be refused for lack of fuel — fuel \
                 bounds depth, not fan-out"
            );
        }

        for _ in 0..3 {
            match wait_for_settle(&parent_inbox) {
                crate::bus::Item::Agent(result) => assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                    "every sibling must settle by replying, got: {:?}",
                    result.outcome
                ),
                other => panic!("expected an Agent result item, got {other:?}"),
            }
        }
    }

    /// The capture went stale the instant the *calling* session's inbox
    /// epoch moved past what was snapshotted at install.
    #[test]
    fn agent_start_refuses_after_clear() {
        let (desk, _fleet, parent_inbox) = spawnable_desk(3);
        parent_inbox.clear(); // the /clear gesture, on this caller
        let err = with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "hi", "helper", true))
        })
        .expect_err("a stale epoch must refuse");
        assert!(
            err.message.contains("cleared"),
            "must name the /clear cause, got: {}",
            err.message
        );
    }

    /// A refusal at adoption — here, a restriction record the capability
    /// decoder will not read, which only the engine's own freeze discovers —
    /// drops the adopted fork rather than leaving it to be adopted twice.
    #[test]
    fn refused_enquiry_leaves_the_nursery_empty() {
        let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
        let (err, left_parked) = with_parked(&desk, |desk, session| {
            let restriction = FOValue::Map {
                entries: vec![("net".to_string(), text("yes"))],
            };
            let answer = desk.ask(start(
                ForkClaim::Parked(session),
                spec("go", "helper", SpawnGrant::Restrict(restriction), false),
            ));
            (answer, still_parked(desk, session))
        });
        let err = err.expect_err("a `net axis that is not a Bool must refuse");
        assert!(
            err.message.contains("net"),
            "must name the axis it could not read, got: {}",
            err.message
        );
        assert!(
            !left_parked,
            "a refusal downstream of adopt must not leave the fork \
             re-adoptable — it was already claimed and simply drops"
        );
    }

    /// `` `cancel `` may only reach what this agent started; `` `message ``
    /// reaches any live agent but this one.
    #[test]
    fn cancel_scopes_to_descendants_and_message_does_not() {
        let (services, fleet, _root_inbox) = services_with(3, |_| {});
        let desk_root = ExarchDesk { services };
        // root -> mid -> grandchild, and root -> sibling (mid's sibling).
        let under = |name: &str, parent: &Arc<Agent>| {
            let mut spec = crate::agent::testkit::TestAgentSpec::new(name);
            spec.parent = Some(parent.clone());
            crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh child of a live parent")
        };
        let root = desk_root.services.agent.clone();
        let mid = under("mid", &root);
        let _sibling = under("sibling", &root);
        let _grandchild = under("grandchild", &mid);

        let mut desk1 = desk_root;
        desk1.services.agent = mid;

        for who in ["sibling", "parent", "grandchild"] {
            assert!(
                desk1.ask(message_req(who, "hi")).is_ok(),
                "a message must reach {who}, whichever way across the tree it runs"
            );
        }

        let err = desk1
            .ask(message_req("mid", "hi"))
            .expect_err("a message to oneself must be refused");
        assert_eq!(
            err.message,
            "agent 'mid' is you; `exarch-agents `message` reaches another agent — to wake yourself, arm a `exarch-schedules` fire"
        );

        let cancel_err = desk1
            .ask(Request::Agents(Agents::Cancel("parent".into())))
            .expect_err("cancelling an ancestor must be refused");
        assert_eq!(
            cancel_err.message,
            "agent 'parent' is not an agent you started; `exarch-agents `cancel` may only reach a descendant of yours"
        );

        assert!(
            desk1
                .ask(Request::Agents(Agents::Cancel("grandchild".into())))
                .is_ok(),
            "cancelling a proper descendant must succeed"
        );
    }

    /// The read-after-write law once more, through a real spawn: a surfaced
    /// value must render before an enquiry raised after it in the same run.
    #[test]
    fn surface_then_spawn_observes_the_surface_first() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, rx) = channel();
        let emit = Emitter::new(tx, session.agent.id);
        let _ = session.ral(r#"exarch-pins `clear "test-marker"; exarch-agents `start [prompt: #'go'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `inherit]"#,
            5,
            &emit,
        );
        drop(emit);

        let mut saw_unpin = false;
        let mut saw_spawn = false;
        while let Ok(sig) = rx.try_recv() {
            match sig {
                Signal::Transient(_, Transient::Unpin { .. }) => saw_unpin = true,
                Signal::Fact(_, fact) => {
                    if let Record::Display(Display::HarnessCall { verb, .. }) = fact.value()
                        && verb == "spawn"
                    {
                        assert!(
                            saw_unpin,
                            "the surfaced unpin must be observed before the spawn's HarnessCall line"
                        );
                        saw_spawn = true;
                    }
                }
                Signal::Transient(..) => {}
            }
        }
        assert!(
            saw_spawn,
            "the agent spawn's HarnessCall chrome must have been emitted"
        );
    }

    /// Refused before anything is armed, naming the flag that grants — and
    /// naming the tag the model typed, not a wire word it never saw.
    #[test]
    fn every_schedule_tag_is_refused_in_the_models_own_vocabulary() {
        for (request, verb) in [
            (add_req("1s", "nightly", "wake"), "exarch-schedules `add"),
            (
                Request::Schedules(Schedules::List),
                "exarch-schedules `list",
            ),
            (remove_req("sched-0"), "exarch-schedules `remove"),
        ] {
            let err = desk()
                .ask(request)
                .expect_err("every schedule tag is refused without the grant");
            assert!(
                err.message.starts_with(&format!("`{verb}` refused:")),
                "the refusal must name the tag the model typed, got: {}",
                err.message
            );
            assert!(
                err.message.contains("--allow-schedule"),
                "must name the grant flag, got: {}",
                err.message
            );
        }
    }

    /// A schedule with no label is refused, naming the missing field, and
    /// registers nothing. The field is missing by *name*: the desk reads the
    /// model's record the way the door wrote it.
    #[test]
    fn schedule_without_a_label_is_refused() {
        let desk = granted_desk();
        let err = desk
            .handle(&family_req(
                "schedules",
                "add",
                Some(FOValue::Map {
                    entries: vec![
                        ("trigger".to_string(), tag("after", Some(text("1s")))),
                        ("prompt".to_string(), text("wake")),
                    ],
                }),
            ))
            .expect_err("a schedule with no label must be refused");
        assert!(
            err.message.contains("no `label field"),
            "must name the missing field, got: {}",
            err.message
        );
        assert!(
            desk.services.agent.schedules.list().is_empty(),
            "the refused attempt registers nothing"
        );
    }

    /// Arm one, see the answer already list it with the fields it was given,
    /// remove it by label, see the table empty again — every tag answers the
    /// table, so no tag needs a second call to learn what it did.
    #[test]
    fn schedules_lists_what_add_registered() {
        let desk = granted_desk();
        let rows = table(
            desk.ask(add_req("2h", "nightly", "wake"))
                .expect("a valid `add must succeed"),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(str_field(&rows[0], "label"), Some("nightly"));
        assert_eq!(str_field(&rows[0], "trigger"), Some("after 2h"));
        assert!(rows[0].field("id").is_none(), "the table carries no id");

        let listed = table(
            desk.ask(Request::Schedules(Schedules::List))
                .expect("`list must succeed"),
        );
        assert_eq!(listed.len(), 1, "`list sees what `add armed");

        let after_removal = table(
            desk.ask(remove_req("nightly"))
                .expect("`remove must succeed"),
        );
        assert!(
            after_removal.is_empty(),
            "the wakeup must be gone from the very table `remove answers"
        );
    }

    /// Refused before a second schedule is registered.
    #[test]
    fn schedule_at_the_desk_refuses_a_duplicate_label() {
        let desk = granted_desk();
        desk.ask(add_req("1s", "nightly", "wake"))
            .expect("the first schedule must succeed");
        let err = desk
            .ask(add_req("1s", "nightly", "wake"))
            .expect_err("a duplicate label must be refused");
        assert!(err.message.contains("nightly"), "got: {}", err.message);
        assert_eq!(
            desk.services.agent.schedules.list().len(),
            1,
            "the duplicate registers nothing"
        );
    }

    /// The row goes up after the registry call precisely so `failed` can carry
    /// the outcome; emitted before, every schedule would read as one that
    /// landed.
    #[test]
    fn a_refused_schedule_tiers_its_act_row() {
        let (tx, rx) = channel();
        let mut desk = granted_desk();
        desk.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        desk.services.emit = Emitter::new(tx, 0);
        desk.ask(add_req("1s", "nightly", "wake"))
            .expect("the first schedule must succeed");
        desk.ask(add_req("1s", "nightly", "wake"))
            .expect_err("a duplicate label must be refused");

        let acts: Vec<(String, bool)> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(Display::HarnessCall {
                    verb,
                    subject,
                    payload,
                    failed,
                }) if verb == "schedule" => {
                    assert_eq!(subject.as_deref(), Some("nightly"));
                    Some((payload, failed))
                }
                _ => None,
            })
            .collect();
        assert_eq!(acts.len(), 2, "both attempts draw a row: {acts:?}");
        assert!(!acts[0].1, "the schedule that landed is not tiered");
        assert_eq!(acts[0].0, "after 1s", "a landed row carries its trigger");
        assert!(acts[1].1, "the refused schedule is tiered hot");
        assert!(
            acts[1].0.starts_with("refused: "),
            "a refusal states itself in the payload: {:?}",
            acts[1].0
        );
    }

    // ── `reply` ───────────────────────────────────────────────────────────

    /// Refused before the payload is decoded, and the cell is left empty.
    #[test]
    fn reply_refused_without_returns() {
        let (emit, _rx) = crate::bus::dummy_emitter();
        let (services, _fleet, _inbox) = services_with(3, |spec| spec.returns = false);
        let mut d = ExarchDesk { services };
        d.services.emit = emit;
        let err = d
            .ask(reply_req(FOValue::Int { value: 1 }))
            .expect_err("a non-returning agent's reply must be refused");
        assert!(
            err.message
                .contains("you converse with the user; you do not return"),
            "got: {}",
            err.message
        );
        assert!(
            d.services.reply.take().is_none(),
            "a refused reply must never reach the cell"
        );
    }

    /// Each call also puts a subject-less act on the rail, carrying its value.
    #[test]
    fn reply_stages_the_payload_last_write_wins() {
        let (tx, rx) = channel();
        let mut d = desk();
        d.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        d.services.emit = Emitter::new(tx, 0);

        for text in ["first", "second"] {
            d.ask(reply_req(FOValue::String { value: text.into() }))
                .expect("a returning agent's reply must succeed");
        }

        assert_eq!(
            d.services.reply.take(),
            Some(FOValue::String {
                value: "second".into()
            }),
            "the last staged reply wins"
        );

        let acts: Vec<String> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(Display::HarnessCall {
                    verb,
                    subject,
                    payload,
                    failed,
                }) if verb == "reply" => {
                    assert_eq!(subject, None, "`reply` addresses no named subject");
                    assert!(!failed, "a staged reply is not a refusal");
                    Some(payload)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            acts,
            ["first", "second"],
            "each reply emits its own act row, carrying the value it staged"
        );
    }

    /// The audit is the model's only account of what survived an unwind, so it
    /// must name every committed act, in the order the call committed them.
    #[test]
    fn the_fragment_keeps_committed_acts_in_the_order_they_landed() {
        let desk = granted_desk();
        desk.ask(add_req("2h", "nightly", "wake"))
            .expect("a valid schedule must succeed");
        desk.ask(remove_req("nightly"))
            .expect("unscheduling the label just armed must remove it");
        desk.ask(reply_req(FOValue::String {
            value: "done".into(),
        }))
        .expect("a returning agent's reply must succeed");

        assert_eq!(
            desk.services.acts.audit().as_deref(),
            Some(
                "audit: this call had already armed the wakeup 'nightly'; removed the wakeup \
                 'nightly'; staged your reply; that work stands — do not repeat it.\n"
            )
        );
    }

    /// A refused act changed nothing, so the fragment stays silent: an entry
    /// here would tell the model to leave standing work it never did.
    #[test]
    fn a_refused_act_leaves_the_fragment_empty() {
        let desk = desk();
        desk.ask(message_req("nobody", "hi"))
            .expect_err("a message to an unknown name must be refused");
        assert!(
            desk.services.acts.audit().is_none(),
            "a call that committed nothing owes the model no audit"
        );
    }

    /// The rail parity a seventh act cannot break by construction: a landed
    /// and a refused attempt both draw their row, but only the landed one
    /// reaches the fragment.
    #[test]
    fn rail_draws_every_attempt_the_fragment_holds_only_what_landed() {
        let (tx, rx) = channel();
        let mut desk = granted_desk();
        desk.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        desk.services.emit = Emitter::new(tx, 0);

        desk.ask(add_req("1s", "nightly", "wake"))
            .expect("a valid schedule must land");
        desk.ask(message_req("nobody", "hi"))
            .expect_err("a message to an unknown name must be refused");

        let rows: Vec<(String, bool)> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(Display::HarnessCall { verb, failed, .. }) => Some((verb, failed)),
                _ => None,
            })
            .collect();
        assert_eq!(
            rows.iter()
                .map(|(v, f)| (v.as_str(), *f))
                .collect::<Vec<_>>(),
            vec![("schedule", false), ("message", true)],
            "the rail draws one row per attempt, landed or refused"
        );

        let audit = desk
            .services
            .acts
            .audit()
            .expect("the landed schedule owes an audit");
        assert!(
            audit.contains("armed the wakeup") && !audit.contains("message"),
            "the fragment carries the landed schedule alone, got: {audit}"
        );
    }

    // ── engaged-child lifecycle ────────────────────────────────────────────

    /// Attend `child` to completion on a detached thread, in `spawn_async`'s own
    /// worker-epilogue order: `settle` delivers only a non-reply outcome — a
    /// `` `reply ``'s notice already rode `attend`'s own deposit — before
    /// retiring it.
    fn attend_and_deliver(mut child: Avatar) -> std::thread::JoinHandle<()> {
        let id = child.agent.id;
        std::thread::spawn(move || {
            let (tx, _rx) = crate::bus::channel();
            let emit = Emitter::new(tx, id);
            let (outcome, _payload) = child.attend(&mut crate::agent::NoControl, &emit);
            child.settle(outcome);
        })
    }

    /// A live, never-attended child of `parent`: just enough for
    /// [`Agent::has_busy_children`] to read true, so `parent` parks
    /// [`crate::bus::ParkMode::HeldByChildren`] instead of quiescing.  The
    /// caller must hold what comes back — that is what keeps it live.
    fn keepalive(fleet: &Arc<Fleet>, parent: &Arc<Agent>) -> Arc<Agent> {
        let mut spec =
            crate::agent::testkit::TestAgentSpec::new(&format!("keepalive-{}", parent.id));
        spec.parent = Some(parent.clone());
        crate::agent::testkit::test_agent(fleet, spec).expect("a fresh child of a live parent")
    }

    /// Poll `path` until it contains `needle` or `timeout` elapses. A child's
    /// `record.jsonl` records every turn, and it is the only channel into a
    /// thread the test does not otherwise touch mid-flight.
    fn eventually_logged(path: &std::path::Path, needle: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Ok(body) = std::fs::read_to_string(path)
                && body.contains(needle)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// The lifecycle turns on a steer's delivery alone and knows nothing of TUI
    /// focus. The keepalive grandchild stops the child quiescing before it is
    /// engaged, so both steers land instead of racing its loop.
    #[test]
    fn engaged_child_answers_a_second_steer_with_no_focus_involved() {
        let parent = Avatar::for_test("system").unwrap();
        let child = parent.fork_named("helper").expect("fork child");
        child.provider_handle().swap(Arc::new(Provider::scripted(
            "test-model",
            Script::new()
                .then(Reply::text("first response, no reply yet"))
                .then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "exarch-agents `reply 'second response arrived'",
                )])),
        )));
        child.seed("say hi".into());
        let log_dir = child.log_dir();
        let child_agent = child.agent.clone();
        let _keepalive = keepalive(&parent.fleet, &child_agent);
        let handle = attend_and_deliver(child);

        child_agent.mailbox().steer("first message".into());
        assert!(child_agent.engaged(), "steer renews the exchange clock");
        assert!(
            eventually_logged(
                &log_dir.join("record.jsonl"),
                "first response, no reply yet",
                Duration::from_secs(5),
            ),
            "the child must answer the first steer before the second is sent"
        );

        child_agent.mailbox().steer("second message".into());
        match wait_for_settle(&parent.inbox()) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                    "the second steer's reply must notify the parent, got: {:?}",
                    result.outcome
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
        assert_eq!(
            parent
                .agent
                .descendant(&child_agent)
                .and_then(|c| c.reply()),
            Some(FOValue::String {
                value: "second response arrived".into()
            }),
            "the reply the second steer produced must be the one deposited"
        );
        // A replied child parks for a follow-up; only a terminate ends it.
        child_agent.cancel_tree(ral_core::process::CancelCause::Explicit);
        handle.join().expect("worker thread must not panic");
    }

    /// The reaped child delivers exactly one
    /// [`crate::bus::AgentOutcome::Cancelled`] to the parent inbox.
    #[test]
    fn ms_lease_child_never_renewed_is_cancelled() {
        // The ttl must expire well inside the round-trip loop's own
        // `MAX_TURNS` cap, so the lease and not that cap ends the exchange.
        let ttl = Duration::from_millis(25);
        let parent = Avatar::for_test_with(crate::agent::TestTrunk {
            lease: ttl,
            ..crate::agent::TestTrunk::new("system")
        })
        .unwrap();
        let child = parent.fork_named("child-a").expect("fork child a");
        let mut long_script = Script::new();
        for i in 0..2_000u32 {
            long_script = long_script.then(Reply::tool_calls(vec![ral_call(&i.to_string(), "1")]));
        }
        child
            .provider_handle()
            .swap(Arc::new(Provider::scripted("test-model", long_script)));
        child.seed("go".into());
        let handle = attend_and_deliver(child);

        match wait_for_settle(&parent.inbox()) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Cancelled),
                    "a never-renewed lease reaps mid-exchange with Cancelled, got {:?}",
                    result.outcome
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
        handle.join().expect("worker thread must not panic");
        assert_eq!(
            crate::fleet::roster::summary(&parent.agent).live,
            0,
            "the reaped child settles, and the walk that looked for it pruned it"
        );
    }

    #[test]
    fn ms_lease_child_renewed_at_half_the_ttl_survives_the_bound() {
        // Generous, because the renewal is paced by `thread::sleep` on the test
        // thread, where jitter stretches a short sleep past nominal.
        let ttl = Duration::from_secs(1);
        let parent = Avatar::for_test_with(crate::agent::TestTrunk {
            lease: ttl,
            ..crate::agent::TestTrunk::new("system")
        })
        .unwrap();
        let fleet = parent.fleet.clone();
        // Parked by a keepalive grandchild and given no script to race, so the
        // renewal alone must be what defers its reap.
        let child = parent.fork_named("child-b").expect("fork child b");
        let agent = child.agent.clone();
        let keepalive = keepalive(&fleet, &agent);
        let handle = attend_and_deliver(child);

        std::thread::sleep(ttl / 2);
        // A bare stamp, not a steer: the script is empty, so an actual
        // delivery would give the attend loop work it cannot answer.
        agent.mailbox().stamp_exchange();
        keepalive.mailbox().stamp_exchange();

        std::thread::sleep(ttl / 2 + Duration::from_millis(150));
        assert!(
            !agent.cancel_token().is_cancelled(),
            "renewed at half the ttl, still alive past the original bound"
        );

        // Wind the child down rather than leave its thread parked forever: per
        // `ParkMode`, a terminate-cause cancel ends an unengaged park at once.
        agent.cancel_tree(ral_core::process::CancelCause::Explicit);
        let _ = wait_for_settle(&parent.inbox());
        handle.join().expect("worker thread must not panic");
    }
}

/// The desk's wire arm: `` `start `` dialling a guest that is listening for
/// exactly one connection, over a fake dialler and a re-exec'd `--engine`
/// child standing in for the hatched guest — the same vehicle the wire seat's
/// own tests already use, since a genuine vsock dial only means anything
/// inside a real guest.
#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod wire_tests {
    use super::tests::{confined, message_req, spec, start};
    use super::*;
    use crate::agent::cancel::InterruptTarget;
    use crate::agent::event::AgentLog;
    use crate::bus::Inbox;
    use ral_core::types::NurseryId;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex as StdMutex;

    /// Each log takes the next session id, so two of them are never the same
    /// session to the wire.
    fn fresh_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        AgentLog::for_test(n, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    /// Re-exec this binary as a bare `--engine` child holding `guest` on fd 3,
    /// exactly [`crate::agent::seat::tests::spawn_engine`]'s vehicle. The
    /// caller drops its own copy afterwards, as `hatch_over` does, so the
    /// child's death reads as EOF on the host's end.
    fn spawn_engine_on(guest: &UnixStream) -> std::process::Child {
        let guest_fd = guest.as_raw_fd();
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("current exe"));
        cmd.arg("--engine");
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        // SAFETY: runs between fork and exec, calling only async-signal-safe
        // `dup2`/`close`, with no allocation and no locking.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::dup2(guest_fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if guest_fd != 3 {
                    libc::close(guest_fd);
                }
                Ok(())
            });
        }
        cmd.spawn().expect("spawn engine child")
    }

    /// What the guest at the far end of a [`FakeDial`] does with the dial.
    enum Guest {
        /// The whole spine: read the token, spawn the child, then ack — the
        /// order the guest's hatch in `ral_core::hatch` keeps, since the ack is the
        /// claim that the child exists.
        Hatches,
        /// A guest-side hatch that failed: read the token and close, saying
        /// nothing. The host's ack read sees EOF.
        ClosesWithoutAcking,
        /// Nothing is listening on that port.
        Refuses(String),
    }

    /// A fake [`crate::agent::Dial`] that plays the guest. `dial` hands back
    /// one end of a socketpair and leaves a thread on the other end doing
    /// whatever this fake's [`Guest`] says, so the desk under test drives a
    /// real handshake against a real peer.
    struct FakeDial {
        guest: Guest,
        /// Every port `dial` was asked for, in order.
        ports: StdMutex<Vec<u32>>,
        /// Every token a guest thread actually read off the wire.
        tokens: Arc<StdMutex<Vec<u64>>>,
        /// Engine children the guest threads spawned, for the test to reap.
        children: Arc<StdMutex<Vec<std::process::Child>>>,
    }

    impl FakeDial {
        fn new(guest: Guest) -> Arc<Self> {
            Arc::new(Self {
                guest,
                ports: StdMutex::new(Vec::new()),
                tokens: Arc::new(StdMutex::new(Vec::new())),
                children: Arc::new(StdMutex::new(Vec::new())),
            })
        }

        fn ports(&self) -> Vec<u32> {
            self.ports.lock().unwrap().clone()
        }
    }

    impl crate::agent::Dial for FakeDial {
        fn dial(&self, port: u32) -> Result<ral_core::wire::WireStream, String> {
            self.ports.lock().unwrap().push(port);
            let hatches = match &self.guest {
                Guest::Refuses(reason) => return Err(reason.clone()),
                Guest::Hatches => true,
                Guest::ClosesWithoutAcking => false,
            };
            let (host, guest) =
                UnixStream::pair().map_err(|e| format!("socketpair for the fake dial: {e}"))?;
            let tokens = self.tokens.clone();
            let children = self.children.clone();
            std::thread::spawn(move || {
                let mut guest = guest;
                let mut claim = [0u8; 8];
                if guest.read_exact(&mut claim).is_err() {
                    return;
                }
                tokens.lock().unwrap().push(u64::from_le_bytes(claim));
                if !hatches {
                    return;
                }
                let child = spawn_engine_on(&guest);
                guest
                    .write_all(&[ral_core::protocol::HATCH_ACK])
                    .expect("ack the hatch");
                children.lock().unwrap().push(child);
            });
            Ok(host)
        }
    }

    /// A wire reach with a genuine `ControlSender` behind it — a disposable
    /// `--engine` child adopted and killed at once, since these fixtures need
    /// a real reach value's *shape* but never actually cancel or interrupt
    /// through it.
    fn fake_wire_reach() -> InterruptTarget {
        let (host, guest) = UnixStream::pair().expect("socketpair standing in for the dial");
        let mut child = spawn_engine_on(&guest);
        drop(guest);
        let transport =
            ral_core::protocol::WireTransport::adopt(host, ral_core::protocol::Liveness::default())
                .expect("adopt host stream");
        let control = ral_core::protocol::Transport::control(&transport).clone();
        let _ = child.kill();
        let _ = child.wait();
        InterruptTarget::new(control)
    }

    /// A wire-seat desk fixture whose parent holds the very inbox this
    /// returns, exactly [`super::tests::spawnable_desk`]'s identity shape with
    /// `kind: SeatKind::Wire` and a dialler installed.
    fn wire_spawnable_desk(fuel: u32, dial: Arc<FakeDial>) -> (ExarchDesk, Arc<Fleet>, Inbox) {
        let parent_inbox = Inbox::new();
        let fleet = Fleet::new();
        let mut spec = crate::agent::testkit::TestAgentSpec::new("parent");
        spec.reach = fake_wire_reach();
        spec.mailbox = parent_inbox.mailbox();
        spec.fuel = fuel;
        spec.returns = true;
        spec.search = true;
        spec.dial = Some(dial);
        let agent =
            crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh fleet's wire trunk");
        let (emit, _rx) = crate::bus::dummy_emitter();
        let desk = ExarchDesk {
            services: HostServices {
                fleet: fleet.clone(),
                kind: SeatKind::Wire,
                stamp: agent.mailbox().stamp(),
                agent,
                emit,
                cwd: PathBuf::from("/work"),
                home: Some(PathBuf::from("/tmp")),
                reply: ReplyCell::default(),
                log: LogCell::new(fresh_log()),
                branch: None,
                acts: ActFragment::default(),
                principal: ral_core::host::user(),
            },
        };
        (desk, fleet, parent_inbox)
    }

    /// `` `exarch-agents `start `` as a listening engine sends it: the port its
    /// listener is bound to, and the token that listener will check the
    /// host's dial against.
    fn wire_start_req(name: &str, port: u32, token: u64) -> Request {
        start(
            ForkClaim::Listening { port, token },
            spec("go", name, confined(), false),
        )
    }

    /// A guest whose own hatch failed closes without acking. The host has a
    /// connection and no child, and must say so rather than register one.
    #[test]
    fn wire_spawn_refuses_when_the_guest_closes_without_acking() {
        let dial = FakeDial::new(Guest::ClosesWithoutAcking);
        let (desk, _fleet, _parent_inbox) = wire_spawnable_desk(3, dial);

        let err = desk
            .ask(wire_start_req("unacked", 41_731, 7))
            .expect_err("an unacknowledged hatch must be refused");
        assert!(
            err.message
                .contains("the guest closed the connection before acknowledging the hatch"),
            "got: {}",
            err.message
        );
        assert_eq!(
            summary(&desk.services.agent).live,
            0,
            "a hatch that was never acknowledged names no child on the roster"
        );
    }

    /// A dial the guest refuses — nothing bound on that port — is refused
    /// with a sentence naming the dial, so the builtin's own listener thread
    /// learns why it was never reached.
    #[test]
    fn wire_spawn_refuses_naming_the_dial_when_the_guest_refuses_it() {
        let dial = FakeDial::new(Guest::Refuses("connection reset by peer".to_string()));
        let (desk, _fleet, _parent_inbox) = wire_spawnable_desk(3, dial);

        let err = desk
            .ask(wire_start_req("never-reached", 41_731, 7))
            .expect_err("a refused dial must be refused");
        assert!(
            err.message.contains("could not dial") && err.message.contains("41731"),
            "must name the dial and the port, got: {}",
            err.message
        );
    }

    /// The identity tag on a wire desk is a guest claiming to be in process.
    /// Refused, and before anything is dialled.
    #[test]
    fn wire_spawn_refuses_a_parked_fork_without_dialling() {
        let dial = FakeDial::new(Guest::Hatches);
        let (desk, _fleet, _parent_inbox) = wire_spawnable_desk(3, dial.clone());

        let err = desk
            .ask(super::tests::start_req(
                NurseryId(0),
                "go",
                "in-process",
                false,
            ))
            .expect_err("a wire desk has no nursery to adopt a parked fork from");
        assert!(
            err.message.contains("`listening [port, token]`"),
            "must name the tag this host does take, got: {}",
            err.message
        );
        assert!(
            dial.ports().is_empty(),
            "a fork this desk cannot reach is refused before any dial"
        );
    }

    /// The peer-messaging pin: one identity-reach and one wire-reach child of
    /// the same parent exchange marked notes through the desk's `message`
    /// handler, which touches only the tree and a mailbox — never a seat — so
    /// sender and recipient never learn each other's transport.
    #[test]
    fn identity_and_wire_peers_exchange_messages_through_one_desk() {
        let fleet = Fleet::new();
        let mut parent_spec = crate::agent::testkit::TestAgentSpec::new("parent");
        parent_spec.fuel = 3;
        parent_spec.returns = true;
        parent_spec.search = true;
        let parent =
            crate::agent::testkit::test_agent(&fleet, parent_spec).expect("a fresh fleet's trunk");

        let identity_inbox = Inbox::new();
        let mut identity = crate::agent::testkit::TestAgentSpec::new("identity-peer");
        identity.parent = Some(parent.clone());
        identity.mailbox = identity_inbox.mailbox();
        let _identity_peer = crate::agent::testkit::test_agent(&fleet, identity)
            .expect("a fresh child of a live parent");

        let wire_inbox = Inbox::new();
        let mut wire = crate::agent::testkit::TestAgentSpec::new("wire-peer");
        wire.parent = Some(parent.clone());
        wire.reach = fake_wire_reach();
        wire.mailbox = wire_inbox.mailbox();
        let _wire_peer = crate::agent::testkit::test_agent(&fleet, wire)
            .expect("a fresh child of a live parent");

        let (emit, _rx) = crate::bus::dummy_emitter();
        let desk = ExarchDesk {
            services: HostServices {
                fleet,
                // Neither peer's transport matters to `message`, which
                // touches only the tree and a mailbox.
                kind: SeatKind::Wire,
                stamp: parent.mailbox().stamp(),
                agent: parent,
                emit,
                cwd: PathBuf::from("/"),
                home: Some(PathBuf::from("/tmp")),
                reply: ReplyCell::default(),
                log: LogCell::new(fresh_log()),
                branch: None,
                acts: ActFragment::default(),
                principal: ral_core::host::user(),
            },
        };

        desk.ask(message_req("identity-peer", "note for identity"))
            .expect("the parent may message its identity-reach descendant");
        desk.ask(message_req("wire-peer", "note for wire"))
            .expect("the parent may message its wire-reach descendant");

        match identity_inbox.next_item() {
            Some(crate::bus::Item::Message(m)) => {
                assert_eq!(m.text, "note for identity");
            }
            other => panic!("expected an AgentMessage item, got {other:?}"),
        }
        match wire_inbox.next_item() {
            Some(crate::bus::Item::Message(m)) => {
                assert_eq!(m.text, "note for wire");
            }
            other => panic!("expected an AgentMessage item, got {other:?}"),
        }
    }
}
