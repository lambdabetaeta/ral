//! Computation-layer evaluator for the CBPV IR: [`eval_comp`] dispatches on
//! `CompKind`, one helper per operational rule.
//!
//! Tail position is granted, never ambient: an eliminator forwards `tail` to
//! its one final sub-computation and hands [`Tail::No`] to the rest. So a
//! `TailCall` escapes through [`Raw`], and an absorption point above
//! ([`super::absorb_tail`]) must land it before a settled caller sees it.

use crate::ir::{Comp, CompKind, IrPattern, PipeYield, ScopeOp, Val};
use crate::source::Spanned;
use crate::typecheck::Scheme;
use crate::types::{Binding, Break, Control, Error, Mooring, Raw, Shell, Tail, Value};
use std::sync::Arc;

use super::pattern::assign_pattern;
use super::val::{eval_val, interpolate_piece};
use super::{call, case, expr, scope};
use crate::runtime::pipeline;

// ── eval_comp ────────────────────────────────────────────────────────────

/// Evaluates a computation term of the CBPV IR.
///
/// An error unwinding through this node picks up `comp.span` unless it
/// already carries one, so a runtime error names the innermost node it broke
/// on. Of the eliminators, only application consumes [`Tail::Yes`], emitting
/// a `TailCall`; `if` and `case` forward it into the branch they select.
pub(crate) fn eval_comp(
    comp: &Arc<Comp>,
    mooring: &Mooring,
    shell: &mut Shell,
    tail: Tail,
) -> Raw<Value> {
    let result = match &comp.item {
        CompKind::Return(val) => eval_return(val, shell),

        // A bare `Lam` is stuck: no step applies until it heads an
        // application. Well-typed code never lands here — closures are born
        // as `Val::Thunk(Lam)`, and a curried inner `Lam` is closed by
        // `close_lam` in the trampoline rather than routed through this arm.
        CompKind::Lam { .. } => Err(shell
            .err_hint(
                "tried to evaluate a bare lambda in computation position — a function value must be \
                 applied to arguments or held as a thunk",
                "either call this function with arguments, or bind it as `Thunk(...)` to pass it as a value",
                1,
            )
            .into()),

        CompKind::LetRec { slot, bindings } => eval_letrec(*slot, bindings, shell),

        CompKind::Force(val) => step_force(val, mooring, shell),

        CompKind::Interpolation(parts) => eval_interpolation(parts, shell),

        CompKind::Binary(op, lhs, rhs) => expr::eval_binary(*op, lhs, rhs, shell).map_err(Into::into),
        CompKind::Negate(val) => expr::eval_negate(val, shell).map_err(Into::into),
        CompKind::Not(val) => expr::eval_not(val, shell).map_err(Into::into),

        CompKind::Index { target, keys, .. } => eval_index(target, keys, shell),

        CompKind::Bind {
            comp: m,
            pattern,
            rest,
            scheme,
        } => eval_bind(m, pattern, rest, scheme.as_deref(), tail, mooring, shell),

        CompKind::Capture(body) => eval_capture(body, mooring, shell),

        CompKind::Decode(body) => eval_decode(body, mooring, shell),

        // `invoke` is the ordinary call evaluator, for pipeline stages and
        // bare calls alike.
        CompKind::App { .. } | CompKind::Exec(_) => call::invoke(comp, tail, mooring, shell),

        CompKind::Pipeline { stages, yields, .. } => {
            eval_pipeline(stages, *yields, tail, mooring, shell)
        }

        CompKind::Chain(parts) => eval_chain(parts, tail, mooring, shell),

        CompKind::If { cond, then, else_ } => {
            eval_if(&cond.item, then, else_, tail, mooring, shell)
        }

        CompKind::Case { scrutinee, arms } => {
            case::eval_case(&scrutinee.item, arms, tail, mooring, shell)
        }

        CompKind::Scope(op) => match op {
            // `invoke` runs the body inside the fd frame and absorbs its
            // tail call there: a tail callee that escaped would land after
            // restoration, writing to the parent instead of the target.
            ScopeOp::Redirect { .. } => call::invoke(comp, tail, mooring, shell),
            // These brackets apply their body thunk through the trampoline,
            // which absorbs the body's tail call inside the frame, so they
            // have no tail position to grant.
            ScopeOp::Within { opts, body } => scope::eval_within(opts, body, mooring, shell),
            ScopeOp::Grant { caps, body } => scope::eval_grant(caps, body, mooring, shell),
            ScopeOp::Try { body, handler } => scope::eval_try(body, handler, mooring, shell),
            ScopeOp::Guard { body, cleanup } => scope::eval_guard(body, cleanup, mooring, shell),
            ScopeOp::Audit { body } => scope::eval_audit(body, mooring, shell),
        },

        CompKind::Seq(comps) => eval_seq(comps, tail, mooring, shell),
    };

    match (result, comp.span) {
        (Err(Control::Break(Break::Error(e))), Some(span)) if e.span.is_none() => {
            Err(e.at_span(span).into())
        }
        (result, _) => result,
    }
}

