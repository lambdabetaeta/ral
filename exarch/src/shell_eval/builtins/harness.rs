//! The harness builtins — `exarch-agents`, `exarch-schedules`, `exarch-pins`,
//! `exarch-context`, `exarch-transcript` — with the type schemes that gate
//! them. A returning agent's reply is a tag of `exarch-agents`
//! (`` `reply ``), not a builtin of its own — the fleet is one family.
//!
//! Each body validates at the door before it enquires, so a malformed call
//! never reaches the host. `exarch-agents`'s `` `start `` tag forks this
//! shell and tells the host how to reach the fork, which is what the run's
//! [`Fork`](ral_core::types::Fork) door says: an in-process host adopts a
//! fork parked in the run's nursery, since the reentrancy law bars a desk
//! handler from holding `&mut Shell` to fork one itself; a host across a wire
//! is handed a guest port to dial, and dials it while it answers.
//! [`crate::fleet::desk::ExarchDesk`] answers every enquiry on the other side.
//!
//! One verb per addressable thing, named as the model names it:
//! `exarch-agents`, `exarch-schedules`, `exarch-pins`, `exarch-context` and
//! `exarch-transcript` each carry the model's tag as a nested variant and its
//! record verbatim, and the tag selects what happens. All but
//! `exarch-transcript` name a state, and answer it afterwards.
//! `exarch-transcript` names the record instead, which no tag of it writes,
//! so its tags answer what they were asked for rather than a state.

use crate::fleet::desk::Selection;
use crate::fleet::schedule::{CronSchedule, parse_duration};
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{closed_record, fun, mk_scheme as scheme, pure, thunk};
use ral_core::typecheck::{Field, Label, Row, RowVar, Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Fork, Mooring, Settled, sig};
use ral_core::{Shell, SpawnGrant, Value};
use std::borrow::Cow;

/// The label of a nullary tag — the shape both `type` and `grant` must have.
fn bare_tag(v: &Value) -> Option<&str> {
    match v {
        Value::Variant {
            label,
            payload: None,
        } => Some(label),
        _ => None,
    }
}

/// Check a spawn's `type`. [`scheme_agents`] leaves that row open, so the
/// enumeration is closed here, where the error can name both memory modes.
fn agent_type_label(v: &Value) -> Settled<()> {
    if matches!(bare_tag(v), Some("amnemon" | "mnemon")) {
        return Ok(());
    }
    Err(sig(format!(
        "exarch-agents: `type` must be `amnemon (blank context) or `mnemon (inherits your conversation) — got {v}"
    )))
}

/// Read a `grant`, closing the row [`scheme_agents`] leaves open so the error
/// can enumerate every legal shape. `` `restrict ``'s record is carried across
/// as plain data rather than decoded here: the capability vocabulary is the
/// far side's, and every form is one more layer pushed onto the parent's
/// stack, so no form of this can widen the child.
fn spawn_grant(v: &Value) -> Settled<SpawnGrant> {
    match v {
        Value::Variant {
            label,
            payload: None,
        } if label == "inherit" => Ok(SpawnGrant::Inherit),
        Value::Variant {
            label,
            payload: None,
        } if crate::policy::SPAWN_BASES.contains(&label.as_str()) => {
            Ok(SpawnGrant::Base(label.clone()))
        }
        Value::Variant {
            label,
            payload: Some(record),
        } if label == "restrict" => FOValue::try_from(record.as_ref())
            .map(SpawnGrant::Restrict)
            .map_err(|_| {
                sig(
                    "exarch-agents: `grant`'s `restrict record must be first-order data — no \
                     closures, handles, or environments — since the ceiling crosses to the host \
                     as plain data",
                )
            }),
        other => Err(sig(format!(
            "exarch-agents: `grant` must be `inherit, `confined, `read-only, `edit-only, \
             `reasonable, or `restrict [exec: …, fs: …, net: …, detach: …, editor: …, shell: …] — \
             `inherit is how you decline to narrow at all, and `restrict must carry a capability \
             record of the same shape `grant [...] takes, every key optional — got {other}"
        ))),
    }
}

/// Check a spawn's `provider` or `model`, closing the row [`scheme_agents`]
/// leaves open so the error can name both arms. ral has no optional field, so
/// the two are always written, and `` `inherit `` is how a spawn says it has
/// no opinion.
fn selection_label(v: &Value, field: &str) -> Settled<Selection> {
    match v {
        Value::Variant {
            label,
            payload: None,
        } if label == "inherit" => Ok(Selection::Inherit),
        Value::Variant {
            label,
            payload: Some(payload),
        } if label == "named" => match payload.as_ref() {
            Value::String(name) if !name.is_empty() => Ok(Selection::Named(name.clone())),
            other => Err(sig(format!(
                "exarch-agents: `{field}`'s `named` must carry a non-empty Str naming the \
                 {field} — got {other}"
            ))),
        },
        other => Err(sig(format!(
            "exarch-agents: `{field}` must be `inherit (whatever you are running on) or \
             `named '<{field}>' — got {other}"
        ))),
    }
}

/// Check a `schedule` spec's `trigger`, re-running the real parsers
/// ([`CronSchedule::parse`]/[`parse_duration`]) engine-side so a malformed
/// expression carries their own message home before any enquiry crosses.
/// The desk parses again on arrival: a guest may send whatever it likes.
fn schedule_trigger(v: &Value) -> Settled<()> {
    let Value::Variant {
        label,
        payload: Some(payload),
    } = v
    else {
        return Err(sig(format!(
            "exarch-schedules: trigger must be `cron '<5-field-cron-expr>'` or `after '<n><unit>'`, got {v}"
        )));
    };
    let Value::String(expr) = payload.as_ref() else {
        return Err(sig(format!(
            "exarch-schedules: `{label}`'s payload must be a Str, got {}",
            payload.type_name()
        )));
    };
    match label.as_str() {
        "cron" => CronSchedule::parse(expr)
            .map(|_| ())
            .map_err(|e| sig(format!("exarch-schedules: {e}"))),
        "after" => parse_duration(expr)
            .map(|_| ())
            .map_err(|e| sig(format!("exarch-schedules: {e}"))),
        other => Err(sig(format!(
            "exarch-schedules: trigger must be `cron '<5-field-cron-expr>'` or `after '<n><unit>'`, got `{other}`"
        ))),
    }
}

/// Check a `schedule` spec's `label`: the wakeup's name, required — every
/// schedule now names itself, so there is no default to fall back to.
fn schedule_label(v: &Value) -> Settled<()> {
    if matches!(v, Value::String(_)) {
        return Ok(());
    }
    Err(sig(format!(
        "exarch-schedules: `label` must be a Str naming the wakeup, got {}",
        v.type_name()
    )))
}

/// A request in the nested form the desk matches: the registry's family, then
/// the tag the model typed, then that tag's own payload.
fn request(family: &str, tag: &str, payload: Option<FOValue>) -> FOValue {
    FOValue::Variant {
        label: family.to_string(),
        payload: Some(Box::new(FOValue::Variant {
            label: tag.to_string(),
            payload: payload.map(Box::new),
        })),
    }
}

/// The model's own record, sent verbatim. Its recursion is the seam's
/// first-orderness check, so the door need not re-encode field by field.
fn verbatim(spec: &Value, verb: &str) -> Settled<FOValue> {
    FOValue::try_from(spec).map_err(|_| {
        sig(format!(
            "{verb}: the spec must be first-order data — no closures, handles, or environments — \
             since it crosses to the host as plain data"
        ))
    })
}

/// The `exarch-agents` family's answer: `` `roster [rows] `` from `` `list ``, and
/// `` `summary [live, replied] `` from every other tag. `` `read `` answers
/// the fetched value instead, so [`builtin_agents`] never routes it here.
/// This door is what tells the shapes apart, the family's answer type being a
/// free `α` ([`scheme_agents`]).
fn fleet_answer(answer: FOValue) -> Settled<Value> {
    let FOValue::Variant {
        label,
        payload: Some(payload),
    } = answer
    else {
        return Err(sig(
            "exarch-agents: host answered an unexpected shape for the fleet",
        ));
    };
    match (label.as_str(), *payload) {
        ("roster", FOValue::List { items }) => {
            Ok(Value::list(items.into_iter().map(Value::from).collect()))
        }
        ("summary", record @ FOValue::Map { .. }) => Ok(Value::from(record)),
        ("roster", _) => Err(sig(
            "exarch-agents: host's `roster answer must carry a list of agent rows",
        )),
        ("summary", _) => Err(sig(
            "exarch-agents: host's `summary answer must carry [live: Int, replied: Int]",
        )),
        _ => Err(sig(format!(
            "exarch-agents: host answered `{label} where `roster or `summary was expected — `list \
             answers the rows, every other tag the two counts"
        ))),
    }
}

/// `` `start ``'s payload: the model's record verbatim, and how the fork it
/// asks for reaches the desk.
fn start_request(spec: FOValue, fork: FOValue) -> FOValue {
    request(
        "agents",
        "start",
        Some(FOValue::Map {
            entries: vec![("spec".to_string(), spec), ("fork".to_string(), fork)],
        }),
    )
}

/// `` `start ``'s `fork` tag for an in-process host: the nursery slot the
/// fork is parked in, for the handler to adopt by id.
fn parked(session: i64) -> FOValue {
    FOValue::Variant {
        label: "parked".to_string(),
        payload: Some(Box::new(FOValue::Int { value: session })),
    }
}

/// `` `start ``'s `fork` tag for a host across a wire: where this engine is
/// listening, and the eight bytes the host must write when it dials.
#[cfg(target_os = "linux")]
fn listening(port: u32, token: u64) -> FOValue {
    FOValue::Variant {
        label: "listening".to_string(),
        payload: Some(Box::new(FOValue::Map {
            entries: vec![
                (
                    "port".to_string(),
                    FOValue::Int {
                        value: i64::from(port),
                    },
                ),
                // Bit-preserving: the token rides as whatever i64 bits were
                // minted, never arithmetic on it.
                (
                    "token".to_string(),
                    FOValue::Int {
                        value: token.cast_signed(),
                    },
                ),
            ],
        })),
    }
}

/// Eight bytes the host must write before this engine hatches onto the
/// connection it dialled. The guest kernel's refusal to route a guest-local
/// dial is the standing defence; this is the second line, against a jailed
/// command that guesses a CID rather than reading one.
#[cfg(target_os = "linux")]
fn mint_token() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS randomness");
    u64::from_le_bytes(bytes)
}

