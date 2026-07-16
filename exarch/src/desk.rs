//! The exarch enquiry desk: the host half of `shell.enquire(class)`.
//!
//! [`HostServices`] is the capture set a desk handler is allowed to touch —
//! everything [`crate::agent::Agent::host_services`] can hand off `&Agent`
//! without lending `&mut Agent`/`&mut Shell`, since the reentrancy law
//! (`docs/ral-wiki/decisions/260706_enquiry-channel.md` §3) bars a handler
//! from ever taking the session lock. [`ExarchDesk`] answers one enquiry
//! against that capture; [`DeskBinding`] is the identity-transport adapter
//! that keeps a handler's own chrome from outrunning the turn's earlier
//! surface output. Step 0 of `dev/docs/260715_tools_to_builtins.md` wired
//! both desk bindings answering every enquiry class alike, with the
//! extension law's unrecognised-class error; this module now also carries
//! Step 1, the agent family (`` `agent-start ``, `` `agent-list ``,
//! `` `agent-cancel ``, `` `message ``), Step 2, the schedule family
//! (`` `schedule ``, `` `schedule-list ``, `` `unschedule ``), Step 3, the
//! commitment pair (`` `commit-open ``, `` `commit-verify ``), and Step 4,
//! `` `reply ``.

use crate::agent::{Agent, Build, ProviderHandle, ReplyCell};
use crate::agent_registry::{AgentRegistry, MessageError, NotADescendant};
use crate::bus::{AgentId, Emitter, Kind, Mailbox};
use crate::event::AgentLog;
use crate::schedule::{CronSchedule, ScheduleId, ScheduleRegistry, Trigger, parse_duration};
use crate::shell_eval::{self, PinDigests};
use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use ral_core::transport::{Event, EventReceiver, Frame};
use ral_core::types::{Capabilities, EnquiryDesk, Error, Nursery, NurseryId};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

/// Everything a desk handler may read off `&Agent`, snapshotted fresh at
/// every [`crate::agent::Agent::run_shell`] install
/// (`crate::agent::Agent::host_services`) so a handler never reaches back
/// through `&mut Agent`/`&mut Shell` to get it. Mirrors
/// [`crate::agent::Build`] minus the shell, plus the handles the harness
/// verbs need that `Build` has no reason to carry (the registry, the
/// parent's mailbox, the nursery, …).
///
/// `agent-start`/`agent-list`/`agent-cancel`/`message` (Step 1),
/// `schedule`/`schedule-list`/`unschedule` (Step 2), `commit-open`/
/// `commit-verify` (Step 3), and `reply` (Step 4, reading `returns` and
/// `reply`) read this capture between them.
pub(crate) struct HostServices {
    /// The fleet's shared agent registry — `agents.clone()`.
    pub registry: AgentRegistry,
    /// This turn's agent: the child's upward edge for `agent-start`'s
    /// receipt and the descendant check `message`/`agent-cancel` enforce.
    pub parent: AgentId,
    /// The parent's own inbox — where a spawned child's eventual result and
    /// a `message` delivery land.
    pub mailbox: Mailbox,
    /// Rail chrome plus child-tab derivation for a spawn's `Kind::ToolCall`
    /// line and the acting verbs' `ToolCall`/`ToolResult` pair.
    pub emit: Emitter,
    /// The parent's hot-swappable provider handle, seeded into a spawned
    /// child's own handle.
    pub provider: ProviderHandle,
    /// The parent ceiling `policy::narrow` attenuates against for
    /// `agent-start`'s `permissions` argument.
    pub caps: Capabilities,
    /// The working directory frozen at install, for `policy::narrow` and
    /// any handler that needs a path root.
    pub cwd: PathBuf,
    /// The depth-budget snapshot for this turn, immutable for its extent:
    /// `agent-start` refuses once it reads zero.
    pub fuel: u32,
    /// Whether this agent holds `reply` — the desk's `reply` refusal reads
    /// this bit, never trunk-ness.
    pub returns: bool,
    /// Whether this agent holds the self-wakeup grant — the schedule
    /// family's handlers refuse without it.
    pub allow_schedule: bool,
    /// The shared slot the `reply` handler stages a returning agent's
    /// return value into — `agent.reply.clone()`.  `Agent::apply`'s drain
    /// lifts whatever is staged into a `TurnOutcome::Replied` once the
    /// batch drains; the handler never holds `&mut Agent` to write it any
    /// other way.
    pub reply: ReplyCell,
    /// This agent's live scheduled wakeups.
    pub schedules: ScheduleRegistry,
    /// The protected commitment/service pin register `commit-open` /
    /// `commit-verify` read and write.
    pub pins: PinDigests,
    /// This session's canonical event log — `mnemon`'s context-inheriting
    /// fork reads it.
    pub log: Arc<Mutex<AgentLog>>,
    /// The system prompt, for a spawned child's own log.
    pub system: String,
    /// The fleet's focused-agent handle, for a spawned child to become
    /// parkable the instant the human `TAB`s to it.
    pub focus: Arc<AtomicU64>,
    /// Whether a human is attached to the fleet, inherited by every spawn.
    pub interactive: bool,
    /// The adoption end of this turn's body-side session forks
    /// (`Shell::fork_into_nursery`) — `agent-start` and `commit-open` /
    /// `commit-verify` redeem a [`ral_core::types::NurseryId`] here.
    pub nursery: Nursery,
    /// The agent registry's generation at install, so a desk installed
    /// before `/clear` can refuse an `agent-start` raised after it.
    pub generation: u64,
    /// The operator's disk-warn ceiling, threaded verbatim into a spawned
    /// child's own `Build` exactly as `Agent::fork_with` does — a host
    /// setting, not a per-agent choice.
    pub disk_warn_bytes: Option<u64>,
}

/// The exarch enquiry desk: answers one [`FOValue`] enquiry against a
/// captured [`HostServices`]. Installed fresh per `ral` call
/// (`crate::agent::Agent::run_shell`), wrapped in [`DeskBinding`] for the
/// identity transport and passed bare into `shell_eval::run_shell`'s
/// `on_enquiry` binding for the wire-future path — one handler, two
/// bindings, per `docs/ral-wiki/decisions/260706_enquiry-channel.md`.
pub(crate) struct ExarchDesk {
    pub(crate) services: HostServices,
}

/// An agent's display label: its title when known, else its numeric id —
/// the desk twin of `tools::agent`'s own copy, shared by [`ExarchDesk::message`]
/// and [`ExarchDesk::agent_cancel`].
fn label(title: Option<&str>, id: AgentId) -> String {
    title.map_or_else(|| id.to_string(), str::to_string)
}

/// `AgentId` is `u64`; every id this desk ever mints or reads back is a
/// small monotonic counter, so the cast to the `Int` an [`FOValue`] carries
/// never wraps.
#[allow(clippy::cast_possible_wrap, reason = "AgentId is a small monotonic counter; no realistic i64 wrap")]
fn agent_id_to_fo(id: AgentId) -> FOValue {
    FOValue::Int { value: id as i64 }
}

/// `ScheduleId` is `u64`; every id the schedule registry mints is a small
/// monotonic counter, so the cast to the `Int` an [`FOValue`] carries never
/// wraps.
#[allow(clippy::cast_possible_wrap, reason = "ScheduleId is a small monotonic counter; no realistic i64 wrap")]
fn schedule_id_to_fo(id: ScheduleId) -> FOValue {
    FOValue::Int { value: id as i64 }
}

/// Decode one enquiry payload as a fixed-length list, or a didactic error
/// naming the expected shape — the host-side half of the both-ends
/// validation law: a builtin's door already checked its own arguments, but
/// the desk trusts nothing about what actually crossed the wire.
fn payload_list(
    payload: Option<Box<FOValue>>,
    class: &str,
    shape: &str,
    n: usize,
) -> Result<Vec<FOValue>, Error> {
    let Some(payload) = payload else {
        return Err(Error::new(format!("`{class}` requires a payload {shape}"), 1));
    };
    let FOValue::List { items } = *payload else {
        return Err(Error::new(format!("`{class}` payload must be a list {shape}"), 1));
    };
    if items.len() != n {
        return Err(Error::new(
            format!(
                "`{class}` payload must have exactly {n} element(s) {shape}, got {}",
                items.len()
            ),
            1,
        ));
    }
    Ok(items)
}

fn payload_int(v: FOValue, class: &str, field: &str) -> Result<i64, Error> {
    match v {
        FOValue::Int { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `{field}` must be an Int, got {other:?}"),
            1,
        )),
    }
}

fn payload_string(v: FOValue, class: &str, field: &str) -> Result<String, Error> {
    match v {
        FOValue::String { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `{field}` must be a Str, got {other:?}"),
            1,
        )),
    }
}

fn payload_tag(v: FOValue, class: &str, field: &str) -> Result<String, Error> {
    match v {
        FOValue::Variant { label, payload: None } => Ok(label),
        other => Err(Error::new(
            format!("`{class}`: `{field}` must be a bare variant tag, got {other:?}"),
            1,
        )),
    }
}

/// Decode a `schedule` trigger variant into a live [`Trigger`], re-running
/// the same parsers ([`CronSchedule::parse`]/[`parse_duration`]) the
/// `schedule` builtin's own door already ran — the host-side half of the
/// both-ends validation law: a builtin's door check is never this desk's
/// only line of defence.
fn payload_trigger(v: FOValue, class: &str) -> Result<Trigger, Error> {
    match v {
        FOValue::Variant { label, payload: Some(payload) } if label == "cron" => {
            let expr = payload_string(*payload, class, "trigger")?;
            let schedule = CronSchedule::parse(&expr).map_err(|e| Error::new(format!("`{class}`: {e}"), 1))?;
            Ok(Trigger::Cron { schedule, expr })
        }
        FOValue::Variant { label, payload: Some(payload) } if label == "after" => {
            let dur = payload_string(*payload, class, "trigger")?;
            let d = parse_duration(&dur).map_err(|e| Error::new(format!("`{class}`: {e}"), 1))?;
            Ok(Trigger::After(d))
        }
        other => Err(Error::new(
            format!("`{class}`: `trigger` must be `cron <expr>` or `after <dur>`, got {other:?}"),
            1,
        )),
    }
}

