//! The harness builtins — `agents`, `schedules`, `pin-read`, `pin-list`,
//! `context`, `transcript` — with the type schemes that gate them. A
//! returning agent's reply is a tag of `agents` (`` `reply ``), not a builtin
//! of its own — the fleet is one family.
//!
//! Each body validates at the door before it enquires, so a malformed call
//! never reaches the host. `agents`'s `` `start `` tag forks this shell and
//! tells the host how to reach the fork, which is what the run's
//! [`Fork`](ral_core::types::Fork) door says: an in-process host adopts a
//! fork parked in the run's nursery, since the reentrancy law bars a desk
//! handler from holding `&mut Shell` to fork one itself; a host across a wire
//! is handed a guest port to dial, and dials it while it answers.
//! [`crate::fleet::desk::ExarchDesk`] answers every enquiry on the other side.
//!
//! One verb per addressable thing, named as the model names it: `agents`,
//! `schedules`, `context` and `transcript` each carry the model's tag as a
//! nested variant and its record verbatim, and the tag selects what happens.
//! The first three name a state, and answer it afterwards. `transcript`
//! names the store instead, which no tag of it writes, so its tags answer
//! what they were asked for rather than a state.

use crate::fleet::desk::Selection;
use crate::fleet::schedule::{CronSchedule, parse_duration};
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{closed_record, fun, mk_scheme as scheme, pure, thunk};
use ral_core::typecheck::{Row, RowVar, Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Fork, Mooring, Settled, sig};
use ral_core::{Shell, Value};
use std::borrow::Cow;

/// The bases a spawn's `grant` may name — a subset of what
/// `crate::policy::base::resolve_base` offers a launching human, and kept in
/// step by hand.
///
/// Each admits the bundled coreutils (`ral_core::uutils`). Those spawn by bare
/// name, so a base that states its exec admissions as directory prefixes alone
/// denies every one of them, and a child that cannot run `ls` cannot widen its
/// own grant to get it back.
const PERMISSION_LABELS: [&str; 5] = [
    "confined",
    "read-only",
    "edit-only",
    "reasonable",
    "dangerous",
];

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
        "agents: `type` must be `amnemon (blank context) or `mnemon (inherits your conversation) — got {v}"
    )))
}