// ── Per-rule helpers ─────────────────────────────────────────────────────

fn eval_return(val: &Val, shell: &mut Shell) -> Raw<Value> {
    let v = eval_val(val, shell)?;
    set_status_from_value(&v, shell);
    Ok(v)
}

/// A Bool result becomes `last_status` (true → 0), mirroring POSIX
/// predicates like `test`; any other value leaves the status untouched.
fn set_status_from_value(v: &Value, shell: &mut Shell) {
    if let Value::Bool(b) = v {
        shell.set_status_from_bool(*b);
    }
}

/// Simultaneous fixed point for mutual recursion: each binding is installed
/// as a thunk that re-enters `LetRec` at its own slot, so a sibling call
/// resolves through this same rule. `slot = None` resolves the whole group
/// into the caller's scope; `Some(i)` is that re-entry, yielding one lambda.
fn eval_letrec(
    slot: Option<usize>,
    bindings: &Arc<Vec<(String, Val)>>,
    shell: &mut Shell,
) -> Raw<Value> {
    // Only the group install reaches the caller's scope, where a name could
    // shadow a PATH command; the per-slot re-entry installs nothing there.
    if slot.is_none() {
        for (name, _) in bindings.iter() {
            super::pattern::check_path_shadow(name, shell)?;
        }
    }
    let snap = shell.snapshot();
    let install = |shell: &mut Shell| {
        for (i, (name, _rhs)) in bindings.iter().enumerate() {
            shell.install_scope_binding(
                name.clone(),
                Binding {
                    value: Value::Block {
                        body: Arc::new(Spanned::synthetic(CompKind::LetRec {
                            slot: Some(i),
                            bindings: bindings.clone(),
                        })),
                        captured: snap.clone(),
                    },
                    scheme: None,
                },
            );
        }
    };
    match slot {
        None => {
            let lambdas = with_scope(shell, |shell| {
                install(shell);
                bindings
                    .iter()
                    .map(|(_, lam)| eval_val(lam, shell))
                    .collect::<Result<Vec<_>, _>>()
            })?;
            for ((name, _), lambda) in bindings.iter().zip(lambdas) {
                shell.install_scope_binding(
                    name.clone(),
                    Binding {
                        value: lambda,
                        scheme: None,
                    },
                );
            }
            Ok(Value::Unit)
        }
        Some(i) => with_scope(shell, |shell| -> Raw<Value> {
            install(shell);
            Ok(eval_val(&bindings[i].1, shell)?)
        }),
    }
}

/// `force V` — a [`Value::Block`] runs through the evaluator; a
/// [`Value::Lambda`] is already a value, returned unchanged for the
/// trampoline to apply once a call site supplies arguments.
pub(crate) fn step_force(val: &Val, mooring: &Mooring, shell: &mut Shell) -> Raw<Value> {
    let v = eval_val(val, shell)?;
    let result = match v {
        // Not a tail position: the caller threads this value onward.
        Value::Block { body, captured } => {
            super::eval_block(&body, &captured, Tail::No, mooring, shell)?
        }
        lam @ Value::Lambda { .. } => lam,
        // An arity-0 native is a thunk, run like a `Block`; any other native
        // returns unchanged, like a `Lambda`.
        Value::Native { entry, applied } if entry.fixed_arity() == Some(0) => {
            super::audit::run_native(&entry, &applied, mooring, shell)?
        }
        native @ Value::Native { .. } => native,
        other => {
            return Err(shell
                .err_hint(
                    format!("cannot force {}: ! requires a Block", other.type_name()),
                    "wrap in a block: !{ expr }",
                    1,
                )
                .into());
        }
    };
    set_status_from_value(&result, shell);
    Ok(result)
}

/// Computation-typed only because a variable lookup inside a piece can
/// fail; the join itself is pure concatenation.
fn eval_interpolation(parts: &[Val], shell: &mut Shell) -> Raw<Value> {
    parts
        .iter()
        .try_fold(String::new(), |mut s, p| {
            s.push_str(&interpolate_piece(&eval_val(p, shell)?, shell)?);
            Ok::<_, Error>(s)
        })
        .map(Value::String)
        .map_err(Into::into)
}

