//! Computation-layer evaluator for the CBPV IR.
//!
//! [`eval_comp`] dispatches on the `CompKind` of a computation;
//! the per-rule helpers below implement each operational arm.
//! [`with_scope`] brackets a body in a fresh lexical scope. Its
//! callers are the selected `if`/`else` branch, the lambda call frame
//! ([`apply_lambda_frame`](super::trampoline)), and the spawned/watch
//! workers (`builtins::concurrency`).
//!
//! Tail-call signals escape via [`Raw<Value>`]; the public surface in
//! the module root absorbs them through the trampoline before they
//! reach user-visible callers.

use crate::io::strip_trailing_newline;
use crate::ir::*;
use crate::source::Spanned;
use crate::types::*;
use std::sync::Arc;

use super::pattern::assign_pattern;
use super::val::{eval_val, interpolate_piece};
use super::{call, case, expr, scope};
use crate::runtime::pipeline;

// ── eval_comp ────────────────────────────────────────────────────────────

/// Evaluates a computation term of the CBPV IR.
///
/// Each `CompKind` variant is reduced by its operational rule:
/// `Return` produces a value, `Force` eliminates a thunk, `App` and
/// `Exec` enter call dispatch, `Bind` sequences a computation with a
/// continuation under a value binding, and so on. Source-position
/// tracking is updated from the node's span before dispatch so
/// errors carry location information.
///
/// `tail` is the tail position of this computation — whether it sits
/// under a trivial continuation. It is *granted*, never ambient: an
/// eliminator forwards `tail` to a single final sub-computation (the
/// selected `if`/`case` arm, the last chain arm, the final pipeline
/// stage, a bind's continuation) and passes [`Tail::No`] to every
/// other. Only the application and `case` eliminators consume
/// [`Tail::Yes`], emitting a [`TailCall`] for the trampoline.
///
/// A [`TailCall`] may therefore escape via the [`Raw`] return type;
/// the caller (or [`super::evaluate`]) must land it through the
/// trampoline. The `pub(crate)` visibility matches `Raw`'s privacy.
pub(crate) fn eval_comp(comp: &Arc<Comp>, shell: &mut Shell, tail: Tail) -> Raw<Value> {
    // Update source position from the node's span.
    if let Some(span) = comp.span {
        if let Some(src) = shell.turn.loc.source.as_ref() {
            let (l, c) = src.byte_to_line_col(span.start as usize);
            shell.turn.loc.line = l;
            shell.turn.loc.col = c;
        } else {
            shell.turn.loc.line = span.start as usize;
            shell.turn.loc.col = 0;
        }
    }

    match &comp.item {
        CompKind::Return(val) => eval_return(val, shell),

        // CBPV: a bare `Lam` in computation position is the
        // function-typed computation `λx. M`, which is stuck — no
        // operational step applies until it appears as the head of
        // an application. Well-typed code never reaches this arm:
        // closures are born via `eval_val(Val::Thunk(Lam))` and
        // consumed through `App` / `Force` dispatch. Reaching it
        // indicates that the typechecker missed a function-typed
        // thunk in non-application position; we surface a clean
        // dynamic error so the command fails without crashing the
        // REPL.
        CompKind::Lam { .. } => Err(shell
            .err_hint(
                "tried to evaluate a bare lambda in computation position — a function value must be \
                 applied to arguments or held as a thunk",
                "either call this function with arguments, or bind it as `Thunk(...)` to pass it as a value",
                1,
            )
            .into()),

        CompKind::LetRec { slot, bindings } => eval_letrec(*slot, bindings, shell),

        CompKind::Force(val) => step_force(val, shell),

        CompKind::Interpolation(parts) => eval_interpolation(parts, shell),

        CompKind::Binary(op, lhs, rhs) => expr::eval_binary(*op, lhs, rhs, shell).map_err(Into::into),
        CompKind::Not(val) => expr::eval_not(val, shell).map_err(Into::into),

        CompKind::Index { target, keys, .. } => eval_index(target, keys, shell),

        CompKind::Bind {
            comp: m,
            pattern,
            rest,
            scheme,
            rhs_output,
        } => eval_bind(m, pattern, rest, scheme.as_deref(), *rhs_output, tail, shell),

        // `App` and `Exec` dispatch through `invoke` — the same call
        // evaluator that pipelines use, here applied at the empty
        // upstream.  The call sits in this computation's tail position,
        // so `tail` flows through to the application eliminator.
        CompKind::App { .. } | CompKind::Exec(_) => call::invoke(comp, None, tail, shell),

        CompKind::Pipeline { stages, wires } => eval_pipeline(stages, wires, tail, shell),

        CompKind::Chain(parts) => eval_chain(parts, tail, shell),

        CompKind::If { cond, then, else_ } => eval_if(&cond.item, then, else_, tail, shell),

        CompKind::Case { scrutinee, table } => {
            case::eval_case(&scrutinee.item, &table.item, tail, shell)
        }

        CompKind::Scope(op) => match op {
            // `Redirect` installs fds around `body` and absorbs any
            // tail call escaping the frame; it is dispatched through
            // `invoke` so it shares the trampoline-absorption shape.
            // The body sits in the redirect's tail position.
            ScopeOp::Redirect { .. } => call::invoke(comp, None, tail, shell),
            // The scope brackets apply their body thunk through the
            // trampoline (`apply`), which absorbs any terminal tail
            // call inside the `with_*` frame, so the body's tail-ness
            // never escapes the bracket — these arms grant nothing.
            ScopeOp::Within { opts, body } => scope::eval_within(opts, body, shell),
            ScopeOp::Grant { caps, body } => scope::eval_grant(caps, body, shell),
            ScopeOp::Try { body, handler } => scope::eval_try(body, handler, shell),
            ScopeOp::Guard { body, cleanup } => scope::eval_guard(body, cleanup, shell),
            ScopeOp::Audit { body } => scope::eval_audit(body, shell),
        },

        CompKind::Seq(comps) => eval_seq(comps, tail, shell),
    }
}

