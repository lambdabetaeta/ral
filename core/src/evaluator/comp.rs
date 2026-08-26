//! Computation-layer evaluator for the CBPV IR: [`eval_comp`] dispatches on
//! `CompKind`, one helper per operational rule.
//!
//! Tail position is granted, never ambient: an eliminator forwards `tail` to
//! its one final sub-computation and hands [`Tail::No`] to the rest. So a
//! `TailCall` escapes through [`Raw`], and an absorption point above
//! ([`super::absorb_tail`]) must land it before a settled caller sees it.

use crate::ir::{Comp, CompKind, IrPattern, PipeYield, Val};
use crate::source::Spanned;
use crate::types::{Binding, Break, Closure, Control, Error, Mooring, Raw, Shell, Tail, Value};
use std::sync::Arc;

use super::pattern;
use super::val::{close, interpolate_piece};
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
        // as `Val::Thunk(Lam)`, and a curried inner `Lam` is closed directly
        // in the trampoline rather than routed through this arm.
        CompKind::Lam { .. } => Err(shell
            .err_hint(
                "tried to evaluate a bare lambda in computation position — a function value must be \
                 applied to arguments or held as a thunk",
                "either call this function with arguments, or bind it as `Thunk(...)` to pass it as a value",
                1,
            )
            .into()),

        CompKind::Rec { group, index } => eval_rec(group, *index, tail, mooring, shell),

        CompKind::Observe(reg) => super::observe::observe(reg, shell).map_err(Into::into),

        CompKind::Force(val) => step_force(val, mooring, shell),

        CompKind::Interpolation(parts) => eval_interpolation(parts, shell),

        CompKind::Binary(op, lhs, rhs) => {
            expr::eval_binary(*op, lhs, rhs, &shell.env).map_err(Into::into)
        }
        CompKind::Negate(val) => expr::eval_negate(val, &shell.env).map_err(Into::into),
        CompKind::Not(val) => expr::eval_not(val, &shell.env).map_err(Into::into),

        CompKind::Index { target, keys, .. } => eval_index(target, keys, shell),

        CompKind::Bind {
            comp: m,
            pattern,
            rest,
        } => eval_bind(m, pattern, rest, tail, mooring, shell),

        CompKind::Capture(body) => eval_capture(body, mooring, shell),

        CompKind::Decode(body) => eval_decode(body, mooring, shell),

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

        // `invoke` is the ordinary call evaluator, for pipeline stages, bare
        // calls, and redirects alike: it runs the body inside the fd frame
        // and absorbs its tail call there, so an escaped tail callee would
        // land after restoration, writing to the parent instead of the target.
        CompKind::App { .. } | CompKind::Exec(_) | CompKind::Redirect { .. } => {
            call::invoke(comp, tail, mooring, shell)
        }
        // These brackets apply their body thunk through the trampoline,
        // which absorbs the body's tail call inside the frame, so they
        // have no tail position to grant.
        CompKind::Within { opts, body } => scope::eval_within(opts, body, mooring, shell),
        CompKind::Grant { caps, body } => scope::eval_grant(caps, body, mooring, shell),
        CompKind::Try { body, handler } => scope::eval_try(body, handler, mooring, shell),
        CompKind::Guard { body, cleanup } => scope::eval_guard(body, cleanup, mooring, shell),
        CompKind::Audit { body } => scope::eval_audit(body, mooring, shell),
        // A temporary is dead once `body` is, so the frame pops here.
        // Tail position passes through: a tail callee carries its own
        // captured environment and never reads these slots.
        CompKind::Hoisted { body } => {
            with_scope(shell, |shell| eval_comp(body, mooring, shell, tail))
        }

        CompKind::Source { path, rest } => eval_source(path, rest, comp.span, tail, mooring, shell),
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
    let v = close(val, &shell.env)?;
    set_status_from_value(&v, shell);
    Ok(v)
}

/// A Bool result becomes `last_status` (true → 0), mirroring POSIX
/// predicates like `test`; any other value leaves the status untouched.
/// `pub(crate)` so `run_phrases` (`evaluator.rs`) can apply the same rule to
/// a `Define`'s RHS.
pub(crate) fn set_status_from_value(v: &Value, shell: &mut Shell) {
    if let Value::Bool(b) = v {
        shell.set_status_from_bool(*b);
    }
}

