//! CBPV evaluator for `ral`.
//!
//! [`eval_comp`] and [`eval_val`] implement the computation and value layers
//! of the Call-By-Push-Value IR.  [`evaluate`] is the top-level entry point;
//! it runs a computation and lands any escaping `TailCall` signals via the
//! trampoline.  Support machinery lives in submodules:
//!
//! - [`expr`] — primitive operators and indexing
//! - [`dispatch`] — command dispatch, argument evaluation, effect handlers
//! - [`trampoline`] — iterative tail-call loop
//! - [`pattern`] — destructuring bind
//! - [`exec`] — external process spawning and I/O wiring
//! - [`audit`] — exec-tree recording for `audit { … }` blocks

pub mod audit;
pub(crate) mod dispatch;
pub mod exec;
pub mod expr;
pub(crate) mod pattern;
pub(crate) mod pipeline;
pub(crate) mod trampoline;

use crate::diagnostic;
use crate::io::Sink;
use crate::ir::*;
use crate::types::*;
use crate::util::{expand_tilde_path, parse_literal};
use pattern::assign_pattern;
use std::sync::{Arc, Mutex};
pub use trampoline::trampoline;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Run `f` with `shell.control.in_tail_position` set to `val`, then restore the saved value.
pub(crate) fn with_tail<R>(shell: &mut Shell, val: bool, f: impl FnOnce(&mut Shell) -> R) -> R {
    let saved = std::mem::replace(&mut shell.control.in_tail_position, val);
    let r = f(shell);
    shell.control.in_tail_position = saved;
    r
}

/// Swap stdout for an in-memory buffer, run `f`, restore, return `(result, bytes)`.
/// §4.3: lets callers bind the byte output of commands as a value.  Sets
/// `shell.io.capture_outer` so a `Seq` flushes non-final stages to the outer
/// (visible) stdout, leaving only the last stage's bytes in the buffer —
/// the let-binding semantics.
pub fn with_capture<R, F>(shell: &mut Shell, f: F) -> (R, Vec<u8>)
where
    F: FnOnce(&mut Shell) -> R,
{
    capture_inner(shell, true, f)
}

/// Like `with_capture`, but does not install `capture_outer` — `Seq`'s
/// non-final-flush rule (§4.3) is suppressed, so every stage's bytes
/// accumulate in the buffer.  Used by `try`/`_try` to capture the body's
/// full transcript, matching `await`'s "the record carries everything"
/// semantics.
pub fn with_full_capture<R, F>(shell: &mut Shell, f: F) -> (R, Vec<u8>)
where
    F: FnOnce(&mut Shell) -> R,
{
    capture_inner(shell, false, f)
}

fn capture_inner<R, F>(shell: &mut Shell, install_outer: bool, f: F) -> (R, Vec<u8>)
where
    F: FnOnce(&mut Shell) -> R,
{
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let saved = std::mem::replace(&mut shell.io.stdout, Sink::Buffer(buf.clone()));
    let new_outer = if install_outer {
        saved.try_clone().ok()
    } else {
        None
    };
    let prev_outer = std::mem::replace(&mut shell.io.capture_outer, new_outer);
    let result = f(shell);
    shell.io.capture_outer = prev_outer;
    shell.io.stdout = saved;
    let bytes = buf
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default();
    (result, bytes)
}

/// §4.3: capture stdout when RHS is byte-mode; clear tail position throughout.
fn eval_bind_rhs(m: &Comp, shell: &mut Shell) -> Result<Value, EvalSignal> {
    with_tail(shell, false, |shell| {
        let mode = crate::ty::InferCtx::new().output_mode(m, Some(shell));
        if mode == crate::ty::Mode::Bytes {
            let (inner, bytes) = with_capture(shell, |shell| eval_comp(m, shell));
            inner.and_then(|v| match v {
                Value::Unit => {
                    let mut s = String::from_utf8(bytes).map_err(|e| {
                        EvalSignal::Error(
                            Error::new(
                                format!("`(let)` returned bytes that are not valid UTF-8: {e}"),
                                1,
                            )
                            .with_hint("bind with `| from-bytes` to keep raw output"),
                        )
                    })?;
                    if s.ends_with('\n') {
                        s.pop();
                    }
                    Ok(Value::String(s))
                }
                other => Ok(other),
            })
        } else {
            eval_comp(m, shell)
        }
    })
}

