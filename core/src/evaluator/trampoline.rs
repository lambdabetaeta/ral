//! Tail-call trampoline: [`apply`] applies a callee to arguments and loops
//! on an escaping [`TailCall`] instead of recursing. Every tail call the
//! evaluator emits lands here, directly or through `super::absorb_tail`,
//! and only a `Settled<Value>` leaves.

use super::comp::eval_comp;
use super::pattern::assign_pattern;
use crate::ir::IrPattern;
use crate::types::{
    Break, Control, Env, Error, Mooring, Raw, Settled, Shell, Tail, TailCall, ThunkBody, Value,
};

/// One β-step, run in place on the caller's shell: [`Shell::with_thunk_body`]
/// swaps in a mobile rescoped to `captured` plus a fresh frame, `pat` is bound
/// to `arg` there, and `body` runs — sharing the caller's run, session and
/// local state by identity.
///
/// As a [`ThunkBody::Lambda`] the frame enters with a fresh `$?` and folds
/// `{last_status, cwd}` back on the way out. The fold happens even when `body`
/// escapes with a tail call, which is what lets that call abandon the frame.
fn apply_lambda_frame(
    captured: &Env,
    pat: &IrPattern,
    arg: &Value,
    mooring: &Mooring,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> Raw<Value>,
) -> Raw<Value> {
    shell.with_thunk_body(ThunkBody::Lambda, captured, |shell, mobile| {
        shell.run_with_mobile(mobile, |shell| {
            assign_pattern(pat, arg, None, mooring, shell)?;
            body(shell)
        })
    })
}

/// Applies `callee` to `args`, looping on [`TailCall`] for O(1) tail-call
/// space.
///
/// The `recursion_limit` cap raises a clean error before the host stack can
/// overflow; a tail call re-enters the loop rather than a frame, so it never
/// counts against the cap.
pub(crate) fn apply(
    callee: Value,
    args: Vec<Value>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    if shell.mobile.control.call_depth >= shell.mobile.control.recursion_limit {
        return Err(Break::Error(
            Error::new(
                format!(
                    "recursion limit exceeded ({})",
                    shell.mobile.control.recursion_limit
                ),
                1,
            )
            .with_hint(
                "usually a runaway recursive function — \
                 raise via rc recursion_limit: or --recursion-limit",
            ),
        ));
    }
    shell.mobile.control.call_depth += 1;
    let result = apply_inner(callee, args, mooring, shell);
    shell.mobile.control.call_depth -= 1;
    result
}

