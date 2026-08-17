//! The exarch enquiry desk: the host half of `shell.enquire(class)`.
//!
//! [`HostServices`] is all `&Agent` can lend a handler without lending
//! `&mut Agent`/`&mut Shell`, since the reentrancy law bars a handler from
//! taking the session lock. [`ExarchDesk`] answers one enquiry against that
//! capture, paired with its [`SurfaceApplier`] in one [`HostSeam`];
//! [`DeskBinding`] wraps the seam for the identity transport, where a
//! handler's chrome would otherwise outrun the run's earlier surface output.

use crate::agent::event::{CACHE_SENTENCE, ContextOp, EditAuthority};
use crate::agent::{Agent, Build, LogCell, ProviderHandle, ReplyCell};
use crate::bus::{AgentId, Emitter, Mailbox};
use crate::egress;
use crate::fleet::registry::{AgentRegistry, MessageError, NotADescendant};
use crate::fleet::schedule::{CronSchedule, ScheduleRegistry, Trigger, parse_duration};
use crate::record::commit::SurfaceBuffer;
use crate::shell_eval::{self, PinDigests, Surface};
use crate::sync::LockExt;
use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use ral_core::transport::{Event, EventReceiver};
use ral_core::types::{
    CallSite, Capabilities, EnquiryDesk, Error, NO_DESK, NO_DESK_STATUS, Nursery, NurseryId,
    Observation, Observed,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A hatched child's cwd — `vm_manager::MachineSpec::GUEST_WORKSPACE`
/// restated as a literal, since the desk may not depend on `vm_manager`.
const HATCHED_CWD: &str = "/work";
/// A hatched child's home — the guest's disposable scratch tmpfs, restated
/// for the same reason (`synod::grant::GUEST_SCRATCH`): `$HOME` pointed at
/// the workspace would litter it with whatever XDG-defaulting tools drop.
const HATCHED_HOME: &str = "/tmp";

/// Mint a fresh, single-use hatch token from the OS entropy source: the
/// token, not the preamble's published magic, is the dial's real defense.
fn mint_token() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS randomness");
    u64::from_le_bytes(bytes)
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
    ContextDrop,
    ContextFold,
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
            Self::ContextDrop => "context-drop",
            Self::ContextFold => "context-fold",
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
            "context-drop" => Self::ContextDrop,
            "context-fold" => Self::ContextFold,
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
            Self::ContextDrop => named("dropped context"),
            Self::ContextFold => named("folded context"),
        }
    }
}

/// What this `ral` call has committed, in the order it landed: one
/// [`Observation`] per attempt that landed, minted at [`HostServices::commit_act`]
/// — the door every acting handler funnels through — and read back once a
/// raise discards the call's bindings but not its acts.
///
/// Effects persist across an unwind: a call the wall cut short still started
/// what it started and delivered what it delivered, while every binding it made
/// is gone. The fragment is how the raise says so, since the model's only other
/// in-band signal — the diagnostic — speaks of the failure and not of the acts.
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

/// Everything a desk handler may read off `&Agent`, snapshotted fresh at every
/// [`crate::agent::Agent::run_shell`] install so no capture goes stale mid-call.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent axis captured off the agent (interactive, returns, allow_schedule, search); not a candidate for a combined enum"
)]
pub(crate) struct HostServices {
    pub registry: AgentRegistry,
    /// `None` on a wire seat, whose real scratch lives in the guest it dials.
    pub scratch: Option<Arc<crate::bootstrap::Scratch>>,
    /// This run's own agent: parent of what it spawns, root of the descendant
    /// check `message`/`agent-cancel` enforce.
    pub parent: AgentId,
    pub mailbox: Mailbox,
    pub emit: Emitter,
    pub provider: ProviderHandle,
    /// The ceiling [`crate::policy::narrow`] attenuates a `grant` against.
    pub caps: Capabilities,
    /// Frozen at install: the path root `narrow` and every handler resolves from.
    pub cwd: PathBuf,
    /// Immutable for this run's extent; `agent-start` refuses once it reads zero.
    pub fuel: u32,
    /// Whether this agent holds `reply`; the desk's refusal keys on this bit,
    /// never on trunk-ness.
    pub returns: bool,
    pub allow_schedule: bool,
    /// A ceiling like `caps`: a spawn may narrow this bit, never widen it.
    pub search: bool,
    /// Where the `reply` handler stages its value, holding no `&mut Agent` to
    /// write it any other way.
    pub reply: ReplyCell,
    pub schedules: ScheduleRegistry,
    /// A `mnemon` spawn forks its inherited context off this.
    pub log: LogCell,
    /// Still carrying [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`]: the index is
    /// filtered per agent, so only an unresolved template is shareable.
    pub system_template: String,
    /// Resolves `system_template` with no live `Shell` at the desk.
    pub index: std::sync::Arc<crate::prompt::BuiltinIndex>,
    pub interactive: bool,
    /// The adoption end of the body's own [`ral_core::Shell::fork_into_nursery`];
    /// `agent-start` redeems a [`NurseryId`] here.
    pub nursery: Nursery,
    /// Read at install, so a desk older than a `/clear` refuses to spawn.
    pub generation: u64,
    pub disk_warn_bytes: Option<u64>,
    /// A spawn shares its parent's egress policy, never a fresh one.
    pub egress: egress::Egress,
    /// What this call has committed so far: minted per `ral` call, and read
    /// back by [`crate::shell_eval::report::render`] when a raise discards
    /// the bindings but not the acts.
    pub acts: ActFragment,
    /// Who the acts are committed on behalf of, read once at install: the
    /// desk holds no `Shell` to ask, and a host act's principal is the host's.
    pub principal: String,
    /// The agent's own pin register, shared verbatim so `pin-read`/`pin-list`
    /// answer from the same mirror the nudge already reads. `None` on a seat
    /// with no mirror of its own — the wire test seat above all.
    pub pins: Option<PinDigests>,
    /// Stated, never inferred from `scratch`'s incidental absence:
    /// `agent-start` chooses its identity or wire arm on this one fact.
    pub wire_seat: bool,
    /// The dial-side capability a wire trunk's `agent-start` hatches
    /// through; `None` on every identity trunk.
    pub hatchery: Option<Arc<dyn crate::agent::Hatchery>>,
    /// Fleet-shared: `agent-start`'s wire arm reserves a name and a token
    /// here, `agent-hatched`/`agent-abort` redeem or drop it.
    pub pending_hatches: crate::fleet::hatch::PendingHatches,
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
            CallSite::default(),
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
/// per `ral` call; wrapped in [`DeskBinding`] for the identity transport and
/// passed bare to `shell_eval::run_shell` for the wire path.
pub(crate) struct ExarchDesk {
    pub(crate) services: HostServices,
}

/// Whole seconds, saturating: nothing this desk answers comes near `i64::MAX`,
/// and saturation stays total where an `as` cast would wrap in silence.
fn secs_to_i64(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// Decode a payload as an `N`-element list, or a didactic error naming the
/// shape. The builtin's door checked already; the desk trusts nothing that
/// crossed the wire.
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
            format!("`{class}`: `{field}` must be an Int, got {}", other.shape()),
            1,
        )),
    }
}

fn payload_exchange(v: FOValue, class: &str, field: &str) -> Result<u64, Error> {
    let value = payload_int(v, class, field)?;
    u64::try_from(value).map_err(|_| {
        Error::new(
            format!("`{class}`: `{field}` must be a non-negative exchange number"),
            1,
        )
    })
}

/// The exchange list *is* the payload, not an argument wrapping one.
fn payload_exchanges(payload: Option<Box<FOValue>>, class: &str) -> Result<Vec<u64>, Error> {
    let Some(FOValue::List { items }) = payload.map(|payload| *payload) else {
        return Err(Error::new(
            format!("`{class}`: `exchanges` must be a list of non-negative Ints"),
            1,
        ));
    };
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| payload_exchange(item, class, &format!("exchanges[{index}]")))
        .collect()
}

/// The rail subject `` `context-read ``/`` `context-drop `` both mint from an
/// exchange list: `"exchanges [1, 2, 3]"`.
fn exchanges_subject(exchanges: &[u64]) -> String {
    format!(
        "exchanges [{}]",
        exchanges
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn payload_string(v: FOValue, class: &str, field: &str) -> Result<String, Error> {
    match v {
        FOValue::String { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `{field}` must be a Str, got {}", other.shape()),
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
            format!(
                "`{class}`: `{field}` must be a bare variant tag, got {}",
                other.shape()
            ),
            1,
        )),
    }
}

fn payload_bool(v: FOValue, class: &str, field: &str) -> Result<bool, Error> {
    match v {
        FOValue::Bool { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `{field}` must be a Bool, got {}", other.shape()),
            1,
        )),
    }
}

/// A bare variant tag carrying no payload — the shape `agent-cancel` and
/// `unschedule` answer with, since the whole of their outcome is which case
/// obtained.
fn bare_tag(label: &str) -> FOValue {
    FOValue::Variant {
        label: label.to_string(),
        payload: None,
    }
}

/// Decode a `schedule` trigger into a live [`Trigger`], re-running the parsers
/// the builtin's door already ran: that check is never the only line of defence.
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
            format!(
                "`{class}`: `trigger` must be `cron <expr>` or `after <dur>`, got {}",
                other.shape()
            ),
            1,
        )),
    }
}

/// Decode a `schedule` payload's `label`: a required `Str`, since every
/// caller now names its own schedule.
fn payload_label(v: FOValue, class: &str) -> Result<String, Error> {
    match v {
        FOValue::String { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `label` must be a Str, got {}", other.shape()),
            1,
        )),
    }
}

/// All that varies across `agent-start`'s two spawn kinds (`amnemon`/`mnemon`,
/// the `agent` builtin's `type`); every other step of the spawn spine is
/// identical for both and lives once in [`ExarchDesk::launch`].
struct Launch<'a> {
    /// The nursery id the builtin body parked its fork under.
    session_id: u64,
    grant: &'a str,
    /// Import the parent's model-visible conversation — a `mnemon` spawn only.
    inherit_context: bool,
    /// Refused if any live agent already bears it.
    name: String,
    prompt: String,
    /// The caller's request, clamped rather than taken at face value.
    search: bool,
}

impl ExarchDesk {
    /// Decode one enquiry class and answer it. An unrecognised class draws the
    /// extension law's error — never a silent default.
    ///
    /// # Errors
    /// Returns `Err` if `req` is not a [`FOValue::Variant`], names a class this
    /// desk does not answer, or the named class itself refuses.
    pub(crate) fn handle(&self, req: FOValue) -> Result<FOValue, Error> {
        let FOValue::Variant { label, payload } = req else {
            return Err(Error::new(
                format!(
                    "enquiry request must be a variant naming its class, got {}",
                    req.shape()
                ),
                1,
            ));
        };
        match label.as_str() {
            "agent-start" => self.agent_start(payload),
            "agent-hatched" => self.agent_hatched(payload),
            "agent-abort" => self.agent_abort(payload),
            "agent-list" => Ok(self.agent_list()),
            "agent-cancel" => self.agent_cancel(payload),
            "message" => self.message(payload),
            "schedule" => self.schedule(payload),
            "schedule-list" => self.schedule_list(),
            "unschedule" => self.unschedule(payload),
            "reply" => self.reply(payload),
            "pin-read" => self.pin_read(payload),
            "pin-list" => Ok(self.pin_list()),
            "context" => Ok(self.context()),
            "context-read" => self.context_read(payload),
            "context-drop" => self.context_drop(payload),
            "context-fold" => self.context_fold(payload),
            other => Err(Error::new(
                format!("unrecognised enquiry class `{other}`"),
                1,
            )),
        }
    }