/// Render one piece of a string interpolation as text.
/// Only scalar types (`Unit`, `String`, `Int`, `Float`, `Bool`) are interpolable;
/// structured values produce a diagnostic error.
fn interpolate_piece(v: &Value, shell: &Shell) -> Result<String, EvalSignal> {
    match v {
        Value::Unit => Ok(String::new()),
        Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => Ok(v.to_string()),
        Value::Bytes(_) => Err(shell.err_hint(
            "cannot interpolate Bytes in string",
            "decode first: read-string $bytes",
            1,
        )),
        _ => Err(shell.err_hint(
            format!("cannot interpolate {} in string", v.type_name()),
            "use str to convert, or index into the value",
            1,
        )),
    }
}

// ── Entry point ──────────────────────────────────────────────────────────

/// Top-level evaluation entry point.
///
/// Runs `comp` in the given environment.  If the outermost result is a
/// `TailCall` signal (from a tail-position application), it is landed by
/// the trampoline rather than propagated to the caller.
pub fn evaluate(comp: &Comp, shell: &mut Shell) -> Result<Value, EvalSignal> {
    debug_assert!(
        !shell.dynamic.capabilities_stack.is_empty(),
        "capabilities_stack must be non-empty; \
         Shell::new pre-pushes root — someone popped past it"
    );
    match eval_comp(comp, shell) {
        Err(EvalSignal::TailCall { callee, args }) => trampoline(callee, args, shell),
        other => other,
    }
}

// ── Computation evaluation ───────────────────────────────────────────────