/// A fresh `Rec` node re-entering the group at member `j` — what a
/// recursive reference forces, closed under whatever `E` is live when it
/// runs, so the unfolding re-extends from there each time.
fn rec_node(group: &Arc<[(String, Arc<Comp>)]>, index: usize) -> Arc<Comp> {
    Arc::new(Spanned::synthetic(CompKind::Rec {
        group: group.clone(),
        index,
    }))
}

/// Unfold a recursive group: bind every member's name to the thunk of its
/// own projection, then run the chosen member. `rec f. M` is the group of
/// one; a sibling reference forces its own name, which re-enters here and
/// re-installs the whole group — no knot is tied by backpatching.
fn eval_rec(
    group: &Arc<[(String, Arc<Comp>)]>,
    index: usize,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let snap = shell.env.clone();
    with_scope(shell, |shell| -> Raw<Value> {
        for (j, (name, _)) in group.iter().enumerate() {
            shell.install_scope_binding(
                name.clone(),
                Binding {
                    value: Value::Thunk(Closure {
                        comp: rec_node(group, j),
                        env: snap.clone(),
                    }),
                    scheme: None,
                },
            );
        }
        let member = &group[index].1;
        // A literal `Lam` is already a value — closing it here avoids
        // `eval_comp`'s bare-lambda rejection. Anything else, `Rec` included
        // (a member is never itself a further `Rec` in practice, but the
        // test mirrors `close_lam`'s exactly), runs: `Comp::arrow` is not the
        // right test here — it sees *through* `Rec`, and §2.5 requires every
        // force of a `Rec` thunk to re-enter this unfold, not skip it.
        match member.item {
            crate::ir::CompKind::Lam { .. } => Ok(Value::Thunk(Closure {
                comp: Arc::clone(member),
                env: shell.env.clone(),
            })),
            _ => eval_comp(member, mooring, shell, tail),
        }
    })
}

/// `force V` — `force(thunk M) = M`: a `Thunk` whose comp is a literal `Lam`
/// is already a value, returned unchanged for the trampoline to apply once a
/// call site supplies arguments; otherwise it runs through the evaluator
/// (S10). Not `Comp::arrow`: a `Rec` thunk must re-enter `eval_rec` on every
/// force (§2.5), which `Comp::arrow`'s see-through-`Rec` reading would skip.
pub(crate) fn step_force(val: &Val, mooring: &Mooring, shell: &mut Shell) -> Raw<Value> {
    let v = close(val, &shell.env)?;
    let result = match v {
        // Not a tail position: the caller threads this value onward.
        Value::Thunk(closure) if !matches!(closure.comp.item, crate::ir::CompKind::Lam { .. }) => {
            super::eval_block(&closure.comp, &closure.env, Tail::No, mooring, shell)?
        }
        lam @ Value::Thunk(_) => lam,
        // An arity-0 native is a thunk, run like a `Block`; any other native
        // returns unchanged, like a `Lambda`.
        Value::Native { entry, applied } if entry.fixed_arity() == 0 => {
            let env = shell.env.clone();
            super::audit::run_native(&entry, &applied, &env, mooring, shell).map_err(Control::from)?
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
            s.push_str(&interpolate_piece(&close(p, &shell.env)?)?);
            Ok::<_, Error>(s)
        })
        .map(Value::String)
        .map_err(Into::into)
}

