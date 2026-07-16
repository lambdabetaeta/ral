//! The agent-, schedule-, and commitment-family harness builtins:
//! `amnemon`, `mnemon`, `agents`, `message`, `agent-cancel`, `schedule`,
//! `schedules`, `unschedule`, `commit`, `verify-commitment` — the model's
//! action surface onto the `` `agent-start ``/`` `agent-list ``/
//! `` `agent-cancel ``/`` `message ``/`` `schedule ``/`` `schedule-list ``/
//! `` `unschedule ``/`` `commit-open ``/`` `commit-verify `` enquiry classes
//! [`crate::desk::ExarchDesk`] answers.
//!
//! Each body validates its own arguments at the door — arity, the title
//! contract, the six-label permissions vocabulary, the trigger/label
//! vocabularies, the `commitment:*` key grammar — before it ever forks a
//! session or puts an enquiry, so a malformed call never reaches the host.
//! A spawning body (`amnemon`/`mnemon`/`commit`/`verify-commitment`) forks
//! this shell into the turn's nursery
//! ([`ral_core::Shell::fork_into_nursery`]) and enquires with the adopted
//! session's id; every other body enquires directly. The desk answers class
//! by class in `crate::desk`; this module only ever crosses that one seam.

use crate::schedule::{CronSchedule, parse_duration};
use ral_core::builtins::util::check_arity;
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{BuiltinTypeRule, closed_record, fun, mk_scheme as scheme, pure, thunk};
use ral_core::typecheck::{Row, Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Settled, sig};
use ral_core::{Shell, Value};
use std::borrow::Cow;

/// The six bake-in permission bases a spawn's `permissions` argument may
/// name — see `crate::policy::base::resolve_base`, whose profiles these
/// mirror exactly.
const PERMISSION_LABELS: [&str; 6] = [
    "confined",
    "minimal",
    "read-only",
    "edit-only",
    "reasonable",
    "dangerous",
];

