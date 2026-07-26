//! The exarch enquiry desk: the host half of `shell.enquire(class)`.
//!
//! [`HostServices`] is the capture set a desk handler is allowed to touch —
//! everything [`crate::agent::Agent::host_services`] can hand off `&Agent`
//! without lending `&mut Agent`/`&mut Shell`, since the reentrancy law bars a
//! handler from ever taking the session lock. [`ExarchDesk`] answers one enquiry
//! against that capture; [`DeskBinding`] is the identity-transport adapter
//! that keeps a handler's own chrome from outrunning the run's earlier
//! surface output. Step 0 wired both desk bindings answering every enquiry
//! class alike, with the extension law's unrecognised-class error; this
//! module now also carries
//! Step 1, the agent family (`` `agent-start ``, `` `agent-list ``,
//! `` `agent-cancel ``, `` `message ``), Step 2, the schedule family
//! (`` `schedule ``, `` `schedule-list ``, `` `unschedule ``), Step 3,
//! `` `reply ``, and the egress family, `` `fetch-url ``.

use crate::agent::{Agent, Build, LogCell, ProviderHandle, ReplyCell};
use crate::bus::{AgentId, Emitter, Kind, Mailbox};
use crate::fleet::egress;
use crate::fleet::registry::{AgentRegistry, MessageError, NotADescendant};
use crate::fleet::schedule::{CronSchedule, ScheduleRegistry, Trigger, parse_duration};
use crate::shell_eval::{self, PinDigests};
use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use ral_core::transport::{Event, EventReceiver};
use ral_core::types::{Capabilities, EnquiryDesk, Error, Nursery, NurseryId};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Everything a desk handler may read off `&Agent`, snapshotted fresh at
/// every [`crate::agent::Agent::run_shell`] install
/// ([`crate::agent::Agent::host_services`]) so a handler never reaches back
/// through `&mut Agent`/`&mut Shell` to get it. Mirrors
/// [`crate::agent::Build`] minus the seat, plus the handles the harness
/// verbs need that [`Build`] has no reason to carry (the registry, the
/// parent's mailbox, the nursery, …).
///
/// `agent-start`/`agent-list`/`agent-cancel`/`message` (Step 1),
/// `schedule`/`schedule-list`/`unschedule` (Step 2), and `reply` (Step 3,
/// reading `returns` and `reply`) read this capture between them.
pub(crate) struct HostServices {
    /// The fleet's shared agent registry — `agents.clone()`.
    pub registry: AgentRegistry,
    /// The session scratch a spawned child's identity seat shares with its
    /// parent — `None` on a wire seat, which owns no host-side scratch at
    /// all ([`Agent::host_services`]).
    pub scratch: Option<Arc<crate::bootstrap::Scratch>>,
    /// This run's agent: the child's upward edge for `agent-start`'s
    /// receipt and the descendant check `message`/`agent-cancel` enforce.
    pub parent: AgentId,
    /// The parent's own inbox — where a spawned child's eventual result and
    /// a `message` delivery land.
    pub mailbox: Mailbox,
    /// Rail chrome plus child-tab derivation for a spawn's [`Kind::HarnessCall`]
    /// line and the acting verbs' [`Kind::HarnessCall`]/[`Kind::HarnessResult`] pair.
    pub emit: Emitter,
    /// The parent's hot-swappable provider handle, seeded into a spawned
    /// child's own handle.
    pub provider: ProviderHandle,
    /// The parent ceiling [`crate::policy::narrow`] attenuates against for
    /// `agent-start`'s `permissions` argument.
    pub caps: Capabilities,
    /// The working directory frozen at install, for [`crate::policy::narrow`] and
    /// any handler that needs a path root.
    pub cwd: PathBuf,
    /// The depth-budget snapshot for this run, immutable for its extent:
    /// `agent-start` refuses once it reads zero.
    pub fuel: u32,
    /// Whether this agent holds `reply` — the desk's `reply` refusal reads
    /// this bit, never trunk-ness.
    pub returns: bool,
    /// Whether this agent holds the self-wakeup grant — the schedule
    /// family's handlers refuse without it.
    pub allow_schedule: bool,
    /// The shared slot the `reply` handler stages a returning agent's
    /// return value into — a fresh cell [`Agent::run_shell`] mints for this
    /// one call and harvests back into [`Agent::reply`] once the desk
    /// retires; the handler never holds `&mut Agent` to write it any other
    /// way.
    pub reply: ReplyCell,
    /// This agent's live scheduled wakeups.
    pub schedules: ScheduleRegistry,
    /// This session's canonical event log — `mnemon`'s context-inheriting
    /// fork reads it.
    pub log: LogCell,
    /// The system prompt *template* a spawned child's own [`Build::system`]
    /// is fed from — still carrying [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`]
    /// rather than a baked-in builtin index, since the index is filtered
    /// per agent and this template is agent-invariant. [`Agent::assemble`]
    /// resolves it into the child's own [`Agent::system`] from the child's
    /// own `returns`/`allow_schedule`.
    pub system_template: String,
    /// The fleet-shared builtin-index table the spawned child's own
    /// `system` resolves from — no live `Shell` needed at the desk.
    pub indexes: std::sync::Arc<crate::prompt::BuiltinIndexes>,
    /// Whether a human is attached to the fleet, inherited by every spawn.
    pub interactive: bool,
    /// The adoption end of this run's body-side session forks
    /// ([`ral_core::Shell::fork_into_nursery`]) — `agent-start` redeems a
    /// [`ral_core::types::NurseryId`] here.
    pub nursery: Nursery,
    /// The agent registry's generation at install, so a desk installed
    /// before `/clear` can refuse an `agent-start` raised after it.
    pub generation: u64,
    /// The operator's disk-warn ceiling, threaded verbatim into a spawned
    /// child's own [`Build`] exactly as `Agent::fork_with` does — a host
    /// setting, not a per-agent choice.
    pub disk_warn_bytes: Option<u64>,
    /// The IT-set fetch-url policy, audit ledger, and rate budget —
    /// threaded verbatim into a spawned child exactly as `disk_warn_bytes`
    /// is: a spawn shares its parent's egress policy, never a fresh one.
    pub egress: egress::Egress,
}

/// The exarch enquiry desk: answers one [`FOValue`] enquiry against a
/// captured [`HostServices`]. Installed fresh per `ral` call
/// ([`crate::agent::Agent::run_shell`]), wrapped in [`DeskBinding`] for the
/// identity transport and passed bare into [`crate::shell_eval::run_shell`]'s
/// `on_enquiry` binding for the wire-future path — one handler, two
/// bindings.
pub(crate) struct ExarchDesk {
    pub(crate) services: HostServices,
}

/// Convert a duration's whole seconds to a saturating `i64` — the
/// convention any duration this desk answers (a schedule's `next-s` in both
/// the `schedule` receipt and the `schedules` listing, and an agent's
/// elapsed seconds in the `agents` listing) shares: none of them approaches
/// `i64::MAX` in practice, but `unwrap_or` keeps the conversion total
/// without an `as` cast's silent wraparound.
fn secs_to_i64(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// Decode one enquiry payload as a fixed-length list, or a didactic error
/// naming the expected shape — the host-side half of the both-ends
/// validation law: a builtin's door already checked its own arguments, but
/// the desk trusts nothing about what actually crossed the wire. `N` is
/// inferred from the caller's array pattern.
fn payload_list<const N: usize>(
    payload: Option<Box<FOValue>>,
    class: &str,
    shape: &str,
) -> Result<[FOValue; N], Error> {
    let Some(payload) = payload else {
        return Err(Error::new(
            format!("`{class}` requires a payload {shape}"),
            1,
        ));
    };
    let FOValue::List { items } = *payload else {
        return Err(Error::new(
            format!("`{class}` payload must be a list {shape}"),
            1,
        ));
    };
    <[FOValue; N]>::try_from(items).map_err(|items| {
        Error::new(
            format!(
                "`{class}` payload must have exactly {N} element(s) {shape}, got {}",
                items.len()
            ),
            1,
        )
    })
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
        FOValue::Variant {
            label,
            payload: None,
        } => Ok(label),
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
        FOValue::Variant {
            label,
            payload: Some(payload),
        } if label == "cron" => {
            let expr = payload_string(*payload, class, "trigger")?;
            let schedule =
                CronSchedule::parse(&expr).map_err(|e| Error::new(format!("`{class}`: {e}"), 1))?;
            Ok(Trigger::Cron { schedule, expr })
        }
        FOValue::Variant {
            label,
            payload: Some(payload),
        } if label == "after" => {
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
        FOValue::Variant {
            label,
            payload: Some(payload),
        } if label == "some" => Ok(Some(payload_string(*payload, class, "label")?)),
        FOValue::Variant {
            label,
            payload: None,
        } if label == "none" => Ok(None),
        other => Err(Error::new(
            format!("`{class}`: `label` must be `some <str>` or `none`, got {other:?}"),
            1,
        )),
    }
}

/// The parameters that vary across `agent-start`'s two spawn kinds
/// (`amnemon`/`mnemon`, the `agent` builtin's `type` argument). Every other
/// step of the spawn spine (the generation guard, the fuel guard, the
/// uniqueness pre-check, [`Nursery::adopt`], [`crate::policy::narrow`], the parent-log
/// fork, [`Agent::assemble`]'s `Build` literal,
/// [`spawn_async`](crate::shell_eval::tools::agent::spawn_async)'s
/// register/detach/settle mechanics, and the `` `started `` receipt) is
/// identical for both and lives once in [`ExarchDesk::launch`].
struct Launch<'a> {
    /// The nursery id the builtin body parked its fork under.
    session_id: u64,
    /// The caller-chosen grant to narrow the child's authority against.
    grant: &'a str,
    /// Whether the child imports the parent's model-visible conversation —
    /// true only for a `mnemon` spawn.
    inherit_context: bool,
    /// The child's requested name — refused if any live agent already bears
    /// it (the uniqueness pre-check inside [`ExarchDesk::launch`]).
    name: String,
    prompt: String,
}