    /// The spawn spine behind `agent-start`: adopt the parked fork, narrow its
    /// authority, fork its log off the parent's, assemble it at one less unit of
    /// fuel, hand it to `spawn_async`. Every cheap guard runs before
    /// [`Nursery::adopt`]; a refusal after it simply drops the adopted `Shell`.
    fn launch(&self, spec: Launch) -> Result<FOValue, Error> {
        let s = &self.services;

        // The captured fuel, caps, and grant are only as fresh as the
        // generation they were snapshotted under.
        if s.generation != s.registry.generation() {
            return Err(Error::new(
                "agent-start refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale — \
                 issue agent again on your next turn",
                1,
            ));
        }

        // Fuel bounds depth, not fan-out: the parent's own is never debited, so
        // siblings are free and only a deep enough chain bottoms out.
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

        // Didactic, not race-free: `register` below (identity) or
        // `PendingHatches::reserve` (wire) re-checks under its own lock, and
        // that is what closes a same-name race. A name a wire spawn is still
        // waiting to dial home under counts exactly like a live agent's.
        if s.registry.name_live(&spec.name) || s.pending_hatches.name_reserved(&spec.name) {
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

        if s.wire_seat {
            return self.launch_wire(spec);
        }

        let Some(shell) = s.nursery.adopt(NurseryId(spec.session_id)) else {
            return Err(Error::new(
                "`agent-start`: no forked session parked under this id — it may already have \
                 started, or the run that forked it has ended",
                1,
            ));
        };

        let (child_caps, child_log, system_prompt) = self.fork_child(&spec)?;

        // Mirrors `Agent::fork_with`'s `Build` literal, with the adopted shell
        // and forked log standing in for `fork_session`'s fresh ones.
        let fuel = s.fuel - 1;
        // Unreachable: an identity seat always carries its own scratch.
        let Some(scratch) = s.scratch.clone() else {
            return Err(Error::new(
                "agent-start refused: this session has no host-side scratch to fork an \
                 in-process child into",
                1,
            ));
        };
        // No detach: the shell was forked, not booted, so it carries no policy.
        let seat =
            crate::agent::seat::Seat::identity(shell, scratch, s.cwd.clone(), false, &child_log);
        let child = Agent::assemble(Build {
            system: s.system_template.clone(),
            system_prompt,
            index: s.index.clone(),
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
            // A child may narrow its parent's search reach, never widen it.
            search: s.search && spec.search,
            agents: s.registry.clone(),
            run_lock: None,
            resume_summary: None,
            disk_warn_bytes: s.disk_warn_bytes,
            egress: s.egress.clone(),
            hatchery: s.hatchery.clone(),
            pending_hatches: s.pending_hatches.clone(),
        });

        self.spawn_and_receipt(child, spec.name, spec.prompt)
    }

    /// The child-log/capability half of the spawn spine, shared by both
    /// arms: an identity spawn runs it once it has adopted its shell, a wire
    /// spawn runs it at phase 1 and stashes the result for phase 2, since the
    /// host has no shell of its own to gate on.
    fn fork_child(
        &self,
        spec: &Launch,
    ) -> Result<(Capabilities, crate::agent::event::AgentLog, String), Error> {
        let s = &self.services;
        // `narrow` names all six legal bases in its own diagnostic, so an
        // unknown `grant` needs no refusal text here.
        let cwd = s.cwd.to_string_lossy();
        let child_caps = crate::policy::narrow(&s.caps, spec.grant, &cwd)
            .map_err(|reason| Error::new(reason, 1))?;

        // On the raw `AgentLog`, not through `Agent::inherit_context`, which
        // needs a `&Self` the child is not yet. The index resolves here, off the
        // same bits the `Build` below fixes, so the opening bookend records the
        // child's real system length rather than the template's.
        let child_id = crate::agent::fresh_id();
        let system_prompt = s.index.apply(
            &s.system_template,
            &crate::prompt::Grants {
                returns: true,
                allow_schedule: s.allow_schedule,
                spawns: s.fuel.saturating_sub(1) > 0,
            },
        );
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
        Ok((child_caps, child_log, system_prompt))
    }

    /// `agent-start`'s wire arm: no shell to adopt host-side, so mint a
    /// token, stash everything `agent-hatched` will need to finish the spine,
    /// and answer `` `hatch `` instead of `` `started ``.
    fn launch_wire(&self, spec: Launch) -> Result<FOValue, Error> {
        let s = &self.services;
        let Some(hatchery) = s.hatchery.clone() else {
            return Err(Error::new(
                "agent-start refused: this wire session has no hatchery installed to dial a \
                 helper engine through — a construction bug, since a fuelled wire trunk is \
                 refused at Agent::root without one",
                1,
            ));
        };
        let (child_caps, child_log, system_prompt) = self.fork_child(&spec)?;

        let token = mint_token();
        s.pending_hatches.reserve(
            token,
            crate::fleet::hatch::PendingHatch::new(
                spec.name,
                spec.prompt,
                child_caps,
                child_log,
                system_prompt,
                s.search && spec.search,
            ),
        );
        Ok(FOValue::Variant {
            label: "hatch".to_string(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![
                    (
                        "token".to_string(),
                        // Bit-preserving: the guest hands this same value
                        // back verbatim, never arithmetic on it.
                        FOValue::Int {
                            value: token.cast_signed(),
                        },
                    ),
                    (
                        "port".to_string(),
                        FOValue::Int {
                            value: i64::from(hatchery.port()),
                        },
                    ),
                ],
            })),
        })
    }

    /// `` `agent-hatched `` — phase 2 of the wire arm: await the correlated
    /// dial, check its preamble, adopt it as a wire seat, and finish the
    /// spawn spine `launch_wire` stashed. The answer is the same `` `started ``
    /// receipt the identity arm gives; the builtin cannot tell which served it.
    fn agent_hatched(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let [token] = payload_list(payload, "agent-hatched", "[token]")?;
        // The same bit-preserving cast `launch_wire` minted with.
        let token = payload_int(token, "agent-hatched", "token")?.cast_unsigned();

        let Some(pending) = s.pending_hatches.take(token) else {
            return Err(Error::new(
                "agent-hatched: no pending hatch for this token — it may already have been \
                 aborted, or it expired waiting",
                1,
            ));
        };
        let Some(hatchery) = s.hatchery.clone() else {
            return Err(Error::new(
                "agent-hatched: this session has no hatchery installed — a construction bug",
                1,
            ));
        };

        let mut stream = hatchery
            .await_dial(token, crate::agent::DIAL_PATIENCE)
            .map_err(|reason| {
                Error::new(
                    format!(
                        "agent-hatched refused: the helper's engine never dialled home within \
                         {:?} — {reason}",
                        crate::agent::DIAL_PATIENCE
                    ),
                    1,
                )
            })?;
        let dialed = ral_core::hatch_preamble::read(&mut stream).map_err(|e| Error::new(e, 1))?;
        if dialed != token {
            return Err(Error::new(
                "agent-hatched refused: the dial's own token did not match the one this hatch \
                 was minted for",
                1,
            ));
        }
        let transport = ral_core::transport::WireTransport::adopt(
            stream,
            ral_core::transport::Liveness::default(),
        )
        .map_err(|e| {
            Error::new(
                format!("agent-hatched: could not adopt the hatched wire: {e}"),
                1,
            )
        })?;
        // The same two paths synod's own trunk seat uses.
        let seat = crate::agent::seat::Seat::wire(
            transport,
            PathBuf::from(HATCHED_CWD),
            PathBuf::from(HATCHED_HOME),
        );

        let fuel = s.fuel - 1;
        let child = Agent::assemble(Build {
            system: s.system_template.clone(),
            system_prompt: pending.system_prompt,
            index: s.index.clone(),
            caps: pending.child_caps,
            seat,
            log: pending.child_log,
            parent: Some(s.parent),
            fuel,
            provider: ProviderHandle::new(s.provider.current()),
            interactive: s.interactive,
            returns: true,
            allow_schedule: s.allow_schedule,
            tool_enabled: true,
            search: pending.search,
            agents: s.registry.clone(),
            run_lock: None,
            resume_summary: None,
            disk_warn_bytes: s.disk_warn_bytes,
            egress: s.egress.clone(),
            hatchery: Some(hatchery),
            pending_hatches: s.pending_hatches.clone(),
        });

        self.spawn_and_receipt(child, pending.name, pending.prompt)
    }

    /// `` `agent-abort `` — the builtin's own cleanup when its guest-side
    /// hatch fails after `agent-start` already minted a token: drop the
    /// pending hatch and free the name it reserved. Idempotent: a token
    /// already redeemed or already expired is simply gone.
    fn agent_abort(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let [token] = payload_list(payload, "agent-abort", "[token]")?;
        let token = payload_int(token, "agent-abort", "token")?.cast_unsigned();
        s.pending_hatches.take(token);
        Ok(FOValue::Unit)
    }

    /// Hand `child` to `spawn_async` and answer the same `` `started `` receipt
    /// both arms give — the builtin cannot tell which arm served it.
    fn spawn_and_receipt(
        &self,
        child: Agent,
        name: String,
        prompt: String,
    ) -> Result<FOValue, Error> {
        let s = &self.services;
        // Held past the move, so the commitment arm can still name the act.
        let acted_name = name.clone();
        let acted_prompt = prompt.clone();
        let spawned = crate::shell_eval::tools::agent::spawn_async(
            &s.registry,
            s.parent,
            s.mailbox.clone(),
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

    /// `` `agent-start `` — the desk half of the `agent` builtin: decode, then
    /// hand off to [`Self::launch`] for the spawn spine.
    fn agent_start(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let [session, kind, prompt, name, grant, search] = payload_list(
            payload,
            "agent-start",
            "[session, kind, prompt, name, grant, search]",
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
        let search = payload_bool(search, "agent-start", "search")?;

        self.launch(Launch {
            session_id,
            grant: &grant,
            inherit_context: mnemon,
            name,
            prompt,
            search,
        })
    }

    /// `` `agent-list `` — the desk half of `agents`: a registry listing scoped
    /// to this agent's descendants. Silent, since the answer is the listing.
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

    /// `` `agent-cancel `` — resolve a live descendant by name and cancel it,
    /// scoped as [`AgentRegistry::cancel_scoped`] enforces. Answers `` `cancelled ``
    /// or `` `no-such-agent ``: both are successful calls, so the outcome rides
    /// in the answer rather than in whether the call raised at all.
    fn agent_cancel(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let [name] = payload_list(payload, "agent-cancel", "[name]")?;
        let name = payload_string(name, "agent-cancel", "name")?;

        // The row is derived after the call: one claiming "cancelled" ahead of
        // it would assert an effect the world never saw. `cancel` takes no
        // argument, so its payload column carries the outcome instead.
        let cancelled = s
            .registry
            .resolve_name(&name)
            .map_or(Ok(false), |id| s.registry.cancel_scoped(s.parent, id));
        let landed = matches!(cancelled, Ok(true));
        let (payload, content, answer) = match cancelled {
            Ok(true) => (
                String::new(),
                format!("cancelling agent '{name}'"),
                Ok(bare_tag("cancelled")),
            ),
            Ok(false) => (
                "no live agent by that name".to_string(),
                format!("no live agent named '{name}'"),
                Ok(bare_tag("no-such-agent")),
            ),
            Err(NotADescendant(_)) => (
                "refused: not a descendant".to_string(),
                format!(
                    "agent '{name}' is not an agent you started; agent-cancel may only reach a descendant of yours"
                ),
                Err(()),
            ),
        };
        s.commit_act(DeskAct::Cancel, Some(&name), payload, !landed);
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        // Only a scope violation raises; the raise is the model's only copy
        // of it. A real cancel and a miss both answer — the tag names which.
        answer.map_err(|()| Error::new(content, 1))
    }

    /// `` `message `` — resolve a live descendant by name and send it a note.
    fn message(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        let [name, text] = payload_list(payload, "message", "[name, text]")?;
        let name = payload_string(name, "message", "name")?;
        let text = payload_string(text, "message", "text")?;

        // Unlike `cancel`, an unresolved name refuses rather than no-ops:
        // `message` promises delivery and there is nothing to deliver to. A
        // target settling between the name resolving and the send lands in the
        // same arm, one step later. The row goes up after the send, since a
        // delivery's payload is its text but a refusal's is why.
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
        s.commit_act(DeskAct::Message, Some(&name), payload, !ok);
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        // Success is its own confirmation; a refusal raises, and the raise is
        // the model's only copy of it.
        if ok {
            Ok(FOValue::Unit)
        } else {
            Err(Error::new(content, 1))
        }
    }

    /// The self-wakeup guard the whole schedule family runs first.
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

    /// `` `schedule `` — arm a self-wakeup through [`ScheduleRegistry::schedule`]
    /// and receipt it as `[label: Str, next-s: Int]`.
    fn schedule(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        self.require_schedule_grant("schedule")?;
        let s = &self.services;

        let [trigger, label, prompt] =
            payload_list(payload, "schedule", "[trigger, label, prompt]")?;
        let trigger = payload_trigger(trigger, "schedule")?;
        let label = payload_label(label, "schedule")?;
        let prompt = payload_string(prompt, "schedule", "prompt")?;

        // The row goes up after the registry call, so a refusal tiers instead
        // of reading as one that landed.
        let described = trigger.describe();
        let result = s
            .schedules
            .schedule(trigger, prompt, label.clone(), &s.mailbox);
        let (payload, content) = match &result {
            Ok(receipt) => (
                described,
                format!(
                    "scheduled '{}' ({}s to first fire)",
                    receipt.label,
                    secs_to_i64(receipt.next_in)
                ),
            ),
            Err(e) => (format!("refused: {e}"), format!("could not schedule: {e}")),
        };
        s.commit_act(DeskAct::Schedule, Some(&label), payload, result.is_err());
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
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

    /// `` `schedule-list `` — the desk half of `schedules`: a snapshot of this
    /// agent's live wakeups. Silent, like `` `agent-list ``.
    fn schedule_list(&self) -> Result<FOValue, Error> {
        self.require_schedule_grant("schedule-list")?;
        let s = &self.services;
        Ok(FOValue::List {
            items: s
                .schedules
                .list()
                .into_iter()
                .map(|info| {
                    // A cron with nothing inside `next_delay`'s search horizon
                    // says "never" as `secs_to_i64`'s own saturated ceiling.
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

    /// `` `unschedule `` — remove one scheduled wakeup by label. Answers
    /// `` `removed `` or `` `no-such-label ``: both are successful calls, so the
    /// outcome rides in the answer rather than in whether the call raised.
    fn unschedule(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        self.require_schedule_grant("unschedule")?;
        let s = &self.services;
        let [label] = payload_list(payload, "unschedule", "[label]")?;
        let label = payload_string(label, "unschedule", "label")?;

        // Shaped like `agent-cancel`: only the grant refusal raises, and the
        // rail's payload column spells out the miss the answer's tag names,
        // since the verb has no argument of its own to show there.
        let removed = s.schedules.unschedule(&label);
        let (payload, content, answer) = if removed {
            (
                String::new(),
                format!("unscheduled '{label}'"),
                bare_tag("removed"),
            )
        } else {
            (
                "no live schedule by that label".to_string(),
                format!("no live schedule labelled '{label}'"),
                bare_tag("no-such-label"),
            )
        };
        s.commit_act(DeskAct::Unschedule, Some(&label), payload, !removed);
        s.record_forensic(crate::record::Forensic::HarnessResult { text: content });
        Ok(answer)
    }

    /// `` `reply `` — stage the payload into the cell [`Agent::deliberate`] lifts
    /// into an `Outcome::Replied` terminal once the batch drains. Refused on
    /// every non-returning agent, keyed on `returns` and never on trunk-ness.
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
        let payload = if display.is_empty() {
            "(empty reply)".into()
        } else {
            display
        };
        // No subject: the parent is the only recipient a returning agent has.
        s.commit_act(DeskAct::Reply, None, payload, false);
        s.reply.set(value);
        Ok(FOValue::Unit)
    }

    /// `` `pin-read `` — the card stored under `key` on this agent's own
    /// register, canonically re-encoded, or `` `unit `` on a miss or an absent
    /// mirror. Read-after-write within one run is sound because
    /// [`DeskBinding::enquire`] drains queued surface frames before handling.
    fn pin_read(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let [key] = payload_list(payload, "pin-read", "[key]")?;
        let key = payload_string(key, "pin-read", "key")?;
        let Some(pins) = &self.services.pins else {
            return Ok(FOValue::Unit);
        };
        let m = pins.lock().expect("pin register poisoned");
        match m.get(&key) {
            // A card value is always first-order, so this conversion never fails.
            Some(digest) => FOValue::try_from(&crate::bus::card::encode_card(&digest.card)),
            None => Ok(FOValue::Unit),
        }
    }

    /// `` `pin-list `` — the keys currently occupied on this agent's own
    /// register, in `BTreeMap` order; an absent mirror answers empty.
    fn pin_list(&self) -> FOValue {
        FOValue::List {
            items: self.services.pins.as_ref().map_or_else(Vec::new, |pins| {
                pins.lock()
                    .expect("pin register poisoned")
                    .keys()
                    .map(|key| FOValue::String { value: key.clone() })
                    .collect()
            }),
        }
    }

    /// `` `context `` — a survey of this agent's own transcript: one span per
    /// digest/import/exchange, plus the running byte and step totals. Silent,
    /// like `` `agent-list ``: the answer is the survey.
    fn context(&self) -> FOValue {
        let survey = self.services.log.lock().context_survey();
        let spans = survey
            .items
            .into_iter()
            .map(|item| FOValue::Map {
                entries: vec![
                    (
                        "exchange".to_string(),
                        FOValue::Int {
                            value: u64_to_i64(item.exchange),
                        },
                    ),
                    (
                        "kind".to_string(),
                        FOValue::String {
                            value: item.kind.as_str().to_string(),
                        },
                    ),
                    (
                        "prompt".to_string(),
                        FOValue::String {
                            value: item.opening,
                        },
                    ),
                    (
                        "bytes".to_string(),
                        FOValue::Int {
                            value: usize_to_i64(item.bytes),
                        },
                    ),
                    (
                        "steps".to_string(),
                        FOValue::Int {
                            value: usize_to_i64(item.steps),
                        },
                    ),
                    ("live".to_string(), FOValue::Bool { value: item.live }),
                ],
            })
            .collect();
        FOValue::Map {
            entries: vec![
                ("spans".to_string(), FOValue::List { items: spans }),
                (
                    "total-bytes".to_string(),
                    FOValue::Int {
                        value: usize_to_i64(survey.total_bytes),
                    },
                ),
                (
                    "total-steps".to_string(),
                    FOValue::Int {
                        value: usize_to_i64(survey.total_steps),
                    },
                ),
                (
                    "cache".to_string(),
                    FOValue::String {
                        value: CACHE_SENTENCE.to_string(),
                    },
                ),
            ],
        }
    }

    /// `` `context-read `` — the rendered transcript of the named exchanges,
    /// probing rather than acting: a read commits no act, so the verb string
    /// below is a second mint outside [`DeskAct::verb`] on purpose.
    fn context_read(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let exchanges = payload_exchanges(payload, "context-read")?;
        let subject = exchanges_subject(&exchanges);
        let result = self.services.log.lock().read_context(&exchanges);
        self.services
            .record_display(crate::record::Display::HarnessCall {
                verb: "context-read".to_string(),
                subject: Some(subject.clone()),
                payload: subject,
                failed: result.is_err(),
            });
        result
            .map(|value| FOValue::String { value })
            .map_err(|error| Error::new(error, 1))
    }

    /// `` `context-drop `` — shed the named exchanges from the view, receipted
    /// by [`Self::context_edit`] with the resulting byte delta.
    fn context_drop(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let exchanges = payload_exchanges(payload, "context-drop")?;
        let subject = exchanges_subject(&exchanges);
        self.context_edit(
            DeskAct::ContextDrop,
            Some(&subject),
            subject.clone(),
            ContextOp::Drop { exchanges },
        )
    }

    /// `` `context-fold `` — collapse every exchange through `through` into
    /// one digest, receipted by [`Self::context_edit`] with the resulting byte
    /// delta.
    fn context_fold(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let [through, digest] = payload_list(payload, "context-fold", "[through, digest]")?;
        let through = payload_exchange(through, "context-fold", "through")?;
        let digest = payload_string(digest, "context-fold", "digest")?;
        let subject = format!("through {through}");
        let payload = format!("{subject}, digest {digest}");
        self.context_edit(
            DeskAct::ContextFold,
            Some(&subject),
            payload,
            ContextOp::Fold {
                through_exchange: through,
                digest,
            },
        )
    }

    /// The `` `context-drop ``/`` `context-fold `` tail both share: apply the
    /// edit, commit the act under `refused` on either outcome, and mirror the
    /// receipt or the refusal onto the trace before answering.
    fn context_edit(
        &self,
        act: DeskAct,
        subject: Option<&str>,
        payload: String,
        op: ContextOp,
    ) -> Result<FOValue, Error> {
        let result = self
            .services
            .log
            .lock()
            .apply_edit(op, EditAuthority::Model);
        match result {
            // `apply_edit` records `ContextEdited` through the seam, and the
            // live row derives from that published record: nothing separate
            // to emit here.
            Ok(receipt) => {
                self.services.commit_act(act, subject, payload, false);
                let text = format!(
                    "context changed by {:+} serialized bytes",
                    receipt.bytes_delta
                );
                self.services
                    .record_forensic(crate::record::Forensic::HarnessResult { text });
                Ok(FOValue::Map {
                    entries: vec![(
                        "bytes-delta".to_string(),
                        FOValue::Int {
                            value: receipt.bytes_delta,
                        },
                    )],
                })
            }
            Err(error) => {
                self.services.commit_act(act, subject, payload, true);
                self.services
                    .record_forensic(crate::record::Forensic::HarnessResult {
                        text: error.clone(),
                    });
                Err(Error::new(error, 1))
            }
        }
    }
}

/// Decodes a surfaced value onto the bus, folding a `` `pin ``/`` `unpin ``
/// into the pin mirror and the io/diff surfaces into the commit producer's
/// [`SurfaceBuffer`], so what reaches the record is the deduped, coalesced
/// form the user saw. [`HostSeam::apply`] is the one instance
/// `shell_eval::run_shell`'s own drain and [`DeskBinding`]'s both reach
/// through, so the two paths render off the same applier rather than two that
/// could drift.
pub(crate) struct SurfaceApplier {
    pub(crate) pins: Option<PinDigests>,
    /// The owning session's id and record seam — what the buffered commits
    /// stamp through when they flush.
    pub(crate) id: AgentId,
    pub(crate) recorder: crate::record::Emitter,
    /// Per-call, like the applier itself: `run_shell` flushes it at the call
    /// boundary, so a call's effects record contiguously.
    pub(crate) surface: Mutex<SurfaceBuffer>,
}

impl SurfaceApplier {
    /// Apply one live [`Event::Surface`] value.
    pub(crate) fn live(&self, val: FOValue) {
        if let Some(surface) = shell_eval::accepted_surface(&RalValue::from(val), &self.recorder) {
            if let Some(pins) = &self.pins {
                // Fatal, never skipped: dropping a disposition here would
                // desync the mirror from the stream with no signal at all.
                let mut m = pins.lock().expect("pin register poisoned");
                match &surface {
                    Surface::Pin { key, card } => {
                        m.insert(key.clone(), shell_eval::PinDigest::new(card.clone()));
                    }
                    Surface::Unpin { key } => {
                        m.remove(key);
                    }
                    _ => {}
                }
            }
            let mut buf = self.surface.lock_ignore_poison();
            if let Err(error) = absorb_surface(&mut buf, &self.recorder, self.id, &surface) {
                drop(buf);
                self.recorder.report_fault(&error);
            }
        }
    }

    /// The call boundary: record whatever the buffers still hold, in their
    /// barrier order.
    pub(crate) fn flush(&self) {
        let mut buf = self.surface.lock_ignore_poison();
        if let Err(error) = buf.flush_surfaces(&self.recorder) {
            drop(buf);
            self.recorder.report_fault(&error);
        }
    }
}

/// Feed one decoded [`Surface`] to the commit producer — an io observation
/// into its bucket, a lone diff card into the patch buffer, and every other
/// surface class as its own commit — shared by [`SurfaceApplier::live`] and
/// the deferred-batch path in `agent::attend::announce`, so the two cannot
/// drift on what records.
///
/// Only the bucketed observations — read, exec, grep, write — enter the
/// buffer; a worker birth or capability check joins no group and records at
/// once as its own commit, exactly the line the buffer's own `unreachable!`
/// arms draw.  A done, notice, or generic card flushes the buffer first:
/// those close or interrupt the run around them, so the record keeps the
/// order the user saw.
///
/// A pin publishes twice, per the mirror it also feeds in
/// [`SurfaceApplier::live`]: [`crate::record::Forensic::Pin`]/`Unpin` is the
/// durable breadcrumb a resume replays, [`crate::record::Transient::Pin`]/
/// `Unpin` is the register the live process is holding, which a resume does
/// not restore.
pub(crate) fn absorb_surface(
    buf: &mut SurfaceBuffer,
    recorder: &crate::record::Emitter,
    id: AgentId,
    surface: &Surface,
) -> std::io::Result<()> {
    match surface {
        Surface::Observation(event) => match &event.what {
            Observed::Read { .. }
            | Observed::Command { .. }
            | Observed::Grep { .. }
            | Observed::Write { .. } => buf.absorb_observation(recorder, id, (**event).clone()),
            Observed::Worker { .. } | Observed::Capability { .. } | Observed::Act { .. } => {
                let value = crate::bus::card::observation_wire(event);
                let _recorded = recorder.emit(crate::record::Display::Observation { value })?;
                Ok(())
            }
        },
        Surface::Card(card) => match card.clone().into_single_diff() {
            Ok((path, hunks)) => buf.absorb_patch(recorder, id, path, hunks),
            // A richer card is the kit's own communication: the mark tree is
            // the fact, so it records whole and opaquely.  The buffers flush
            // first, keeping the record in the order the user saw — a card
            // never joins a group, so nothing coalesced is torn by the flush.
            Err(card) => {
                buf.flush_surfaces(recorder)?;
                let marks =
                    serde_json::to_value(&card).expect("Card's derived Serialize cannot fail");
                let _recorded = recorder.emit(crate::record::Display::Card { marks })?;
                Ok(())
            }
        },
        Surface::Done(outcome) => {
            buf.flush_surfaces(recorder)?;
            let _recorded = recorder.emit(crate::record::Display::Done {
                outcome: done_fact(outcome),
            })?;
            Ok(())
        }
        Surface::Notice(notice) => {
            buf.flush_surfaces(recorder)?;
            let _recorded = recorder.emit(crate::record::Display::Notice {
                notice: notice_fact(notice),
            })?;
            Ok(())
        }
        // Register state, not scrollback: the history informs the resume note,
        // and the buffer is left unflushed on purpose — a pin joins no group
        // and must not split the one it is never offered to.
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

/// The data half of a settled worker's `` `done `` card, in the record's own
/// spelling of the same three outcomes.
fn done_fact(outcome: &crate::bus::card::DoneOutcome) -> crate::record::DoneOutcome {
    match outcome {
        crate::bus::card::DoneOutcome::Ok => crate::record::DoneOutcome::Ok,
        crate::bus::card::DoneOutcome::Err { message, status } => crate::record::DoneOutcome::Err {
            message: message.clone(),
            status: *status,
        },
        crate::bus::card::DoneOutcome::Panic { message } => crate::record::DoneOutcome::Panic {
            message: message.clone(),
        },
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

/// Installed by [`crate::agent::seat::RunGuard`] once a call's real desk
/// retires, so nothing holds that call's [`HostServices`] — its [`Emitter`]
/// above all — past the call that owns it. Never observed in practice: nothing
/// dispatches without a fresh real desk installed first.
pub(crate) struct AbsentDesk;

impl EnquiryDesk for AbsentDesk {
    /// `_cancel` goes unpolled: every answer here is immediate and
    /// unconditional, so there is no wait for a poll point to interrupt.
    fn enquire(
        &self,
        _req: FOValue,
        _cancel: &ral_core::process::CancelScope,
    ) -> Result<FOValue, Error> {
        Err(Error::new(NO_DESK, NO_DESK_STATUS))
    }
}

/// One `ral` call's whole host seam: the desk that answers its enquiries and
/// the applier that renders its surfaced values, built once per call and
/// shared by `Arc` between [`DeskBinding`] (the identity install) and
/// `shell_eval::run_shell`'s own drain — so the two paths through one call can
/// never be handed different desks, or different appliers, by mistake.
pub(crate) struct HostSeam {
    pub(crate) desk: ExarchDesk,
    pub(crate) apply: SurfaceApplier,
}

/// The identity transport's drain-then-handle adapter.
///
/// Under [`ral_core::transport::IdentityTransport`], [`ral_core::Shell::enquire`]
/// calls into the installed desk synchronously, mid-`dispatch` — before
/// `dispatch_to_report`'s drain loop, which starts only once `dispatch` returns.
/// The run's earlier surface frames are therefore still queued, so
/// [`Self::enquire`] applies them first: answering ahead of them would render a
/// handler's chrome before output that preceded it.
pub(crate) struct DeskBinding {
    pub(crate) seam: Arc<HostSeam>,
    pub(crate) events: Arc<EventReceiver>,
}

impl EnquiryDesk for DeskBinding {
    /// `_cancel` goes unpolled: `ExarchDesk::handle` answers every class from
    /// its captured `HostServices` without parking on another party — a spawn
    /// starts a detached worker and returns, a cancel signals a token and
    /// returns, neither waits for the far side to react. The one arm that
    /// grows rather than finishing at once is `agent-start`'s `mnemon` copy of
    /// the parent's inherited context (`ExarchDesk::launch`): still bounded,
    /// by the conversation so far, but the largest uncancellable window this
    /// desk ever holds open.
    fn enquire(
        &self,
        req: FOValue,
        _cancel: &ral_core::process::CancelScope,
    ) -> Result<FOValue, Error> {
        while let Some((_, event)) = self.events.try_recv() {
            if let Event::Surface(val) = event {
                self.seam.apply.live(val);
            }
        }
        self.seam.desk.handle(req)
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
    use crate::agent::testkit::ral_call;
    use crate::bus::{Inbox, Signal, channel};
    use crate::egress::Egress;
    use crate::fleet::registry::{AGENT_LEASE_IDLE, EvalReach, InterruptTarget, Registration};
    use crate::provider::{
        Provider,
        scripted::{Reply, Script},
    };
    use crate::record::{Display, FleetSink, Record, Transient};
    use crate::shell_eval::builtins::harness;
    use ral_core::transport::{DispatchId, IdentityTransport, Program, Run, Transport};
    use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};

    fn fresh_log() -> AgentLog {
        AgentLog::for_test(0, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    fn scripted_provider() -> Arc<Provider> {
        Arc::new(Provider::scripted("test-model", Script::new()))
    }

    /// The base capture every desk below builds on, so growing [`HostServices`]
    /// means touching one literal, not three.
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
            search: true,
            reply: ReplyCell::default(),
            schedules: ScheduleRegistry::new(),
            log: LogCell::new(fresh_log()),
            system_template: String::new(),
            index: crate::prompt::BuiltinIndex::resolve(&ral_core::Shell::new(
                ral_core::io::TerminalState::default(),
            )),
            interactive: false,
            nursery: Nursery::default(),
            generation: 0,
            disk_warn_bytes: None,
            egress: Egress::for_test(),
            acts: ActFragment::default(),
            principal: ral_core::host::user(),
            pins: None,
            wire_seat: false,
            hatchery: None,
            pending_hatches: crate::fleet::hatch::PendingHatches::new(),
        }
    }

    fn desk() -> ExarchDesk {
        ExarchDesk {
            services: base_services(),
        }
    }

    fn complete_exchange(log: &mut AgentLog, prompt: &str, answer: &str) {
        log.append_user(prompt.to_string(), None).expect("prompt");
        log.append_assistant(
            genai::chat::ChatMessage::assistant(answer),
            Vec::new(),
            None,
        )
        .expect("answer");
    }

    /// Every context-verb test request below goes through the same encoders
    /// [`harness::builtin_context_drop`] and its siblings call, so a change to
    /// an encoder breaks these tests instead of leaving them to hand-build a
    /// payload that has quietly drifted from what the builtin actually sends.
    fn exchanges_value(exchanges: &[i64]) -> RalValue {
        RalValue::list(
            exchanges
                .iter()
                .map(|value| RalValue::Int(*value))
                .collect(),
        )
    }

    fn context_drop_request(exchanges: &[i64]) -> FOValue {
        let payload =
            harness::context_exchanges_payload(&exchanges_value(exchanges), "context-drop")
                .expect("valid exchange list");
        FOValue::Variant {
            label: "context-drop".to_string(),
            payload: Some(Box::new(payload)),
        }
    }

    fn context_fold_request(through: i64, digest: &str) -> FOValue {
        let spec = RalValue::map(vec![
            ("through".to_string(), RalValue::Int(through)),
            ("digest".to_string(), RalValue::String(digest.to_string())),
        ]);
        let payload = harness::context_fold_payload(&spec).expect("valid fold spec");
        FOValue::Variant {
            label: "context-fold".to_string(),
            payload: Some(Box::new(payload)),
        }
    }

    /// The `` `context `` survey call, shaped exactly as `builtin_context`
    /// sends it: no payload to encode, since the answer is the whole survey.
    fn context_survey_request() -> FOValue {
        FOValue::Variant {
            label: "context".to_string(),
            payload: None,
        }
    }

    fn context_read_request(exchanges: &[i64]) -> FOValue {
        let payload =
            harness::context_exchanges_payload(&exchanges_value(exchanges), "context-read")
                .expect("valid exchange list");
        FOValue::Variant {
            label: "context-read".to_string(),
            payload: Some(Box::new(payload)),
        }
    }

    fn int_field(value: FOValue, key: &str) -> i64 {
        let FOValue::Map { entries } = value else {
            panic!("expected a record")
        };
        entries
            .into_iter()
            .find_map(|(name, value)| (name == key).then_some(value))
            .and_then(|value| match value {
                FOValue::Int { value } => Some(value),
                _ => None,
            })
            .unwrap_or_else(|| panic!("record has no Int field `{key}`"))
    }

    /// [`desk`] holding the self-wakeup grant, so a schedule test reaches past it.
    fn granted_desk() -> ExarchDesk {
        ExarchDesk {
            services: HostServices {
                allow_schedule: true,
                ..base_services()
            },
        }
    }

    /// A desk whose parent is itself registered — the precondition
    /// [`AgentRegistry::register`] enforces for any child — so
    /// `agent-start`/`agent-cancel`/`message` run end to end, unlike [`desk`].
    fn spawnable_desk(fuel: u32) -> (ExarchDesk, AgentRegistry, Inbox) {
        let parent_inbox = Inbox::new();
        let registry = AgentRegistry::new();
        // Minted, never a literal: children draw from the same process-wide
        // counter, so a hardcoded id collides with the first child this desk
        // spawns — overwriting the parent's entry, and hanging rather than
        // failing.
        let parent_id = crate::agent::fresh_id();
        let _ = registry.register(Registration {
            id: parent_id,
            parent: None,
            lease: None,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/parent"),
            cancel: crate::agent::cancel::Token::new(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: InterruptTarget::default(),
            },
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

    /// A scratch directory for one test, deleted when the guard falls.
    fn tmp(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("exarch-desk-full-{tag}-"))
            .tempdir()
            .expect("test scratch dir")
    }

    /// Booted exactly once per test. `boot_shell` resets process-global signal
    /// state, the ceremony the fleet runs once at its root; booting again per
    /// spawn races that reset against a sibling's in-flight run and deadlocks.
    fn root_shell() -> ral_core::Shell {
        crate::bootstrap::boot_shell()
    }

    /// The fork [`ral_core::Shell::fork_into_nursery`] would perform on a live
    /// session, off the shared [`root_shell`] rather than a freshly booted one.
    fn forkable_child_shell(root: &ral_core::Shell) -> ral_core::Shell {
        root.fork_session()
    }

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

    /// Names the expected shape rather than panicking or silently defaulting.
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

    #[test]
    fn context_survey_lists_digest_import_and_live_exchange() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            complete_exchange(&mut log, "closed\nexchange", "answer");
            complete_exchange(&mut log, "folded", "answer");
            log.apply_edit(
                ContextOp::Fold {
                    through_exchange: 1,
                    digest: "kept summary".into(),
                },
                EditAuthority::Model,
            )
            .expect("fold");
            log.import_context(vec![genai::chat::ChatMessage::user("inherited\ncontext")])
                .expect("import");
            log.append_user("live".into(), None).expect("live prompt");
        }
        let expected_bytes = desk.services.log.lock().history_bytes();

        let FOValue::Map { entries } = desk
            .handle(context_survey_request())
            .expect("context survey")
        else {
            panic!("context must answer a record")
        };
        let spans = entries
            .iter()
            .find_map(|(key, value)| (key == "spans").then_some(value))
            .expect("survey spans");
        let FOValue::List { items } = spans else {
            panic!("survey spans must be a list")
        };
        assert_eq!(items.len(), 4);
        let kinds = items
            .iter()
            .map(|item| {
                let FOValue::Map { entries } = item else {
                    panic!("survey item must be a record")
                };
                entries
                    .iter()
                    .find_map(|(key, value)| {
                        (key == "kind").then(|| match value {
                            FOValue::String { value } => value.as_str(),
                            _ => panic!("survey kind must be a string"),
                        })
                    })
                    .expect("survey kind")
            })
            .collect::<Vec<_>>();
        assert_eq!(kinds, vec!["digest", "exchange", "import", "exchange"]);
        let total_bytes = entries
            .iter()
            .find_map(|(key, value)| {
                (key == "total-bytes").then(|| match value {
                    FOValue::Int { value } => *value,
                    _ => panic!("survey total bytes must be an Int"),
                })
            })
            .expect("survey total bytes");
        assert_eq!(total_bytes, i64::try_from(expected_bytes).unwrap());
        let live = items.last().expect("live exchange");
        assert!(matches!(
            live,
            FOValue::Map { entries }
                if entries.iter().any(|(key, value)| key == "live" && matches!(value, FOValue::Bool { value: true }))
        ));
    }

    #[test]
    fn context_read_returns_one_transcript_without_committing_an_act() {
        let mut desk = desk();
        {
            let mut log = desk.services.log.lock();
            complete_exchange(&mut log, "first prompt", "first answer");
        }
        let (tx, rx) = channel();
        desk.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        desk.services.emit = Emitter::new(tx, 0);

        let FOValue::String { value } = desk
            .handle(context_read_request(&[1]))
            .expect("context-read")
        else {
            panic!("context-read must answer one Str")
        };
        assert!(value.contains("[user]\nfirst prompt"));
        assert!(value.contains("[assistant]\nfirst answer"));
        assert!(desk.services.acts.audit().is_none(), "a read has no act");
        let record = crate::bus::drain_records(&rx)
            .into_iter()
            .next()
            .expect("the read must reach the trace");
        assert!(matches!(
            record,
            Record::Display(Display::HarnessCall {
                verb,
                subject: Some(subject),
                payload,
                failed: false,
            }) if verb == "context-read" && subject == "exchanges [1]" && payload == "exchanges [1]"
        ));

        let error = desk
            .handle(context_read_request(&[]))
            .expect_err("an empty read is not meaningful");
        assert_eq!(
            error.message,
            "context-read must name at least one exchange"
        );
        let record = crate::bus::drain_records(&rx)
            .into_iter()
            .next()
            .expect("a refused read still reaches the trace");
        assert!(matches!(
            record,
            Record::Display(Display::HarnessCall {
                verb,
                failed: true,
                ..
            }) if verb == "context-read"
        ));
    }

    #[test]
    fn context_edit_refusals_surface_the_admissibility_sentence() {
        let live_desk = desk();
        {
            let mut log = live_desk.services.log.lock();
            log.append_user("live".into(), None).expect("live prompt");
        }
        let err = live_desk
            .handle(context_drop_request(&[1]))
            .expect_err("the live exchange is not editable");
        assert_eq!(
            err.message,
            "exchange 1 is the one you are in — a context edit may only name closed exchanges"
        );

        let unknown_desk = desk();
        {
            let mut log = unknown_desk.services.log.lock();
            complete_exchange(&mut log, "one", "answer");
        }
        let err = unknown_desk
            .handle(context_drop_request(&[7]))
            .expect_err("an unknown exchange is not editable");
        assert_eq!(err.message, "exchange 7 is not present in the current view");

        let folded_desk = desk();
        {
            let mut log = folded_desk.services.log.lock();
            complete_exchange(&mut log, "one", "answer");
            complete_exchange(&mut log, "two", "answer");
        }
        folded_desk
            .handle(context_fold_request(2, "kept summary"))
            .expect("fold");
        let err = folded_desk
            .handle(context_drop_request(&[1]))
            .expect_err("an exchange inside the digest is not addressable");
        assert_eq!(
            err.message,
            "exchange 1 is folded into the digest through 2 — name 2 to drop the digest whole, or fold further"
        );

        let err = folded_desk
            .handle(context_drop_request(&[]))
            .expect_err("an empty drop is not an edit");
        assert_eq!(
            err.message,
            "a context edit must name at least one exchange"
        );
    }

    #[test]
    fn context_drop_receipt_reports_the_view_byte_delta() {
        let desk = desk();
        let before = {
            let mut log = desk.services.log.lock();
            complete_exchange(&mut log, "one", "a longer answer");
            complete_exchange(&mut log, "two", "another answer");
            log.history_bytes()
        };
        let delta = int_field(
            desk.handle(context_drop_request(&[1]))
                .expect("context drop"),
            "bytes-delta",
        );
        let after = desk.services.log.lock().history_bytes();
        assert_eq!(
            delta,
            i64::try_from(before).unwrap() - i64::try_from(after).unwrap()
        );
        assert!(delta > 0, "dropping a visible exchange should shed bytes");
    }

    /// The fold counterpart to [`context_drop_receipt_reports_the_view_byte_delta`]:
    /// a fold receipts its byte delta and commits [`DeskAct::ContextFold`], not
    /// [`DeskAct::ContextDrop`] — the audit sentence names the act it actually
    /// took, which nothing else in this suite pins.
    #[test]
    fn context_fold_receipt_commits_the_fold_act() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            complete_exchange(&mut log, "one", "a longer answer");
            complete_exchange(&mut log, "two", "another answer");
        }
        let delta = int_field(
            desk.handle(context_fold_request(2, "kept summary"))
                .expect("context fold"),
            "bytes-delta",
        );
        assert_ne!(
            delta, 0,
            "folding two exchanges into a digest should change the byte count"
        );
        let audit = desk
            .services
            .acts
            .audit()
            .expect("a landed fold leaves an act");
        assert!(
            audit.contains("folded context"),
            "the audit sentence must name the fold, got: {audit}"
        );
    }

    /// `enquire` is called bare, off the dispatching thread: the adapter's drain
    /// is what is under test, not [`ral_core::Shell::enquire`]'s routing, which
    /// core's own suite covers.
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
                trail: None,
            },
        );

        let (tx, rx) = crate::bus::channel();
        let recorder = crate::record::Emitter::none();
        recorder.attach(crate::record::FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        let binding = DeskBinding {
            seam: Arc::new(HostSeam {
                desk: desk(),
                apply: SurfaceApplier {
                    pins: None,
                    id: 0,
                    recorder,
                    surface: Mutex::new(SurfaceBuffer::new()),
                },
            }),
            events: transport.events_shared(),
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

        // Proof `apply` ran, not just that `handle` did.
        let saw_unpin = crate::bus::drain_transients(&rx)
            .into_iter()
            .any(|t| matches!(t, Transient::Unpin { key } if key == "test-marker"));
        assert!(
            saw_unpin,
            "the drained surface frame must render onto the bus"
        );

        // Nothing pending survives, so no straggler is left for a later drain to
        // reorder ahead of.
        assert!(
            binding.events.try_recv().is_none(),
            "enquire must drain the transport's event queue fully"
        );
    }

    fn fresh_mirror() -> PinDigests {
        Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()))
    }

    /// A `` `pin [key, body] `` surface value carrying a one-span text card —
    /// the minimal shape [`SurfaceApplier::live`] folds into the mirror.
    fn pin_value(key: &str, text: &str) -> FOValue {
        FOValue::Variant {
            label: "pin".into(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![
                    ("key".into(), FOValue::String { value: key.into() }),
                    (
                        "body".into(),
                        FOValue::Variant {
                            label: "text".into(),
                            payload: Some(Box::new(FOValue::Map {
                                entries: vec![(
                                    "spans".into(),
                                    FOValue::List {
                                        items: vec![FOValue::Map {
                                            entries: vec![(
                                                "text".into(),
                                                FOValue::String { value: text.into() },
                                            )],
                                        }],
                                    },
                                )],
                            })),
                        },
                    ),
                ],
            })),
        }
    }

    fn unpin_value(key: &str) -> FOValue {
        FOValue::Variant {
            label: "unpin".into(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![("key".into(), FOValue::String { value: key.into() })],
            })),
        }
    }

    fn pin_read_req(key: &str) -> FOValue {
        FOValue::Variant {
            label: "pin-read".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: key.into() }],
            })),
        }
    }

    /// A pin applied through [`SurfaceApplier::live`] comes back from
    /// `pin-read` as the canonical card — the readback and the pinned mark
    /// agree on shape, which is the whole point of a readable register.
    #[test]
    fn pin_read_returns_the_canonical_card() {
        let mirror = fresh_mirror();
        SurfaceApplier {
            pins: Some(mirror.clone()),
            id: 0,
            recorder: crate::record::Emitter::none(),
            surface: Mutex::new(SurfaceBuffer::new()),
        }
        .live(pin_value("tasks", "hi"));

        let d = ExarchDesk {
            services: HostServices {
                pins: Some(mirror),
                ..base_services()
            },
        };
        let answer = d
            .handle(pin_read_req("tasks"))
            .expect("a hit must answer Ok");
        let card = crate::bus::card::value_to_card(&RalValue::from(answer))
            .expect("the readback must decode as a card");
        assert!(
            matches!(
                card.marks(),
                [crate::bus::card::Mark::Text { spans }]
                    if spans.len() == 1 && spans[0].role.is_none() && spans[0].text == "hi"
            ),
            "the canonical card must round-trip the pinned text mark, got {card:?}"
        );
    }

    /// A key never pinned, and a key unpinned after being pinned, both answer
    /// `unit` — a miss and a clear are the same absence to `pin-read`.
    #[test]
    fn pin_read_answers_unit_on_miss_and_after_unpin() {
        let mirror = fresh_mirror();
        let applier = SurfaceApplier {
            pins: Some(mirror.clone()),
            id: 0,
            recorder: crate::record::Emitter::none(),
            surface: Mutex::new(SurfaceBuffer::new()),
        };
        let d = ExarchDesk {
            services: HostServices {
                pins: Some(mirror),
                ..base_services()
            },
        };

        assert!(
            matches!(d.handle(pin_read_req("tasks")), Ok(FOValue::Unit)),
            "a key never pinned must answer unit"
        );

        applier.live(pin_value("tasks", "hi"));
        applier.live(unpin_value("tasks"));
        assert!(
            matches!(d.handle(pin_read_req("tasks")), Ok(FOValue::Unit)),
            "an unpinned key must answer unit"
        );
    }

    /// `pin-list` names exactly the occupied keys, in `BTreeMap` order, and
    /// tracks a set/clear pair.
    #[test]
    fn pin_list_tracks_set_and_clear() {
        let mirror = fresh_mirror();
        let applier = SurfaceApplier {
            pins: Some(mirror.clone()),
            id: 0,
            recorder: crate::record::Emitter::none(),
            surface: Mutex::new(SurfaceBuffer::new()),
        };
        let d = ExarchDesk {
            services: HostServices {
                pins: Some(mirror),
                ..base_services()
            },
        };
        let keys = |d: &ExarchDesk| match d
            .handle(FOValue::Variant {
                label: "pin-list".into(),
                payload: None,
            })
            .expect("pin-list must answer Ok")
        {
            FOValue::List { items } => items
                .into_iter()
                .map(|v| match v {
                    FOValue::String { value } => value,
                    other => panic!("pin-list must answer strings, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("pin-list must answer a list, got {other:?}"),
        };

        assert!(keys(&d).is_empty(), "an empty register lists no keys");

        applier.live(pin_value("b", "one"));
        applier.live(pin_value("a", "two"));
        assert_eq!(
            keys(&d),
            vec!["a", "b"],
            "keys list in BTreeMap (lexicographic) order"
        );

        applier.live(unpin_value("b"));
        assert_eq!(keys(&d), vec!["a"], "a clear drops its key from the list");
    }

    /// Every surface class the live applier renders also records through the
    /// seam — a `Display` record (or, for the pin register, `Forensic`) —
    /// and a generic card records *behind* the io group buffered before it,
    /// keeping the record
    /// in the order the user saw.  A pin/unpin additionally publishes a
    /// `Transient` beside its `Forensic` twin: the durable breadcrumb and the
    /// live register the process is holding, two records for one act.
    #[test]
    fn live_surfaces_record_their_seam_twins() {
        use crate::bus::Signal;
        use crate::record::{Display, Forensic, Record, Transient};

        let (tx, rx) = channel();
        let recorder = crate::record::Emitter::none();
        recorder.attach(crate::record::FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        let applier = SurfaceApplier {
            pins: None,
            id: 0,
            recorder,
            surface: Mutex::new(SurfaceBuffer::new()),
        };

        let read = ral_core::types::Observation::instant(
            ral_core::types::CallSite::default(),
            String::new(),
            ral_core::types::Observed::Read {
                path: "a.rs".into(),
            },
        );
        applier.live(crate::bus::card::observation_wire(&read));
        applier.live(FOValue::Variant {
            label: "card".into(),
            payload: Some(Box::new(FOValue::List { items: vec![] })),
        });
        applier.live(FOValue::Variant {
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
        applier.live(FOValue::Variant {
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
        applier.live(pin_value("tasks", "hi"));
        applier.live(unpin_value("tasks"));

        let mut facts: Vec<&'static str> = Vec::new();
        let mut transients: Vec<&'static str> = Vec::new();
        while let Ok(sig) = rx.try_recv() {
            match sig {
                Signal::Fact(_, fact) => facts.push(match fact.value() {
                    Record::Display(Display::Observation { .. }) => "io",
                    Record::Display(Display::Card { .. }) => "card",
                    Record::Display(Display::Done {
                        outcome: crate::record::DoneOutcome::Ok,
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
            "every class records its twin, the buffered read flushed ahead of the card"
        );
    }

    /// The protected-`services` guard blocks a model's `` `pin `` write,
    /// never a read: `` `pin-read `` answers a host-authored `"services"`
    /// slot exactly like any other key.
    #[test]
    fn pin_read_of_services_answers() {
        let mirror = fresh_mirror();
        mirror.lock().expect("pin register poisoned").insert(
            "services".to_string(),
            shell_eval::PinDigest::new(crate::bus::card::Card(vec![
                crate::bus::card::Mark::Text {
                    spans: vec![crate::bus::card::Span {
                        role: None,
                        text: "svc".into(),
                    }],
                },
            ])),
        );
        let d = ExarchDesk {
            services: HostServices {
                pins: Some(mirror),
                ..base_services()
            },
        };
        assert!(
            d.handle(pin_read_req("services")).is_ok(),
            "reads are not writes: the protected-pin guard must not reach `pin-read`"
        );
    }

    /// The receipt names the child, and its reply lands in the parent's inbox
    /// once it settles.
    #[test]
    fn agent_start_spawns_and_delivers_result_to_parent_inbox() {
        let (desk, _registry, parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
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
                        FOValue::Bool { value: true },
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

    /// A request above the parent's ceiling is narrowed, never refused — the
    /// shape [`crate::policy::narrow`] gives `grant`. Only that half is visible
    /// here: the clamped bit lands in a private `Agent` field, so
    /// `agent::build`'s fork test asserts the narrowing itself.
    #[test]
    fn agent_start_admits_a_search_request_above_the_parents_ceiling() {
        let (mut desk, _registry, parent_inbox) = spawnable_desk(3);
        desk.services.search = false;
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call("r1", "reply 'done'")])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
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
                        value: "searcher".into(),
                    },
                    FOValue::Variant {
                        label: "confined".into(),
                        payload: None,
                    },
                    FOValue::Bool { value: true },
                ],
            })),
        });
        assert!(
            answer.is_ok(),
            "a spawn asking for more search reach than its parent holds is narrowed, not refused"
        );
        let _ = wait_for_settle(&parent_inbox);
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
            crate::record::Record::Protocol(crate::record::Protocol::SessionStarted {
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
        let (mut desk, _registry, parent_inbox) = spawnable_desk(3);
        let template = format!(
            "persona\n\n# Builtins\n\n{}",
            crate::prompt::BUILTIN_INDEX_PLACEHOLDER
        );
        desk.services.system_template = template.clone();
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "reply 'hi from child'",
            )])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        // The production seam: the index table resolves from the same booted
        // surface the parked child shells fork from.
        desk.services.index = crate::prompt::BuiltinIndex::resolve(&root);
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
                        FOValue::Bool { value: true },
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
            .index
            .apply(
                &template,
                &crate::prompt::Grants {
                    returns: true,
                    allow_schedule: desk.services.allow_schedule,
                    spawns: desk.services.fuel.saturating_sub(1) > 0,
                },
            )
            .len();
        assert_eq!(
            recorded_system_prompt_bytes(std::path::Path::new(&log_dir)),
            expected,
            "the bookend must record the spawned child's own resolved \
             system, not the unresolved template HostServices captured"
        );

        // Drain the settle, so no background thread outlives this test.
        let _ = wait_for_settle(&parent_inbox);
    }

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
                        FOValue::Bool { value: true },
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

    /// [`AgentRegistry::name_live`] catches the ordinary case before the parked
    /// fork is ever adopted, so the refused spawn registers no child.
    #[test]
    fn agent_start_refuses_a_name_already_borne_by_a_live_agent() {
        let (desk, registry, _parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
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
                    FOValue::Bool { value: true },
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
                        FOValue::Bool { value: true },
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

    /// Siblings in one turn all succeed off the same captured fuel, since the
    /// parent's own is never debited.
    #[test]
    fn fuel_bounds_depth_not_fanout() {
        let (desk, _registry, parent_inbox) = spawnable_desk(1);
        let provider = Arc::new(Provider::scripted(
            "test-model",
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
                        FOValue::Bool { value: true },
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

    /// The capture went stale the instant the registry's shared generation moved
    /// past what was snapshotted at install.
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
                        FOValue::Bool { value: true },
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

    /// A refusal after [`Nursery::adopt`] — here, a bad grant label — drops the
    /// adopted `Shell` rather than leaving it to be adopted twice.
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
                        FOValue::Bool { value: true },
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

    /// Never a sibling, an ancestor, or itself: what [`AgentRegistry`]'s own
    /// tests check inside, checked here at the desk entry point.
    #[test]
    fn message_and_cancel_scope_to_descendants() {
        let (desk_root, registry, _root_inbox) = spawnable_desk(3);
        // Minted, never literals — see `spawnable_desk` on why. The trunk here
        // is the agent `spawnable_desk` named "parent".
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
                reach: EvalReach::Identity {
                    eval_root: Some(ral_core::process::DurableRoot::default()),
                    interrupt_target: InterruptTarget::default(),
                },
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

    /// `desk_binding_drains_pending_surfaces_before_handling`'s law again, but
    /// through a real `agent` spawn rather than a stub class.
    #[test]
    fn surface_then_spawn_observes_the_surface_first() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, rx) = channel();
        let emit = Emitter::new(tx, session.id);
        let _ = session.run_shell(
            "call-1".to_string(),
            r#"surface `unpin [key: "test-marker"]; agent [prompt: #'go'#, name: 't', type: `amnemon, grant: `confined, search: true]"#,
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

    /// Refused before the payload is even decoded, naming the flag that grants.
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
                        FOValue::String {
                            value: "nightly".into(),
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

    /// A schedule with no label is refused, naming the missing field, and
    /// registers nothing.
    #[test]
    fn schedule_without_a_label_is_refused() {
        let desk = granted_desk();
        let err = desk
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
            .expect_err("a schedule with no label must be refused");
        assert!(
            err.message.contains("label"),
            "must name the missing field, got: {}",
            err.message
        );
        assert!(
            desk.services.schedules.list().is_empty(),
            "the refused attempt registers nothing"
        );
    }

    /// Arm one, see it listed with the fields it was given, remove it by label,
    /// see the listing empty again.
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
                        FOValue::String {
                            value: "nightly".into(),
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
            "the receipt must carry the given label"
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

    /// Refused before a second schedule is registered.
    #[test]
    fn schedule_at_the_desk_refuses_a_duplicate_label() {
        let desk = granted_desk();
        let spec = |label: &str| FOValue::List {
            items: vec![
                FOValue::Variant {
                    label: "after".into(),
                    payload: Some(Box::new(FOValue::String { value: "1s".into() })),
                },
                FOValue::String {
                    value: label.into(),
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
        let spec = || FOValue::List {
            items: vec![
                FOValue::Variant {
                    label: "after".into(),
                    payload: Some(Box::new(FOValue::String { value: "1s".into() })),
                },
                FOValue::String {
                    value: "nightly".into(),
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
        desk.handle(FOValue::Variant {
            label: "schedule".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::Variant {
                        label: "after".into(),
                        payload: Some(Box::new(FOValue::String { value: "2h".into() })),
                    },
                    FOValue::String {
                        value: "nightly".into(),
                    },
                    FOValue::String {
                        value: "wake".into(),
                    },
                ],
            })),
        })
        .expect("a valid schedule must succeed");
        desk.handle(FOValue::Variant {
            label: "unschedule".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String {
                    value: "nightly".into(),
                }],
            })),
        })
        .expect("unscheduling the label just armed must remove it");
        desk.handle(FOValue::Variant {
            label: "reply".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String {
                    value: "done".into(),
                }],
            })),
        })
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
        desk.handle(FOValue::Variant {
            label: "message".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::String {
                        value: "nobody".into(),
                    },
                    FOValue::String { value: "hi".into() },
                ],
            })),
        })
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

        desk.handle(FOValue::Variant {
            label: "schedule".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::Variant {
                        label: "after".into(),
                        payload: Some(Box::new(FOValue::String { value: "1s".into() })),
                    },
                    FOValue::String {
                        value: "nightly".into(),
                    },
                    FOValue::String {
                        value: "wake".into(),
                    },
                ],
            })),
        })
        .expect("a valid schedule must land");
        desk.handle(FOValue::Variant {
            label: "message".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::String {
                        value: "nobody".into(),
                    },
                    FOValue::String { value: "hi".into() },
                ],
            })),
        })
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

    /// The registration half of `spawn_async`, but with a caller-chosen lease: a
    /// production spawn always arms the fixed [`AGENT_LEASE_IDLE`], which no
    /// test can wait out.
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
                reach: child.seat.eval_reach(),
                mailbox: child.mailbox(),
                provider: child.provider_handle(),
            })
            .expect("a fresh child of a live parent always registers")
    }

    /// Attend `child` to completion on a detached thread, in `spawn_async`'s own
    /// worker-epilogue order: deliver to `parent_mailbox`, then settle.
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

    /// A live, never-attended child: just enough for
    /// [`AgentRegistry::has_children`] to read true, so `parent_id` parks
    /// [`crate::bus::ParkMode::HeldByChildren`] instead of quiescing.
    fn register_keepalive(registry: &AgentRegistry, parent_id: AgentId, log_dir: &std::path::Path) {
        let _ = registry.register(Registration {
            id: crate::agent::fresh_id(),
            parent: Some(parent_id),
            lease: None,
            name: format!("keepalive-{parent_id}"),
            log_dir: log_dir.to_path_buf(),
            cancel: crate::agent::cancel::Token::new(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: InterruptTarget::default(),
            },
            mailbox: crate::bus::Inbox::new().mailbox(),
            provider: ProviderHandle::new(scripted_provider()),
        });
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

    /// The lifecycle turns on [`AgentRegistry::steer`]'s delivery alone and knows
    /// nothing of TUI focus. The keepalive grandchild stops the child quiescing
    /// before it is engaged, so both steers land instead of racing its loop.
    #[test]
    fn engaged_child_answers_a_second_steer_with_no_focus_involved() {
        let dir = tmp("engaged-second-steer");
        let parent = Agent::for_test("system").unwrap();
        let child = parent.fork(parent.caps().clone()).expect("fork child");
        child.provider_handle().swap(Arc::new(Provider::scripted(
            "test-model",
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
        register_keepalive(&registry, child_id, &dir.path().join("keepalive"));
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
                &log_dir.join("record.jsonl"),
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

    /// The reaped child delivers exactly one
    /// [`crate::bus::AgentOutcome::Cancelled`] to the parent inbox.
    #[test]
    fn ms_lease_child_never_renewed_is_cancelled_but_a_renewed_one_survives() {
        let dir = tmp("ms-lease-integration");
        let parent = Agent::for_test("system").unwrap();
        // Child A's ttl must expire well inside the round-trip loop's own
        // `MAX_STEPS` cap, so the lease and not the step cap ends its exchange.
        // Child B's is generous instead: its renewal is paced by `thread::sleep`
        // on the test thread, where jitter stretches a short sleep past nominal.
        let ttl_a = Duration::from_millis(25);
        let ttl_b = Duration::from_millis(300);

        let child_a = parent.fork(parent.caps().clone()).expect("fork child a");
        let mut long_script = Script::new();
        for i in 0..2_000u32 {
            long_script = long_script.then(Reply::tool_calls(vec![ral_call(&i.to_string(), "1")]));
        }
        child_a
            .provider_handle()
            .swap(Arc::new(Provider::scripted("test-model", long_script)));
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

        // Child B is parked by a keepalive grandchild and given no script to
        // race, so `renew` alone must be what defers its reap.
        let child_b = parent.fork(parent.caps().clone()).expect("fork child b");
        let id_b = child_b.id;
        let gen_b = register_lease_child(parent.id, Some(ttl_b), "child-b", &child_b);
        register_keepalive(&registry, id_b, &dir.path().join("keepalive-b"));
        let handle_b = attend_and_deliver(child_b, "child-b", gen_b, parent.mailbox());

        std::thread::sleep(ttl_b / 2);
        assert!(registry.renew(id_b), "a live entry renews");

        std::thread::sleep(ttl_b / 2 + Duration::from_millis(150));
        assert!(
            registry.is_live(id_b),
            "renewed at half the ttl, still alive past the original bound"
        );

        // Wind child B down rather than leave its thread parked forever: per
        // `ParkMode`, a terminate-cause cancel ends an unengaged park at once.
        registry.cancel(id_b);
        let _ = wait_for_settle(&parent.inbox());
        handle_b.join().expect("worker thread must not panic");
    }
}

/// The desk's wire arm: `agent-start`'s hatch dance, over a fake hatchery and
/// a re-exec'd `--engine` child standing in for a hatched guest — the same
/// vehicle the wire seat's own tests already use, since a genuine vsock dial
/// only means anything inside a real guest.
#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod wire_tests {
    use super::*;
    use crate::agent::Hatchery;
    use crate::agent::event::AgentLog;
    use crate::agent::testkit::ral_call;
    use crate::bus::Inbox;
    use crate::egress::Egress;
    use crate::fleet::hatch::PendingHatches;
    use crate::fleet::registry::{EvalReach, InterruptTarget, Registration};
    use crate::provider::{
        Provider,
        scripted::{Reply, Script},
    };
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// Each log takes the next session id, so two of them are never the same
    /// session to the wire.
    fn fresh_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        AgentLog::for_test(n, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    fn scripted_provider(script: Script) -> Arc<Provider> {
        Arc::new(Provider::scripted("test-model", script))
    }

    /// Poll `inbox` for the next exchange-boundary item — a spawned child's
    /// settled result lands here.
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

    /// A fake [`Hatchery`] whose `await_dial` answers whatever the test
    /// queued through [`Self::queue`], or refuses with a fixed message —
    /// standing in for both a landed dial and a `ral-daemon`-style timeout.
    struct FakeHatchery {
        port: u32,
        dial: StdMutex<Option<Result<UnixStream, String>>>,
    }

    impl FakeHatchery {
        fn new(port: u32) -> Self {
            Self {
                port,
                dial: StdMutex::new(None),
            }
        }

        fn queue(&self, dial: Result<UnixStream, String>) {
            *self.dial.lock().unwrap() = Some(dial);
        }
    }

    impl crate::agent::Hatchery for FakeHatchery {
        fn await_dial(
            &self,
            _token: u64,
            _patience: Duration,
        ) -> Result<ral_core::wire::WireStream, String> {
            self.dial
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err("no dial queued".to_string()))
        }

        fn port(&self) -> u32 {
            self.port
        }
    }

    /// Spawn a bare `--engine` child over a fresh socketpair, exactly
    /// [`crate::agent::seat::tests::spawn_engine`]'s vehicle — but write a
    /// hatch preamble bearing `token` onto the guest's own end first, the way
    /// [`ral_core::hatch::hatch_over`] writes it on the dial *before* handing
    /// the same fd to the spawned child. The host end, `preamble` bytes
    /// included, is what a real hatchery hands the desk to adopt.
    fn hatched_engine(token: u64) -> (UnixStream, std::process::Child) {
        let (host, guest) = UnixStream::pair().expect("socketpair standing in for the vsock dial");
        {
            let mut preamble = [0u8; 16];
            preamble[..8].copy_from_slice(b"ralagent");
            preamble[8..].copy_from_slice(&token.to_le_bytes());
            let mut writer = guest.try_clone().expect("clone guest end");
            writer.write_all(&preamble).expect("write preamble");
        }
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
        let child = cmd.spawn().expect("spawn engine child");
        drop(guest);
        (host, child)
    }

    /// A [`EvalReach::Wire`] with a genuine `ControlSender` behind it — a
    /// disposable `--engine` child adopted and killed at once, since these
    /// fixtures need a real reach value's *shape* but never actually cancel
    /// or interrupt through it.
    fn fake_wire_reach() -> EvalReach {
        let (host, mut child) = hatched_engine(0);
        let transport = ral_core::transport::WireTransport::adopt(
            host,
            ral_core::transport::Liveness::default(),
        )
        .expect("adopt host stream");
        let control = ral_core::transport::Transport::control(&transport).clone();
        let _ = child.kill();
        let _ = child.wait();
        EvalReach::Wire(control)
    }

    /// A wire-seat desk fixture, its parent registered so `agent-start`'s
    /// spawn spine runs end to end, exactly [`super::tests::spawnable_desk`]'s
    /// identity shape with `wire_seat: true` and a hatchery installed.
    fn wire_spawnable_desk(
        fuel: u32,
        hatchery: Arc<FakeHatchery>,
    ) -> (ExarchDesk, AgentRegistry, Inbox) {
        let parent_inbox = Inbox::new();
        let registry = AgentRegistry::new();
        let parent_id = crate::agent::fresh_id();
        let _ = registry.register(Registration {
            id: parent_id,
            parent: None,
            lease: None,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/wire-parent"),
            cancel: crate::agent::cancel::Token::new(),
            reach: fake_wire_reach(),
            mailbox: parent_inbox.mailbox(),
            provider: ProviderHandle::new(scripted_provider(Script::new())),
        });
        let (emit, _rx) = crate::bus::dummy_emitter();
        let desk = ExarchDesk {
            services: HostServices {
                registry: registry.clone(),
                scratch: None,
                parent: parent_id,
                mailbox: parent_inbox.mailbox(),
                emit,
                provider: ProviderHandle::new(scripted_provider(Script::new())),
                caps: Capabilities::root(),
                cwd: PathBuf::from("/work"),
                fuel,
                returns: true,
                allow_schedule: false,
                search: true,
                reply: ReplyCell::default(),
                schedules: ScheduleRegistry::new(),
                log: LogCell::new(fresh_log()),
                system_template: String::new(),
                index: crate::prompt::BuiltinIndex::resolve(&ral_core::Shell::new(
                    ral_core::io::TerminalState::default(),
                )),
                interactive: false,
                nursery: Nursery::default(),
                generation: registry.generation(),
                disk_warn_bytes: None,
                egress: Egress::for_test(),
                acts: ActFragment::default(),
                principal: ral_core::host::user(),
                pins: None,
                wire_seat: true,
                hatchery: Some(hatchery),
                pending_hatches: PendingHatches::new(),
            },
        };
        (desk, registry, parent_inbox)
    }

    fn agent_start_req(name: &str) -> FOValue {
        FOValue::Variant {
            label: "agent-start".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::Int { value: 0 },
                    FOValue::Variant {
                        label: "amnemon".to_string(),
                        payload: None,
                    },
                    FOValue::String {
                        value: "go".to_string(),
                    },
                    FOValue::String {
                        value: name.to_string(),
                    },
                    FOValue::Variant {
                        label: "confined".to_string(),
                        payload: None,
                    },
                    FOValue::Bool { value: false },
                ],
            })),
        }
    }

    fn agent_hatched_req(token: u64) -> FOValue {
        FOValue::Variant {
            label: "agent-hatched".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::Int {
                    value: token.cast_signed(),
                }],
            })),
        }
    }

    /// Unwrap `agent-start`'s `` `hatch [token, port] `` answer.
    fn hatch_token(answer: FOValue) -> u64 {
        let FOValue::Variant {
            label,
            payload: Some(payload),
        } = answer
        else {
            panic!("expected a `hatch variant")
        };
        assert_eq!(label, "hatch");
        let FOValue::Map { entries } = *payload else {
            panic!("expected a hatch record payload")
        };
        entries
            .into_iter()
            .find_map(|(k, v)| match (k.as_str(), v) {
                ("token", FOValue::Int { value }) => Some(value.cast_unsigned()),
                _ => None,
            })
            .expect("hatch answer must carry a token")
    }

    /// The whole spine: `agent-start` mints a hatch, a re-exec'd `--engine`
    /// child stands in for the guest's own hatch, `agent-hatched` adopts its
    /// dial, and the model-driven helper's `reply` lands in the parent's
    /// inbox exactly as an identity spawn's would.
    #[test]
    fn wire_hatch_spawns_and_delivers_result_to_parent_inbox() {
        let port = 1731;
        let hatchery = Arc::new(FakeHatchery::new(port));
        let (desk, _registry, parent_inbox) = wire_spawnable_desk(3, hatchery.clone());

        let started = desk
            .handle(agent_start_req("helper"))
            .expect("agent-start must answer");
        let token = hatch_token(started);
        assert_eq!(
            hatchery.port(),
            port,
            "the hatch answer's port must be the hatchery's own"
        );

        let (host, mut child) = hatched_engine(token);
        hatchery.queue(Ok(host));

        let provider = scripted_provider(Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "reply 'hi from wire'",
        )])));
        // The assembled child seeds its own provider from the desk's
        // captured handle, so swap it before `agent-hatched` assembles.
        desk.services.provider.swap(provider);

        let receipt = desk
            .handle(agent_hatched_req(token))
            .expect("agent-hatched must adopt the dial and spawn");
        let FOValue::Variant {
            label,
            payload: Some(payload),
        } = receipt
        else {
            panic!("expected a `started` variant")
        };
        assert_eq!(label, "started");
        let FOValue::Map { entries } = *payload else {
            panic!("expected a record payload")
        };
        assert!(
            entries
                .iter()
                .any(|(k, v)| k == "name"
                    && matches!(v, FOValue::String { value } if value == "helper")),
            "the receipt must carry the child's name"
        );

        match wait_for_settle(&parent_inbox) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    result.text.contains("hi from wire"),
                    "the hatched child's reply must reach the parent's inbox, got: {}",
                    result.text
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }

        let _ = child.kill();
        let _ = child.wait();
    }

    /// A dial whose preamble carries the wrong token is refused, never
    /// silently adopted: the token is the hatch's real defense, not the
    /// magic alone.
    #[test]
    fn agent_hatched_refuses_a_dial_bearing_the_wrong_token() {
        let hatchery = Arc::new(FakeHatchery::new(1731));
        let (desk, _registry, _parent_inbox) = wire_spawnable_desk(3, hatchery.clone());

        let started = desk
            .handle(agent_start_req("mismatched"))
            .expect("agent-start must answer");
        let token = hatch_token(started);

        // A dial genuinely hatched, but under a different token than the one
        // this hatch was minted for — a stray or a confused sibling.
        let (host, mut child) = hatched_engine(token.wrapping_add(1));
        hatchery.queue(Ok(host));

        let err = desk
            .handle(agent_hatched_req(token))
            .expect_err("a token mismatch must be refused");
        assert!(
            err.message.contains("did not match"),
            "got: {}",
            err.message
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// A dial that never arrives — the hatchery's own timeout — answers a
    /// refusal naming the wait, the failure-honesty path `agent-hatched`
    /// owes a builtin that must then kill and reap its own hatched child.
    #[test]
    fn agent_hatched_refuses_naming_the_wait_on_a_hatchery_timeout() {
        let hatchery = Arc::new(FakeHatchery::new(1731));
        let (desk, _registry, _parent_inbox) = wire_spawnable_desk(3, hatchery);

        let started = desk
            .handle(agent_start_req("never-dials"))
            .expect("agent-start must answer");
        let token = hatch_token(started);
        // No dial queued: `FakeHatchery::await_dial` answers its own refusal.

        let err = desk
            .handle(agent_hatched_req(token))
            .expect_err("a hatchery timeout must be refused");
        assert!(
            err.message.contains("never dialled home"),
            "must name the wait, got: {}",
            err.message
        );
    }

    /// `agent-abort` drops the pending hatch and frees the name it reserved,
    /// so a retry under the same name succeeds.
    #[test]
    fn agent_abort_frees_the_reserved_name() {
        let hatchery = Arc::new(FakeHatchery::new(1731));
        let (desk, _registry, _parent_inbox) = wire_spawnable_desk(3, hatchery);

        let started = desk
            .handle(agent_start_req("aborted"))
            .expect("agent-start must answer");
        let token = hatch_token(started);

        let retry = desk.handle(agent_start_req("aborted"));
        assert!(
            retry.is_err(),
            "the name is reserved while the hatch is pending"
        );

        desk.handle(FOValue::Variant {
            label: "agent-abort".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::Int {
                    value: token.cast_signed(),
                }],
            })),
        })
        .expect("agent-abort always answers unit");

        assert!(
            desk.handle(agent_start_req("aborted")).is_ok(),
            "aborting the pending hatch must free its reserved name"
        );
    }

    /// The peer-messaging pin: one identity-reach and one wire-reach entry in
    /// the same registry exchange marked notes through the desk's `message`
    /// handler, which touches only the registry and a mailbox — never a
    /// seat — so sender and recipient never learn each other's transport.
    #[test]
    fn identity_and_wire_peers_exchange_messages_through_one_desk() {
        let registry = AgentRegistry::new();
        let parent_id = crate::agent::fresh_id();
        let _ = registry.register(Registration {
            id: parent_id,
            parent: None,
            lease: None,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/peer-parent"),
            cancel: crate::agent::cancel::Token::new(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: InterruptTarget::default(),
            },
            mailbox: Inbox::new().mailbox(),
            provider: ProviderHandle::new(scripted_provider(Script::new())),
        });

        let identity_inbox = Inbox::new();
        let identity_id = crate::agent::fresh_id();
        let _ = registry.register(Registration {
            id: identity_id,
            parent: Some(parent_id),
            lease: None,
            name: "identity-peer".into(),
            log_dir: PathBuf::from("/tmp/peer-identity"),
            cancel: crate::agent::cancel::Token::new(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: InterruptTarget::default(),
            },
            mailbox: identity_inbox.mailbox(),
            provider: ProviderHandle::new(scripted_provider(Script::new())),
        });

        let wire_inbox = Inbox::new();
        let wire_id = crate::agent::fresh_id();
        let _ = registry.register(Registration {
            id: wire_id,
            parent: Some(parent_id),
            lease: None,
            name: "wire-peer".into(),
            log_dir: PathBuf::from("/tmp/peer-wire"),
            cancel: crate::agent::cancel::Token::new(),
            reach: fake_wire_reach(),
            mailbox: wire_inbox.mailbox(),
            provider: ProviderHandle::new(scripted_provider(Script::new())),
        });

        let (emit, _rx) = crate::bus::dummy_emitter();
        let desk = ExarchDesk {
            services: HostServices {
                registry: registry.clone(),
                scratch: None,
                parent: parent_id,
                mailbox: Inbox::new().mailbox(),
                emit,
                provider: ProviderHandle::new(scripted_provider(Script::new())),
                caps: Capabilities::root(),
                cwd: PathBuf::from("/"),
                fuel: 3,
                returns: true,
                allow_schedule: false,
                search: true,
                reply: ReplyCell::default(),
                schedules: ScheduleRegistry::new(),
                log: LogCell::new(fresh_log()),
                system_template: String::new(),
                index: crate::prompt::BuiltinIndex::resolve(&ral_core::Shell::new(
                    ral_core::io::TerminalState::default(),
                )),
                interactive: false,
                nursery: Nursery::default(),
                generation: registry.generation(),
                disk_warn_bytes: None,
                egress: Egress::for_test(),
                acts: ActFragment::default(),
                principal: ral_core::host::user(),
                pins: None,
                wire_seat: false,
                hatchery: None,
                pending_hatches: PendingHatches::new(),
            },
        };

        let message_req = |name: &str, text: &str| FOValue::Variant {
            label: "message".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::String {
                        value: name.to_string(),
                    },
                    FOValue::String {
                        value: text.to_string(),
                    },
                ],
            })),
        };

        desk.handle(message_req("identity-peer", "note for identity"))
            .expect("the parent may message its identity-reach descendant");
        desk.handle(message_req("wire-peer", "note for wire"))
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