/// Decode a `schedule` label variant into an `Option<String>` —
/// `` `none `` sends the absent label so
/// [`ScheduleRegistry::schedule`] mints its `sched-{id}` default.
fn payload_label(v: FOValue, class: &str) -> Result<Option<String>, Error> {
    match v {
        FOValue::Variant { label, payload: Some(payload) } if label == "some" => {
            Ok(Some(payload_string(*payload, class, "label")?))
        }
        FOValue::Variant { label, payload: None } if label == "none" => Ok(None),
        other => Err(Error::new(
            format!("`{class}`: `label` must be `some <str>` or `none`, got {other:?}"),
            1,
        )),
    }
}

impl ExarchDesk {
    /// Decode one enquiry class and answer it.
    ///
    /// The agent family (`` `agent-start ``, `` `agent-list ``,
    /// `` `agent-cancel ``, `` `message ``), the schedule family
    /// (`` `schedule ``, `` `schedule-list ``, `` `unschedule ``), the
    /// commitment pair (`` `commit-open ``, `` `commit-verify ``), and
    /// `` `reply `` are answered here; an unrecognised class still answers
    /// the extension law's error — never a silent default
    /// (`docs/ral-wiki/decisions/260706_enquiry-channel.md`, "the extension
    /// law").
    ///
    /// # Errors
    /// Returns `Err` if `req` is not a [`FOValue::Variant`], names a class
    /// this desk does not answer, or the named class itself refuses.
    pub(crate) fn handle(&self, req: FOValue) -> Result<FOValue, Error> {
        let FOValue::Variant { label, payload } = req else {
            return Err(Error::new(
                format!("enquiry request must be a variant naming its class, got {req:?}"),
                1,
            ));
        };
        match label.as_str() {
            "agent-start" => self.agent_start(payload),
            "agent-list" => Ok(self.agent_list()),
            "agent-cancel" => self.agent_cancel(payload),
            "message" => self.message(payload),
            "schedule" => self.schedule(payload),
            "schedule-list" => self.schedule_list(),
            "unschedule" => self.unschedule(payload),
            "commit-open" => self.commit_open(payload),
            "commit-verify" => self.commit_verify(payload),
            "reply" => self.reply(payload),
            other => Err(Error::new(format!("unrecognised enquiry class `{other}`"), 1)),
        }
    }

    /// `` `agent-start `` — the desk half of `amnemon`/`mnemon`: adopt the
    /// nursery-parked fork the builtin body forked, narrow its permissions,
    /// assemble it into a full child, and hand it to [`spawn_async`] exactly
    /// as `tools::agent::dispatch_spawn` does today. A relocation of that
    /// function's post-parse logic, not a rewrite.
    fn agent_start(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;

        let items = payload_list(
            payload,
            "agent-start",
            "[session, kind, prompt, title, permissions]",
            5,
        )?;
        let [session, kind, prompt, title, permissions]: [FOValue; 5] = items
            .try_into()
            .unwrap_or_else(|_| unreachable!("payload_list already checked the length"));
        let session_id = payload_int(session, "agent-start", "session")?;
        if session_id < 0 {
            return Err(Error::new("`agent-start`: `session` must be a non-negative Int", 1));
        }
        let kind = payload_tag(kind, "agent-start", "kind")?;
        let mnemon = match kind.as_str() {
            "amnemon" => false,
            "mnemon" => true,
            other => {
                return Err(Error::new(
                    format!("`agent-start`: `kind` must be `amnemon` or `mnemon`, got `{other}`"),
                    1,
                ));
            }
        };
        let prompt = payload_string(prompt, "agent-start", "prompt")?;
        let title = payload_string(title, "agent-start", "title")?;
        let permissions = payload_tag(permissions, "agent-start", "permissions")?;

        // 1. The /clear guard: this call's captured fuel, caps, and grant
        // are only as fresh as the generation they were snapshotted under.
        if s.generation != s.registry.generation() {
            return Err(Error::new(
                "agent-start refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale — issue \
                 amnemon/mnemon again on your next turn",
                1,
            ));
        }

        // 2. Fuel bounds depth, not fan-out (agent.rs's SPAWN_FUEL doc): the
        // parent's own fuel is never debited here, so unlimited siblings may
        // start at this depth; only a chain that recurses this many
        // generations deep bottoms out.
        if s.fuel == 0 {
            return Err(Error::new(
                "agent-start refused: no spawn fuel remains at this depth, so amnemon/mnemon \
                 cannot delegate any further here. Fuel bounds how deep a chain of spawns may \
                 recurse, never how many children you may start at any one depth — starting \
                 several agents here costs nothing extra. agent-cancel on any node stops its \
                 whole live subtree regardless of depth.",
                1,
            ));
        }

        // 3. Adopt the parked fork the builtin body forked. Once past this
        // point every remaining refusal simply drops the adopted `Shell` —
        // never a leak, since it is an ordinary owned value.
        #[allow(clippy::cast_sign_loss, reason = "session_id checked non-negative above")]
        let Some(shell) = s.nursery.adopt(NurseryId(session_id as u64)) else {
            return Err(Error::new(
                "`agent-start`: no forked session parked under this id — it may already have \
                 started, or the turn that forked it has ended",
                1,
            ));
        };

        // 4. Narrow the child's authority against the parent's own ceiling.
        // `policy::narrow` names all six legal bases in its own diagnostic
        // when `permissions` is not one of them.
        let cwd = s.cwd.to_string_lossy();
        let child_caps = crate::policy::narrow(&s.caps, &permissions, &cwd)
            .map_err(|reason| Error::new(reason, 1))?;

        // 5. Fork the child's own log off the parent's; a mnemon spawn
        // additionally imports the parent's model-visible context, exactly
        // as `Agent::inherit_context` does for `fork_remembering`/`branch`
        // — done here, on the raw `AgentLog`, since the child is not yet an
        // `Agent` for `inherit_context` to take a `&Self` of.
        let child_id = crate::agent::fresh_id();
        let child_log = {
            let parent_log = s.log.lock().expect("log poisoned");
            let mut child_log = parent_log
                .fork(child_id, s.system.len())
                .map_err(|e| Error::new(format!("could not fork child session log: {e}"), 1))?;
            let inherited = mnemon.then(|| parent_log.inherited_context_messages());
            drop(parent_log);
            if let Some(messages) = inherited {
                child_log
                    .import_context(messages)
                    .map_err(|e| Error::new(e, 1))?;
            }
            child_log
        };

        // 6. Assemble the child at one less unit of fuel than this agent
        // holds — mirroring `Agent::fork_with`'s `Build` literal verbatim,
        // with the adopted shell and forked log standing in for
        // `fork_session`'s fresh ones.
        let fuel = s.fuel - 1;
        let child = Agent::assemble(Build {
            system: s.system.clone(),
            caps: child_caps,
            shell,
            log: child_log,
            parent: Some(s.parent),
            fuel,
            provider: ProviderHandle::new(s.provider.current()),
            focus: s.focus.clone(),
            interactive: s.interactive,
            returns: true,
            allow_schedule: s.allow_schedule,
            tools: crate::tools::tools_for(true, s.allow_schedule, fuel > 0),
            agents: s.registry.clone(),
            disk_warn_bytes: s.disk_warn_bytes,
        })
        .map_err(|e| Error::new(format!("could not assemble child session: {e}"), 1))?;

        // 7. Register, detach, and spawn — the fork-detach-register
        // mechanics every launch-only tool shares, verbatim. `spawn_async`
        // already emits the parity `Kind::ToolCall` spawn line, so this
        // handler does not double it.
        let spawned = crate::tools::agent::spawn_async(
            &s.registry,
            s.parent,
            s.mailbox.clone(),
            child,
            crate::tools::agent::AsyncSpawn {
                tool: if mnemon { "mnemon" } else { "amnemon" },
                title,
                prompt: Some(prompt),
                commitment: None,
            },
            &s.emit,
        );
        match spawned {
            Ok(child) => Ok(FOValue::Variant {
                label: "started".to_string(),
                payload: Some(Box::new(FOValue::Map {
                    entries: vec![
                        ("id".to_string(), agent_id_to_fo(child.id)),
                        ("title".to_string(), FOValue::String { value: child.title }),
                        ("log-dir".to_string(), FOValue::String { value: child.log_dir }),
                    ],
                })),
            }),
            Err(reason) => Err(Error::new(reason, 1)),
        }
    }

    /// `` `agent-list `` — the desk half of `agents`: a registry listing
    /// scoped to this agent's own descendants. Silent, like today's
    /// `AgentsTool` — no chrome, the answer carries the whole listing.
    fn agent_list(&self) -> FOValue {
        let s = &self.services;
        FOValue::List {
            items: s
                .registry
                .list(s.parent)
                .into_iter()
                .map(|a| {
                    // An agent's elapsed seconds never approaches i64::MAX;
                    // `unwrap_or` never actually saturates in practice, but
                    // keeps this door total without an `as` cast's silent
                    // wraparound.
                    let elapsed_s = i64::try_from(a.elapsed.as_secs()).unwrap_or(i64::MAX);
                    FOValue::Map {
                        entries: vec![
                            ("id".to_string(), agent_id_to_fo(a.id)),
                            ("title".to_string(), FOValue::String { value: a.title }),
                            ("elapsed-s".to_string(), FOValue::Int { value: elapsed_s }),
                            (
                                "log-dir".to_string(),
                                FOValue::String {
                                    value: a.log_dir.display().to_string(),
                                },
                            ),
                        ],
                    }
                })
                .collect(),
        }
    }