/// Evaluate a computation term of the CBPV IR.
///
/// Each `CompKind` variant maps to its operational semantics: `Return`
/// produces a value, `Lam` captures a closure, `Force` demands a thunk,
/// `App`/`Exec` dispatch commands, `Bind` sequences with destructuring,
/// and so on.  Source-position tracking is updated from the node's span
/// before dispatch so errors carry location information.
///
/// A `TailCall` signal may escape from tail-position applications;
/// the caller (or [`evaluate`]) must land it via the trampoline.
pub fn eval_comp(comp: &Comp, shell: &mut Shell) -> Result<Value, EvalSignal> {
    // Update source position from the node's span.
    if let Some(span) = comp.span {
        if let Some(src) = shell.location.source.as_deref() {
            let (l, c) = crate::diagnostic::byte_to_line_col(src, span.start as usize);
            shell.location.line = l;
            shell.location.col = c;
        } else {
            shell.location.line = span.start as usize;
            shell.location.col = 0;
        }
    }

    match &comp.kind {
        CompKind::Return(val) => {
            let v = eval_val(val, shell)?;
            if let Value::Bool(b) = v {
                shell.set_status_from_bool(b);
            }
            Ok(v)
        }

        CompKind::Lam { .. } => Ok(Value::Thunk {
            body: Arc::new(comp.clone()),
            captured: shell.snapshot(),
        }),

        CompKind::Rec { name, body } => {
            // rec(f. M) → M with f bound to thunk(rec(f. M))
            let rec_thunk = Value::Thunk {
                body: Arc::new(comp.clone()),
                captured: shell.snapshot(),
            };
            with_scope(shell, |shell| {
                shell.set(name.clone(), rec_thunk);
                eval_comp(body, shell)
            })
        }

        CompKind::LetRec { slot, bindings } => {
            let snap = shell.snapshot();
            shell.push_scope();
            // Fixpoint encoding: install each binding as a self-referential thunk.
            for (i, (name, _rhs)) in bindings.iter().enumerate() {
                shell.set(
                    name.clone(),
                    Value::Thunk {
                        body: Arc::new(Comp::new(CompKind::LetRec {
                            slot: Some(i),
                            bindings: bindings.clone(),
                        })),
                        captured: snap.clone(),
                    },
                );
            }
            match slot {
                None => {
                    let lambdas = bindings
                        .iter()
                        .map(|(_, lam)| eval_comp(lam, shell))
                        .collect::<Result<Vec<_>, _>>()?;
                    shell.pop_scope();
                    for ((name, _), lambda) in bindings.iter().zip(lambdas) {
                        shell.set(name.clone(), lambda);
                    }
                    Ok(Value::Unit)
                }
                Some(i) => {
                    let lambda = eval_comp(&bindings[*i].1, shell)?;
                    shell.pop_scope();
                    Ok(lambda)
                }
            }
        }

        CompKind::Force(val) => {
            let v = eval_val(val, shell)?;
            let result = force(v, shell)?;
            if let Value::Bool(b) = result {
                shell.set_status_from_bool(b);
                return Ok(Value::Bool(b));
            }
            Ok(result)
        }

        CompKind::Interpolation(parts) => parts
            .iter()
            .try_fold(String::new(), |mut s, p| {
                s.push_str(&interpolate_piece(&eval_val(p, shell)?, shell)?);
                Ok(s)
            })
            .map(Value::String),

        CompKind::PrimOp(op, args) => expr::eval_primop(*op, args, shell),

        CompKind::Index { target, keys } => {
            let mut v = eval_comp(target, shell)?;
            for key in keys {
                v = expr::index_value(&v, &eval_comp(key, shell)?, shell)?;
            }
            Ok(v)
        }

        CompKind::Bind {
            comp: m,
            pattern,
            rest,
        } => {
            crate::signal::check(shell)?;
            let val = eval_bind_rhs(m, shell)?;
            if let Value::Bool(b) = val {
                shell.set_status_from_bool(b);
            }
            assign_pattern(pattern, &val, shell)?;
            eval_comp(rest, shell)
        }

        CompKind::App {
            head,
            args,
            redirects,
        } => {
            let (head_val, arg_vals, redir_eval) = dispatch::eval_subcall(shell, |shell| {
                let head_val = eval_comp(head, shell)?;
                let (arg_vals, redir_eval) = dispatch::eval_call_parts(args, redirects, shell)?;
                Ok((head_val, arg_vals, redir_eval))
            })?;
            dispatch::eval_app(&head_val, &arg_vals, &redir_eval, shell)
        }

        CompKind::Exec {
            name,
            args,
            redirects,
            external_only,
        } => {
            let (arg_vals, redir_eval) =
                dispatch::eval_subcall(shell, |shell| dispatch::eval_call_parts(args, redirects, shell))?;
            dispatch::dispatch_by_name(name, &arg_vals, &redir_eval, *external_only, shell)
        }

        CompKind::Pipeline(stages) => {
            if stages.len() == 1 {
                return eval_comp(&stages[0], shell);
            }
            pipeline::run_pipeline(stages, shell)
        }

        CompKind::Chain(parts) => {
            // `a ? b`: try each arm; return first success, last error, or Unit.
            let mut last_err = None;
            for part in parts {
                crate::signal::check(shell)?;
                match eval_comp(part, shell) {
                    Ok(result) => return Ok(result),
                    Err(EvalSignal::Error(e)) => {
                        shell.control.last_status = e.status;
                        last_err = Some(EvalSignal::Error(e));
                    }
                    Err(other) => return Err(other),
                }
            }
            last_err.map_or(Ok(Value::Unit), Err)
        }

        CompKind::Background(inner) => {
            let captured = shell.snapshot();
            let thunk = Value::Thunk {
                body: Arc::new(*inner.clone()),
                captured: captured.clone(),
            };
            let mut fork_env = Shell::child_of(&captured, shell);
            let result = crate::builtins::call("spawn", &[thunk], &mut fork_env)?
                .ok_or_else(|| shell.err("internal error: spawn not found", 1))?;
            shell.control.last_status = 0;
            Ok(result)
        }

        CompKind::If { cond, then, else_ } => {
            let saved_tail = shell.control.in_tail_position;
            let cond_val = with_tail(shell, false, |shell| eval_comp(cond, shell))?;
            let b = match cond_val {
                Value::Bool(b) => b,
                other => {
                    return Err(shell.err(
                        format!("if: expected Bool, got {} '{}'", other.type_name(), other),
                        1,
                    ));
                }
            };
            shell.set_status_from_bool(b);
            let branch = if b { then } else { else_ };
            let branch_val = eval_val(branch, shell)?;
            if saved_tail && matches!(branch_val, Value::Thunk { .. }) {
                return Err(EvalSignal::TailCall {
                    callee: branch_val,
                    args: Vec::new(),
                });
            }
            force(branch_val, shell)
        }

        CompKind::Seq(comps) => {
            let mut result = Value::Unit;
            let len = comps.len();
            let was_tail = shell.control.in_tail_position;
            for (i, c) in comps.iter().enumerate() {
                crate::signal::check(shell)?;
                let last = i == len - 1;
                result = with_tail(shell, was_tail && last, |shell| eval_comp(c, shell))?;
                // §4.3: flush non-final bytes so side-effects remain visible.
                if !last
                    && let Some(outer) = &shell.io.capture_outer
                {
                    shell.io
                        .stdout
                        .flush_to(outer)
                        .map_err(|e| shell.err(format!("seq flush: {e}"), 1))?;
                }
            }
            Ok(result)
        }
    }
}