// ── Per-rule helpers ─────────────────────────────────────────────────────

/// `return V` — produce the value of `V`.
///
/// A Bool return updates `last_status` (true → 0, false → 1),
/// mirroring POSIX predicates like `test` that encode their result
/// in the exit code. Non-Bool returns leave `last_status` untouched
/// so it keeps reflecting the last command with status, exactly as
/// `x=42` or `echo hi` would in a shell.
fn eval_return(val: &Val, shell: &mut Shell) -> Raw<Value> {
    let v = eval_val(val, shell)?;
    if let Value::Bool(b) = v {
        shell.set_status_from_bool(b);
    }
    Ok(v)
}

/// `letrec { x1 = λ…; …; xn = λ… } [in slot]` — simultaneous fixed
/// point for mutually recursive functions.
///
/// Each binding is first installed as a self-referential thunk that
/// re-enters `LetRec` with its own slot index; this is the CBPV
/// fixpoint encoding, since mutual recursion requires the fixpoint
/// to close over thunked references to its siblings. With
/// `slot = None`, every RHS is resolved in the recursive
/// environment, the temporary scope is popped, the resulting
/// lambdas are reinstalled in the caller's scope, and `Unit` is
/// returned. With `slot = Some(i)`, only binding `i` is resolved
/// and its lambda returned — the per-slot re-entry that lets one
/// sibling call another.
fn eval_letrec(
    slot: Option<usize>,
    bindings: &Arc<Vec<(String, Val)>>,
    shell: &mut Shell,
) -> Raw<Value> {
    let snap = shell.snapshot();
    shell.mobile.scope.push_scope();
    // Fixpoint encoding: install each binding as a self-referential
    // thunk.
    for (i, (name, _rhs)) in bindings.iter().enumerate() {
        shell.mobile.scope.set(
            name.clone(),
            Value::Block {
                body: Arc::new(Spanned::synthetic(CompKind::LetRec {
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
                .map(|(_, lam)| eval_val(lam, shell))
                .collect::<Result<Vec<_>, _>>()?;
            shell.mobile.scope.pop_scope();
            for ((name, _), lambda) in bindings.iter().zip(lambdas) {
                shell.mobile.scope.set(name.clone(), lambda);
            }
            Ok(Value::Unit)
        }
        Some(i) => {
            let lambda = eval_val(&bindings[i].1, shell)?;
            shell.mobile.scope.pop_scope();
            Ok(lambda)
        }
    }
}

/// `force V` — eliminate a thunk.
///
/// A [`Value::Block`] is run through the evaluator; a
/// [`Value::Lambda`] is itself a value (the function), so it is
/// returned unchanged — the trampoline applies it when the call site
/// supplies arguments. Other runtime types are a type error.
fn step_force(val: &Val, shell: &mut Shell) -> Raw<Value> {
    let v = eval_val(val, shell)?;
    let result = match v {
        // `!{ … }` eliminates a thunk to its value, which the caller
        // threads onward — a non-trivial continuation, so `Tail::No`.
        Value::Block { body, captured } => super::eval_block(&body, captured, Tail::No, shell)?,
        lam @ Value::Lambda { .. } => lam,
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
    if let Value::Bool(b) = &result {
        shell.set_status_from_bool(*b);
    }
    Ok(result)
}

/// String interpolation — fold each piece into a single string.
///
/// Effectful only because variable lookups inside pieces can fail;
/// the join itself is pure concatenation via [`interpolate_piece`].
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

/// `V[k1][k2]…` — eliminate a collection value at a sequence of
/// keys.
///
/// Computation-typed only because indexing can fail (key not found,
/// out of bounds); the target and keys are themselves pure values.
fn eval_index(target: &Val, keys: &[crate::source::Spanned<Val>], shell: &mut Shell) -> Raw<Value> {
    let mut v = eval_val(target, shell)?;
    for key in keys {
        v = expr::index_value(&v, &eval_val(&key.item, shell)?, shell)?;
    }
    Ok(v)
}

/// Right-hand side of a `Bind`: evaluated under a non-trivial
/// continuation ([`Tail::No`]), since its value is bound and the
/// continuation `rest` runs after it.  Captures stdout when the RHS is
/// byte-mode so the captured bytes become the bound value (SPEC §4.3).
/// The byte-mode bit is the checker's ground output mode, written onto
/// the node by the annotation pass.
fn eval_bind_rhs(
    m: &Arc<Comp>,
    rhs_output: crate::mode::ByteMode,
    shell: &mut Shell,
) -> Raw<Value> {
    if rhs_output != crate::mode::ByteMode::Bytes {
        return eval_comp(m, shell, Tail::No);
    }
    let (result, mut bytes) =
        super::capture::with_capture(shell, |shell| eval_comp(m, shell, Tail::No));
    match result? {
        Value::Unit => {
            strip_trailing_newline(&mut bytes);
            let s = crate::builtins::util::decode_utf8_strict(
                bytes,
                "`(let)` returned bytes that are not valid UTF-8",
                "bind with `| from-bytes` to keep raw output",
            )?;
            Ok(Value::String(s))
        }
        other => Ok(other),
    }
}

/// `M to x. N` — run `M`, destructure its result against `pattern`,
/// then continue with `rest`.
///
/// CBPV Bind sequences a computation with a continuation under a
/// value binding. A Bool result also updates `last_status` so
/// subsequent shell-level status checks see the predicate outcome.
/// The RHS itself is evaluated by [`eval_bind_rhs`], which handles
/// byte-mode capture per SPEC §4.3.  The node's scheme — the checker's
/// verdict for this bind — installs together with the value, so the
/// next turn's check is seeded from the live binding.
///
/// The bound computation `m` runs under a non-trivial continuation;
/// the continuation `rest` inherits the bind's own tail position, so a
/// tail call in `rest` is the whole expression's tail call.
fn eval_bind(
    m: &Arc<Comp>,
    pattern: &IrPattern,
    rest: &Arc<Comp>,
    scheme: Option<&crate::typecheck::Scheme>,
    rhs_output: crate::mode::ByteMode,
    tail: Tail,
    shell: &mut Shell,
) -> Raw<Value> {
    crate::process::check(shell)?;
    let val = eval_bind_rhs(m, rhs_output, shell)?;
    if let Value::Bool(b) = val {
        shell.set_status_from_bool(b);
    }
    assign_pattern(pattern, &val, scheme, shell)?;
    eval_comp(rest, shell, tail)
}

/// Pipeline — concurrent stages connected by Unix pipes.
///
/// A single-stage pipeline reduces to its inner computation, inheriting
/// the pipeline's tail position; the multi-stage case delegates to
/// [`pipeline::run_pipeline`], which grants the pipeline's tail-ness to
/// the final stage alone (every earlier stage's value crosses a value
/// edge, so its tail call must not discard downstream stages).
fn eval_pipeline(
    stages: &[Arc<Comp>],
    wires: &[crate::mode::Wire],
    tail: Tail,
    shell: &mut Shell,
) -> Raw<Value> {
    if stages.len() == 1 {
        return eval_comp(&stages[0], shell, tail);
    }
    pipeline::run_pipeline(stages, wires, tail, shell)
}

/// `a ? b ? c` — fallback chain.
///
/// Each arm is tried in order; the first success is returned,
/// otherwise the last error, or `Unit` if the chain is empty. A
/// failed arm sets `last_status` to its exit code, so subsequent
/// checks see the most recent failure.
///
/// Only the final arm inherits the chain's tail position. A non-final
/// arm runs under a non-trivial continuation ([`Tail::No`]): the chain
/// must still observe its failure to run the next fallback, so its
/// tail call must remain a catchable application rather than a
/// [`TailCall`] that escapes the chain.
fn eval_chain(parts: &[Arc<Comp>], tail: Tail, shell: &mut Shell) -> Raw<Value> {
    let mut last_err: Option<Error> = None;
    let last = parts.len().saturating_sub(1);
    for (i, part) in parts.iter().enumerate() {
        crate::process::check(shell)?;
        let arm_tail = if i == last { tail } else { Tail::No };
        match eval_comp(part, shell, arm_tail) {
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

/// `if V then M else N` — conditional on `V : Bool`.
///
/// CBPV Bind has scope-local binding semantics; ral's runtime keeps
/// Bind-bound names in the surrounding scope so that REPL turns
/// persist their `let`s. At `if` branches that is the wrong default:
/// a `let` inside a branch should not leak. Pushing a fresh scope
/// around the branch restores the CBPV reading without disturbing
/// top-level Bind persistence. The selected branch inherits the
/// `if`'s tail position — the unselected branch is never entered.
fn eval_if(
    cond: &Val,
    then: &Arc<Comp>,
    else_: &Arc<Comp>,
    tail: Tail,
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
    with_scope(shell, |shell| eval_comp(branch, shell, tail))
}

/// Sequence of computations — the last value is the result.
///
/// Only the final element inherits the sequence's tail position, so
/// non-final computations run under a non-trivial continuation and
/// never observe themselves as tail-called. SPEC §4.3: non-final bytes
/// are flushed to any active outer capture sink, so side-effects remain
/// visible to a watching `with_capture` even if a later step fails.
fn eval_seq(comps: &[Arc<Comp>], tail: Tail, shell: &mut Shell) -> Raw<Value> {
    let mut result = Value::Unit;
    let len = comps.len();
    for (i, c) in comps.iter().enumerate() {
        crate::process::check(shell)?;
        let last = i == len - 1;
        let elem_tail = if last { tail } else { Tail::No };
        result = eval_comp(c, shell, elem_tail)?;
        // SPEC §4.3: flush non-final bytes so side-effects remain
        // visible to any outer capture sink.
        if !last && let Some(outer) = &shell.turn.io.capture_outer {
            shell
                .turn
                .io
                .stdout
                .flush_to(outer)
                .map_err(|e| shell.err(format!("seq flush: {e}"), 1))?;
        }
    }
    Ok(result)
}

// ── Scope bracket ────────────────────────────────────────────────────────

/// Runs `f` inside a fresh scope, popping it on return. Called by the
/// selected `if`/`else` branch, the lambda call frame, and the
/// spawned/watch workers.
pub(crate) fn with_scope<T>(shell: &mut Shell, f: impl FnOnce(&mut Shell) -> T) -> T {
    shell.mobile.scope.push_scope();
    let r = f(shell);
    shell.mobile.scope.pop_scope();
    r
}
