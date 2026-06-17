//! List and collection combinators: `each`, `map`, `filter`, `sort-list`,
//! `sort-list-by`, and `fold`.
//!
//! These builtins provide the standard higher-order iteration primitives
//! over ral lists.  `each` and `map` participate in the audit tree when
//! auditing is active, recording their execution as interior nodes; the
//! other combinators run their per-element applications directly, so those
//! children land in the enclosing trail without a wrapping combinator node.

use crate::types::*;

use super::apply;
use super::util::{as_list, check_arity, value_ordering};

/// How many `range` steps run between cancellation polls.  The
/// higher-order combinators poll once per element — negligible beside the
/// `apply` they already perform — but `range`'s per-step work is a single
/// push, so an unconditional poll would dominate it; checking every
/// `INTERRUPT_POLL_CHUNK` steps keeps a tight numeric loop responsive
/// while a range shorter than the chunk pays nothing.
const INTERRUPT_POLL_CHUNK: usize = 1024;

/// `each <fn> <list>` -- call `fn` on each element for side effects.
/// Returns the result of the last application, or `Unit` for an empty list.
pub(super) fn builtin_each(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "each")?;
    let func = &args[0];
    let items = as_list(&args[1], "each")?;
    iterate_audited("for", shell, |shell| {
        let mut last = Value::Unit;
        for item in &items {
            if let Err(e) = crate::process::check(shell) {
                return (last, Some(e));
            }
            match apply(func, std::slice::from_ref(item), shell) {
                Ok(v) => last = v,
                Err(e) => return (last, Some(e)),
            }
        }
        (last, None)
    })
}

/// `map <fn> <list>` -- apply `fn` to each element, return a new list.
pub(super) fn builtin_map(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "map")?;
    let func = &args[0];
    let items = as_list(&args[1], "map")?;
    iterate_audited("map", shell, |shell| {
        let mut out = Vec::with_capacity(items.len());
        for item in &items {
            if let Err(e) = crate::process::check(shell) {
                return (Value::list(out), Some(e));
            }
            match apply(func, std::slice::from_ref(item), shell) {
                Ok(v) => out.push(v),
                Err(e) => return (Value::list(out), Some(e)),
            }
        }
        (Value::list(out), None)
    })
}

/// Run an iteration combinator, optionally inside an audit scope.
///
/// `body` returns its (possibly partial) value and an optional error: this
/// keeps the recorded audit-tree node faithful to whatever was accumulated
/// at the point of failure while propagating the error upwards.
fn iterate_audited(
    cmd: &str,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> (Value, Option<Break>),
) -> Settled<Value> {
    let (value, err) = if shell.local.audit.active() {
        let start = crate::evaluator::audit::start(shell);
        let principal = shell.mobile.context.principal();
        let (fragment, (value, err)) = shell.audit_child(body);
        let last_status = shell.mobile.control.last_status;
        let (stderr, status) = match &err {
            Some(Break::Error(e)) => (e.message.clone().into_bytes(), e.exit_code()),
            _ => (Vec::new(), last_status),
        };
        let node = ExecNode::command(
            cmd,
            Vec::new(),
            status,
            start.site.clone(),
            AuditIo {
                stdout: Vec::new(),
                stderr,
            },
            value.clone(),
            fragment.into_nodes(),
            AuditTime {
                start: start.time,
                end: crate::types::epoch_us(),
            },
            principal,
        );
        shell.local.audit.push(node);
        (value, err)
    } else {
        body(shell)
    };
    match err {
        Some(e) => Err(e),
        None => Ok(value),
    }
}

/// `filter <fn> <list>` -- keep elements where `fn` returns `true`.
pub(super) fn builtin_filter(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "filter")?;
    let func = &args[0];
    let items = as_list(&args[1], "filter")?;
    let mut results = Vec::new();
    for item in &items {
        crate::process::check(shell)?;
        let result = apply(func, std::slice::from_ref(item), shell)?;
        let keep = match &result {
            Value::Bool(b) => *b,
            _ => {
                return Err(sig(format!(
                    "filter: predicate must return Bool, got {} '{}'",
                    result.type_name(),
                    result
                )));
            }
        };
        if keep {
            results.push(item.clone());
        }
    }
    Ok(Value::list(results))
}

/// Sort `keyed` by the total `value_ordering` over each pair's first
/// component, surfacing the first uncomparable pairing as an error and
/// returning the second components in sorted order.  `sort_by` demands an
/// infallible comparator, so a failure is parked in `err` and the first
/// one is returned once the sort settles.
fn ordered_sort(mut keyed: Vec<(Value, Value)>, name: &str) -> Settled<Value> {
    let mut err: Option<Break> = None;
    keyed.sort_by(|(ka, _), (kb, _)| {
        value_ordering(ka, kb, name).unwrap_or_else(|e| {
            err.get_or_insert(e);
            std::cmp::Ordering::Equal
        })
    });
    match err {
        Some(e) => Err(e),
        None => Ok(Value::list(keyed.into_iter().map(|(_, v)| v).collect())),
    }
}

/// `sort-list <list>` -- sort a list into ascending order.
pub(super) fn builtin_sort(args: &[Value]) -> Settled<Value> {
    check_arity(args, 1, "sort-list")?;
    let items = as_list(&args[0], "sort-list")?;
    ordered_sort(
        items.into_iter().map(|v| (v.clone(), v)).collect(),
        "sort-list",
    )
}