/// `V[k1][k2]…` — computation-typed only because indexing can fail (key
/// not found, out of bounds); target and keys are pure values.
fn eval_index(target: &Val, keys: &[Spanned<Val>], shell: &mut Shell) -> Raw<Value> {
    let mut v = eval_val(target, shell)?;
    for key in keys {
        v = expr::index_value(&v, &eval_val(&key.item, shell)?, shell)?;
    }
    Ok(v)
}

/// `capture M` — run `M` with its byte channel captured; the bytes the
/// handler collected *are* the value, exactly and entirely. `M`'s own value is
/// `Unit` by typing (WF-2), so no inspection is needed. Reading those bytes as
/// text is not this node's business: the checker composes a `Decode` over it.
///
/// On failure the payload was never a value, so what `M` already wrote is seen
/// exactly where a discarded statement's bytes are — the nearest visible
/// stream, not the buffer one bracket out. A partial write (`echo half; exit
/// 3`) stays visible however deep the brackets nest.
fn eval_capture(body: &Arc<Comp>, mooring: &Mooring, shell: &mut Shell) -> Raw<Value> {
    let (result, bytes) =
        super::capture::with_capture(shell, |shell| eval_comp(body, mooring, shell, Tail::No));
    if result.is_err() && !bytes.is_empty() {
        super::capture::with_ambient_stdout(shell, |shell| shell.write_stdout(&bytes))
            .and_then(std::convert::identity)
            .map_err(|e| shell.err(format!("capture flush: {e}"), 1))?;
    }
    // Type-ensured, not merely expected: every operation that grounds a route
    // to `Bytes` unifies the paired type with `Unit` (WF-2).  A firing assert
    // means that story has a hole — silent in release, it once meant a
    // captured string standing in for a typed value — so it aborts in every
    // profile, not just debug.
    assert!(
        matches!(&result, Err(_) | Ok(Value::Unit)),
        "capture's operand is result: Bytes, so WF-2 makes its value Unit"
    );
    result?;
    Ok(Value::Bytes(bytes))
}

/// `Decode M` — read the bytes `M` returns as text: one trailing line
/// terminator dropped, because a command's line of output is its value and not
/// its formatting, then a strict UTF-8 decode. The lossy, partial half of the
/// value boundary, which is why it is not folded back into `eval_capture`.
///
/// The checker writes this node over a `Capture` and nowhere else, so its
/// meaning is settled before the program runs: there is no name to resolve and
/// no scope to consult, and a session's aliases, bindings, and handler frames
/// cannot come between captured bytes and the text a `let` binds.
///
/// The buffer is moved out of the operand's value and decoded in place, so the
/// shell's hottest path — `let x = <command>` — copies no bytes here.
fn eval_decode(body: &Arc<Comp>, mooring: &Mooring, shell: &mut Shell) -> Raw<Value> {
    let value = eval_comp(body, mooring, shell, Tail::No)?;
    let Value::Bytes(mut bytes) = value else {
        return Err(shell
            .err_hint(
                format!(
                    "the value boundary was handed {} where a captured byte payload belongs",
                    value.type_name()
                ),
                "only the type checker writes this step, so this is a fault in ral rather than in \
                 your program — please report the source that produced it",
                1,
            )
            .into());
    };
    crate::io::strip_trailing_newline(&mut bytes);
    let text = crate::builtins::util::decode_utf8_strict(
        bytes,
        "captured output is not valid UTF-8",
        "bind with `| from-bytes` to keep raw output",
    )?;
    Ok(Value::String(text))
}

/// `M to x. N` — run `M`, destructure its result against `pattern`, then
/// continue with `rest`, which inherits the bind's own tail position.
///
/// The checker's `scheme` for the node installs together with the value, so
/// the next run's check is seeded from the live binding.
fn eval_bind(
    m: &Arc<Comp>,
    pattern: &IrPattern,
    rest: &Arc<Comp>,
    scheme: Option<&Scheme>,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    crate::process::check(mooring)?;
    super::pattern::check_pattern_shadow(pattern, shell)?;
    let val = eval_comp(m, mooring, shell, Tail::No)?;
    set_status_from_value(&val, shell);
    assign_pattern(pattern, &val, scheme, mooring, shell)?;
    eval_comp(rest, mooring, shell, tail)
}

