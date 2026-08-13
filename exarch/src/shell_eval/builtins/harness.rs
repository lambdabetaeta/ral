//! The harness builtins — `agent`, `agents`, `message`, `agent-cancel`,
//! `schedule`, `schedules`, `unschedule`, `reply`, `pin-read`, `pin-list`,
//! `context`, `context-read`, `context-drop`, `context-fold` —
//! with the type schemes that gate them.
//!
//! Each body validates at the door before it enquires, so a malformed call
//! never reaches the host. `agent` forks this shell into the run's nursery
//! ([`ral_core::Shell::fork_into_nursery`]) and enquires with the parked
//! fork's id: the reentrancy law bars a desk handler from holding
//! `&mut Shell` to fork one itself. [`crate::fleet::desk::ExarchDesk`]
//! answers every class on the other side.

use crate::fleet::schedule::{CronSchedule, parse_duration};
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{
    BuiltinTypeRule, closed_record, fun, mk_scheme as scheme, pure, thunk,
};
use ral_core::typecheck::{Row, Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled, sig};
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

/// The tab-bar contract: non-empty, ≤24 chars, ASCII alphanumeric plus
/// `-`/`_`, since the tab bar lays a name out as a single token.
pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Decode a spawn's `type` into its bare tag. [`scheme_agent`] leaves that
/// row open, so the enumeration is closed here, where the error can name
/// both memory modes.
fn agent_type_label(v: &Value) -> Settled<String> {
    if let Value::Variant {
        label,
        payload: None,
    } = v
        && (label == "amnemon" || label == "mnemon")
    {
        return Ok(label.clone());
    }
    Err(sig(format!(
        "agent: `type` must be `amnemon (blank context) or `mnemon (inherits your conversation) — got {v}"
    )))
}

/// Decode a `grant` into its bare tag, closing the row [`scheme_agent`]
/// leaves open so the error can enumerate every legal label.
fn permission_label(v: &Value) -> Settled<String> {
    if let Value::Variant {
        label,
        payload: None,
    } = v
        && PERMISSION_LABELS.contains(&label.as_str())
    {
        return Ok(label.clone());
    }
    Err(sig(format!(
        "grant must be one of `confined, `read-only, `edit-only, `reasonable, `dangerous — got {v}"
    )))
}

/// Decode a `schedule` spec's `trigger`, re-running the real parsers
/// ([`CronSchedule::parse`]/[`parse_duration`]) engine-side so a malformed
/// expression carries their own message home before any enquiry crosses.
fn schedule_trigger(v: &Value) -> Settled<FOValue> {
    let Value::Variant {
        label,
        payload: Some(payload),
    } = v
    else {
        return Err(sig(format!(
            "schedule: trigger must be `cron '<5-field-cron-expr>'` or `after '<n><unit>'`, got {v}"
        )));
    };
    let Value::String(expr) = payload.as_ref() else {
        return Err(sig(format!(
            "schedule: `{label}`'s payload must be a Str, got {}",
            payload.type_name()
        )));
    };
    match label.as_str() {
        "cron" => {
            CronSchedule::parse(expr).map_err(|e| sig(format!("schedule: {e}")))?;
            Ok(FOValue::Variant {
                label: "cron".to_string(),
                payload: Some(Box::new(FOValue::String {
                    value: expr.clone(),
                })),
            })
        }
        "after" => {
            parse_duration(expr).map_err(|e| sig(format!("schedule: {e}")))?;
            Ok(FOValue::Variant {
                label: "after".to_string(),
                payload: Some(Box::new(FOValue::String {
                    value: expr.clone(),
                })),
            })
        }
        other => Err(sig(format!(
            "schedule: trigger must be `cron '<5-field-cron-expr>'` or `after '<n><unit>'`, got `{other}`"
        ))),
    }
}

/// Decode a `schedule` spec's `label`: the wakeup's name, required — every
/// schedule now names itself, so there is no default to fall back to.
fn schedule_label(v: &Value) -> Settled<FOValue> {
    let Value::String(name) = v else {
        return Err(sig(format!(
            "schedule: `label` must be a Str naming the wakeup, got {}",
            v.type_name()
        )));
    };
    Ok(FOValue::String {
        value: name.clone(),
    })
}

/// Unwrap the `` `started `` receipt `agent-start`/`agent-hatched` answer with.
fn spawn_receipt(answer: FOValue) -> Settled<Value> {
    let FOValue::Variant {
        label,
        payload: Some(payload),
    } = answer
    else {
        return Err(sig(
            "agent: host answered an unexpected shape for its receipt",
        ));
    };
    if label != "started" {
        return Err(sig(format!("agent: host refused: {label}")));
    }
    Ok(Value::from(*payload))
}

