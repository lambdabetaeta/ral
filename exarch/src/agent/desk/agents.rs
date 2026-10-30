//! The agents family of the desk.

use crate::agent::engine::{Engine, EngineKind};
use crate::agent::roster::summary;
use crate::agent::{Agent, Body};
use crate::bus::Refused;
use crate::enquiry::{Deposit, ForkClaim, Message, Name, Selection, Start};
use crate::provider::Provider;
use crate::record::{Born, ChildStatus};
use crate::shell_eval;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::shell::runtime::Error;
use std::sync::Arc;

use super::{DeskAct, ExarchDesk};

impl ExarchDesk {
    /// Construction and adoption both run on the parent's thread.
    pub(super) fn launch(&self, Start { spec, fork }: Start) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(super::no_desk)?;
        let call = &self.call;
        let Name(name) = spec.name;
        if call.handle.0.fuel == 0 {
            return Err(Error::new(
                "`` exarch-agents `start `` refused: no spawn fuel remains at this depth. \
                 Fuel bounds how deep a chain of spawns may recurse, not how many helpers \
                 you may start here. You may still stop a descendant's exchange or dismiss \
                 one of your own helpers.",
            ));
        }
        let reserved = call
            .fleet
            .registry
            .name(&name)
            .map_err(|why| Error::new(format!("`` exarch-agents `start `` refused: {why}")))?;
        let provider = self.child_provider(&body, &spec.model)?;
        let child = Agent::child_of(
            (&call.handle, call.life.child()),
            &body.log,
            &call.fleet,
            reserved,
            self.fork_engine(fork)?,
            provider,
            spec.memory,
        )
        .map_err(|why| Error::new(format!("`` exarch-agents `start `` refused: {why}")))?;
        self.spawn_child(&mut body, child, &name, spec.prompt)
    }

    /// The child's provider: the parent's own `Arc` for `Inherit`, else the
    /// one the human bound to the named role at launch.
    pub(super) fn child_provider(
        &self,
        body: &Body,
        model: &Selection,
    ) -> Result<Arc<Provider>, Error> {
        let s = &self.call;
        match model {
            Selection::Inherit => Ok(body.client.clone()),
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
    /// engine.
    fn fork_engine(&self, fork: ForkClaim) -> Result<Engine, Error> {
        let refused =
            |why: String| Error::new(format!("`` exarch-agents `start `` refused: {why}"));
        match (&self.call.kind, fork) {
            (EngineKind::Identity(parent), ForkClaim::Parked(id)) => parent
                .adopt_parked(id)
                .map(Engine::adopted)
                .map_err(refused),
            (EngineKind::Wire { engine }, ForkClaim::Sealed(seed)) => {
                self.make_engine(engine, seed).map_err(refused)
            }
            (EngineKind::Identity(_), ForkClaim::Sealed(_)) => Err(refused(
                "this host runs its children in its own process, so the fork must be `parked \
                 <nursery id>`: a sealed bequest names a wire this desk does not have"
                    .into(),
            )),
            (EngineKind::Wire { .. }, ForkClaim::Parked(_)) => Err(refused(
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
    ) -> Result<Engine, String> {
        let s = &self.call;
        let factory = s.fleet.launch.factory.as_ref().ok_or(
            "this wire session has no engine factory: a construction bug, since a fuelled wire \
             trunk is refused at Agent::root without one",
        )?;
        let stream = factory
            .engine()
            .map_err(|why| format!("could not make a helper engine: {why}"))?;
        let transport = ral_core::carrier::WireTransport::adopt(
            stream,
            ral_core::protocol::channel::Liveness::default(),
        )
        .map_err(|e| format!("could not adopt the helper engine's wire: {e}"))?;
        Engine::wire(
            transport,
            ral_core::protocol::Attach::child(engine.clone(), seal),
        )
        .map_err(|lost| crate::agent::EngineLost::starting(&lost, s.handle.run_dir()).to_string())
    }

    fn spawn_child(
        &self,
        body: &mut Body,
        child: Agent,
        name: &str,
        prompt: String,
    ) -> Result<FOValue, Error> {
        let call = &self.call;
        let handle = child.handle();
        let born = Born {
            log_dir: child.log_dir().clone(),
            name: name.to_string(),
            parent: Some(call.handle.id()),
            resumed: None,
            reader: child.recorder().reader(),
        };
        let thread = child.launch(born, Some(prompt.clone()), &call.emit);
        call.commit_act(body, DeskAct::Spawn, Some(name), prompt, thread.is_err());
        let thread = thread.map_err(|error| {
            body.failures.extend(error.failures);
            Error::new(format!(
                "could not spawn a worker thread for agent '{}': {}",
                handle.id(),
                error.cause
            ))
        })?;
        body.nursery.adopt(handle, thread);
        Ok(self.summary())
    }

    fn summary(&self) -> FOValue {
        summary(&self.call.handle, &self.call.fleet).encode()
    }

    pub(super) fn agent_stop(&self, name: &str) -> Result<FOValue, Error> {
        let body = self.loan.get().ok_or_else(super::no_desk)?;
        let call = &self.call;
        let verdict = match call.fleet.registry.resolve(name) {
            None => Ok(Said::new(
                "no live agent by that name",
                format!("no live agent named '{name}'"),
            )),
            Some(found) => match call.handle.descendant(&found) {
                Some(target) => {
                    let text = if target.stop_current() {
                        format!(
                            "stopped agent '{name}': its current exchange is cancelled; its own helpers work on"
                        )
                    } else {
                        format!("agent '{name}' is at rest")
                    };
                    Ok(Said::new(text.clone(), text))
                }
                None => Err(Said::new(
                    "refused: not a descendant",
                    format!(
                        "agent '{name}' is not an agent you started; {} may only reach a descendant of yours",
                        ral_core::text::code("exarch-agents `stop")
                    ),
                )),
            },
        };
        self.conclude(&body, DeskAct::Stop, name, verdict)
    }

    pub(super) fn agent_dismiss(&self, name: &str) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(super::no_desk)?;
        let verdict = match body.nursery.named(name).cloned() {
            Some(helper) => {
                let ended = !body.nursery.holds(helper.id());
                helper.end();
                let failures = body.nursery.join(helper.id());
                body.failures.extend(failures);
                let outcome = if ended {
                    "had already ended"
                } else {
                    "dismissed"
                };
                Ok(Said::new("", format!("agent '{name}' {outcome}")))
            }
            None => Err(Said::new(
                "refused: not your own helper",
                "dismiss only an agent you started; its parent may dismiss it".into(),
            )),
        };
        self.conclude(&body, DeskAct::Dismiss, name, verdict)
    }

    pub(super) fn message(&self, Message { to: name, text }: Message) -> Result<FOValue, Error> {
        let body = self.loan.get().ok_or_else(super::no_desk)?;
        let call = &self.call;
        let verdict = match call.fleet.registry.resolve(&name) {
            None => Err(Said::new(
                "refused: no live agent by that name",
                format!("no live agent named '{name}'; did it finish already?"),
            )),
            Some(to) if to.id() == call.handle.id() => Err(Said::new(
                "refused: that is you",
                format!(
                    "agent '{name}' is you; `` exarch-agents `message `` reaches another agent: to wake yourself, arm a `exarch-schedules` fire"
                ),
            )),
            Some(to) => {
                let said = Said::new(text.clone(), format!("sent message to agent '{name}'"));
                to.message(&call.handle, text).map(|()| said).map_err(|why| match why {
                    Refused::Ended => Said::new(
                        "refused: the agent ended",
                        format!("agent '{name}' has ended; start a new one"),
                    ),
                    Refused::Resting => {
                        let parent = to.parent().expect("a resting helper has a parent");
                        let parent = &parent.0.name;
                        Said::new(
                            "refused: the agent rests",
                            format!("agent '{name}' has answered '{parent}' and rests; only '{parent}' can message it now"),
                        )
                    }
                })
            }
        };
        self.conclude(&body, DeskAct::Message, &name, verdict)
    }

    /// Commit `verdict`, `Err` being a refusal, and answer: the summary for an
    /// act that stands, a raise for a refusal, since the raise is the model's
    /// only copy of it.
    fn conclude(
        &self,
        body: &Body,
        act: DeskAct,
        subject: &str,
        verdict: Result<Said, Said>,
    ) -> Result<FOValue, Error> {
        let s = &self.call;
        let refused = verdict.is_err();
        let (Ok(Said { payload, text }) | Err(Said { payload, text })) = verdict;
        s.commit_act(body, act, Some(subject), payload, refused);
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Forensic::HarnessResult { text: text.clone() });
        if refused {
            Err(Error::new(text))
        } else {
            Ok(self.summary())
        }
    }

    /// The loop takes the staged reply after this batch returns.
    pub(super) fn agent_reply(&self, value: FOValue) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(super::no_desk)?;
        let s = &self.call;
        if !s.handle.0.role.returns() {
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
        s.commit_act(&body, DeskAct::Reply, None, payload, false);
        body.reply = Some(value);
        Ok(self.summary())
    }

    pub(super) fn agent_read(&self, name: String) -> Result<FOValue, Error> {
        let body = self.loan.get().ok_or_else(super::no_desk)?;
        let content = match body.nursery.named(&name) {
            None => Err(format!(
                "you started no agent named '{name}'; {} reads only your own helpers, and a clear forgets earlier ones",
                ral_core::text::code("exarch-agents `read")
            )),
            Some(helper) => {
                let live = helper.is_live();
                match helper.status() {
                    Err(why) => Err(format!(
                        "agent '{name}' has ended and its state is unavailable: {why}"
                    )),
                    Ok(status) => {
                        if live && status.working() {
                            Err(format!(
                                "agent '{name}' has not replied yet: it is still working; wait for its notice instead of polling"
                            ))
                        } else {
                            match status.report {
                                Some(ChildStatus::Replied(reply)) => Ok(reply),
                                Some(ChildStatus::Failed(reason)) => Err(format!(
                                    "agent '{name}' failed without a reply to read: {reason}"
                                )),
                                Some(ChildStatus::Stopped) => Err(format!(
                                    "agent '{name}' stopped without a reply to read; message it to continue"
                                )),
                                None => Err(format!("agent '{name}' has no reply to read")),
                            }
                        }
                    }
                }
            }
        };
        let text = match &content {
            Ok(_) => format!("read agent '{name}'s reply"),
            Err(text) => text.clone(),
        };
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Forensic::HarnessResult { text });
        content
            .map(|reply| Deposit { name, reply }.encode())
            .map_err(Error::new)
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
