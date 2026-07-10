//! Tail-call trampoline. [`apply`] applies a callee to arguments and
//! loops on `Control::Tail`, so every tail-call signal emitted by the
//! evaluator body is absorbed here before control returns. Every
//! public entry into evaluation (`evaluate`, [`apply`]) hands back a
//! [`Settled<Value>`] — [`Tail`] cannot escape this module.

use super::comp::eval_comp;
use super::pattern::assign_pattern;
use crate::ir::IrPattern;
use crate::types::{Env, Value, Shell, Raw, ThunkBody, Settled, Break, Error, Tail, Control, TailCall};

/// One lambda call frame: evaluate the body *in place* on the caller's
/// shell via [`Shell::with_thunk_body`] — the body shares the caller's
/// turn, session, and local state by identity, swapping in only a mobile
/// rescoped to the lambda's `captured` environment plus a fresh frame.
/// `pat` is bound to `arg` in that frame (pattern binding lives in this
/// evaluator layer, not on `Shell`), then `body` runs. As a
/// [`ThunkBody::Lambda`] the frame enters with a fresh `$?` and folds
/// `{last_status, cwd}` back. The single call site supplies one `body`
/// closure that closes a curried inner `Lam` into a fresh
/// `Value::Lambda` via [`close_lam`](super::val::close_lam), otherwise
/// evaluates the body via [`eval_comp`].
///
/// `is_last` is the lambda body's tail position — true when this is the
/// final argument of the call, so the body's final computation sits
/// under the trivial continuation that returns from the call. The
/// trampoline is the sole mint point for [`Tail::Yes`] besides an
/// eliminator forwarding its own tail-ness; `body` consults `is_last`
/// to decide what tail to grant its computation. A tail call escapes as
/// `Err(Control::Tail)` past the in-place run (the fold-back has already
/// happened) and lands in the trampoline loop below.
fn apply_lambda_frame(
    captured: &Env,
    pat: &IrPattern,
    arg: &Value,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> Raw<Value>,
) -> Raw<Value> {
    shell.with_thunk_body(ThunkBody::Lambda, captured, |shell, mobile| {
        shell.run_with_mobile(mobile, |shell| {
            assign_pattern(pat, arg, None, shell)?;
            body(shell)
        })
    })
}

/// Applies `callee` to `args`, looping on [`Tail`] for O(1)
/// tail-call space.
///
/// A recursion cap (`shell.mobile.control.recursion_limit`) raises a
/// clean error before the host stack can overflow; tail calls land
/// in the loop without entering a new frame, so they do not count
/// against the cap. The cap defaults to `DEFAULT_RECURSION_LIMIT`
/// and is overridden by the rc `recursion_limit:` key or the
/// `--recursion-limit` flag.
pub(crate) fn apply(callee: Value, args: Vec<Value>, shell: &mut Shell) -> Settled<Value> {
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
    let result = apply_inner(callee, args, shell);
    shell.mobile.control.call_depth -= 1;
    result
}

fn apply_inner(mut callee: Value, mut args: Vec<Value>, shell: &mut Shell) -> Settled<Value> {
    loop {
        match &callee {
            // Lambda — apply one arg per iteration; with no args, the
            // lambda is a value, return it as-is.
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
                // The body sits in tail position exactly when this is
                // the call's final argument: its value is what the call
                // returns, under the trivial continuation the trampoline
                // provides.
                let body_tail = if is_last { Tail::Yes } else { Tail::No };
                // Curried lambdas (`λx.λy.M`) are flattened by the
                // elaborator, so `body` itself can be `Lam`. Closing the
                // inner `Lam` directly avoids `eval_comp`'s `Lam` arm,
                // which would wrongly raise the "bare lambda in
                // computation position" error for a legitimate curried
                // application.
                let result = apply_lambda_frame(&captured, &param, &arg, shell, |child| {
                    match super::val::close_lam(&body, child) {
                        Some(lam) => Ok(lam),
                        None => eval_comp(&body, child, body_tail),
                    }
                });
                if let Some(done) = step(result, &mut callee, &mut args, shell) {
                    return done;
                }
            }
            // Block — force through the block boundary so the
            // install-vs-discard mobile policy matches `force` and
            // `grant` / `within`.
            //
            // The block body sits in tail position exactly when no
            // arguments remain to apply; `eval_block` forwards that
            // grant into the body's mobile so its final call
            // trampolines here rather than recursing on the host stack.
            Value::Block { body, captured } => {
                let block_tail = if args.is_empty() { Tail::Yes } else { Tail::No };
                let body = body.clone();
                let result =
                    super::eval_block(&body, captured, block_tail, shell).map_err(Control::from);
                if let Some(done) = step(result, &mut callee, &mut args, shell) {
                    return done;
                }
            }
            // Non-callable: done if no args, error otherwise.
            _ if args.is_empty() => return Ok(callee),
            _ => {
                let hint = if matches!(callee, Value::Unit) {
                    "too many arguments — the function returned before consuming all of them"
                } else {
                    "only Lambdas and Blocks are functions"
                };
                return Err(Break::Error(
                    Error::new(format!("{} is not a function", callee.type_name()), 1)
                        .with_hint(hint),
                ));
            }
        }
    }
}

/// One trampoline step: `Some(done)` to exit, `None` to iterate.
///
/// `Control::Tail` is absorbed here — the loop continues with the
/// new callee and arguments. Every other [`Control`] variant
/// collapses to a [`Break`] returned to the caller as
/// [`Settled<Value>`].
fn step(
    result: Raw<Value>,
    callee: &mut Value,
    args: &mut Vec<Value>,
    shell: &Shell,
) -> Option<Settled<Value>> {
    match result {
        Ok(v) if args.is_empty() => Some(Ok(v)),
        Ok(v) => {
            *callee = v;
            None
        }
        Err(Control::Tail(TailCall { callee: c, args: a })) => {
            if let Err(b) = crate::process::check(shell) {
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

    /// When `call_depth` has already reached the configured limit,
    /// entering `apply` raises a clean error instead of recursing
    /// further. This is the unit-level analogue of "the host stack
    /// is about to overflow" — exercising it through real recursion
    /// in a debug build would exhaust the OS stack before the
    /// counter trips.
    #[test]
    fn cap_fires_at_limit() {
        let mut shell = Shell::new(TerminalState::default());
        shell.mobile.control.recursion_limit = 8;
        shell.mobile.control.call_depth = 8;
        let result = apply(Value::Unit, vec![Value::Unit], &mut shell);
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
        // call_depth must be unchanged on the early-return path.
        assert_eq!(shell.mobile.control.call_depth, 8);
    }

    /// One apply call below the cap increments and decrements
    /// `call_depth` cleanly, leaving it at its original value.
    #[test]
    fn cap_not_fired_below_limit() {
        let mut shell = Shell::new(TerminalState::default());
        shell.mobile.control.recursion_limit = 8;
        shell.mobile.control.call_depth = 7;
        // Value::Unit with no args returns Ok(Unit) without recursing.
        let _ = apply(Value::Unit, vec![], &mut shell);
        assert_eq!(shell.mobile.control.call_depth, 7);
    }
}