/// True for titles that fit the tab-bar contract — non-empty, ≤24 chars,
/// ASCII alphanumeric plus `-`/`_`. Spaces and punctuation are excluded
/// because the tab bar lays them out token-by-token. Shared by `amnemon`
/// and `mnemon`, whose `title` argument is mandatory here (the JSON tools'
/// silent `sub-{N}` fallback has no place at a door that can simply refuse).
pub(crate) fn valid_title(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Decode a permissions argument into its bare tag, or a door error naming
/// all six legal labels. The argument's own type is row-open (see
/// `scheme_agent_spawn`'s doc for why), so this runtime check is what
/// actually closes the rule — the same standard the retiring JSON schema's
/// `enum` held.
fn permission_label(v: &Value) -> Settled<String> {
    if let Value::Variant { label, payload: None } = v
        && PERMISSION_LABELS.contains(&label.as_str())
    {
        return Ok(label.clone());
    }
    Err(sig(format!(
        "permissions must be one of `confined, `minimal, `read-only, `edit-only, `reasonable, `dangerous — got {v}"
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
    let Value::Variant { label, payload: Some(payload) } = v else {
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
                payload: Some(Box::new(FOValue::String { value: expr.clone() })),
            })
        }
        "after" => {
            parse_duration(expr).map_err(|e| sig(format!("schedule: {e}")))?;
            Ok(FOValue::Variant {
                label: "after".to_string(),
                payload: Some(Box::new(FOValue::String { value: expr.clone() })),
            })
        }
        other => Err(sig(format!(
            "schedule: trigger must be `cron '<5-field-cron-expr>'` or `after '<n><unit>'`, got `{other}`"
        ))),
    }
}

/// Decode a `schedule` spec's `label` field — `` `some '<name>' `` or
/// `` `none `` — into the [`FOValue`] the desk expects, or a door error
/// naming both legal shapes. The field's variant row is open, the same
/// standard [`schedule_trigger`] and `permission_label` hold theirs to.
fn schedule_label(v: &Value) -> Settled<FOValue> {
    match v {
        Value::Variant { label, payload: None } if label == "none" => Ok(FOValue::Variant {
            label: "none".to_string(),
            payload: None,
        }),
        Value::Variant { label, payload: Some(payload) } if label == "some" => {
            let Value::String(name) = payload.as_ref() else {
                return Err(sig(format!(
                    "schedule: `some`'s payload must be a Str, got {}",
                    payload.type_name()
                )));
            };
            Ok(FOValue::Variant {
                label: "some".to_string(),
                payload: Some(Box::new(FOValue::String { value: name.clone() })),
            })
        }
        other => Err(sig(format!(
            "schedule: label must be `some '<name>'` or `none`, got {other}"
        ))),
    }
}

/// Validate a `commit`/`verify-commitment` `key` argument against the
/// protected pin grammar — `` `commitment:` `` followed by one or more
/// ASCII letters, digits, `.`, `_`, or `-` — re-running
/// [`crate::tools::commitment::valid_commitment_key`] here so a malformed
/// key never reaches the host. `tool` names the caller for the door error.
fn commitment_key(v: &Value, tool: &str) -> Settled<String> {
    let key = v.to_string();
    if !crate::tools::commitment::valid_commitment_key(&key) {
        return Err(sig(format!(
            "{tool}: `key` must look like `{}<id>` using ASCII letters, digits, `.`, `_`, or \
             `-`, got {key:?}",
            crate::shell_eval::COMMITMENT_PIN_PREFIX
        )));
    }
    Ok(key)
}

/// The `amnemon`/`mnemon` shared body: validate the door, fork this shell
/// into the turn's nursery, and enquire `` `agent-start `` with the
/// adopted session's id — the builtin-body half of a spawn, mirroring
/// `tools::agent::dispatch_spawn`'s parse-then-dispatch split. `kind` is
/// the bare enquiry tag (`amnemon`/`mnemon`) the desk selects the log-fork
/// behaviour from.
fn spawn_body(args: &[Value], shell: &Shell, tool: &str, kind: &'static str) -> Settled<Value> {
    check_arity(args, 3, tool)?;
    let prompt = args[0].to_string();
    let title = args[1].to_string();
    if !valid_title(&title) {
        return Err(sig(format!(
            "{tool}: title must be non-empty, at most 24 characters, and only ASCII letters, \
             digits, `-`, or `_` (the tab-bar contract) — got {title:?}"
        )));
    }
    let permissions = permission_label(&args[2])?;

    let session = shell.fork_into_nursery()?;
    // A NurseryId is a small monotonic per-turn counter; `unwrap_or` never
    // actually saturates in practice, but keeps this door total without an
    // `as` cast's silent wraparound.
    let session_id = i64::try_from(session.0).unwrap_or(i64::MAX);
    let answer = shell.enquire(FOValue::Variant {
        label: "agent-start".to_string(),
        payload: Some(Box::new(FOValue::List {
            items: vec![
                FOValue::Int { value: session_id },
                FOValue::Variant { label: kind.to_string(), payload: None },
                FOValue::String { value: prompt },
                FOValue::String { value: title },
                FOValue::Variant { label: permissions, payload: None },
            ],
        })),
    })?;

    let FOValue::Variant { label, payload: Some(payload) } = answer else {
        return Err(sig(format!("{tool}: host answered an unexpected shape for its receipt")));
    };
    if label != "started" {
        return Err(sig(format!("{tool}: host refused: {label}")));
    }
    Ok(Value::from(*payload))
}

/// `amnemon <prompt> <title> <permissions>` — launch an independent,
/// blank-context sub-agent. This is launch-only and always asynchronous:
/// the call returns immediately with a receipt `[id: Int, title: Str,
/// log-dir: Str]`; the child's own reply is NOT this call's result — it
/// arrives later, as its own marked turn in your inbox, once the child
/// finishes. `prompt` is the child's whole instruction: it starts a fresh
/// conversation with no shared history, only a value-snapshot of your
/// shell's bindings, cwd, and env; if it carries `$`, `!`, or quotes, write
/// it as a raw string `#'…'#` so it reaches the child literally. `title`
/// names its tab and must fit the tab-bar contract — non-empty, at most 24
/// characters, ASCII letters/digits/`-`/`_` only — or the call is refused.
/// `permissions` bounds the child's authority to at most your own and must
/// be exactly one of the six variants `` `confined `` (offline, no home
/// reads), `` `minimal `` (working tree + /tmp + network), `` `read-only ``
/// (writes only to scratch), `` `edit-only `` (edits the working tree, no
/// build tooling), `` `reasonable `` (everyday tooling), `` `dangerous ``
/// (no narrowing); any other label is refused, naming all six. Delegation
/// depth is finite: each descendant is handed one less unit of fuel than
/// its spawner holds, and once fuel reaches zero this call is refused —
/// fuel bounds how deep a chain may recurse, never how many children you
/// may start at any one depth, so starting several at once costs nothing
/// extra. Answered only on the turn that calls it: inside `spawn { … }`
/// this errors, since a detached worker outlives the turn whose desk could
/// answer it.
fn builtin_amnemon(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    spawn_body(args, shell, "amnemon", "amnemon")
}

/// `mnemon <prompt> <title> <permissions>` — launch a sub-agent that
/// inherits your current model-visible conversation (every message you and
/// the model have exchanged so far) and reuses your current provider
/// selection, appending `prompt` as its fresh final prompt — useful when
/// the child needs the context you already built and the provider can
/// reuse its prompt cache. It also gets a value-snapshot of your shell's
/// bindings, cwd, and env, exactly as `amnemon` would. This is launch-only
/// and always asynchronous: the call returns immediately with a receipt
/// `[id: Int, title: Str, log-dir: Str]`; the child's own reply is NOT this
/// call's result — it arrives later, as its own marked turn in your inbox,
/// once the child finishes. `prompt` should ride a raw string `#'…'#` if it
/// carries `$`, `!`, or quotes, so it reaches the child literally. `title`
/// names its tab and must fit the tab-bar contract — non-empty, at most 24
/// characters, ASCII letters/digits/`-`/`_` only — or the call is refused.
/// `permissions` bounds the child's authority to at most your own and must
/// be exactly one of the six variants `` `confined `` (offline, no home
/// reads), `` `minimal `` (working tree + /tmp + network), `` `read-only ``
/// (writes only to scratch), `` `edit-only `` (edits the working tree, no
/// build tooling), `` `reasonable `` (everyday tooling), `` `dangerous ``
/// (no narrowing); any other label is refused, naming all six. Delegation
/// depth is finite: each descendant is handed one less unit of fuel than
/// its spawner holds, and once fuel reaches zero this call is refused —
/// fuel bounds how deep a chain may recurse, never how many children you
/// may start at any one depth, so starting several at once costs nothing
/// extra. Answered only on the turn that calls it: inside `spawn { … }`
/// this errors, since a detached worker outlives the turn whose desk could
/// answer it.
fn builtin_mnemon(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    spawn_body(args, shell, "mnemon", "mnemon")
}

/// `agents` — list the live descendants you started that are still
/// running, each as `[id: Int, title: Str, elapsed-s: Int, log-dir: Str]`.
/// Use it to recover ids after a context compaction, then `agent-cancel` to
/// stop a straggler. Settled agents are not listed: their replies arrive on
/// their own as marked turns in your inbox. Answered only on the turn that
/// calls it: inside `spawn { … }` this errors.
#[allow(
    clippy::unnecessary_wraps,
    reason = "registered as a `BuiltinBody::Static` fn pointer; the `Settled<Value>` return is the shape the builtin table dispatches through, not a choice of this body"
)]
fn builtin_agents(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let answer = shell.enquire(FOValue::Variant {
        label: "agent-list".to_string(),
        payload: None,
    })?;
    let FOValue::List { items } = answer else {
        return Err(sig("agents: host answered an unexpected shape for the listing"));
    };
    Ok(Value::list(items.into_iter().map(Value::from).collect()))
}

