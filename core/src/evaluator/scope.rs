//! Lifted helpers for the structural scope IR nodes.
//!
//! The evaluator arms for `Within`/`Grant`/`Try`/`Guard`/`Audit` need
//! three pieces of vocabulary:
//!
//!   * [`Outcome`] / [`classify`] — `try`'s body-result classification
//!     into a normalised record the handler-call and audit-tag paths
//!     can read uniformly.
//!   * [`WithinScope`] — the parsed `within [...]` options map plus the
//!     [`WithinScope::enter`] composer that runs a body inside the
//!     resulting nest of `Shell::with_*` scopes.
//!   * [`eval_within`] / [`eval_grant`] / [`eval_try`] / [`eval_guard`] /
//!     [`eval_audit`] — the per-arm bodies extracted out of
//!     [`crate::evaluator::eval_comp`]'s giant `match`.  Lifting them
//!     keeps the match arms themselves one-liners so the unoptimised
//!     debug frame of `eval_comp` stays small enough for deep
//!     recursion under the default 2 MiB test-thread stack (see
//!     `deeply_nested_calls`).

use crate::ir::Val;
use crate::types::*;

use crate::evaluator::val::eval_val;
use crate::evaluator::{apply, audit};
use std::collections::HashMap;
use std::path::PathBuf;

/// Classified outcome handed to `try`'s handler-call and audit-tag
/// paths.  Derived from the body's result; never carries the canonical
/// `Result` — all fields below are projection.
pub(crate) struct Outcome {
    pub ok: bool,
    pub status: i32,
    pub value: Value,
    pub message: String,
    pub cmd: String,
    pub line: usize,
    pub col: usize,
}

/// The error record handed to `try`'s handler thunk — `{cmd, status,
/// message, line, col}`.  Bytes are absent by design (§10.1): use `audit`
/// for forensic capture.  Shared by `try`'s handler call and `poll`'s
/// `` `err `` outcome so a caught error and a polled failure read the same
/// shape; the field order matches `typecheck::builtins::try_error_record`.
pub(crate) fn error_record(
    cmd: &str,
    status: i32,
    message: &str,
    line: usize,
    col: usize,
) -> Value {
    Value::map(vec![
        ("cmd".into(), Value::String(cmd.to_string())),
        ("status".into(), Value::Int(status as i64)),
        ("message".into(), Value::String(message.to_string())),
        ("line".into(), Value::Int(line as i64)),
        ("col".into(), Value::Int(col as i64)),
    ])
}

pub(crate) fn classify(
    body: &BodyResult,
    children: &[ExecNode],
    call_line: usize,
    call_col: usize,
) -> Outcome {
    match body {
        BodyResult::Value(v) => Outcome {
            ok: true,
            status: 0,
            value: v.clone(),
            message: String::new(),
            cmd: String::new(),
            line: call_line,
            col: call_col,
        },
        BodyResult::Error(e) => {
            let failing = children.iter().rev().find(|n| n.status != 0);
            Outcome {
                ok: false,
                status: e.exit_code(),
                value: Value::Unit,
                message: e.message.clone(),
                cmd: failing
                    .map(|n| n.cmd.clone())
                    .unwrap_or_else(|| "<runtime>".into()),
                line: e.loc.as_ref().map(|l| l.line).unwrap_or(call_line),
                col: e.loc.as_ref().map(|l| l.col).unwrap_or(call_col),
            }
        }
    }
}

/// The parsed `within [...]` options, ready to enter a scope.  Each key
/// becomes a `Shell::with_*` call, composed left-to-right in `enter`.
pub(crate) struct WithinScope {
    env_overrides: Option<HashMap<String, String>>,
    cwd: Option<PathBuf>,
    handlers: Option<(Vec<HandlerEntry>, Option<Value>)>,
}