fn apply_inner(
    mut callee: Value,
    mut args: Vec<Value>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    loop {
        match &callee {
            Value::Lambda {
                param,
                body,
                captured,
            } => {
                if args.is_empty() {
                    return Ok(callee);
                }
                let param = param.clone();
                let body = body.clone();
                let captured = captured.clone();
                let arg = args.remove(0);
                let is_last = args.is_empty();
                // Only the final argument's body is in tail position: the
                // loop below is the trivial continuation it returns through.
                let body_tail = if is_last { Tail::Yes } else { Tail::No };
                // The elaborator strips the `return thunk(...)` around a
                // curried inner lambda, so `body` may itself be a `Lam`.
                // Closing it here avoids `eval_comp`'s `Lam` arm, which
                // rejects a bare lambda in computation position.
                let result = apply_lambda_frame(&captured, &param, &arg, mooring, shell, |child| {
                    match super::val::close_lam(&body, child) {
                        Some(lam) => Ok(lam),
                        None => eval_comp(&body, mooring, child, body_tail),
                    }
                });
                if let Some(done) = step(result, &mut callee, &mut args, mooring) {
                    return done;
                }
            }
            // Applying a block eliminates it through `eval_block`, so it
            // obeys the same scope-isolated, mobile-discarding contract as
            // `force`.
            Value::Block { body, captured } => {
                let block_tail = if args.is_empty() { Tail::Yes } else { Tail::No };
                let body = body.clone();
                let result = super::eval_block(&body, captured, block_tail, mooring, shell)
                    .map_err(Control::from);
                if let Some(done) = step(result, &mut callee, &mut args, mooring) {
                    return done;
                }
            }
            // The single arity gate: collect until `fixed_arity` is reached,
            // then run the body. Leftover args loop onto the result, so
            // over-application hits the `not a function` arm like a Lambda's.
            // The audit frame sits here so command heads and value-position
            // applications record identically as the entry's name.
            Value::Native { entry, applied } => {
                let entry = entry.clone();
                let needed = entry.fixed_arity().unwrap_or(applied.len());
                let take = needed.saturating_sub(applied.len()).min(args.len());
                let mut collected = applied.clone();
                collected.extend(args.drain(0..take));
                if collected.len() < needed {
                    return Ok(Value::Native {
                        entry,
                        applied: collected,
                    });
                }
                let result = super::audit::frame_call(&entry.name, &collected, shell, |shell| {
                    entry
                        .body
                        .call(&collected, mooring, shell)
                        .map_err(Control::from)
                });
                if let Some(done) = step(result, &mut callee, &mut args, mooring) {
                    return done;
                }
            }
            _ if args.is_empty() => return Ok(callee),
            _ => {
                let hint = if matches!(callee, Value::Unit) {
                    "too many arguments — the function returned before consuming all of them"
                } else {
                    "only Lambdas, Blocks, and natives are functions"
                };
                return Err(Break::Error(
                    Error::new(format!("{} is not a function", callee.type_name()), 1)
                        .with_hint(hint),
                ));
            }
        }
    }
}

/// One trampoline step: `Some(done)` to exit, `None` to iterate with the new
/// callee and arguments.
///
/// The cancel check sits on the tail edge because a tail loop unwinds through
/// no frame boundary — for a body that neither binds nor sequences, this is
/// the only place a cancellation reaches it.
fn step(
    result: Raw<Value>,
    callee: &mut Value,
    args: &mut Vec<Value>,
    mooring: &Mooring,
) -> Option<Settled<Value>> {
    match result {
        Ok(v) if args.is_empty() => Some(Ok(v)),
        Ok(v) => {
            *callee = v;
            None
        }
        Err(Control::Tail(TailCall { callee: c, args: a })) => {
            if let Err(b) = crate::process::check(mooring) {
                return Some(Err(b));
            }
            *callee = c;
            *args = a;
            None
        }
        Err(Control::Break(b)) => Some(Err(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::TerminalState;

    /// Entering `apply` at the limit raises a clean error. The depth is
    /// pre-loaded because real recursion in a debug build would exhaust the
    /// OS stack before the counter tripped.
    #[test]
    fn cap_fires_at_limit() {
        let mut shell = Shell::new(TerminalState::default());
        shell.mobile.control.recursion_limit = 8;
        shell.mobile.control.call_depth = 8;
        let result = apply(
            Value::Unit,
            vec![Value::Unit],
            &Mooring::adrift(),
            &mut shell,
        );
        match result {
            Err(Break::Error(e)) => {
                assert!(
                    e.message.contains("recursion limit"),
                    "expected 'recursion limit' in {:?}",
                    e.message,
                );
            }
            other => panic!("expected recursion-limit error, got {other:?}"),
        }
        // The early return must leave the counter untouched.
        assert_eq!(shell.mobile.control.call_depth, 8);
    }

    /// `Value::Unit` with no arguments returns without recursing, so the
    /// increment/decrement bracket must leave `call_depth` where it started.
    #[test]
    fn cap_not_fired_below_limit() {
        let mut shell = Shell::new(TerminalState::default());
        shell.mobile.control.recursion_limit = 8;
        shell.mobile.control.call_depth = 7;
        let _ = apply(Value::Unit, vec![], &Mooring::adrift(), &mut shell);
        assert_eq!(shell.mobile.control.call_depth, 7);
    }
}