/// `message <id> <text>` — send `text` as a marked turn to the live
/// descendant named by `id` (from `agents`, a spawn receipt, or an id you
/// deliberately included in a child's prompt); it lands at its next turn
/// boundary, not as human input. Only a descendant of yours may receive
/// it — never a sibling, an ancestor, or yourself — and the call is refused
/// otherwise. This does not return the recipient's answer: it is for
/// coordination only. Answered only on the turn that calls it: inside
/// `spawn { … }` this errors.
fn builtin_message(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "message")?;
    let id = match args[0].as_int() {
        Some(n) if n >= 0 => n,
        _ => {
            return Err(sig(format!(
                "message: `id` must be a non-negative Int, got {}",
                args[0].type_name()
            )));
        }
    };
    let text = args[1].to_string();
    shell.enquire(FOValue::Variant {
        label: "message".to_string(),
        payload: Some(Box::new(FOValue::List {
            items: vec![FOValue::Int { value: id }, FOValue::String { value: text }],
        })),
    })?;
    Ok(Value::Unit)
}

/// `agent-cancel <id>` — cancel the live descendant named by `id` (from
/// `agents`). It is asked to stop at its next checkpoint and then delivers
/// a cancelled result to your inbox; a no-op if no live agent has that id.
/// Only a descendant of yours may be cancelled — never a sibling, an
/// ancestor, or yourself — and the call is refused otherwise. Answered only
/// on the turn that calls it: inside `spawn { … }` this errors.
fn builtin_agent_cancel(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "agent-cancel")?;
    let id = match args[0].as_int() {
        Some(n) if n >= 0 => n,
        _ => {
            return Err(sig(format!(
                "agent-cancel: `id` must be a non-negative Int, got {}",
                args[0].type_name()
            )));
        }
    };
    shell.enquire(FOValue::Variant {
        label: "agent-cancel".to_string(),
        payload: Some(Box::new(FOValue::List {
            items: vec![FOValue::Int { value: id }],
        })),
    })?;
    Ok(Value::Unit)
}

/// `schedule <spec>` — arm a self-wakeup: at the chosen time a marked turn
/// carrying the spec's `prompt` is delivered to your inbox and re-engages
/// you at your next turn boundary, with no human present. `spec` is a record
/// with exactly three fields: `trigger`, `label`, and `prompt`. `trigger` is
/// exactly one of `` `cron '<expr>' `` — a five-field cron expression
/// (minute hour day-of-month month day-of-week) in the host's local
/// timezone, e.g. `` `cron '0 9 * * 1-5' `` for weekdays at 09:00; recurring
/// — or `` `after '<n><unit>' `` — a one-shot relative delay, unit one of
/// s/m/h/d, e.g. `` `after '30m' ``, `` `after '2h' ``; any other shape is
/// refused, naming both. `label` is `` `some '<name>' `` to give the
/// schedule a human-readable name shown by `schedules`, or `` `none `` to
/// take the default `sched-{id}`; any other shape is refused, naming both.
/// `prompt` is the natural-language instruction you act on when woken, not
/// code — e.g. `` schedule [trigger: `after '30m', label: `none, prompt:
/// 'check the build'] ``. Returns the new schedule's id. Requires the
/// self-wakeup grant (`--allow-schedule`) — an agent that can wake itself
/// indefinitely holds real authority, so without the grant this call is
/// refused. Answered only on the turn that calls it: inside `spawn { … }`
/// this errors.
fn builtin_schedule(args: &[Value], shell: &mut Shell) -> Settled<Value> {
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

    let answer = shell.enquire(FOValue::Variant {
        label: "schedule".to_string(),
        payload: Some(Box::new(FOValue::List {
            items: vec![trigger, label, FOValue::String { value: prompt }],
        })),
    })?;
    let FOValue::Int { value } = answer else {
        return Err(sig("schedule: host answered an unexpected shape for its receipt"));
    };
    Ok(Value::Int(value))
}

/// `schedules` — list your live scheduled wakeups, each as `[id: Int, label:
/// Str, trigger: Str, next-s: Int, fires: Int]` — `next-s` the seconds until
/// the next fire, `fires` how many times it has fired so far. Use it to
/// recover schedule ids after a context compaction, then `unschedule` to
/// remove one. Requires the self-wakeup grant (`--allow-schedule`) and is
/// refused without it. Answered only on the turn that calls it: inside
/// `spawn { … }` this errors.
#[allow(
    clippy::unnecessary_wraps,
    reason = "registered as a `BuiltinBody::Static` fn pointer; the `Settled<Value>` return is the shape the builtin table dispatches through, not a choice of this body"
)]
fn builtin_schedules(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let answer = shell.enquire(FOValue::Variant {
        label: "schedule-list".to_string(),
        payload: None,
    })?;
    let FOValue::List { items } = answer else {
        return Err(sig("schedules: host answered an unexpected shape for the listing"));
    };
    Ok(Value::list(items.into_iter().map(Value::from).collect()))
}

/// `unschedule <id>` — remove a scheduled wakeup by its id (from
/// `schedules`). A no-op if no schedule has that id. Requires the
/// self-wakeup grant (`--allow-schedule`) and is refused without it.
/// Answered only on the turn that calls it: inside `spawn { … }` this
/// errors.
fn builtin_unschedule(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "unschedule")?;
    let id = match args[0].as_int() {
        Some(n) if n >= 0 => n,
        _ => {
            return Err(sig(format!(
                "unschedule: `id` must be a non-negative Int, got {}",
                args[0].type_name()
            )));
        }
    };
    shell.enquire(FOValue::Variant {
        label: "unschedule".to_string(),
        payload: Some(Box::new(FOValue::List {
            items: vec![FOValue::Int { value: id }],
        })),
    })?;
    Ok(Value::Unit)
}

