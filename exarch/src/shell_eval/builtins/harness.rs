//! The agent- and schedule-family harness builtins: `agent`, `agents`,
//! `message`, `agent-cancel`, `schedule`, `schedules`, `unschedule`,
//! `reply` — the model's action surface onto the
//! `` `agent-start ``/`` `agent-list ``/`` `agent-cancel ``/`` `message ``/
//! `` `schedule ``/`` `schedule-list ``/`` `unschedule ``/`` `reply ``
//! enquiry classes [`crate::fleet::desk::ExarchDesk`] answers.
//!
//! Each body validates its own arguments at the door — arity, the name
//! contract, the six-label grant vocabulary, the trigger/label
//! vocabularies — before it ever forks a session or puts an enquiry, so a
//! malformed call never reaches the host. The spawning body (`agent`) forks
//! this shell into the run's nursery
//! ([`ral_core::Shell::fork_into_nursery`]) and enquires with the adopted
//! session's id; every other body enquires directly. The desk answers class
//! by class in `crate::fleet::desk`; this module only ever crosses that one seam.

use crate::fleet::schedule::{CronSchedule, parse_duration};
use ral_core::builtins::util::check_arity;
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{
    BuiltinTypeRule, closed_record, fun, mk_scheme as scheme, pure, thunk,
};
use ral_core::typecheck::{Row, Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled, sig};
use ral_core::{Shell, Value};
use std::borrow::Cow;

/// The six bake-in permission bases a spawn's `grant` argument may name —
/// see `crate::policy::base::resolve_base`, whose profiles these mirror
/// exactly.
const PERMISSION_LABELS: [&str; 6] = [
    "confined",
    "minimal",
    "read-only",
    "edit-only",
    "reasonable",
    "dangerous",
];

/// True for names that fit the tab-bar contract — non-empty, ≤24 chars,
/// ASCII alphanumeric plus `-`/`_`. Spaces and punctuation are excluded
/// because the tab bar lays them out token-by-token. The `agent` builtin's
/// `name` argument is mandatory — it identifies the child everywhere the
/// model can see it, not just on the tab bar, so there is no default to
/// fall back to; a door that can refuse simply refuses.
pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Decode an `agent` spawn's `type` argument into its bare tag, or a door
/// error naming both legal memory modes. The argument's own type is
/// row-open (see [`scheme_agent`]'s doc for why), so this runtime check is
/// what actually closes the rule — the same shape [`permission_label`]
/// closes for `grant`.
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

/// Decode a `grant` argument into its bare tag, or a door error naming all
/// six legal labels. The argument's own type is row-open (see
/// [`scheme_agent`]'s doc for why), so this runtime check is what actually
/// closes the rule.
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
        "grant must be one of `confined, `minimal, `read-only, `edit-only, `reasonable, `dangerous — got {v}"
    )))
}

/// Decode a `schedule` spec's `trigger` field — `` `cron '<expr>' `` or
/// `` `after '<dur>' `` — re-running the real parsers
/// ([`CronSchedule::parse`]/[`parse_duration`]) engine-side so a malformed
/// expression or duration errors, carrying the parser's own message, before
/// any enquiry crosses. The field's variant row is open (see
/// [`scheme_schedule`]'s doc for why), so this runtime check is what
/// actually closes the rule.
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

/// Decode a `schedule` spec's `label` field — `` `some '<name>' `` or
/// `` `none `` — into the [`FOValue`] the desk expects, or a door error
/// naming both legal shapes. The field's variant row is open, for the
/// same reason [`schedule_trigger`] and `permission_label` hold theirs
/// open.
fn schedule_label(v: &Value) -> Settled<FOValue> {
    match v {
        Value::Variant {
            label,
            payload: None,
        } if label == "none" => Ok(FOValue::Variant {
            label: "none".to_string(),
            payload: None,
        }),
        Value::Variant {
            label,
            payload: Some(payload),
        } if label == "some" => {
            let Value::String(name) = payload.as_ref() else {
                return Err(sig(format!(
                    "schedule: `some`'s payload must be a Str, got {}",
                    payload.type_name()
                )));
            };
            Ok(FOValue::Variant {
                label: "some".to_string(),
                payload: Some(Box::new(FOValue::String {
                    value: name.clone(),
                })),
            })
        }
        other => Err(sig(format!(
            "schedule: label must be `some '<name>'` or `none`, got {other}"
        ))),
    }
}