    /// `` `agent-cancel `` — the desk half of `agent-cancel`: cancel a live
    /// descendant by id, scoped exactly as `AgentRegistry::cancel_scoped`
    /// enforces. Error texts and chrome kept verbatim from today's
    /// `AgentCancelTool`.
    fn agent_cancel(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let items = payload_list(payload, "agent-cancel", "[id]", 1)?;
        let [id]: [FOValue; 1] = items
            .try_into()
            .unwrap_or_else(|_| unreachable!("payload_list already checked the length"));
        let id = payload_int(id, "agent-cancel", "id")?;
        if id < 0 {
            return Err(Error::new("`agent-cancel`: `id` must be a non-negative Int", 1));
        }
        #[allow(clippy::cast_sign_loss, reason = "id checked non-negative above")]
        let id = id as u64;

        let who = label(s.registry.title_for(id).as_deref(), id);
        s.emit.emit(Kind::ToolCall {
            tool: "agent-cancel",
            cmd: format!("cancelling agent {id}"),
            summary: Some(format!("Agent {who} cancelled.")),
        });
        let result = s.registry.cancel_scoped(s.parent, id);
        let content = match &result {
            Ok(true) => format!("cancelling agent {who}"),
            Ok(false) => format!("no live agent with id {id}"),
            Err(NotADescendant(n)) => format!(
                "agent {n} is not an agent you started; agent_cancel may only reach a descendant of yours"
            ),
        };
        s.emit.emit(Kind::ToolResult(content.clone()));
        // Both `Ok(true)` (cancelled) and `Ok(false)` (no live agent with
        // that id — a no-op) are a successful call; only a scope violation
        // raises, carrying the same text the chrome already rendered.
        match result {
            Ok(_) => Ok(FOValue::Unit),
            Err(_) => Err(Error::new(content, 1)),
        }
    }

    /// `` `message `` — the desk half of `message`: send a marked note to a
    /// live descendant by id. Error texts and chrome kept verbatim from
    /// today's `MessageTool`.
    fn message(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let items = payload_list(payload, "message", "[id, text]", 2)?;
        let [id, text]: [FOValue; 2] = items
            .try_into()
            .unwrap_or_else(|_| unreachable!("payload_list already checked the length"));
        let id = payload_int(id, "message", "id")?;
        if id < 0 {
            return Err(Error::new("`message`: `id` must be a non-negative Int", 1));
        }
        #[allow(clippy::cast_sign_loss, reason = "id checked non-negative above")]
        let id = id as u64;
        let text = payload_string(text, "message", "text")?;

        let who = label(s.registry.title_for(id).as_deref(), id);
        s.emit.emit(Kind::ToolCall {
            tool: "message",
            cmd: text.clone(),
            summary: Some(format!("Messaged agent {who}.")),
        });
        let result = s.registry.message(s.parent, id, text);
        let content = match &result {
            Ok(()) => format!("sent message to agent {who}"),
            Err(MessageError::UnknownRecipient(n)) => {
                format!("no live agent with id {n}; did it finish already?")
            }
            Err(MessageError::UnknownSender(n)) => {
                format!("cannot send from agent {n}: it is no longer live")
            }
            Err(MessageError::NotADescendant(n)) => format!(
                "agent {n} is not an agent you started; message may only reach a descendant of yours"
            ),
            Err(MessageError::RecipientInboxFull(n, reject)) => {
                format!("agent {n} did not receive the message: {reject}")
            }
        };
        s.emit.emit(Kind::ToolResult(content.clone()));
        // Success of the command is the confirmation; a refusal raises,
        // carrying the same text the chrome already rendered — the desk
        // twin of the pure confirmations' `F Unit` contract.
        match result {
            Ok(()) => Ok(FOValue::Unit),
            Err(_) => Err(Error::new(content, 1)),
        }
    }

    /// `` `schedule `` — the desk half of `schedule`: arm a self-wakeup,
    /// wrapping [`ScheduleRegistry::schedule`] exactly as
    /// `tools::schedule::ScheduleTool::dispatch` does today. `trigger` and
    /// `label` are re-decoded and re-parsed here with the same parsers the
    /// builtin's own door already ran — the both-ends validation law: a
    /// builtin's door check is never this desk's only line of defence.
    fn schedule(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        if !s.allow_schedule {
            return Err(Error::new(
                "`schedule` refused: this agent does not hold the self-wakeup grant. An \
                 agent that can wake itself indefinitely holds real authority, so the grant \
                 is off by default; relaunch with `--allow-schedule` if this session \
                 genuinely needs to schedule its own wakeups.",
                1,
            ));
        }

        let items = payload_list(payload, "schedule", "[trigger, label, prompt]", 3)?;
        let [trigger, label, prompt]: [FOValue; 3] = items
            .try_into()
            .unwrap_or_else(|_| unreachable!("payload_list already checked the length"));
        let trigger = payload_trigger(trigger, "schedule")?;
        let label = payload_label(label, "schedule")?;
        let prompt = payload_string(prompt, "schedule", "prompt")?;

        s.emit.emit(Kind::ToolCall {
            tool: "schedule",
            cmd: format!("{}: {}", trigger.describe(), prompt),
            summary: label.clone(),
        });
        let result = s.schedules.schedule(trigger, prompt, label, &s.mailbox);
        let content = match &result {
            Ok(sid) => format!("scheduled (id {sid})"),
            Err(e) => format!("could not schedule: {e}"),
        };
        s.emit.emit(Kind::ToolResult(content.clone()));
        match result {
            Ok(sid) => Ok(schedule_id_to_fo(sid)),
            Err(_) => Err(Error::new(content, 1)),
        }
    }

