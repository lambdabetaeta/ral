//! The agents family of the desk.

use crate::agent::build::Parent;
use crate::agent::engine::{Engine, EngineKind};
use crate::agent::roster::summary;
use crate::agent::{Agent, Body};
use crate::bus::{AgentMessage, Input, Refused};
use crate::enquiry::{Deposit, Message, Name, Selection, Start};
use crate::provider::Provider;
use crate::record::{Born, ChildFailure, ChildStatus, Memory};
use crate::shell_eval;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::error::Error;
use ral_core::fact::{
    AgentFailure, AgentFailureEnded, AgentFailureNoLiveAgent, AgentFailureNotDescendant,
    AgentFailureNotOwned, AgentFailureQueuedUnsynced, AgentFailureReplyAbsent,
    AgentFailureReplyFailed, AgentFailureReplyPending, AgentFailureReplyStopped,
    AgentFailureReplyUnavailable, AgentFailureResting, AgentFailureSelfMessage, EngineCause,
    EngineFailure, EngineStep, ExarchFailure, HelperFailure, InvariantFailure, IoOperation,
    IoSubject, Reason,
};
use ral_core::marshal::Marshalled;
use ral_core::shell::runtime::no_desk;
use std::sync::Arc;

use super::{DeskAct, ExarchDesk};