/// `sort-list-by <fn> <list>` -- sort by a key function.
/// Applies `fn` to each element to obtain a sort key, then sorts into
/// ascending order of those keys.
pub(super) fn builtin_sort_by(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "sort-list-by")?;
    let func = &args[0];
    let items = as_list(&args[1], "sort-list-by")?;
    let keyed: Vec<(Value, Value)> = items
        .into_iter()
        .map(|item| {
            crate::process::check(shell)?;
            let key = apply(func, std::slice::from_ref(&item), shell)?;
            Ok((key, item))
        })
        .collect::<Settled<Vec<_>>>()?;
    ordered_sort(keyed, "sort-list-by")
}

/// `range <start> <end>` -- generate a list of integers from start (inclusive) to end (exclusive).
pub(super) fn builtin_range(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "range")?;
    let start = match &args[0] {
        Value::Int(n) => *n,
        other => {
            return Err(sig(format!(
                "range: expected Int for start, got {}",
                other.type_name()
            )));
        }
    };
    let end = match &args[1] {
        Value::Int(n) => *n,
        other => {
            return Err(sig(format!(
                "range: expected Int for end, got {}",
                other.type_name()
            )));
        }
    };
    let count = if end > start {
        end.checked_sub(start)
            .and_then(|span| usize::try_from(span).ok())
            .ok_or_else(|| {
                sig(format!(
                    "range: span from {start} to {end} exceeds the addressable range"
                ))
            })?
    } else {
        0
    };
    let mut out = Vec::with_capacity(count);
    let mut i = start;
    let mut since_poll = 0usize;
    while i < end {
        out.push(Value::Int(i));
        i += 1;
        since_poll += 1;
        if since_poll == INTERRUPT_POLL_CHUNK {
            crate::process::check(shell)?;
            since_poll = 0;
        }
    }
    Ok(Value::list(out))
}

/// `fold <fn> <init> <list>` — left fold, data-last.
pub(super) fn builtin_fold(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 3, "fold")?;
    let func = &args[0];
    let mut acc = args[1].clone();
    let items = as_list(&args[2], "fold")?;
    for item in &items {
        crate::process::check(shell)?;
        acc = apply(func, &[acc, item.clone()], shell)?;
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Force a lambda literal to a `Value` so a combinator can be driven
    /// directly.  The bodies here (`$x`, a `$[…]` Bool) are bare values,
    /// so *applying* the lambda reaches no statement-level poll point —
    /// the only checkpoint a combinator can hit is the one it now performs
    /// itself, which is exactly what these tests pin.
    fn lambda(shell: &mut Shell, src: &str) -> Value {
        let comp = Arc::new(crate::compile(src).expect("compile lambda"));
        crate::evaluator::eval_top_level(&comp, shell).expect("lambda value")
    }

    fn ints(n: i64) -> Value {
        Value::list((0..n).map(Value::Int).collect())
    }

    fn status(b: Break) -> i32 {
        match b {
            Break::Error(e) => e.exit_code(),
            other => panic!("expected Break::Error, got {other:?}"),
        }
    }

    /// A cancelled scope aborts `map` from inside its own loop — before
    /// the per-element `apply`, whose value-bodied callback would never
    /// poll on its own.  The list (500) is under `INTERRUPT_POLL_CHUNK`,
    /// so it is built without `range`'s poll masking the combinator's.
    #[test]
    fn map_polls_cancellation_within_its_loop() {
        let mut shell = Shell::new(Default::default());
        let func = lambda(&mut shell, "{ |x| $x }");
        shell
            .turn
            .cancel
            .cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_map(&[func, ints(500)], &mut shell)
            .expect_err("a cancelled scope must abort map");
        assert_eq!(status(err), 130);
    }

    #[test]
    fn filter_polls_cancellation_within_its_loop() {
        let mut shell = Shell::new(Default::default());
        let func = lambda(&mut shell, "{ |x| $[0 == 0] }");
        shell
            .turn
            .cancel
            .cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_filter(&[func, ints(500)], &mut shell)
            .expect_err("a cancelled scope must abort filter");
        assert_eq!(status(err), 130);
    }

    #[test]
    fn fold_polls_cancellation_within_its_loop() {
        let mut shell = Shell::new(Default::default());
        let func = lambda(&mut shell, "{ |acc x| $x }");
        shell
            .turn
            .cancel
            .cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_fold(&[func, Value::Int(0), ints(500)], &mut shell)
            .expect_err("a cancelled scope must abort fold");
        assert_eq!(status(err), 130);
    }

    #[test]
    fn each_polls_cancellation_within_its_loop() {
        let mut shell = Shell::new(Default::default());
        let func = lambda(&mut shell, "{ |x| $x }");
        shell
            .turn
            .cancel
            .cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_each(&[func, ints(500)], &mut shell)
            .expect_err("a cancelled scope must abort each");
        assert_eq!(status(err), 130);
    }

    /// `range` polls only every `INTERRUPT_POLL_CHUNK` steps, so a span
    /// longer than the chunk observes the cancel and aborts.
    #[test]
    fn long_range_polls_past_the_chunk() {
        let mut shell = Shell::new(Default::default());
        shell
            .turn
            .cancel
            .cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_range(&[Value::Int(0), Value::Int(4096)], &mut shell)
            .expect_err("a long range under a cancelled scope must abort");
        assert_eq!(status(err), 130);
    }

    /// A span shorter than the chunk pays no poll at all — it completes
    /// even under a cancelled scope, the "small ranges pay nothing"
    /// guarantee that keeps the common case free.
    #[test]
    fn short_range_pays_no_poll() {
        let mut shell = Shell::new(Default::default());
        shell
            .turn
            .cancel
            .cancel(crate::process::CancelCause::Interrupt);
        let v = builtin_range(&[Value::Int(0), Value::Int(10)], &mut shell)
            .expect("a short range pays no poll and completes");
        assert_eq!(as_list(&v, "range").expect("list").len(), 10);
    }
}
