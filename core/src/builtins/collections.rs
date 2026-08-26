//! List combinators — `each`, `map`, `filter`, `sort-list`, `sort-list-by`,
//! `fold`, `fold-lines` — and the `range` constructor.
//!
//! Every combinator's per-element applications land in the enclosing trail
//! unwrapped: none of them is an observation in its own right, only the
//! combinator's own command observation (from `evaluator::audit::frame_call`)
//! is real.

use crate::types::{Break, Mooring, Settled, Shell, Value, as_list, sig};

use super::apply;
use super::util::value_ordering;

/// Steps `range` runs between cancellation polls: its per-step work is a
/// single push, which an unconditional poll would dominate.  The combinators
/// poll every element, since each already pays for an `apply`.
const INTERRUPT_POLL_CHUNK: usize = 1024;

/// Ceiling on `range`'s pre-sized `Vec` — a huge span must not ask
/// `with_capacity` for terabytes, which aborts the process rather than
/// erroring.  Past it the vector just grows.
const RANGE_INITIAL_CAP: usize = 1 << 16;

/// The prelude spelling (`let for = { |list fn| each $fn $list }`) a loop is
/// normally written in; either way the audit tree shows the real builtin
/// that ran, `each`.
pub(super) fn builtin_each(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let func = &args[0];
    let items = as_list(&args[1], "each")?;
    let mut last = Value::Unit;
    for item in &items {
        crate::process::check(mooring)?;
        last = apply(func, std::slice::from_ref(item), mooring, shell)?;
    }
    Ok(last)
}

pub(super) fn builtin_map(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let func = &args[0];
    let items = as_list(&args[1], "map")?;
    let mut out = Vec::with_capacity(items.len());
    for item in &items {
        crate::process::check(mooring)?;
        out.push(apply(func, std::slice::from_ref(item), mooring, shell)?);
    }
    Ok(Value::list(out))
}

pub(super) fn builtin_filter(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let func = &args[0];
    let items = as_list(&args[1], "filter")?;
    let mut results = Vec::new();
    for item in &items {
        crate::process::check(mooring)?;
        let result = apply(func, std::slice::from_ref(item), mooring, shell)?;
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

/// Sort `keyed` by its keys and return the values.  `sort_by` demands an
/// infallible comparator, so the first uncomparable pairing is parked in
/// `err` and surfaces once the sort has settled.
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

pub(super) fn builtin_sort(args: &[Value]) -> Settled<Value> {
    let items = as_list(&args[0], "sort-list")?;
    ordered_sort(
        items.into_iter().map(|v| (v.clone(), v)).collect(),
        "sort-list",
    )
}

pub(super) fn builtin_sort_by(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let func = &args[0];
    let items = as_list(&args[1], "sort-list-by")?;
    let keyed: Vec<(Value, Value)> = items
        .into_iter()
        .map(|item| {
            crate::process::check(mooring)?;
            let key = apply(func, std::slice::from_ref(&item), mooring, shell)?;
            Ok((key, item))
        })
        .collect::<Settled<Vec<_>>>()?;
    ordered_sort(keyed, "sort-list-by")
}

pub(super) fn builtin_range(args: &[Value], mooring: &Mooring) -> Settled<Value> {
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
    let mut out = Vec::with_capacity(count.min(RANGE_INITIAL_CAP));
    let mut i = start;
    let mut since_poll = 0usize;
    while i < end {
        out.push(Value::Int(i));
        i += 1;
        since_poll += 1;
        if since_poll == INTERRUPT_POLL_CHUNK {
            crate::process::check(mooring)?;
            since_poll = 0;
        }
    }
    Ok(Value::list(out))
}

pub(super) fn builtin_fold(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let func = &args[0];
    let mut acc = args[1].clone();
    let items = as_list(&args[2], "fold")?;
    for item in &items {
        crate::process::check(mooring)?;
        acc = apply(func, &[acc, item.clone()], mooring, shell)?;
    }
    Ok(acc)
}

/// `fold` fed by the byte channel: one line at a time, no list in hand.
pub(super) fn builtin_fold_lines(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let func = args[0].clone();
    let mut acc = args[1].clone();
    super::util::for_each_stdin_line("fold-lines", shell, |line, shell| {
        acc = apply(&func, &[acc.clone(), Value::String(line)], mooring, shell)?;
        Ok(())
    })?;
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Force a lambda literal to a `Value` so a combinator can be driven
    /// directly.  The bodies below are bare values, so applying one reaches
    /// no statement-level poll point: the only cancellation checkpoint left
    /// is the combinator's own, which is what these tests pin.
    fn lambda(mooring: &Mooring, shell: &mut Shell, src: &str) -> Value {
        let top = crate::compile(src).expect("compile lambda");
        crate::evaluator::run_phrases(
            &top.phrases,
            shell.env.clone(),
            crate::evaluator::Mode::Session,
            mooring,
            shell,
        )
        .outcome
        .expect("lambda value")
    }

    fn ints(n: i64) -> Value {
        Value::list((0..n).map(Value::Int).collect())
    }

    fn status(b: Break) -> i32 {
        match b {
            Break::Error(e) => e.exit_code(),
            other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
        }
    }

    #[test]
    fn map_polls_cancellation_within_its_loop() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let func = lambda(&m, &mut shell, "{ |x| $x }");
        m.cancel.cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_map(&[func, ints(500)], &m, &mut shell)
            .expect_err("a cancelled scope must abort map");
        assert_eq!(status(err), 130);
    }

    #[test]
    fn filter_polls_cancellation_within_its_loop() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let func = lambda(&m, &mut shell, "{ |x| $[0 == 0] }");
        m.cancel.cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_filter(&[func, ints(500)], &m, &mut shell)
            .expect_err("a cancelled scope must abort filter");
        assert_eq!(status(err), 130);
    }

    #[test]
    fn fold_polls_cancellation_within_its_loop() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let func = lambda(&m, &mut shell, "{ |acc x| $x }");
        m.cancel.cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_fold(&[func, Value::Int(0), ints(500)], &m, &mut shell)
            .expect_err("a cancelled scope must abort fold");
        assert_eq!(status(err), 130);
    }

    #[test]
    fn each_polls_cancellation_within_its_loop() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let func = lambda(&m, &mut shell, "{ |x| $x }");
        m.cancel.cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_each(&[func, ints(500)], &m, &mut shell)
            .expect_err("a cancelled scope must abort each");
        assert_eq!(status(err), 130);
    }

    #[test]
    fn long_range_polls_past_the_chunk() {
        let m = Mooring::adrift();
        m.cancel.cancel(crate::process::CancelCause::Interrupt);
        let err = builtin_range(&[Value::Int(0), Value::Int(4096)], &m)
            .expect_err("a long range under a cancelled scope must abort");
        assert_eq!(status(err), 130);
    }

    /// The other half of the chunk contract: a short range is cheap because
    /// it never polls, so a cancelled scope does not stop it.
    #[test]
    fn short_range_pays_no_poll() {
        let m = Mooring::adrift();
        m.cancel.cancel(crate::process::CancelCause::Interrupt);
        let v = builtin_range(&[Value::Int(0), Value::Int(10)], &m)
            .expect("a short range pays no poll and completes");
        assert_eq!(as_list(&v, "range").expect("list").len(), 10);
    }
}