/// Decode `agent-start`'s wire arm's `` `hatch [token, port] `` payload.
fn decode_hatch_answer(payload: FOValue) -> Settled<(u64, u32)> {
    let FOValue::Map { entries } = payload else {
        return Err(sig(
            "agent: host's `hatch answer must be a record carrying token and port",
        ));
    };
    let mut token = None;
    let mut port = None;
    for (key, value) in entries {
        match (key.as_str(), value) {
            // Bit-preserving: the token rides as whatever i64 bits the desk
            // minted, never arithmetic on it.
            ("token", FOValue::Int { value }) => token = Some(value.cast_unsigned()),
            ("port", FOValue::Int { value }) => port = Some(value),
            _ => {}
        }
    }
    let token =
        token.ok_or_else(|| sig("agent: host's `hatch answer is missing `token`".to_string()))?;
    let port =
        port.ok_or_else(|| sig("agent: host's `hatch answer is missing `port`".to_string()))?;
    let port = u32::try_from(port)
        .map_err(|_| sig("agent: host's `hatch answer carries an out-of-range port".to_string()))?;
    Ok((token, port))
}

/// The guest-side hatch itself: spawns the child engine over a fresh vsock
/// dial to the port the desk named, seeded from the nursery-parked fork.
/// Only ever reachable inside a Linux guest — the dial primitive means
/// nothing anywhere else — so a wire trunk built on any other platform
/// refuses here rather than at a silent no-op.
#[cfg(target_os = "linux")]
fn run_hatch(
    port: u32,
    token: u64,
    mooring: &Mooring,
    session: ral_core::types::NurseryId,
    grant: String,
) -> Result<u32, String> {
    ral_core::hatch::hatch_from_nursery(port, token, mooring, session, grant)
}

#[cfg(not(target_os = "linux"))]
fn run_hatch(
    _port: u32,
    _token: u64,
    _mooring: &Mooring,
    _session: ral_core::types::NurseryId,
    _grant: String,
) -> Result<u32, String> {
    Err(
        "agent: this engine has no hatch support outside a Linux guest — a wire trunk's helper \
         spawn only ever reaches one"
            .to_string(),
    )
}

fn enquire_hatched(mooring: &Mooring, shell: &Shell, token: u64) -> Settled<FOValue> {
    Ok(shell.enquire(
        mooring,
        FOValue::Variant {
            label: "agent-hatched".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::Int {
                    value: token.cast_signed(),
                }],
            })),
        },
    )?)
}

#[cfg(target_os = "linux")]
fn kill_hatched(pid: u32) {
    ral_core::hatch::kill_hatched(pid);
}

#[cfg(not(target_os = "linux"))]
fn kill_hatched(_pid: u32) {
    unreachable!("only the Linux run_hatch arm can return a child pid")
}

/// Best effort: the desk drops the pending hatch either way, and there is
/// nothing more useful to do with a refusal here than with success.
fn enquire_abort(mooring: &Mooring, shell: &Shell, token: u64) {
    let _ = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "agent-abort".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::Int {
                    value: token.cast_signed(),
                }],
            })),
        },
    );
}

/// `agent [prompt: …, name: …, type: …, grant: …, search: …]` — validate,
/// fork this shell into the run's nursery, enquire `` `agent-start `` with
/// the parked fork's id; the desk's `launch` is the other half.
///
/// [`scheme_agent`]'s closed row already guarantees the five fields, so the
/// `else` arms below are unreachable through the type checker; they stay
/// didactic rather than trust it alone.
fn builtin_agent(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Map(spec) = &args[0] else {
        return Err(sig(format!(
            "agent: expected a [prompt: …, name: …, type: …, grant: …, search: …] record, got {}",
            args[0].type_name()
        )));
    };
    let Some(prompt) = spec.get("prompt") else {
        return Err(sig(
            "agent: the spec record needs a `prompt` field — the instruction the child starts with",
        ));
    };
    let Some(name) = spec.get("name") else {
        return Err(sig(
            "agent: the spec record needs a `name` field — the child's identity",
        ));
    };
    let Some(kind) = spec.get("type") else {
        return Err(sig(
            "agent: the spec record needs a `type` field — `amnemon or `mnemon",
        ));
    };
    let Some(grant) = spec.get("grant") else {
        return Err(sig(
            "agent: the spec record needs a `grant` field — one of the six permission bases",
        ));
    };
    let Some(search) = spec.get("search") else {
        return Err(sig(
            "agent: the spec record needs a `search` field — whether the child may use the \
             provider's built-in web search",
        ));
    };

    let name = name.to_string();
    if !valid_name(&name) {
        return Err(sig(format!(
            "agent: `name` must be non-empty, at most 24 characters, and only ASCII letters, \
             digits, `-`, or `_` (the tab-bar contract) — got {name:?}"
        )));
    }
    let kind = agent_type_label(kind)?;
    let grant = permission_label(grant)?;
    let Value::Bool(search) = search else {
        return Err(sig(format!(
            "agent: `search` must be a Bool — got {}",
            search.type_name()
        )));
    };
    let prompt = prompt.to_string();

    let session = shell.fork_into_nursery(mooring)?;
    // `Nursery::park` mints ids from a monotonic per-run counter, so this
    // never saturates; `unwrap_or` keeps the door total without an `as`
    // cast's silent wraparound.
    let session_id = i64::try_from(session.0).unwrap_or(i64::MAX);
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "agent-start".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::Int { value: session_id },
                    FOValue::Variant {
                        label: kind,
                        payload: None,
                    },
                    FOValue::String { value: prompt },
                    FOValue::String { value: name },
                    FOValue::Variant {
                        label: grant.clone(),
                        payload: None,
                    },
                    FOValue::Bool { value: *search },
                ],
            })),
        },
    )?;

    let FOValue::Variant { label, payload } = answer else {
        return Err(sig(
            "agent: host answered an unexpected shape for its receipt",
        ));
    };
    if label != "hatch" {
        return spawn_receipt(FOValue::Variant { label, payload });
    }
    // The wire arm: the trunk's own engine hatches the child itself, then
    // enquires `agent-hatched` to hand the desk its dial to await.
    let Some(payload) = payload else {
        return Err(sig("agent: host's `hatch answer carries no payload"));
    };
    let (token, port) = decode_hatch_answer(*payload)?;
    match run_hatch(port, token, mooring, session, grant) {
        Ok(pid) => match enquire_hatched(mooring, shell, token) {
            Ok(answer) => spawn_receipt(answer),
            Err(e) => {
                // The dial never landed in time, or landed and was refused:
                // either way the desk has given up, so this engine must not
                // leave the hatched child dialling into silence.
                kill_hatched(pid);
                Err(e)
            }
        },
        Err(reason) => {
            enquire_abort(mooring, shell, token);
            Err(sig(format!("agent: {reason}")))
        }
    }
}

