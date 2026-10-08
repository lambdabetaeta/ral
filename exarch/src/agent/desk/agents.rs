//! The agents family of the desk.

use crate::agent::roster::summary;
use crate::agent::seat::{Seat, SeatKind};
use crate::agent::{Avatar, Build, ProviderHandle};
use crate::enquiry::{Deposit, ForkClaim, Memory, Message, Name, Selection, Start};
use crate::provider::Provider;
use crate::shell_eval::{self};
use ral_core::SpawnGrant;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::sync::LockExt;
use ral_core::types::Error;
use std::path::PathBuf;
use std::sync::Arc;

use super::{DeskAct, ExarchDesk};

/// The two words that bracket one hatch, in opposite directions: the host
/// writes the eight token bytes the guest's listener is waiting for, and reads
/// back the single byte that listener writes only once the child's `spawn` has
/// returned. Both precede the first frame; neither is one.
///
/// The clock is the transport's own, so a dial carries no second deadline —
/// and it is lifted again before the stream is adopted, since the reader
/// thread must then park in `read_frame` for as long as the child lives.
fn greet_hatch(
    stream: &mut ral_core::protocol::channel::WireStream,
    token: u64,
) -> Result<(), String> {
    use std::io::{Read, Write};
    let patience = ral_core::protocol::channel::Liveness::default().deadline;
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
            "the guest answered the hatch with byte {:#04x} rather than the acknowledgement: \
             whatever is listening on that port is not a ral engine waiting to be hatched",
            ack[0]
        ));
    }
    stream
        .set_read_timeout(None)
        .map_err(|e| format!("could not lift the hatch deadline from the wire: {e}"))
}

impl ExarchDesk {
    /// The spawn spine behind `` `start ``: take up the fork, fork its log off
    /// the parent's, assemble it at one less unit of fuel, hand it to
    /// `spawn_async`. Every cheap guard runs before the fork is taken up; a
    /// refusal after it simply drops the child's engine.
    pub(super) fn launch(&self, Start { spec, fork }: Start) -> Result<FOValue, Error> {
        let s = &self.services;
        let Name(name) = spec.name;

        // The captured fuel, caps, and grant are only as fresh as the
        // envelope they were snapshotted under — this caller's own, so a
        // `/clear` in another tab does not refuse a spawn here.
        if s.stamp.is_stale() {
            return Err(Error::new(
                "`exarch-agents `start` refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale: \
                 issue agent again on your next turn",
            ));
        }

        // Fuel bounds depth, not fan-out: the parent's own is never debited, so
        // siblings are free and only a deep enough chain bottoms out.
        if s.agent.fuel == 0 {
            return Err(Error::new(
                "`exarch-agents `start` refused: no spawn fuel remains at this depth, so you cannot \
                 delegate any further here. Fuel bounds how deep a chain of spawns may \
                 recurse, never how many children you may start at any one depth: starting \
                 several agents here costs nothing extra. `exarch-agents `cancel` on any node stops \
                 its whole live subtree regardless of depth.",
            ));
        }

        // Didactic, not race-free: `register` below re-checks under its own
        // lock, and that is what closes a same-name race.
        if s.fleet.name_live(&name) {
            return Err(Error::new(format!(
                "`exarch-agents `start` refused: a live agent already bears the name '{name}'; pick \
                     another, or wait for it to settle. Names identify live agents; agents \
                     lists yours."
            )));
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
            s.agent.search && spec.search,
            spec.memory == Memory::Mnemon,
        )?;
        self.spawn_child(child, name, spec.prompt)
    }

    /// The child's provider: the parent's own `Arc` verbatim when the
    /// selection resolves to the parent's own pair, and otherwise one minted
    /// by [`Bureau::reselect`](crate::provider::Bureau::reselect), which
    /// refuses a model the account does not list.
    pub(super) fn child_provider(
        &self,
        provider: &Selection,
        model: &Selection,
    ) -> Result<Arc<Provider>, Error> {
        let s = &self.services;
        let current = s.agent.current_provider();
        let refused = |why: String| Error::new(format!("`exarch-agents `start` refused: {why}"));
        let bureau = &s.fleet.launch.bureau;
        let account = match provider {
            Selection::Inherit => current.account().clone(),
            Selection::Named(name) => {
                crate::provider::models::resolve_pinned_provider(name, &bureau.available())
                    .map_err(refused)?
            }
        };
        let model = match model {
            Selection::Inherit => current.model().to_string(),
            Selection::Named(model) => model.clone(),
        };
        if account.id == current.account().id && model == current.model() {
            return Ok(current);
        }
        bureau.reselect(&current, &account, model).map_err(refused)
    }