/// The wire arm of `` `start ``: bind a guest port for the duration of this
/// one spawn, name it in the enquiry, and let the host dial while it answers.
///
/// The answer arrives only once the child exists, because the listener thread
/// acknowledges the dial after `spawn()` succeeds. So there is one enquiry and
/// one rule for its outcome: raise the thread's reason if it has one — it was
/// nearer the failure — otherwise the host's.
#[cfg(target_os = "linux")]
fn hatch_over_the_wire(
    spec: FOValue,
    grant: SpawnGrant,
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<FOValue> {
    let token = mint_token();
    let (socket, port) =
        super::guest_port::bind().map_err(|why| sig(format!("exarch-agents: {why}")))?;
    let listener = ral_core::hatch::listen_for_hatch(socket, token, &shell.fork_scrubbed(), grant)
        .map_err(|reason| sig(format!("exarch-agents: {reason}")))?;
    let answer = shell.enquire(mooring, start_request(spec, listening(port, token)));
    // A host that refused never dialled, so the thread is still in its poll:
    // wake it, or the join below never returns.
    if answer.is_err() {
        listener.cancel();
    }
    match listener.join() {
        Err(ral_core::hatch::Unhatched::Failed(reason)) => {
            Err(sig(format!("exarch-agents: {reason}")))
        }
        _ => Ok(answer?),
    }
}

/// The dial this arm waits for means nothing outside a Linux guest, so a wire
/// trunk built on any other platform refuses here rather than at a silent
/// no-op.
#[cfg(not(target_os = "linux"))]
fn hatch_over_the_wire(
    _spec: FOValue,
    _grant: SpawnGrant,
    _mooring: &Mooring,
    _shell: &Shell,
) -> Settled<FOValue> {
    Err(sig(
        "exarch-agents: this engine has no hatch support outside a Linux guest — a wire trunk's helper \
         spawn only ever reaches one",
    ))
}

/// `` `start ``'s payload: validate, fork this shell, and enquire
/// `` exarch-agents `start `` with the model's record and the fork tag this run's
/// [`Fork`] door calls for; the desk's `launch` is the other half.
///
/// [`scheme_agents`]'s closed record row inside `` `start `` already
/// guarantees the seven fields, so the `else` arms below are unreachable
/// through the type checker; they stay didactic rather than trust it alone.
fn start_agent(spec: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::Map(fields) = spec else {
        return Err(sig(format!(
            "exarch-agents: `start`'s payload must be a [prompt: …, name: …, type: …, grant: …, search: …, provider: …, model: …] record, got {}",
            spec.type_name()
        )));
    };
    if fields.get("prompt").is_none() {
        return Err(sig(
            "exarch-agents: the spec record needs a `prompt` field — the instruction the child starts with",
        ));
    }
    let Some(name) = fields.get("name") else {
        return Err(sig(
            "exarch-agents: the spec record needs a `name` field — the child's identity",
        ));
    };
    let Some(kind) = fields.get("type") else {
        return Err(sig(
            "exarch-agents: the spec record needs a `type` field — `amnemon or `mnemon",
        ));
    };
    let Some(grant) = fields.get("grant") else {
        return Err(sig(
            "exarch-agents: the spec record needs a `grant` field — the child's ceiling: `inherit, \
             `confined, `read-only, `edit-only, `reasonable, or `restrict [...]",
        ));
    };
    let Some(search) = fields.get("search") else {
        return Err(sig(
            "exarch-agents: the spec record needs a `search` field — whether the child may use the \
             provider's built-in web search",
        ));
    };
    let Some(provider) = fields.get("provider") else {
        return Err(sig(
            "exarch-agents: the spec record needs a `provider` field — `inherit to run the child on \
             your own account, or `named '<provider>'",
        ));
    };
    let Some(model) = fields.get("model") else {
        return Err(sig(
            "exarch-agents: the spec record needs a `model` field — `inherit to run the child on your \
             own model, or `named '<model>'",
        ));
    };

    let name = name.to_string();
    // The door's own early refusal; `Fleet::enrol` is what makes it
    // unskippable.
    crate::fleet::check_name(&name).map_err(|why| sig(format!("exarch-agents: {why}")))?;
    agent_type_label(kind)?;
    let grant = spawn_grant(grant)?;
    if !matches!(search, Value::Bool(_)) {
        return Err(sig(format!(
            "exarch-agents: `search` must be a Bool — got {}",
            search.type_name()
        )));
    }
    selection_label(provider, "provider")?;
    selection_label(model, "model")?;
    let spec = verbatim(spec, "exarch-agents")?;

    match mooring.fork() {
        Some(Fork::Listen) => hatch_over_the_wire(spec, grant, mooring, shell),
        // `fork_into_nursery` owns the sentence for both remaining doors: the
        // park itself, and the honest absence when a host adopts no fork.
        Some(Fork::Park(_)) | None => {
            let session = shell.fork_into_nursery(mooring)?;
            // `Nursery::park` mints ids from a monotonic per-run counter, so
            // this never saturates; `unwrap_or` keeps the door total without
            // an `as` cast's silent wraparound.
            let session = i64::try_from(session.0).unwrap_or(i64::MAX);
            Ok(shell.enquire(mooring, start_request(spec, parked(session)))?)
        }
    }
}

/// `` `message ``'s payload: enquires `` exarch-agents `message `` with the model's
/// record; name resolution and delivery errors all belong to the desk.
fn message_agent(spec: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::Map(fields) = spec else {
        return Err(sig(format!(
            "exarch-agents: `message`'s payload must be a [to: …, text: …] record, got {}",
            spec.type_name()
        )));
    };
    let Some(to) = fields.get("to") else {
        return Err(sig(
            "exarch-agents: the `message` spec needs a `to` field — the recipient's name",
        ));
    };
    let Some(text) = fields.get("text") else {
        return Err(sig(
            "exarch-agents: the `message` spec needs a `text` field — what to send",
        ));
    };
    if !matches!(to, Value::String(_)) {
        return Err(sig(format!(
            "exarch-agents: `to` must be a Str naming the recipient, got {}",
            to.type_name()
        )));
    }
    if !matches!(text, Value::String(_)) {
        return Err(sig(format!(
            "exarch-agents: `text` must be a Str, got {}",
            text.type_name()
        )));
    }
    Ok(shell.enquire(
        mooring,
        request("agents", "message", Some(verbatim(spec, "exarch-agents")?)),
    )?)
}

/// `` `reply ``'s payload: the value crosses to whoever spawned this agent as
/// plain data, so it is checked first-order at this door exactly as the old
/// standalone `reply` builtin checked it — the desk's `!returns` refusal is
/// the only other reason this tag can fail.
fn reply_agent(value: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let payload = FOValue::try_from(value).map_err(|_| {
        sig(
            "exarch-agents: `reply`'s value must be first-order data — no closures, handles, or \
             environments — since it crosses to whoever spawned you as plain data",
        )
    })?;
    Ok(shell.enquire(
        mooring,
        request(
            "agents",
            "reply",
            Some(FOValue::List {
                items: vec![payload],
            }),
        ),
    )?)
}

/// `` `read ``'s payload: a descendant's name; the answer is the record it
/// fetches, not the roster.
fn read_agent(target: &Value, mooring: &Mooring, shell: &Shell) -> Settled<Value> {
    let Value::String(name) = target else {
        return Err(sig(format!(
            "exarch-agents: `read`'s payload must be a Str naming the descendant, got {}",
            target.type_name()
        )));
    };
    let answer = shell.enquire(
        mooring,
        request(
            "agents",
            "read",
            Some(FOValue::String {
                value: name.clone(),
            }),
        ),
    )?;
    Ok(Value::from(answer))
}

/// `exarch-agents <tag>` — one enquiry per tag. Every tag but `` `read `` answers
/// with the roster; `` `read `` answers the fetched record instead, so it
/// returns directly rather than falling through to [`roster`].
fn builtin_agents(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "exarch-agents: expected a `list, `start, `message, `cancel, `reply, or `read tag, got {}",
            args[0].type_name()
        )));
    };
    if let ("read", Some(target)) = (label.as_str(), payload) {
        return read_agent(target, mooring, shell);
    }
    let answer = match (label.as_str(), payload) {
        ("list", None) => shell.enquire(mooring, request("agents", "list", None))?,
        ("start", Some(spec)) => start_agent(spec, mooring, shell)?,
        ("message", Some(spec)) => message_agent(spec, mooring, shell)?,
        ("cancel", Some(target)) => {
            let Value::String(name) = target.as_ref() else {
                return Err(sig(format!(
                    "exarch-agents: `cancel`'s payload must be a Str naming the descendant, got {}",
                    target.type_name()
                )));
            };
            shell.enquire(
                mooring,
                request(
                    "agents",
                    "cancel",
                    Some(FOValue::String {
                        value: name.clone(),
                    }),
                ),
            )?
        }
        ("reply", Some(value)) => reply_agent(value, mooring, shell)?,
        _ => {
            return Err(sig(format!(
                "exarch-agents: tag must be one of `list, `start, `message, `cancel, `reply, `read — got \
                 {label}"
            )));
        }
    };
    fleet_answer(answer)
}

/// `` `add ``'s payload: checked through
/// [`schedule_trigger`]/[`schedule_label`], then enquired verbatim as
/// `` exarch-schedules `add ``. The self-wakeup grant and label uniqueness are
/// refusals the desk and the schedule registry own.
fn add_schedule(spec: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::Map(fields) = spec else {
        return Err(sig(format!(
            "exarch-schedules: `add`'s payload must be a [trigger: …, label: …, prompt: …] record, got {}",
            spec.type_name()
        )));
    };
    let Some(trigger) = fields.get("trigger") else {
        return Err(sig(
            "exarch-schedules: the `add` spec needs a `trigger` field — `cron '<expr>' or `after '<dur>'",
        ));
    };
    let Some(label) = fields.get("label") else {
        return Err(sig(
            "exarch-schedules: the `add` spec needs a `label` field — a Str naming the wakeup",
        ));
    };
    if fields.get("prompt").is_none() {
        return Err(sig(
            "exarch-schedules: the `add` spec needs a `prompt` field — the instruction delivered when the wakeup fires",
        ));
    }
    schedule_trigger(trigger)?;
    schedule_label(label)?;

    Ok(shell.enquire(
        mooring,
        request(
            "schedules",
            "add",
            Some(verbatim(spec, "exarch-schedules")?),
        ),
    )?)
}

/// `exarch-schedules <tag>` — one enquiry, whose answer is the registry itself:
/// every tag answers with the table, never a receipt of its own. The
/// self-wakeup grant refusal is the desk's.
fn builtin_schedules(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "exarch-schedules: expected a `list, `add, or `remove tag, got {}",
            args[0].type_name()
        )));
    };
    let answer = match (label.as_str(), payload) {
        ("list", None) => shell.enquire(mooring, request("schedules", "list", None))?,
        ("add", Some(spec)) => add_schedule(spec, mooring, shell)?,
        ("remove", Some(target)) => {
            let Value::String(target_label) = target.as_ref() else {
                return Err(sig(format!(
                    "exarch-schedules: `remove`'s payload must be a Str naming the wakeup, got {}",
                    target.type_name()
                )));
            };
            shell.enquire(
                mooring,
                request(
                    "schedules",
                    "remove",
                    Some(FOValue::String {
                        value: target_label.clone(),
                    }),
                ),
            )?
        }
        _ => {
            return Err(sig(format!(
                "exarch-schedules: tag must be one of `list, `add, `remove — got {label}"
            )));
        }
    };
    let FOValue::List { items } = answer else {
        return Err(sig(
            "exarch-schedules: host answered an unexpected shape for the listing",
        ));
    };
    Ok(Value::list(items.into_iter().map(Value::from).collect()))
}

/// `` `set ``'s payload: a `[key: Str, body]` record, checked and then sent
/// verbatim — the desk decodes `body` exactly as a surfaced `` `pin ``'s.
fn set_pin(spec: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::Map(fields) = spec else {
        return Err(sig(format!(
            "exarch-pins: `set`'s payload must be a [key: Str, body: …] record, got {}",
            spec.type_name()
        )));
    };
    let Some(key) = fields.get("key") else {
        return Err(sig(
            "exarch-pins: the `set` spec needs a `key` field — the register slot to write",
        ));
    };
    if !matches!(key, Value::String(_)) {
        return Err(sig(format!(
            "exarch-pins: `key` must be a Str, got {}",
            key.type_name()
        )));
    }
    if fields.get("body").is_none() {
        return Err(sig(
            "exarch-pins: the `set` spec needs a `body` field — the card to pin",
        ));
    }
    Ok(shell.enquire(
        mooring,
        request("pins", "set", Some(verbatim(spec, "exarch-pins")?)),
    )?)
}

