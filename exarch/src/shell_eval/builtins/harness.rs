//! The harness builtins — `agents`, `schedules`, `pin-read`, `pin-list`,
//! `context`, `context-read`, `context-drop`, `context-fold` — with the type
//! schemes that gate them. A returning agent's reply is a tag of `agents`
//! (`` `reply ``), not a builtin of its own — the fleet is one family.
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
//! A registry is one enquiry class, named as the model names it: `agents` and
//! `schedules` each carry the model's tag as a nested variant and its record
//! verbatim, and each answers the registry's own state.

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
/// guarantees the five fields, so the `else` arms below are unreachable
/// through the type checker; they stay didactic rather than trust it alone.
fn start_agent(spec: &Value, mooring: &Mooring, shell: &Shell) -> Settled<FOValue> {
    let Value::Map(fields) = spec else {
        return Err(sig(format!(
            "agents: `start`'s payload must be a [prompt: …, name: …, type: …, grant: …, search: …] record, got {}",
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

/// The `` `context `` scheme (`context_receipt_ty`) is a closed four-field
/// record, so a survey missing any of them is host-side drift, not a call
/// error — name what is missing rather than shrugging at the whole shape.
fn context_receipt(answer: FOValue) -> Settled<Value> {
    const FIELDS: [&str; 4] = ["spans", "total-bytes", "total-steps", "cache"];
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

fn edit_receipt(answer: FOValue, verb: &str) -> Settled<Value> {
    let shape_error = || {
        sig(format!(
            "{verb}: host answered an unexpected shape for the receipt"
        ))
    };
    let FOValue::Map { entries } = &answer else {
        return Err(shape_error());
    };
    if entries
        .iter()
        .any(|(key, value)| key == "bytes-delta" && matches!(value, FOValue::Int { .. }))
    {
        Ok(Value::from(answer))
    } else {
        Err(shape_error())
    }
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

pub(crate) fn context_fold_payload(value: &Value) -> Settled<FOValue> {
    let Value::Map(spec) = value else {
        return Err(sig(format!(
            "context-fold: expected [through: Int, digest: Str], got {}",
            value.type_name()
        )));
    };
    let Some(through) = spec.get("through") else {
        return Err(sig(
            "context-fold: the spec record needs a `through` field — the last exchange to fold",
        ));
    };
    let Value::Int(through) = through else {
        return Err(sig(format!(
            "context-fold: `through` must be an Int, got {}",
            through.type_name()
        )));
    };
    if *through < 0 {
        return Err(sig(format!(
            "context-fold: `through` must be non-negative, got {through}"
        )));
    }
    let Some(digest) = spec.get("digest") else {
        return Err(sig(
            "context-fold: the spec record needs a `digest` field — the model's summary text",
        ));
    };
    let Value::String(digest) = digest else {
        return Err(sig(format!(
            "context-fold: `digest` must be a Str, got {}",
            digest.type_name()
        )));
    };
    Ok(FOValue::List {
        items: vec![
            FOValue::Int { value: *through },
            FOValue::String {
                value: digest.clone(),
            },
        ],
    })
}

/// `context` — enquires `` `context ``; the answer is a survey of this
/// agent's own transcript, so the call takes no argument.
fn builtin_context(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "context".to_string(),
            payload: None,
        },
    )?;
    context_receipt(answer)
}

/// `context-read` — enquires `` `context-read ``; the answer is the rendered
/// transcript of the named exchanges, one Str.
fn builtin_context_read(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let payload = context_exchanges_payload(&args[0], "context-read")?;
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "context-read".to_string(),
            payload: Some(Box::new(payload)),
        },
    )?;
    let FOValue::String { value } = answer else {
        return Err(sig(
            "context-read: host answered an unexpected shape; expected a Str transcript",
        ));
    };
    Ok(Value::String(value))
}

/// `context-drop` — enquires `` `context-drop ``; the answer is the byte-delta
/// receipt of the edit.
fn builtin_context_drop(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let payload = context_exchanges_payload(&args[0], "context-drop")?;
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "context-drop".to_string(),
            payload: Some(Box::new(payload)),
        },
    )?;
    edit_receipt(answer, "context-drop")
}

/// `context-fold` — enquires `` `context-fold ``; the answer is the byte-delta
/// receipt of the edit.
fn builtin_context_fold(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let payload = context_fold_payload(&args[0])?;
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "context-fold".to_string(),
            payload: Some(Box::new(payload)),
        },
    )?;
    edit_receipt(answer, "context-fold")
}