/// `commit <key> <description>` — open a new protected commitment. `key` is
/// yours to choose: `` `commitment:` `` followed by one or more ASCII
/// letters, digits, `.`, `_`, or `-` (e.g. `commitment:plan-x`); any other
/// shape is refused, naming the grammar. `description` is what you are
/// committing to, in your own words, and must not be empty — it is not
/// saved verbatim. This is launch-only and always asynchronous: the call
/// forks a host-owned, read-only writer child that formalizes your
/// description into concrete, falsifiable criteria, and returns
/// immediately with a receipt `[id: Int, title: Str, log-dir: Str]`; the
/// writer's own reply is NOT this call's result — it arrives later, as its
/// own marked turn in your inbox, and only a well-formed criteria card
/// opens the pin. You choose the key and the description; the host alone
/// builds the writer's prompt and bounds it to read-only, so you can never
/// steer its criteria or its authority. The call is refused if `key` is
/// already a live commitment — verify or clear it first. Once open, only a
/// passing `verify-commitment` can close it; you cannot unpin or overwrite
/// it yourself. Delegation depth is finite: the writer is handed one less
/// unit of fuel than you hold, and once your fuel reaches zero this call is
/// refused. Answered only on the turn that calls it: inside `spawn { … }`
/// this errors, since a detached worker outlives the turn whose desk could
/// answer it.
fn builtin_commit(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "commit")?;
    let key = commitment_key(&args[0], "commit")?;
    let description = args[1].to_string();
    if description.trim().is_empty() {
        return Err(sig("commit: `description` must not be empty"));
    }

    let session = shell.fork_into_nursery()?;
    // A NurseryId is a small monotonic per-turn counter; `unwrap_or` never
    // actually saturates in practice, but keeps this door total without an
    // `as` cast's silent wraparound.
    let session_id = i64::try_from(session.0).unwrap_or(i64::MAX);
    let answer = shell.enquire(FOValue::Variant {
        label: "commit-open".to_string(),
        payload: Some(Box::new(FOValue::List {
            items: vec![
                FOValue::Int { value: session_id },
                FOValue::String { value: key },
                FOValue::String { value: description },
            ],
        })),
    })?;

    let FOValue::Variant { label, payload: Some(payload) } = answer else {
        return Err(sig("commit: host answered an unexpected shape for its receipt"));
    };
    if label != "started" {
        return Err(sig(format!("commit: host refused: {label}")));
    }
    Ok(Value::from(*payload))
}

/// `verify-commitment <key>` — ask a host-owned, read-only verifier child
/// to check one live protected commitment pin. `key` must be the full
/// `` `commitment:` ``-prefixed key of a currently live commitment (from a
/// `commit` receipt or your own record); any other shape is refused, naming
/// the grammar. You supply only the key — no instructions, evidence, or
/// verifier prompt of your own reach it. This is launch-only and always
/// asynchronous: the call forks the verifier and returns immediately with a
/// receipt `[id: Int, title: Str, log-dir: Str]`; the verifier's own reply
/// is NOT this call's result — it arrives later, as its own marked turn in
/// your inbox. The host alone reads the saved commitment card, builds the
/// verifier's prompt, and bounds it to read-only; the pin clears only if
/// the verifier returns a structured pass verdict, and stays live on a fail
/// or on any other reply shape. Delegation depth is finite: the verifier is
/// handed one less unit of fuel than you hold, and once your fuel reaches
/// zero this call is refused. Answered only on the turn that calls it:
/// inside `spawn { … }` this errors, since a detached worker outlives the
/// turn whose desk could answer it.
fn builtin_verify_commitment(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "verify-commitment")?;
    let key = commitment_key(&args[0], "verify-commitment")?;

    let session = shell.fork_into_nursery()?;
    // A NurseryId is a small monotonic per-turn counter; `unwrap_or` never
    // actually saturates in practice, but keeps this door total without an
    // `as` cast's silent wraparound.
    let session_id = i64::try_from(session.0).unwrap_or(i64::MAX);
    let answer = shell.enquire(FOValue::Variant {
        label: "commit-verify".to_string(),
        payload: Some(Box::new(FOValue::List {
            items: vec![FOValue::Int { value: session_id }, FOValue::String { value: key }],
        })),
    })?;

    let FOValue::Variant { label, payload: Some(payload) } = answer else {
        return Err(sig("verify-commitment: host answered an unexpected shape for its receipt"));
    };
    if label != "started" {
        return Err(sig(format!("verify-commitment: host refused: {label}")));
    }
    Ok(Value::from(*payload))
}

/// `reply <value>` — hand `value` back to whoever spawned you: the sole
/// return path for a returning agent. `value` must be first-order data (no
/// closures, handles, or environments); a door check runs
/// [`FOValue::try_from`] before any enquiry crosses, so a violation fails
/// only this call, engine-side, with a didactic error — the run does not
/// end, and you may call `reply` again with a value that qualifies.
fn builtin_reply(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "reply")?;
    let payload = FOValue::try_from(&args[0]).map_err(|_| {
        sig(
            "reply: the value must be first-order data — no closures, handles, or \
             environments — since it crosses to whoever spawned you as plain data",
        )
    })?;
    shell.enquire(FOValue::Variant {
        label: "reply".to_string(),
        payload: Some(Box::new(FOValue::List { items: vec![payload] })),
    })?;
    Ok(Value::Unit)
}

/// The `[id: Int, title: Str, log-dir: Str]` receipt `amnemon`/`mnemon`/
/// `commit`/`verify-commitment` all answer with.
fn spawn_receipt_ty() -> Ty {
    closed_record(&[("id", Ty::Int), ("title", Ty::String), ("log-dir", Ty::String)])
}

/// `amnemon`/`mnemon :: Str → Str → ∀ρ. Variant ρ → F [id: Int, title: Str, log-dir: Str]`
/// — the two builtins share this exact shape, so one scheme function backs
/// both registry entries (the same convention `core/src/typecheck/builtins.rs`
/// documents for its own shared shapes).
///
/// The permissions argument's row is open — `` ∀ρ. Variant ρ ``, exactly
/// [`ral_core::typecheck::builtins::scheme::surface_op`]'s own shape — not
/// the closed six-label row a first look at "closed rules live in argument
/// types" suggests. A literal tag infers its *own* open row (`` `bogus``
/// infers `` [`bogus: Unit | ρ] ``, `typecheck::infer`'s doc on `Val::Variant`),
/// so unifying it against a *closed* row here would make an unknown label a
/// static type error — sound, but the wrong failure mode: it never reaches
/// [`permission_label`], so the model would see a bare row-mismatch
/// diagnostic instead of the six legal labels enumerated. The open row
/// defers the whole check to that runtime door, which is where "closed
/// rule, named labels" actually lives for this argument — the same door
/// [`valid_title`] already is for the title contract.
fn scheme_agent_spawn(u: &mut Unifier) -> Scheme {
    let permissions_row = u.fresh_row_var();
    scheme(
        &[],
        &[],
        &[permissions_row],
        thunk(fun(
            Ty::String,
            fun(
                Ty::String,
                fun(Ty::Variant(Row::Var(permissions_row)), pure(spawn_receipt_ty())),
            ),
        )),
    )
}