/// Decode a spawn enquiry's `` `started `` receipt into the record the
/// builtin returns, or a didactic error naming the shape violation or the
/// refusal — the builtin-body half of [`builtin_agent`]'s spawn.
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

/// `agent [prompt: …, name: …, type: …, grant: …, search: …]` — validate the
/// door, fork this shell into the run's nursery, and enquire
/// `` `agent-start `` with the adopted session's id — the builtin-body half
/// of a spawn, the desk's own launch spine
/// ([`crate::fleet::desk::ExarchDesk::launch`]) the other half. The argument
/// arrives as a [`Value::Map`] — the closed record row [`scheme_agent`]
/// mints guarantees the five fields statically — but the `else` arms below
/// stay didactic anyway, matching [`builtin_schedule`]'s own style: a
/// defensive door costs nothing and never has to trust the type checker
/// alone.
fn builtin_agent(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "agent")?;
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
    // A NurseryId is a small monotonic per-run counter; `unwrap_or` never
    // actually saturates in practice, but keeps this door total without an
    // `as` cast's silent wraparound.
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
                        label: grant,
                        payload: None,
                    },
                    FOValue::Bool { value: *search },
                ],
            })),
        },
    )?;
    spawn_receipt(answer)
}

/// `agents` — thin wrapper around the `` `agent-list `` enquiry: silent, no
/// chrome; the answer's listing is the return value directly.
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

/// `message <name> <text>` — passes `name` through as the recipient's
/// identity, then enquires `` `message ``; resolution, descendant-scoping,
/// and delivery errors are the desk's.
fn builtin_message(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "message")?;
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

/// `agent-cancel <name>` — passes `name` through as the target's identity,
/// then enquires `` `agent-cancel ``; resolution and descendant-scoping are
/// the desk's.
fn builtin_agent_cancel(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "agent-cancel")?;
    let name = args[0].to_string();
    shell.enquire(
        mooring,
        FOValue::Variant {
            label: "agent-cancel".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: name }],
            })),
        },
    )?;
    Ok(Value::Unit)
}

/// Decode the `schedule` enquiry's receipt — a `` `[label: Str, next-s: Int] ``
/// record — into the value the builtin returns, or a didactic error naming
/// the shape violation, mirroring [`spawn_receipt`]'s own style.
fn schedule_receipt(answer: FOValue) -> Settled<Value> {
    let FOValue::Map { .. } = answer else {
        return Err(sig(
            "schedule: host answered an unexpected shape for its receipt",
        ));
    };
    Ok(Value::from(answer))
}

/// `schedule <spec>` — decodes the spec record's `trigger`/`label` fields
/// through [`schedule_trigger`]/[`schedule_label`] (re-running the real
/// parsers so a malformed expression fails here, not at the desk), then
/// enquires `` `schedule ``. The grant refusal and the label-uniqueness /
/// reserved-namespace refusals are the desk's/registry's.
fn builtin_schedule(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "schedule")?;
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
            "schedule: the spec record needs a `label` field — `some '<name>' or `none",
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

/// `schedules` — thin wrapper around the `` `schedule-list `` enquiry:
/// silent, no chrome; the grant refusal is the desk's.
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

/// `unschedule <label>` — passes `label` through as the target schedule's
/// identity, then enquires `` `unschedule ``; resolution (including the
/// no-op case) and the grant refusal are the desk's/registry's.
fn builtin_unschedule(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "unschedule")?;
    let label = args[0].to_string();
    shell.enquire(
        mooring,
        FOValue::Variant {
            label: "unschedule".to_string(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: label }],
            })),
        },
    )?;
    Ok(Value::Unit)
}