/// A tag whose whole payload is the key it names, sent bare — `` `clear ``
/// and `` `read `` alike.
fn keyed_pin(key: &Value, tag: &str, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::String(key) = key else {
        return Err(sig(format!(
            "exarch-pins: `{tag}`'s payload must be a Str naming the slot, got {}",
            key.type_name()
        )));
    };
    Ok(shell.enquire(
        mooring,
        request("pins", tag, Some(FOValue::String { value: key.clone() })),
    )?)
}

/// `exarch-pins <tag>` — one enquiry per tag onto the register: `` `set ``
/// writes a slot, `` `clear `` empties one, `` `read `` and `` `list ``
/// answer it back.
fn builtin_pins(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "exarch-pins: expected a `set, `clear, `read, or `list tag, got {}",
            args[0].type_name()
        )));
    };
    match (label.as_str(), payload) {
        ("set", Some(spec)) => {
            set_pin(spec, mooring, shell)?;
            Ok(Value::Unit)
        }
        ("clear", Some(key)) => {
            keyed_pin(key, "clear", mooring, shell)?;
            Ok(Value::Unit)
        }
        ("read", Some(key)) => Ok(Value::from(keyed_pin(key, "read", mooring, shell)?)),
        ("list", None) => {
            let answer = shell.enquire(mooring, request("pins", "list", None))?;
            let FOValue::List { items } = answer else {
                return Err(sig(
                    "exarch-pins: host answered an unexpected shape for the listing",
                ));
            };
            Ok(Value::list(items.into_iter().map(Value::from).collect()))
        }
        _ => Err(sig(format!(
            "exarch-pins: tag must be one of `set, `clear, `read, `list — got {label}"
        ))),
    }
}

/// The `` `exarch-context `` scheme (`context_receipt_ty`) is a closed record, so a
/// survey missing any of its fields is host-side drift, not a call error —
/// name what is missing rather than shrugging at the whole shape.
fn context_receipt(answer: FOValue) -> Settled<Value> {
    const FIELDS: [&str; 2] = ["rows", "total-bytes"];
    let FOValue::Map { entries } = &answer else {
        return Err(sig(
            "exarch-context: host answered an unexpected shape for the survey",
        ));
    };
    if let Some(missing) = FIELDS
        .iter()
        .find(|field| !entries.iter().any(|(key, _)| key == *field))
    {
        return Err(sig(format!(
            "exarch-context: host answered a survey missing the `{missing}` field"
        )));
    }
    Ok(Value::from(answer))
}

/// A turn address: a List of turn ids, however the model built it. Which
/// turns the set may name is the desk's to judge — this is the type check the
/// row cannot state.
pub(crate) fn turn_list_payload(value: &Value, verb: &str) -> Settled<FOValue> {
    let Value::List(items) = value else {
        return Err(sig(format!(
            "{verb}: `turns` must be a List of turn Ints, got {}",
            value.type_name()
        )));
    };
    let mut turns = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Value::Int(turn) = item else {
            return Err(sig(format!(
                "{verb}: turn at index {index} must be an Int, got {}",
                item.type_name()
            )));
        };
        if *turn < 0 {
            return Err(sig(format!(
                "{verb}: turn at index {index} must be non-negative, got {turn}"
            )));
        }
        turns.push(FOValue::Int { value: *turn });
    }
    Ok(FOValue::List { items: turns })
}

/// The `` `evict `` spec, checked field by field and then sent verbatim: the
/// record crosses to the desk by name, as every other family's does, so
/// widening the reach later adds a field rather than shifting a position.
///
/// The scheme leaves the record open on `note`, since a closed row cannot
/// express an optional field, so the door is where a `note` of the wrong
/// type is caught; every rule on what a well-typed note may contain — empty,
/// oversized, multi-line — is the desk's.
pub(crate) fn context_evict_payload(value: &Value) -> Settled<FOValue> {
    const VERB: &str = "exarch-context `evict";
    let Value::Map(spec) = value else {
        return Err(sig(format!(
            "{VERB}: expected [turns: [Int]] or [turns: [Int], note: Str], got {}",
            value.type_name()
        )));
    };
    let Some(turns) = spec.get("turns") else {
        return Err(sig(format!(
            "{VERB}: the spec record needs a `turns` field — the turns to evict; \
             `!{{range a b}}` builds a run"
        )));
    };
    let _ = turn_list_payload(turns, VERB)?;
    if let Some(note) = spec.get("note")
        && !matches!(note, Value::String(_))
    {
        return Err(sig(format!(
            "{VERB}: `note` must be a Str, got {}",
            note.type_name()
        )));
    }
    verbatim(value, VERB)
}

/// `exarch-context <tag>` — one enquiry, whose answer is the context itself:
/// `` `survey `` describes it, `` `evict `` edits it, and both answer the
/// survey the transition leaves behind. Which turns an eviction may name —
/// unrecorded, already departed, the one being written, or none at all — is
/// the desk's.
fn builtin_context(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "exarch-context: expected a `survey or `evict tag, got {}",
            args[0].type_name()
        )));
    };
    let request = match (label.as_str(), payload) {
        ("survey", None) => request("context", "survey", None),
        ("evict", Some(spec)) => request("context", "evict", Some(context_evict_payload(spec)?)),
        _ => {
            return Err(sig(format!(
                "exarch-context: tag must be one of `survey, `evict — got {label}"
            )));
        }
    };
    context_receipt(shell.enquire(mooring, request)?)
}

/// `exarch-transcript <tag>` — one enquiry onto the record: `` `index `` lists every
/// turn it holds, `` `read `` returns the ones a read names as material, and
/// `` `grep `` searches them. Each tag answers its own shape, so the answer
/// is checked per tag rather than once.
fn builtin_transcript(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "exarch-transcript: expected an `index, `read, or `grep tag, got {}",
            args[0].type_name()
        )));
    };
    let (request, listed) = match (label.as_str(), payload) {
        ("index", None) => (request("transcript", "index", None), true),
        ("read", Some(spec)) => (
            request("transcript", "read", Some(transcript_read_payload(spec)?)),
            true,
        ),
        ("grep", Some(spec)) => (
            request("transcript", "grep", Some(transcript_grep_payload(spec)?)),
            false,
        ),
        _ => {
            return Err(sig(format!(
                "exarch-transcript: tag must be one of `index, `read, `grep — got {label}"
            )));
        }
    };
    let answer = shell.enquire(mooring, request)?;
    let shaped = if listed {
        matches!(answer, FOValue::List { .. })
    } else {
        matches!(answer, FOValue::Map { .. })
    };
    if !shaped {
        return Err(sig(format!(
            "exarch-transcript: host answered an unexpected shape for `{label}"
        )));
    }
    Ok(Value::from(answer))
}

/// `` `read ``'s spec, checked field by field and then sent verbatim. The
/// scheme closes the record on `turns`, so what reaches here is already a
/// List; the element check is the row's to leave and this one's to make.
/// Which turns are readable is the desk's to refuse, as it holds the record.
pub(crate) fn transcript_read_payload(value: &Value) -> Settled<FOValue> {
    const VERB: &str = "exarch-transcript `read";
    let Value::Map(spec) = value else {
        return Err(sig(format!(
            "{VERB}: expected [turns: [Int]], got {}",
            value.type_name()
        )));
    };
    let Some(turns) = spec.get("turns") else {
        return Err(sig(format!(
            "{VERB}: the spec record needs a `turns` field — the turns to read; \
             `!{{range a b}}` builds a run"
        )));
    };
    let _ = turn_list_payload(turns, VERB)?;
    verbatim(value, VERB)
}

/// `` `grep ``'s spec, checked field by field and then sent verbatim. The
/// scheme leaves the record open on `turns`, since a closed row cannot
/// express an optional field, so the door is where an address of the wrong
/// type is caught; the pattern itself is the desk's to compile.
pub(crate) fn transcript_grep_payload(value: &Value) -> Settled<FOValue> {
    const VERB: &str = "exarch-transcript `grep";
    let Value::Map(spec) = value else {
        return Err(sig(format!(
            "{VERB}: expected [pattern: Str], with an optional `turns: [Int]`, got {}",
            value.type_name()
        )));
    };
    let Some(pattern) = spec.get("pattern") else {
        return Err(sig(format!(
            "{VERB}: the spec record needs a `pattern` field — the Rust regex to search for"
        )));
    };
    if !matches!(pattern, Value::String(_)) {
        return Err(sig(format!(
            "{VERB}: `pattern` must be a Str, got {}",
            pattern.type_name()
        )));
    }
    if let Some(turns) = spec.get("turns") {
        let _ = turn_list_payload(turns, VERB)?;
    }
    verbatim(value, VERB)
}

/// A variant over a row of tags with stated payloads, ending in `tail`.
fn variant_row(tags: &[(&str, Ty)], tail: Row) -> Ty {
    let mut row = tail;
    for (label, ty) in tags.iter().rev() {
        row = Row::Extend(
            Label::Case((*label).to_string()),
            Field::present(ty.clone()),
            Box::new(row),
        );
    }
    Ty::Variant(row)
}

/// Left open on `tail` so an unknown tag reaches the runtime door that
/// enumerates the legal ones rather than dying as a row-unification mismatch.
fn open_variant(tags: &[(&str, Ty)], tail: RowVar) -> Ty {
    variant_row(tags, Row::Var(tail))
}

/// A record type left open on `tail`: the one shape a row can give an
/// *optional* field, whose type is then the door's to check.
fn open_record(fields: &[(&str, Ty)], tail: RowVar) -> Ty {
    let mut row = Row::Var(tail);
    for (label, ty) in fields.iter().rev() {
        row = Row::Extend(
            Label::Field((*label).to_string()),
            Field::present(ty.clone()),
            Box::new(row),
        );
    }
    Ty::Record(row)
}

/// `exarch-agents :: ∀α β ρ1 ρ2 ρ3 ρ4 ρ5. <list | start [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, search: Bool, provider: Variant ρ3, model: Variant ρ4] | message [to: Str, text: Str] | cancel Str | reply β | read Str | ρ5> → F α`
///
/// The outer tag row is open (`ρ5`) so an unrecognised tag reaches the
/// runtime door that names the six legal ones, rather than dying as a
/// row-unification mismatch.
///
/// The answer is not one fixed shape: `` `list `` answers the roster
/// `[[name, spawner, state, idle-s, elapsed-s, log-dir]]`, every other tag but
/// `` `read `` the summary `[live, replied]`, and `` `read `` the value a
/// descendant handed up, whose shape this call cannot know — so `α` is left
/// free rather than fixed to any of the three. This is the
/// `` `exarch-pins `read `` / `from-json` move ([`scheme_pins`]): trusted, not
/// checked, since only [`fleet_answer`]'s runtime door can tell them apart.
///
/// `start`'s and `message`'s record rows are closed because a record
/// literal with literal keys infers an exact one (`infer_map_val` builds on
/// `Row::Empty`), so a missing or misspelled field is a static error naming
/// it. The `type`, `grant`, `provider` and `model` rows *inside* `start` stay
/// open, because a literal tag infers its own open row: closing them would
/// make `` `bogus `` a bare row-mismatch diagnostic that never reaches
/// [`agent_type_label`]/[`spawn_grant`]/[`selection_label`], which
/// enumerate the legal labels. `search` is two-state rather than an
/// enumeration, so `Ty::Bool` closes it outright. `reply`'s `β` is likewise
/// trusted first-order data,
/// checked at [`reply_agent`]'s door rather than by the row.
fn scheme_agents(u: &mut Unifier) -> Scheme {
    let type_row = u.fresh_row_var();
    let grant_row = u.fresh_row_var();
    let provider_row = u.fresh_row_var();
    let model_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    let reply_ty = u.fresh_tyvar();
    let answer_ty = u.fresh_tyvar();
    scheme(
        &[reply_ty, answer_ty],
        &[],
        &[type_row, grant_row, provider_row, model_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("list", Ty::Unit),
                    (
                        "start",
                        closed_record(&[
                            ("prompt", Ty::String),
                            ("name", Ty::String),
                            ("type", Ty::Variant(Row::Var(type_row))),
                            ("grant", Ty::Variant(Row::Var(grant_row))),
                            ("search", Ty::Bool),
                            ("provider", Ty::Variant(Row::Var(provider_row))),
                            ("model", Ty::Variant(Row::Var(model_row))),
                        ]),
                    ),
                    (
                        "message",
                        closed_record(&[("to", Ty::String), ("text", Ty::String)]),
                    ),
                    ("cancel", Ty::String),
                    ("reply", Ty::Var(reply_ty)),
                    ("read", Ty::String),
                ],
                tag_row,
            ),
            pure(Ty::Var(answer_ty)),
        )),
    )
}