/// A single-stage pipeline is just its inner computation, and inherits the
/// pipeline's tail position.  A multi-stage pipeline grants that position to
/// no one: every stage runs in a child, and a tail call cannot cross the
/// process boundary.
fn eval_pipeline(
    stages: &[Arc<Comp>],
    yields: PipeYield,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    if stages.len() == 1 {
        return eval_comp(&stages[0], mooring, shell, tail);
    }
    pipeline::run_pipeline(stages, yields, mooring, shell)
}

/// `a ? b ? c` — the first arm to succeed wins, else the last error (`Unit`
/// for an empty chain); a failed arm leaves its exit code in `last_status`.
/// Only the final arm inherits the chain's tail position: the chain must
/// observe a non-final arm's failure to run the next fallback, so that arm's
/// call has to stay catchable rather than escape as a tail call.
fn eval_chain(parts: &[Arc<Comp>], tail: Tail, mooring: &Mooring, shell: &mut Shell) -> Raw<Value> {
    let mut last_err: Option<Error> = None;
    let last = parts.len().saturating_sub(1);
    for (i, part) in parts.iter().enumerate() {
        crate::process::check(mooring)?;
        let arm_tail = if i == last { tail } else { Tail::No };
        match with_scope(shell, |shell| eval_comp(part, mooring, shell, arm_tail)) {
            Ok(result) => return Ok(result),
            Err(Control::Break(Break::Error(e))) => {
                shell.mobile.control.last_status = e.exit_code();
                last_err = Some(e);
            }
            Err(other) => return Err(other),
        }
    }
    last_err.map_or(Ok(Value::Unit), |e| Err(e.into()))
}

/// `if V then M else N`, on `V : Bool`. ral keeps Bind-bound names in the
/// surrounding scope so a REPL run's `let`s persist; inside a branch that is
/// the wrong default, so the selected branch runs in a fresh scope — CBPV's
/// scope-local reading, without disturbing top-level persistence.
fn eval_if(
    cond: &Val,
    then: &Arc<Comp>,
    else_: &Arc<Comp>,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let cond_val = eval_val(cond, shell)?;
    let b = match cond_val {
        Value::Bool(b) => b,
        other => {
            return Err(shell
                .err(
                    format!("if: expected Bool, got {} '{}'", other.type_name(), other),
                    1,
                )
                .into());
        }
    };
    shell.set_status_from_bool(b);
    let branch = if b { then } else { else_ };
    with_scope(shell, |shell| eval_comp(branch, mooring, shell, tail))
}

/// Sequence of computations — the last value is the result, and only it
/// inherits the sequence's tail position.  A non-final part's value is
/// discarded, so its bytes are effect and it runs against the ambient sink:
/// the destination is fixed by the writer's position in its own block, before
/// a byte is written, and never revised.
fn eval_seq(comps: &[Arc<Comp>], tail: Tail, mooring: &Mooring, shell: &mut Shell) -> Raw<Value> {
    let mut result = Value::Unit;
    let len = comps.len();
    for (i, c) in comps.iter().enumerate() {
        crate::process::check(mooring)?;
        result = if i + 1 == len {
            eval_comp(c, mooring, shell, tail)?
        } else {
            let step = super::capture::with_ambient_stdout(shell, |shell| {
                eval_comp(c, mooring, shell, Tail::No)
            })
            .map_err(|e| shell.err(format!("statement sink: {e}"), 1))?;
            step.map_err(|control| note_abandoned_steps(control, len - i - 1))?
        };
    }
    Ok(result)
}

/// A failing part abandons the parts after it, and nothing downstream can tell
/// that from a sequence that simply had fewer parts: `audit`'s `children` just
/// stops short. Name the abandonment on the error, on the innermost sequence
/// that suffered it — an outer one leaves the inner hint standing.
fn note_abandoned_steps(control: Control, abandoned: usize) -> Control {
    match control {
        Control::Break(Break::Error(e)) if e.hint.is_none() => {
            let steps = if abandoned == 1 { "step" } else { "steps" };
            Control::Break(Break::Error(e.with_hint(format!(
                "{abandoned} later {steps} in this block did not run; wrap a step in \
                 `attempt` if its failure should not stop the rest"
            ))))
        }
        other => other,
    }
}

// ── Scope bracket ────────────────────────────────────────────────────────

/// Runs `f` inside a fresh scope, popping it on return. Used by the
/// selected `if` branch, each `?`-chain arm, the letrec fixpoint frame, and
/// the thread workers in `builtins::concurrency`.
pub(crate) fn with_scope<T>(shell: &mut Shell, f: impl FnOnce(&mut Shell) -> T) -> T {
    shell.mobile.scope.push_scope();
    let r = f(shell);
    shell.mobile.scope.pop_scope();
    r
}