/// `V[k1][k2]…` — computation-typed only because indexing can fail (key
/// not found, out of bounds); target and keys are pure values.
fn eval_index(target: &Val, keys: &[Spanned<Val>], shell: &mut Shell) -> Raw<Value> {
    let mut v = close(target, &shell.env)?;
    for key in keys {
        let k = close(&key.item, &shell.env)?;
        v = expr::index_value(&v, &k)?;
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
///
/// "Exactly and entirely" is kept by refusal, not by an unbounded buffer: a
/// body that outruns the 16 MiB cap fails here rather than binding the prefix
/// the buffer kept. A truncated capture would be bytes the program could not
/// tell from the command's own. Being a failure, it takes the road above: the
/// prefix is flushed where it can be seen, and only then does the error go up.
fn eval_capture(body: &Arc<Comp>, mooring: &Mooring, shell: &mut Shell) -> Raw<Value> {
    let (result, bytes, overflowed) =
        super::capture::with_capture(shell, |shell| eval_comp(body, mooring, shell, Tail::No));
    if (result.is_err() || overflowed) && !bytes.is_empty() {
        super::capture::with_ambient_stdout(shell, |shell| shell.write_stdout(&bytes))
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
    // The body's own failure first: it is the earlier event, and it explains
    // more than the cap the flood happened to reach on the way.
    result?;
    if overflowed {
        return Err(shell
            .err_hint(
                format!(
                    "capture exceeded {} MiB — the bytes that fit went out to the visible stream \
                     rather than into the value, because a prefix is not what the command wrote",
                    crate::io::SINK_BUFFER_CAP / (1024 * 1024)
                ),
                "did you mean to write this to a file? `cmd > out.txt` keeps every byte",
                1,
            )
            .into());
    }
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
/// `M` runs under the ambient sink (S11): its own bytes are effect, not the
/// bound value, exactly as a discarded statement's are — `a; b` is `a to _.
/// b`, so this is sequencing's one routing rule; a failure names the
/// abandonment of `rest`, as a `Seq`'s non-final part once did.
///
/// The PATH-shadow check is `run_phrases`'s `Define` arm's alone, under
/// `Mode::Session`: a nested `Bind`'s pattern is a local lexical name, never
/// a command name, so it binds unchecked here.  A nested `Bind` never
/// generalises a scheme either — that lives on `Phrase::Define` alone.
fn eval_bind(
    m: &Arc<Comp>,
    pattern: &IrPattern,
    rest: &Arc<Comp>,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    crate::process::check(mooring)?;
    let step = super::capture::with_ambient_stdout(shell, |shell| {
        eval_comp(m, mooring, shell, Tail::No)
    });
    let val = step.map_err(note_abandoned_steps)?;
    set_status_from_value(&val, shell);
    let env = pattern::bind_pattern(pattern, &val, &[], shell.env.clone(), mooring, shell)?;
    shell.env = env;
    eval_comp(rest, mooring, shell, tail)
}

/// `source path` in a block: run the file's phrases in [`super::Mode::Local`]
/// — a block-local `source` leases nothing (§3.2) — then continue with
/// `rest` under the file's `Define`s, already installed into
/// `shell.env`: today's install-into-ambient-scope made this
/// structural.  The file's own halt halts the block, after its `Define`s
/// before the halt are threaded (S12).
fn eval_source(
    path: &Arc<Comp>,
    rest: &Arc<Comp>,
    span: Option<crate::source::Span>,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let path_val = eval_comp(path, mooring, shell, Tail::No)?;
    let Value::String(p) = path_val else {
        return Err(shell
            .err_hint(
                format!("source: expected String, got {}", path_val.type_name()),
                "the path to `source` must be a computation of type F String",
                1,
            )
            .into());
    };
    let env = shell.env.clone();
    let ran = crate::builtins::modules::source(&p, env, super::Mode::Local, span, mooring, shell)
        .map_err(Control::Break)?;
    shell.env = ran.env;
    match ran.outcome {
        Ok(_) => eval_comp(rest, mooring, shell, tail),
        Err(e) => Err(Control::Break(e)),
    }
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
    let env = shell.env.clone();
    pipeline::run_pipeline(stages, yields, &env, mooring, shell)
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
                shell.last_status = e.exit_code();
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
    let cond_val = close(cond, &shell.env)?;
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

/// A failing part abandons the parts after it, and nothing downstream can tell
/// that from a sequence that simply had fewer parts: `audit`'s `children` just
/// stops short. Name the abandonment on the error, on the innermost sequence
/// that suffered it — an outer one leaves the inner hint standing. No count
/// (S11): once a hoisted temporary is a step of the chain too, the count
/// would name binds rather than statements.
pub(crate) fn note_abandoned_steps(control: Control) -> Control {
    match control {
        Control::Break(Break::Error(e)) if e.hint.is_none() => {
            Control::Break(Break::Error(e.with_hint(
                "later steps in this block did not run; wrap a step in `attempt` if its \
                 failure should not stop the rest",
            )))
        }
        other => other,
    }
}

// ── Scope bracket ────────────────────────────────────────────────────────

/// Runs `f` inside a fresh scope, popping it on return. Used by the
/// selected `if` branch, each `?`-chain arm, the letrec fixpoint frame, and
/// the thread workers in `builtins::concurrency`.
pub(crate) fn with_scope<T>(shell: &mut Shell, f: impl FnOnce(&mut Shell) -> T) -> T {
    let saved = shell.env.clone();
    let r = f(shell);
    shell.env = saved;
    r
}