/// Check a `grant`, closing the row [`scheme_agents`] leaves open so the
/// error can enumerate every legal label.
fn permission_label(v: &Value) -> Settled<()> {
    if bare_tag(v).is_some_and(|label| PERMISSION_LABELS.contains(&label)) {
        return Ok(());
    }
    Err(sig(format!(
        "grant must be one of `confined, `read-only, `edit-only, `reasonable, `dangerous — got {v}"
    )))
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
                "agents: `{field}`'s `named` must carry a non-empty Str naming the \
                 {field} — got {other}"
            ))),
        },
        other => Err(sig(format!(
            "agents: `{field}` must be `inherit (whatever you are running on) or \
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
            "schedules: trigger must be `cron '<5-field-cron-expr>'` or `after '<n><unit>'`, got {v}"
        )));
    };
    let Value::String(expr) = payload.as_ref() else {
        return Err(sig(format!(
            "schedules: `{label}`'s payload must be a Str, got {}",
            payload.type_name()
        )));
    };
    match label.as_str() {
        "cron" => CronSchedule::parse(expr)
            .map(|_| ())
            .map_err(|e| sig(format!("schedules: {e}"))),
        "after" => parse_duration(expr)
            .map(|_| ())
            .map_err(|e| sig(format!("schedules: {e}"))),
        other => Err(sig(format!(
            "schedules: trigger must be `cron '<5-field-cron-expr>'` or `after '<n><unit>'`, got `{other}`"
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
        "schedules: `label` must be a Str naming the wakeup, got {}",
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

/// The `agents` family's answer: `` `roster [rows] ``, whichever tag was
/// sent. Every tag but `` `read `` answers this way — `` `read `` answers the
/// fetched record instead, so [`builtin_agents`] never routes it here.
fn roster(answer: FOValue) -> Settled<Value> {
    let FOValue::Variant {
        label,
        payload: Some(payload),
    } = answer
    else {
        return Err(sig(
            "agents: host answered an unexpected shape for the roster",
        ));
    };
    if label != "roster" {
        return Err(sig(format!(
            "agents: host answered `{label} where `roster was expected — every tag in this family \
             answers the roster"
        )));
    }
    let FOValue::List { items } = *payload else {
        return Err(sig(
            "agents: host's `roster answer must carry a list of agent rows",
        ));
    };
    Ok(Value::list(items.into_iter().map(Value::from).collect()))
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
    grant: String,
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<FOValue> {
    let token = mint_token();
    let (socket, port) = super::guest_port::bind().map_err(|why| sig(format!("agents: {why}")))?;
    let listener = ral_core::hatch::listen_for_hatch(socket, token, &shell.fork_scrubbed(), grant)
        .map_err(|reason| sig(format!("agents: {reason}")))?;
    let answer = shell.enquire(mooring, start_request(spec, listening(port, token)));
    // A host that refused never dialled, so the thread is still in its poll:
    // wake it, or the join below never returns.
    if answer.is_err() {
        listener.cancel();
    }
    match listener.join() {
        Err(ral_core::hatch::Unhatched::Failed(reason)) => Err(sig(format!("agents: {reason}"))),
        _ => Ok(answer?),
    }
}

/// The dial this arm waits for means nothing outside a Linux guest, so a wire
/// trunk built on any other platform refuses here rather than at a silent
/// no-op.
#[cfg(not(target_os = "linux"))]
fn hatch_over_the_wire(
    _spec: FOValue,
    _grant: String,
    _mooring: &Mooring,
    _shell: &Shell,
) -> Settled<FOValue> {
    Err(sig(
        "agents: this engine has no hatch support outside a Linux guest — a wire trunk's helper \
         spawn only ever reaches one",
    ))
}

/// `` `start ``'s payload: validate, fork this shell, and enquire
/// `` agents `start `` with the model's record and the fork tag this run's
/// [`Fork`] door calls for; the desk's `launch` is the other half.
///
/// [`scheme_agents`]'s closed record row inside `` `start `` already
/// guarantees the seven fields, so the `else` arms below are unreachable
/// through the type checker; they stay didactic rather than trust it alone.
fn start_agent(spec: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::Map(fields) = spec else {
        return Err(sig(format!(
            "agents: `start`'s payload must be a [prompt: …, name: …, type: …, grant: …, search: …, provider: …, model: …] record, got {}",
            spec.type_name()
        )));
    };
    if fields.get("prompt").is_none() {
        return Err(sig(
            "agents: the spec record needs a `prompt` field — the instruction the child starts with",
        ));
    }
    let Some(name) = fields.get("name") else {
        return Err(sig(
            "agents: the spec record needs a `name` field — the child's identity",
        ));
    };
    let Some(kind) = fields.get("type") else {
        return Err(sig(
            "agents: the spec record needs a `type` field — `amnemon or `mnemon",
        ));
    };
    let Some(grant) = fields.get("grant") else {
        return Err(sig(
            "agents: the spec record needs a `grant` field — one of the five permission bases",
        ));
    };
    let Some(search) = fields.get("search") else {
        return Err(sig(
            "agents: the spec record needs a `search` field — whether the child may use the \
             provider's built-in web search",
        ));
    };
    let Some(provider) = fields.get("provider") else {
        return Err(sig(
            "agents: the spec record needs a `provider` field — `inherit to run the child on \
             your own account, or `named '<provider>'",
        ));
    };
    let Some(model) = fields.get("model") else {
        return Err(sig(
            "agents: the spec record needs a `model` field — `inherit to run the child on your \
             own model, or `named '<model>'",
        ));
    };

    let name = name.to_string();
    // The door's own early refusal; `Fleet::enrol` is what makes it
    // unskippable.
    crate::fleet::check_name(&name).map_err(|why| sig(format!("agents: {why}")))?;
    agent_type_label(kind)?;
    permission_label(grant)?;
    // The door admitted it, so the grant is a bare tag; the hatch needs its
    // label to narrow the child guest-side.
    let grant = bare_tag(grant).unwrap_or_default().to_string();
    if !matches!(search, Value::Bool(_)) {
        return Err(sig(format!(
            "agents: `search` must be a Bool — got {}",
            search.type_name()
        )));
    }
    selection_label(provider, "provider")?;
    selection_label(model, "model")?;
    let spec = verbatim(spec, "agents")?;

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

/// `` `message ``'s payload: enquires `` agents `message `` with the model's
/// record; name resolution, descendant-scoping, and delivery errors all
/// belong to the desk.
fn message_agent(spec: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::Map(fields) = spec else {
        return Err(sig(format!(
            "agents: `message`'s payload must be a [to: …, text: …] record, got {}",
            spec.type_name()
        )));
    };
    let Some(to) = fields.get("to") else {
        return Err(sig(
            "agents: the `message` spec needs a `to` field — the descendant's name",
        ));
    };
    let Some(text) = fields.get("text") else {
        return Err(sig(
            "agents: the `message` spec needs a `text` field — what to send",
        ));
    };
    if !matches!(to, Value::String(_)) {
        return Err(sig(format!(
            "agents: `to` must be a Str naming the descendant, got {}",
            to.type_name()
        )));
    }
    if !matches!(text, Value::String(_)) {
        return Err(sig(format!(
            "agents: `text` must be a Str, got {}",
            text.type_name()
        )));
    }
    Ok(shell.enquire(
        mooring,
        request("agents", "message", Some(verbatim(spec, "agents")?)),
    )?)
}

/// `` `reply ``'s payload: the value crosses to whoever spawned this agent as
/// plain data, so it is checked first-order at this door exactly as the old
/// standalone `reply` builtin checked it — the desk's `!returns` refusal is
/// the only other reason this tag can fail.
fn reply_agent(value: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let payload = FOValue::try_from(value).map_err(|_| {
        sig(
            "agents: `reply`'s value must be first-order data — no closures, handles, or \
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
            "agents: `read`'s payload must be a Str naming the descendant, got {}",
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

/// `agents <tag>` — one enquiry per tag. Every tag but `` `read `` answers
/// with the roster; `` `read `` answers the fetched record instead, so it
/// returns directly rather than falling through to [`roster`].
fn builtin_agents(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "agents: expected a `list, `start, `message, `cancel, `reply, or `read tag, got {}",
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
                    "agents: `cancel`'s payload must be a Str naming the descendant, got {}",
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
                "agents: tag must be one of `list, `start, `message, `cancel, `reply, `read — got \
                 {label}"
            )));
        }
    };
    roster(answer)
}

/// `` `add ``'s payload: checked through
/// [`schedule_trigger`]/[`schedule_label`], then enquired verbatim as
/// `` schedules `add ``. The self-wakeup grant and label uniqueness are
/// refusals the desk and the schedule registry own.
fn add_schedule(spec: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::Map(fields) = spec else {
        return Err(sig(format!(
            "schedules: `add`'s payload must be a [trigger: …, label: …, prompt: …] record, got {}",
            spec.type_name()
        )));
    };
    let Some(trigger) = fields.get("trigger") else {
        return Err(sig(
            "schedules: the `add` spec needs a `trigger` field — `cron '<expr>' or `after '<dur>'",
        ));
    };
    let Some(label) = fields.get("label") else {
        return Err(sig(
            "schedules: the `add` spec needs a `label` field — a Str naming the wakeup",
        ));
    };
    if fields.get("prompt").is_none() {
        return Err(sig(
            "schedules: the `add` spec needs a `prompt` field — the instruction delivered when the wakeup fires",
        ));
    }
    schedule_trigger(trigger)?;
    schedule_label(label)?;

    Ok(shell.enquire(
        mooring,
        request("schedules", "add", Some(verbatim(spec, "schedules")?)),
    )?)
}

/// `schedules <tag>` — one enquiry, whose answer is the registry itself:
/// every tag answers with the table, never a receipt of its own. The
/// self-wakeup grant refusal is the desk's.
fn builtin_schedules(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "schedules: expected a `list, `add, or `remove tag, got {}",
            args[0].type_name()
        )));
    };
    let answer = match (label.as_str(), payload) {
        ("list", None) => shell.enquire(mooring, request("schedules", "list", None))?,
        ("add", Some(spec)) => add_schedule(spec, mooring, shell)?,
        ("remove", Some(target)) => {
            let Value::String(target_label) = target.as_ref() else {
                return Err(sig(format!(
                    "schedules: `remove`'s payload must be a Str naming the wakeup, got {}",
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
                "schedules: tag must be one of `list, `add, `remove — got {label}"
            )));
        }
    };
    let FOValue::List { items } = answer else {
        return Err(sig(
            "schedules: host answered an unexpected shape for the listing",
        ));
    };
    Ok(Value::list(items.into_iter().map(Value::from).collect()))
}

/// `pin-read <key>` — enquires `` `pin-read ``; the mirror lookup, the miss
/// (→ `Unit`), and the canonical re-encoding are the desk's.
fn builtin_pin_read(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let key = args[0].to_string();
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "pin-read".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: key }],
            })),
        },
    )?;
    Ok(Value::from(answer))
}

/// `pin-list` — enquires `` `pin-list ``; the key ordering is the desk's.
fn builtin_pin_list(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "pin-list".to_string(),
            payload: None,
        },
    )?;
    let FOValue::List { items } = answer else {
        return Err(sig(
            "pin-list: host answered an unexpected shape for the listing",
        ));
    };
    Ok(Value::list(items.into_iter().map(Value::from).collect()))
}