fn schedule_row_ty() -> Ty {
    closed_record(&[
        ("label", Ty::String),
        ("trigger", Ty::String),
        ("next-s", Ty::Int),
        ("fires", Ty::Int),
    ])
}

/// `exarch-schedules :: ∀ρ1 ρ2. <list | add [trigger: Variant ρ1, label: Str, prompt: Str] | remove Str | ρ2> → F [[label: Str, trigger: Str, next-s: Int, fires: Int]]`
///
/// Same shape as [`scheme_agents`]: an open outer tag row so an unknown tag
/// reaches the door naming the three legal ones, a closed `add` record row
/// so a missing or misspelled field is static, and an open `trigger` row
/// inside it so an unrecognised trigger reaches [`schedule_trigger`], which
/// names the legal shapes. `label` is a plain `Str` — every schedule names
/// itself, so there is no shape left to leave open.
fn scheme_schedules(u: &mut Unifier) -> Scheme {
    let trigger_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    scheme(
        &[],
        &[],
        &[trigger_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("list", Ty::Unit),
                    (
                        "add",
                        closed_record(&[
                            ("trigger", Ty::Variant(Row::Var(trigger_row))),
                            ("label", Ty::String),
                            ("prompt", Ty::String),
                        ]),
                    ),
                    ("remove", Ty::String),
                ],
                tag_row,
            ),
            pure(Ty::List(Box::new(schedule_row_ty()))),
        )),
    )
}

/// `exarch-pins :: ∀β α ρ1 ρ2. <set [key: Str, body: β] | clear Str | read Str | list | ρ2> → F α`
///
/// Same shape as [`scheme_agents`]: the outer tag row is open (`ρ2`) so an
/// unrecognised tag reaches [`builtin_pins`]'s door, and `set`'s record row
/// is closed since a record literal infers an exact one. `body`'s `β` is
/// trusted, unchecked, first-order data, the same move `reply`'s does —
/// only the desk's decoder judges whether it is a card. `` `read ``'s
/// answer is `α`, the `from-json`/`` `read `` precedent
/// ([`ral_core::typecheck::builtins::scheme::from_json`]): trusted, not
/// checked, since only the desk's own decoder can judge whether the card
/// read back matches the shape it expects.
fn scheme_pins(u: &mut Unifier) -> Scheme {
    let body_ty = u.fresh_tyvar();
    let tag_row = u.fresh_row_var();
    let answer_ty = u.fresh_tyvar();
    scheme(
        &[body_ty, answer_ty],
        &[],
        &[tag_row],
        thunk(fun(
            open_variant(
                &[
                    (
                        "set",
                        closed_record(&[("key", Ty::String), ("body", Ty::Var(body_ty))]),
                    ),
                    ("clear", Ty::String),
                    ("read", Ty::String),
                    ("list", Ty::Unit),
                ],
                tag_row,
            ),
            pure(Ty::Var(answer_ty)),
        )),
    )
}

/// One turn as both `` exarch-context `survey `` and `` exarch-transcript `index `` name
/// it; the index adds `held`, which a closed row of this shape cannot carry,
/// so `` `exarch-transcript ``'s own answer type is left free.
fn context_turn_ty() -> Ty {
    closed_record(&[
        ("id", Ty::Int),
        ("role", Ty::String),
        ("kind", Ty::String),
        ("label", Ty::String),
        ("bytes", Ty::Int),
    ])
}

fn context_receipt_ty() -> Ty {
    closed_record(&[
        ("rows", Ty::List(Box::new(context_turn_ty()))),
        ("total-bytes", Ty::Int),
    ])
}

/// `exarch-context :: ∀ρ1 ρ2. <survey | evict [turns: [Int] | ρ1] | ρ2> → F [rows: [[id: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int]`
///
/// Same shape as [`scheme_agents`] and [`scheme_schedules`]: an open outer
/// tag row so an unknown tag reaches the door naming the two legal ones.
///
/// `evict`'s record row anchors `turns` — the address is the edit, so there
/// is nothing optional about it — and stays open on `ρ1` because `note` is
/// optional and a closed row cannot say so; [`context_evict_payload`]
/// refuses a `note` of the wrong type, and the desk an empty, oversized, or
/// multi-line one — a required `note` would invite `''`, and a marker
/// reading `Your note at eviction: ""` is a defect.
///
/// One answer for both tags: an edit changes what is addressable, so the
/// survey the transition leaves behind is what the next edit must be written
/// against.
fn scheme_context(u: &mut Unifier) -> Scheme {
    let evict_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    scheme(
        &[],
        &[],
        &[evict_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("survey", Ty::Unit),
                    (
                        "evict",
                        open_record(&[("turns", Ty::List(Box::new(Ty::Int)))], evict_row),
                    ),
                ],
                tag_row,
            ),
            pure(context_receipt_ty()),
        )),
    )
}

/// `exarch-transcript :: ∀α ρ2 ρ3. <index | read [turns: [Int]] | grep [pattern: Str | ρ2] | ρ3> → F α`
///
/// The outer tag row is open (`ρ3`) so an unrecognised tag reaches the
/// runtime door that names the three legal ones, rather than dying as a
/// row-unification mismatch.
///
/// Each tag answers its own shape — a listing, one record per turn a read
/// named, a hit table — so `α` is left free rather than fixed to any one of
/// them, exactly as [`scheme_agents`] leaves it free for `` `read ``. The
/// answer's shape is then the door's to check and the docstring's to state.
///
/// `read`'s record is closed: its one field is the address it reads, so a
/// misspelling is a type error rather than a call that reaches the host
/// naming nothing. `grep`'s row is open on `ρ2`, since `turns` narrows it
/// and `pattern` alone is required, and a closed row cannot say so.
fn scheme_transcript(u: &mut Unifier) -> Scheme {
    let grep_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    let answer_ty = u.fresh_tyvar();
    scheme(
        &[answer_ty],
        &[],
        &[grep_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("index", Ty::Unit),
                    (
                        "read",
                        closed_record(&[("turns", Ty::List(Box::new(Ty::Int)))]),
                    ),
                    ("grep", open_record(&[("pattern", Ty::String)], grep_row)),
                ],
                tag_row,
            ),
            pure(Ty::Var(answer_ty)),
        )),
    )
}