impl WithinScope {
    pub(crate) fn parse(opts: &Map, shell: &mut Shell) -> Settled<Self> {
        let mut env_overrides = None;
        let mut cwd = None;
        let mut entries: Vec<HandlerEntry> = Vec::new();
        let mut catch_all: Option<Value> = None;
        let mut saw_handlers = false;

        for (k, v) in opts {
            match k.as_str() {
                "env" => {
                    let overrides = as_map(v, "within env")?;
                    for k in overrides.keys() {
                        if matches!(k.as_str(), "PWD" | "OLDPWD") {
                            return Err(sig(format!(
                                "within env: `{k}` is derived from the shell's working \
                                 directory and cannot be set here; use `cd` to change the \
                                 directory"
                            )));
                        }
                    }
                    let map = overrides
                        .into_iter()
                        .map(|(ek, ev)| {
                            let sv = match ev {
                                Value::String(s) => s,
                                Value::Int(n) => n.to_string(),
                                Value::Float(n) => n.to_string(),
                                Value::Bool(b) => b.to_string(),
                                other => {
                                    return Err(sig(format!(
                                        "within env: value for '{ek}' must be a scalar (string, int, float, or bool), got {}",
                                        other.type_name()
                                    )));
                                }
                            };
                            Ok((ek, sv))
                        })
                        .collect::<Settled<HashMap<String, String>>>()?;
                    env_overrides = Some(map);
                }
                "dir" => {
                    let path = v.to_string();
                    if path.is_empty() {
                        return Err(sig("within dir: path cannot be empty"));
                    }
                    let rp = shell.resolve(&path);
                    shell.check_fs_read(&rp)?;
                    if !rp.as_path().is_dir() {
                        return Err(sig(format!("within dir: {path}: not a directory")));
                    }
                    cwd = Some(rp.into_inner());
                }
                "handlers" => {
                    let map = as_map(v, "within handlers")?;
                    entries = map
                        .into_iter()
                        .map(|(cmd, thunk_val)| {
                            validate_handler_arity(
                                &thunk_val,
                                1,
                                &format!("within handlers: `{cmd}`"),
                            )?;
                            let Value::Lambda { param, body, .. } = &thunk_val else {
                                unreachable!("validate_handler_arity guarantees a unary lambda");
                            };
                            if shell.mobile.scope.get(&cmd).is_some()
                                || crate::builtins::is_builtin(&cmd)
                            {
                                return Err(sig(format!(
                                    "within handlers: cannot install handler for binding `{cmd}`"
                                )));
                            }
                            crate::typecheck::alias_arm_scheme(
                                &cmd,
                                param,
                                body,
                                shell.session_schemes(),
                            )
                            .map_err(|m| {
                                use crate::typecheck::fmt_mode;
                                sig(format!(
                                    "within handlers: `{cmd}`'s body changes the head's \
                                     pipeline mode ({} vs {}); a handler reinterprets a head \
                                     and must preserve its modes — use a byte-output body or a codec",
                                    fmt_mode(&m.left),
                                    fmt_mode(&m.right)
                                ))
                            })?;
                            Ok(HandlerEntry::ral_per_name(cmd, thunk_val))
                        })
                        .collect::<Settled<Vec<HandlerEntry>>>()?;
                    saw_handlers = true;
                }
                "handler" => {
                    validate_handler_arity(v, 2, "within handler: catch-all")?;
                    catch_all = Some(v.clone());
                    saw_handlers = true;
                }
                _ => return Err(sig(format!("within: unknown key '{k}'"))),
            }
        }

        let handlers = if saw_handlers {
            Some((entries, catch_all))
        } else {
            None
        };
        Ok(Self {
            env_overrides,
            cwd,
            handlers,
        })
    }

    /// Compose the parsed keys as nested `with_*` scopes around `body`.
    pub(crate) fn enter<R>(self, shell: &mut Shell, body: impl FnOnce(&mut Shell) -> R) -> R {
        let Self {
            env_overrides,
            cwd,
            handlers,
        } = self;
        let wrapped = |shell: &mut Shell| match handlers {
            Some((entries, catch_all)) => shell.with_handlers(entries, catch_all, body),
            None => body(shell),
        };
        let wrapped = |shell: &mut Shell| match cwd {
            Some(path) => shell.with_cwd(path, wrapped),
            None => wrapped(shell),
        };
        match env_overrides {
            Some(o) => shell.with_env(o, wrapped),
            None => wrapped(shell),
        }
    }
}

// ── Lifted `eval_comp` arms ──────────────────────────────────────────────
//
// Each helper below was the body of one match arm in
// `crate::evaluator::eval_comp`.  Pulling them out keeps `eval_comp`'s
// debug-mode stack frame small — the unoptimised compiler reserves
// space for every arm's locals on entry regardless of which arm is
// taken, and these scope arms each carry several `Value` / `Comp` /
// closure locals.  See the module comment for the deeper rationale.

pub(crate) fn eval_within(opts: &Val, body: &Val, shell: &mut Shell) -> Raw<Value> {
    let opts_val = eval_val(opts, shell)?;
    let opts_map = as_map(&opts_val, "within")?;
    let scope = WithinScope::parse(&opts_map, shell)?;
    let body = eval_val(body, shell)?;
    audit::with_scope(shell, "within", |shell| {
        scope.enter(shell, |shell| apply(body, vec![], shell))
    })
    .map_err(Into::into)
}