/// A variant over a row of tags with stated payloads, left open on `tail` so
/// an unknown tag reaches the runtime door that enumerates the legal ones
/// rather than dying as a row-unification mismatch.
fn open_variant(tags: &[(&str, Ty)], tail: RowVar) -> Ty {
    use ral_core::syntax::tag::tag_row_label;
    let mut row = Row::Var(tail);
    for (label, ty) in tags.iter().rev() {
        row = Row::Extend(tag_row_label(label), Box::new(ty.clone()), Box::new(row));
    }
    Ty::Variant(row)
}

/// `agents :: ∀α β ρ1 ρ2 ρ3. <list | start [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, search: Bool] | message [to: Str, text: Str] | cancel Str | reply β | read Str | ρ3> → F α`
///
/// The outer tag row is open (`ρ3`) so an unrecognised tag reaches the
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
/// it. The `type` and `grant` rows *inside* `start` stay open, because a
/// literal tag infers its own open row: closing them would make `` `bogus ``
/// a bare row-mismatch diagnostic that never reaches
/// [`agent_type_label`]/[`permission_label`], which enumerate the legal
/// labels. `search` is two-state rather than an enumeration, so `Ty::Bool`
/// closes it outright. `reply`'s `β` is likewise trusted first-order data,
/// checked at [`reply_agent`]'s door rather than by the row.
fn scheme_agents(u: &mut Unifier) -> Scheme {
    let type_row = u.fresh_row_var();
    let grant_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    let reply_ty = u.fresh_tyvar();
    let answer_ty = u.fresh_tyvar();
    scheme(
        &[reply_ty, answer_ty],
        &[],
        &[type_row, grant_row, tag_row],
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
        ("total-bytes", Ty::Int),
        ("total-steps", Ty::Int),
        ("cache", Ty::String),
    ])
}

fn edit_receipt_ty() -> Ty {
    closed_record(&[("bytes-delta", Ty::Int)])
}

/// `context :: F [spans: [[exchange: Int, kind: Str, prompt: Str, bytes: Int, steps: Int, live: Bool]], total-bytes: Int, total-steps: Int, cache: Str]`
fn scheme_context(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(pure(context_receipt_ty())))
}

/// `context-read :: [Int] → F Str`
fn scheme_context_read(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::List(Box::new(Ty::Int)), pure(Ty::String))),
    )
}

/// `context-drop :: [Int] → F [bytes-delta: Int]`
fn scheme_context_drop(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::List(Box::new(Ty::Int)), pure(edit_receipt_ty()))),
    )
}

/// `context-fold :: [through: Int, digest: Str] → F [bytes-delta: Int]`
fn scheme_context_fold(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(
            closed_record(&[("through", Ty::Int), ("digest", Ty::String)]),
            pure(edit_receipt_ty()),
        )),
    )
}