/// `reply <value>` — the first-orderness door runs [`FOValue::try_from`]
/// before any enquiry crosses, so a violation fails only this call,
/// engine-side; the non-returning refusal is the desk's.
fn builtin_reply(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "reply")?;
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

/// The `[name: Str, log-dir: Str]` receipt the `agent` builtin answers with.
fn spawn_receipt_ty() -> Ty {
    closed_record(&[("name", Ty::String), ("log-dir", Ty::String)])
}

/// `agent :: ∀ρ1 ρ2. [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, search: Bool] → F [name: Str, log-dir: Str]`
///
/// The record row is closed: a record literal with literal keys infers an
/// exact row (`infer_map_val` builds on `Row::Empty`), so unifying it
/// against this closed row makes a missing or misspelled field a static
/// error naming that field — the same rationale [`scheme_schedule`] gives
/// for its own closed record row. `search` joins `prompt`/`name`/`type`/`grant`
/// as a fifth required field, exactly `grant`'s own character: a spawn
/// states its child's powers rather than inheriting them silently, and
/// whether the child may reach the provider's built-in web search is one
/// more such power.
///
/// The two variant rows *inside* the record — `type` and `grant` — stay
/// open: a literal tag
/// infers its *own* open row (`` `bogus `` infers `` [`bogus: Unit | ρ] ``,
/// `typecheck::infer`'s doc on `Val::Variant`), so unifying it against a
/// *closed* row here would make an unknown label a static type error —
/// sound, but the wrong failure mode: it would never reach
/// [`agent_type_label`]/[`permission_label`], so the model would see a bare
/// row-mismatch diagnostic instead of the legal labels enumerated. The open
/// row defers the whole check to those runtime doors, which is where
/// "closed rule, named labels" actually lives for these two arguments — the
/// same door [`valid_name`] already is for the name contract. `search` has
/// only two states, not an enumeration, so `Ty::Bool` is the right type for
/// it outright — no runtime door of its own to defer to.
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

/// `agent-cancel :: Str → F Unit`
fn scheme_agent_cancel(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::Unit))))
}

/// The `[label: Str, trigger: Str, next-s: Int, fires: Int]` listing row
/// `schedules` answers with.
fn schedule_row_ty() -> Ty {
    closed_record(&[
        ("label", Ty::String),
        ("trigger", Ty::String),
        ("next-s", Ty::Int),
        ("fires", Ty::Int),
    ])
}

/// The `[label: Str, next-s: Int]` receipt the `schedule` builtin answers
/// with.
fn schedule_receipt_ty() -> Ty {
    closed_record(&[("label", Ty::String), ("next-s", Ty::Int)])
}

