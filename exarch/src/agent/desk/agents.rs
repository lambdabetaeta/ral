//! The agents family of the desk.

use crate::agent::Avatar;
use crate::agent::roster::summary;
use crate::agent::seat::{Seat, SeatKind};
use crate::enquiry::{Deposit, ForkClaim, Message, Name, Selection, Start};
use crate::provider::Provider;
use crate::shell_eval;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::shell::runtime::Error;
use std::sync::Arc;

use super::{DeskAct, ExarchDesk};

impl ExarchDesk {
    /// The spawn spine behind `` `start ``: take up the fork, fork its log off
    /// the parent's, assemble it at one less unit of fuel, hand it to
    /// `spawn_async`. Every cheap guard runs before the fork is taken up; a
    /// refusal after it simply drops the child's engine.
    pub(super) fn launch(&self, Start { spec, fork }: Start) -> Result<FOValue, Error> {
        let s = &self.services;
        let Name(name) = spec.name;

        // The captured fuel and grant are only as fresh as the
        // envelope they were snapshotted under — this caller's own, so a
        // `/clear` in another tab does not refuse a spawn here.
        if s.stamp.is_stale() {
            return Err(Error::new(
                "`` exarch-agents `start `` refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale: \
                 issue agent again on your next turn",
            ));
        }

        // Fuel bounds depth, not fan-out: the parent's own is never debited, so
        // siblings are free and only a deep enough chain bottoms out.
        if s.agent.fuel == 0 {
            return Err(Error::new(
                "`` exarch-agents `start `` refused: no spawn fuel remains at this depth, so you cannot \
                 delegate any further here. Fuel bounds how deep a chain of spawns may \
                 recurse, never how many children you may start at any one depth: starting \
                 several agents here costs nothing extra. `` exarch-agents `cancel `` on any node stops \
                 its whole live subtree regardless of depth.",
            ));
        }

        // Before the fork is taken up, so a refused name unwinds nothing and
        // two same-name spawns have one winner.
        let claim = s
            .fleet
            .claim(&name)
            .map_err(|why| Error::new(format!("`` exarch-agents `start `` refused: {why}")))?;

        // Before the seat split, so both arms share one resolution and a
        // refusal unwinds nothing: no adopted shell, no forked log, no helper engine.
        let provider = self.child_provider(&spec.model)?;

        let seat = self.fork_seat(fork)?;
        let child = Avatar::child_of(
            (&s.agent, s.stamp.clone()),
            &s.log.borrow(),
            &s.fleet,
            claim,
            seat,
            provider,
            spec.memory,
        )
        .map_err(|why| Error::new(format!("`` exarch-agents `start `` refused: {why}")))?;
        self.spawn_child(child, name, spec.prompt)
    }

    /// The child's provider: the parent's own `Arc` for `Inherit`, else the
    /// one the human bound to the named role at launch.
    pub(super) fn child_provider(&self, model: &Selection) -> Result<Arc<Provider>, Error> {
        let s = &self.services;
        match model {
            Selection::Inherit => Ok(s.agent.current_provider()),
            Selection::Role(name) => s
                .fleet
                .launch
                .roles
                .get(name)
                .map_err(|why| Error::new(format!("`` exarch-agents `start `` refused: {why}"))),
        }
    }

    /// Take up the fork a builtin body left for this desk: adopt it out of
    /// the parent's own transport, or make an engine for the sealed bequest
    /// across the wire.  The engine narrows by the bequest's grant on either
    /// seat.
    fn fork_seat(&self, fork: ForkClaim) -> Result<Seat, Error> {
        let refused =
            |why: String| Error::new(format!("`` exarch-agents `start `` refused: {why}"));
        match (&self.services.kind, fork) {
            (SeatKind::Identity(parent), ForkClaim::Parked(id)) => {
                parent.adopt_parked(id).map(Seat::adopted).map_err(refused)
            }
            (SeatKind::Wire { engine }, ForkClaim::Sealed(seed)) => {
                self.make_engine(engine, seed).map_err(refused)
            }
            (SeatKind::Identity(_), ForkClaim::Sealed(_)) => Err(refused(
                "this host runs its children in its own process, so the fork must be `parked \
                 <nursery id>`: a sealed bequest names a wire this desk does not have"
                    .into(),
            )),
            (SeatKind::Wire { .. }, ForkClaim::Parked(_)) => Err(refused(
                "this host makes its children over a wire, so the fork must be `sealed <bytes>`: \
                 a session that says it is parked in process is naming a nursery this desk does \
                 not have"
                    .into(),
            )),
        }
    }