/// `agents` — the `` `agent-list `` enquiry's listing, returned as is.
#[allow(
    clippy::unnecessary_wraps,
    reason = "registered as a `BuiltinBody::Static` fn pointer; the `Settled<Value>` return is the shape the builtin table dispatches through, not a choice of this body"
)]
fn builtin_agents(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "agent-list".to_string(),
            payload: None,
        },
    )?;
    let FOValue::List { items } = answer else {
        return Err(sig(
            "agents: host answered an unexpected shape for the listing",
        ));
    };
    Ok(Value::list(items.into_iter().map(Value::from).collect()))
}

/// `message <name> <text>` — enquires `` `message ``; name resolution,
/// descendant-scoping, and delivery errors all belong to the desk.
fn builtin_message(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let name = args[0].to_string();
    let text = args[1].to_string();
    shell.enquire(
        mooring,
        FOValue::Variant {
            label: "message".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![
                    FOValue::String { value: name },
                    FOValue::String { value: text },
                ],
            })),
        },
    )?;
    Ok(Value::Unit)
}

/// The tags `agent-cancel` answers, and `unschedule`'s. Each list is the one
/// source for both the builtin's type ([`closed_variant`]) and the runtime
/// check on what the desk actually sent ([`tag_receipt`]), so the row a caller
/// may match on and the row the host may answer cannot come apart.
const AGENT_CANCEL_TAGS: [&str; 2] = ["cancelled", "no-such-agent"];
const UNSCHEDULE_TAGS: [&str; 2] = ["removed", "no-such-label"];

/// Check a host answer is one of the bare tags the builtin's own type admits.
/// The desk is trusted to answer in shape but never assumed to, as
/// [`schedule_receipt`] has it: a tag outside the row would reach the caller
/// as something the scheme promised could not arrive.
fn tag_receipt(answer: FOValue, verb: &str, tags: &[&str]) -> Settled<Value> {
    match &answer {
        FOValue::Variant {
            label,
            payload: None,
        } if tags.contains(&label.as_str()) => Ok(Value::from(answer)),
        _ => Err(sig(format!(
            "{verb}: host answered an unexpected shape for its receipt"
        ))),
    }
}

/// `agent-cancel <name>` — enquires `` `agent-cancel ``; resolution and
/// descendant-scoping are the desk's.
fn builtin_agent_cancel(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let name = args[0].to_string();
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "agent-cancel".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: name }],
            })),
        },
    )?;
    tag_receipt(answer, "agent-cancel", &AGENT_CANCEL_TAGS)
}

/// Check the `` `schedule `` receipt is the record shape before handing it on.
fn schedule_receipt(answer: FOValue) -> Settled<Value> {
    let FOValue::Map { .. } = answer else {
        return Err(sig(
            "schedule: host answered an unexpected shape for its receipt",
        ));
    };
    Ok(Value::from(answer))
}