// A named array, not a promoted temporary: rustc refuses promotion once an
// entry carries `BuiltinEntry`'s interior-mutable arity cache.
static HARNESS_BUILTINS_ARR: [BuiltinEntry; 8] = [
    BuiltinEntry::new(
        Cow::Borrowed("agents"),
        scheme_agents,
        "agents <tag>  — the fleet: `list what is live, `start a child, `message one, `cancel one, `reply to hand your own value up, `read one back off a descendant. Every tag but `read answers with the roster afterwards, [[name: Str, state: `busy|`waiting-on-agents|`replied|`waiting, idle-s: Int, elapsed-s: Int, log-dir: Str]], so what you read back is always what is live now rather than a receipt for what you just did.\n\nagents `list  — your live descendants at any depth, oldest first. `state` is `busy while working, `waiting-on-agents while held only by a busy child of its own, `replied once it has called `reply and parked, `waiting once a human has engaged it and it parked with no reply. `idle-s` is seconds since it parked — zero while `busy` or `waiting-on-agents. A settled agent (cancelled, failed, or reaped past its hour) is not listed. This is how you recover names after a context compaction.\n\nagents `start [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>]  — launch a sub-agent. Launch-only and always asynchronous: the child's reply is NOT this call's result — it arrives later, as a one-line notice in your inbox, and you fetch the value with `read. The answer's roster carries the child's row, and that row's name and log-dir are its receipt. `type` selects the child's memory: `amnemon` starts blank (no shared history), while `mnemon` inherits your current model-visible conversation and reuses your provider selection for cache locality. Every child receives the value-snapshot of the parent's bindings, cwd, and env — `mnemon` too; the serializable fragment crosses, while a live job handle becomes an opaque placeholder. `prompt` is a computed string and becomes the child's fresh final prompt. Keep large material in a named binding rather than splicing it into prompt; small, certainly-needed material may still be spliced. Wrap `prompt` in a raw string #'…'# if it carries $, !, or quotes. `name` is the child's identity — non-empty, at most 24 characters, ASCII letters/digits/-/_ only — and must not be borne by any live agent, or the call is refused; pick something descriptive, like 'fix-parser-tests'. `grant` bounds the child to at most your own authority and must be exactly one of `confined (offline, no home reads), `read-only (writes only to scratch), `edit-only (edits the working tree, no build tooling), `reasonable (everyday tooling), `dangerous (no narrowing); any other label is refused, naming all five. `search` states whether the child may use the provider's own built-in web search, bounded above by your own — asking for it when you do not have it silently yields a child without it. Delegation depth is finite — each descendant is handed one less unit of fuel than its spawner holds, and once fuel reaches zero this call is refused; fuel bounds how deep a chain may recurse, never how many children you may start at any one depth.\n\nagents `message [to: <Str>, text: <Str>]  — send `text` as a marked item to the live descendant named `to`; it lands at that child's next exchange boundary, not as human input, and wakes a `replied or `waiting child into a fresh exchange. Only a descendant of yours may receive it — never a sibling, an ancestor, or yourself; refused otherwise. It does not return the recipient's answer: this is coordination, not a call. Nothing in the roster changes, so the answer is the plain confirmation that the recipient was live when you sent.\n\nagents `cancel <name>  — ask the live descendant named `name` to stop. It stops at its next checkpoint and then delivers a cancelled result to your inbox. Only a descendant of yours may be cancelled — never a sibling, an ancestor, or yourself; refused otherwise. A cancel is a request, not a transaction: the child is still running when this answers, so its row is still in the roster you get back. A name you still see listed is NOT a failed cancel — do not fire it again; read `list later and find it gone.\n\nagents `reply <value>  — hand `value` back to whoever spawned you. Your parent receives exactly this value, nothing else — not your reasoning, your shell bindings, or any prose you streamed along the way. `value` must be first-order data: no closures, handles, or environments; passing one fails this call with a didactic error and your run continues, so fix the value and call `reply again. Call it more than once in an exchange and the last call wins — an earlier value is discarded, not appended. It does not end your run: you park (`state `replied) rather than settle, and may be `message`d for a follow-up — answer that with another `reply. A non-finite Float (NaN, +Infinity, -Infinity) reaches your parent as the string \"NaN\"/\"Infinity\"/\"-Infinity\" — JSON, which the value eventually crosses into, has no such numbers. Refused on the interactive trunk and every /branch child: they converse with the user turn after turn and never return, so they hold no obligation to call this.\n\nagents `read <name>  — fetch the value the live descendant named `name` last handed to `reply, as [name: Str, reply: <value>]. The one tag that does not answer the roster. Only a descendant of yours may be read — never a sibling, an ancestor, or yourself; refused otherwise, as is a name that never replied. Idempotent: reading again before the child replies afresh answers the same value.\n\nEach tag is one exchange with the host, and — for every tag but `read — the roster it answers is the registry as it stands once the transition has landed. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_agents),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("schedules"),
        scheme_schedules,
        "schedules <tag>  — your self-wakeups: `list what is armed, `add one, `remove one. Every tag answers with the table afterwards, [[label: Str, trigger: Str, next-s: Int, fires: Int]], so what you read back is always what is armed now rather than a receipt for what you just did. Requires the self-wakeup grant (--allow-schedule) — an agent that can wake itself indefinitely holds real authority, so without the grant every tag is refused.\n\nschedules `list  — your live wakeups, oldest first: label as you named it, trigger as its source text (a cron expression, or `after 30m`), next-s the seconds until the next fire, recomputed as you ask, and fires how many times it has fired so far. Only live schedules appear: a spent one-shot has already removed itself, so a label you armed with `after and then see no more of has fired, not vanished. This is how you recover labels after a context compaction.\n\nschedules `add [trigger: `cron <Str>|`after <Str>, label: <Str>, prompt: <Str>]  — arm a self-wakeup: at the chosen time a marked item carrying `prompt` is delivered to your inbox and re-engages you with no human present. It drains at your next exchange boundary — as soon as the tool batch in flight settles, not only at the end of the exchange — and arrives as marked chrome, `[scheduled '<label>' · <trigger>] <prompt>`, never read as a command even when the prompt opens with `/`. `trigger` is exactly one of two variants; any other shape is refused, naming both. `cron '<expr>'` is recurring: five whitespace-separated fields, minute hour day-of-month month day-of-week, read in the host's local timezone — e.g. `cron '0 9 * * 1-5'` for weekdays at 09:00. Each field is a comma list of `*`, a number, a range `a-b`, or a step over either (`*/15`, `a-b/2`, `N/step` meaning N up to the field's maximum); month and day-of-week also accept three-letter names (jan…dec, sun…sat), and day-of-week accepts 7 as a second spelling of Sunday. When both day fields are restricted, either one matching fires it (Vixie-cron's OR rule); when only one is, that one decides. Every fire recomputes the next occurrence in the host timezone, so DST shifts, clock steps, and suspends are absorbed rather than accumulated. `after '<n><unit>'` is a one-shot relative delay from the moment of arming, unit one of s/m/h/d and the count greater than zero — e.g. `after '30m'`, `after '2h'`. A trigger with no next occurrence at all — a parseable but impossible date such as `cron '0 0 30 2 *'` — is refused here rather than arming silently. `label` names the wakeup and is its identity: it must not be borne by another live schedule, and you must always supply one. `prompt` is the natural-language instruction you act on when woken, not code. Read the new row's next-s out of the answer to catch a cron expression that parsed but does not mean what you meant. Once armed: an `after removes itself when it fires; a cron re-arms itself, and drops itself only when nothing further lies inside its search horizon. A fire whose previous wakeup is still sitting undrained in your inbox is skipped, not queued behind it, and does not count as a fire. While any schedule is live this session parks for the next wakeup at quiescence instead of ending, so a recurring schedule you never remove keeps this agent alive indefinitely — that is what the grant buys. `/clear` drops every live schedule.\n\nschedules `remove <label>  — disarm the wakeup bearing `label`; its next occurrence goes with it and nothing further is delivered. The entry is gone in the answer, so the row's absence is the confirmation. A label that was never there answers the same way, and that is no evidence of a mistake: a one-shot may have fired and removed itself since you read it.\n\nEach tag is one exchange with the host, and the table it answers is the schedule registry as it stands once the transition has landed. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
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
        "context  — survey the finite, addressable model context. Returns [spans: [[exchange: Int, kind: Str, prompt: Str, bytes: Int, steps: Int, live: Bool]], total-bytes: Int, total-steps: Int, cache: Str]. Each span is an exchange or import, and a digest is represented by its reach in `exchange`; `prompt` is its opening line, `bytes` its serialized weight, `steps` its provider-step count, and `live` marks the exchange currently in progress. The cache sentence explains that editing before the cache watermark re-reads the prefix on the next request. This is a survey: it does not edit context.",
        BuiltinBody::Static(builtin_context),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("context-read"),
        scheme_context_read,
        "context-read <exchanges>  — read named closed exchanges as one transcript Str, with roles marked and steps delimited. The list must be non-empty; name a digest by its reach, and do not name an exchange folded strictly inside that digest. Only stdout echoes into a turn's tool result; a `let`-bound value prints nothing. Binding is silent, but both stdout and a tool call's final VALUE enter model context; read large bindings in slices — never bare-print a whole binding merely to inspect it, because the transcript is material, not a survey.",
        BuiltinBody::Static(builtin_context_read),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("context-drop"),
        scheme_context_drop,
        "context-drop <exchanges>  — shed whole closed exchanges from the model context, where <exchanges> is a list of non-negative exchange numbers from `context`. The live exchange cannot be named, unknown or already-folded exchanges are refused with an explanation, and an empty list is not an edit. The model is always mid-exchange when it speaks, so a rewind-shaped request for a suffix of closed exchanges is this verb with a range; there is no context-rewind. Returns [bytes-delta: Int], the serialized model-view bytes before minus after (a negative value is honest when a digest is larger than what it replaces). Applied immediately at the desk; the edit is recorded as a model context event.",
        BuiltinBody::Static(builtin_context_drop),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("context-fold"),
        scheme_context_fold,
        "context-fold [through: <Int>, digest: <Str>]  — replace the visible prefix through a closed exchange with the digest text you supply. `through` may extend the current digest by its reach, but cannot cross the live exchange; name the digest reach itself to fold further. Returns [bytes-delta: Int], the serialized model-view bytes before minus after, and records one model context event immediately. A digest is curation, not a promise of compression, so a negative delta is possible and meaningful.",
        BuiltinBody::Static(builtin_context_fold),
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
        let root = ral_core::types::Capabilities::root();
        let cwd = std::env::current_dir().unwrap().display().to_string();
        for label in PERMISSION_LABELS {
            crate::policy::narrow(&root, label, &cwd)
                .unwrap_or_else(|e| panic!("door label `{label} must name a bake-in base: {e}"));
        }
        let err = crate::policy::narrow(&root, "bogus", &cwd)
            .expect_err("an unknown base must be refused");
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `bogus, search: true]",
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `bogus, grant: `confined, search: true]",
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

    #[test]
    fn invalid_name_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r#"agents `start [prompt: #'hi'#, name: "has space", type: `amnemon, grant: `confined, search: true]"#,
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `amnemon, search: true]",
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
            r"agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grnat: `confined, search: true]",
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
            r"agents `start [prompt: #'say hi'#, name: 'helper', type: `amnemon, grant: `read-only, search: false]",
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
            session.schedules.list().is_empty(),
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
            session.schedules.list().is_empty(),
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
            session.schedules.list().is_empty(),
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
            session.schedules.list().is_empty(),
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
            session.schedules.list().is_empty(),
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
        let live = session.schedules.list();
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
            session.schedules.list().len(),
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
            session.schedules.list().is_empty(),
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
            r"agents `start [prompt: #'find files'#, name: 'finder', type: `amnemon, grant: `read-only, search: false]",
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
            r"agents `start [prompt: #'pin and read back'#, name: 'pinner', type: `amnemon, grant: `read-only, search: false]",
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
            r"agents `start [prompt: #'read an absent key'#, name: 'reader', type: `amnemon, grant: `read-only, search: false]",
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

    /// `add-task`, `transition`, `tag-task`, and `note-task` all read and
    /// write the "tasks" pin through `sync-tasks`; `render-tasks` and a direct
    /// `decode-tasks !{pin-read "tasks"}` must agree on every field, including
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
            r#"add-task "fix the parser""#,
            BUDGET,
            &emit,
        );
        session.run_shell(
            "call-2".to_string(),
            r#"add-task "write docs""#,
            BUDGET,
            &emit,
        );
        session.run_shell("call-3".to_string(), "transition 1 `doing", BUDGET, &emit);
        session.run_shell(
            "call-4".to_string(),
            r#"tag-task 1 "urgent""#,
            BUDGET,
            &emit,
        );
        session.run_shell(
            "call-5".to_string(),
            r#"note-task 1 "blocked on review""#,
            BUDGET,
            &emit,
        );

        let rendered = session.run_shell("call-6".to_string(), "render-tasks", BUDGET, &emit);
        assert!(
            rendered
                .content
                .contains("#1  `doing  fix the parser [urgent]  -- blocked on review"),
            "render-tasks must show the tagged, noted task, got: {}",
            rendered.content
        );
        assert!(
            rendered.content.contains("#2  `open  write docs"),
            "render-tasks must show the untouched second task, got: {}",
            rendered.content
        );

        let read = session.run_shell(
            "call-7".to_string(),
            r#"let [t, _] = !{decode-tasks !{pin-read "tasks"}}
               echo $t[desc]
               echo $t[status]
               echo !{intercalate "," $t[tags]}
               echo $t[notes]"#,
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

    /// `add-task` inside a function body pins to the register, which SPEC
    /// §10's block-discard rule never touches — a later, separate top-level
    /// run still sees it.
    #[test]
    fn add_task_inside_a_function_body_survives_the_block_and_the_call() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            r#"let f = { add-task "inside a block" }; !{f}"#,
            5,
            &emit,
        );

        let rendered = session.run_shell("call-2".to_string(), "render-tasks", 5, &emit);
        assert!(
            rendered.content.contains("inside a block"),
            "a task added inside a function body must survive to the next top-level run, got: {}",
            rendered.content
        );
    }

    /// A sub-agent's register is its own: a child's `add-task` must never
    /// reach the parent's "tasks" pin.
    #[test]
    fn sub_agent_pinning_tasks_leaves_the_parents_register_untouched() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell("call-1".to_string(), r#"add-task "parent task""#, 5, &emit);

        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"add-task "child task"; agents `reply "done""#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);

        let result = session.run_shell(
            "call-2".to_string(),
            r"agents `start [prompt: #'add a task'#, name: 'tasker', type: `amnemon, grant: `read-only, search: false]",
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

        let rendered = session.run_shell("call-3".to_string(), "render-tasks", 5, &emit);
        assert!(
            rendered.content.contains("parent task"),
            "the parent's own task must survive, got: {}",
            rendered.content
        );
        assert!(
            !rendered.content.contains("child task"),
            "the child's pin must never reach the parent's register, got: {}",
            rendered.content
        );
    }

    /// The protected-`services` guard blocks the write direction only:
    /// `pin-set "services"` is refused with the existing diagnostic, and
    /// `pin-read "services"` still answers (reads are not writes).
    #[test]
    fn services_pin_refuses_writes_but_pin_read_still_answers() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell(
            "call-1".to_string(),
            r#"pin-set "services" `card [`text [spans: [[text: "nope"]]]]"#,
            5,
            &emit,
        );
        let saw_refusal = crate::bus::drain_records(&rx).iter().any(|rec| {
            matches!(rec, crate::record::Record::Forensic(crate::record::Forensic::Error { text }) if text.contains("protected service-ledger pin"))
        });
        assert!(saw_refusal, "expected the protected-pin diagnostic");

        let read = session.run_shell("call-2".to_string(), r#"pin-read "services""#, 5, &emit);
        assert!(
            read.content.contains("EXIT: 0"),
            "pin-read of a protected key must still answer, got: {}",
            read.content
        );
    }

    /// `sync-tasks` clears the slot once no work remains: transitioning the
    /// last open task to `` `done `` empties the pin, and a later `add-task`
    /// finds no register and restarts id allocation at 1.
    #[test]
    fn transitioning_the_last_open_task_to_done_clears_the_pin_and_restarts_ids() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.run_shell("call-1".to_string(), r#"add-task "only task""#, 5, &emit);
        session.run_shell("call-2".to_string(), "transition 1 `done", 5, &emit);

        let read = session.run_shell("call-3".to_string(), r#"pin-read "tasks""#, 5, &emit);
        assert!(
            !read.content.contains("VALUE:"),
            "an all-done list must clear the pin to unit, got: {}",
            read.content
        );

        session.run_shell("call-4".to_string(), r#"add-task "fresh""#, 5, &emit);
        let rendered = session.run_shell("call-5".to_string(), "render-tasks", 5, &emit);
        assert!(
            rendered.content.contains("#1  `open  fresh"),
            "id allocation must restart at 1 once the register is empty, got: {}",
            rendered.content
        );
    }

    /// A card under "tasks" that `decode-tasks` does not recognise — the
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

        let result = session.run_shell("call-2".to_string(), r#"add-task "x""#, 5, &emit);
        assert!(
            result
                .content
                .contains("tasks: the card under the 'tasks' pin is not task-shaped"),
            "the didactic fail must name the expected shape, got: {}",
            result.content
        );
    }
}