    /// `` `schedule-list `` — the desk half of `schedules`: a snapshot of
    /// this agent's own live scheduled wakeups. Silent, like today's
    /// `SchedulesTool` — no chrome, the answer carries the whole listing.
    fn schedule_list(&self) -> Result<FOValue, Error> {
        let s = &self.services;
        if !s.allow_schedule {
            return Err(Error::new(
                "`schedule-list` refused: this agent does not hold the self-wakeup grant. An \
                 agent that can wake itself indefinitely holds real authority, so the grant \
                 is off by default; relaunch with `--allow-schedule` if this session \
                 genuinely needs to schedule its own wakeups.",
                1,
            ));
        }
        Ok(FOValue::List {
            items: s
                .schedules
                .list()
                .into_iter()
                .map(|info| {
                    // A schedule's seconds-to-next-fire and fire count never
                    // approach i64::MAX; `unwrap_or` never actually
                    // saturates in practice, but keeps this door total
                    // without an `as` cast's silent wraparound. A cron with
                    // no further occurrence within the search horizon
                    // (vanishingly rare in practice — see `Trigger::next_delay`'s
                    // doc) reports "never" as this same saturated ceiling.
                    let next_s = info
                        .next_in
                        .map_or(i64::MAX, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
                    let fires = i64::try_from(info.fires).unwrap_or(i64::MAX);
                    FOValue::Map {
                        entries: vec![
                            ("id".to_string(), schedule_id_to_fo(info.id)),
                            ("label".to_string(), FOValue::String { value: info.label }),
                            ("trigger".to_string(), FOValue::String { value: info.trigger }),
                            ("next-s".to_string(), FOValue::Int { value: next_s }),
                            ("fires".to_string(), FOValue::Int { value: fires }),
                        ],
                    }
                })
                .collect(),
        })
    }

    /// `` `unschedule `` — the desk half of `unschedule`: remove one
    /// scheduled wakeup by id. Error texts and chrome kept verbatim from
    /// today's `UnscheduleTool`.
    fn unschedule(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        if !s.allow_schedule {
            return Err(Error::new(
                "`unschedule` refused: this agent does not hold the self-wakeup grant. An \
                 agent that can wake itself indefinitely holds real authority, so the grant \
                 is off by default; relaunch with `--allow-schedule` if this session \
                 genuinely needs to schedule its own wakeups.",
                1,
            ));
        }
        let items = payload_list(payload, "unschedule", "[id]", 1)?;
        let [id]: [FOValue; 1] = items
            .try_into()
            .unwrap_or_else(|_| unreachable!("payload_list already checked the length"));
        let id = payload_int(id, "unschedule", "id")?;
        if id < 0 {
            return Err(Error::new("`unschedule`: `id` must be a non-negative Int", 1));
        }
        #[allow(clippy::cast_sign_loss, reason = "id checked non-negative above")]
        let id = id as u64;

        s.emit.emit(Kind::ToolCall {
            tool: "unschedule",
            cmd: id.to_string(),
            summary: None,
        });
        // A no-op (no schedule with that id) is a successful call, like
        // `agent-cancel`'s: only the grant refusal above raises for this verb.
        let content = if s.schedules.unschedule(id) {
            format!("unscheduled {id}")
        } else {
            format!("no schedule with id {id}")
        };
        s.emit.emit(Kind::ToolResult(content));
        Ok(FOValue::Unit)
    }

    /// `` `commit-open `` — the desk half of `commit`: adopt the
    /// nursery-parked fork the builtin body forked, narrow it to
    /// `read-only` itself (the actor never chooses the writer's authority),
    /// and hand it to [`spawn_async`](crate::tools::agent::spawn_async)
    /// tagged [`CommitmentIntent::Write`](crate::tools::commitment::CommitmentIntent::Write)
    /// exactly as `tools::commitment::commit` does today. A relocation of
    /// that function's post-parse logic, not a rewrite; the pin-liveness
    /// refusal reads `services.pins` through
    /// [`shell_eval::commitment_card`] rather than `&Agent`.
    fn commit_open(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;

        let items = payload_list(payload, "commit-open", "[session, key, description]", 3)?;
        let [session, key, description]: [FOValue; 3] = items
            .try_into()
            .unwrap_or_else(|_| unreachable!("payload_list already checked the length"));
        let session_id = payload_int(session, "commit-open", "session")?;
        if session_id < 0 {
            return Err(Error::new("`commit-open`: `session` must be a non-negative Int", 1));
        }
        let key = payload_string(key, "commit-open", "key")?;
        if !crate::tools::commitment::valid_commitment_key(&key) {
            return Err(Error::new(
                format!(
                    "`commit-open`: `key` must look like `{}<id>` using ASCII letters, digits, \
                     `.`, `_`, or `-`",
                    shell_eval::COMMITMENT_PIN_PREFIX
                ),
                1,
            ));
        }
        let description = payload_string(description, "commit-open", "description")?;

        // 1. The /clear guard, exactly as `agent-start`'s.
        if s.generation != s.registry.generation() {
            return Err(Error::new(
                "commit-open refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale — issue \
                 commit again on your next turn",
                1,
            ));
        }

        // 2. Fuel bounds depth, not fan-out, exactly as `agent-start`'s.
        if s.fuel == 0 {
            return Err(Error::new(
                "commit-open refused: no spawn fuel remains at this depth, so commit cannot \
                 delegate any further here. Fuel bounds how deep a chain of spawns may recurse, \
                 never how many children you may start at any one depth — starting several \
                 commitments here costs nothing extra. agent-cancel on any node stops its whole \
                 live subtree regardless of depth.",
                1,
            ));
        }

        // 3. Refuse a key that is already live before ever adopting the
        // fork, so a refused call never spawns a writer — chrome kept
        // verbatim from today's `CommitTool::dispatch`.
        if shell_eval::commitment_card(&s.pins, &key).is_ok() {
            let msg = format!(
                "`{key}` is already a live commitment; verify or clear it before opening a new one"
            );
            s.emit.emit(Kind::ToolCall {
                tool: "commit",
                cmd: format!("key: {key}"),
                summary: Some(format!("Commit {key}: already live.")),
            });
            s.emit.emit(Kind::ToolResult(msg.clone()));
            return Err(Error::new(msg, 1));
        }

        // 4. Adopt the parked fork the builtin body forked.
        #[allow(clippy::cast_sign_loss, reason = "session_id checked non-negative above")]
        let Some(shell) = s.nursery.adopt(NurseryId(session_id as u64)) else {
            return Err(Error::new(
                "`commit-open`: no forked session parked under this id — it may already have \
                 started, or the turn that forked it has ended",
                1,
            ));
        };

        // 5. Narrow to `read-only` here, in the handler: the actor never
        // chooses the writer's prompt or authority.
        let cwd = s.cwd.to_string_lossy();
        let child_caps = crate::policy::narrow(&s.caps, "read-only", &cwd)
            .map_err(|reason| Error::new(reason, 1))?;

        // 6. Fork the writer's own log off the parent's, blank like an
        // `amnemon` child — a writer never inherits model-visible context.
        let child_id = crate::agent::fresh_id();
        let child_log = {
            let parent_log = s.log.lock().expect("log poisoned");
            parent_log
                .fork(child_id, s.system.len())
                .map_err(|e| Error::new(format!("could not fork child session log: {e}"), 1))?
        };

        // 7. Assemble the child at one less unit of fuel than this agent
        // holds, mirroring `agent-start`'s `Build` literal verbatim.
        let fuel = s.fuel - 1;
        let child = Agent::assemble(Build {
            system: s.system.clone(),
            caps: child_caps,
            shell,
            log: child_log,
            parent: Some(s.parent),
            fuel,
            provider: ProviderHandle::new(s.provider.current()),
            focus: s.focus.clone(),
            interactive: s.interactive,
            returns: true,
            allow_schedule: s.allow_schedule,
            tools: crate::tools::tools_for(true, s.allow_schedule, fuel > 0),
            agents: s.registry.clone(),
            disk_warn_bytes: s.disk_warn_bytes,
        })
        .map_err(|e| Error::new(format!("could not assemble child session: {e}"), 1))?;

        // 8. Register, detach, and spawn — the host-owned writer prompt,
        // untouched from `tools::commitment::writer_prompt`.
        let prompt = crate::tools::commitment::writer_prompt(&key, &description);
        let title = crate::tools::commitment::shorten(&description);
        let spawned = crate::tools::agent::spawn_async(
            &s.registry,
            s.parent,
            s.mailbox.clone(),
            child,
            crate::tools::agent::AsyncSpawn {
                tool: "commit",
                title,
                prompt: Some(prompt),
                commitment: Some(crate::tools::commitment::CommitmentIntent::Write(key)),
            },
            &s.emit,
        );
        match spawned {
            Ok(child) => Ok(FOValue::Variant {
                label: "started".to_string(),
                payload: Some(Box::new(FOValue::Map {
                    entries: vec![
                        ("id".to_string(), agent_id_to_fo(child.id)),
                        ("title".to_string(), FOValue::String { value: child.title }),
                        ("log-dir".to_string(), FOValue::String { value: child.log_dir }),
                    ],
                })),
            }),
            Err(reason) => Err(Error::new(reason, 1)),
        }
    }

    /// `` `commit-verify `` — the desk half of `verify-commitment`: adopt
    /// the nursery-parked fork the builtin body forked, narrow it to
    /// `read-only` itself, and hand it to
    /// [`spawn_async`](crate::tools::agent::spawn_async) tagged
    /// [`CommitmentIntent::Verify`](crate::tools::commitment::CommitmentIntent::Verify)
    /// exactly as `tools::commitment::verify_commitment` does today. A
    /// relocation of that function's post-parse logic, not a rewrite; the
    /// pin-liveness read goes through [`shell_eval::commitment_card`]
    /// rather than `&Agent`.
    fn commit_verify(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;

        let items = payload_list(payload, "commit-verify", "[session, key]", 2)?;
        let [session, key]: [FOValue; 2] = items
            .try_into()
            .unwrap_or_else(|_| unreachable!("payload_list already checked the length"));
        let session_id = payload_int(session, "commit-verify", "session")?;
        if session_id < 0 {
            return Err(Error::new("`commit-verify`: `session` must be a non-negative Int", 1));
        }
        let key = payload_string(key, "commit-verify", "key")?;
        if !crate::tools::commitment::valid_commitment_key(&key) {
            return Err(Error::new(
                format!(
                    "`commit-verify`: `key` must look like `{}<id>` using ASCII letters, digits, \
                     `.`, `_`, or `-`",
                    shell_eval::COMMITMENT_PIN_PREFIX
                ),
                1,
            ));
        }

        // 1. The /clear guard, exactly as `agent-start`'s.
        if s.generation != s.registry.generation() {
            return Err(Error::new(
                "commit-verify refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale — issue \
                 verify-commitment again on your next turn",
                1,
            ));
        }

        // 2. Fuel bounds depth, not fan-out, exactly as `agent-start`'s.
        if s.fuel == 0 {
            return Err(Error::new(
                "commit-verify refused: no spawn fuel remains at this depth, so \
                 verify-commitment cannot delegate any further here. Fuel bounds how deep a \
                 chain of spawns may recurse, never how many children you may start at any one \
                 depth — starting several verifications here costs nothing extra. agent-cancel \
                 on any node stops its whole live subtree regardless of depth.",
                1,
            ));
        }

        // 3. The key must be live — chrome kept verbatim from today's
        // `VerifyCommitmentTool::dispatch`.
        let card = match shell_eval::commitment_card(&s.pins, &key) {
            Ok(card) => card,
            Err(reason) => {
                s.emit.emit(Kind::ToolCall {
                    tool: "verify_commitment",
                    cmd: format!("key: {key}"),
                    summary: Some(format!("Verify {key}: {reason}.")),
                });
                s.emit.emit(Kind::ToolResult(reason.clone()));
                return Err(Error::new(reason, 1));
            }
        };

        // 4. Adopt the parked fork the builtin body forked.
        #[allow(clippy::cast_sign_loss, reason = "session_id checked non-negative above")]
        let Some(shell) = s.nursery.adopt(NurseryId(session_id as u64)) else {
            return Err(Error::new(
                "`commit-verify`: no forked session parked under this id — it may already have \
                 started, or the turn that forked it has ended",
                1,
            ));
        };

        // 5. Narrow to `read-only` here, in the handler: the actor never
        // chooses the verifier's prompt or authority.
        let cwd = s.cwd.to_string_lossy();
        let child_caps = crate::policy::narrow(&s.caps, "read-only", &cwd)
            .map_err(|reason| Error::new(reason, 1))?;

        // 6. Fork the verifier's own log off the parent's, blank like an
        // `amnemon` child.
        let child_id = crate::agent::fresh_id();
        let child_log = {
            let parent_log = s.log.lock().expect("log poisoned");
            parent_log
                .fork(child_id, s.system.len())
                .map_err(|e| Error::new(format!("could not fork child session log: {e}"), 1))?
        };

        // 7. Assemble the child at one less unit of fuel than this agent
        // holds, mirroring `agent-start`'s `Build` literal verbatim.
        let fuel = s.fuel - 1;
        let child = Agent::assemble(Build {
            system: s.system.clone(),
            caps: child_caps,
            shell,
            log: child_log,
            parent: Some(s.parent),
            fuel,
            provider: ProviderHandle::new(s.provider.current()),
            focus: s.focus.clone(),
            interactive: s.interactive,
            returns: true,
            allow_schedule: s.allow_schedule,
            tools: crate::tools::tools_for(true, s.allow_schedule, fuel > 0),
            agents: s.registry.clone(),
            disk_warn_bytes: s.disk_warn_bytes,
        })
        .map_err(|e| Error::new(format!("could not assemble child session: {e}"), 1))?;

        // 8. Register, detach, and spawn — the host-owned verifier prompt,
        // untouched from `tools::commitment::verifier_prompt`.
        let summary = crate::card::summary_line(&card);
        let prompt = crate::tools::commitment::verifier_prompt(&key, &card, &summary);
        let title = crate::tools::commitment::shorten(summary.lines().next().unwrap_or(&summary));
        let spawned = crate::tools::agent::spawn_async(
            &s.registry,
            s.parent,
            s.mailbox.clone(),
            child,
            crate::tools::agent::AsyncSpawn {
                tool: "verify_commitment",
                title,
                prompt: Some(prompt),
                commitment: Some(crate::tools::commitment::CommitmentIntent::Verify(key)),
            },
            &s.emit,
        );
        match spawned {
            Ok(child) => Ok(FOValue::Variant {
                label: "started".to_string(),
                payload: Some(Box::new(FOValue::Map {
                    entries: vec![
                        ("id".to_string(), agent_id_to_fo(child.id)),
                        ("title".to_string(), FOValue::String { value: child.title }),
                        ("log-dir".to_string(), FOValue::String { value: child.log_dir }),
                    ],
                })),
            }),
            Err(reason) => Err(Error::new(reason, 1)),
        }
    }

    /// `` `reply `` — the desk half of the `reply` builtin: stage the
    /// payload into the reply cell for `Agent::apply`'s drain to lift into
    /// a `Replied` terminal once the batch drains, and emit today's
    /// "Replied to parent" chrome (`tools::reply::ReplyTool::dispatch`,
    /// kept verbatim). Refused on every non-returning agent — the
    /// conversing trunk and each `/branch` child alike — keyed on the
    /// captured `returns` bit, never on trunk-ness: the desk equivalent of
    /// `Gate::Returns`.
    fn reply(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        if !s.returns {
            return Err(Error::new(
                "reply refused: you converse with the user; you do not return. `reply` \
                 terminates a returning agent's run and hands its value to a parent — the \
                 interactive trunk and every /branch child instead keep talking, turn after \
                 turn, and hold no `reply` to call.",
                1,
            ));
        }
        let items = payload_list(payload, "reply", "[value]", 1)?;
        let [value]: [FOValue; 1] = items
            .try_into()
            .unwrap_or_else(|_| unreachable!("payload_list already checked the length"));
        let display =
            shell_eval::ral_value_to_text(&RalValue::from(value.clone())).unwrap_or_default();
        s.emit.emit(Kind::ToolCall {
            tool: "reply",
            cmd: if display.is_empty() {
                "(empty reply)".into()
            } else {
                display
            },
            summary: Some("Replied to parent.".to_string()),
        });
        s.reply.set(value);
        Ok(FOValue::Unit)
    }
}

/// [`SurfaceApplier::live`]/[`SurfaceApplier::deferred`] apply one surfaced
/// value or deferred batch, decoding it and folding a `` `pin ``/`` `unpin ``
/// disposition into the pin mirror exactly as `shell_eval::run_shell`'s own
/// drain loop does — because it *is* that loop's logic, extracted so
/// [`DeskBinding`] can share it rather than risk a second copy drifting
/// from the first (`dev/docs/260715_tools_to_builtins.md`, "`HostServices`
/// and the desk").
pub(crate) struct SurfaceApplier {
    pub(crate) emit: Emitter,
    pub(crate) pins: Option<PinDigests>,
}

impl SurfaceApplier {
    /// Apply one live [`Event::Surface`] value.
    pub(crate) fn live(&self, val: FOValue) {
        if let Some(kind) = shell_eval::accepted_surface(&RalValue::from(val), &self.emit) {
            if let Some(pins) = &self.pins
                && let Ok(mut m) = pins.lock()
            {
                match &kind {
                    Kind::Pin { key, card } => {
                        m.insert(key.clone(), shell_eval::PinDigest::new(key, card.clone()));
                    }
                    Kind::Unpin { key } => {
                        m.remove(key);
                    }
                    _ => {}
                }
            }
            self.emit.emit(kind);
        }
    }