/// `schedule :: ∀ρ1 ρ2. [trigger: Variant ρ1, label: Variant ρ2, prompt: Str] → F [label: Str, next-s: Int]`
///
/// The record row is closed: a record literal with literal keys infers an
/// exact row (`infer_map_val` builds on `Row::Empty`), so unifying it
/// against this closed row makes a missing or misspelled field a static
/// error naming that field — the accurate diagnostic a closed *variant* row
/// could not give ([`scheme_agent`]'s doc), because a literal tag infers
/// an open row where a record literal infers a closed one.
///
/// The two variant rows *inside* the record stay open, for exactly the
/// closed-variant reason: an unknown `` `bogus `` trigger or label must
/// reach the runtime doors [`schedule_trigger`]/[`schedule_label`], which
/// error naming the legal labels, rather than dying as a generic
/// row-unification mismatch.
fn scheme_schedule(u: &mut Unifier) -> Scheme {
    let trigger_row = u.fresh_row_var();
    let label_row = u.fresh_row_var();
    scheme(
        &[],
        &[],
        &[trigger_row, label_row],
        thunk(fun(
            closed_record(&[
                ("trigger", Ty::Variant(Row::Var(trigger_row))),
                ("label", Ty::Variant(Row::Var(label_row))),
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

/// `unschedule :: Str → F Unit`
fn scheme_unschedule(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::Unit))))
}

/// `reply :: ∀α. α → F Unit` — fully polymorphic in its argument, exactly
/// the shape `service-handle`'s own `∀α` scheme mints
/// (`builtins.rs`'s `scheme_service_handle`); first-orderness is a
/// runtime door check ([`builtin_reply`]), not a static constraint on `α`.
fn scheme_reply(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    scheme(&[av], &[], &[], thunk(fun(Ty::Var(av), pure(Ty::Unit))))
}

pub static HARNESS_BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: Cow::Borrowed("agent"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_agent),
        doc: "agent [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>]  — launch a sub-agent. Launch-only and always asynchronous: returns immediately with a receipt [name: Str, log-dir: Str]; the child's reply is NOT this call's result — it arrives later, as its own marked item in your inbox. `type` selects the child's memory: `amnemon starts blank (no shared history, only a value-snapshot of your shell's bindings/cwd/env); `mnemon inherits your current model-visible conversation and reuses your provider selection for cache locality, receiving `prompt` as its fresh final prompt. Wrap `prompt` in a raw string #'…'# if it carries $, !, or quotes. `name` is the child's identity — non-empty, at most 24 characters, ASCII letters/digits/-/_ only — and must not be borne by any live agent, or the call is refused; pick something descriptive, like 'fix-parser-tests'. `grant` bounds the child to at most your own authority and must be exactly one of `confined (offline, no home reads), `minimal (working tree + /tmp + network), `read-only (writes only to scratch), `edit-only (edits the working tree, no build tooling), `reasonable (everyday tooling), `dangerous (no narrowing); any other label is refused, naming all six. `search` states whether the child may use the provider's own built-in web search, bounded above by your own — asking for it when you do not have it silently yields a child without it. Delegation depth is finite — each descendant is handed one less unit of fuel than its spawner holds, and once fuel reaches zero this call is refused; fuel bounds how deep a chain may recurse, never how many children you may start at any one depth. Answered only on the run that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_agent),
    },
    BuiltinEntry {
        name: Cow::Borrowed("agents"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_agents),
        doc: "agents  — list the live descendants you started that are still running: [[name: Str, elapsed-s: Int, log-dir: Str]]. Use it to recover names after a context compaction, then agent-cancel to stop a straggler. Settled agents are not listed — their replies arrive on their own as marked items in your inbox. Answered only on the run that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_agents),
    },
    BuiltinEntry {
        name: Cow::Borrowed("message"),
        type_rule: BuiltinTypeRule::Scheme(Some(2), scheme_message),
        doc: "message <name> <text>  — send `text` as a marked item to the live descendant named `name` (from agents or a spawn receipt); it lands at its next exchange boundary, not as human input. Only a descendant of yours may receive it — never a sibling, an ancestor, or yourself; refused otherwise. Does not return the recipient's answer — coordination only. Answered only on the run that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_message),
    },
    BuiltinEntry {
        name: Cow::Borrowed("agent-cancel"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_agent_cancel),
        doc: "agent-cancel <name>  — cancel the live descendant named `name` (from agents). It is asked to stop at its next checkpoint and then delivers a cancelled result to your inbox; a no-op if no live agent bears that name. Only a descendant of yours may be cancelled — never a sibling, an ancestor, or yourself; refused otherwise. Answered only on the run that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_agent_cancel),
    },
    BuiltinEntry {
        name: Cow::Borrowed("schedule"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_schedule),
        doc: "schedule <spec>  — arm a self-wakeup: at the chosen time a marked item carrying the spec's `prompt` is delivered to your inbox and re-engages you at your next exchange boundary, with no human present. `spec` is a record with exactly three fields: trigger, label, prompt. `trigger` is exactly one of `cron '<expr>'` — a five-field cron expression (minute hour day-of-month month day-of-week) in the host's local timezone, e.g. `cron '0 9 * * 1-5'` for weekdays at 09:00; recurring — or `after '<n><unit>'` — a one-shot relative delay, unit one of s/m/h/d, e.g. `after '30m'`, `after '2h'`; any other shape is refused, naming both. `label` is `some '<name>'` to name the wakeup — the label is its identity: it must not be borne by another live schedule, and the sched-<n> form is reserved for defaults — or `none` to take the default sched-<n>; any other shape is refused, naming both. `prompt` is the natural-language instruction you act on when woken, not code — e.g. schedule [trigger: `after '30m', label: `none, prompt: 'check the build']. Returns a receipt [label: Str, next-s: Int] — next-s is the seconds until the first fire; read it back to catch a cron expression that parsed but does not mean what you meant. Requires the self-wakeup grant (--allow-schedule) — an agent that can wake itself indefinitely holds real authority, so without the grant this call is refused. Answered only on the run that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_schedule),
    },
    BuiltinEntry {
        name: Cow::Borrowed("schedules"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_schedules),
        doc: "schedules  — list your live scheduled wakeups: [[label: Str, trigger: Str, next-s: Int, fires: Int]] — next-s the seconds until the next fire, fires how many times it has fired so far. Use it to recover labels after a context compaction, then unschedule to remove one. Requires the self-wakeup grant (--allow-schedule) and is refused without it. Answered only on the run that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_schedules),
    },
    BuiltinEntry {
        name: Cow::Borrowed("unschedule"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_unschedule),
        doc: "unschedule <label>  — remove a scheduled wakeup by its label (from schedules or a schedule receipt). A no-op if no live schedule bears that label. Requires the self-wakeup grant (--allow-schedule) and is refused without it. Answered only on the run that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_unschedule),
    },
    BuiltinEntry {
        name: Cow::Borrowed("reply"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_reply),
        doc: "reply <value>  — hand `value` back to whoever spawned you: the sole return path for a returning agent. Your parent receives exactly this value, nothing else — not your reasoning, your shell bindings, or any prose you streamed along the way. `value` must be first-order data: no closures, handles, or environments; passing one fails this call with a didactic error and your run continues, so fix the value and call reply again. Call it more than once in an exchange and the last call wins — an earlier value is discarded, not appended. The run does not end at this call: it ends once the enclosing ral call's whole batch of statements finishes draining, so write reply last and let earlier statements in the same script run to completion first. A non-finite Float (NaN, +Infinity, -Infinity) reaches your parent as the string \"NaN\"/\"Infinity\"/\"-Infinity\" — JSON, which the value eventually crosses into, has no such numbers. Refused on the interactive trunk and every /branch child: they converse with the user turn after turn and never return, so they hold no obligation to call this. Answered only on the run that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_reply),
    },
];

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

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
    fn permission_label_rejects_an_unknown_tag_naming_all_six() {
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

    /// A tab-carrying payload on a grant variant is not a bare tag —
    /// refused just like an unknown label, never silently truncated.
    #[test]
    fn permission_label_rejects_a_variant_carrying_a_payload() {
        let v = Value::Variant {
            label: "confined".to_string(),
            payload: Some(Box::new(Value::Int(1))),
        };
        assert!(permission_label(&v).is_err());
    }

    /// A unique scratch directory per test, mirroring `tests/agent_deliberate.rs`'s
    /// own `tmp` helper.
    fn tmp(tag: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("exarch-harness-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// An unknown `grant` label errors engine-side, naming all six legal
    /// bases, before any enquiry crosses — the door validates `name`,
    /// `type`, and `grant` before `fork_into_nursery`/`enquire` ever run, so
    /// a malformed call never registers a child.
    #[test]
    fn unknown_grant_label_errors_before_any_enquiry_crosses() {
        let dir = tmp("unknown-grant");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
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

    /// An unknown `type` tag errors engine-side, naming both legal memory
    /// modes, before any enquiry crosses.
    #[test]
    fn unknown_type_tag_errors_before_any_enquiry_crosses() {
        let dir = tmp("unknown-type");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
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

    /// An invalid name errors engine-side, naming the tab-bar contract,
    /// before any enquiry crosses.
    #[test]
    fn invalid_name_errors_before_any_enquiry_crosses() {
        let dir = tmp("invalid-name");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
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

    /// A spec record missing a required field is a *static* error naming
    /// that field: the record literal infers a closed row, and unifying it
    /// against `scheme_agent`'s closed record row reports exactly which
    /// label is absent — mirroring `missing_spec_field_errors_statically_naming_the_field`
    /// in the schedule family below.
    #[test]
    fn missing_agent_field_errors_statically_naming_the_field() {
        let dir = tmp("missing-agent-field");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
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

    /// The full stack, end to end: a scripted provider issues a `ral` tool
    /// call whose script is
    /// `` agent [prompt: #'say hi'#, name: 'helper', type: `amnemon, grant: `read-only, search: false] ``
    /// — real source, parsed and type-checked, crossing the desk through a
    /// real nursery fork — and the receipt record is the run's value,
    /// while the child's own reply later settles into the parent's inbox.
    ///
    /// Drives `run_shell` directly rather than `Agent::deliberate`'s provider
    /// loop: the spawned child inherits the parent's *own* `Arc<Provider>`
    /// (`agent-start`'s `ProviderHandle::new(services.provider.current())`),
    /// so a script consumed by both a driven parent exchange and its spawned
    /// child races unpredictably over which one gets which stage.
    #[test]
    fn agent_full_stack_round_trip_delivers_receipt_and_settles_into_inbox() {
        let dir = tmp("full-stack-round-trip");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
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
    // In a record literal the fields are comma-delimited, so a nullary
    // `` `none `` label can never absorb its neighbour as a payload
    // (`Token::Comma` is a payload terminator, `at_tag_payload_end` in
    // `core/src/syntax/parser.rs`) — the very reason `schedule` takes one
    // spec record rather than three positional arguments.

    /// A malformed cron expression errors engine-side, carrying the
    /// parser's own message, before any enquiry crosses.
    #[test]
    fn bad_cron_expr_errors_before_any_enquiry_crosses() {
        let dir = tmp("bad-cron-expr");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `cron '* * * *', label: `none, prompt: #'wake'#]",
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

    /// A malformed `after` duration errors engine-side, carrying the
    /// parser's own message, before any enquiry crosses.
    #[test]
    fn bad_duration_errors_before_any_enquiry_crosses() {
        let dir = tmp("bad-duration");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after 'nope', label: `none, prompt: #'wake'#]",
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

    /// A label that is neither `` `some `` nor `` `none `` errors
    /// engine-side, naming both legal shapes, before any enquiry crosses.
    #[test]
    fn label_neither_some_nor_none_errors_before_any_enquiry_crosses() {
        let dir = tmp("bad-label");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '1s', label: `bogus, prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(result.content.contains("some"), "got: {}", result.content);
        assert!(result.content.contains("none"), "got: {}", result.content);
        assert!(
            session.schedules.list().is_empty(),
            "an unknown label must never register a schedule"
        );
    }

    /// A trigger that is neither `` `cron `` nor `` `after `` errors
    /// engine-side, naming both legal shapes, before any enquiry crosses.
    #[test]
    fn trigger_neither_cron_nor_after_errors_before_any_enquiry_crosses() {
        let dir = tmp("bad-trigger");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `bogus 'x', label: `none, prompt: #'wake'#]",
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

    /// A spec record missing a required field is a *static* error naming
    /// that field: the record literal infers a closed row, and unifying it
    /// against `scheme_schedule`'s closed record row reports exactly which
    /// label is absent.
    #[test]
    fn missing_spec_field_errors_statically_naming_the_field() {
        let dir = tmp("missing-spec-field");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '1s', label: `none]",
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

    /// A spec record carrying an unknown extra field is likewise a static
    /// error naming the surplus label — the closed record row admits
    /// exactly trigger/label/prompt.
    #[test]
    fn unknown_extra_spec_field_errors_statically_naming_the_field() {
        let dir = tmp("extra-spec-field");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '1s', label: `none, prompt: #'wake'#, extra: 1]",
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

    /// The full stack, end to end: `` schedule [trigger: `after '1s',
    /// label: `none, prompt: #'wake'#] `` — real source, parsed and
    /// type-checked, crossing the desk — answers a receipt record naming
    /// the resolved `sched-{n}` default label and the seconds to first
    /// fire, and once it fires the marked wakeup lands in the inbox as a
    /// [`crate::bus::Item::Wakeup`]. Mirrors `crate::fleet::schedule`'s own
    /// `after_fires_once_then_is_removed` for how to wait for the fire — a
    /// real second, since `parse_duration`'s smallest unit is whole
    /// seconds.
    #[test]
    fn schedule_full_stack_round_trip_answers_receipt_and_fires_into_inbox() {
        let dir = tmp("schedule-full-stack");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        session.allow_schedule = true;
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '1s', label: `none, prompt: #'wake'#]",
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
        assert!(
            live[0].label.starts_with("sched-"),
            "must take the minted default label, got: {}",
            live[0].label
        );
        assert!(
            result.content.contains(&live[0].label),
            "the receipt must carry the resolved label, got: {}",
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

    /// `` unschedule <label> `` full stack: schedule one with an explicit
    /// label, then remove it by that same label — real source, crossing
    /// the desk and the registry — and confirm nothing live remains.
    #[test]
    fn unschedule_full_stack_removes_by_label() {
        let dir = tmp("unschedule-full-stack");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        session.allow_schedule = true;
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "schedule [trigger: `after '10m', label: `some 'nightly', prompt: #'wake'#]",
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
            session.schedules.list().is_empty(),
            "unschedule by label must remove the schedule"
        );
    }

    // ── `reply` ───────────────────────────────────────────────────────────

    /// A `ral` tool call carrying a real script as its `cmd` — the shape a
    /// scripted child's own exchange issues, mirroring `agent.rs`'s private
    /// helper of the same name (this test module has no access to it).
    fn ral_call(id: &str, cmd: &str) -> genai::chat::ToolCall {
        genai::chat::ToolCall {
            call_id: id.into(),
            fn_name: "ral".into(),
            fn_arguments: serde_json::json!({
                "cmd": cmd,
                "description": "test command",
            }),
            thought_signatures: None,
        }
    }

    /// The `reply` builtin's full stack, end to end: a spawned child's own
    /// scripted exchange runs `` reply [files: $found] `` — real source, parsed
    /// and type-checked, crossing the desk through a real enquiry — and the
    /// structured record it built reaches the parent's inbox, not a
    /// flattened string. Substitutes the child's own script for the
    /// `reply` *tool* call `agent_full_stack_round_trip_delivers_receipt_and_settles_into_inbox`
    /// uses, for the same non-driven-parent reason that test documents.
    #[test]
    fn reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox() {
        let dir = tmp("reply-full-stack-round-trip");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
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

    /// A non-first-order `reply` argument — a bare Block, which no ral
    /// value the model can serialise across the host seam ever is — errors
    /// engine-side, naming the first-order rule, before any enquiry
    /// crosses. The refusal is an ordinary call error, not a termination:
    /// the session stays usable and a later, well-formed `reply` succeeds
    /// normally.
    #[test]
    fn reply_refuses_a_non_first_order_value_and_does_not_terminate() {
        let dir = tmp("reply-non-first-order");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
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

    /// A double `reply` within one exchange is last-wins: the second call's
    /// value is what the exchange settles `Replied` with.
    #[test]
    fn double_reply_in_one_exchange_is_last_wins() {
        let dir = tmp("double-reply-last-wins");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
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
}