/// `schedule <spec>` — decodes through
/// [`schedule_trigger`]/[`schedule_label`], then enquires `` `schedule ``.
/// The self-wakeup grant and label uniqueness are refusals the desk and the
/// schedule registry own.
fn builtin_schedule(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let Value::Map(spec) = &args[0] else {
        return Err(sig(format!(
            "schedule: expected a [trigger: …, label: …, prompt: …] record, got {}",
            args[0].type_name()
        )));
    };
    let Some(trigger) = spec.get("trigger") else {
        return Err(sig(
            "schedule: the spec record needs a `trigger` field — `cron '<expr>' or `after '<dur>'",
        ));
    };
    let Some(label) = spec.get("label") else {
        return Err(sig(
            "schedule: the spec record needs a `label` field — a Str naming the wakeup",
        ));
    };
    let Some(prompt) = spec.get("prompt") else {
        return Err(sig(
            "schedule: the spec record needs a `prompt` field — the instruction delivered when the wakeup fires",
        ));
    };
    let trigger = schedule_trigger(trigger)?;
    let label = schedule_label(label)?;
    let prompt = prompt.to_string();

    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "schedule".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![trigger, label, FOValue::String { value: prompt }],
            })),
        },
    )?;
    schedule_receipt(answer)
}

/// `schedules` — the `` `schedule-list `` enquiry's listing; the
/// self-wakeup grant refusal is the desk's.
#[allow(
    clippy::unnecessary_wraps,
    reason = "registered as a `BuiltinBody::Static` fn pointer; the `Settled<Value>` return is the shape the builtin table dispatches through, not a choice of this body"
)]
fn builtin_schedules(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "schedule-list".to_string(),
            payload: None,
        },
    )?;
    let FOValue::List { items } = answer else {
        return Err(sig(
            "schedules: host answered an unexpected shape for the listing",
        ));
    };
    Ok(Value::list(items.into_iter().map(Value::from).collect()))
}

/// `unschedule <label>` — enquires `` `unschedule ``; resolution and the
/// grant refusal are the desk's.
fn builtin_unschedule(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let label = args[0].to_string();
    let answer = shell.enquire(
        mooring,
        FOValue::Variant {
            label: "unschedule".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: label }],
            })),
        },
    )?;
    tag_receipt(answer, "unschedule", &UNSCHEDULE_TAGS)
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

/// `reply <value>` — `FOValue::try_from` runs before any enquiry crosses,
/// so a non-first-order value fails this call alone and leaves the session
/// running; the refusal for a non-returning agent is the desk's.
fn builtin_reply(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let payload = FOValue::try_from(&args[0]).map_err(|_| {
        sig(
            "reply: the value must be first-order data — no closures, handles, or \
             environments — since it crosses to whoever spawned you as plain data",
        )
    })?;
    shell.enquire(
        mooring,
        FOValue::Variant {
            label: "reply".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![payload],
            })),
        },
    )?;
    Ok(Value::Unit)
}

fn spawn_receipt_ty() -> Ty {
    closed_record(&[("name", Ty::String), ("log-dir", Ty::String)])
}

/// `agent :: ∀ρ1 ρ2. [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, search: Bool] → F [name: Str, log-dir: Str]`
///
/// The record row is closed because a record literal with literal keys
/// infers an exact one (`infer_map_val` builds on `Row::Empty`), so a
/// missing or misspelled field is a static error naming it.
///
/// The `type` and `grant` rows *inside* stay open, because a literal tag
/// infers its own open row: closing them would make `` `bogus `` a bare
/// row-mismatch diagnostic that never reaches
/// [`agent_type_label`]/[`permission_label`], which enumerate the legal
/// labels. `search` is two-state rather than an enumeration, so `Ty::Bool`
/// closes it outright.
fn scheme_agent(u: &mut Unifier) -> Scheme {
    let type_row = u.fresh_row_var();
    let grant_row = u.fresh_row_var();
    scheme(
        &[],
        &[],
        &[type_row, grant_row],
        thunk(fun(
            closed_record(&[
                ("prompt", Ty::String),
                ("name", Ty::String),
                ("type", Ty::Variant(Row::Var(type_row))),
                ("grant", Ty::Variant(Row::Var(grant_row))),
                ("search", Ty::Bool),
            ]),
            pure(spawn_receipt_ty()),
        )),
    )
}

/// `agents :: F [[name: Str, elapsed-s: Int, log-dir: Str]]`
fn scheme_agents(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(pure(Ty::List(Box::new(closed_record(&[
            ("name", Ty::String),
            ("elapsed-s", Ty::Int),
            ("log-dir", Ty::String),
        ]))))),
    )
}

/// `message :: Str → Str → F Unit`
fn scheme_message(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::String, fun(Ty::String, pure(Ty::Unit)))),
    )
}

/// A variant type over a closed row of bare tags, each carrying `Ty::Unit` —
/// [`closed_record`]'s counterpart for an enumeration rather than a set of
/// fields. `agent-cancel` and `unschedule` each answer one of exactly two
/// such tags, so the row is closed: a third arm would be a static error,
/// not a runtime one.
fn closed_variant(labels: &[&str]) -> Ty {
    use ral_core::syntax::tag::tag_row_label;
    let mut row = Row::Empty;
    for label in labels.iter().rev() {
        row = Row::Extend(tag_row_label(label), Box::new(Ty::Unit), Box::new(row));
    }
    Ty::Variant(row)
}