    /// Take up the fork a builtin body left for this desk: adopt it out of
    /// the parent's own transport, narrowed there by `grant`, or dial the
    /// listener it opened across the wire, whose engine narrows itself by the
    /// seed it carries.
    fn fork_seat(&self, verb: &str, fork: ForkClaim, grant: &SpawnGrant) -> Result<Seat, Error> {
        let refused = |why: String| Error::new(format!("`exarch-agents `{verb}` refused: {why}"));
        match (&self.services.kind, fork) {
            (SeatKind::Identity(parent), ForkClaim::Parked(id)) => parent
                .adopt_parked(id, grant)
                .map(Seat::adopted)
                .map_err(refused),
            (SeatKind::Wire { cwd, home }, ForkClaim::Listening { port, token }) => self
                .dial_seat(port, token, cwd.clone(), home.clone())
                .map_err(refused),
            (SeatKind::Identity(_), ForkClaim::Listening { .. }) => Err(refused(
                "this host runs its children in its own process, so the fork must be `parked \
                 <nursery id>`: a session that says it is listening for a dial is describing a \
                 wire this desk does not have"
                    .into(),
            )),
            (SeatKind::Wire { .. }, ForkClaim::Parked(_)) => Err(refused(
                "this host reaches its children across a wire, so the fork must be `listening \
                 [port, token]`: a session that says it is parked in process is naming a \
                 nursery this desk does not have"
                    .into(),
            )),
        }
    }