/// `agents :: F [[id: Int, title: Str, elapsed-s: Int, log-dir: Str]]`
fn scheme_agents(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(pure(Ty::List(Box::new(closed_record(&[
            ("id", Ty::Int),
            ("title", Ty::String),
            ("elapsed-s", Ty::Int),
            ("log-dir", Ty::String),
        ]))))),
    )
}

/// `message :: Int → Str → F Unit`
fn scheme_message(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::Int, fun(Ty::String, pure(Ty::Unit)))))
}

/// `agent-cancel :: Int → F Unit`
fn scheme_agent_cancel(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::Int, pure(Ty::Unit))))
}

/// The `[id: Int, label: Str, trigger: Str, next-s: Int, fires: Int]`
/// listing row `schedules` answers with.
fn schedule_row_ty() -> Ty {
    closed_record(&[
        ("id", Ty::Int),
        ("label", Ty::String),
        ("trigger", Ty::String),
        ("next-s", Ty::Int),
        ("fires", Ty::Int),
    ])
}

/// `schedule :: ∀ρ1 ρ2. [trigger: Variant ρ1, label: Variant ρ2, prompt: Str] → F Int`
///
/// The record row is closed: a record literal with literal keys infers an
/// exact row (`infer_map_val` builds on `Row::Empty`), so unifying it
/// against this closed row makes a missing or misspelled field a static
/// error naming that field — the accurate diagnostic a closed *variant* row
/// could not give (`scheme_agent_spawn`'s doc), because a literal tag infers
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
            pure(Ty::Int),
        )),
    )
}

/// `schedules :: F [[id: Int, label: Str, trigger: Str, next-s: Int, fires: Int]]`
fn scheme_schedules(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(pure(Ty::List(Box::new(schedule_row_ty())))))
}

/// `unschedule :: Int → F Unit`
fn scheme_unschedule(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::Int, pure(Ty::Unit))))
}

/// `commit :: Str → Str → F [id: Int, title: Str, log-dir: Str]`
fn scheme_commit(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::String, fun(Ty::String, pure(spawn_receipt_ty())))),
    )
}

/// `verify-commitment :: Str → F [id: Int, title: Str, log-dir: Str]`
fn scheme_verify_commitment(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::String, pure(spawn_receipt_ty()))))
}

/// `reply :: ∀α. α → F Unit` — fully polymorphic in its argument, exactly
/// the shape `service-handle`'s own `∀α` scheme mints
/// (`agent_builtins.rs`'s `scheme_service_handle`); first-orderness is a
/// runtime door check ([`builtin_reply`]), not a static constraint on `α`.
fn scheme_reply(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    scheme(&[av], &[], &[], thunk(fun(Ty::Var(av), pure(Ty::Unit))))
}