pub(crate) fn eval_grant(caps: &Val, body: &Val, shell: &mut Shell) -> Raw<Value> {
    let caps_val = eval_val(caps, shell)?;
    let home = shell.mobile.context.home();
    let cwd = shell.cwd();
    let ctx = crate::path::sigil::FreezeCtx {
        home: &home,
        cwd: &cwd,
    };
    let caps = crate::capability::decode_capability_map(&caps_val, "grant", &ctx)?;
    let body = eval_val(body, shell)?;
    audit::with_scope(shell, "grant", |shell| {
        shell.with_capabilities(caps, |shell| apply(body, vec![], shell))
    })
    .map_err(Into::into)
}

pub(crate) fn eval_try(body: &Val, handler: &Val, shell: &mut Shell) -> Raw<Value> {
    // `try` is pure control flow: bytes still flow through fd 1/2, so
    // the capture policy is `None`.  `record_scope` still collects
    // children (forced) so we can name the failing command in the
    // error record.  Exit/Stopped propagate cleanly as `Err(Escape)`
    // through `?`, which lifts via `From<Escape> for Control`; the
    // parked-pipeline case never reaches `classify`.
    let call_line = shell.turn.loc.line;
    let call_col = shell.turn.loc.col;
    let body_val = eval_val(body, shell)?;
    let handler_val = eval_val(handler, shell)?;
    let record = audit::record_scope(shell, "try", CapturePolicy::Off, |s| {
        apply(body_val, vec![], s)
    })?;

    if cfg!(debug_assertions)
        && let BodyResult::Error(e) = &record.body
    {
        let loc = e
            .loc
            .as_ref()
            .map(|l| format!(" ({}:{})", l.line, l.col))
            .unwrap_or_default();
        eprintln!("ral: try caught error{loc}: {}", e.message);
    }

    let outcome = classify(&record.body, &record.node.children, call_line, call_col);

    let err_record = error_record(
        &outcome.cmd,
        outcome.status,
        &outcome.message,
        outcome.line,
        outcome.col,
    );

    // The `[`ok A | `err ErrorRec]` variant attached to the audit node's
    // `value` so `--audit` output retains the success/failure tag for
    // each `try`; `try` itself returns body-or-handler value directly.
    let variant = if outcome.ok {
        Value::Variant {
            label: "ok".into(),
            payload: Some(Box::new(outcome.value.clone())),
        }
    } else {
        Value::Variant {
            label: "err".into(),
            payload: Some(Box::new(err_record.clone())),
        }
    };

    let mut node = record.node;
    node.value = variant;
    shell.local.audit.push(node);
    shell.mobile.control.last_status = 0;

    if outcome.ok {
        Ok(outcome.value)
    } else {
        apply(handler_val, vec![err_record], shell).map_err(Into::into)
    }
}

pub(crate) fn eval_guard(body: &Val, cleanup: &Val, shell: &mut Shell) -> Raw<Value> {
    let body_val = eval_val(body, shell)?;
    let cleanup_val = eval_val(cleanup, shell)?;
    audit::with_scope(shell, "guard", |shell| {
        let body_result = apply(body_val, vec![], shell);
        // Cleanup is the guard's finalizer, run after the body whatever
        // the body did. A cleanup *error* is catchable and best-effort:
        // it is logged and the body's result stands, so an ordinary
        // failure in the finalizer cannot mask the body's outcome. A
        // cleanup *escape* (`exit`, `Stopped`) is non-local control that
        // must not be swallowed — dropping `Stopped` orphans a stopped
        // process group (pgid lost, never resumable or reapable) — so it
        // takes priority over the body result and propagates.
        match apply(cleanup_val, vec![], shell) {
            Ok(_) => body_result,
            Err(Break::Error(err)) => {
                crate::diagnostic::cmd_error("guard", &format!("cleanup failed: {err}"));
                body_result
            }
            Err(escape @ Break::Escape(_)) => Err(escape),
        }
    })
    .map_err(Into::into)
}

pub(crate) fn eval_audit(body: &Val, shell: &mut Shell) -> Raw<Value> {
    // Exit / Stopped propagate as `Err(Escape)` from `record_scope`;
    // `?` lifts via `From<Escape> for Control` without classification.
    let body_val = eval_val(body, shell)?;
    let record = audit::record_scope(shell, "audit", CapturePolicy::Bytes, |s| {
        apply(body_val, vec![], s)
    })?;
    let node = record.node;
    let status = node.status;
    shell.local.audit.push(node.clone());
    shell.mobile.control.last_status = status;
    Ok(node.to_value())
}