// A named array, not a promoted temporary: rustc refuses promotion once an
// entry carries `BuiltinEntry`'s interior-mutable arity cache.
static HARNESS_BUILTINS_ARR: [BuiltinEntry; 5] = [
    BuiltinEntry::new(
        Cow::Borrowed("exarch-agents"),
        scheme_agents,
        "exarch-agents <tag>  — the fleet: `list what is live, `start a child, `message one, `cancel one, `reply to hand your own value up, `read one back off a descendant. Every tag but `list and `read answers `summary [live: Int, replied: Int] afterwards — how many other agents are alive around you, and how many of the agents you started park holding a value you have not fetched. It is the world after the transition rather than a receipt for what you just did, but two integers rather than a roster: after a `start you already know the name you chose, and what you could not have derived is whether someone is waiting on you. A non-zero `replied` is the one number asking you to act — call `read. Call `list when you want the rows.\n\nexarch-agents `list  — the rows: every live agent in your own tree, oldest first, you among them — not only the ones you started, because you may message any of them. `spawner` says who started each one: `root for an agent a human started, `agent <name> otherwise, so the flat listing is still the spawn tree, and the rows reachable from your own name are the ones you may also `cancel and `read. `state` is `busy while working, `waiting-on-agents while held only by a busy child of its own, `replied once it has called `reply and parked, `waiting once a human has engaged it and it parked with no reply. `idle-s` is seconds since it parked — zero while `busy` or `waiting-on-agents. A settled agent (cancelled, failed, or reaped past its hour) is not listed. This is how you recover names after an eviction, your own among them.\n\nexarch-agents `start [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>, provider: `inherit|`named <Str>, model: `inherit|`named <Str>]  — launch a sub-agent. Launch-only and always asynchronous: the child's reply is NOT this call's result — it arrives later, as a one-line notice in your inbox, and you fetch the value with `read. The answer's roster carries the child's row, and that row's name and log-dir are its receipt. `type` selects the child's memory: `amnemon` starts blank (no shared history), while `mnemon` inherits your current model-visible conversation. A `mnemon` child left on your own selection reuses your provider's cache; one sent to another account or model is still sound — reasoning crosses as plain text, not as signed blocks — but forfeits that locality, so pay for it deliberately. Every child receives the value-snapshot of the parent's bindings, cwd, and env — `mnemon` too; the serializable fragment crosses, while a live job handle becomes an opaque placeholder. `prompt` is a computed string and becomes the child's fresh final prompt. Keep large material in a named binding rather than splicing it into prompt; small, certainly-needed material may still be spliced. Wrap `prompt` in a raw string #'…'# if it carries $, !, or quotes. `name` is the child's identity — non-empty, at most 24 characters, ASCII letters/digits/-/_ only — and must not be borne by any live agent, or the call is refused; pick something descriptive, like 'fix-parser-tests'. `grant` is the child's ceiling: `inherit (decline to narrow — the child runs under your own authority), `confined (offline, no home reads), `read-only (writes only to scratch), `edit-only (edits the working tree, no build tooling), `reasonable (everyday tooling), or `restrict <record>, which takes a capability record of the same shape `grant [...] takes — [exec, fs, net, detach, editor, shell], every key optional — and is how you hand a child a ceiling you computed in your own shell, e.g. `grant: `restrict $ceiling`. Any other shape is refused, naming all six. Every form bounds the child to at most your own authority: a grant is one more narrowing layer on the stack you already hold, never a widening, so asking for more than you have silently yields less rather than failing. `search` states whether the child may use the provider's own built-in web search, bounded above by your own — asking for it when you do not have it silently yields a child without it. `provider` and `model` say what the child runs on, and both are always written — there is no omitting them, and `inherit is how you say you have no opinion. `provider: `inherit, model: `inherit` shares your own provider outright and is the plain default. `provider: `inherit, model: `named '<model>'` keeps your account and credential and changes only the model — the way to spend a cheaper, faster model on a narrow child while you keep a stronger one for yourself. `provider: `named '<provider>', model: `inherit` moves the child to another signed-in account: your own model if that account is the one you are on, otherwise that account's default model, and the call is refused naming `model` if it publishes none. `provider: `named …, model: `named …` says both outright. A provider name that no signed-in account answers to, or that several answer to, is refused naming the accounts you have; pick from those. Effort, temperature, and output cap are the operator's knobs rather than part of a model's identity, so they carry across whatever you name. Delegation depth is finite — each descendant is handed one less unit of fuel than its spawner holds, and once fuel reaches zero this call is refused; fuel bounds how deep a chain may recurse, never how many children you may start at any one depth.\n\nexarch-agents `message [to: <Str>, text: <Str>]  — send `text` as a marked item to the live agent named `to`; it lands at that agent's next exchange boundary, not as human input, and wakes a `replied or `waiting one into a fresh exchange. Any live agent may receive it — a descendant, a sibling, an ancestor — but not yourself; the fleet is one mailbox space, and `list names all of it, so anyone you can see you can write to. It does not return the recipient's answer: this is coordination, not a call. Nothing in the roster changes, so the answer is the plain confirmation that the recipient was live when you sent.\n\nexarch-agents `cancel <name>  — ask the live descendant named `name` to stop. It stops at its next checkpoint and then delivers a cancelled result to your inbox. Only a descendant of yours may be cancelled — never a sibling, an ancestor, or yourself; refused otherwise. The roster names the whole fleet, so it lists agents this tag will refuse: `spawner` is how you tell them apart before you ask. A cancel is a request, not a transaction: the child is still running when this answers, and still counted by the `summary you get back. A name still on a later `list is NOT a failed cancel — do not fire it again; read `list later still and find it gone.\n\nexarch-agents `reply <value>  — hand `value` back to whoever spawned you. Your parent receives exactly this value, nothing else — not your reasoning, your shell bindings, or any prose you streamed along the way. `value` must be first-order data: no closures, handles, or environments; passing one fails this call with a didactic error and your run continues, so fix the value and call `reply again. Call it more than once in an exchange and the last call wins — an earlier value is discarded, not appended. It does not end your run: you park (`state `replied) rather than settle, and may be `message`d for a follow-up — answer that with another `reply. A non-finite Float (NaN, +Infinity, -Infinity) reaches your parent as the string \"NaN\"/\"Infinity\"/\"-Infinity\" — JSON, which the value eventually crosses into, has no such numbers. Refused on the interactive trunk and every /branch child: they converse with the user turn after turn and never return, so they hold no obligation to call this.\n\nexarch-agents `read <name>  — fetch the value the live descendant named `name` last handed to `reply, as [name: Str, reply: <value>]. The one tag that does not answer the roster. Only a descendant of yours may be read — never a sibling, an ancestor, or yourself; refused otherwise, as is a name that never replied. Idempotent: reading again before the child replies afresh answers the same value.\n\nEach tag is one exchange with the host, and what it answers — the rows for `list, the two counts for every tag but `list and `read — is the fleet as it stands once the transition has landed. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_agents),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-schedules"),
        scheme_schedules,
        "exarch-schedules <tag>  — your self-wakeups: `list what is armed, `add one, `remove one. Every tag answers with the table afterwards, [[label: Str, trigger: Str, next-s: Int, fires: Int]], so what you read back is always what is armed now rather than a receipt for what you just did. Requires the self-wakeup grant (--allow-schedule) — an agent that can wake itself indefinitely holds real authority, so without the grant every tag is refused.\n\nexarch-schedules `list  — your live wakeups, oldest first: label as you named it, trigger as its source text (a cron expression, or `after 30m`), next-s the seconds until the next fire, recomputed as you ask, and fires how many times it has fired so far. Only live schedules appear: a spent one-shot has already removed itself, so a label you armed with `after and then see no more of has fired, not vanished. This is how you recover labels after an eviction.\n\nexarch-schedules `add [trigger: `cron <Str>|`after <Str>, label: <Str>, prompt: <Str>]  — arm a self-wakeup: at the chosen time a marked item carrying `prompt` is delivered to your inbox and re-engages you with no human present. It drains at your next exchange boundary — as soon as the tool batch in flight settles, not only at the end of the exchange — and arrives as marked chrome, `[scheduled '<label>' · <trigger>] <prompt>`, never read as a command even when the prompt opens with `/`. `trigger` is exactly one of two variants; any other shape is refused, naming both. `cron '<expr>'` is recurring: five whitespace-separated fields, minute hour day-of-month month day-of-week, read in the host's local timezone — e.g. `cron '0 9 * * 1-5'` for weekdays at 09:00. Each field is a comma list of `*`, a number, a range `a-b`, or a step over either (`*/15`, `a-b/2`, `N/step` meaning N up to the field's maximum); month and day-of-week also accept three-letter names (jan…dec, sun…sat), and day-of-week accepts 7 as a second spelling of Sunday. When both day fields are restricted, either one matching fires it (Vixie-cron's OR rule); when only one is, that one decides. Every fire recomputes the next occurrence in the host timezone, so DST shifts, clock steps, and suspends are absorbed rather than accumulated. `after '<n><unit>'` is a one-shot relative delay from the moment of arming, unit one of s/m/h/d and the count greater than zero — e.g. `after '30m'`, `after '2h'`. A trigger with no next occurrence at all — a parseable but impossible date such as `cron '0 0 30 2 *'` — is refused here rather than arming silently. `label` names the wakeup and is its identity: it must not be borne by another live schedule, and you must always supply one. `prompt` is the natural-language instruction you act on when woken, not code. Read the new row's next-s out of the answer to catch a cron expression that parsed but does not mean what you meant. Once armed: an `after removes itself when it fires; a cron re-arms itself, and drops itself only when nothing further lies inside its search horizon. A fire whose previous wakeup is still sitting undrained in your inbox is skipped, not queued behind it, and does not count as a fire. While any schedule is live this session parks for the next wakeup at quiescence instead of ending, so a recurring schedule you never remove keeps this agent alive indefinitely — that is what the grant buys. `/clear` drops every live schedule.\n\nexarch-schedules `remove <label>  — disarm the wakeup bearing `label`; its next occurrence goes with it and nothing further is delivered. The entry is gone in the answer, so the row's absence is the confirmation. A label that was never there answers the same way, and that is no evidence of a mistake: a one-shot may have fired and removed itself since you read it.\n\nEach tag is one exchange with the host, and the table it answers is the schedule registry as it stands once the transition has landed. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_schedules),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-pins"),
        scheme_pins,
        "exarch-pins <tag>  — your register of pinned state: a small set of named slots that outlive any one exchange. `set` writes a slot, `clear` empties one, `read` fetches a slot back, `list` names every occupied one. Reads and writes your own register only.\n\nexarch-pins `set [key: <Str>, body: <card>]  — overwrite the register slot named `key` with `body`, a `card [...]` value (or one of its marks bare, e.g. `text [...]`). A body with nothing to show clears the slot instead of pinning an empty one.\n\nexarch-pins `clear <key>  — empty the register slot named `key`. Clearing an already-empty slot is not an error.\n\nexarch-pins `read <key>  — the card currently pinned under `key`, as a `card value you can destructure, or () if the slot is empty.\n\nexarch-pins `list  — the keys currently occupied on your register, as [String]. Read one back with `read`.\n\nEach tag is one exchange with the host. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_pins),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-context"),
        scheme_context,
        "exarch-context <tag>  — the context: the messages the provider is sent on your next request, as a list of turns. A turn is either a user turn — a prompt, or an import's opening — or an assistant turn — one assistant message, the tool results it called for, and any steering delivered before the next request. Turn ids are minted in one increasing sequence per lineage and never reused; every tool result ends with `TURN: <id>`, the id of the assistant turn it closes, and `exarch-context `survey` lists the rest. Every tag answers the survey after it has acted: [rows: [[id: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int].\n\nexarch-context `survey  — acts on nothing. `rows` is one row per turn in the context, oldest first: `id` the turn's id; `role` `user` or `assistant`; `kind` `own` (recorded by this session), `import` (a note the harness imported, e.g. on resume), or `inherited` (recorded by an ancestor before you were forked); `label` the first 50 characters of the turn's first line; `bytes` the serialised size of the turn's messages. `total-bytes` is the serialised size of what is actually sent — the resident turns plus every marker — and is the figure to weigh against the provider's context window.\n\nexarch-context `evict [turns: [Int], note: Str]  — removes the named turns from the context. `turns` is a list of turn ids in any order, repeats ignored; `!{range 41 44}` is [41, 42, 43]. Refused, naming the turn: an id never recorded; an id that has already left; the id of the turn being written now, i.e. the assistant turn whose result this call is part of. Kept silently: a user turn while any assistant turn answering it — the assistant turns between it and the next user turn — is in the context and not named; a set left empty by this rule is refused. Every other named turn leaves at once, wherever it lies. Where a run of consecutive turns has left, the context carries one marker in their place: a bracketed user-role message stating which turns left, one line per turn (id, role, label, KB; at most 40 lines per marker, older ones collapsed to a count), the note of the eviction that took them, and how to read them back. `note` is optional; if given it is one line of at most 240 bytes and appears verbatim in that marker. Evicted turns remain in the transcript and are readable with `exarch-transcript `read`. Cost: the provider's cache holds only the prefix before the earliest change, so the next request re-reads everything from the first evicted turn onward.\n\nWhen the context nears the provider's window, the harness evicts the oldest turns itself at the next turn boundary, without a note; as the context grows into the reserve before that point you are warned once, at a tool boundary, naming the turns the cut would take. Making that cut yourself is how a note gets attached.\n\nEach tag is one exchange with the host, and the survey it answers is the context as it stands once the transition has landed; an eviction lands at the desk immediately and is recorded. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_context),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-transcript"),
        scheme_transcript,
        "exarch-transcript <tag>  — the record of every turn this session or any ancestor of it ever recorded, in the context or not. Read-only. Every turn is readable except the one being written now — the assistant turn whose result this call is part of. Every tag addresses turns by id, as a list: `!{range 41 44}` is [41, 42, 43]; `exarch-context `survey` and `exarch-transcript `index` show the ids.\n\nexarch-transcript `index  — [[id: Int, role: Str, kind: Str, label: Str, bytes: Int, held: Str]], every recorded turn oldest first. The first five fields are the survey's; `held` is `resident` (in the context) or `evicted` (left it).\n\nexarch-transcript `read [turns: [Int]]  — [[turn: Int, role: Str, messages: [Message]]], one element per named turn in id order, each turn's messages exactly as the provider was sent them. Refused, naming the turn: an id never recorded, and the turn being written now. A Message is [role: `system|`user|`assistant|`tool, parts: [Part]]. A Part is one of: `text [content: Str]; `program [tool: Str, source: Str, keys: [Str]] — a tool call, where for the ral tool `source` is the script and `keys` is empty, and for any other tool `source` is empty and `keys` names its arguments; `result [content: Str] — a tool result as the model saw it, clipping included; `reasoning [content: Str] — reasoning in full; `binary [content-type: Str, name: Str, bytes: Int] — an attachment's metadata, never its bytes; `custom [provider: Str, model: Str] — a provider extension's identity, never its payload. Nothing here is clipped or capped: bind the answer and take slices of it, since the whole of it in your context is what the eviction saved.\n\nexarch-transcript `grep [pattern: Str, turns: [Int]]  — [hits: [[turn: Int, role: Str, line: Int, text: Str]], total: Int]: every line of every message in the searched turns matching `pattern`, a Rust regex. `turns` is optional; absent, every recorded turn is searched. `hits` holds at most the 100 oldest matches, each `text` clipped to 200 bytes, `line` 1-based within its message; `total` is the count of all matches. `role` is the message's role.\n\nA `mnemon child (exarch-agents `start [type: `mnemon, …]) shares this transcript and can search or read it in its own context; an `amnemon child has only its own.\n\nAnswered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_transcript),
    ),
];
pub static HARNESS_BUILTINS: &[BuiltinEntry] = &HARNESS_BUILTINS_ARR;

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::testkit::ral_call;

    #[test]
    fn spawn_grant_admits_every_bare_tag() {
        for label in bare_grant_tags() {
            let v = Value::Variant {
                label: label.to_string(),
                payload: None,
            };
            let grant = spawn_grant(&v).unwrap_or_else(|e| panic!("must admit `{label}: {e:?}"));
            let read_back = match (&grant, label) {
                (SpawnGrant::Inherit, "inherit") => true,
                (SpawnGrant::Base(base), _) => base == label,
                _ => false,
            };
            assert!(read_back, "`{label} must read back as its own grant");
        }
    }

    /// The record is carried, not decoded: the door's whole judgement is that
    /// it is first-order data.
    #[test]
    fn spawn_grant_carries_a_restrict_record_verbatim() {
        let v = Value::Variant {
            label: "restrict".to_string(),
            payload: Some(Box::new(Value::map(vec![(
                "net".to_string(),
                Value::Bool(false),
            )]))),
        };
        let grant = spawn_grant(&v).unwrap_or_else(|e| panic!("must admit `restrict: {e:?}"));
        let SpawnGrant::Restrict(FOValue::Map { entries }) = grant else {
            panic!("`restrict must carry its record through as first-order data");
        };
        assert_eq!(entries.len(), 1, "the record must cross unchanged");
    }

    #[test]
    fn spawn_grant_rejects_an_unknown_tag_naming_every_legal_shape() {
        let v = Value::Variant {
            label: "bogus".to_string(),
            payload: None,
        };
        let err = match spawn_grant(&v) {
            Err(ral_core::types::Break::Error(e)) => e,
            other => panic!("expected a door error, got {other:?}"),
        };
        for label in bare_grant_tags().chain(["restrict"]) {
            assert!(
                err.message.contains(label),
                "must name `{label}`, got: {}",
                err.message
            );
        }
    }

    /// Every bare tag the door admits: the bases, plus `` `inherit ``, which
    /// names none and so is the one admitted tag policy has nothing to resolve.
    fn bare_grant_tags() -> impl Iterator<Item = &'static str> {
        std::iter::once("inherit").chain(crate::policy::SPAWN_BASES)
    }

    /// Every base the door admits must resolve to a bake-in profile — which
    /// also parses and evaluates that profile's `data/*.exarch.ral` — so a label
    /// added here alone shows up. The door's table is the narrower of the two:
    /// the policy layer offers a launching human bases a child is not handed.
    #[test]
    fn every_permission_label_resolves_to_a_bake_in_base() {
        let cwd = std::env::current_dir().unwrap().display().to_string();
        for label in crate::policy::SPAWN_BASES {
            crate::policy::base_layer(label, &cwd)
                .unwrap_or_else(|e| panic!("door label `{label} must name a bake-in base: {e}"));
        }
        let err =
            crate::policy::base_layer("bogus", &cwd).expect_err("an unknown base must be refused");
        let offered: std::collections::BTreeSet<&str> = err
            .rsplit_once("expected one of: ")
            .unwrap_or_else(|| panic!("the refusal must enumerate the bases, got: {err}"))
            .1
            .split(", ")
            .collect();
        assert!(
            crate::policy::SPAWN_BASES
                .iter()
                .all(|l| offered.contains(l)),
            "every door label must be a base the policy layer offers, got: {offered:?}"
        );
    }

    /// A base carrying a payload is refused, never truncated to its label:
    /// `` `restrict `` is the only tag that takes one.
    #[test]
    fn spawn_grant_rejects_a_base_carrying_a_payload() {
        let v = Value::Variant {
            label: "confined".to_string(),
            payload: Some(Box::new(Value::Int(1))),
        };
        assert!(spawn_grant(&v).is_err());
    }

    /// The door validates `name`, `type`, and `grant` before
    /// `fork_into_nursery`/`enquire` ever run, so no child is registered.
    #[test]
    fn unknown_grant_label_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `bogus, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        for label in bare_grant_tags() {
            assert!(
                result.content.contains(label),
                "must name `{label}`, got: {}",
                result.content
            );
        }
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an unknown grant label must never register a child"
        );
    }

    /// `` `dangerous `` has left the spawn surface: there it resolved to ⊤, a
    /// layer saying nothing, which is what `` `inherit `` now says outright —
    /// so the refusal must send a model there rather than leave it guessing.
    #[test]
    fn dangerous_is_no_longer_a_spawn_grant_and_the_refusal_points_at_inherit() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `dangerous, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result
                .content
                .contains("inherit is how you decline to narrow"),
            "the refusal must point at `inherit, got: {}",
            result.content
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "`dangerous must never register a child"
        );
    }

    /// `` `restrict `` is the one grant tag that takes a payload, so bare it is
    /// a shape error, and the refusal must say what it was missing.
    #[test]
    fn a_bare_restrict_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `restrict, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("capability record"),
            "the refusal must name the record `restrict carries, got: {}",
            result.content
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "a bare `restrict must never register a child"
        );
    }

    /// Both new spellings pass the grant door — the open `grant` row carries a
    /// payload-bearing tag too — so what comes back is the *next* door's
    /// refusal, naming `provider`, and still no child.
    #[test]
    fn inherit_and_restrict_pass_the_grant_door() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        for (call, grant) in [("call-1", "`inherit"), ("call-2", "`restrict [net: false]")] {
            let result = session.run_shell(
                call.to_string(),
                &format!(
                    r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: {grant}, search: true, provider: `guess, model: `inherit]"
                ),
                5,
                &emit,
            );
            assert!(
                result.content.contains("`provider`") && !result.content.contains("`grant`"),
                "{grant} must pass the grant door and be refused at `provider`, got: {}",
                result.content
            );
            assert!(
                crate::fleet::roster::summary(&session.agent).live == 0,
                "a later door's refusal must never leave a child registered"
            );
        }
    }

    #[test]
    fn unknown_type_tag_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `bogus, grant: `confined, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("amnemon"),
            "got: {}",
            result.content
        );
        assert!(result.content.contains("mnemon"), "got: {}", result.content);
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an unknown type tag must never register a child"
        );
    }

    /// The `provider`/`model` rows are open too, so an unrecognised arm must
    /// reach the door and be told the two that exist.
    #[test]
    fn unknown_selection_tag_errors_naming_both_arms() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `guess, model: `inherit]",
            5,
            &emit,
        );
        for arm in ["inherit", "named"] {
            assert!(
                result.content.contains(arm),
                "must name `{arm}, got: {}",
                result.content
            );
        }
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an unknown selection tag must never register a child"
        );
    }

    /// An empty name is a mistake, not a way of spelling `` `inherit ``.
    #[test]
    fn an_empty_named_selection_is_refused() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `named '']",
            5,
            &emit,
        );
        assert!(
            result.content.contains("non-empty"),
            "the refusal must say the name may not be empty, got: {}",
            result.content
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an empty selection name must never register a child"
        );
    }

    #[test]
    fn invalid_name_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r#"exarch-agents `start [prompt: #'hi'#, name: "has space", type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `inherit]"#,
            5,
            &emit,
        );
        assert!(result.content.contains("name"), "got: {}", result.content);
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an invalid name must never register a child"
        );
    }

    /// `scheme_agents`'s outer tag row (`ρ3`) is open, so an unrecognised tag
    /// must reach `builtin_agents`'s door rather than die as a row mismatch.
    #[test]
    fn unknown_outer_tag_reaches_the_door_naming_every_legal_label() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell("call-1".to_string(), "exarch-agents `stop 'x'", 5, &emit);
        for tag in ["list", "start", "message", "cancel"] {
            assert!(
                result.content.contains(tag),
                "must name `{tag}, got: {}",
                result.content
            );
        }
    }

    /// Static, not a door error: `scheme_agents`'s closed `` `start `` record
    /// row reports which label is absent.
    #[test]
    fn missing_agent_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("field named 'grant'"),
            "the diagnostic must name the missing field, got: {}",
            result.content
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "a missing spec field must never register a child"
        );
    }

    /// A misspelled field (`grnat` for `grant`) is not the same fault as an
    /// absent one: the closed `` `start `` row rejects it statically too, and
    /// must still name a field rather than shrug at the whole record.
    #[test]
    fn misspelled_start_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grnat: `confined, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("no field named 'grnat'"),
            "the diagnostic must name the offending field, got: {}",
            result.content
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "a misspelled spec field must never register a child"
        );
    }

    /// Drives `run_shell` rather than `Avatar::deliberate`'s provider loop:
    /// the spawn seeds the child's handle from the parent's *own*
    /// `Arc<Provider>`, so one script consumed by both a driven parent
    /// exchange and its child races over which gets which stage.
    #[test]
    fn agent_full_stack_round_trip_answers_the_summary_and_parks_a_reply() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "reply-1",
                    r"exarch-agents `reply 'say hi'",
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'say hi'#, name: 'helper', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("live: 1"),
            "the summary answered afterwards must count the child, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    let notice = r.outcome.marked_item(&r.name, r.elapsed);
                    assert!(
                        notice.contains("exarch-agents `read 'helper'"),
                        "the reply notice must name the fetch command, got: {notice}"
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        let read = session.run_shell(
            "call-2".to_string(),
            r"exarch-agents `read 'helper'",
            5,
            &emit,
        );
        assert!(
            read.content.contains("say hi"),
            "exarch-agents `read` must answer the child's deposited reply, got: {}",
            read.content
        );
        let roster = session.run_shell("call-3".to_string(), r"exarch-agents `list", 5, &emit);
        assert!(
            roster.content.contains("replied"),
            "the replied child must stay on the roster as `replied, got: {}",
            roster.content
        );
    }

    /// A cancel is a request, not a transaction: it only stamps the cancel
    /// layers, and the cancelled agent's own loop is what retires it — so the
    /// row must still be listed the instant this answers.
    ///
    /// Pinned with a bare agent rather than a real spawned child: a scripted
    /// child runs to completion and settles on the same synchronous thread
    /// that starts it, so a second `run_shell` racing a real
    /// `` `start ``/`` `cancel `` pair would be racing CPU-bound work with no
    /// reliable window in between.
    #[test]
    fn agents_cancel_answer_still_counts_the_cancelled_agent() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let mut doomed = crate::agent::testkit::TestAgentSpec::new("doomed");
        doomed.parent = Some(session.agent.clone());
        let _doomed = crate::agent::testkit::test_agent(&session.fleet, doomed)
            .expect("a fresh child of a live parent");

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-agents `cancel 'doomed'",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid exarch-agents `cancel call must succeed, got: {}",
            result.content
        );
        assert!(
            result.content.contains("live: 1"),
            "a cancel is a request, not a transaction — the target is still \
             counted by the summary answered afterwards, got: {}",
            result.content
        );
    }

    // ── schedule family door tests ───────────────────────────────────────
    //
    // Tag payloads are greedy, but `at_tag_payload_end` in
    // `core/src/syntax/parser.rs` stops one at a comma — so inside a record
    // literal a nullary tag cannot swallow its neighbour. That is why
    // `` exarch-schedules `add `` takes one spec record, not three positional
    // arguments.

    #[test]
    fn bad_cron_expr_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `cron '* * * *', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("five fields"),
            "must carry the parser's own message, got: {}",
            result.content
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "a bad cron expression must never register a schedule"
        );
    }

    #[test]
    fn bad_duration_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `after 'nope', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("duration"),
            "must carry the parser's own message, got: {}",
            result.content
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "a bad duration must never register a schedule"
        );
    }

    #[test]
    fn trigger_neither_cron_nor_after_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `bogus 'x', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(result.content.contains("cron"), "got: {}", result.content);
        assert!(result.content.contains("after"), "got: {}", result.content);
        assert!(
            session.agent.schedules.list().is_empty(),
            "an unrecognised trigger tag must never register a schedule"
        );
    }

    /// Static, not a door error: `scheme_schedules`'s closed `` `add ``
    /// record row reports which label is absent.
    #[test]
    fn missing_spec_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `after '1s', label: 'nightly']",
            5,
            &emit,
        );
        assert!(
            result.content.contains("missing a field named 'prompt'"),
            "the diagnostic must name the missing field, got: {}",
            result.content
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "a missing spec field must never register a schedule"
        );
    }

    /// The same closed row in the other direction: it admits exactly
    /// trigger/label/prompt.
    #[test]
    fn unknown_extra_spec_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#, extra: 1]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("no field named 'extra'"),
            "the diagnostic must name the surplus field, got: {}",
            result.content
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "an unknown extra spec field must never register a schedule"
        );
    }

    /// A trunk holding the self-wakeup grant, which `for_test` withholds.
    fn granted_trunk() -> crate::agent::Avatar {
        crate::agent::Avatar::for_test_with(crate::agent::TestTrunk {
            allow_schedule: true,
            ..crate::agent::TestTrunk::new("system")
        })
        .expect("a granted test trunk")
    }

    #[test]
    fn schedule_add_answer_carries_the_new_row() {
        let mut session = granted_trunk();

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("nightly"),
            "the table answered afterwards must carry the row just armed, got: {}",
            result.content
        );
    }

    /// The wait is generous because the fire really is a wall-clock second
    /// away: `parse_duration`'s smallest unit is whole seconds.
    #[test]
    fn schedule_full_stack_round_trip_answers_the_table_and_fires_into_inbox() {
        let mut session = granted_trunk();

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid exarch-schedules `add call must succeed, got: {}",
            result.content
        );
        assert!(
            result.content.contains("next-s"),
            "the table answered afterwards must carry the new row, got: {}",
            result.content
        );
        let live = session.agent.schedules.list();
        assert_eq!(live.len(), 1, "the schedule must be registered");
        assert_eq!(live[0].label, "nightly", "must take the given label");
        assert!(
            result.content.contains("nightly"),
            "the table must carry the given label, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Wakeup(text)) => {
                    assert!(
                        text.contains("wake"),
                        "the wakeup must carry the prompt, got: {text}"
                    );
                    break;
                }
                Some(_other) => panic!("expected a Wakeup item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the schedule did not fire within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }

    /// `` `removed ``/`` `no-such-label `` are retired: `` exarch-schedules `remove ``
    /// now answers the table afterwards either way, so the row's absence is
    /// the only evidence — a miss on an already-gone label answers the same
    /// way as a hit, and that is not itself proof of a mistake.
    #[test]
    fn schedule_remove_full_stack_disarms_by_label() {
        let mut session = granted_trunk();

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid exarch-schedules `add call must succeed, got: {}",
            result.content
        );
        assert_eq!(
            session.agent.schedules.list().len(),
            1,
            "the schedule must be registered"
        );

        let result = session.run_shell(
            "call-2".to_string(),
            "exarch-schedules `remove 'nightly'",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid exarch-schedules `remove call must succeed, got: {}",
            result.content
        );
        assert!(
            !result.content.contains("nightly"),
            "the removed row must be gone from the table answered afterwards, got: {}",
            result.content
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "exarch-schedules `remove by label must remove the schedule"
        );

        let miss = session.run_shell(
            "call-3".to_string(),
            "exarch-schedules `remove 'nightly'",
            5,
            &emit,
        );
        assert!(
            miss.content.contains("EXIT: 0"),
            "removing an already-absent label answers the same empty table, not an error, got: {}",
            miss.content
        );
    }

    /// A single armed schedule cannot tell "the removed row is gone" apart
    /// from "the table is empty" — two distinguishable labels can.
    #[test]
    fn schedule_remove_answer_omits_the_removed_row_but_keeps_the_other() {
        let mut session = granted_trunk();

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        session.run_shell(
            "call-2".to_string(),
            "exarch-schedules `add [trigger: `after '10m', label: 'daily', prompt: #'wake'#]",
            5,
            &emit,
        );

        let result = session.run_shell(
            "call-3".to_string(),
            "exarch-schedules `remove 'nightly'",
            5,
            &emit,
        );
        assert!(
            !result.content.contains("nightly"),
            "the removed row must be gone from the table answered afterwards, got: {}",
            result.content
        );
        assert!(
            result.content.contains("daily"),
            "the untouched row must still be in the table answered afterwards, got: {}",
            result.content
        );
    }

    // ── `reply` ───────────────────────────────────────────────────────────

    /// The record must reach the parent's inbox structured, not flattened
    /// to a string. The child's script does the replying, for the
    /// undriven-parent reason
    /// `agent_full_stack_round_trip_answers_the_roster_and_settles_into_inbox`
    /// gives.
    #[test]
    fn reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"let found = ["a.rs", "b.rs"]; exarch-agents `reply [files: $found]"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'find files'#, name: 'finder', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("live: 1"),
            "the summary must be the run's value and must count the child, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(_)) => break,
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        let read = session.run_shell(
            "call-2".to_string(),
            r"exarch-agents `read 'finder'",
            5,
            &emit,
        );
        assert!(
            read.content.contains("files:")
                && read.content.contains("a.rs")
                && read.content.contains("b.rs"),
            "the structured record must reach the parent through `exarch-agents `read`, got: {}",
            read.content
        );
    }

    /// The refusal is an ordinary call error, not a termination: a later,
    /// well-formed `` exarch-agents `reply `` still succeeds.
    #[test]
    fn reply_refuses_a_non_first_order_value_and_does_not_terminate() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `reply { echo hi }",
            5,
            &emit,
        );
        assert!(
            result.content.contains("first-order"),
            "must name the first-order rule, got: {}",
            result.content
        );

        let ok = session.run_shell("call-2".to_string(), r"exarch-agents `reply 42", 5, &emit);
        assert!(
            ok.content.contains("EXIT: 0"),
            "the session must still be usable after a refused reply, got: {}",
            ok.content
        );
    }

    #[test]
    fn double_reply_in_one_exchange_is_last_wins() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"exarch-agents `reply "first"; exarch-agents `reply "second""#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let provider_handle = session.current_provider();
        let outcome = session.deliberate(
            &provider_handle,
            Some("go".into()),
            None,
            &crate::agent::cancel::Token::new(),
            &emit,
        );
        match outcome {
            Ok(crate::agent::deliberate::Outcome::Replied(v)) => {
                assert_eq!(
                    v,
                    ral_core::serial::FOValue::String {
                        value: "second".into()
                    },
                    "the last reply in the exchange must win"
                );
            }
            other => panic!("expected Replied, got {other:?}"),
        }
    }

    // ── `exarch-pins` ────────────────────────────────────────────────────

    /// The scripted-provider round-trip pattern of
    /// `reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox`,
    /// crossed with the desk's `` `exarch-pins `read `` arm: the child pins
    /// with `` `set ``, reads its own pin back in the same run, and hands the
    /// canonical card to its parent.
    #[test]
    fn pin_read_full_stack_round_trip_returns_canonical_card_to_parent() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"exarch-pins `set [key: "note", body: `card ["hi there"]]; exarch-agents `reply !{exarch-pins `read "note"}"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'pin and read back'#, name: 'pinner', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("live: 1"),
            "the summary must be the run's value and must count the child, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    assert!(
                        matches!(r.outcome, crate::bus::AgentOutcome::Replied),
                        "the child must have replied, got: {:?}",
                        r.outcome
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        // The pretty-printer elides past its depth cap, so the span text
        // itself does not survive to this rendering; what proves the round
        // trip *canonical* — a lifted `` `text `` mark, not the bare-string
        // sugar it was authored with — does.
        let read = session.run_shell(
            "call-2".to_string(),
            r"exarch-agents `read 'pinner'",
            5,
            &emit,
        );
        assert!(
            read.content.contains("`card") && read.content.contains("`text [spans:"),
            "the canonical card must reach the parent, got: {}",
            read.content
        );
    }

    /// An absent key answers `Unit`, which crosses to the parent as `reply`'s
    /// empty rendering.
    #[test]
    fn pin_read_full_stack_absent_key_replies_unit() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"exarch-agents `reply !{exarch-pins `read "nope"}"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"exarch-agents `start [prompt: #'read an absent key'#, name: 'reader', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("live: 1"),
            "the summary must be the run's value and must count the child, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    assert!(
                        matches!(r.outcome, crate::bus::AgentOutcome::Replied),
                        "the child must have replied, got: {:?}",
                        r.outcome
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        let read = session.run_shell(
            "call-2".to_string(),
            r"exarch-agents `read 'reader'",
            5,
            &emit,
        );
        assert!(
            read.content.contains("reply: ()"),
            "an absent key must reply unit, got: {}",
            read.content
        );
    }

    // ── the task kit as a pure prelude over the pin family ─────────────────

    /// Every mutating tag reads and writes the "tasks" pin through `tasks-sync`,
    /// so `` exarch-tasks `list `` and a direct
    /// `` tasks-decode !{exarch-pins `read "tasks"} `` must agree on every
    /// field, tags and notes included.
    #[test]
    fn kit_round_trip_holds_every_field_including_tags_and_notes() {
        // Seven evals deep where this file's other tests run one or two, so a
        // debug build sharing the box with the rest of the suite needs more
        // room than the usual 5s: a call that times out here reads as a lost
        // field, not as a slow machine.
        const BUDGET: u64 = 60;

        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            r#"exarch-tasks `add "fix the parser""#,
            BUDGET,
            &emit,
        );
        session.run_shell(
            "call-2".to_string(),
            r#"exarch-tasks `add "write docs""#,
            BUDGET,
            &emit,
        );
        session.run_shell(
            "call-3".to_string(),
            "exarch-tasks `status [id: 1, status: `doing]",
            BUDGET,
            &emit,
        );
        session.run_shell(
            "call-4".to_string(),
            r#"exarch-tasks `tag [id: 1, tag: "urgent"]"#,
            BUDGET,
            &emit,
        );
        session.run_shell(
            "call-5".to_string(),
            r#"exarch-tasks `note [id: 1, note: "blocked on review"]"#,
            BUDGET,
            &emit,
        );

        let listed = session.run_shell("call-6".to_string(), "exarch-tasks `list", BUDGET, &emit);
        for field in ["fix the parser", "`doing", "urgent", "blocked on review"] {
            assert!(
                listed.content.contains(field),
                "exarch-tasks `list must show the tagged, noted task's {field}, got: {}",
                listed.content
            );
        }
        assert!(
            listed.content.contains("write docs"),
            "exarch-tasks `list must show the untouched second task, got: {}",
            listed.content
        );

        let read = session.run_shell(
            "call-7".to_string(),
            r#"let [decoded-task, _] = !{tasks-decode !{exarch-pins `read "tasks"}}
               echo $decoded-task[desc]
               echo $decoded-task[status]
               echo !{intercalate "," $decoded-task[tags]}
               echo $decoded-task[notes]"#,
            BUDGET,
            &emit,
        );
        assert!(
            read.content.contains("fix the parser"),
            "the decoded desc must survive, got: {}",
            read.content
        );
        assert!(
            read.content.contains("doing"),
            "the decoded status must survive, got: {}",
            read.content
        );
        assert!(
            read.content.contains("urgent"),
            "the decoded tags must survive, got: {}",
            read.content
        );
        assert!(
            read.content.contains("blocked on review"),
            "the decoded notes must survive, got: {}",
            read.content
        );
    }

    /// `` exarch-tasks `add `` inside a function body pins to the register, which SPEC
    /// §10's block-discard rule never touches — a later, separate top-level
    /// run still sees it.
    #[test]
    fn add_task_inside_a_function_body_survives_the_block_and_the_call() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            r#"let f = { exarch-tasks `add "inside a block" }; !{f}"#,
            5,
            &emit,
        );

        let listed = session.run_shell("call-2".to_string(), "exarch-tasks `list", 5, &emit);
        assert!(
            listed.content.contains("inside a block"),
            "a task added inside a function body must survive to the next top-level run, got: {}",
            listed.content
        );
    }

    /// A sub-agent's register is its own: a child's `` exarch-tasks `add `` must never
    /// reach the parent's "tasks" pin.
    #[test]
    fn sub_agent_pinning_tasks_leaves_the_parents_register_untouched() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            r#"exarch-tasks `add "parent task""#,
            5,
            &emit,
        );

        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"exarch-tasks `add "child task"; exarch-agents `reply "done""#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);

        let result = session.run_shell(
            "call-2".to_string(),
            r"exarch-agents `start [prompt: #'add a task'#, name: 'tasker', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("live: 1"),
            "the summary must be the run's value and must count the child, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(_)) => break,
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        let listed = session.run_shell("call-3".to_string(), "exarch-tasks `list", 5, &emit);
        assert!(
            listed.content.contains("parent task"),
            "the parent's own task must survive, got: {}",
            listed.content
        );
        assert!(
            !listed.content.contains("child task"),
            "the child's pin must never reach the parent's register, got: {}",
            listed.content
        );
    }

    /// `tasks-sync` clears the slot once no work remains: transitioning the
    /// last open task to `` `done `` empties the pin, and a later `` exarch-tasks `add ``
    /// finds no register and restarts id allocation at 1.
    #[test]
    fn transitioning_the_last_open_task_to_done_clears_the_pin_and_restarts_ids() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            r#"exarch-tasks `add "only task""#,
            5,
            &emit,
        );
        session.run_shell(
            "call-2".to_string(),
            "exarch-tasks `status [id: 1, status: `done]",
            5,
            &emit,
        );

        let read = session.run_shell(
            "call-3".to_string(),
            r#"exarch-pins `read "tasks""#,
            5,
            &emit,
        );
        assert!(
            !read.content.contains("VALUE:"),
            "an all-done list must clear the pin to unit, got: {}",
            read.content
        );

        session.run_shell(
            "call-4".to_string(),
            r#"exarch-tasks `add "fresh""#,
            5,
            &emit,
        );
        let listed = session.run_shell("call-5".to_string(), "exarch-tasks `list", 5, &emit);
        for field in ["id: 1", "fresh", "`open"] {
            assert!(
                listed.content.contains(field),
                "id allocation must restart at 1 once the register is empty, missing {field} in: {}",
                listed.content
            );
        }
    }

    /// A card under "tasks" that `tasks-decode` does not recognise — the
    /// model scribbled on the shared key — fails the next kit call with the
    /// didactic message naming the expected shape, rather than corrupting or
    /// silently discarding it.
    #[test]
    fn a_foreign_card_under_tasks_fails_the_kit_didactically() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            r#"exarch-pins `set [key: "tasks", body: `card [`text [spans: [[text: "not task shaped"]]]]]"#,
            5,
            &emit,
        );

        let result = session.run_shell("call-2".to_string(), r#"exarch-tasks `add "x""#, 5, &emit);
        assert!(
            result
                .content
                .contains("tasks: the card under the 'tasks' pin is not task-shaped"),
            "the didactic fail must name the expected shape, got: {}",
            result.content
        );
    }

    // ── context family door tests ────────────────────────────────────────

    /// A trunk holding one answered prompt, which every context test needs
    /// before it has anything addressable to name.
    fn trunk_with_an_answered_prompt() -> crate::agent::Avatar {
        let session = crate::agent::Avatar::for_test("system").unwrap();
        crate::agent::testkit::close_exchange(&session, "first prompt", "first answer");
        session
    }

    /// `scheme_context`'s outer tag row is open, so `` exarch-context `rewind `` — the
    /// tag a model most plausibly invents — reaches the door naming the two
    /// legal ones rather than dying as a row-unification mismatch.
    #[test]
    fn unknown_context_tag_reaches_the_door_naming_every_legal_tag() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result =
            session.run_shell("call-1".to_string(), "exarch-context `rewind [3]", 5, &emit);
        for tag in ["survey", "evict"] {
            assert!(
                result.content.contains(tag),
                "must name `{tag}, got: {}",
                result.content
            );
        }
    }

    /// `` `evict ``'s record row is open only on the tail, so `turns` itself
    /// is still static: a misspelling reaches the type error, not the door.
    #[test]
    fn misspelled_evict_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-context `evict [turn: [1]]",
            5,
            &emit,
        );
        assert!(
            !result.content.contains("EXIT: 0") && result.content.contains("turns"),
            "the diagnostic must name the field the row demands, got: {}",
            result.content
        );
    }

    /// The address is the edit, so a spec carrying only a note never reaches
    /// the host. `scheme_context` anchors `turns`, so no program written in
    /// ral gets this far — the door is the second line of defence, checked
    /// here where it can be reached at all.
    #[test]
    fn an_eviction_naming_no_turns_is_refused_at_the_door() {
        let refusal = context_evict_payload(&Value::map(vec![(
            "note".to_string(),
            Value::String("nothing to say".to_string()),
        )]))
        .expect_err("an eviction must name the turns it takes");
        let ral_core::types::Break::Error(error) = refusal else {
            panic!("a door refusal is catchable, never an escape")
        };
        assert_eq!(
            error.message,
            "exarch-context `evict: the spec record needs a `turns` field — the turns to evict; \
             `!{range a b}` builds a run"
        );
    }

    /// The whole family through the real shell: an edit answers the survey
    /// the transition leaves behind, so the count of what left is there to
    /// read without a second call. The address names turns — 1 and 2 are the
    /// first prompt and its reply — and the optional `note` rides the
    /// open record row, so both shapes type-check.
    #[test]
    fn an_eviction_answers_the_survey_it_leaves_behind() {
        let mut session = trunk_with_an_answered_prompt();
        crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-context `evict [turns: [1, 2], note: 'the old work is done']",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid eviction must succeed, got: {}",
            result.content
        );
        assert!(
            result.content.contains("total-bytes"),
            "the answer must be the survey afterwards, got: {}",
            result.content
        );
        assert!(
            !result.content.contains("bytes-delta"),
            "the edit answers the state, never a receipt for the transition, got: {}",
            result.content
        );
    }

    /// The same tag with no `note`: the open record row admits it, so the
    /// harness never asks the model for an empty string.
    #[test]
    fn an_eviction_without_a_note_type_checks() {
        let mut session = trunk_with_an_answered_prompt();
        crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-context `evict [turns: !{range 1 3}]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "an eviction with no note must succeed, got: {}",
            result.content
        );
    }

    /// `` `read ``'s answer is a list, so a slice is `$read[0]`, naming its
    /// own `turn` and the `role` it bears, and its messages are ral
    /// records with variant parts rather than a rendered string.
    #[test]
    fn transcript_answers_turn_records_with_variant_parts() {
        let mut session = trunk_with_an_answered_prompt();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r#"let material = exarch-transcript `read [turns: [1, 2]]
               echo !{length $material}
               echo $material[0][turn] $material[0][role]
               let msgs = $material[0][messages]
               let say-role = { |r| case $r [
                 `system: { |_| echo "role=system" },
                 `user: { |_| echo "role=user" },
                 `assistant: { |_| echo "role=assistant" },
                 `tool: { |_| echo "role=tool" },
               ] }
               let say-part = { |p| case $p [
                 `text: { |[content: c]| echo "text=$c" },
                 `program: { |_| echo "part=program" },
                 `result: { |_| echo "part=result" },
                 `reasoning: { |_| echo "part=reasoning" },
                 `binary: { |_| echo "part=binary" },
                 `custom: { |_| echo "part=custom" },
               ] }
               say-role $msgs[0][role]
               say-part $msgs[0][parts][0]
               let reply = $material[1][messages]
               say-role $reply[0][role]
               say-part $reply[0][parts][0]"#,
            5,
            &emit,
        );
        assert!(
            result.content.contains("\n2\n"),
            "two named turns, one record each, got: {}",
            result.content
        );
        assert!(
            result.content.contains("1 user"),
            "the first record names its own turn and its role, got: {}",
            result.content
        );
        assert!(
            result.content.contains("role=user") && result.content.contains("text=first prompt"),
            "the user turn must be a `text part of a `user message, got: {}",
            result.content
        );
        assert!(
            result.content.contains("role=assistant")
                && result.content.contains("text=first answer"),
            "the assistant turn must be a `text part of an `assistant message, got: {}",
            result.content
        );
    }

    /// The door speaks turn ids, so an address built with `range` and one
    /// written out read the same turns, each record naming the role it bears
    /// — a set that spans a prompt boundary answers one record per turn.
    #[test]
    fn transcript_read_addresses_turns_wherever_they_lie() {
        let mut session = trunk_with_an_answered_prompt();
        crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"let spanning = exarch-transcript `read [turns: [2, 3]]
               echo !{length $spanning}
               echo $spanning[0][turn] $spanning[0][role] !{length $spanning[0][messages]}
               echo $spanning[1][turn] $spanning[1][role]
               let built = exarch-transcript `read [turns: !{range 3 5}]
               echo !{length $built} $built[1][turn]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("\n2\n"),
            "two named turns answer one record each, got: {}",
            result.content
        );
        assert!(
            result.content.contains("2 assistant 1"),
            "turn 2 is an assistant turn holding one message, got: {}",
            result.content
        );
        assert!(
            result.content.contains("3 user"),
            "turn 3 is the prompt after it, got: {}",
            result.content
        );
        assert!(
            result.content.contains("2 4"),
            "`range 3 5` is turns 3 and 4, the second of them turn 4, got: {}",
            result.content
        );
    }

    /// `scheme_transcript`'s outer tag row is open, so `` exarch-transcript `search ``
    /// — the tag a model most plausibly invents — reaches the door naming the
    /// three legal ones.
    #[test]
    fn unknown_transcript_tag_reaches_the_door_naming_every_legal_tag() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "exarch-transcript `search 'x'",
            5,
            &emit,
        );
        for tag in ["index", "read", "grep"] {
            assert!(
                result.content.contains(tag),
                "must name `{tag}, got: {}",
                result.content
            );
        }
    }

    /// The answer type is free, so each tag's own shape has to type-check
    /// against the use the program makes of it: an index row carries `id` and
    /// `held`, and a grep answer projects `hits` and `total` as a record, each
    /// hit naming the turn it lies in. The optional `turns` rides `` `grep ``'s
    /// open record row.
    #[test]
    fn index_and_grep_answer_their_own_shapes() {
        let mut session = trunk_with_an_answered_prompt();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            // Bindings are named, not lettered: ral keeps value and command
            // names disjoint, so a one-letter binding fails on any host with
            // that letter on PATH (plan9port ships a `g`).
            r"let listed = exarch-transcript `index
               echo $listed[0][id] $listed[0][role] $listed[0][held]
               let matched = exarch-transcript `grep [pattern: 'first (prompt|answer)']
               echo !{length $matched[hits]} $matched[total]
               echo $matched[hits][0][turn] $matched[hits][1][turn]
               let narrowed = exarch-transcript `grep [pattern: 'first', turns: [2]]
               echo $narrowed[total]
               let missed = exarch-transcript `grep [pattern: 'nothing here', turns: [1, 2]]
               echo $missed[total]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("1 user resident"),
            "the first turn is listed and still in the context, got: {}",
            result.content
        );
        assert!(
            result.content.contains("2 2"),
            "both of its turns match, and `total` counts them all, got: {}",
            result.content
        );
        assert!(
            result.content.contains("1 2"),
            "the hits name the turns they lie in, got: {}",
            result.content
        );
        assert!(
            result.content.contains("\n1\n"),
            "an address narrows the search to the assistant turn alone, got: {}",
            result.content
        );
        assert!(
            result.content.contains("\n0\n"),
            "a pattern that matches nothing answers no hits, got: {}",
            result.content
        );
    }
}