// ── Value evaluation ─────────────────────────────────────────────────────

/// Evaluate a value term of the CBPV IR.
///
/// Values are side-effect-free: literals, variables, thunk closures,
/// list/map constructors, and tilde-expansion.  A `Val::Variable` is
/// looked up first in the environment, then in the builtin registry.
pub(crate) fn eval_val(val: &Val, shell: &mut Shell) -> Result<Value, EvalSignal> {
    match val {
        Val::Unit => Ok(Value::Unit),
        Val::Int(n) => Ok(Value::Int(*n)),
        Val::Float(f) => Ok(Value::Float(*f)),
        Val::Bool(b) => Ok(Value::Bool(*b)),
        Val::Literal(s) => Ok(parse_literal(s)),
        Val::Variable(name) => shell
            .get(name)
            .cloned()
            .or_else(|| shell.resolve_builtin(name))
            .ok_or_else(|| {
                shell.err_hint(
                    format!("undefined variable: ${name}"),
                    "check spelling, or ensure the variable is defined before this line",
                    1,
                )
            }),
        Val::Thunk(body) => Ok(Value::Thunk {
            body: body.clone(),
            captured: shell.snapshot(),
        }),
        Val::List(elems) => eval_list(elems, shell),
        Val::Map(entries) => eval_map(entries, shell),
        Val::TildePath(path) => Ok(Value::String(expand_tilde_path(
            path.user.as_deref(),
            path.suffix.as_deref(),
            &std::env::var("HOME").unwrap_or_default(),
        ))),
    }
}

/// Evaluate a list literal, expanding `...spread` elements inline.
fn eval_list(elems: &[ValListElem], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let mut items = Vec::with_capacity(elems.len());
    for elem in elems {
        match elem {
            ValListElem::Single(v) => items.push(eval_val(v, shell)?),
            ValListElem::Spread(v) => match eval_val(v, shell)? {
                Value::List(inner) => items.extend(inner),
                val => {
                    return Err(shell.err_hint(
                        format!("spread requires a List, got {}", val.type_name()),
                        "spread (...) expands a list",
                        1,
                    ));
                }
            },
        }
    }
    Ok(Value::List(items))
}