impl ExarchDesk {
    /// Decode one enquiry class and answer it.
    ///
    /// The agent family (`` `agent-start ``, `` `agent-list ``,
    /// `` `agent-cancel ``, `` `message ``), the schedule family
    /// (`` `schedule ``, `` `schedule-list ``, `` `unschedule ``),
    /// `` `reply ``, and the egress family (`` `fetch-url ``) are answered
    /// here; an unrecognised class still answers the extension law's error —
    /// never a silent default.
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
            "reply" => self.reply(payload),
            "fetch-url" => self.fetch_url(payload),
            other => Err(Error::new(
                format!("unrecognised enquiry class `{other}`"),
                1,
            )),
        }
    }

    /// The spawn spine behind `agent-start`: adopt the nursery-parked fork
    /// the builtin body forked, narrow its authority against the parent's
    /// own ceiling, fork its own session log off the parent's, assemble it
    /// into a full child at one less unit of fuel, and hand it to
    /// [`spawn_async`](crate::shell_eval::tools::agent::spawn_async). The generation and
    /// fuel guards run before [`Nursery::adopt`]; every refusal downstream
    /// of that point simply drops the adopted `Shell` like any other owned
    /// value, never a leak.
    fn launch(&self, spec: Launch) -> Result<FOValue, Error> {
        let s = &self.services;

        // 1. The /clear guard: this call's captured fuel, caps, and grant
        // are only as fresh as the generation they were snapshotted under.
        if s.generation != s.registry.generation() {
            return Err(Error::new(
                "agent-start refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale — \
                 issue agent again on your next turn",
                1,
            ));
        }

        // 2. Fuel bounds depth, not fan-out (agent.rs's SPAWN_FUEL doc): the
        // parent's own fuel is never debited here, so unlimited siblings may
        // start at this depth; only a chain that recurses this many
        // generations deep bottoms out.
        if s.fuel == 0 {
            return Err(Error::new(
                "agent-start refused: no spawn fuel remains at this depth, so agent cannot \
                 delegate any further here. Fuel bounds how deep a chain of spawns may \
                 recurse, never how many children you may start at any one depth — starting \
                 several agents here costs nothing extra. agent-cancel on any node stops its \
                 whole live subtree regardless of depth.",
                1,
            ));
        }

        // 3. The uniqueness pre-check: a spawn is refused if any live agent
        // already bears the requested name. Cheap and didactic — it runs
        // before ever forking a nursery session — but not by itself
        // race-free: `AgentRegistry::register` (step 8) re-checks under its
        // own lock, which is what actually closes a race between two
        // same-name spawns issued within one turn.
        if s.registry.name_live(&spec.name) {
            return Err(Error::new(
                format!(
                    "agent-start refused: a live agent already bears the name '{}' — pick \
                     another, or wait for it to settle. Names identify live agents; agents \
                     lists yours.",
                    spec.name
                ),
                1,
            ));
        }

        // 4. Adopt the parked fork the builtin body forked. Once past this
        // point every remaining refusal simply drops the adopted `Shell` —
        // never a leak, since it is an ordinary owned value.
        let Some(shell) = s.nursery.adopt(NurseryId(spec.session_id)) else {
            return Err(Error::new(
                "`agent-start`: no forked session parked under this id — it may already have \
                 started, or the run that forked it has ended",
                1,
            ));
        };

        // 5. Narrow the child's authority against the parent's own ceiling.
        // `policy::narrow` names all six legal bases in its own diagnostic
        // when `grant` is not one of them.
        let cwd = s.cwd.to_string_lossy();
        let child_caps = crate::policy::narrow(&s.caps, spec.grant, &cwd)
            .map_err(|reason| Error::new(reason, 1))?;

        // 6. Fork the child's own log off the parent's; a mnemon spawn
        // additionally imports the parent's model-visible context, exactly
        // as `Agent::inherit_context` does for `fork_remembering`/`branch`
        // — done here, on the raw `AgentLog`, since the child is not yet an
        // `Agent` for `inherit_context` to take a `&Self` of. The child's
        // builtin index is resolved once, here, from the bits every spawn
        // through this spine fixes below (`returns: true, allow_schedule:
        // s.allow_schedule`): the bookend records its length — never the
        // unresolved template's raw length — and `Build` carries the same
        // string on to become the child's own `Agent::system`.
        let child_id = crate::agent::fresh_id();
        let system_prompt = s.indexes.apply(&s.system_template, true, s.allow_schedule);
        let child_log = {
            let parent_log = s.log.lock();
            let mut child_log = parent_log
                .fork(child_id, system_prompt.len())
                .map_err(|e| Error::new(format!("could not fork child session log: {e}"), 1))?;
            let inherited = spec
                .inherit_context
                .then(|| parent_log.inherited_context_messages());
            drop(parent_log);
            if let Some(messages) = inherited {
                child_log
                    .import_context(messages)
                    .map_err(|e| Error::new(e, 1))?;
            }
            child_log
        };

        // 7. Assemble the child at one less unit of fuel than this agent
        // holds — mirroring `Agent::fork_with`'s `Build` literal verbatim,
        // with the adopted shell and forked log standing in for
        // `fork_session`'s fresh ones.
        let fuel = s.fuel - 1;
        // Only an identity seat's `agent-start` ever reaches this far: a
        // wire session's fuel is always 0 (step 2 above already refused
        // it), since a guest-side spawn is not yet built.
        let Some(scratch) = s.scratch.clone() else {
            return Err(Error::new(
                "agent-start refused: this session has no host-side scratch to fork an \
                 in-process child into",
                1,
            ));
        };
        // No detach, exactly as `Agent::fork_with`: the adopted shell was
        // forked, not booted, so it carries no detach policy.
        let seat =
            crate::agent::seat::Seat::identity(shell, scratch, s.cwd.clone(), false, &child_log);
        let child = Agent::assemble(Build {
            system: s.system_template.clone(),
            system_prompt,
            indexes: s.indexes.clone(),
            caps: child_caps,
            seat,
            log: child_log,
            parent: Some(s.parent),
            fuel,
            provider: ProviderHandle::new(s.provider.current()),
            interactive: s.interactive,
            returns: true,
            allow_schedule: s.allow_schedule,
            tool_enabled: true,
            agents: s.registry.clone(),
            disk_warn_bytes: s.disk_warn_bytes,
            egress: s.egress.clone(),
        })
        .map_err(|e| Error::new(format!("could not assemble child session: {e}"), 1))?;

        // 8. Register, detach, and spawn — the fork-detach-register
        // mechanics every launch-only tool shares, verbatim. `spawn_async`
        // already emits the parity `Kind::HarnessCall` spawn line, so this
        // handler does not double it. `AgentRegistry::register` re-checks
        // name uniqueness under its own lock — the authoritative half of
        // step 3's pre-check — so a same-name sibling racing this call
        // within the same turn is still refused even though it passed the
        // cheap check above.
        let spawned = crate::shell_eval::tools::agent::spawn_async(
            &s.registry,
            s.parent,
            s.mailbox.clone(),
            child,
            crate::shell_eval::tools::agent::AsyncSpawn {
                verb: "spawn",
                name: spec.name,
                prompt: Some(spec.prompt),
                harness: true,
            },
            &s.emit,
        );
        match spawned {
            Ok(child) => Ok(FOValue::Variant {
                label: "started".to_string(),
                payload: Some(Box::new(FOValue::Map {
                    entries: vec![
                        ("name".to_string(), FOValue::String { value: child.name }),
                        (
                            "log-dir".to_string(),
                            FOValue::String {
                                value: child.log_dir,
                            },
                        ),
                    ],
                })),
            }),
            Err(reason) => Err(Error::new(reason, 1)),
        }
    }

    /// `` `agent-start `` — the desk half of the `agent` builtin: decode the
    /// door (the `type` tag, the `grant` label the launch narrows against)
    /// and hand off to [`Self::launch`] for the spawn spine.
    fn agent_start(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let [session, kind, prompt, name, grant] = payload_list(
            payload,
            "agent-start",
            "[session, kind, prompt, name, grant]",
        )?;
        let session_id = payload_int(session, "agent-start", "session")?;
        let session_id = u64::try_from(session_id)
            .map_err(|_| Error::new("`agent-start`: `session` must be a non-negative Int", 1))?;
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
        let name = payload_string(name, "agent-start", "name")?;
        let grant = payload_tag(grant, "agent-start", "grant")?;

        self.launch(Launch {
            session_id,
            grant: &grant,
            inherit_context: mnemon,
            name,
            prompt,
        })
    }

    /// `` `agent-list `` — the desk half of `agents`: a registry listing
    /// scoped to this agent's own descendants. Silent — no chrome, the
    /// answer carries the whole listing.
    fn agent_list(&self) -> FOValue {
        let s = &self.services;
        FOValue::List {
            items: s
                .registry
                .list(s.parent)
                .into_iter()
                .map(|a| {
                    let elapsed_s = secs_to_i64(a.elapsed);
                    FOValue::Map {
                        entries: vec![
                            ("name".to_string(), FOValue::String { value: a.name }),
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

    /// `` `agent-cancel `` — the desk half of `agent-cancel`: resolve a live
    /// descendant by name and cancel it, scoped exactly as
    /// [`AgentRegistry::cancel_scoped`] enforces.
    fn agent_cancel(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let [name] = payload_list(payload, "agent-cancel", "[name]")?;
        let name = payload_string(name, "agent-cancel", "name")?;

        // Resolve the name, then run the cancel and derive the act row from
        // what actually happened — a row claiming "cancelled" ahead of the
        // call would assert an effect the world never saw. `cancel` takes no
        // argument, so its payload column carries the outcome: blank when
        // the cancel landed.
        let cancelled = s
            .registry
            .resolve_name(&name)
            .map_or(Ok(false), |id| s.registry.cancel_scoped(s.parent, id));
        let (payload, content, ok) = match cancelled {
            Ok(true) => (String::new(), format!("cancelling agent '{name}'"), true),
            Ok(false) => (
                "no live agent by that name".to_string(),
                format!("no live agent named '{name}'"),
                true,
            ),
            Err(NotADescendant(_)) => (
                "refused: not a descendant".to_string(),
                format!(
                    "agent '{name}' is not an agent you started; agent-cancel may only reach a descendant of yours"
                ),
                false,
            ),
        };
        s.emit.emit(Kind::HarnessCall {
            verb: "cancel",
            subject: Some(name),
            payload,
            failed: !ok,
        });
        s.emit.emit(Kind::HarnessResult(content.clone()));
        // A no-op (no live agent by that name) and an actual cancel are both
        // a successful call; only a scope violation raises, and the raise
        // carries the long form — the model's only copy of it.
        if ok {
            Ok(FOValue::Unit)
        } else {
            Err(Error::new(content, 1))
        }
    }

    /// `` `message `` — the desk half of `message`: resolve a live
    /// descendant by name and send it a marked note.
    fn message(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let [name, text] = payload_list(payload, "message", "[name, text]")?;
        let name = payload_string(name, "message", "name")?;
        let text = payload_string(text, "message", "text")?;

        // Resolve the name, then run the send and derive the act row from
        // what actually happened — a row showing the text as sent ahead of
        // the call would assert a delivery that a scope refusal or a full
        // inbox never made. Unlike `cancel`, an unresolved name is a refusal
        // here, not a no-op: `message` promises delivery, and there is
        // nothing to deliver to. A delivered message's payload is its text;
        // a refused one's is why, since the text went nowhere. The target
        // settling in the gap between resolving its name and this send is
        // the same "no live agent" outcome as an unresolved name, just
        // discovered one step later.
        let sent = text.clone();
        let (payload, content, ok) = match s
            .registry
            .resolve_name(&name)
            .map(|to| s.registry.message(s.parent, to, text))
        {
            None | Some(Err(MessageError::UnknownRecipient(_))) => (
                "refused: no live agent by that name".to_string(),
                format!("no live agent named '{name}'; did it finish already?"),
                false,
            ),
            Some(Ok(())) => (sent, format!("sent message to agent '{name}'"), true),
            Some(Err(MessageError::UnknownSender(n))) => (
                "refused: sender is no longer live".to_string(),
                format!("cannot send from agent {n}: it is no longer live"),
                false,
            ),
            Some(Err(MessageError::NotADescendant(_))) => (
                "refused: not a descendant".to_string(),
                format!(
                    "agent '{name}' is not an agent you started; message may only reach a descendant of yours"
                ),
                false,
            ),
            Some(Err(MessageError::RecipientInboxFull(_, reject))) => (
                format!("refused: {reject}"),
                format!("agent '{name}' did not receive the message: {reject}"),
                false,
            ),
        };
        s.emit.emit(Kind::HarnessCall {
            verb: "message",
            subject: Some(name),
            payload,
            failed: !ok,
        });
        s.emit.emit(Kind::HarnessResult(content.clone()));
        // Success of the command is the confirmation; a refusal raises,
        // carrying the long form — the model's only copy of it — the desk
        // twin of the pure confirmations' `F Unit` contract.
        if ok {
            Ok(FOValue::Unit)
        } else {
            Err(Error::new(content, 1))
        }
    }

    /// The self-wakeup grant guard `schedule`, `schedule-list`, and
    /// `unschedule` each refuse without, differing only in the leading verb
    /// name. `verb` names the caller in the refusal text.
    fn require_schedule_grant(&self, verb: &str) -> Result<(), Error> {
        if self.services.allow_schedule {
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

    /// `` `schedule `` — the desk half of `schedule`: arm a self-wakeup by
    /// wrapping [`ScheduleRegistry::schedule`]. `trigger` and
    /// `label` are re-decoded and re-parsed here with the same parsers the
    /// builtin's own door already ran — the both-ends validation law: a
    /// builtin's door check is never this desk's only line of defence.
    /// Answers a receipt `[label: Str, next-s: Int]`: the resolved label
    /// (the caller's own, or the minted `sched-{id}` default) and the
    /// seconds to first fire.
    fn schedule(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        self.require_schedule_grant("schedule")?;
        let s = &self.services;

        let [trigger, label, prompt] =
            payload_list(payload, "schedule", "[trigger, label, prompt]")?;
        let trigger = payload_trigger(trigger, "schedule")?;
        let label = payload_label(label, "schedule")?;
        let prompt = payload_string(prompt, "schedule", "prompt")?;

        // The act row goes up after the registry call, so a schedule that the
        // registry refuses tiers its outcome like every other act rather than
        // reading as one that landed.  The subject stays the caller's own
        // label: the minted `sched-{id}` default is the registry's answer, not
        // something the caller named, so an unlabelled schedule leaves the cell
        // blank.
        let described = trigger.describe();
        let result = s
            .schedules
            .schedule(trigger, prompt, label.clone(), &s.mailbox);
        let content = match &result {
            Ok(receipt) => format!(
                "scheduled '{}' ({}s to first fire)",
                receipt.label,
                secs_to_i64(receipt.next_in)
            ),
            Err(e) => format!("could not schedule: {e}"),
        };
        s.emit.emit(Kind::HarnessCall {
            verb: "schedule",
            subject: label,
            payload: match &result {
                Ok(_) => described,
                Err(e) => format!("refused: {e}"),
            },
            failed: result.is_err(),
        });
        s.emit.emit(Kind::HarnessResult(content.clone()));
        match result {
            Ok(receipt) => Ok(FOValue::Map {
                entries: vec![
                    (
                        "label".to_string(),
                        FOValue::String {
                            value: receipt.label,
                        },
                    ),
                    (
                        "next-s".to_string(),
                        FOValue::Int {
                            value: secs_to_i64(receipt.next_in),
                        },
                    ),
                ],
            }),
            Err(_) => Err(Error::new(content, 1)),
        }
    }

    /// `` `schedule-list `` — the desk half of `schedules`: a snapshot of
    /// this agent's own live scheduled wakeups. Silent — no chrome, the
    /// answer carries the whole listing.
    fn schedule_list(&self) -> Result<FOValue, Error> {
        self.require_schedule_grant("schedule-list")?;
        let s = &self.services;
        Ok(FOValue::List {
            items: s
                .schedules
                .list()
                .into_iter()
                .map(|info| {
                    // A cron with no further occurrence within the search
                    // horizon (vanishingly rare in practice — see
                    // `Trigger::next_delay`'s doc) reports "never" as the
                    // same saturated ceiling `secs_to_i64` uses.
                    let next_s = info.next_in.map_or(i64::MAX, secs_to_i64);
                    let fires = i64::try_from(info.fires).unwrap_or(i64::MAX);
                    FOValue::Map {
                        entries: vec![
                            ("label".to_string(), FOValue::String { value: info.label }),
                            (
                                "trigger".to_string(),
                                FOValue::String {
                                    value: info.trigger,
                                },
                            ),
                            ("next-s".to_string(), FOValue::Int { value: next_s }),
                            ("fires".to_string(), FOValue::Int { value: fires }),
                        ],
                    }
                })
                .collect(),
        })
    }

    /// `` `unschedule `` — the desk half of `unschedule`: remove one
    /// scheduled wakeup by label.
    fn unschedule(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        self.require_schedule_grant("unschedule")?;
        let s = &self.services;
        let [label] = payload_list(payload, "unschedule", "[label]")?;
        let label = payload_string(label, "unschedule", "label")?;

        // A no-op (no schedule bearing that label) is a successful call,
        // like `cancel`'s: only the grant refusal above raises for this verb.
        // `unschedule` takes no argument beyond its label, so — again like
        // `cancel` — its payload column carries the outcome and the row is
        // emitted once that outcome is known: blank when the wakeup was
        // removed.
        let removed = s.schedules.unschedule(&label);
        let (payload, content) = if removed {
            (String::new(), format!("unscheduled '{label}'"))
        } else {
            (
                "no live schedule by that label".to_string(),
                format!("no live schedule labelled '{label}'"),
            )
        };
        s.emit.emit(Kind::HarnessCall {
            verb: "unschedule",
            subject: Some(label),
            payload,
            failed: false,
        });
        s.emit.emit(Kind::HarnessResult(content));
        Ok(FOValue::Unit)
    }

    /// `` `reply `` — the desk half of the `reply` builtin: stage the
    /// payload into the reply cell for [`Agent::deliberate`]'s drain to lift into
    /// a [`crate::agent::deliberate::Outcome::Replied`] terminal once the batch drains, and put the `reply` act
    /// on the rail. Refused on every non-returning agent — the conversing
    /// trunk and each `/branch` child alike — keyed on the captured
    /// `returns` bit, never on trunk-ness.
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
        let [value] = payload_list(payload, "reply", "[value]")?;
        let display =
            shell_eval::ral_value_to_text(&RalValue::from(value.clone())).unwrap_or_default();
        // `reply` addresses no one it must name — the parent is the only
        // recipient a returning agent has — so it carries no subject and its
        // value is the whole payload.
        s.emit.emit(Kind::HarnessCall {
            verb: "reply",
            subject: None,
            payload: if display.is_empty() {
                "(empty reply)".into()
            } else {
                display
            },
            failed: false,
        });
        s.reply.set(value);
        Ok(FOValue::Unit)
    }

    /// Validate the URL, check it against the IT allowlist, take a rate-
    /// budget slot, then perform the capped GET — in that order, so a
    /// refusal at any earlier step never reaches the network. Returns the
    /// resolved host alongside the outcome (blank if the URL never parsed)
    /// so the caller can audit and narrate a refusal even when there is no
    /// fetch to describe.
    fn resolve_and_fetch(&self, raw_url: &str) -> (String, Result<Vec<u8>, String>) {
        let s = &self.services;
        let parsed = match egress::validate_fetch_url(raw_url) {
            Ok(url) => url,
            Err(reason) => return (String::new(), Err(reason)),
        };
        let host = parsed.host_str().unwrap_or_default().to_string();
        if !crate::org_policy::domain_allowed(&host, &s.egress.policy.allow) {
            let reason = format!(
                "'{host}' is not on the list of sites your IT department has approved for this \
                 assistant to reach — ask your IT department to add it if you need this site"
            );
            return (host, Err(reason));
        }
        if !s.egress.limiter.try_take() {
            let reason = "too many fetches in the last minute — wait a little and try again, \
                 or ask your IT department to raise the limit"
                .to_string();
            return (host, Err(reason));
        }
        let client = egress::fetch_client();
        let outcome = egress::fetch(&client, &parsed, s.egress.policy.max_response_bytes);
        (host, outcome)
    }

    /// `` `fetch-url `` — the desk half of the `fetch-url` builtin: resolve,
    /// allowlist-check, rate-check, and fetch (see [`Self::resolve_and_fetch`]),
    /// recording exactly one audit line either way before answering — the
    /// same act-row-after-the-fact discipline [`Self::agent_cancel`]/[`Self::message`] follow.
    fn fetch_url(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let [url] = payload_list(payload, "fetch-url", "[url]")?;
        let url = payload_string(url, "fetch-url", "url")?;

        let (host, outcome) = self.resolve_and_fetch(&url);
        let (ok, act_payload, content, body) = match outcome {
            Ok(bytes) => (
                true,
                format!("{} bytes", bytes.len()),
                format!("fetched {} bytes from {host}", bytes.len()),
                Some(bytes),
            ),
            Err(reason) => (false, format!("refused: {reason}"), reason, None),
        };
        s.egress.audit.record(
            &url,
            &host,
            ok,
            body.as_ref()
                .map(|b| u64::try_from(b.len()).unwrap_or(u64::MAX)),
            (!ok).then_some(content.as_str()),
        );
        s.emit.emit(Kind::HarnessCall {
            verb: "fetch-url",
            subject: Some(url),
            payload: act_payload,
            failed: !ok,
        });
        s.emit.emit(Kind::HarnessResult(content.clone()));
        match body {
            Some(value) => Ok(FOValue::Bytes { value }),
            None => Err(Error::new(content, 1)),
        }
    }
}

/// [`SurfaceApplier::live`]/[`SurfaceApplier::deferred`] apply one surfaced
/// value or deferred batch, decoding it and folding a `` `pin ``/`` `unpin ``
/// disposition into the pin mirror exactly as [`crate::shell_eval::run_shell`]'s own
/// drain loop does — because it *is* that loop's logic, extracted so
/// [`DeskBinding`] can share it rather than risk a second copy drifting
/// from the first.
pub(crate) struct SurfaceApplier {
    pub(crate) emit: Emitter,
    pub(crate) pins: Option<PinDigests>,
}

impl SurfaceApplier {
    /// Apply one live [`Event::Surface`] value.
    pub(crate) fn live(&self, val: FOValue) {
        if let Some(kind) = shell_eval::accepted_surface(&RalValue::from(val), &self.emit) {
            if let Some(pins) = &self.pins {
                // A poisoned register is fatal, never skipped: silently
                // dropping a `pin`/`unpin` here would desync the mirror
                // from the stream with no signal at all.
                let mut m = pins.lock().expect("pin register poisoned");
                match &kind {
                    Kind::Pin { key, card } => {
                        m.insert(key.clone(), shell_eval::PinDigest::new(card.clone()));
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

/// The inert desk the run guard ([`crate::agent::seat::RunGuard`])
/// installs once a call's real desk retires, so nothing keeps that call's
/// [`Emitter`] (and the rest of its [`HostServices`] capture) alive past the
/// call that owns it — `engine.desk` is unobservable between calls, since
/// nothing dispatches without a fresh real desk installed first. Answers
/// the same honest absence [`ral_core::Shell::enquire`] itself gives when no desk is
/// installed at all, in the unreachable case something reads it directly.
pub(crate) struct AbsentDesk;

impl EnquiryDesk for AbsentDesk {
    fn enquire(
        &self,
        _req: FOValue,
        _cancel: &ral_core::process::CancelScope,
    ) -> Result<FOValue, Error> {
        Err(Error::new("this host answers no enquiries", 1))
    }
}

/// The identity transport's drain-then-handle adapter.
///
/// Under [`ral_core::transport::IdentityTransport`], [`ral_core::Shell::enquire`] calls straight into whatever
/// desk is installed, synchronously, mid-`dispatch` — before
/// [`ral_core::transport::dispatch_to_report`]'s own drain loop ever runs, since that loop only
/// starts once `dispatch` returns. Any surface frames this run already
/// emitted are therefore still queued, undrained, on the transport's event
/// channel; answering the enquiry before applying them would let a
/// handler's own chrome render ahead of the run's earlier surface output.
/// [`Self::enquire`] drains every frame already queued through `apply`
/// before delegating to `desk`, so the identity path renders in the same
/// order the wire path's live drain loop would.
pub(crate) struct DeskBinding {
    pub(crate) desk: Arc<ExarchDesk>,
    pub(crate) events: Arc<EventReceiver>,
    pub(crate) apply: SurfaceApplier,
}

impl EnquiryDesk for DeskBinding {
    fn enquire(
        &self,
        req: FOValue,
        _cancel: &ral_core::process::CancelScope,
    ) -> Result<FOValue, Error> {
        while let Some((_, event)) = self.events.try_recv() {
            match event {
                Event::Surface(val) => self.apply.live(val),
                Event::DeferredSurface(batch) => self.apply.deferred(batch),
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
    use crate::agent::event::AgentLog;
    use crate::bus::{Inbox, channel};
    use crate::fleet::egress::{AuditLog, Egress, FetchLimiter};
    use crate::fleet::registry::{AGENT_LEASE_IDLE, Registration};
    use crate::org_policy::OrgPolicy;
    use crate::provider::{
        Provider, ProviderKind,
        scripted::{Reply, Script},
    };
    use ral_core::transport::{DispatchId, IdentityTransport, Program, Run, Transport};
    use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};

    /// A session log under a scratch dir unique to this call — tests run in
    /// parallel, so a shared fixed `sessions_root`/id-0 pair would race this
    /// call's `remove_dir_all` against a sibling test's `create_dir_all` for
    /// the identical path.
    fn fresh_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("exarch-desk-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        AgentLog::root(&root, 0, "test", "test", 0).expect("session log")
    }

    fn scripted_provider() -> Arc<Provider> {
        Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new(),
        ))
    }

    /// The base capture every desk in this module builds on: a fresh
    /// registry, a throwaway parent id `0`, fuel 3, `returns: true`, no
    /// self-wakeup grant. Each helper below adjusts only the fields its
    /// tests are about, so growing [`HostServices`] means touching one
    /// literal, not three.
    fn base_services() -> HostServices {
        let (emit, _rx) = crate::bus::dummy_emitter();
        HostServices {
            registry: AgentRegistry::new(),
            scratch: Some(Arc::new(
                crate::bootstrap::Scratch::for_test(
                    crate::bootstrap::EXARCH,
                    &format!("desk-{}", crate::agent::fresh_id()),
                )
                .expect("scratch dir"),
            )),
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
            log: LogCell::new(fresh_log()),
            system_template: String::new(),
            indexes: crate::prompt::BuiltinIndexes::resolve(&ral_core::Shell::new(
                ral_core::io::TerminalState::default(),
            )),
            interactive: false,
            nursery: Nursery::default(),
            generation: 0,
            disk_warn_bytes: None,
            egress: Egress::for_test(),
        }
    }

    fn desk() -> ExarchDesk {
        ExarchDesk {
            services: base_services(),
        }
    }

    /// [`desk`] with the self-wakeup grant held, so the schedule family's
    /// desk-level tests can exercise `` `schedule ``/`` `schedule-list ``/
    /// `` `unschedule `` past their grant check.
    fn granted_desk() -> ExarchDesk {
        ExarchDesk {
            services: HostServices {
                allow_schedule: true,
                ..base_services()
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
        let _ = registry.register(Registration {
            id: parent_id,
            parent: None,
            lease: None,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/parent"),
            cancel: crate::agent::cancel::Token::new(),
            reach: None,
            mailbox: parent_inbox.mailbox(),
            provider: ProviderHandle::new(scripted_provider()),
        });
        let desk = ExarchDesk {
            services: HostServices {
                registry: registry.clone(),
                parent: parent_id,
                mailbox: parent_inbox.mailbox(),
                fuel,
                generation: registry.generation(),
                ..base_services()
            },
        };
        (desk, registry, parent_inbox)
    }

    /// Poll `inbox` for the next exchange-boundary item — a spawned
    /// child's settled [`crate::bus::AgentResult`] lands here.
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

    /// A unique scratch directory per test, mirroring `tests/agent_deliberate.rs`'s
    /// own `tmp` helper.
    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("exarch-desk-full-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// A `ral` tool call carrying a real script as its `cmd` — the shape a
    /// scripted child's own turn issues. A *returning* spawned child must
    /// call the `reply` builtin to settle cleanly in one round; a plain text
    /// completion instead nudges the child to try again, consuming a second
    /// scripted stage this module's scripts never provision.
    fn ral_call(id: &str, cmd: &str) -> genai::chat::ToolCall {
        genai::chat::ToolCall {
            call_id: id.into(),
            fn_name: "ral".into(),
            fn_arguments: serde_json::json!({ "cmd": cmd, "description": "test command" }),
            thought_signatures: None,
        }
    }

    /// A root shell to fork children off of — booted exactly once per test,
    /// never re-booted per spawn. [`crate::bootstrap::boot_shell`] resets process-global signal
    /// state ([`ral_core::process::clear`]/[`ral_core::process::install_handlers`],
    /// [`crate::agent::cancel::install`]), the same ceremony [`Agent::root`] runs exactly
    /// once for the whole fleet; every fork in production reaches its shell
    /// through [`ral_core::Shell::fork_session`] on that one already-booted root, never
    /// by re-booting. A test that instead calls [`crate::bootstrap::boot_shell`] once per spawn
    /// races that global reset against whichever sibling child's run is
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

    /// [`DeskBinding`] drains every surface frame already queued on the
    /// transport's event channel before it hands the request to the inner
    /// desk — the drain-then-handle adapter's law: a handler's own chrome
    /// must never jump ahead of the run's earlier surface output.
    ///
    /// Dispatches a real run through a real [`ral_core::transport::IdentityTransport`] that
    /// surfaces an `` `unpin `` event via the ground `surface` builtin —
    /// the actual path a surfaced value takes onto the transport's event
    /// queue, left undrained until something calls `events().try_recv()`.
    /// [`DeskBinding::enquire`] is then called directly (bare, off the
    /// dispatching thread — this test exercises the adapter's own drain
    /// logic, not [`ral_core::Shell::enquire`]'s routing, which core's own suite
    /// covers) and both effects of one call are checked: the queued
    /// surface reached the bus, and the inner desk answered.
    #[test]
    fn desk_binding_drains_pending_surfaces_before_handling() {
        let shell = ral_core::Shell::new(ral_core::io::TerminalState::default());
        let transport = IdentityTransport::new(shell);
        transport.dispatch(
            DispatchId(1),
            Run {
                program: Program::Source(r#"surface `unpin [key: "test-marker"]"#.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: RunStdin::Empty,
            },
        );

        let (emit, rx) = crate::bus::dummy_emitter();
        let binding = DeskBinding {
            desk: Arc::new(desk()),
            events: transport.events_shared(),
            apply: SurfaceApplier { emit, pins: None },
        };

        let answer = binding
            .enquire(
                FOValue::Variant {
                    label: "probe".into(),
                    payload: None,
                },
                &ral_core::process::CancelScope::default(),
            )
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
        assert!(
            saw_unpin,
            "the drained surface frame must render onto the bus"
        );

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
    /// parent's inbox once it settles.
    #[test]
    fn agent_start_spawns_and_delivers_result_to_parent_inbox() {
        let (desk, _registry, parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "reply 'hi from child'",
            )])),
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

        let FOValue::Variant {
            label,
            payload: Some(payload),
        } = answer
        else {
            panic!("expected a `started` variant");
        };
        assert_eq!(label, "started");
        let FOValue::Map { entries } = *payload else {
            panic!("expected a record payload");
        };
        assert!(
            entries
                .iter()
                .any(|(k, v)| k == "name"
                    && matches!(v, FOValue::String { value } if value == "helper")),
            "the receipt must carry the child's name"
        );
        assert!(
            entries.iter().any(|(k, _)| k == "log-dir"),
            "the receipt must carry the child's log directory"
        );
        assert_eq!(
            entries.len(),
            2,
            "the receipt is exactly [name, log-dir] — no id"
        );

        match wait_for_settle(&parent_inbox) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    result.text.contains("hi from child"),
                    "the child's reply must reach the parent's inbox, got: {}",
                    result.text
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
    }

    /// Read the recorded `system_prompt_bytes` off a session's
    /// [`crate::agent::event::SessionEvent::SessionStarted`] bookend — the first event in its `events.json`.
    fn recorded_system_prompt_bytes(log_dir: &std::path::Path) -> usize {
        let body = std::fs::read_to_string(log_dir.join("events.json")).expect("events.json");
        let first: crate::agent::event::SessionEvent = serde_json::Deserializer::from_str(&body)
            .into_iter()
            .next()
            .expect("events.json must have at least one event")
            .expect("first event must parse");
        match first {
            crate::agent::event::SessionEvent::SessionStarted {
                system_prompt_bytes,
                ..
            } => system_prompt_bytes,
            other => panic!("first event must be SessionStarted, got {other:?}"),
        }
    }

    /// A desk-spawned child's [`crate::agent::event::SessionEvent::SessionStarted`] bookend must record its own
    /// resolved system prompt length — [`ExarchDesk::launch`] fixes every
    /// spawn's bits at `returns: true, allow_schedule: services.allow_schedule`,
    /// never the raw `HostServices.system_template` [`ExarchDesk::launch`] forks
    /// the child's log from.
    #[test]
    fn agent_start_bookend_records_the_childs_resolved_length() {
        let (mut desk, _registry, parent_inbox) = spawnable_desk(3);
        let template = format!(
            "persona\n\n# Builtins\n\n{}",
            crate::prompt::BUILTIN_INDEX_PLACEHOLDER
        );
        desk.services.system_template = template.clone();
        let provider = Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "reply 'hi from child'",
            )])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        // The production seam: the fleet's index table is resolved from the
        // same booted surface the parked child shells fork from.
        desk.services.indexes = crate::prompt::BuiltinIndexes::resolve(&root);
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

        let FOValue::Variant {
            payload: Some(payload),
            ..
        } = answer
        else {
            panic!("expected a `started` variant");
        };
        let FOValue::Map { entries } = *payload else {
            panic!("expected a record payload");
        };
        let log_dir = entries
            .iter()
            .find_map(|(k, v)| match (k.as_str(), v) {
                ("log-dir", FOValue::String { value }) => Some(value.clone()),
                _ => None,
            })
            .expect("the receipt must carry the child's log directory");

        let expected = desk
            .services
            .indexes
            .apply(&template, true, desk.services.allow_schedule)
            .len();
        assert_eq!(
            recorded_system_prompt_bytes(std::path::Path::new(&log_dir)),
            expected,
            "the bookend must record the spawned child's own resolved \
             system, not the unresolved template HostServices captured"
        );

        // Drain the child's settle so this test does not leave a background
        // thread racing the process past this test's own teardown.
        let _ = wait_for_settle(&parent_inbox);
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
             stay for the run guard to reap, never claimed by a refused call"
        );
    }

    /// A second `agent-start` naming an already-live agent is refused, and
    /// registers no child — the desk's own pre-check
    /// ([`crate::fleet::registry::AgentRegistry::name_live`]) catching the
    /// ordinary case before ever adopting the parked fork.
    #[test]
    fn agent_start_refuses_a_name_already_borne_by_a_live_agent() {
        let (desk, registry, _parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new().then(Reply::tool_calls(vec![ral_call("r1", "reply 'a'")])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell1 = forkable_child_shell(&root);
        let session1 = desk.services.nursery.park(shell1);
        let answer = desk.handle(FOValue::Variant {
            label: "agent-start".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::Int {
                        value: i64::try_from(session1.0).expect("small test id"),
                    },
                    FOValue::Variant {
                        label: "amnemon".into(),
                        payload: None,
                    },
                    FOValue::String { value: "go".into() },
                    FOValue::String {
                        value: "helper".into(),
                    },
                    FOValue::Variant {
                        label: "confined".into(),
                        payload: None,
                    },
                ],
            })),
        });
        assert!(answer.is_ok(), "the first spawn must succeed");

        let shell2 = forkable_child_shell(&root);
        let session2 = desk.services.nursery.park(shell2);
        let err = desk
            .handle(FOValue::Variant {
                label: "agent-start".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::Int {
                            value: i64::try_from(session2.0).expect("small test id"),
                        },
                        FOValue::Variant {
                            label: "amnemon".into(),
                            payload: None,
                        },
                        FOValue::String {
                            value: "go again".into(),
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
            .expect_err("a second spawn naming a live agent must be refused");
        assert!(
            err.message.contains("already bears the name 'helper'"),
            "got: {}",
            err.message
        );
        assert!(
            desk.services.nursery.adopt(session2).is_some(),
            "a name-collision refusal happens before adopt, so the parked \
             fork must stay for the run guard to reap, never claimed by a \
             refused call"
        );
        assert_eq!(
            registry.list(desk.services.parent).len(),
            1,
            "the second, refused spawn must register no child"
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
                .then(Reply::tool_calls(vec![ral_call("r1", "reply 'a'")]))
                .then(Reply::tool_calls(vec![ral_call("r2", "reply 'b'")]))
                .then(Reply::tool_calls(vec![ral_call("r3", "reply 'c'")])),
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
                crate::bus::Item::Agent(result) => texts.push(result.text),
                other => panic!("expected an Agent result item, got {other:?}"),
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

    /// A refusal that happens after [`Nursery::adopt`] (a bad permissions
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
    /// [`AgentRegistry`]'s own scoping tests at the desk entry point.
    #[test]
    fn message_and_cancel_scope_to_descendants() {
        let (desk_root, registry, _root_inbox) = spawnable_desk(3);
        // Every id here is minted, never a literal — see `spawnable_desk`'s
        // own doc on why a hardcoded id can collide with the process-wide
        // `fresh_id` counter and corrupt the tree. The trunk's own name is
        // `spawnable_desk`'s "parent".
        let root_id = desk_root.services.parent;
        let mid = crate::agent::fresh_id();
        let sibling = crate::agent::fresh_id();
        let grandchild = crate::agent::fresh_id();
        // root -> mid -> grandchild, and root -> sibling (mid's sibling).
        for (id, parent, name) in [
            (mid, root_id, "mid"),
            (sibling, root_id, "sibling"),
            (grandchild, mid, "grandchild"),
        ] {
            let _ = registry.register(Registration {
                id,
                parent: Some(parent),
                lease: Some(AGENT_LEASE_IDLE),
                name: name.to_string(),
                log_dir: PathBuf::from(format!("/tmp/{id}")),
                cancel: crate::agent::cancel::Token::new(),
                reach: None,
                mailbox: Inbox::new().mailbox(),
                provider: ProviderHandle::new(scripted_provider()),
            });
        }

        let mut desk1 = desk_root;
        desk1.services.parent = mid;

        let err = desk1
            .handle(FOValue::Variant {
                label: "message".into(),
                payload: Some(Box::new(FOValue::List {
                    items: vec![
                        FOValue::String {
                            value: "sibling".into(),
                        },
                        FOValue::String { value: "hi".into() },
                    ],
                })),
            })
            .expect_err("a message to a sibling must be refused");
        assert_eq!(
            err.message,
            "agent 'sibling' is not an agent you started; message may only reach a descendant of yours"
        );

        assert!(
            desk1
                .handle(FOValue::Variant {
                    label: "message".into(),
                    payload: Some(Box::new(FOValue::List {
                        items: vec![
                            FOValue::String {
                                value: "grandchild".into()
                            },
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
                    items: vec![FOValue::String {
                        value: "parent".into(),
                    }],
                })),
            })
            .expect_err("cancelling an ancestor must be refused");
        assert_eq!(
            cancel_err.message,
            "agent 'parent' is not an agent you started; agent-cancel may only reach a descendant of yours"
        );

        assert!(
            desk1
                .handle(FOValue::Variant {
                    label: "agent-cancel".into(),
                    payload: Some(Box::new(FOValue::List {
                        items: vec![FOValue::String {
                            value: "grandchild".into()
                        }],
                    })),
                })
                .is_ok(),
            "cancelling a proper descendant must succeed"
        );
    }

    /// A desk handler observes the run's earlier surfaced values already
    /// drained before it answers — the identity drain-then-handle adapter's
    /// law — exercised here with a real `agent` spawn rather than
    /// the stub `probe` class `desk_binding_drains_pending_surfaces_before_handling`
    /// uses: the surfaced `` `unpin `` must reach the bus before the
    /// spawn's own [`Kind::HarnessCall`] chrome line.
    #[test]
    fn surface_then_spawn_observes_the_surface_first() {
        let dir = tmp("surface-then-spawn");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, rx) = channel();
        let emit = Emitter::new(tx, session.id);
        let _ = session.run_shell(
            "call-1".to_string(),
            r#"surface `unpin [key: "test-marker"]; agent [prompt: #'go'#, name: 't', type: `amnemon, grant: `confined]"#,
            5,
            &emit,
        );
        drop(emit);

        let mut saw_unpin = false;
        let mut saw_spawn = false;
        for event in rx {
            match event.kind {
                Kind::Unpin { .. } => saw_unpin = true,
                Kind::HarnessCall { verb: "spawn", .. } => {
                    assert!(
                        saw_unpin,
                        "the surfaced unpin must be observed before the spawn's HarnessCall line"
                    );
                    saw_spawn = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_spawn,
            "the agent spawn's HarnessCall chrome must have been emitted"
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
                        FOValue::String {
                            value: "wake".into(),
                        },
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
                    items: vec![FOValue::String {
                        value: "sched-0".into(),
                    }],
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
    /// `sched-{id}` default, rather than an empty-string sentinel, and the
    /// receipt names that resolved default.
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
                        FOValue::String {
                            value: "wake".into(),
                        },
                    ],
                })),
            })
            .expect("a valid schedule with `none` must succeed");
        let FOValue::Map { entries } = answer else {
            panic!("expected a receipt record");
        };
        let Some(FOValue::String { value: label }) = entries
            .iter()
            .find_map(|(k, v)| (k == "label").then_some(v))
        else {
            panic!("expected a `label` entry");
        };
        assert!(
            label.starts_with("sched-"),
            "must be the minted default, got: {label}"
        );
        let listing = desk.services.schedules.list();
        assert_eq!(listing.len(), 1, "exactly one live schedule");
        assert_eq!(&listing[0].label, label);
    }

    /// `` `schedule ``/`` `schedule-list ``/`` `unschedule `` round trip
    /// through the desk arms: schedule one, see it listed with the fields
    /// it was given, remove it by label, and see the listing empty again.
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
                            payload: Some(Box::new(FOValue::String {
                                value: "nightly".into(),
                            })),
                        },
                        FOValue::String {
                            value: "wake".into(),
                        },
                    ],
                })),
            })
            .expect("a valid schedule must succeed");
        let FOValue::Map { entries } = answer else {
            panic!("expected a receipt record");
        };
        assert!(
            entries.iter().any(|(k, v)| k == "label"
                && matches!(v, FOValue::String { value } if value == "nightly")),
            "the receipt must carry the resolved label"
        );
        assert!(
            entries.iter().any(|(k, _)| k == "next-s"),
            "the receipt must carry next-s"
        );

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
            entries.iter().any(|(k, v)| k == "label"
                && matches!(v, FOValue::String { value } if value == "nightly")),
            "the listing must carry the label given at schedule time"
        );
        assert!(
            entries.iter().any(|(k, v)| k == "trigger"
                && matches!(v, FOValue::String { value } if value == "after 2h")),
            "the listing must carry the trigger's description"
        );
        assert!(
            !entries.iter().any(|(k, _)| k == "id"),
            "the listing carries no id"
        );

        desk.handle(FOValue::Variant {
            label: "unschedule".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String {
                    value: "nightly".into(),
                }],
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
        assert!(
            items.is_empty(),
            "the schedule must be gone after unschedule"
        );
    }

    /// The desk half of the duplicate-label refusal: a live schedule's own
    /// label, offered again, is refused before a second schedule is
    /// registered.
    #[test]
    fn schedule_at_the_desk_refuses_a_duplicate_label() {
        let desk = granted_desk();
        let spec = |label: &str| FOValue::List {
            items: vec![
                FOValue::Variant {
                    label: "after".into(),
                    payload: Some(Box::new(FOValue::String { value: "1s".into() })),
                },
                FOValue::Variant {
                    label: "some".into(),
                    payload: Some(Box::new(FOValue::String {
                        value: label.into(),
                    })),
                },
                FOValue::String {
                    value: "wake".into(),
                },
            ],
        };
        desk.handle(FOValue::Variant {
            label: "schedule".into(),
            payload: Some(Box::new(spec("nightly"))),
        })
        .expect("the first schedule must succeed");
        let err = desk
            .handle(FOValue::Variant {
                label: "schedule".into(),
                payload: Some(Box::new(spec("nightly"))),
            })
            .expect_err("a duplicate label must be refused");
        assert!(err.message.contains("nightly"), "got: {}", err.message);
        assert_eq!(
            desk.services.schedules.list().len(),
            1,
            "the duplicate registers nothing"
        );
    }

    /// A schedule the registry refuses tiers its act row like any other act.
    /// The row is emitted after the registry call precisely so `failed` can
    /// carry the outcome — emitted before, every schedule would read as one
    /// that landed.
    #[test]
    fn a_refused_schedule_tiers_its_act_row() {
        let (emit, rx) = crate::bus::dummy_emitter();
        let mut desk = granted_desk();
        desk.services.emit = emit;
        let spec = || FOValue::List {
            items: vec![
                FOValue::Variant {
                    label: "after".into(),
                    payload: Some(Box::new(FOValue::String { value: "1s".into() })),
                },
                FOValue::Variant {
                    label: "some".into(),
                    payload: Some(Box::new(FOValue::String {
                        value: "nightly".into(),
                    })),
                },
                FOValue::String {
                    value: "wake".into(),
                },
            ],
        };
        desk.handle(FOValue::Variant {
            label: "schedule".into(),
            payload: Some(Box::new(spec())),
        })
        .expect("the first schedule must succeed");
        desk.handle(FOValue::Variant {
            label: "schedule".into(),
            payload: Some(Box::new(spec())),
        })
        .expect_err("a duplicate label must be refused");

        let mut acts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Kind::HarnessCall {
                verb: "schedule",
                subject,
                payload,
                failed,
            } = event.kind
            {
                assert_eq!(subject.as_deref(), Some("nightly"));
                acts.push((payload, failed));
            }
        }
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

    /// `` `reply `` is refused outright when this agent does not hold
    /// `returns`, before the payload is even decoded, and the cell is left
    /// empty.
    #[test]
    fn reply_refused_without_returns() {
        let (emit, _rx) = crate::bus::dummy_emitter();
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

    /// A valid `reply` stages the payload into the cell — last write wins —
    /// and puts a subject-less `reply` act on the rail, carrying the value.
    #[test]
    fn reply_stages_the_payload_last_write_wins() {
        let (emit, rx) = crate::bus::dummy_emitter();
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
            Some(FOValue::String {
                value: "second".into()
            }),
            "the last staged reply wins"
        );

        let mut acts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Kind::HarnessCall {
                verb: "reply",
                subject,
                payload,
                failed,
            } = event.kind
            {
                assert_eq!(subject, None, "`reply` addresses no named subject");
                assert!(!failed, "a staged reply is not a refusal");
                acts.push(payload);
            }
        }
        assert_eq!(
            acts,
            ["first", "second"],
            "each reply emits its own act row, carrying the value it staged"
        );
    }

    // ── P3: engaged-child lifecycle, desk-integration scale ────────────────

    /// Register `child` under `parent_id` with `lease`, mirroring the
    /// registration half of [`crate::shell_eval::tools::agent::spawn_async`] — but
    /// with a caller-chosen lease, since a production spawn always arms the
    /// fixed [`crate::fleet::registry::AGENT_LEASE_IDLE`] and a millisecond lease is the only way
    /// these tests can exercise a real reap without waiting out the real
    /// constant.
    fn register_lease_child(
        parent_id: AgentId,
        lease: Option<Duration>,
        name: &str,
        child: &Agent,
    ) -> u64 {
        child
            .agents
            .register(Registration {
                id: child.id,
                parent: Some(parent_id),
                lease,
                name: name.to_string(),
                log_dir: child.log_dir(),
                cancel: child.cancel_token().clone(),
                reach: Some(child.seat.eval_reach()),
                mailbox: child.mailbox(),
                provider: child.provider_handle(),
            })
            .expect("a fresh child of a live parent always registers")
    }

    /// Attend an already-registered `child` to completion on a detached
    /// thread and deliver its result to `parent_mailbox`, exactly as
    /// [`spawn_async`](crate::shell_eval::tools::agent::spawn_async)'s own worker epilogue does (deliver, then settle).
    fn attend_and_deliver(
        mut child: Agent,
        name: &str,
        generation: u64,
        parent_mailbox: Mailbox,
    ) -> std::thread::JoinHandle<()> {
        let id = child.id;
        let registry = child.agents.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            let (tx, _rx) = crate::bus::channel();
            let emit = Emitter::new(tx, id);
            let (outcome, payload) = child.attend(&mut crate::agent::NoControl, &emit);
            let text = payload
                .as_ref()
                .map(crate::agent::render_reply)
                .unwrap_or_default();
            let _ = parent_mailbox.push(crate::bus::Post::AgentResult(crate::bus::AgentResult {
                name,
                outcome,
                text,
                elapsed: Duration::default(),
                generation,
            }));
            registry.settle(id, generation);
        })
    }

    /// Register a live, never-attended child under `parent_id` — just enough
    /// for [`AgentRegistry::has_children`] to read true, so `parent_id` parks
    /// [`crate::bus::ParkMode::HeldByChildren`] rather than quiescing before either steer lands (or,
    /// once engaged, so [`crate::bus::ParkMode::Engaged`] outranks [`crate::bus::ParkMode::HeldByChildren`] regardless — the
    /// P3 plan's own §5 edge case). Never touched again by the test.
    fn register_keepalive(registry: &AgentRegistry, parent_id: AgentId, log_dir: &std::path::Path) {
        let _ = registry.register(Registration {
            id: crate::agent::fresh_id(),
            parent: Some(parent_id),
            lease: None,
            name: format!("keepalive-{parent_id}"),
            log_dir: log_dir.to_path_buf(),
            cancel: crate::agent::cancel::Token::new(),
            reach: None,
            mailbox: crate::bus::Inbox::new().mailbox(),
            provider: ProviderHandle::new(scripted_provider()),
        });
    }

    /// Poll `path`'s contents until they contain `needle` or `timeout`
    /// elapses. The child's own `events.json` records every turn
    /// ([`crate::agent::event::SessionEvent::AssistantMessage`]), so a substring poll proves a
    /// turn actually ran, without any other side channel into a thread the
    /// test does not otherwise touch mid-flight.
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

    /// An engaged child answers a second steer after the human's attention
    /// has moved elsewhere — the lifecycle depends on nothing but
    /// [`AgentRegistry::steer`]'s own delivery, no notion of TUI focus at all.
    /// A fake grandchild keeps the child parked [`crate::bus::ParkMode::HeldByChildren`] rather than
    /// quiescing before it is ever engaged, so both steers land
    /// deterministically instead of racing the child's own attend loop.
    #[test]
    fn engaged_child_answers_a_second_steer_with_no_focus_involved() {
        let dir = tmp("engaged-second-steer");
        let parent = Agent::for_test(&dir, "system").unwrap();
        let child = parent.fork(parent.caps().clone()).expect("fork child");
        child.provider_handle().swap(Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            Script::new()
                .then(Reply::text("first response, no reply yet"))
                .then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "reply 'second response arrived'",
                )])),
        )));
        child.seed("say hi".into());
        let log_dir = child.log_dir();
        let child_id = child.id;
        let registry = child.agents.clone();

        let generation = register_lease_child(parent.id, None, "helper", &child);
        register_keepalive(&registry, child_id, &dir.join("keepalive"));
        let handle = attend_and_deliver(child, "helper", generation, parent.mailbox());

        assert!(
            registry.steer(child_id, "first message".into()),
            "the freshly registered, parked child accepts a steer"
        );
        assert!(
            registry.engaged(child_id),
            "steer renews the exchange clock"
        );
        assert!(
            eventually_logged(
                &log_dir.join("events.json"),
                "first response, no reply yet",
                Duration::from_secs(5),
            ),
            "the child must answer the first steer before the second is sent"
        );

        assert!(
            registry.steer(child_id, "second message".into()),
            "the parked, engaged child still accepts a second steer"
        );
        match wait_for_settle(&parent.inbox()) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    result.text.contains("second response arrived"),
                    "the second steer's response must reach the parent, got: {}",
                    result.text
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
        handle.join().expect("worker thread must not panic");
    }

    /// A millisecond-lease child that is never renewed is reaped mid-exchange
    /// and delivers exactly one [`crate::bus::AgentOutcome::Cancelled`] to the parent
    /// inbox; a sibling renewed partway through the same ttl survives past
    /// it. Child A runs a long, cheap tool-call sequence so the lease has
    /// something to interrupt mid-flight, comfortably inside the round-trip
    /// loop's own `MAX_STEPS` cap (250) — well past the reap's `ttl`, so the
    /// lease always wins the race and the surplus is never popped.
    #[test]
    fn ms_lease_child_never_renewed_is_cancelled_but_a_renewed_one_survives() {
        let dir = tmp("ms-lease-integration");
        let parent = Agent::for_test(&dir, "system").unwrap();
        // Child A's ttl must land comfortably inside the round-trip loop's
        // own `MAX_STEPS` cap (250 steps, well under 100 ms on this box) so
        // the lease — not the step cap — is what ends its exchange. Child B's
        // ttl is independent and generous instead, since its renewal is
        // paced by `thread::sleep` on the test's own thread, and this VM's
        // scheduling jitter (`container-disk-setup`/timing notes) can push
        // a short sleep well past its nominal length.
        let ttl_a = Duration::from_millis(25);
        let ttl_b = Duration::from_millis(300);

        let child_a = parent.fork(parent.caps().clone()).expect("fork child a");
        let mut long_script = Script::new();
        for i in 0..2_000u32 {
            long_script = long_script.then(Reply::tool_calls(vec![ral_call(&i.to_string(), "1")]));
        }
        child_a.provider_handle().swap(Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            long_script,
        )));
        child_a.seed("go".into());
        let id_a = child_a.id;
        let registry = child_a.agents.clone();
        let gen_a = register_lease_child(parent.id, Some(ttl_a), "child-a", &child_a);
        let handle_a = attend_and_deliver(child_a, "child-a", gen_a, parent.mailbox());

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
        assert!(!registry.is_live(id_a), "the reaped entry settles");
        handle_a.join().expect("worker thread must not panic");

        // Child B: renewed at half the ttl, kept `HeldByChildren` by a fake
        // grandchild so there is no script to race — `renew` alone, with no
        // message ever delivered, must be what defers the reap.
        let child_b = parent.fork(parent.caps().clone()).expect("fork child b");
        let id_b = child_b.id;
        let gen_b = register_lease_child(parent.id, Some(ttl_b), "child-b", &child_b);
        register_keepalive(&registry, id_b, &dir.join("keepalive-b"));
        let handle_b = attend_and_deliver(child_b, "child-b", gen_b, parent.mailbox());

        std::thread::sleep(ttl_b / 2);
        assert!(registry.renew(id_b), "a live entry renews");

        std::thread::sleep(ttl_b / 2 + Duration::from_millis(150));
        assert!(
            registry.is_live(id_b),
            "renewed at half the ttl, still alive past the original bound"
        );

        // Wind child B down cleanly rather than leaving its thread parked
        // forever: a terminate-cause cancel ends an unengaged
        // `HeldByChildren` park at once (`bus.rs`'s park-mode contract).
        registry.cancel(id_b);
        let _ = wait_for_settle(&parent.inbox());
        handle_b.join().expect("worker thread must not panic");
    }

    // ── `fetch-url` ───────────────────────────────────────────────────────

    /// A hand-rolled, minimal HTTP/1.1 server: binds an ephemeral port on
    /// 127.0.0.1, accepts exactly one connection, and answers with the
    /// pre-baked `response` bytes. No dev-dependency — the fetch-url tests
    /// need nothing richer than real bytes over a real socket.
    fn spawn_local_http_server(
        response: Vec<u8>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let _ = std::io::Write::write_all(&mut stream, &response);
            }
        });
        (addr, handle)
    }

    fn http_ok(body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn fetch_url_answer(desk: &ExarchDesk, url: &str) -> Result<FOValue, Error> {
        desk.handle(FOValue::Variant {
            label: "fetch-url".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: url.into() }],
            })),
        })
    }

    /// The plain-English refusal register every refusal message below is
    /// checked against: no implementation jargon, ever, in a message the
    /// model (and, via transcripts, a person) may read.
    const FETCH_URL_JARGON: &[&str] = &[
        "VM",
        "sandbox",
        "capability",
        "manifest",
        "card",
        "desk",
        "transport",
    ];

    fn assert_plain_english(message: &str) {
        for jargon in FETCH_URL_JARGON {
            assert!(
                !message.to_lowercase().contains(&jargon.to_lowercase()),
                "refusal must not say '{jargon}', got: {message}"
            );
        }
    }

    /// The identity-seat half of the "one answering function, two seats"
    /// claim: [`ExarchDesk::handle`] directly, no wire/engine involved. An
    /// allowed domain's `fetch-url` returns the served bytes and the audit
    /// ledger gains exactly one record.
    #[test]
    fn fetch_url_allowed_domain_returns_the_bytes_and_records_one_audit_line() {
        let body = b"hello from the test server".to_vec();
        let (addr, server) = spawn_local_http_server(http_ok(&body));
        let audit_path = tmp("fetch-url-allowed-audit").join("audit.jsonl");
        let desk = ExarchDesk {
            services: HostServices {
                egress: Egress {
                    policy: Arc::new(OrgPolicy {
                        allow: vec![addr.ip().to_string()],
                        max_response_bytes: 1_048_576,
                        rate_per_minute: 1_000,
                    }),
                    audit: AuditLog::for_test(&audit_path),
                    limiter: FetchLimiter::new(1_000),
                },
                ..base_services()
            },
        };
        let answer = fetch_url_answer(&desk, &format!("http://{addr}/"))
            .expect("an allowed fetch-url must succeed");
        match answer {
            FOValue::Bytes { value } => assert_eq!(value, body),
            other => panic!("expected FOValue::Bytes, got {other:?}"),
        }
        server.join().expect("server thread must not panic");
        let logged = std::fs::read_to_string(&audit_path).expect("audit ledger readable");
        assert_eq!(
            logged.lines().count(),
            1,
            "exactly one audit record for the one fetch, got: {logged}"
        );
        assert!(logged.contains("\"allowed\":true"));
    }

    /// A domain absent from the allowlist is refused, naming the domain and
    /// pointing at the IT department — and still gains an audit record
    /// (`allowed: false`), recorded before the desk answers.
    #[test]
    fn fetch_url_blocked_domain_is_refused_and_still_records_an_audit_line() {
        let audit_path = tmp("fetch-url-blocked-audit").join("audit.jsonl");
        let desk = ExarchDesk {
            services: HostServices {
                egress: Egress {
                    policy: Arc::new(OrgPolicy {
                        allow: vec!["only-this-host.example".to_string()],
                        max_response_bytes: 1_048_576,
                        rate_per_minute: 1_000,
                    }),
                    audit: AuditLog::for_test(&audit_path),
                    limiter: FetchLimiter::new(1_000),
                },
                ..base_services()
            },
        };
        let err = fetch_url_answer(&desk, "https://not-on-the-list.example/")
            .expect_err("a blocked domain must be refused");
        assert!(
            err.message.contains("not-on-the-list.example"),
            "must name the blocked domain, got: {}",
            err.message
        );
        assert!(
            err.message.contains("IT department"),
            "must point at IT, got: {}",
            err.message
        );
        assert_plain_english(&err.message);
        let logged = std::fs::read_to_string(&audit_path).expect("audit ledger readable");
        assert_eq!(
            logged.lines().count(),
            1,
            "the blocked fetch is still audited"
        );
        assert!(logged.contains("\"allowed\":false"));
    }

    /// A response larger than the policy's size cap is refused, naming the
    /// over-cap message, without the client ever reading the whole body —
    /// the cap is enforced by streaming through `.take(cap + 1)`, so an
    /// oversized response is rejected the instant the limited read fills.
    #[test]
    fn fetch_url_over_cap_response_is_refused() {
        let (addr, server) = spawn_local_http_server(http_ok(&[b'x'; 10_000]));
        let audit_path = tmp("fetch-url-over-cap-audit").join("audit.jsonl");
        let desk = ExarchDesk {
            services: HostServices {
                egress: Egress {
                    policy: Arc::new(OrgPolicy {
                        allow: vec![addr.ip().to_string()],
                        max_response_bytes: 100,
                        rate_per_minute: 1_000,
                    }),
                    audit: AuditLog::for_test(&audit_path),
                    limiter: FetchLimiter::new(1_000),
                },
                ..base_services()
            },
        };
        let err = fetch_url_answer(&desk, &format!("http://{addr}/"))
            .expect_err("an over-cap response must be refused");
        assert!(
            err.message.contains("100-byte limit"),
            "must name the size cap, got: {}",
            err.message
        );
        assert!(
            err.message.contains(&addr.ip().to_string()),
            "must name the host, got: {}",
            err.message
        );
        assert!(
            err.message.contains("IT department"),
            "got: {}",
            err.message
        );
        assert_plain_english(&err.message);
        server.join().expect("server thread must not panic");
    }

    /// A rate-limited fetch is refused once the budget is exhausted.
    #[test]
    fn fetch_url_over_rate_cap_is_refused() {
        let host = "rate-capped.example";
        let desk = ExarchDesk {
            services: HostServices {
                egress: Egress {
                    policy: Arc::new(OrgPolicy {
                        allow: vec![host.to_string()],
                        max_response_bytes: 1_048_576,
                        rate_per_minute: 1,
                    }),
                    audit: AuditLog::for_test(&tmp("fetch-url-rate-audit").join("audit.jsonl")),
                    limiter: FetchLimiter::new(1),
                },
                ..base_services()
            },
        };
        // The first call still fails (nothing is listening at this host),
        // but it consumes the one rate-budget slot before it fails on the
        // network — resolve_and_fetch takes the slot before dialling.
        let _ = fetch_url_answer(&desk, &format!("https://{host}/"));
        let err = fetch_url_answer(&desk, &format!("https://{host}/"))
            .expect_err("the second call within the window must be rate-capped");
        assert!(
            err.message.contains("too many fetches"),
            "must name the rate cap, got: {}",
            err.message
        );
        assert!(
            err.message.contains("IT department"),
            "must point at IT, got: {}",
            err.message
        );
        assert_plain_english(&err.message);
    }

    /// A redirect answer is refused, never followed: following it would let
    /// an allowed host bounce the fetch onto one the allowlist never
    /// admitted.
    #[test]
    fn fetch_url_refuses_a_redirect_rather_than_following_it() {
        let (addr, server) = spawn_local_http_server(
            b"HTTP/1.1 302 Found\r\nLocation: http://not-on-the-list.example/\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        );
        let audit_path = tmp("fetch-url-redirect-audit").join("audit.jsonl");
        let desk = ExarchDesk {
            services: HostServices {
                egress: Egress {
                    policy: Arc::new(OrgPolicy {
                        allow: vec![addr.ip().to_string()],
                        max_response_bytes: 1_048_576,
                        rate_per_minute: 1_000,
                    }),
                    audit: AuditLog::for_test(&audit_path),
                    limiter: FetchLimiter::new(1_000),
                },
                ..base_services()
            },
        };
        let err = fetch_url_answer(&desk, &format!("http://{addr}/"))
            .expect_err("a redirect must be refused, never followed");
        assert!(
            err.message.contains("redirect"),
            "must name the redirect, got: {}",
            err.message
        );
        assert_plain_english(&err.message);
        server.join().expect("server thread must not panic");
    }
}