/// The `` `context `` scheme (`context_receipt_ty`) is a closed record, so a
/// survey missing any of its fields is host-side drift, not a call error —
/// name what is missing rather than shrugging at the whole shape.
fn context_receipt(answer: FOValue) -> Settled<Value> {
    const FIELDS: [&str; 4] = ["spans", "evicted", "total-bytes", "total-steps"];
    let FOValue::Map { entries } = &answer else {
        return Err(sig(
            "context: host answered an unexpected shape for the survey",
        ));
    };
    if let Some(missing) = FIELDS
        .iter()
        .find(|field| !entries.iter().any(|(key, _)| key == *field))
    {
        return Err(sig(format!(
            "context: host answered a survey missing the `{missing}` field"
        )));
    }
    Ok(Value::from(answer))
}

pub(crate) fn context_exchanges_payload(value: &Value, verb: &str) -> Settled<FOValue> {
    let Value::List(items) = value else {
        return Err(sig(format!(
            "{verb}: expected a List of non-negative exchange Ints, got {}",
            value.type_name()
        )));
    };
    let mut exchanges = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Value::Int(exchange) = item else {
            return Err(sig(format!(
                "{verb}: exchange at index {index} must be an Int, got {}",
                item.type_name()
            )));
        };
        if *exchange < 0 {
            return Err(sig(format!(
                "{verb}: exchange at index {index} must be non-negative, got {exchange}"
            )));
        }
        exchanges.push(FOValue::Int { value: *exchange });
    }
    Ok(FOValue::List { items: exchanges })
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
    const VERB: &str = "context `evict";
    let Value::Map(spec) = value else {
        return Err(sig(format!(
            "{VERB}: expected [through: Int] or [through: Int, note: Str], got {}",
            value.type_name()
        )));
    };
    let Some(through) = spec.get("through") else {
        return Err(sig(format!(
            "{VERB}: the spec record needs a `through` field — the last exchange to evict"
        )));
    };
    let Value::Int(through) = through else {
        return Err(sig(format!(
            "{VERB}: `through` must be an Int, got {}",
            through.type_name()
        )));
    };
    if *through < 0 {
        return Err(sig(format!(
            "{VERB}: `through` must be non-negative, got {through}"
        )));
    }
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

/// `context <tag>` — one enquiry, whose answer is the model view itself:
/// `` `survey `` describes it, `` `drop `` and `` `evict `` edit it, and every
/// tag answers the survey the transition leaves behind. The admissibility of
/// an edit — live, unknown, already gone, empty — is the desk's.
fn builtin_context(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "context: expected a `survey, `drop, or `evict tag, got {}",
            args[0].type_name()
        )));
    };
    let request = match (label.as_str(), payload) {
        ("survey", None) => request("context", "survey", None),
        ("drop", Some(exchanges)) => request(
            "context",
            "drop",
            Some(context_exchanges_payload(exchanges, "context `drop")?),
        ),
        ("evict", Some(spec)) => request("context", "evict", Some(context_evict_payload(spec)?)),
        _ => {
            return Err(sig(format!(
                "context: tag must be one of `survey, `drop, `evict — got {label}"
            )));
        }
    };
    context_receipt(shell.enquire(mooring, request)?)
}

/// `transcript <tag>` — one enquiry onto the store: `` `index `` lists every
/// closed exchange, `` `read `` returns the named ones as material, and
/// `` `grep `` searches them. Each tag answers its own shape, so the answer
/// is checked per tag rather than once.
fn builtin_transcript(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Variant { label, payload } = &args[0] else {
        return Err(sig(format!(
            "transcript: expected an `index, `read, or `grep tag, got {}",
            args[0].type_name()
        )));
    };
    let (request, listed) = match (label.as_str(), payload) {
        ("index", None) => (request("transcript", "index", None), true),
        ("read", Some(exchanges)) => (
            request(
                "transcript",
                "read",
                Some(context_exchanges_payload(exchanges, "transcript `read")?),
            ),
            true,
        ),
        ("grep", Some(spec)) => (
            request("transcript", "grep", Some(transcript_grep_payload(spec)?)),
            false,
        ),
        _ => {
            return Err(sig(format!(
                "transcript: tag must be one of `index, `read, `grep — got {label}"
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
            "transcript: host answered an unexpected shape for `{label}"
        )));
    }
    Ok(Value::from(answer))
}

/// `` `grep ``'s spec, checked field by field and then sent verbatim. The
/// scheme leaves the record open on `exchanges`, since a closed row cannot
/// express an optional field, so the door is where a narrowing of the wrong
/// type is caught; the pattern itself is the desk's to compile.
pub(crate) fn transcript_grep_payload(value: &Value) -> Settled<FOValue> {
    const VERB: &str = "transcript `grep";
    let Value::Map(spec) = value else {
        return Err(sig(format!(
            "{VERB}: expected [pattern: Str] or [pattern: Str, exchanges: [Int]], got {}",
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
    if let Some(exchanges) = spec.get("exchanges") {
        let _ = context_exchanges_payload(exchanges, VERB)?;
    }
    verbatim(value, VERB)
}

/// A variant over a row of tags with stated payloads, ending in `tail`.
fn variant_row(tags: &[(&str, Ty)], tail: Row) -> Ty {
    use ral_core::syntax::tag::tag_row_label;
    let mut row = tail;
    for (label, ty) in tags.iter().rev() {
        row = Row::Extend(tag_row_label(label), Box::new(ty.clone()), Box::new(row));
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
        row = Row::Extend((*label).to_string(), Box::new(ty.clone()), Box::new(row));
    }
    Ty::Record(row)
}

/// `agents :: ∀α β ρ1 ρ2 ρ3 ρ4 ρ5. <list | start [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, search: Bool, provider: Variant ρ3, model: Variant ρ4] | message [to: Str, text: Str] | cancel Str | reply β | read Str | ρ5> → F α`
///
/// The outer tag row is open (`ρ5`) so an unrecognised tag reaches the
/// runtime door that names the six legal ones, rather than dying as a
/// row-unification mismatch.
///
/// The answer is no longer one fixed shape: every tag but `` `read `` still
/// answers the roster `[[name, state, idle-s, elapsed-s, log-dir]]`, but
/// `` `read `` answers the value a descendant handed up, whose shape this
/// call cannot know — so `α` is left free rather than fixed to the roster's
/// list type. This is the `pin-read`/`from-json` move
/// ([`scheme_pin_read`]): trusted, not checked, since only [`roster`]'s
/// runtime door can tell the two apart.
///
/// `start`'s and `message`'s record rows are closed because a record
/// literal with literal keys infers an exact one (`infer_map_val` builds on
/// `Row::Empty`), so a missing or misspelled field is a static error naming
/// it. The `type`, `grant`, `provider` and `model` rows *inside* `start` stay
/// open, because a literal tag infers its own open row: closing them would
/// make `` `bogus `` a bare row-mismatch diagnostic that never reaches
/// [`agent_type_label`]/[`permission_label`]/[`selection_label`], which
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

/// `schedules :: ∀ρ1 ρ2. <list | add [trigger: Variant ρ1, label: Str, prompt: Str] | remove Str | ρ2> → F [[label: Str, trigger: Str, next-s: Int, fires: Int]]`
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

/// `pin-read :: ∀α. String → F α` — the `from-json` precedent
/// ([`ral_core::typecheck::builtins::scheme::from_json`]): trusted, not
/// checked, since only the kit's own decoder can judge whether the card
/// read back matches the shape it expects.
fn scheme_pin_read(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    scheme(&[av], &[], &[], thunk(fun(Ty::String, pure(Ty::Var(av)))))
}

/// `pin-list :: F [String]`
fn scheme_pin_list(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(pure(Ty::List(Box::new(Ty::String)))))
}

fn context_span_ty() -> Ty {
    closed_record(&[
        ("exchange", Ty::Int),
        ("kind", Ty::String),
        ("prompt", Ty::String),
        ("bytes", Ty::Int),
        ("steps", Ty::Int),
        ("live", Ty::Bool),
    ])
}

fn context_receipt_ty() -> Ty {
    closed_record(&[
        ("spans", Ty::List(Box::new(context_span_ty()))),
        ("evicted", Ty::Int),
        ("total-bytes", Ty::Int),
        ("total-steps", Ty::Int),
    ])
}

/// `context :: ∀ρ1 ρ2. <survey | drop [Int] | evict [through: Int | ρ1] | ρ2> → F [spans: [[exchange: Int, kind: Str, prompt: Str, bytes: Int, steps: Int, live: Bool]], evicted: Int, total-bytes: Int, total-steps: Int]`
///
/// Same shape as [`scheme_agents`] and [`scheme_schedules`]: an open outer
/// tag row so an unknown tag reaches the door naming the three legal ones.
/// `drop`'s payload is a bare `[Int]`, as `` `cancel ``'s is a bare `Str` —
/// a list of exchange numbers has no shape left to name.
///
/// `evict`'s record row is open on `ρ1` because `note` is optional and a
/// closed row cannot say so; [`context_evict_payload`] refuses a `note` of
/// the wrong type, and the desk an empty, oversized, or multi-line one — a
/// required `note` would invite `''`, and a marker reading
/// `Your note at eviction: ""` is a defect.
///
/// One answer for all three tags: an edit changes what is addressable, so
/// the survey the transition leaves behind is what the next edit must be
/// written against.
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
                    ("drop", Ty::List(Box::new(Ty::Int))),
                    ("evict", open_record(&[("through", Ty::Int)], evict_row)),
                ],
                tag_row,
            ),
            pure(context_receipt_ty()),
        )),
    )
}