    /// Apply one deferred worker's settled [`Event::DeferredSurface`]
    /// batch, in order. Never touches the pin mirror: a deferred flush
    /// renders identically to the live path but carries no live-pin state
    /// of its own.
    pub(crate) fn deferred(&self, batch: Vec<FOValue>) {
        for val in batch {
            if let Some(kind) = shell_eval::accepted_surface(&RalValue::from(val), &self.emit) {
                self.emit.emit(kind);
            }
        }
    }
}

/// The inert desk `Agent::run_shell` installs once its own real desk's call
/// is done, so nothing keeps that call's `Emitter` (and the rest of its
/// `HostServices` capture) alive past the call that owns it — `engine.desk`
/// is unobservable between calls, since nothing dispatches without
/// `run_shell` installing a fresh real desk first. Answers the same honest
/// absence `Shell::enquire` itself gives when no desk is installed at all,
/// in the unreachable case something reads it directly.
pub(crate) struct AbsentDesk;

impl EnquiryDesk for AbsentDesk {
    fn enquire(&self, _req: FOValue) -> Result<FOValue, Error> {
        Err(Error::new("this host answers no enquiries", 1))
    }
}

/// The identity transport's drain-then-handle adapter.
///
/// Under `IdentityTransport`, `Shell::enquire` calls straight into whatever
/// desk is installed, synchronously, mid-`dispatch` — before
/// `dispatch_to_report`'s own drain loop ever runs, since that loop only
/// starts once `dispatch` returns. Any surface frames this turn already
/// emitted are therefore still queued, undrained, on the transport's event
/// channel; answering the enquiry before applying them would let a
/// handler's own chrome render ahead of the turn's earlier surface output.
/// [`Self::enquire`] drains every frame already queued through `apply`
/// before delegating to `desk`, so the identity path renders in the same
/// order the wire path's live drain loop would
/// (`docs/ral-wiki/decisions/260706_enquiry-channel.md`, "Identity
/// binding").
pub(crate) struct DeskBinding {
    pub(crate) desk: Arc<ExarchDesk>,
    pub(crate) events: Arc<EventReceiver>,
    pub(crate) apply: SurfaceApplier,
}

impl EnquiryDesk for DeskBinding {
    fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
        while let Some(frame) = self.events.try_recv() {
            match frame {
                Frame::Event(_, Event::Surface(val)) => self.apply.live(val),
                Frame::Event(_, Event::DeferredSurface(batch)) => self.apply.deferred(batch),
                _ => {}
            }
        }
        self.desk.handle(req)
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::bus::{BusReceiver, Inbox, NO_FOCUS, channel};
    use crate::provider::{Provider, ProviderKind, scripted::{Reply, Script}};
    use ral_core::transport::{DispatchId, IdentityTransport, Program, Transport, Turn};
    use ral_core::{RequestedTerminalAccess, TurnIo, TurnStdin};

    fn dummy_emitter() -> (Emitter, BusReceiver) {
        let (tx, rx) = channel();
        (Emitter::new(tx, 0), rx)
    }

    /// A session log under a scratch dir unique to this call — tests run in
    /// parallel, and every call in this module used to share one fixed
    /// `sessions_root`/id-0 pair, racing `remove_dir_all` against a sibling
    /// test's `create_dir_all` for the identical path.
    fn fresh_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "exarch-desk-test-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        AgentLog::root(&root, 0, "test", "test", 0).expect("session log")
    }

    fn scripted_provider() -> Arc<Provider> {
        Arc::new(Provider::scripted("test-model", ProviderKind::Openai, Script::new()))
    }

    fn desk() -> ExarchDesk {
        let (emit, _rx) = dummy_emitter();
        ExarchDesk {
            services: HostServices {
                registry: AgentRegistry::new(),
                parent: 0,
                mailbox: Inbox::new().mailbox(),
                emit,
                provider: ProviderHandle::new(scripted_provider()),
                caps: Capabilities::root(),
                cwd: PathBuf::from("/"),
                fuel: 3,
                returns: true,
                allow_schedule: false,
                reply: ReplyCell::default(),
                schedules: ScheduleRegistry::new(),
                pins: Arc::default(),
                log: Arc::new(Mutex::new(fresh_log())),
                system: String::new(),
                focus: Arc::new(AtomicU64::new(NO_FOCUS)),
                interactive: false,
                nursery: Nursery::default(),
                generation: 0,
                disk_warn_bytes: None,
            },
        }
    }

    /// [`desk`] with the self-wakeup grant held, so the schedule family's
    /// desk-level tests can exercise `` `schedule ``/`` `schedule-list ``/
    /// `` `unschedule `` past their grant check.
    fn granted_desk() -> ExarchDesk {
        let (emit, _rx) = dummy_emitter();
        ExarchDesk {
            services: HostServices {
                registry: AgentRegistry::new(),
                parent: 0,
                mailbox: Inbox::new().mailbox(),
                emit,
                provider: ProviderHandle::new(scripted_provider()),
                caps: Capabilities::root(),
                cwd: PathBuf::from("/"),
                fuel: 3,
                returns: true,
                allow_schedule: true,
                reply: ReplyCell::default(),
                schedules: ScheduleRegistry::new(),
                pins: Arc::default(),
                log: Arc::new(Mutex::new(fresh_log())),
                system: String::new(),
                focus: Arc::new(AtomicU64::new(NO_FOCUS)),
                interactive: false,
                nursery: Nursery::default(),
                generation: 0,
                disk_warn_bytes: None,
            },
        }
    }

    /// Build a desk whose parent id is actually registered in its own
    /// registry (the precondition [`AgentRegistry::register`] enforces for
    /// any child), so `agent-start`/`agent-cancel`/`message` can run
    /// end to end — unlike the bare [`desk`] helper above, which never
    /// spawns. Returns the desk, the registry (so a test can register more
    /// tree nodes or bump the generation), and the parent's own [`Inbox`]
    /// (so a test can drain a spawned child's settled result).
    fn spawnable_desk(fuel: u32) -> (ExarchDesk, AgentRegistry, Inbox) {
        let parent_inbox = Inbox::new();
        let registry = AgentRegistry::new();
        // Minted, never a literal — a spawned child's own id also comes
        // from `crate::agent::fresh_id()`'s single process-wide counter, so
        // a hardcoded parent id here can collide with the very first child
        // this desk ever spawns (observed: both mint `0` when this is the
        // first agent the process creates), silently overwriting the
        // parent's own registry entry with the child's and corrupting the
        // tree — a hang downstream, not a clean failure.
        let parent_id = crate::agent::fresh_id();
        registry.register(crate::agent_registry::Registration {
            id: parent_id,
            parent: None,
            ceiling: false,
            title: "parent".into(),
            log_dir: PathBuf::from("/tmp/parent"),
            cancel: crate::cancel::Token::new(),
            eval_root: None,
            turn_scope: None,
            mailbox: parent_inbox.mailbox(),
            provider: ProviderHandle::new(scripted_provider()),
        });
        let (emit, _rx) = dummy_emitter();
        let desk = ExarchDesk {
            services: HostServices {
                registry: registry.clone(),
                parent: parent_id,
                mailbox: parent_inbox.mailbox(),
                emit,
                provider: ProviderHandle::new(scripted_provider()),
                caps: Capabilities::root(),
                cwd: PathBuf::from("/"),
                fuel,
                returns: true,
                allow_schedule: false,
                reply: ReplyCell::default(),
                schedules: ScheduleRegistry::new(),
                pins: Arc::default(),
                log: Arc::new(Mutex::new(fresh_log())),
                system: String::new(),
                focus: Arc::new(AtomicU64::new(NO_FOCUS)),
                interactive: false,
                nursery: Nursery::default(),
                generation: registry.generation(),
                disk_warn_bytes: None,
            },
        };
        (desk, registry, parent_inbox)
    }