impl ExarchDesk {
    /// Child construction runs on the parent's thread.
    pub(super) fn launch(&self, Start { spec, parent }: Start) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(no_desk)?;
        let call = &self.call;
        body.log
            .check_available()
            .map_err(|why| super::failure(ExarchFailure::Recording(why.failure())))?;
        let Name(name) = spec.name;
        if call.handle.0.fuel == 0 {
            return Err(agent_failure(AgentFailure::NoFuel));
        }
        let reserved = call
            .fleet
            .registry
            .name(&name)
            .map_err(|why| agent_failure(why.failure()).with_context("exarch-agents `start"))?;
        let provider = self.child_provider(&body, &spec.model)?;
        let inherited = (spec.memory == Memory::Mnemon).then(|| body.log.inherited_context());
        let child = Agent::born(
            &call.fleet,
            Parent::Agent(&call.handle, call.home.child()),
            reserved,
            self.fork_engine(parent)?,
            provider,
            inherited,
        )
        .map_err(|why| why.with_context("exarch-agents `start"))?;
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
            Selection::Role(name) => s.fleet.launch.roles.get(name).map_err(|why| {
                agent_failure(AgentFailure::UnknownProviderRole(why))
                    .with_context("exarch-agents `start")
            }),
        }
    }

    /// Build a child from the same parent bytes on either engine.
    fn fork_engine(&self, parent: Marshalled) -> Result<Engine, Error> {
        match &self.call.kind {
            EngineKind::Identity(transport) => transport.child(&parent).map(Engine::adopted),
            EngineKind::Wire { engine } => self.make_engine(engine, parent),
        }
        .map_err(|why| why.with_context("`` exarch-agents `start `` refused"))
    }

    /// The wire arm: one factory engine attached to the marshalled parent.
    /// The child is attached before the roster names it.
    pub(super) fn make_engine(
        &self,
        engine: &ral_core::protocol::EngineSpec,
        parent: Marshalled,
    ) -> Result<Engine, Error> {
        let s = &self.call;
        let factory = s.fleet.launch.factory.as_ref().ok_or_else(|| {
            Error::new(Reason::Invariant(Box::new(
                InvariantFailure::MissingEngineFactory,
            )))
        })?;
        let stream = factory.engine().map_err(|cause| {
            super::failure(ExarchFailure::Engine(EngineFailure {
                step: EngineStep::Factory,
                cause: EngineCause::Factory(cause),
            }))
        })?;
        let transport = ral_core::carrier::WireTransport::adopt(
            stream,
            ral_core::protocol::channel::Liveness::default(),
        )
        .map_err(|e| {
            super::failure(ExarchFailure::Engine(EngineFailure {
                step: EngineStep::Adopt,
                cause: EngineCause::Io(ral_core::io::failure(&e)),
            }))
        })?;
        Engine::wire(
            transport,
            ral_core::protocol::Attach::child(engine.clone(), parent),
        )
        .map_err(|lost| crate::agent::engine::failure(EngineStep::Attach, &lost))
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
        let task = Input::Task(AgentMessage {
            from: call.handle.id(),
            from_name: call.handle.0.name.clone(),
            text: prompt.clone(),
        });
        let thread = child.launch(born, Some(task), &call.emit);
        call.commit_act(body, DeskAct::Spawn, Some(name), prompt, thread.is_err());
        let thread = thread.map_err(|error| {
            body.failures.extend(error.failures);
            Error::io(
                IoOperation::StartThread,
                IoSubject::Thread(handle.id().to_string()),
                &error.cause,
            )
        })?;
        body.nursery.adopt(handle, thread);
        Ok(self.summary())
    }

    fn summary(&self) -> FOValue {
        summary(&self.call.handle, &self.call.fleet).encode()
    }

    pub(super) fn agent_stop(&self, name: &str) -> Result<FOValue, Error> {
        let body = self.loan.get().ok_or_else(no_desk)?;
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
                None => Err(AgentFailure::NotDescendant(AgentFailureNotDescendant {
                    name: name.into(),
                })),
            },
        };
        self.conclude(&body, DeskAct::Stop, name, verdict)
    }

    pub(super) fn agent_dismiss(&self, name: &str) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(no_desk)?;
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
            None => Err(AgentFailure::NotOwned(AgentFailureNotOwned {
                name: name.into(),
            })),
        };
        self.conclude(&body, DeskAct::Dismiss, name, verdict)
    }

    pub(super) fn message(&self, Message { to: name, text }: Message) -> Result<FOValue, Error> {
        let body = self.loan.get().ok_or_else(no_desk)?;
        let call = &self.call;
        let verdict = match call.fleet.registry.resolve(&name) {
            None => Err(AgentFailure::NoLiveAgent(AgentFailureNoLiveAgent {
                name: name.clone(),
            })),
            Some(to) if to.id() == call.handle.id() => {
                Err(AgentFailure::SelfMessage(AgentFailureSelfMessage {
                    name: name.clone(),
                }))
            }
            Some(to) => {
                let said = Said::new(text.clone(), format!("sent message to agent '{name}'"));
                match to.message(&call.handle, text) {
                    Ok(()) => Ok(said),
                    Err(Refused::Ended) => Err(AgentFailure::Ended(AgentFailureEnded {
                        name: name.clone(),
                    })),
                    Err(Refused::Unsynced(why)) => {
                        let error = agent_failure(AgentFailure::QueuedUnsynced(
                            AgentFailureQueuedUnsynced {
                                name: name.clone(),
                                cause: why.failure(),
                            },
                        ));
                        let message = error.plain();
                        call.commit_act(
                            &body,
                            DeskAct::Message,
                            Some(&name),
                            message.clone(),
                            false,
                        );
                        body.log
                            .record_emitter()
                            .emit_or_end(crate::record::Forensic::HarnessResult { text: message });
                        return Err(error);
                    }
                    Err(Refused::Resting) => {
                        let parent = to.parent().expect("a resting helper has a parent");
                        let parent = &parent.0.name;
                        Err(AgentFailure::Resting(AgentFailureResting {
                            name: name.clone(),
                            parent: parent.clone(),
                        }))
                    }
                }
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
        verdict: Result<Said, AgentFailure>,
    ) -> Result<FOValue, Error> {
        let s = &self.call;
        let (payload, text, answer) = match verdict {
            Ok(Said { payload, text }) => (payload, text, Ok(())),
            Err(cause) => {
                let error = agent_failure(cause);
                let text = error.plain();
                (format!("refused: {text}"), text, Err(error))
            }
        };
        s.commit_act(body, act, Some(subject), payload, answer.is_err());
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Forensic::HarnessResult { text });
        answer.map(|()| self.summary())
    }

    /// The loop takes the staged reply after this batch returns.
    pub(super) fn agent_reply(&self, value: FOValue) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(no_desk)?;
        let s = &self.call;
        let returns = s
            .handle
            .0
            .address
            .transact(|txn| txn.mail().returns())
            .map_err(|why| super::failure(ExarchFailure::Recording(why.failure())))?;
        if !returns {
            return Err(agent_failure(AgentFailure::NoReturnRecipient));
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
        let body = self.loan.get().ok_or_else(no_desk)?;
        let content = match body.nursery.named(&name) {
            None => Err(agent_failure(AgentFailure::NotOwned(
                AgentFailureNotOwned { name: name.clone() },
            ))),
            Some(helper) => {
                let live = helper.is_live();
                match helper.status() {
                    Err(why) => Err(agent_failure(AgentFailure::ReplyUnavailable(
                        AgentFailureReplyUnavailable {
                            name: name.clone(),
                            cause: why.failure(),
                        },
                    ))),
                    Ok(status) if live && status.reply_pending() => Err(agent_failure(
                        AgentFailure::ReplyPending(AgentFailureReplyPending { name: name.clone() }),
                    )),
                    Ok(status) => match status.report {
                        Some(ChildStatus::Replied(reply)) => Ok(reply),
                        Some(ChildStatus::Failed(failure)) => Err(reply_failure(&name, failure)),
                        Some(ChildStatus::Stopped) => {
                            Err(agent_failure(AgentFailure::ReplyStopped(
                                AgentFailureReplyStopped { name: name.clone() },
                            )))
                        }
                        None => Err(agent_failure(AgentFailure::ReplyAbsent(
                            AgentFailureReplyAbsent { name: name.clone() },
                        ))),
                    },
                }
            }
        };
        let text = match &content {
            Ok(_) => format!("read agent '{name}'s reply"),
            Err(error) => error.plain(),
        };
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Forensic::HarnessResult { text });
        content.map(|reply| Deposit { name, reply }.encode())
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

fn agent_failure(cause: AgentFailure) -> Error {
    super::failure(ExarchFailure::Agent(cause))
}

fn reply_failure(name: &str, failure: ChildFailure) -> Error {
    let cause = match failure {
        ChildFailure::Refused(wire) => return wire.to_error(),
        ChildFailure::Cause(cause) => cause,
        ChildFailure::Panicked(_) => HelperFailure::Panicked,
    };
    agent_failure(AgentFailure::ReplyFailed(AgentFailureReplyFailed {
        name: name.into(),
        cause,
    }))
}
