//! Tail-call trampoline: apply a thunk to args, looping on `EvalSignal::TailCall`.

use super::pattern::assign_pattern;
use super::{eval_block_body, eval_comp, with_scope};
use crate::ir::CompKind;
use crate::types::*;

/// Apply `callee` to `args`, looping on `TailCall` for O(1) tail-call space.
///
/// Refuses to recurse past `shell.control.recursion_limit` — the cap fires as a
/// clean error rather than letting the host stack overflow.  Tail
/// calls are landed in this loop without entering a new frame and so
/// don't count toward the cap.  Default is `DEFAULT_RECURSION_LIMIT`;
/// the rc `recursion_limit:` key and the `--recursion-limit` flag
/// override it.
pub fn trampoline(
    callee: Value,
    args: Vec<Value>,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    if shell.control.call_depth >= shell.control.recursion_limit {
        return Err(EvalSignal::Error(
            Error::new(
                format!("recursion limit exceeded ({})", shell.control.recursion_limit),
                1,
            )
            .with_hint(
                "usually a runaway recursive function — \
                 raise via rc recursion_limit: or --recursion-limit",
            ),
        ));
    }
    shell.control.call_depth += 1;
    let result = trampoline_inner(callee, args, shell);
    shell.control.call_depth -= 1;
    result
}

fn trampoline_inner(
    mut callee: Value,
    mut args: Vec<Value>,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    loop {
        match &callee {
            // thunk(λx. M) — apply if args available, return if not.
            Value::Thunk { body, captured, .. }
                if matches!(body.as_ref().kind, CompKind::Lam { .. }) =>
            {
                if args.is_empty() {
                    return Ok(callee);
                }
                let CompKind::Lam {
                    param: pat,
                    body: lam_body,
                } = &body.as_ref().kind
                else {
                    unreachable!()
                };
                let arg = args.remove(0);
                let is_last = args.is_empty();
                let captured = captured.clone();
                let pat = pat.clone();
                let lam_body = lam_body.clone();
                let result = shell.with_child(&captured, |child| {
                    with_scope(child, |child| {
                        child.control.in_tail_position = is_last;
                        assign_pattern(&pat, &arg, child)?;
                        eval_comp(&lam_body, child)
                    })
                });
                if let Some(done) = step(result, &mut callee, &mut args, shell) {
                    return done;
                }
            }
            // thunk(M) where M is not a lambda — force it.
            Value::Thunk { body, captured, .. } => {
                let is_last = args.is_empty();
                let result = shell.with_child(captured, |child| {
                    child.control.in_tail_position = is_last;
                    eval_block_body(body, child)
                });
                if let Some(done) = step(result, &mut callee, &mut args, shell) {
                    return done;
                }
            }
            // Non-thunk: done if no args, error otherwise.
            _ if args.is_empty() => return Ok(callee),
            _ => {
                let hint = if matches!(callee, Value::Unit) {
                    "too many arguments — the function returned before consuming all of them"
                } else {
                    "only Blocks are functions"
                };
                return Err(EvalSignal::Error(
                    Error::new(format!("{} is not a function", callee.type_name()), 1)
                        .with_hint(hint),
                ));
            }
        }
    }
}

/// One trampoline step: `Some(done)` to exit, `None` to iterate.
fn step(
    result: Result<Value, EvalSignal>,
    callee: &mut Value,
    args: &mut Vec<Value>,
    shell: &mut Shell,
) -> Option<Result<Value, EvalSignal>> {
    match result {
        Ok(v) if args.is_empty() => Some(Ok(v)),
        Ok(v) => {
            *callee = v;
            None
        }
        Err(EvalSignal::TailCall { callee: c, args: a }) => {
            if let Err(e) = crate::signal::check(shell) {
                return Some(Err(e));
            }
            *callee = c;
            *args = a;
            None
        }
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When `call_depth` has already reached the configured limit,
    /// entering the trampoline raises a clean error instead of
    /// recursing further.  This is the unit-level equivalent of "the
    /// host stack is about to die" — exercising it through real
    /// recursion in a debug build would exhaust the OS stack before
    /// the counter trips.
    #[test]
    fn cap_fires_at_limit() {
        let mut shell = Shell::new(Default::default());
        shell.control.recursion_limit = 8;
        shell.control.call_depth = 8;
        let result = trampoline(Value::Unit, vec![Value::Unit], &mut shell);
        match result {
            Err(EvalSignal::Error(e)) => {
                assert!(
                    e.message.contains("recursion limit"),
                    "expected 'recursion limit' in {:?}",
                    e.message,
                );
            }
            other => panic!("expected recursion-limit error, got {other:?}"),
        }
        // call_depth must be unchanged on the early-return path.
        assert_eq!(shell.control.call_depth, 8);
    }

    /// One trampoline call below the cap increments and decrements
    /// `call_depth` cleanly, leaving it at its original value.
    #[test]
    fn cap_not_fired_below_limit() {
        let mut shell = Shell::new(Default::default());
        shell.control.recursion_limit = 8;
        shell.control.call_depth = 7;
        // Value::Unit with no args returns Ok(Unit) without recursing.
        let _ = trampoline(Value::Unit, vec![], &mut shell);
        assert_eq!(shell.control.call_depth, 7);
    }
}