/// `agent-cancel :: Str → F <cancelled | no-such-agent>`
fn scheme_agent_cancel(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::String, pure(closed_variant(&AGENT_CANCEL_TAGS)))),
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

fn schedule_receipt_ty() -> Ty {
    closed_record(&[("label", Ty::String), ("next-s", Ty::Int)])
}

/// `schedule :: ∀ρ. [trigger: Variant ρ, label: Str, prompt: Str] → F [label: Str, next-s: Int]`
///
/// Closed record row, an open variant row for `trigger` alone, for the reason
/// [`scheme_agent`] gives: an unknown trigger must reach [`schedule_trigger`],
/// which names the legal shapes. `label` is a plain `Str` — every schedule
/// names itself, so there is no shape left to leave open.
fn scheme_schedule(u: &mut Unifier) -> Scheme {
    let trigger_row = u.fresh_row_var();
    scheme(
        &[],
        &[],
        &[trigger_row],
        thunk(fun(
            closed_record(&[
                ("trigger", Ty::Variant(Row::Var(trigger_row))),
                ("label", Ty::String),
                ("prompt", Ty::String),
            ]),
            pure(schedule_receipt_ty()),
        )),
    )
}

/// `schedules :: F [[label: Str, trigger: Str, next-s: Int, fires: Int]]`
fn scheme_schedules(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(pure(Ty::List(Box::new(schedule_row_ty())))),
    )
}

/// `unschedule :: Str → F <removed | no-such-label>`
fn scheme_unschedule(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::String, pure(closed_variant(&UNSCHEDULE_TAGS)))),
    )
}

/// `reply :: ∀α. α → F Unit` — first-orderness is [`builtin_reply`]'s
/// runtime door, not a static constraint on `α`.
fn scheme_reply(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    scheme(&[av], &[], &[], thunk(fun(Ty::Var(av), pure(Ty::Unit))))
}