/// `transcript :: ∀α ρ1 ρ2. <index | read [Int] | grep [pattern: Str | ρ1] | ρ2> → F α`
///
/// The outer tag row is open (`ρ2`) so an unrecognised tag reaches the
/// runtime door that names the three legal ones, rather than dying as a
/// row-unification mismatch.
///
/// Each tag answers its own shape — a listing, one span record per named
/// exchange, a hit table — so `α` is left free rather than fixed to any one
/// of them, exactly as [`scheme_agents`] leaves it free for `` `read ``.
/// The answer's shape is then the door's to check and the docstring's to
/// state.
///
/// `grep`'s record row is open on `ρ1` because `exchanges` is optional and a
/// closed row cannot say so; [`transcript_grep_payload`] refuses one of the
/// wrong type.
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
                    ("read", Ty::List(Box::new(Ty::Int))),
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
static HARNESS_BUILTINS_ARR: [BuiltinEntry; 6] = [
    BuiltinEntry::new(
        Cow::Borrowed("agents"),
        scheme_agents,
        "agents <tag>  — the fleet: `list what is live, `start a child, `message one, `cancel one, `reply to hand your own value up, `read one back off a descendant. Every tag but `read answers with the roster afterwards, [[name: Str, state: `busy|`waiting-on-agents|`replied|`waiting, idle-s: Int, elapsed-s: Int, log-dir: Str]], so what you read back is always what is live now rather than a receipt for what you just did.\n\nagents `list  — your live descendants at any depth, oldest first. `state` is `busy while working, `waiting-on-agents while held only by a busy child of its own, `replied once it has called `reply and parked, `waiting once a human has engaged it and it parked with no reply. `idle-s` is seconds since it parked — zero while `busy` or `waiting-on-agents. A settled agent (cancelled, failed, or reaped past its hour) is not listed. This is how you recover names after an eviction.\n\nagents `start [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>, provider: `inherit|`named <Str>, model: `inherit|`named <Str>]  — launch a sub-agent. Launch-only and always asynchronous: the child's reply is NOT this call's result — it arrives later, as a one-line notice in your inbox, and you fetch the value with `read. The answer's roster carries the child's row, and that row's name and log-dir are its receipt. `type` selects the child's memory: `amnemon` starts blank (no shared history), while `mnemon` inherits your current model-visible conversation. A `mnemon` child left on your own selection reuses your provider's cache; one sent to another account or model is still sound — reasoning crosses as plain text, not as signed blocks — but forfeits that locality, so pay for it deliberately. Every child receives the value-snapshot of the parent's bindings, cwd, and env — `mnemon` too; the serializable fragment crosses, while a live job handle becomes an opaque placeholder. `prompt` is a computed string and becomes the child's fresh final prompt. Keep large material in a named binding rather than splicing it into prompt; small, certainly-needed material may still be spliced. Wrap `prompt` in a raw string #'…'# if it carries $, !, or quotes. `name` is the child's identity — non-empty, at most 24 characters, ASCII letters/digits/-/_ only — and must not be borne by any live agent, or the call is refused; pick something descriptive, like 'fix-parser-tests'. `grant` bounds the child to at most your own authority and must be exactly one of `confined (offline, no home reads), `read-only (writes only to scratch), `edit-only (edits the working tree, no build tooling), `reasonable (everyday tooling), `dangerous (no narrowing); any other label is refused, naming all five. `search` states whether the child may use the provider's own built-in web search, bounded above by your own — asking for it when you do not have it silently yields a child without it. `provider` and `model` say what the child runs on, and both are always written — there is no omitting them, and `inherit is how you say you have no opinion. `provider: `inherit, model: `inherit` shares your own provider outright and is the plain default. `provider: `inherit, model: `named '<model>'` keeps your account and credential and changes only the model — the way to spend a cheaper, faster model on a narrow child while you keep a stronger one for yourself. `provider: `named '<provider>', model: `inherit` moves the child to another signed-in account: your own model if that account is the one you are on, otherwise that account's default model, and the call is refused naming `model` if it publishes none. `provider: `named …, model: `named …` says both outright. A provider name that no signed-in account answers to, or that several answer to, is refused naming the accounts you have; pick from those. Effort, temperature, and output cap are the operator's knobs rather than part of a model's identity, so they carry across whatever you name. Delegation depth is finite — each descendant is handed one less unit of fuel than its spawner holds, and once fuel reaches zero this call is refused; fuel bounds how deep a chain may recurse, never how many children you may start at any one depth.\n\nagents `message [to: <Str>, text: <Str>]  — send `text` as a marked item to the live descendant named `to`; it lands at that child's next exchange boundary, not as human input, and wakes a `replied or `waiting child into a fresh exchange. Only a descendant of yours may receive it — never a sibling, an ancestor, or yourself; refused otherwise. It does not return the recipient's answer: this is coordination, not a call. Nothing in the roster changes, so the answer is the plain confirmation that the recipient was live when you sent.\n\nagents `cancel <name>  — ask the live descendant named `name` to stop. It stops at its next checkpoint and then delivers a cancelled result to your inbox. Only a descendant of yours may be cancelled — never a sibling, an ancestor, or yourself; refused otherwise. A cancel is a request, not a transaction: the child is still running when this answers, so its row is still in the roster you get back. A name you still see listed is NOT a failed cancel — do not fire it again; read `list later and find it gone.\n\nagents `reply <value>  — hand `value` back to whoever spawned you. Your parent receives exactly this value, nothing else — not your reasoning, your shell bindings, or any prose you streamed along the way. `value` must be first-order data: no closures, handles, or environments; passing one fails this call with a didactic error and your run continues, so fix the value and call `reply again. Call it more than once in an exchange and the last call wins — an earlier value is discarded, not appended. It does not end your run: you park (`state `replied) rather than settle, and may be `message`d for a follow-up — answer that with another `reply. A non-finite Float (NaN, +Infinity, -Infinity) reaches your parent as the string \"NaN\"/\"Infinity\"/\"-Infinity\" — JSON, which the value eventually crosses into, has no such numbers. Refused on the interactive trunk and every /branch child: they converse with the user turn after turn and never return, so they hold no obligation to call this.\n\nagents `read <name>  — fetch the value the live descendant named `name` last handed to `reply, as [name: Str, reply: <value>]. The one tag that does not answer the roster. Only a descendant of yours may be read — never a sibling, an ancestor, or yourself; refused otherwise, as is a name that never replied. Idempotent: reading again before the child replies afresh answers the same value.\n\nEach tag is one exchange with the host, and — for every tag but `read — the roster it answers is the registry as it stands once the transition has landed. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_agents),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("schedules"),
        scheme_schedules,
        "schedules <tag>  — your self-wakeups: `list what is armed, `add one, `remove one. Every tag answers with the table afterwards, [[label: Str, trigger: Str, next-s: Int, fires: Int]], so what you read back is always what is armed now rather than a receipt for what you just did. Requires the self-wakeup grant (--allow-schedule) — an agent that can wake itself indefinitely holds real authority, so without the grant every tag is refused.\n\nschedules `list  — your live wakeups, oldest first: label as you named it, trigger as its source text (a cron expression, or `after 30m`), next-s the seconds until the next fire, recomputed as you ask, and fires how many times it has fired so far. Only live schedules appear: a spent one-shot has already removed itself, so a label you armed with `after and then see no more of has fired, not vanished. This is how you recover labels after an eviction.\n\nschedules `add [trigger: `cron <Str>|`after <Str>, label: <Str>, prompt: <Str>]  — arm a self-wakeup: at the chosen time a marked item carrying `prompt` is delivered to your inbox and re-engages you with no human present. It drains at your next exchange boundary — as soon as the tool batch in flight settles, not only at the end of the exchange — and arrives as marked chrome, `[scheduled '<label>' · <trigger>] <prompt>`, never read as a command even when the prompt opens with `/`. `trigger` is exactly one of two variants; any other shape is refused, naming both. `cron '<expr>'` is recurring: five whitespace-separated fields, minute hour day-of-month month day-of-week, read in the host's local timezone — e.g. `cron '0 9 * * 1-5'` for weekdays at 09:00. Each field is a comma list of `*`, a number, a range `a-b`, or a step over either (`*/15`, `a-b/2`, `N/step` meaning N up to the field's maximum); month and day-of-week also accept three-letter names (jan…dec, sun…sat), and day-of-week accepts 7 as a second spelling of Sunday. When both day fields are restricted, either one matching fires it (Vixie-cron's OR rule); when only one is, that one decides. Every fire recomputes the next occurrence in the host timezone, so DST shifts, clock steps, and suspends are absorbed rather than accumulated. `after '<n><unit>'` is a one-shot relative delay from the moment of arming, unit one of s/m/h/d and the count greater than zero — e.g. `after '30m'`, `after '2h'`. A trigger with no next occurrence at all — a parseable but impossible date such as `cron '0 0 30 2 *'` — is refused here rather than arming silently. `label` names the wakeup and is its identity: it must not be borne by another live schedule, and you must always supply one. `prompt` is the natural-language instruction you act on when woken, not code. Read the new row's next-s out of the answer to catch a cron expression that parsed but does not mean what you meant. Once armed: an `after removes itself when it fires; a cron re-arms itself, and drops itself only when nothing further lies inside its search horizon. A fire whose previous wakeup is still sitting undrained in your inbox is skipped, not queued behind it, and does not count as a fire. While any schedule is live this session parks for the next wakeup at quiescence instead of ending, so a recurring schedule you never remove keeps this agent alive indefinitely — that is what the grant buys. `/clear` drops every live schedule.\n\nschedules `remove <label>  — disarm the wakeup bearing `label`; its next occurrence goes with it and nothing further is delivered. The entry is gone in the answer, so the row's absence is the confirmation. A label that was never there answers the same way, and that is no evidence of a mistake: a one-shot may have fired and removed itself since you read it.\n\nEach tag is one exchange with the host, and the table it answers is the schedule registry as it stands once the transition has landed. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_schedules),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("pin-read"),
        scheme_pin_read,
        "pin-read <key>  — the card currently pinned under KEY on your register, as a `card value you can destructure, or () if the slot is empty. Reads your own register only. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_pin_read),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("pin-list"),
        scheme_pin_list,
        "pin-list  — the keys currently occupied on your pin register, as [String]. Read one back with pin-read. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_pin_list),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("context"),
        scheme_context,
        "context <tag>  — the window: what the provider is sent. Every tag answers the survey afterwards, [spans: [[exchange: Int, kind: Str, prompt: Str, bytes: Int, steps: Int, live: Bool]], evicted: Int, total-bytes: Int, total-steps: Int].\n\ncontext `survey  — one span per exchange in your context, oldest first; `evicted` counts the closed exchanges that have left it (readable with transcript). `total-bytes` against your window is the number that decides whether to edit at all. Changes nothing.\n\ncontext `drop <exchanges>  — shed whole closed exchanges from the window; they remain in the store. The provider re-reads everything after the earliest dropped exchange on your next request, so a drop that sheds little can cost more than it saves.\n\ncontext `evict [through: <Int>, note: <Str>]  — every exchange through a closed one leaves the window at once, replaced by the harness's index of them; `note` is optional, one short line for your future self shown beside that index. This is what the harness does for you when the window fills, without a note; do it yourself only to leave one, or to cut early on purpose.\n\nEach tag is one exchange with the host, and the survey it answers is the model view as it stands once the transition has landed; an edit lands at the desk immediately and is recorded as a model context event. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_context),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("transcript"),
        scheme_transcript,
        "transcript <tag>  — the store: every closed exchange this session or its ancestors recorded, whether or not it is still in your context. Read-only.\n\ntranscript `index  — [[exchange: Int, kind: Str, prompt: Str, steps: Int, bytes: Int, in-view: Bool]], oldest first. `kind` is `exchange`, `import`, or `inherited` (an ancestor's).\n\ntranscript `read <exchanges>  — the named closed exchanges as material, [[exchange: Int, messages: [Message]]] in store order, each element carrying its own `exchange`. A Message is [role: `system|`user|`assistant|`tool, parts: [Part]], one Message per model turn the span holds — a step boundary carries no message of its own. A Part is a variant, one arm per kind of content: `text [content: Str] is plain text; `program [tool: Str, source: Str, keys: [Str]] is a tool call — for exarch's own ral tool, `source` is the script that ran and `keys` is empty; for any other tool, `source` is empty and `keys` names its arguments; `result [content: Str] is a tool's response, already the digest the model saw; `reasoning [content: Str] is a model's reasoning, carried in full; `binary [content-type: Str, name: Str, bytes: Int] is an image/audio/video/PDF attachment's metadata only, never its payload; `custom [provider: Str, model: Str] names a provider-specific extension, never its payload. Narrow this material with `filter`/`take`/`view-text` over the records — do not expect elision or byte caps here, that is your job to apply. Bind the answer and read it in slices; the whole of it entering your context is what eviction just saved you from.\n\ntranscript `grep [pattern: <Str>, exchanges: <[Int]>]  — a Rust regex over every closed exchange (or the named ones): prompts, your programs, their results, your reasoning. `exchanges` is optional and narrows the search to those closed exchanges. Answers [hits: [[exchange: Int, role: Str, line: Int, text: Str]], total: Int], at most 100 hits oldest first; `total` is the true count, so a large one means narrow the pattern or the exchanges. Then `read the exchange a hit names.\n\nTo search a long store without spending your own context, hand the task to a `mnemon child: agents `start [type: `mnemon, prompt: 'transcript `grep … then reply with …'] — it shares your store and runs its own transcript against it. An `amnemon child starts blank and has no store but its own.\n\nAnswered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_transcript),
    ),
];
pub static HARNESS_BUILTINS: &[BuiltinEntry] = &HARNESS_BUILTINS_ARR;

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::testkit::ral_call;

    #[test]
    fn permission_label_accepts_every_bake_in() {
        for label in PERMISSION_LABELS {
            let v = Value::Variant {
                label: label.to_string(),
                payload: None,
            };
            permission_label(&v).unwrap_or_else(|e| panic!("the door must admit `{label}: {e:?}"));
        }
    }

    #[test]
    fn permission_label_rejects_an_unknown_tag_naming_every_offered_base() {
        let v = Value::Variant {
            label: "bogus".to_string(),
            payload: None,
        };
        let err = match permission_label(&v) {
            Err(ral_core::types::Break::Error(e)) => e,
            other => panic!("expected a door error, got {other:?}"),
        };
        for label in PERMISSION_LABELS {
            assert!(
                err.message.contains(label),
                "must name `{label}`, got: {}",
                err.message
            );
        }
    }

    /// Every label the door admits must resolve to a bake-in profile — which
    /// also parses and evaluates that profile's `data/*.exarch.ral` — so a label
    /// added here alone shows up. The door's table is the narrower of the two:
    /// the policy layer offers a launching human bases a child is not handed.
    #[test]
    fn every_permission_label_resolves_to_a_bake_in_base() {
        let cwd = std::env::current_dir().unwrap().display().to_string();
        for label in PERMISSION_LABELS {
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
            PERMISSION_LABELS.iter().all(|l| offered.contains(l)),
            "every door label must be a base the policy layer offers, got: {offered:?}"
        );
    }

    /// A payload-carrying tag is refused, never truncated to its label.
    #[test]
    fn permission_label_rejects_a_variant_carrying_a_payload() {
        let v = Value::Variant {
            label: "confined".to_string(),
            payload: Some(Box::new(Value::Int(1))),
        };
        assert!(permission_label(&v).is_err());
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `bogus, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        for label in PERMISSION_LABELS {
            assert!(
                result.content.contains(label),
                "must name `{label}`, got: {}",
                result.content
            );
        }
        assert!(
            crate::fleet::roster::listing(&session.agent).is_empty(),
            "an unknown grant label must never register a child"
        );
    }

    #[test]
    fn unknown_type_tag_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"agents `start [prompt: #'hi'#, name: 't', type: `bogus, grant: `confined, search: true, provider: `inherit, model: `inherit]",
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
            crate::fleet::roster::listing(&session.agent).is_empty(),
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `guess, model: `inherit]",
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
            crate::fleet::roster::listing(&session.agent).is_empty(),
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `named '']",
            5,
            &emit,
        );
        assert!(
            result.content.contains("non-empty"),
            "the refusal must say the name may not be empty, got: {}",
            result.content
        );
        assert!(
            crate::fleet::roster::listing(&session.agent).is_empty(),
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
            r#"agents `start [prompt: #'hi'#, name: "has space", type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `inherit]"#,
            5,
            &emit,
        );
        assert!(result.content.contains("name"), "got: {}", result.content);
        assert!(
            crate::fleet::roster::listing(&session.agent).is_empty(),
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
        let result = session.run_shell("call-1".to_string(), "agents `stop 'x'", 5, &emit);
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `amnemon, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("field named 'grant'"),
            "the diagnostic must name the missing field, got: {}",
            result.content
        );
        assert!(
            crate::fleet::roster::listing(&session.agent).is_empty(),
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grnat: `confined, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("no field named 'grnat'"),
            "the diagnostic must name the offending field, got: {}",
            result.content
        );
        assert!(
            crate::fleet::roster::listing(&session.agent).is_empty(),
            "a misspelled spec field must never register a child"
        );
    }

    /// Drives `run_shell` rather than `Avatar::deliberate`'s provider loop:
    /// the spawn seeds the child's handle from the parent's *own*
    /// `Arc<Provider>`, so one script consumed by both a driven parent
    /// exchange and its child races over which gets which stage.
    #[test]
    fn agent_full_stack_round_trip_answers_the_roster_and_parks_a_reply() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "reply-1",
                    r"agents `reply 'say hi'",
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"agents `start [prompt: #'say hi'#, name: 'helper', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("helper"),
            "the roster answered afterwards must carry the child's row, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    let notice = r.outcome.marked_item(&r.name);
                    assert!(
                        notice.contains("agents `read 'helper'"),
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

        let read = session.run_shell("call-2".to_string(), r"agents `read 'helper'", 5, &emit);
        assert!(
            read.content.contains("say hi"),
            "agents `read` must answer the child's deposited reply, got: {}",
            read.content
        );
        let roster = session.run_shell("call-3".to_string(), r"agents `list", 5, &emit);
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
    fn agents_cancel_answer_still_lists_the_cancelled_row() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let mut doomed = crate::agent::testkit::TestAgentSpec::new("doomed");
        doomed.parent = Some(session.agent.clone());
        let _doomed = crate::agent::testkit::test_agent(&session.fleet, doomed)
            .expect("a fresh child of a live parent");

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell("call-1".to_string(), "agents `cancel 'doomed'", 5, &emit);
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid agents `cancel call must succeed, got: {}",
            result.content
        );
        assert!(
            result.content.contains("doomed"),
            "a cancel is a request, not a transaction — the cancelled row must \
             still be in the roster answered afterwards, got: {}",
            result.content
        );
    }

    // ── schedule family door tests ───────────────────────────────────────
    //
    // Tag payloads are greedy, but `at_tag_payload_end` in
    // `core/src/syntax/parser.rs` stops one at a comma — so inside a record
    // literal a nullary tag cannot swallow its neighbour. That is why
    // `` schedules `add `` takes one spec record, not three positional
    // arguments.

    #[test]
    fn bad_cron_expr_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedules `add [trigger: `cron '* * * *', label: 'nightly', prompt: #'wake'#]",
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
            "schedules `add [trigger: `after 'nope', label: 'nightly', prompt: #'wake'#]",
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
            "schedules `add [trigger: `bogus 'x', label: 'nightly', prompt: #'wake'#]",
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
            "schedules `add [trigger: `after '1s', label: 'nightly']",
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
            "schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#, extra: 1]",
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
            "schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
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
            "schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid schedules `add call must succeed, got: {}",
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

    /// `` `removed ``/`` `no-such-label `` are retired: `` schedules `remove ``
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
            "schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid schedules `add call must succeed, got: {}",
            result.content
        );
        assert_eq!(
            session.agent.schedules.list().len(),
            1,
            "the schedule must be registered"
        );

        let result = session.run_shell(
            "call-2".to_string(),
            "schedules `remove 'nightly'",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid schedules `remove call must succeed, got: {}",
            result.content
        );
        assert!(
            !result.content.contains("nightly"),
            "the removed row must be gone from the table answered afterwards, got: {}",
            result.content
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "schedules `remove by label must remove the schedule"
        );

        let miss = session.run_shell(
            "call-3".to_string(),
            "schedules `remove 'nightly'",
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
            "schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        session.run_shell(
            "call-2".to_string(),
            "schedules `add [trigger: `after '10m', label: 'daily', prompt: #'wake'#]",
            5,
            &emit,
        );

        let result = session.run_shell(
            "call-3".to_string(),
            "schedules `remove 'nightly'",
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
                    r#"let found = ["a.rs", "b.rs"]; agents `reply [files: $found]"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"agents `start [prompt: #'find files'#, name: 'finder', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("finder"),
            "the roster must be the run's value and must name the child, got: {}",
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

        let read = session.run_shell("call-2".to_string(), r"agents `read 'finder'", 5, &emit);
        assert!(
            read.content.contains("files:")
                && read.content.contains("a.rs")
                && read.content.contains("b.rs"),
            "the structured record must reach the parent through `agents `read`, got: {}",
            read.content
        );
    }

    /// The refusal is an ordinary call error, not a termination: a later,
    /// well-formed `` agents `reply `` still succeeds.
    #[test]
    fn reply_refuses_a_non_first_order_value_and_does_not_terminate() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result =
            session.run_shell("call-1".to_string(), r"agents `reply { echo hi }", 5, &emit);
        assert!(
            result.content.contains("first-order"),
            "must name the first-order rule, got: {}",
            result.content
        );

        let ok = session.run_shell("call-2".to_string(), r"agents `reply 42", 5, &emit);
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
                    r#"agents `reply "first"; agents `reply "second""#,
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

    // ── `pin-read` / `pin-list` ─────────────────────────────────────────────

    /// The scripted-provider round-trip pattern of
    /// `reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox`,
    /// crossed with the desk's `` `pin-read `` arm: the child pins through
    /// `surface`, reads its own pin back in the same run, and hands the
    /// canonical card to its parent.
    #[test]
    fn pin_read_full_stack_round_trip_returns_canonical_card_to_parent() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"surface `pin [key: "note", body: `card ["hi there"]]; agents `reply !{pin-read "note"}"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"agents `start [prompt: #'pin and read back'#, name: 'pinner', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("pinner"),
            "the roster must be the run's value and must name the child, got: {}",
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
        let read = session.run_shell("call-2".to_string(), r"agents `read 'pinner'", 5, &emit);
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
                    r#"agents `reply !{pin-read "nope"}"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"agents `start [prompt: #'read an absent key'#, name: 'reader', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("reader"),
            "the roster must be the run's value and must name the child, got: {}",
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

        let read = session.run_shell("call-2".to_string(), r"agents `read 'reader'", 5, &emit);
        assert!(
            read.content.contains("reply: ()"),
            "an absent key must reply unit, got: {}",
            read.content
        );
    }

    // ── the task kit as a pure prelude over the pin family ─────────────────

    /// `tasks-add`, `tasks-status`, `tasks-tag`, and `tasks-note` all read and
    /// write the "tasks" pin through `tasks-sync`; `tasks-list` and a direct
    /// `tasks-decode !{pin-read "tasks"}` must agree on every field, including
    /// the tags and notes the old pinned rollup never rendered.
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
            r#"tasks-add "fix the parser""#,
            BUDGET,
            &emit,
        );
        session.run_shell(
            "call-2".to_string(),
            r#"tasks-add "write docs""#,
            BUDGET,
            &emit,
        );
        session.run_shell("call-3".to_string(), "tasks-status 1 `doing", BUDGET, &emit);
        session.run_shell(
            "call-4".to_string(),
            r#"tasks-tag 1 "urgent""#,
            BUDGET,
            &emit,
        );
        session.run_shell(
            "call-5".to_string(),
            r#"tasks-note 1 "blocked on review""#,
            BUDGET,
            &emit,
        );

        let listed = session.run_shell("call-6".to_string(), "tasks-list", BUDGET, &emit);
        for field in ["fix the parser", "`doing", "urgent", "blocked on review"] {
            assert!(
                listed.content.contains(field),
                "tasks-list must show the tagged, noted task's {field}, got: {}",
                listed.content
            );
        }
        assert!(
            listed.content.contains("write docs"),
            "tasks-list must show the untouched second task, got: {}",
            listed.content
        );

        let read = session.run_shell(
            "call-7".to_string(),
            r#"let [decoded-task, _] = !{tasks-decode !{pin-read "tasks"}}
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

    /// `tasks-add` inside a function body pins to the register, which SPEC
    /// §10's block-discard rule never touches — a later, separate top-level
    /// run still sees it.
    #[test]
    fn add_task_inside_a_function_body_survives_the_block_and_the_call() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            r#"let f = { tasks-add "inside a block" }; !{f}"#,
            5,
            &emit,
        );

        let listed = session.run_shell("call-2".to_string(), "tasks-list", 5, &emit);
        assert!(
            listed.content.contains("inside a block"),
            "a task added inside a function body must survive to the next top-level run, got: {}",
            listed.content
        );
    }

    /// A sub-agent's register is its own: a child's `tasks-add` must never
    /// reach the parent's "tasks" pin.
    #[test]
    fn sub_agent_pinning_tasks_leaves_the_parents_register_untouched() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell("call-1".to_string(), r#"tasks-add "parent task""#, 5, &emit);

        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"tasks-add "child task"; agents `reply "done""#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);

        let result = session.run_shell(
            "call-2".to_string(),
            r"agents `start [prompt: #'add a task'#, name: 'tasker', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("tasker"),
            "the roster must be the run's value and must name the child, got: {}",
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

        let listed = session.run_shell("call-3".to_string(), "tasks-list", 5, &emit);
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
    /// last open task to `` `done `` empties the pin, and a later `tasks-add`
    /// finds no register and restarts id allocation at 1.
    #[test]
    fn transitioning_the_last_open_task_to_done_clears_the_pin_and_restarts_ids() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell("call-1".to_string(), r#"tasks-add "only task""#, 5, &emit);
        session.run_shell("call-2".to_string(), "tasks-status 1 `done", 5, &emit);

        let read = session.run_shell("call-3".to_string(), r#"pin-read "tasks""#, 5, &emit);
        assert!(
            !read.content.contains("VALUE:"),
            "an all-done list must clear the pin to unit, got: {}",
            read.content
        );

        session.run_shell("call-4".to_string(), r#"tasks-add "fresh""#, 5, &emit);
        let listed = session.run_shell("call-5".to_string(), "tasks-list", 5, &emit);
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
            r#"pin-set "tasks" `card [`text [spans: [[text: "not task shaped"]]]]"#,
            5,
            &emit,
        );

        let result = session.run_shell("call-2".to_string(), r#"tasks-add "x""#, 5, &emit);
        assert!(
            result
                .content
                .contains("tasks: the card under the 'tasks' pin is not task-shaped"),
            "the didactic fail must name the expected shape, got: {}",
            result.content
        );
    }

    // ── context family door tests ────────────────────────────────────────

    /// A trunk holding one closed exchange, which every context test needs
    /// before it has anything addressable to name.
    fn trunk_with_a_closed_exchange() -> crate::agent::Avatar {
        let session = crate::agent::Avatar::for_test("system").unwrap();
        crate::agent::testkit::close_exchange(&session, "first prompt", "first answer");
        session
    }

    /// `scheme_context`'s outer tag row is open, so `` context `rewind `` — the
    /// tag a model most plausibly invents — reaches the door naming the three
    /// legal ones rather than dying as a row-unification mismatch.
    #[test]
    fn unknown_context_tag_reaches_the_door_naming_every_legal_tag() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell("call-1".to_string(), "context `rewind [3]", 5, &emit);
        for tag in ["survey", "drop", "evict"] {
            assert!(
                result.content.contains(tag),
                "must name `{tag}, got: {}",
                result.content
            );
        }
    }

    /// `` `evict ``'s record row is open only on the tail, so `through`
    /// itself is still static: a misspelling reaches the type error, not the
    /// door.
    #[test]
    fn misspelled_evict_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell("call-1".to_string(), "context `evict [thru: 1]", 5, &emit);
        assert!(
            !result.content.contains("EXIT: 0") && result.content.contains("through"),
            "the diagnostic must name the field the row demands, got: {}",
            result.content
        );
    }

    /// The whole family through the real shell: an edit answers the survey
    /// the transition leaves behind, so the count of what left is there to
    /// read without a second call. The optional `note` rides the open record
    /// row, so both shapes type-check.
    #[test]
    fn an_eviction_answers_the_survey_it_leaves_behind() {
        let mut session = trunk_with_a_closed_exchange();
        crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "context `evict [through: 1, note: 'the old work is done']",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid eviction must succeed, got: {}",
            result.content
        );
        assert!(
            result.content.contains("evicted") && result.content.contains("total-bytes"),
            "the answer must be the survey afterwards, carrying the evicted count, got: {}",
            result.content
        );
        assert!(
            !result.content.contains("bytes-delta"),
            "the edit answers the state, never a receipt for the step, got: {}",
            result.content
        );
    }

    /// The same tag with no `note`: the open record row admits it, so the
    /// harness never asks the model for an empty string.
    #[test]
    fn an_eviction_without_a_note_type_checks() {
        let mut session = trunk_with_a_closed_exchange();
        crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "context `evict [through: 1]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "an eviction with no note must succeed, got: {}",
            result.content
        );
    }

    /// §2.4's shape, end to end: `` `read ``'s answer is a list, so a slice
    /// is `$spans[0]`, addressed by its own `exchange` field, and its messages
    /// are ral records with variant parts rather than a rendered string.
    #[test]
    fn transcript_answers_span_records_with_variant_parts() {
        let mut session = trunk_with_a_closed_exchange();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r#"let spans = transcript `read [1]
               echo !{length $spans}
               echo $spans[0][exchange]
               let msgs = $spans[0][messages]
               echo !{length $msgs}
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
               say-role $msgs[1][role]
               say-part $msgs[1][parts][0]"#,
            5,
            &emit,
        );
        assert!(
            result.content.contains("\n1\n"),
            "one named span, got: {}",
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

    /// `scheme_transcript`'s outer tag row is open, so `` transcript `search ``
    /// — the tag a model most plausibly invents — reaches the door naming the
    /// three legal ones.
    #[test]
    fn unknown_transcript_tag_reaches_the_door_naming_every_legal_tag() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell("call-1".to_string(), "transcript `search 'x'", 5, &emit);
        for tag in ["index", "read", "grep"] {
            assert!(
                result.content.contains(tag),
                "must name `{tag}, got: {}",
                result.content
            );
        }
    }

    /// The answer type is free, so each tag's own shape has to type-check
    /// against the use the program makes of it: a listing indexes as a list,
    /// a grep answer projects `hits` and `total` as a record. The optional
    /// `exchanges` rides `` `grep ``'s open record row.
    #[test]
    fn index_and_grep_answer_their_own_shapes() {
        let mut session = trunk_with_a_closed_exchange();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let result = session.run_shell(
            "call-1".to_string(),
            // Bindings are named, not lettered: ral keeps value and command
            // names disjoint, so a one-letter binding fails on any host with
            // that letter on PATH (plan9port ships a `g`).
            r"let listed = transcript `index
               echo $listed[0][exchange] $listed[0][in-view]
               let matched = transcript `grep [pattern: 'first (prompt|answer)']
               echo !{length $matched[hits]} $matched[total]
               let missed = transcript `grep [pattern: 'nothing here', exchanges: [1]]
               echo $missed[total]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("1 true"),
            "the one closed exchange is listed and still in view, got: {}",
            result.content
        );
        assert!(
            result.content.contains("2 2"),
            "both of its turns match, and `total` counts them all, got: {}",
            result.content
        );
        assert!(
            result.content.contains("\n0\n"),
            "a pattern that matches nothing answers no hits, got: {}",
            result.content
        );
    }
}