/// Evaluate a map literal.  Explicit entries are processed first and take
/// priority; spread entries fill in keys not already present.  Duplicate
/// explicit keys emit a diagnostic warning.
fn eval_map(entries: &[ValMapEntry], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let mut pairs: Vec<(String, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    // Explicit entries first so they win over spreads.
    for entry in entries {
        if let ValMapEntry::Entry(key_val, v) = entry {
            let key_value = eval_val(key_val, shell)?;
            let key = match &key_value {
                Value::String(s) => s.clone(),
                _ => {
                    return Err(EvalSignal::Error(
                        Error::new(
                            format!(
                                "map key must be a String, got {} '{key_value}'",
                                key_value.type_name()
                            ),
                            1,
                        )
                        .with_hint("use str to convert"),
                    ));
                }
            };
            if !seen.insert(key.clone()) {
                diagnostic::shell_warning(&format!(
                    "duplicate key '{key}' (line {})",
                    shell.location.line
                ));
            }
            pairs.push((key, eval_val(v, shell)?));
        }
    }
    // Spreads fill in keys not already present.
    for entry in entries {
        if let ValMapEntry::Spread(v) = entry {
            match eval_val(v, shell)? {
                Value::Map(inner) => {
                    for (k, v) in inner {
                        if seen.insert(k.clone()) {
                            pairs.push((k, v));
                        }
                    }
                }
                val => {
                    return Err(shell.err_hint(
                        format!("spread requires a Map, got {}", val.type_name()),
                        "spread (...) in a map expands key-value pairs",
                        1,
                    ));
                }
            }
        }
    }
    Ok(Value::Map(pairs))
}

// ── Force and block execution ────────────────────────────────────────────

/// Force a thunk: evaluate its body in a child environment derived from
/// the captured snapshot.  Non-lambda thunks get a fresh scope via
/// `eval_block_body`; lambdas are evaluated directly (they bind their own
/// parameter on application).  Non-thunk values produce an error.
fn force(val: Value, shell: &mut Shell) -> Result<Value, EvalSignal> {
    match &val {
        Value::Thunk { body, captured, .. } => with_tail(shell, false, |shell| {
            shell.with_child(captured, |child| {
                if matches!(body.as_ref().kind, CompKind::Lam { .. }) {
                    eval_comp(body, child)
                } else {
                    eval_block_body(body, child)
                }
            })
        }),
        other => Err(shell.err_hint(
            format!("cannot force {}: ! requires a Block", other.type_name()),
            "wrap in a block: !{ expr }",
            1,
        )),
    }
}

/// Run `f` inside a fresh scope, popping it on return.
pub(crate) fn with_scope<T>(shell: &mut Shell, f: impl FnOnce(&mut Shell) -> T) -> T {
    shell.push_scope();
    let r = f(shell);
    shell.pop_scope();
    r
}

/// Evaluate a block body inside a fresh scope.
pub fn eval_block_body(body: &Comp, shell: &mut Shell) -> Result<Value, EvalSignal> {
    with_scope(shell, |shell| eval_comp(body, shell))
}

// ── Public API ───────────────────────────────────────────────────────────

/// Public entry point for calling a value as a function.
/// Delegates to the trampoline with the given arguments.
pub fn call_value_pub(val: &Value, args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    trampoline(val.clone(), args.to_vec(), shell)
}

/// §16.4 `_try-apply f val`: apply `f` catching only a parameter pattern-mismatch.
/// Returns `Ok(Some(v))` on success, `Ok(None)` on mismatch, `Err` otherwise.
pub fn try_apply(f: &Value, val: &Value, shell: &mut Shell) -> Result<Option<Value>, EvalSignal> {
    let Value::Thunk { body, captured, .. } = f else {
        return Err(EvalSignal::Error(Error::new(
            format!("_try-apply: expected a block, got {}", f.type_name()),
            1,
        )));
    };
    let CompKind::Lam {
        param: pat,
        body: lam_body,
    } = &body.as_ref().kind
    else {
        return trampoline(f.clone(), vec![val.clone()], shell).map(Some);
    };
    let outcome: Result<Option<Value>, EvalSignal> = shell.with_child(captured, |child| {
        with_scope(child, |child| {
            child.control.in_tail_position = false;
            match assign_pattern(pat, val, child) {
                Err(EvalSignal::Error(e)) if e.kind == ErrorKind::PatternMismatch => Ok(None),
                Err(e) => Err(e),
                Ok(()) => eval_comp(lam_body, child).map(Some),
            }
        })
    });
    match outcome {
        Ok(opt) => Ok(opt),
        Err(EvalSignal::TailCall { callee, args }) => trampoline(callee, args, shell).map(Some),
        Err(e) => Err(e),
    }
}