pub static HARNESS_BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: Cow::Borrowed("amnemon"),
        type_rule: BuiltinTypeRule::Scheme(Some(3), scheme_agent_spawn),
        doc: "amnemon <prompt> <title> <permissions>  — launch an independent, blank-context sub-agent. Launch-only and always asynchronous: returns immediately with a receipt [id: Int, title: Str, log-dir: Str]; the child's reply is NOT this call's result — it arrives later, as its own marked turn in your inbox. `prompt` starts a fresh conversation (no shared history, only a value-snapshot of your shell's bindings/cwd/env); wrap it in a raw string #'…'# if it carries $, !, or quotes. `title` must fit the tab-bar contract: non-empty, at most 24 characters, ASCII letters/digits/-/_ only, or the call is refused. `permissions` bounds the child to at most your own authority and must be exactly one of `confined (offline, no home reads), `minimal (working tree + /tmp + network), `read-only (writes only to scratch), `edit-only (edits the working tree, no build tooling), `reasonable (everyday tooling), `dangerous (no narrowing); any other label is refused, naming all six. Delegation depth is finite — each descendant is handed one less unit of fuel than its spawner holds, and once fuel reaches zero this call is refused; fuel bounds how deep a chain may recurse, never how many children you may start at any one depth. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_amnemon),
    },
    BuiltinEntry {
        name: Cow::Borrowed("mnemon"),
        type_rule: BuiltinTypeRule::Scheme(Some(3), scheme_agent_spawn),
        doc: "mnemon <prompt> <title> <permissions>  — launch a sub-agent that inherits your current model-visible conversation and reuses your current provider selection, appending `prompt` as its fresh final prompt (good for cache locality when the child needs the context you already built); it also gets a value-snapshot of your shell's bindings/cwd/env. Launch-only and always asynchronous: returns immediately with a receipt [id: Int, title: Str, log-dir: Str]; the child's reply is NOT this call's result — it arrives later, as its own marked turn in your inbox. Wrap `prompt` in a raw string #'…'# if it carries $, !, or quotes. `title` must fit the tab-bar contract: non-empty, at most 24 characters, ASCII letters/digits/-/_ only, or the call is refused. `permissions` bounds the child to at most your own authority and must be exactly one of `confined (offline, no home reads), `minimal (working tree + /tmp + network), `read-only (writes only to scratch), `edit-only (edits the working tree, no build tooling), `reasonable (everyday tooling), `dangerous (no narrowing); any other label is refused, naming all six. Delegation depth is finite — each descendant is handed one less unit of fuel than its spawner holds, and once fuel reaches zero this call is refused; fuel bounds how deep a chain may recurse, never how many children you may start at any one depth. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_mnemon),
    },
    BuiltinEntry {
        name: Cow::Borrowed("agents"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_agents),
        doc: "agents  — list the live descendants you started that are still running: [[id: Int, title: Str, elapsed-s: Int, log-dir: Str]]. Use it to recover ids after a context compaction, then agent-cancel to stop a straggler. Settled agents are not listed — their replies arrive on their own as marked turns in your inbox. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_agents),
    },
    BuiltinEntry {
        name: Cow::Borrowed("message"),
        type_rule: BuiltinTypeRule::Scheme(Some(2), scheme_message),
        doc: "message <id> <text>  — send `text` as a marked turn to the live descendant named by `id` (from agents, a spawn receipt, or an id you included in a child's prompt); it lands at its next turn boundary, not as human input. Only a descendant of yours may receive it — never a sibling, an ancestor, or yourself; refused otherwise. Does not return the recipient's answer — coordination only. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_message),
    },
    BuiltinEntry {
        name: Cow::Borrowed("agent-cancel"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_agent_cancel),
        doc: "agent-cancel <id>  — cancel the live descendant named by `id` (from agents). It is asked to stop at its next checkpoint and then delivers a cancelled result to your inbox; a no-op if no live agent has that id. Only a descendant of yours may be cancelled — never a sibling, an ancestor, or yourself; refused otherwise. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_agent_cancel),
    },
    BuiltinEntry {
        name: Cow::Borrowed("schedule"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_schedule),
        doc: "schedule <spec>  — arm a self-wakeup: at the chosen time a marked turn carrying the spec's `prompt` is delivered to your inbox and re-engages you at your next turn boundary, with no human present. `spec` is a record with exactly three fields: trigger, label, prompt. `trigger` is exactly one of `cron '<expr>'` — a five-field cron expression (minute hour day-of-month month day-of-week) in the host's local timezone, e.g. `cron '0 9 * * 1-5'` for weekdays at 09:00; recurring — or `after '<n><unit>'` — a one-shot relative delay, unit one of s/m/h/d, e.g. `after '30m'`, `after '2h'`; any other shape is refused, naming both. `label` is `some '<name>'` to give the schedule a human-readable name shown by schedules, or `none` to take the default sched-{id}; any other shape is refused, naming both. `prompt` is the natural-language instruction you act on when woken, not code — e.g. schedule [trigger: `after '30m', label: `none, prompt: 'check the build']. Returns the new schedule's id. Requires the self-wakeup grant (--allow-schedule) — an agent that can wake itself indefinitely holds real authority, so without the grant this call is refused. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_schedule),
    },
    BuiltinEntry {
        name: Cow::Borrowed("schedules"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_schedules),
        doc: "schedules  — list your live scheduled wakeups: [[id: Int, label: Str, trigger: Str, next-s: Int, fires: Int]] — next-s the seconds until the next fire, fires how many times it has fired so far. Use it to recover schedule ids after a context compaction, then unschedule to remove one. Requires the self-wakeup grant (--allow-schedule) and is refused without it. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_schedules),
    },
    BuiltinEntry {
        name: Cow::Borrowed("unschedule"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_unschedule),
        doc: "unschedule <id>  — remove a scheduled wakeup by its id (from schedules). A no-op if no schedule has that id. Requires the self-wakeup grant (--allow-schedule) and is refused without it. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_unschedule),
    },
    BuiltinEntry {
        name: Cow::Borrowed("commit"),
        type_rule: BuiltinTypeRule::Scheme(Some(2), scheme_commit),
        doc: "commit <key> <description>  — open a new protected commitment. `key` is yours to choose: `commitment:` followed by one or more ASCII letters, digits, `.`, `_`, or `-` (e.g. commitment:plan-x); any other shape is refused, naming the grammar. `description` is what you are committing to, in your own words, and must not be empty — it is not saved verbatim. Launch-only and always asynchronous: forks a host-owned, read-only writer child that formalizes your description into concrete, falsifiable criteria, and returns immediately with a receipt [id: Int, title: Str, log-dir: Str]; the writer's own reply is NOT this call's result — it arrives later, as its own marked turn in your inbox, and only a well-formed criteria card opens the pin. You choose the key and the description; the host alone builds the writer's prompt and bounds it to read-only, so you can never steer its criteria or its authority. Refused if `key` is already a live commitment — verify or clear it first. Once open, only a passing verify-commitment can close it; you cannot unpin or overwrite it yourself. Delegation depth is finite — the writer is handed one less unit of fuel than you hold, and once your fuel reaches zero this call is refused. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_commit),
    },
    BuiltinEntry {
        name: Cow::Borrowed("verify-commitment"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_verify_commitment),
        doc: "verify-commitment <key>  — ask a host-owned, read-only verifier child to check one live protected commitment pin. `key` must be the full commitment:-prefixed key of a currently live commitment (from a commit receipt or your own record); any other shape is refused, naming the grammar. You supply only the key — no instructions, evidence, or verifier prompt of your own reach it. Launch-only and always asynchronous: forks the verifier and returns immediately with a receipt [id: Int, title: Str, log-dir: Str]; the verifier's own reply is NOT this call's result — it arrives later, as its own marked turn in your inbox. The host alone reads the saved commitment card, builds the verifier's prompt, and bounds it to read-only; the pin clears only if the verifier returns a structured pass verdict, and stays live on a fail or on any other reply shape. Delegation depth is finite — the verifier is handed one less unit of fuel than you hold, and once your fuel reaches zero this call is refused. Answered only on the turn that calls it: inside spawn { … } this errors.",
        body: BuiltinBody::Static(builtin_verify_commitment),
    },
    BuiltinEntry {
        name: Cow::Borrowed("reply"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_reply),
        doc: "reply <value>  — hand `value` back to whoever spawned you: the sole return path for a returning agent. Your parent receives exactly this value, nothing else — not your reasoning, your shell bindings, or any prose you streamed along the way. `value` must be first-order data: no closures, handles, or environments; passing one fails this call with a didactic error and your run continues, so fix the value and call reply again. Call it more than once in a turn and the last call wins — an earlier value is discarded, not appended. The run does not end at this call: it ends once the enclosing ral call's whole batch of statements finishes draining, so write reply last and let earlier statements in the same script run to completion first. A non-finite Float (NaN, +Infinity, -Infinity) reaches your parent as the string \"NaN\"/\"Infinity\"/\"-Infinity\" — JSON, which the value eventually crosses into, has no such numbers. Refused on the interactive trunk and every /branch child: they converse with the user turn after turn and never return, so they hold no obligation to call this. Answered only on the turn that calls it: inside spawn { … } this errors.",
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
    fn valid_title_boundaries() {
        assert!(valid_title("a"));
        assert!(valid_title("refactor-output"));
        assert!(valid_title("audit_deps"));
        assert!(valid_title(&"x".repeat(24)));
        assert!(!valid_title(""));
        assert!(!valid_title(&"x".repeat(25)));
        assert!(!valid_title("has space"));
        assert!(!valid_title("non-ascii-é"));
    }

    #[test]
    fn permission_label_accepts_every_bake_in() {
        for label in PERMISSION_LABELS {
            let v = Value::Variant { label: label.to_string(), payload: None };
            assert_eq!(permission_label(&v).unwrap(), label);
        }
    }

    #[test]
    fn permission_label_rejects_an_unknown_tag_naming_all_six() {
        let v = Value::Variant { label: "bogus".to_string(), payload: None };
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

    /// A tab-carrying payload on a permissions variant is not a bare tag —
    /// refused just like an unknown label, never silently truncated.
    #[test]
    fn permission_label_rejects_a_variant_carrying_a_payload() {
        let v = Value::Variant {
            label: "confined".to_string(),
            payload: Some(Box::new(Value::Int(1))),
        };
        assert!(permission_label(&v).is_err());
    }

    /// A unique scratch directory per test, mirroring `tests/agent_apply.rs`'s
    /// own `tmp` helper.
    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("exarch-harness-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// An unknown permissions label errors engine-side, naming all six
    /// legal bases, before any enquiry crosses — the door validates title
    /// and permissions before `fork_into_nursery`/`enquire` ever run, so a
    /// malformed call never registers a child.
    #[test]
    fn unknown_permissions_label_errors_before_any_enquiry_crosses() {
        let dir = tmp("unknown-permissions");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r#"amnemon #'hi'# "t" `bogus"#,
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
            "an unknown permissions label must never register a child"
        );
    }

    /// An invalid title errors engine-side naming the tab-bar contract,
    /// before any enquiry crosses — the door check the JSON tool's silent
    /// `sub-{N}` fallback used to paper over.
    #[test]
    fn invalid_title_errors_before_any_enquiry_crosses() {
        let dir = tmp("invalid-title");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            r#"amnemon #'hi'# "has space" `confined"#,
            5,
            &emit,
        );
        assert!(
            result.content.contains("title"),
            "got: {}",
            result.content
        );
        assert!(
            session.agents.list(session.id).is_empty(),
            "an invalid title must never register a child"
        );
    }

    /// The full stack, end to end: a scripted provider issues a `ral` tool
    /// call whose script is `` amnemon #'say hi'# 'helper' `read-only ``
    /// — real source, parsed and type-checked, crossing the desk through a
    /// real nursery fork — and the receipt record is the turn's value,
    /// while the child's own reply later settles into the parent's inbox.
    ///
    /// Drives `run_shell` directly rather than `Agent::apply`'s provider
    /// loop: the spawned child inherits the parent's *own* `Arc<Provider>`
    /// (`agent-start`'s `ProviderHandle::new(services.provider.current())`),
    /// so a script consumed by both a driven parent turn and its spawned
    /// child races unpredictably over which one gets which stage — the same
    /// reason `tools::commitment`'s own scripted-child tests never drive the
    /// parent's `apply` either.
    #[test]
    fn amnemon_full_stack_round_trip_delivers_receipt_and_settles_into_inbox() {
        let dir = tmp("full-stack-round-trip");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(crate::provider::scripted::Reply::tool_calls(vec![
                genai::chat::ToolCall {
                    call_id: "reply-1".into(),
                    fn_name: "reply".into(),
                    fn_arguments: serde_json::json!({ "result": "say hi" }),
                    thought_signatures: None,
                },
            ])),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"amnemon #'say hi'# 'helper' `read-only",
            5,
            &emit,
        );
        assert!(
            result.content.contains("helper"),
            "the receipt record must be the turn's value, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.drain_turn_for_test() {
                Some(crate::bus::Turn::Agent(r)) => {
                    assert!(
                        r.text.contains("say hi"),
                        "the child's reply must settle into the parent's inbox, got: {}",
                        r.text
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result turn"),
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
    /// parser's own message, before any enquiry crosses — port of
    /// `ScheduleTool::dispatch`'s trigger-rule matrix (`tools/schedule.rs`
    /// ~111–141) as a builtin door test.
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
    /// label is absent — the accurate diagnostic the closed-variant
    /// experiment never got.
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
    /// type-checked, crossing the desk — answers the new schedule's id, and
    /// once it fires the marked wakeup lands in the inbox as a
    /// [`crate::bus::Turn::Wakeup`]. Mirrors `crate::schedule`'s own
    /// `after_fires_once_then_is_removed` for how to wait for the fire — a
    /// real second, since `parse_duration`'s smallest unit is whole
    /// seconds.
    #[test]
    fn schedule_full_stack_round_trip_answers_id_and_fires_into_inbox() {
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
        let live = session.schedules.list();
        assert_eq!(live.len(), 1, "the schedule must be registered");
        assert_eq!(live[0].label, format!("sched-{}", live[0].id));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match session.drain_turn_for_test() {
                Some(crate::bus::Turn::Wakeup(text)) => {
                    assert!(
                        text.contains("wake"),
                        "the wakeup must carry the prompt, got: {text}"
                    );
                    break;
                }
                Some(_other) => panic!("expected a Wakeup turn"),
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

    // ── commitment family door tests ─────────────────────────────────────

    /// A key that does not match the `commitment:*` grammar errors
    /// engine-side, naming it, before any enquiry crosses.
    #[test]
    fn bad_commit_key_errors_before_any_enquiry_crosses() {
        let dir = tmp("bad-commit-key");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "commit 'not-a-key' 'do the thing'",
            5,
            &emit,
        );
        assert!(
            result.content.contains("commitment:"),
            "must name the grammar, got: {}",
            result.content
        );
        assert!(
            session.agents.list(session.id).is_empty(),
            "a malformed key must never register a writer"
        );
    }

    /// An empty `description` errors engine-side, before any enquiry
    /// crosses.
    #[test]
    fn empty_commit_description_errors_before_any_enquiry_crosses() {
        let dir = tmp("empty-commit-description");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell(
            "call-1".to_string(),
            "commit 'commitment:abc' ''",
            5,
            &emit,
        );
        assert!(
            result.content.contains("description"),
            "got: {}",
            result.content
        );
        assert!(
            session.agents.list(session.id).is_empty(),
            "an empty description must never register a writer"
        );
    }

    /// A key that does not match the `commitment:*` grammar errors
    /// engine-side for `verify-commitment` too, before any enquiry crosses.
    #[test]
    fn bad_verify_commitment_key_errors_before_any_enquiry_crosses() {
        let dir = tmp("bad-verify-commitment-key");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let result = session.run_shell("call-1".to_string(), "verify-commitment 'nope'", 5, &emit);
        assert!(
            result.content.contains("commitment:"),
            "must name the grammar, got: {}",
            result.content
        );
        assert!(
            session.agents.list(session.id).is_empty(),
            "a malformed key must never register a verifier"
        );
    }

    /// The full stack, end to end: `` commit 'commitment:abc' 'ship the
    /// thing, tests must pass' `` — real source, parsed and type-checked,
    /// crossing the desk through a real nursery fork — and the receipt
    /// record is the turn's value, while the writer's own well-formed
    /// criteria card settles tagged for the parent to open, mirroring
    /// `tools::commitment::writer_settles_the_new_commitment_open_for_the_parent`.
    /// Drives `run_shell` directly rather than `Agent::apply`'s provider
    /// loop, for the same reason `amnemon_full_stack_round_trip_delivers_receipt_and_settles_into_inbox`
    /// does: a script consumed by both a driven parent turn and its spawned
    /// child races unpredictably over which one gets which stage.
    #[test]
    fn commit_full_stack_round_trip_delivers_receipt_and_tags_the_open_settle() {
        let dir = tmp("commit-full-stack-round-trip");
        let key = "commitment:abc";
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let card_json = serde_json::json!({
            "kind": "commitment_card",
            "commitment_key": key,
            "summary": "ship the thing",
            "criteria": [{ "id": "tests", "text": "tests pass" }],
        })
        .to_string();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(crate::provider::scripted::Reply::tool_calls(vec![
                genai::chat::ToolCall {
                    call_id: "reply-1".into(),
                    fn_name: "reply".into(),
                    fn_arguments: serde_json::json!({ "result": card_json }),
                    thought_signatures: None,
                },
            ])),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            "commit 'commitment:abc' 'ship the thing, tests must pass'",
            5,
            &emit,
        );
        assert!(
            result.content.contains("EXIT: 0"),
            "a valid commit call must succeed, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.drain_turn_for_test() {
                Some(crate::bus::Turn::Agent(r)) => {
                    assert!(
                        matches!(
                            &r.commitment_settle,
                            Some(crate::tools::commitment::CommitmentSettle::Open { key: k, .. }) if k == key
                        ),
                        "a passing writer card must tag the settle for the parent to open, got: {:?}",
                        r.commitment_settle
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result turn"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "writer child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }

    // ── `reply` ───────────────────────────────────────────────────────────

    /// A `ral` tool call carrying a real script as its `cmd` — the shape a
    /// scripted child's own turn issues, mirroring `agent.rs`'s private
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
    /// scripted turn runs `` reply [files: $found] `` — real source, parsed
    /// and type-checked, crossing the desk through a real enquiry — and the
    /// structured record it built reaches the parent's inbox, not a
    /// flattened string. Substitutes the child's own script for the
    /// `reply` *tool* call `amnemon_full_stack_round_trip_delivers_receipt_and_settles_into_inbox`
    /// uses, for the same non-driven-parent reason that test documents.
    #[test]
    fn reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox() {
        let dir = tmp("reply-full-stack-round-trip");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(crate::provider::scripted::Reply::tool_calls(vec![
                ral_call("c1", r#"let found = ["a.rs", "b.rs"]; reply [files: $found]"#),
            ])),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);

        let result = session.run_shell(
            "call-1".to_string(),
            r"amnemon #'find files'# 'finder' `read-only",
            5,
            &emit,
        );
        assert!(
            result.content.contains("finder"),
            "the receipt record must be the turn's value, got: {}",
            result.content
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.drain_turn_for_test() {
                Some(crate::bus::Turn::Agent(r)) => {
                    assert!(
                        r.text.contains("files:") && r.text.contains("a.rs") && r.text.contains("b.rs"),
                        "the structured record must reach the parent inbox, got: {}",
                        r.text
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result turn"),
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

    /// A double `reply` within one turn is last-wins: the second call's
    /// value is what the turn settles `Replied` with.
    #[test]
    fn double_reply_in_one_turn_is_last_wins() {
        let dir = tmp("double-reply-last-wins");
        let mut session = crate::agent::Agent::for_test(&dir, "system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new().then(crate::provider::scripted::Reply::tool_calls(vec![
                ral_call("c1", r#"reply "first"; reply "second""#),
            ])),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let provider_handle = session.current_provider();
        let outcome = session.apply(
            &provider_handle,
            Some("go".into()),
            &crate::cancel::Token::new(),
            &emit,
        );
        match outcome {
            Ok(crate::agent::TurnOutcome::Replied(v)) => {
                assert_eq!(
                    v,
                    ral_core::serial::FOValue::String { value: "second".into() },
                    "the last reply in the turn must win"
                );
            }
            other => panic!("expected Replied, got {other:?}"),
        }
    }
}