/// `pin-read :: ∀α. String → F α` — the `from-json` precedent
/// (`TyTemplate::Any`, `core/src/typecheck/builtins.rs:391`): trusted, not
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
static HARNESS_BUILTINS_ARR: [BuiltinEntry; 14] = [
    BuiltinEntry::new(
        Cow::Borrowed("agent"),
        BuiltinTypeRule::Scheme(scheme_agent),
        "agent [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>]  — launch a sub-agent. Launch-only and always asynchronous: returns immediately with a receipt [name: Str, log-dir: Str]; the child's reply is NOT this call's result — it arrives later, as its own marked item in your inbox. `type` selects the child's memory: `amnemon` starts blank (no shared history), while `mnemon` inherits your current model-visible conversation and reuses your provider selection for cache locality. Every child receives the value-snapshot of the parent's bindings, cwd, and env — `mnemon` too; the serializable fragment crosses, while a live job handle becomes an opaque placeholder. `prompt` is a computed string and becomes the child's fresh final prompt. Keep large material in a named binding rather than splicing it into prompt; small, certainly-needed material may still be spliced. Wrap `prompt` in a raw string #'…'# if it carries $, !, or quotes. `name` is the child's identity — non-empty, at most 24 characters, ASCII letters/digits/-/_ only — and must not be borne by any live agent, or the call is refused; pick something descriptive, like 'fix-parser-tests'. `grant` bounds the child to at most your own authority and must be exactly one of `confined (offline, no home reads), `read-only (writes only to scratch), `edit-only (edits the working tree, no build tooling), `reasonable (everyday tooling), `dangerous (no narrowing); any other label is refused, naming all five. `search` states whether the child may use the provider's own built-in web search, bounded above by your own — asking for it when you do not have it silently yields a child without it. Delegation depth is finite — each descendant is handed one less unit of fuel than its spawner holds, and once fuel reaches zero this call is refused; fuel bounds how deep a chain may recurse, never how many children you may start at any one depth. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_agent),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("agents"),
        BuiltinTypeRule::Scheme(scheme_agents),
        "agents  — list the live descendants you started that are still running: [[name: Str, elapsed-s: Int, log-dir: Str]]. Use it to recover names after a context compaction, then agent-cancel to stop a straggler. Settled agents are not listed — their replies arrive on their own as marked items in your inbox. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_agents),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("message"),
        BuiltinTypeRule::Scheme(scheme_message),
        "message <name> <text>  — send `text` as a marked item to the live descendant named `name` (from agents or a spawn receipt); it lands at its next exchange boundary, not as human input. Only a descendant of yours may receive it — never a sibling, an ancestor, or yourself; refused otherwise. Does not return the recipient's answer — coordination only. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_message),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("agent-cancel"),
        BuiltinTypeRule::Scheme(scheme_agent_cancel),
        "agent-cancel <name>  — cancel the live descendant named `name` (from agents). It is asked to stop at its next checkpoint and then delivers a cancelled result to your inbox. Answers `cancelled if a live agent bore that name, `no-such-agent if none did — branch on it rather than assuming the name reached anyone. Only a descendant of yours may be cancelled — never a sibling, an ancestor, or yourself; refused otherwise. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_agent_cancel),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("schedule"),
        BuiltinTypeRule::Scheme(scheme_schedule),
        "schedule <spec>  — arm a self-wakeup: at the chosen time a marked item carrying the spec's `prompt` is delivered to your inbox and re-engages you at your next exchange boundary, with no human present. `spec` is a record with exactly three fields: trigger, label, prompt. `trigger` is exactly one of `cron '<expr>'` — a five-field cron expression (minute hour day-of-month month day-of-week) in the host's local timezone, e.g. `cron '0 9 * * 1-5'` for weekdays at 09:00; recurring — or `after '<n><unit>'` — a one-shot relative delay, unit one of s/m/h/d, e.g. `after '30m'`, `after '2h'`; any other shape is refused, naming both. `label` is a Str naming the wakeup — its identity: it must not be borne by another live schedule, and you must always supply one. `prompt` is the natural-language instruction you act on when woken, not code — e.g. schedule [trigger: `after '30m', label: 'nightly', prompt: 'check the build']. Returns a receipt [label: Str, next-s: Int] — next-s is the seconds until the first fire; read it back to catch a cron expression that parsed but does not mean what you meant. Requires the self-wakeup grant (--allow-schedule) — an agent that can wake itself indefinitely holds real authority, so without the grant this call is refused. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_schedule),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("schedules"),
        BuiltinTypeRule::Scheme(scheme_schedules),
        "schedules  — list your live scheduled wakeups: [[label: Str, trigger: Str, next-s: Int, fires: Int]] — next-s the seconds until the next fire, fires how many times it has fired so far. Use it to recover labels after a context compaction, then unschedule to remove one. Requires the self-wakeup grant (--allow-schedule) and is refused without it. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_schedules),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("unschedule"),
        BuiltinTypeRule::Scheme(scheme_unschedule),
        "unschedule <label>  — remove a scheduled wakeup by its label (from schedules or a schedule receipt). Answers `removed if a live schedule bore that label, `no-such-label if none did — branch on it rather than assuming the label reached anything. Requires the self-wakeup grant (--allow-schedule) and is refused without it. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_unschedule),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("reply"),
        BuiltinTypeRule::Scheme(scheme_reply),
        "reply <value>  — hand `value` back to whoever spawned you: the sole return path for a returning agent. Your parent receives exactly this value, nothing else — not your reasoning, your shell bindings, or any prose you streamed along the way. `value` must be first-order data: no closures, handles, or environments; passing one fails this call with a didactic error and your run continues, so fix the value and call reply again. Call it more than once in an exchange and the last call wins — an earlier value is discarded, not appended. The run does not end at this call: it ends once the enclosing ral call's whole batch of statements finishes draining, so write reply last and let earlier statements in the same script run to completion first. A non-finite Float (NaN, +Infinity, -Infinity) reaches your parent as the string \"NaN\"/\"Infinity\"/\"-Infinity\" — JSON, which the value eventually crosses into, has no such numbers. Refused on the interactive trunk and every /branch child: they converse with the user turn after turn and never return, so they hold no obligation to call this. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_reply),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("pin-read"),
        BuiltinTypeRule::Scheme(scheme_pin_read),
        "pin-read <key>  — the card currently pinned under KEY on your register, as a `card value you can destructure, or unit if the slot is empty. Reads your own register only. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_pin_read),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("pin-list"),
        BuiltinTypeRule::Scheme(scheme_pin_list),
        "pin-list  — the keys currently occupied on your pin register, as [String]. Read one back with pin-read. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_pin_list),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("context"),
        BuiltinTypeRule::Scheme(scheme_context),
        "context  — survey the finite, addressable model context. Returns [spans: [[exchange: Int, kind: Str, prompt: Str, bytes: Int, steps: Int, live: Bool]], total-bytes: Int, total-steps: Int, cache: Str]. Each span is an exchange or import, and a digest is represented by its reach in `exchange`; `prompt` is its opening line, `bytes` its serialized weight, `steps` its provider-step count, and `live` marks the exchange currently in progress. The cache sentence explains that editing before the cache watermark re-reads the prefix on the next request. This is a survey: it does not edit context.",
        BuiltinBody::Static(builtin_context),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("context-read"),
        BuiltinTypeRule::Scheme(scheme_context_read),
        "context-read <exchanges>  — read named closed exchanges as one transcript Str, with roles marked and steps delimited. The list must be non-empty; name a digest by its reach, and do not name an exchange folded strictly inside that digest. Only stdout echoes into a turn's tool result; a `let`-bound value prints nothing. Binding is silent, but both stdout and a tool call's final VALUE enter model context; read large bindings in slices — never bare-print a whole binding merely to inspect it, because the transcript is material, not a survey.",
        BuiltinBody::Static(builtin_context_read),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("context-drop"),
        BuiltinTypeRule::Scheme(scheme_context_drop),
        "context-drop <exchanges>  — shed whole closed exchanges from the model context, where <exchanges> is a list of non-negative exchange numbers from `context`. The live exchange cannot be named, unknown or already-folded exchanges are refused with an explanation, and an empty list is not an edit. The model is always mid-exchange when it speaks, so a rewind-shaped request for a suffix of closed exchanges is this verb with a range; there is no context-rewind. Returns [bytes-delta: Int], the serialized model-view bytes before minus after (a negative value is honest when a digest is larger than what it replaces). Applied immediately at the desk; the edit is recorded as a model context event.",
        BuiltinBody::Static(builtin_context_drop),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("context-fold"),
        BuiltinTypeRule::Scheme(scheme_context_fold),
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
    fn valid_name_boundaries() {
        assert!(valid_name("a"));
        assert!(valid_name("refactor-output"));
        assert!(valid_name("audit_deps"));
        assert!(valid_name(&"x".repeat(24)));
        assert!(!valid_name(""));
        assert!(!valid_name(&"x".repeat(25)));
        assert!(!valid_name("has space"));
        assert!(!valid_name("non-ascii-é"));
    }

    #[test]
    fn permission_label_accepts_every_bake_in() {
        for label in PERMISSION_LABELS {
            let v = Value::Variant {
                label: label.to_string(),
                payload: None,
            };
            assert_eq!(permission_label(&v).unwrap(), label);
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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"agent [prompt: #'hi'#, name: 't', type: `amnemon, grant: `bogus, search: true]",
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
            session.agents.list(session.id).is_empty(),
            "an unknown grant label must never register a child"
        );
    }

    #[test]
    fn unknown_type_tag_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"agent [prompt: #'hi'#, name: 't', type: `bogus, grant: `confined, search: true]",
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
            session.agents.list(session.id).is_empty(),
            "an unknown type tag must never register a child"
        );
    }

    #[test]
    fn invalid_name_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r#"agent [prompt: #'hi'#, name: "has space", type: `amnemon, grant: `confined, search: true]"#,
            5,
            &emit,
        );
        assert!(result.content.contains("name"), "got: {}", result.content);
        assert!(
            session.agents.list(session.id).is_empty(),
            "an invalid name must never register a child"
        );
    }

    /// Static, not a door error: `scheme_agent`'s closed record row reports
    /// which label is absent.
    #[test]
    fn missing_agent_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r"agent [prompt: #'hi'#, name: 't', type: `amnemon, search: true]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("field named 'grant'"),
            "the diagnostic must name the missing field, got: {}",
            result.content
        );
        assert!(
            session.agents.list(session.id).is_empty(),
            "a missing spec field must never register a child"
        );
    }

    /// Drives `run_shell` rather than `Agent::deliberate`'s provider loop:
    /// the spawn seeds the child's handle from the parent's *own*
    /// `Arc<Provider>`, so one script consumed by both a driven parent
    /// exchange and its child races over which gets which stage.
    #[test]
    fn agent_full_stack_round_trip_delivers_receipt_and_settles_into_inbox() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "reply-1",
                    "reply 'say hi'",
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"agent [prompt: #'say hi'#, name: 'helper', type: `amnemon, grant: `read-only, search: false]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("helper"),
            "the receipt record must be the run's value, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    assert!(
                        r.text.contains("say hi"),
                        "the child's reply must settle into the parent's inbox, got: {}",
                        r.text
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
    }

    // ── schedule family door tests ───────────────────────────────────────
    //
    // Tag payloads are greedy, but `at_tag_payload_end` in
    // `core/src/syntax/parser.rs` stops one at a comma — so inside a record
    // literal a nullary tag cannot swallow its neighbour. That is why
    // `schedule` takes one spec record, not three positional arguments.

    #[test]
    fn bad_cron_expr_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `cron '* * * *', label: 'nightly', prompt: #'wake'#]",
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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after 'nope', label: 'nightly', prompt: #'wake'#]",
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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `bogus 'x', label: 'nightly', prompt: #'wake'#]",
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

    /// Static, not a door error: `scheme_schedule`'s closed record row
    /// reports which label is absent.
    #[test]
    fn missing_spec_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '1s', label: 'nightly']",
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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '1s', label: 'nightly', prompt: #'wake'#, extra: 1]",
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

    /// The wait is generous because the fire really is a wall-clock second
    /// away: `parse_duration`'s smallest unit is whole seconds.
    #[test]
    fn schedule_full_stack_round_trip_answers_receipt_and_fires_into_inbox() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        session.allow_schedule = true;
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '1s', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid schedule call must succeed, got: {}",
            result.content
        );
        assert!(
            result.content.contains("next-s"),
            "the receipt record must be the run's value, got: {}",
            result.content
        );
        let live = session.schedules.list();
        assert_eq!(live.len(), 1, "the schedule must be registered");
        assert_eq!(live[0].label, "nightly", "must take the given label");
        assert!(
            result.content.contains("nightly"),
            "the receipt must carry the given label, got: {}",
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

    #[test]
    fn unschedule_full_stack_removes_by_label() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        session.allow_schedule = true;
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid schedule call must succeed, got: {}",
            result.content
        );
        assert_eq!(
            session.schedules.list().len(),
            1,
            "the schedule must be registered"
        );

        let result = session.run_shell("call-2".to_string(), "unschedule 'nightly'", 5, &emit);
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid unschedule call must succeed, got: {}",
            result.content
        );
        assert!(
            result.content.contains("`removed"),
            "a hit must answer `removed, got: {}",
            result.content
        );
        assert!(
            session.schedules.list().is_empty(),
            "unschedule by label must remove the schedule"
        );

        let miss = session.run_shell("call-3".to_string(), "unschedule 'nightly'", 5, &emit);
        assert!(
            miss.content.contains("`no-such-label"),
            "a miss must answer `no-such-label, got: {}",
            miss.content
        );
    }

    // ── `reply` ───────────────────────────────────────────────────────────

    /// The record must reach the parent's inbox structured, not flattened
    /// to a string. The child's script does the replying, for the
    /// undriven-parent reason
    /// `agent_full_stack_round_trip_delivers_receipt_and_settles_into_inbox`
    /// gives.
    #[test]
    fn reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"let found = ["a.rs", "b.rs"]; reply [files: $found]"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"agent [prompt: #'find files'#, name: 'finder', type: `amnemon, grant: `read-only, search: false]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("finder"),
            "the receipt record must be the run's value, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    assert!(
                        r.text.contains("files:")
                            && r.text.contains("a.rs")
                            && r.text.contains("b.rs"),
                        "the structured record must reach the parent inbox, got: {}",
                        r.text
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
    }

    /// The refusal is an ordinary call error, not a termination: a later,
    /// well-formed `reply` still succeeds.
    #[test]
    fn reply_refuses_a_non_first_order_value_and_does_not_terminate() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell("call-1".to_string(), "reply { echo hi }", 5, &emit);
        assert!(
            result.content.contains("first-order"),
            "must name the first-order rule, got: {}",
            result.content
        );

        let ok = session.run_shell("call-2".to_string(), "reply 42", 5, &emit);
        assert!(
            ok.content.contains("EXIT: 0"),
            "the session must still be usable after a refused reply, got: {}",
            ok.content
        );
    }

    #[test]
    fn double_reply_in_one_exchange_is_last_wins() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"reply "first"; reply "second""#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"surface `pin [key: "note", body: `card ["hi there"]]; reply !{pin-read "note"}"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"agent [prompt: #'pin and read back'#, name: 'pinner', type: `amnemon, grant: `read-only, search: false]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("pinner"),
            "the receipt record must be the run's value, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    // The pretty-printer elides past its depth cap, so the
                    // span text itself does not survive to this rendering;
                    // what proves the round trip *canonical* — a lifted
                    // `` `text `` mark, not the bare-string sugar it was
                    // authored with — does.
                    assert!(
                        r.text.contains("`card") && r.text.contains("`text [spans:"),
                        "the canonical card must reach the parent, got: {}",
                        r.text
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
    }

    /// An absent key answers `Unit`, which crosses to the parent as `reply`'s
    /// empty rendering.
    #[test]
    fn pin_read_full_stack_absent_key_replies_unit() {
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"reply !{pin-read "nope"}"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"agent [prompt: #'read an absent key'#, name: 'reader', type: `amnemon, grant: `read-only, search: false]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("reader"),
            "the receipt record must be the run's value, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    assert_eq!(
                        r.text.trim(),
                        "",
                        "an absent key must reply unit, got: {}",
                        r.text
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

        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        session.run_shell("call-1".to_string(), r#"add-task "parent task""#, 5, &emit);

        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"add-task "child task"; reply "done""#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);

        let result = session.run_shell(
            "call-2".to_string(),
            r"agent [prompt: #'add a task'#, name: 'tasker', type: `amnemon, grant: `read-only, search: false]",
            5,
            &emit,
        );
        assert!(
            result.content.contains("tasker"),
            "the receipt record must be the run's value, got: {}",
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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        session.run_shell(
            "call-1".to_string(),
            r#"pin-set "services" `card [`text [spans: [[text: "nope"]]]]"#,
            5,
            &emit,
        );
        let saw_refusal = std::iter::from_fn(|| rx.try_recv().ok()).any(|event| {
            matches!(&event.kind, crate::bus::Kind::Error(msg) if msg.contains("protected service-ledger pin"))
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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

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
        let mut session = crate::agent::Agent::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

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