    /// Poll `inbox` for the next turn-boundary deliverable — a spawned
    /// child's settled [`crate::bus::AgentResult`] lands here — mirroring
    /// `tools::commitment`'s own `wait_for_settle`.
    fn wait_for_settle(inbox: &Inbox) -> crate::bus::Turn {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(turn) = inbox.drain_turn() {
                return turn;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child did not settle within the timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A unique scratch directory per test, mirroring `tests/agent_apply.rs`'s
    /// own `tmp` helper.
    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("exarch-desk-full-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// A `reply` tool call carrying `result` — the JSON `reply` tool is
    /// still installed alongside the harness builtins during this
    /// migration's measurement window, and a *returning* spawned child must
    /// call it (or, once Step 4 lands, the `reply` builtin) to settle
    /// cleanly in one round; a plain text completion instead nudges the
    /// child to try again, consuming a second scripted stage this module's
    /// scripts never provision.
    fn reply_call(result: &str) -> genai::chat::ToolCall {
        genai::chat::ToolCall {
            call_id: "reply-1".into(),
            fn_name: "reply".into(),
            fn_arguments: serde_json::json!({ "result": result }),
            thought_signatures: None,
        }
    }

    /// A root shell to fork children off of — booted exactly once per test,
    /// never re-booted per spawn. `boot_shell` resets process-global signal
    /// state (`ral_core::process::clear`/`install_handlers`,
    /// `cancel::install`), the same ceremony `Agent::root` runs exactly
    /// once for the whole fleet; every fork in production reaches its shell
    /// through `Shell::fork_session` on that one already-booted root, never
    /// by re-booting. A test that instead calls `boot_shell` once per spawn
    /// races that global reset against whichever sibling child's turn is
    /// already in flight on its own worker thread, and deadlocks rather
    /// than erroring.
    fn root_shell() -> ral_core::Shell {
        crate::bootstrap::boot_shell()
    }

    /// A shell to park in the nursery for an `agent-start` enquiry — the
    /// same fork [`ral_core::Shell::fork_into_nursery`] performs on the
    /// body's own live session, applied here to a test's shared
    /// [`root_shell`] so every spawn in one test forks the identical
    /// already-booted root (see [`root_shell`]'s doc for why re-booting per
    /// spawn deadlocks).
    fn forkable_child_shell(root: &ral_core::Shell) -> ral_core::Shell {
        root.fork_session()
    }

    /// An unrecognised class answers the extension law's error, naming the
    /// class.
    #[test]
    fn unknown_class_answers_the_extension_error() {
        let err = desk()
            .handle(FOValue::Variant {
                label: "no-such-class".into(),
                payload: None,
            })
            .expect_err("an unrecognised class must not answer Ok");
        assert_eq!(err.message, "unrecognised enquiry class `no-such-class`");
    }

    /// A non-variant request errors naming the expected shape, rather than
    /// panicking or silently defaulting.
    #[test]
    fn non_variant_request_errors_didactically() {
        let err = desk()
            .handle(FOValue::Unit)
            .expect_err("a non-variant request must not answer Ok");
        assert!(
            err.message.contains("must be a variant"),
            "error must name the expected shape, got: {}",
            err.message
        );
    }

    /// `DeskBinding` drains every surface frame already queued on the
    /// transport's event channel before it hands the request to the inner
    /// desk — the drain-then-handle adapter's law
    /// (`docs/ral-wiki/decisions/260706_enquiry-channel.md`, "Identity
    /// binding"): a handler's own chrome must never jump ahead of the
    /// turn's earlier surface output.
    ///
    /// Dispatches a real turn through a real `IdentityTransport` that
    /// surfaces an `` `unpin `` event via the ground `surface` builtin —
    /// the actual path a surfaced value takes onto the transport's event
    /// queue, left undrained until something calls `events().try_recv()`.
    /// `DeskBinding::enquire` is then called directly (bare, off the
    /// dispatching thread — this test exercises the adapter's own drain
    /// logic, not `Shell::enquire`'s routing, which core's own suite
    /// covers) and both effects of one call are checked: the queued
    /// surface reached the bus, and the inner desk answered.
    #[test]
    fn desk_binding_drains_pending_surfaces_before_handling() {
        let shell = ral_core::Shell::new(ral_core::io::TerminalState::default());
        let transport = IdentityTransport::new(shell);
        transport.dispatch(
            DispatchId(1),
            Turn {
                program: Program::Source(r#"surface `unpin [key: "test-marker"]"#.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: TurnStdin::Empty,
            },
        );

        let (emit, rx) = dummy_emitter();
        let binding = DeskBinding {
            desk: Arc::new(desk()),
            events: transport.events_shared(),
            apply: SurfaceApplier { emit, pins: None },
        };

        let answer = binding
            .enquire(FOValue::Variant {
                label: "probe".into(),
                payload: None,
            })
            .expect_err("the stub desk answers every class with the extension-law error");
        assert_eq!(answer.message, "unrecognised enquiry class `probe`");

        // The queued surface frame must have reached the bus by the time
        // `enquire` returns — proof `apply` ran, not just that `handle` did.
        let mut saw_unpin = false;
        while let Ok(event) = rx.try_recv() {
            if let Kind::Unpin { key } = event.kind {
                assert_eq!(key, "test-marker");
                saw_unpin = true;
            }
        }
        assert!(saw_unpin, "the drained surface frame must render onto the bus");

        // And the transport's queue is now empty: `enquire` consumed
        // everything pending, never leaving a straggler for a later drain
        // to reorder ahead of.
        assert!(
            binding.events.try_recv().is_none(),
            "enquire must drain the transport's event queue fully"
        );
    }

    /// A valid `agent-start` adopts the parked fork, spawns the child, and
    /// the receipt names it; the child's own reply then lands in the
    /// parent's inbox once it settles — the whole round trip
    /// `tools::agent::dispatch_spawn` runs today, relocated onto the desk.
    #[test]
    fn agent_start_spawns_and_delivers_result_to_parent_inbox() {
        let (desk, _registry, parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new().then(Reply::tool_calls(vec![reply_call("hi from child")])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        let answer = desk
            .handle(FOValue::Variant {
                label: "agent-start".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::Variant {
                            label: "amnemon".into(),
                            payload: None,
                        },
                        FOValue::String {
                            value: "say hi".into(),
                        },
                        FOValue::String {
                            value: "helper".into(),
                        },
                        FOValue::Variant {
                            label: "confined".into(),
                            payload: None,
                        },
                    ],
                })),
            })
            .expect("a valid agent-start must succeed");

        let FOValue::Variant { label, payload: Some(payload) } = answer else {
            panic!("expected a `started` variant");
        };
        assert_eq!(label, "started");
        let FOValue::Map { entries } = *payload else {
            panic!("expected a record payload");
        };
        assert!(
            entries.iter().any(|(k, v)| k == "title"
                && matches!(v, FOValue::String { value } if value == "helper")),
            "the receipt must carry the child's title"
        );
        assert!(
            entries.iter().any(|(k, _)| k == "id"),
            "the receipt must carry the child's id"
        );
        assert!(
            entries.iter().any(|(k, _)| k == "log-dir"),
            "the receipt must carry the child's log directory"
        );

        match wait_for_settle(&parent_inbox) {
            crate::bus::Turn::Agent(result) => {
                assert!(
                    result.text.contains("hi from child"),
                    "the child's reply must reach the parent's inbox, got: {}",
                    result.text
                );
            }
            other => panic!("expected an Agent result turn, got {other:?}"),
        }
    }

    /// A spawner with no fuel left is refused with the exhaustion text —
    /// fuel bounds how deep a chain may recurse, and never touches the
    /// parent's own fuel.
    #[test]
    fn agent_start_refuses_at_zero_fuel_with_the_exhaustion_text() {
        let (desk, _registry, _parent_inbox) = spawnable_desk(0);
        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);
        let err = desk
            .handle(FOValue::Variant {
                label: "agent-start".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::Variant {
                            label: "amnemon".into(),
                            payload: None,
                        },
                        FOValue::String { value: "hi".into() },
                        FOValue::String {
                            value: "helper".into(),
                        },
                        FOValue::Variant {
                            label: "confined".into(),
                            payload: None,
                        },
                    ],
                })),
            })
            .expect_err("zero fuel must refuse");
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
            desk.services.nursery.adopt(session).is_some(),
            "a fuel refusal happens before adopt, so the parked fork must \
             stay for the turn guard to reap, never claimed by a refused call"
        );
    }

    /// N sibling `agent-start`s in one turn all succeed off the same
    /// captured fuel: fuel bounds how deep a chain may recurse, never how
    /// many children may start at any one depth. The parent's own fuel is
    /// never debited, so a second and third sibling cost nothing extra.
    #[test]
    fn fuel_bounds_depth_not_fanout() {
        let (desk, _registry, parent_inbox) = spawnable_desk(1);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new()
                .then(Reply::tool_calls(vec![reply_call("a")]))
                .then(Reply::tool_calls(vec![reply_call("b")]))
                .then(Reply::tool_calls(vec![reply_call("c")])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        for i in 0..3 {
            let shell = forkable_child_shell(&root);
            let session = desk.services.nursery.park(shell);
            let answer = desk.handle(FOValue::Variant {
                label: "agent-start".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::Variant {
                            label: "amnemon".into(),
                            payload: None,
                        },
                        FOValue::String { value: "go".into() },
                        FOValue::String {
                            value: format!("t{i}"),
                        },
                        FOValue::Variant {
                            label: "confined".into(),
                            payload: None,
                        },
                    ],
                })),
            });
            assert!(
                answer.is_ok(),
                "sibling {i} must not be refused for lack of fuel — fuel \
                 bounds depth, not fan-out"
            );
        }

        let mut texts = Vec::new();
        for _ in 0..3 {
            match wait_for_settle(&parent_inbox) {
                crate::bus::Turn::Agent(result) => texts.push(result.text),
                other => panic!("expected an Agent result turn, got {other:?}"),
            }
        }
        for want in ["a", "b", "c"] {
            assert!(
                texts.iter().any(|t| t.contains(want)),
                "all three siblings must settle, got: {texts:?}"
            );
        }
    }

    /// A desk installed before `/clear` refuses `agent-start` raised after
    /// it: the fuel/caps/grant this call captured are stale the instant the
    /// registry's shared generation counter — bumped by a `/clear`
    /// anywhere in the fleet — moves past what was snapshotted at install.
    #[test]
    fn agent_start_refuses_after_clear() {
        let (desk, registry, _parent_inbox) = spawnable_desk(3);
        registry.clear_subtree(desk.services.parent); // the /clear gesture: bumps the shared generation
        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);
        let err = desk
            .handle(FOValue::Variant {
                label: "agent-start".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::Variant {
                            label: "amnemon".into(),
                            payload: None,
                        },
                        FOValue::String { value: "hi".into() },
                        FOValue::String {
                            value: "helper".into(),
                        },
                        FOValue::Variant {
                            label: "confined".into(),
                            payload: None,
                        },
                    ],
                })),
            })
            .expect_err("a stale generation must refuse");
        assert!(
            err.message.contains("cleared"),
            "must name the /clear cause, got: {}",
            err.message
        );
    }

    /// A refusal that happens after `nursery.adopt` (a bad permissions
    /// label) simply drops the adopted `Shell` like any other owned value —
    /// never left in the nursery to be adopted twice.
    #[test]
    fn refused_enquiry_leaves_the_nursery_empty() {
        let (desk, _registry, _parent_inbox) = spawnable_desk(3);
        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);
        let err = desk
            .handle(FOValue::Variant {
                label: "agent-start".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::Variant {
                            label: "amnemon".into(),
                            payload: None,
                        },
                        FOValue::String { value: "hi".into() },
                        FOValue::String {
                            value: "helper".into(),
                        },
                        FOValue::Variant {
                            label: "bogus".into(),
                            payload: None,
                        },
                    ],
                })),
            })
            .expect_err("an unknown permissions label must refuse");
        assert!(
            err.message.contains("confined"),
            "must name the legal bases, got: {}",
            err.message
        );
        assert!(
            desk.services.nursery.adopt(session).is_none(),
            "a refusal downstream of adopt must not leave the fork \
             re-adoptable — it was already claimed and simply drops"
        );
    }

    /// `message`/`agent-cancel` may only reach a proper descendant of the
    /// calling agent — never a sibling, an ancestor, or itself — mirroring
    /// `AgentRegistry`'s own scoping tests at the desk entry point.
    #[test]
    fn message_and_cancel_scope_to_descendants() {
        let (desk_root, registry, _root_inbox) = spawnable_desk(3);
        // Every id here is minted, never a literal — see `spawnable_desk`'s
        // own doc on why a hardcoded id can collide with the process-wide
        // `fresh_id` counter and corrupt the tree.
        let root_id = desk_root.services.parent;
        let mid = crate::agent::fresh_id();
        let sibling = crate::agent::fresh_id();
        let grandchild = crate::agent::fresh_id();
        // root -> mid -> grandchild, and root -> sibling (mid's sibling).
        for (id, parent) in [(mid, root_id), (sibling, root_id), (grandchild, mid)] {
            registry.register(crate::agent_registry::Registration {
                id,
                parent: Some(parent),
                ceiling: true,
                title: format!("a{id}"),
                log_dir: PathBuf::from(format!("/tmp/{id}")),
                cancel: crate::cancel::Token::new(),
                eval_root: None,
                turn_scope: None,
                mailbox: Inbox::new().mailbox(),
                provider: ProviderHandle::new(scripted_provider()),
            });
        }

        let mut desk1 = desk_root;
        desk1.services.parent = mid;

        let sibling_fo = i64::try_from(sibling).expect("small test id");
        let grandchild_fo = i64::try_from(grandchild).expect("small test id");
        let root_fo = i64::try_from(root_id).expect("small test id");

        let err = desk1
            .handle(FOValue::Variant {
                label: "message".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int { value: sibling_fo },
                        FOValue::String { value: "hi".into() },
                    ],
                })),
            })
            .expect_err("a message to a sibling must be refused");
        assert_eq!(
            err.message,
            format!(
                "agent {sibling} is not an agent you started; message may only reach a descendant of yours"
            )
        );

        assert!(
            desk1
                .handle(FOValue::Variant {
                    label: "message".into(),
                    payload: Some(Box::new(FOValue::List {
                        items: vec![
                            FOValue::Int { value: grandchild_fo },
                            FOValue::String { value: "hi".into() },
                        ],
                    })),
                })
                .is_ok(),
            "a message to a proper descendant must succeed"
        );

        let cancel_err = desk1
            .handle(FOValue::Variant {
                label: "agent-cancel".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![FOValue::Int { value: root_fo }],
                })),
            })
            .expect_err("cancelling an ancestor must be refused");
        assert_eq!(
            cancel_err.message,
            format!(
                "agent {root_id} is not an agent you started; agent_cancel may only reach a descendant of yours"
            )
        );

        assert!(
            desk1
                .handle(FOValue::Variant {
                    label: "agent-cancel".into(),
                    payload: Some(Box::new(FOValue::List {
                        items: vec![FOValue::Int { value: grandchild_fo }],
                    })),
                })
                .is_ok(),
            "cancelling a proper descendant must succeed"
        );
    }

    /// A desk handler observes the turn's earlier surfaced values already
    /// drained before it answers — the identity drain-then-handle adapter's
    /// law (`docs/ral-wiki/decisions/260706_enquiry-channel.md`, "Identity
    /// binding") — exercised here with a real `amnemon` spawn rather than
    /// the stub `probe` class `desk_binding_drains_pending_surfaces_before_handling`
    /// uses: the surfaced `` `unpin `` must reach the bus before the
    /// spawn's own `Kind::ToolCall` chrome line.
    #[test]
    fn surface_then_spawn_observes_the_surface_first() {
        let dir = tmp("surface-then-spawn");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, rx) = channel();
        let emit = Emitter::new(tx, session.id);
        let _ = session.run_shell(
            "call-1".to_string(),
            r#"surface `unpin [key: "test-marker"]; amnemon #'go'# "t" `confined"#,
            5,
            &emit,
        );
        drop(emit);

        let mut saw_unpin = false;
        let mut saw_spawn = false;
        for event in rx {
            match event.kind {
                Kind::Unpin { .. } => saw_unpin = true,
                Kind::ToolCall { tool: "amnemon", .. } => {
                    assert!(
                        saw_unpin,
                        "the surfaced unpin must be observed before the spawn's ToolCall line"
                    );
                    saw_spawn = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_spawn,
            "the amnemon spawn's ToolCall chrome must have been emitted"
        );
    }

    /// `` `schedule `` is refused outright when this agent does not hold the
    /// self-wakeup grant, naming the `--allow-schedule` flag — before any
    /// decoding of its payload.
    #[test]
    fn schedule_refused_without_the_grant() {
        let err = desk()
            .handle(FOValue::Variant {
                label: "schedule".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Variant {
                            label: "after".into(),
                            payload: Some(Box::new(FOValue::String { value: "1s".into() })),
                        },
                        FOValue::Variant {
                            label: "none".into(),
                            payload: None,
                        },
                        FOValue::String { value: "wake".into() },
                    ],
                })),
            })
            .expect_err("schedule must be refused without the grant");
        assert!(
            err.message.contains("--allow-schedule"),
            "must name the grant flag, got: {}",
            err.message
        );
    }

    /// `` `schedule-list `` is likewise refused without the grant.
    #[test]
    fn schedule_list_refused_without_the_grant() {
        let err = desk()
            .handle(FOValue::Variant {
                label: "schedule-list".into(),
                payload: None,
            })
            .expect_err("schedule-list must be refused without the grant");
        assert!(
            err.message.contains("--allow-schedule"),
            "must name the grant flag, got: {}",
            err.message
        );
    }

    /// `` `unschedule `` is likewise refused without the grant.
    #[test]
    fn unschedule_refused_without_the_grant() {
        let err = desk()
            .handle(FOValue::Variant {
                label: "unschedule".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![FOValue::Int { value: 0 }],
                })),
            })
            .expect_err("unschedule must be refused without the grant");
        assert!(
            err.message.contains("--allow-schedule"),
            "must name the grant flag, got: {}",
            err.message
        );
    }

    /// A `` `none `` label sends the absent label so the registry mints its
    /// `sched-{id}` default, rather than an empty-string sentinel.
    #[test]
    fn none_label_takes_the_sched_id_default() {
        let desk = granted_desk();
        let answer = desk
            .handle(FOValue::Variant {
                label: "schedule".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Variant {
                            label: "after".into(),
                            payload: Some(Box::new(FOValue::String { value: "1s".into() })),
                        },
                        FOValue::Variant {
                            label: "none".into(),
                            payload: None,
                        },
                        FOValue::String { value: "wake".into() },
                    ],
                })),
            })
            .expect("a valid schedule with `none` must succeed");
        let FOValue::Int { value: id } = answer else {
            panic!("expected an Int id");
        };
        let listing = desk.services.schedules.list();
        assert_eq!(listing.len(), 1, "exactly one live schedule");
        assert_eq!(listing[0].id, u64::try_from(id).expect("small test id"));
        assert_eq!(listing[0].label, format!("sched-{id}"));
    }

    /// `` `schedule ``/`` `schedule-list ``/`` `unschedule `` round trip
    /// through the desk arms: schedule one, see it listed with the fields
    /// it was given, remove it, and see the listing empty again.
    #[test]
    fn schedules_lists_what_schedule_registered() {
        let desk = granted_desk();
        let answer = desk
            .handle(FOValue::Variant {
                label: "schedule".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Variant {
                            label: "after".into(),
                            payload: Some(Box::new(FOValue::String { value: "2h".into() })),
                        },
                        FOValue::Variant {
                            label: "some".into(),
                            payload: Some(Box::new(FOValue::String { value: "nightly".into() })),
                        },
                        FOValue::String { value: "wake".into() },
                    ],
                })),
            })
            .expect("a valid schedule must succeed");
        let FOValue::Int { value: id } = answer else {
            panic!("expected an Int id");
        };

        let listing = desk
            .handle(FOValue::Variant {
                label: "schedule-list".into(),
                payload: None,
            })
            .expect("schedule-list must succeed");
        let FOValue::List { items } = listing else {
            panic!("expected a List");
        };
        assert_eq!(items.len(), 1);
        let FOValue::Map { entries } = &items[0] else {
            panic!("expected a record");
        };
        assert!(
            entries.iter().any(
                |(k, v)| k == "label" && matches!(v, FOValue::String { value } if value == "nightly")
            ),
            "the listing must carry the label given at schedule time"
        );
        assert!(
            entries.iter().any(
                |(k, v)| k == "trigger" && matches!(v, FOValue::String { value } if value == "after 2h")
            ),
            "the listing must carry the trigger's description"
        );

        desk.handle(FOValue::Variant {
            label: "unschedule".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::Int { value: id }],
            })),
        })
        .expect("unschedule must succeed");

        let listing = desk
            .handle(FOValue::Variant {
                label: "schedule-list".into(),
                payload: None,
            })
            .expect("schedule-list must succeed");
        let FOValue::List { items } = listing else {
            panic!("expected a List");
        };
        assert!(items.is_empty(), "the schedule must be gone after unschedule");
    }

    // ── the commitment pair ──────────────────────────────────────────────
    //
    // Ports of `tools::commitment`'s own dispatch-level integration tests to
    // this desk entry point: the assertions survive verbatim, only the
    // entry changes (`ExarchDesk::handle` directly, after a body-side fork
    // parked into the nursery, in place of `CommitTool`/
    // `VerifyCommitmentTool::dispatch`). The originals stay in
    // `tools/commitment.rs`, untouched and still passing — coexistence
    // during the measurement window.

    fn text_card(text: &str) -> crate::card::Card {
        crate::card::Card(vec![crate::card::Mark::Text {
            spans: vec![crate::card::Span {
                role: None,
                text: text.into(),
            }],
        }])
    }

    /// `commit-open` refuses a key that is already a live commitment before
    /// ever adopting the parked fork — chrome text kept verbatim from
    /// today's `CommitTool::dispatch` — port of
    /// `tools::commitment::commit_refuses_when_key_already_live`.
    #[test]
    fn commit_refuses_when_key_already_live() {
        let key = "commitment:abc";
        let (desk, _registry, _parent_inbox) = spawnable_desk(3);
        desk.services
            .pins
            .lock()
            .expect("pin register poisoned")
            .insert(key.to_string(), shell_eval::PinDigest::new(key, text_card("already open")));

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        let err = desk
            .handle(FOValue::Variant {
                label: "commit-open".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::String { value: key.into() },
                        FOValue::String {
                            value: "do the thing".into(),
                        },
                    ],
                })),
            })
            .expect_err("a live key must refuse");
        assert!(
            err.message.contains("already a live commitment"),
            "must refuse without ever forking a writer: {}",
            err.message
        );
        assert!(
            desk.services.nursery.adopt(session).is_some(),
            "a refusal ahead of adopt must leave the parked fork re-adoptable — no writer was \
             ever forked"
        );
    }

    /// A valid `commit-open` adopts the parked fork, spawns a read-only
    /// writer, and the writer's own well-formed criteria card settles
    /// tagged for the parent to open — port of
    /// `tools::commitment::writer_settles_the_new_commitment_open_for_the_parent`.
    #[test]
    fn writer_settles_the_new_commitment_open_for_the_parent() {
        let key = "commitment:abc";
        let (desk, _registry, parent_inbox) = spawnable_desk(3);
        let card_json = serde_json::json!({
            "kind": "commitment_card",
            "commitment_key": key,
            "summary": "ship the thing",
            "criteria": [{ "id": "tests", "text": "tests pass" }],
        })
        .to_string();
        let provider = Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new().then(Reply::tool_calls(vec![reply_call(&card_json)])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        let answer = desk
            .handle(FOValue::Variant {
                label: "commit-open".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::String { value: key.into() },
                        FOValue::String {
                            value: "ship the thing, tests must pass".into(),
                        },
                    ],
                })),
            })
            .expect("a valid commit-open must succeed");

        let FOValue::Variant { label, payload: Some(payload) } = answer else {
            panic!("expected a `started` variant");
        };
        assert_eq!(label, "started");
        let FOValue::Map { entries } = *payload else {
            panic!("expected a record payload");
        };
        assert!(entries.iter().any(|(k, _)| k == "id"));
        assert!(entries.iter().any(|(k, _)| k == "title"));
        assert!(entries.iter().any(|(k, _)| k == "log-dir"));

        match wait_for_settle(&parent_inbox) {
            crate::bus::Turn::Agent(result) => match &result.commitment_settle {
                Some(crate::tools::commitment::CommitmentSettle::Open { key: k, card }) => {
                    assert_eq!(k, key);
                    assert!(crate::card::summary_line(card).contains("tests pass"));
                }
                other => panic!("expected an Open settle, got {other:?}"),
            },
            other => panic!("expected an Agent result turn, got {other:?}"),
        }
    }

    /// `commit-verify` requires the key to already be a live commitment —
    /// the `commitment_card` refusal text kept verbatim, reached through
    /// the desk rather than `&Agent`.
    #[test]
    fn commit_verify_requires_a_live_key() {
        let key = "commitment:abc";
        let (desk, _registry, _parent_inbox) = spawnable_desk(3);
        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        let err = desk
            .handle(FOValue::Variant {
                label: "commit-verify".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::String { value: key.into() },
                    ],
                })),
            })
            .expect_err("a non-live key must refuse");
        assert!(
            err.message.contains("no live commitment pin named"),
            "got: {}",
            err.message
        );
        assert!(
            desk.services.nursery.adopt(session).is_some(),
            "a refusal ahead of adopt must leave the parked fork re-adoptable — no verifier was \
             ever forked"
        );
    }

    /// A valid `commit-verify` against a live key adopts the parked fork,
    /// spawns a read-only verifier, and a passing verdict settles tagged
    /// for the parent to clear — port of
    /// `tools::commitment::passing_verifier_settles_tagged_for_clearing`.
    #[test]
    fn passing_verifier_settles_tagged_for_clearing() {
        let key = "commitment:abc";
        let (desk, _registry, parent_inbox) = spawnable_desk(3);
        desk.services.pins.lock().expect("pin register poisoned").insert(
            key.to_string(),
            shell_eval::PinDigest::new(key, text_card("run tests and keep code minimal")),
        );
        let pass_json = serde_json::json!({
            "kind": "commitment_verdict",
            "commitment_key": key,
            "verdict": "pass",
            "criteria": [],
            "summary": "ok",
            "retry_prompt": "",
        })
        .to_string();
        let provider = Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new().then(Reply::tool_calls(vec![reply_call(&pass_json)])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        let answer = desk
            .handle(FOValue::Variant {
                label: "commit-verify".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session.0).expect("small test id"),
                        },
                        FOValue::String { value: key.into() },
                    ],
                })),
            })
            .expect("a valid commit-verify must succeed");
        assert!(
            matches!(&answer, FOValue::Variant { label, .. } if label == "started"),
            "verify-commitment must launch async, not settle inline"
        );

        match wait_for_settle(&parent_inbox) {
            crate::bus::Turn::Agent(result) => {
                assert!(
                    matches!(
                        &result.commitment_settle,
                        Some(crate::tools::commitment::CommitmentSettle::Clear(k)) if k == key
                    ),
                    "a passing verdict must tag the settle for the parent to clear"
                );
            }
            other => panic!("expected an Agent result turn, got {other:?}"),
        }
    }

    /// A failing verdict settles untagged, leaving the pin live — port of
    /// `tools::commitment::failing_verifier_settles_untagged`.
    #[test]
    fn failing_verifier_settles_untagged() {
        let key = "commitment:abc";
        let (desk, _registry, parent_inbox) = spawnable_desk(3);
        desk.services.pins.lock().expect("pin register poisoned").insert(
            key.to_string(),
            shell_eval::PinDigest::new(key, text_card("run tests and keep code minimal")),
        );
        let fail_json = serde_json::json!({
            "kind": "commitment_verdict",
            "commitment_key": key,
            "verdict": "fail",
            "criteria": [],
            "summary": "missing evidence",
            "retry_prompt": "run the tests",
        })
        .to_string();
        let provider = Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new().then(Reply::tool_calls(vec![reply_call(&fail_json)])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        desk.handle(FOValue::Variant {
            label: "commit-verify".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::Int {
                        value: i64::try_from(session.0).expect("small test id"),
                    },
                    FOValue::String { value: key.into() },
                ],
            })),
        })
        .expect("a valid commit-verify must succeed");

        match wait_for_settle(&parent_inbox) {
            crate::bus::Turn::Agent(result) => {
                assert!(
                    result.commitment_settle.is_none(),
                    "a failing verdict must not tag the settle for clearing"
                );
            }
            other => panic!("expected an Agent result turn, got {other:?}"),
        }
    }

    // ── `reply` ───────────────────────────────────────────────────────────

    /// `` `reply `` is refused outright when this agent does not hold
    /// `returns` — the desk equivalent of `Gate::Returns` — before the
    /// payload is even decoded, and the cell is left empty.
    #[test]
    fn reply_refused_without_returns() {
        let (emit, _rx) = dummy_emitter();
        let mut d = desk();
        d.services.emit = emit;
        d.services.returns = false;
        let err = d
            .handle(FOValue::Variant {
                label: "reply".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![FOValue::Int { value: 1 }],
                })),
            })
            .expect_err("a non-returning agent's reply must be refused");
        assert!(
            err.message.contains("you converse with the user; you do not return"),
            "got: {}",
            err.message
        );
        assert!(
            d.services.reply.take().is_none(),
            "a refused reply must never reach the cell"
        );
    }

    /// A valid `reply` stages the payload into the cell — last write wins —
    /// and emits the "Replied to parent" chrome.
    #[test]
    fn reply_stages_the_payload_last_write_wins() {
        let (emit, rx) = dummy_emitter();
        let mut d = desk();
        d.services.emit = emit;

        for text in ["first", "second"] {
            d.handle(FOValue::Variant {
                label: "reply".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![FOValue::String { value: text.into() }],
                })),
            })
            .expect("a returning agent's reply must succeed");
        }

        assert_eq!(
            d.services.reply.take(),
            Some(FOValue::String { value: "second".into() }),
            "the last staged reply wins"
        );

        let mut saw_chrome = false;
        while let Ok(event) = rx.try_recv() {
            if let Kind::ToolCall { tool: "reply", summary, .. } = event.kind {
                assert_eq!(summary.as_deref(), Some("Replied to parent."));
                saw_chrome = true;
            }
        }
        assert!(saw_chrome, "each reply must emit the parent-facing chrome line");
    }
}