    /// The wire arm: one engine from the factory, attached as the parent was
    /// plus its seal. The parent's enquiry is answered only once this returns,
    /// so the child is attached before the roster names it.
    pub(super) fn make_engine(
        &self,
        engine: &ral_core::protocol::EngineSpec,
        seal: ral_core::seed::Sealed,
    ) -> Result<Seat, String> {
        let s = &self.services;
        let factory = s.fleet.launch.factory.as_ref().ok_or(
            "this wire session has no engine factory: a construction bug, since a fuelled wire \
             trunk is refused at Avatar::root without one",
        )?;
        let stream = factory
            .engine()
            .map_err(|why| format!("could not make a helper engine: {why}"))?;
        let transport = ral_core::carrier::WireTransport::adopt(
            stream,
            ral_core::protocol::channel::Liveness::default(),
        )
        .map_err(|e| format!("could not adopt the helper engine's wire: {e}"))?;
        Seat::wire(
            transport,
            ral_core::protocol::Attach::child(engine.clone(), seal),
        )
        .map_err(|lost| crate::agent::EngineLost::starting(&lost, s.agent.run_dir()).to_string())
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
                resumed: None,
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
        // `cancel` takes no argument, so its payload column carries the
        // outcome, derived after the call: a row claiming "cancelled" ahead of
        // it would assert an effect the world never saw.
        let verdict = match s.fleet.resolve(name) {
            None => Ok(Said::new(
                "no live agent by that name",
                format!("no live agent named '{name}'"),
            )),
            Some(found) => match s.agent.descendant(&found) {
                Some(target) => {
                    target.cancel_tree(ral_core::process::CancelCause::Cancelled);
                    Ok(Said::new("", format!("cancelling agent '{name}'")))
                }
                None => Err(Said::new(
                    "refused: not a descendant",
                    format!(
                        "agent '{name}' is not an agent you started; {} may only reach a descendant of yours",
                        ral_core::text::code("exarch-agents `cancel")
                    ),
                )),
            },
        };
        self.conclude(DeskAct::Cancel, name, verdict)
    }

    /// `` `message `` — resolve any live agent by name and send it a note.
    /// Unscoped, unlike `` `cancel ``: a note is the fleet's one way for a
    /// child to reach an ancestor or a sibling, and it only ever queues a turn.
    pub(super) fn message(&self, Message { to: name, text }: Message) -> Result<FOValue, Error> {
        let s = &self.services;
        // Unlike `cancel`, an unresolved name refuses rather than no-ops:
        // `message` promises delivery and there is nothing to deliver to. A
        // target settling between the name resolving and the send lands in the
        // same arm, one step later.
        let verdict = match s.fleet.resolve(&name) {
            None => Err(Said::new(
                "refused: no live agent by that name",
                format!("no live agent named '{name}'; did it finish already?"),
            )),
            Some(to) if to.id == s.agent.id => Err(Said::new(
                "refused: that is you",
                format!(
                    "agent '{name}' is you; `` exarch-agents `message `` reaches another agent: to wake yourself, arm a `exarch-schedules` fire"
                ),
            )),
            Some(to) => {
                let said = Said::new(text.clone(), format!("sent message to agent '{name}'"));
                s.agent.message(&to, text);
                Ok(said)
            }
        };
        self.conclude(DeskAct::Message, &name, verdict)
    }

    /// Commit `verdict`, `Err` being a refusal, and answer: the summary for an
    /// act that stands, a raise for a refusal, since the raise is the model's
    /// only copy of it.
    fn conclude(
        &self,
        act: DeskAct,
        subject: &str,
        verdict: Result<Said, Said>,
    ) -> Result<FOValue, Error> {
        let s = &self.services;
        let refused = verdict.is_err();
        let (Ok(Said { payload, text }) | Err(Said { payload, text })) = verdict;
        s.commit_act(act, Some(subject), payload, refused);
        s.log
            .borrow()
            .record_emitter()
            .emit_or_report(crate::record::Forensic::HarnessResult { text: text.clone() });
        if refused {
            Err(Error::new(text))
        } else {
            Ok(self.summary())
        }
    }

    /// `` `reply `` — stage the payload into the cell [`Avatar::deliberate`] lifts
    /// into a deposit on this agent's own status once the batch drains, for
    /// its consumer: a parent's `` exarch-agents `read ``, or the headless
    /// driver. Refused on every conversing agent, keyed on its [`Role`](crate::agent::Role) and
    /// never on trunk-ness.
    pub(super) fn agent_reply(&self, value: FOValue) -> Result<FOValue, Error> {
        let s = &self.services;
        if !s.agent.role.returns() {
            return Err(Error::new(
                "exarch-agents `reply` refused: you converse with the user; you do not return. \
                 `reply` hands a spawned or headless agent's answer to whoever launched it; \
                 an interactive root instead keeps talking, turn after turn, and holds no \
                 `reply` to call.",
            ));
        }
        let display = shell_eval::ral_value_to_text(&value).unwrap_or_default();
        let payload = if display.is_empty() {
            "(empty reply)".into()
        } else {
            display
        };
        // No subject: a returning agent has exactly one recipient.
        s.commit_act(DeskAct::Reply, None, payload, false);
        *s.reply.borrow_mut() = Some(value);
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
                    "agent '{name}' is not an agent you started; {} may only reach \
                     a descendant of yours",
                    ral_core::text::code("exarch-agents `read")
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
                s.log.borrow().record_emitter().emit_or_report(
                    crate::record::Forensic::HarnessResult {
                        text: format!("read agent '{name}'s reply"),
                    },
                );
                Ok(Deposit { name, reply }.encode())
            }
            Err(text) => {
                s.log
                    .borrow()
                    .record_emitter()
                    .emit_or_report(crate::record::Forensic::HarnessResult { text: text.clone() });
                Err(Error::new(text))
            }
        }
    }
}

/// An act's two traces: the payload its rail row carries, and the sentence
/// its harness note says.
struct Said {
    payload: String,
    text: String,
}

impl Said {
    fn new(payload: impl Into<String>, text: String) -> Self {
        Self {
            payload: payload.into(),
            text,
        }
    }
}