    /// The wire arm: dial the listener the guest opened for this one fork,
    /// write the token it is waiting for, and seat the child once the guest
    /// acknowledges — which it does only once the child process exists.
    fn dial_seat(
        &self,
        port: u32,
        token: u64,
        cwd: PathBuf,
        home: PathBuf,
    ) -> Result<Seat, String> {
        let s = &self.services;
        let dial = s.fleet.launch.dial.as_ref().ok_or(
            "this wire session has no dialler installed to reach a helper engine's listener: a \
             construction bug, since a fuelled wire trunk is refused at Avatar::root without one",
        )?;
        let mut stream = dial.dial(port).map_err(|reason| {
            format!("could not dial the helper engine's listener on guest port {port}: {reason}")
        })?;
        greet_hatch(&mut stream, token)?;
        // Past the ack the child is alive, so a refusal from here on simply
        // drops the stream: the child reads EOF on fd 3 and the guest's own
        // table reaps it.
        let transport = ral_core::carrier::WireTransport::adopt(
            stream,
            ral_core::protocol::channel::Liveness::default(),
        )
        .map_err(|e| format!("could not adopt the hatched wire: {e}"))?;
        // Attached as the parent was; the seed carries the live cwd.  A
        // hatched helper's logs live under the run that hatched it.
        Seat::wire(transport, cwd, home).map_err(|lost| {
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
        let launch = &s.fleet.launch;
        let fuel = s.agent.fuel.saturating_sub(1);
        // Against the *child's* grants, never this agent's, so the opening
        // bookend records the child's real system length.
        let system_prompt = launch.index.apply(
            &launch.system,
            &crate::prompt::Grants {
                returns,
                allow_schedule: launch.allow_schedule,
                spawns: fuel > 0,
            },
            &name,
        );
        let account =
            crate::record::RecordedAccount::of(provider.account(), &launch.bureau.available());
        let log = {
            let parent_log = s.log.borrow();
            let mut log = parent_log
                .fork(
                    crate::agent::fresh_id(),
                    system_prompt.len(),
                    &crate::record::RecordedModel::of(&provider),
                    &account,
                )
                .map_err(|e| Error::new(format!("could not fork child session log: {e}")))?;
            let inherited = inherit_context.then(|| parent_log.inherited_context());
            drop(parent_log);
            if let Some(inherited) = inherited {
                log.import_context(inherited).map_err(Error::new)?;
            }
            log
        };
        Avatar::assemble(Build {
            name,
            system_prompt,
            // The child's engine holds its own layer; the stack it runs under
            // is its parent's.
            caps: s.agent.caps.clone(),
            seat,
            log,
            parent: returns.then(|| s.agent.clone()),
            fuel,
            provider: ProviderHandle::new(provider),
            returns,
            search,
            fleet: s.fleet.clone(),
        })
        .map_err(|why| Error::new(format!("`exarch-agents `{verb}` refused: {why}")))
    }

    /// Hand `child` to `spawn_async` and answer the roster it now appears in —
    /// the state, not a receipt, and the same answer from either arm.
    fn spawn_child(&self, child: Avatar, name: String, prompt: String) -> Result<FOValue, Error> {
        let s = &self.services;
        // Held past the move, so the commitment arm can still name the act.
        let acted_name = name.clone();
        let acted_prompt = prompt.clone();
        let spawned = crate::agent::spawn::spawn_async(
            child,
            crate::agent::spawn::AsyncSpawn {
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
            Err(reason) => Err(Error::new(reason)),
        }
    }

    /// `` `branch `` — the desk half of the host's `/branch`: the fork takes
    /// the parent's whole authority and context, and waits for the host to
    /// take it up. Refused on any call that is not the host's own `/branch`.
    pub(super) fn agent_branch(&self, fork: ForkClaim) -> Result<FOValue, Error> {
        let s = &self.services;
        let Some(order) = &s.branch else {
            return Err(Error::new(
                "`exarch-agents `branch` is the host's own `/branch` door, and no /branch is \
                 under way on this call",
            ));
        };
        let seat = self.fork_seat("branch", fork, &SpawnGrant::Inherit)?;
        let child = self.child(
            "branch",
            order.name.clone(),
            seat,
            s.agent.current_provider(),
            order.returns,
            s.agent.search,
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
    /// subtree, scoped as [`Agent::descendant`](crate::agent::Agent::descendant) enforces. A real cancel and a
    /// miss are both successful calls answering the summary; only a scope
    /// violation raises.
    pub(super) fn agent_cancel(&self, name: &str) -> Result<FOValue, Error> {
        let s = &self.services;
        // The row is derived after the call: one claiming "cancelled" ahead of
        // it would assert an effect the world never saw. `cancel` takes no
        // argument, so its payload column carries the outcome instead.
        let cancelled = match s.fleet.resolve(name) {
            None => Ok(false),
            Some(found) => match s.agent.descendant(&found) {
                Some(target) => {
                    target.cancel_tree(ral_core::process::CancelCause::Cancelled);
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
            Err(Error::new(content))
        } else {
            Ok(self.summary())
        }
    }

    /// `` `message `` — resolve any live agent by name and send it a note.
    /// Unscoped, unlike `` `cancel ``: a note is the fleet's one way for a
    /// child to reach an ancestor or a sibling, and it only ever queues a turn.
    pub(super) fn message(&self, Message { to: name, text }: Message) -> Result<FOValue, Error> {
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
                    "agent '{name}' is you; `exarch-agents `message` reaches another agent: to wake yourself, arm a `exarch-schedules` fire"
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
            Err(Error::new(content))
        }
    }

    /// `` `reply `` — stage the payload into the cell [`Avatar::deliberate`] lifts
    /// into a deposit on this agent's own status once the batch drains:
    /// it parks the agent and hands the value to the parent's `` exarch-agents `read ``,
    /// rather than ending the run. Refused on every non-returning agent, keyed
    /// on `returns` and never on trunk-ness.
    pub(super) fn agent_reply(&self, value: FOValue) -> Result<FOValue, Error> {
        let s = &self.services;
        if !s.agent.returns {
            return Err(Error::new(
                "exarch-agents `reply` refused: you converse with the user; you do not return. \
                 `reply` parks you and hands your value to your parent's exarch-agents `read: \
                 the interactive trunk and every /branch child instead keep talking, turn after \
                 turn, and hold no `reply` to call.",
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
    pub(super) fn agent_read(&self, name: String) -> Result<FOValue, Error> {
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
                        "agent '{name}' has not replied yet: it is still working; wait for its \
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
                Err(Error::new(text))
            }
        }
    }
}
