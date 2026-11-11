//! List combinators — `each`, `map`, `filter`, `sort-list-by`,
//! `fold`, `fold-lines` — the `range` constructor, and `nth`.
//!
//! Neither a combinator nor its per-element applications is an observation:
//! whatever effects an element's body performs land in the enclosing trail
//! unwrapped, from the doors that performed them.

use crate::error::Error;
use crate::fact::{
    AddressableRange, Expected, InvalidCause, InvalidFailure, Reason, ShapeFailure, Utf8Subject,
    ValueShape,
};
use crate::shell::evaluator::{apply, expr};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Break, Mooring, Settled, Shell, Value};

/// Steps `range` runs between cancellation polls: its per-step work is a
/// single push, which an unconditional poll would dominate.  The combinators
/// poll every element, since each already pays for an `apply`.
const INTERRUPT_POLL_CHUNK: usize = 1024;

/// Ceiling on `range`'s pre-sized `Vec` — a huge span must not ask
/// `with_capacity` for terabytes, which aborts the process rather than
/// erroring.  Past it the vector just grows.
const RANGE_INITIAL_CAP: usize = 1 << 16;

/// One element's application, each preceded by the cancellation poll that the
/// combinators owe every element.
fn call(func: &Value, item: Value, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    shell.poll()?;
    apply(func, vec![item], mooring, shell)
}

/// `nth i xs`: the position read, at a position computed at run time.
pub(super) fn builtin_nth(args: &[Value]) -> Settled<Value> {
    let (i, xs) = (&args[0], &args[1]);
    let n = super::strings::as_index(i, "nth", 0)?;
    Ok(expr::position(xs, n)?)
}

/// The prelude spelling (`let for = { |list fn| each $fn $list }`) a loop is
/// normally written in; either way the audit tree shows the real builtin
/// that ran, `each`.
pub(super) fn builtin_each(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let items = args[1].as_list("each")?;
    items
        .iter()
        .try_for_each(|item| call(&args[0], item.into_owned(), mooring, shell).map(drop))?;
    Ok(Value::Unit)
}

pub(super) fn builtin_map(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let items = args[1].as_list("map")?;
    let mapped = items
        .iter()
        .map(|item| call(&args[0], item.into_owned(), mooring, shell))
        .collect::<Settled<Vec<_>>>()?;
    Ok(Value::list(mapped))
}

pub(super) fn builtin_filter(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let items = args[1].as_list("filter")?;
    let kept = items
        .iter()
        .filter_map(
            |item| match call(&args[0], item.as_ref().clone(), mooring, shell) {
                Ok(Value::Bool(keep)) => keep.then(|| Ok(item.into_owned())),
                Ok(other) => Some(Err(other
                    .expected("filter", Expected::Shape(ValueShape::Bool))
                    .into())),
                Err(e) => Some(Err(e)),
            },
        )
        .collect::<Settled<Vec<_>>>()?;
    Ok(Value::list(kept))
}

/// Sort `keyed` by its keys and return the values.  `sort_by` demands an
/// infallible comparator, so the first uncomparable pairing is parked in
/// `err` and surfaces once the sort has settled.
fn ordered_sort(mut keyed: Vec<(Value, Value)>, name: &str) -> Settled<Value> {
    let mut err: Option<Break> = None;
    keyed.sort_by(|(ka, _), (kb, _)| {
        ka.compare(kb, name).unwrap_or_else(|e| {
            err.get_or_insert(e);
            std::cmp::Ordering::Equal
        })
    });
    match err {
        Some(e) => Err(e),
        None => Ok(Value::list(keyed.into_iter().map(|(_, v)| v).collect())),
    }
}

pub(super) fn builtin_sort_by(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let items = args[1].as_list("sort-list-by")?;
    let keyed = items
        .iter()
        .map(|item| {
            let item = item.into_owned();
            Ok((call(&args[0], item.clone(), mooring, shell)?, item))
        })
        .collect::<Settled<Vec<_>>>()?;
    ordered_sort(keyed, "sort-list-by")
}

pub(super) fn builtin_range(args: &[Value], shell: &Shell) -> Settled<Value> {
    let integer = |index: usize| match &args[index] {
        Value::Int(n) => Ok(*n),
        other => Err(Error::new(Reason::Invalid(Box::new(
            InvalidFailure::new(
                "range".into(),
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Int),
                    found: other.found(),
                }),
            )
            .index(index.into()),
        )))),
    };
    let start = integer(0)?;
    let end = integer(1)?;
    let count = if end > start {
        end.checked_sub(start)
            .and_then(|span| usize::try_from(span).ok())
            .ok_or_else(|| {
                invalid(
                    "range",
                    InvalidCause::AddressableRange(AddressableRange { start, end }),
                )
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
            shell.poll()?;
            since_poll = 0;
        }
    }
    Ok(Value::list(out))
}

pub(super) fn builtin_fold(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let func = &args[0];
    let mut acc = args[1].clone();
    let items = args[2].as_list("fold")?;
    for item in &items {
        shell.poll()?;
        acc = apply(func, vec![acc, item.into_owned()], mooring, shell)?;
    }
    Ok(acc)
}

/// `fold` fed by the byte channel: one line at a time, no list in hand.
pub(super) fn builtin_fold_lines(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let func = &args[0];
    let mut acc = args[1].clone();
    for line in super::util::stdin_lines("fold-lines", shell)? {
        let line =
            crate::shell::runtime::decode_utf8_strict(line?, "fold-lines", Utf8Subject::Input)?;
        acc = apply(func, vec![acc, Value::string(line)], mooring, shell)?;
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::ExitCode as _;

    /// Force a lambda literal to a `Value` so a combinator can be driven
    /// directly.  The bodies below are bare values, so applying one reaches
    /// no statement-level poll point: the only cancellation checkpoint left
    /// is the combinator's own, which is what these tests pin.
    fn lambda(mooring: &Mooring, shell: &mut Shell, src: &str) -> Value {
        let top = crate::compile::compile_and_typecheck(
            src,
            crate::test_helper::core_schemes(),
            crate::source::FileId::DUMMY,
            None,
            None,
        )
        .expect("compile lambda");
        crate::shell::evaluator::run_phrases(
            &top.phrases,
            shell.env.clone(),
            crate::shell::evaluator::Mode::Session,
            mooring,
            shell,
        )
        .outcome
        .expect("lambda value")
    }

    fn ints(n: i64) -> Value {
        Value::list((0..n).map(Value::Int).collect())
    }

    #[test]
    fn map_polls_cancellation_within_its_loop() {
        let mut shell = crate::test_helper::core_shell();
        let m = Mooring::adrift();
        let func = lambda(&m, &mut shell, "{ |x| $x }");
        shell.scope = crate::process::Scope::clone(&m.home);
        m.home.cancel(crate::process::CancelCause::Interrupted);
        let err = builtin_map(&[func, ints(500)], &m, &mut shell)
            .expect_err("a cancelled scope must abort map");
        assert_eq!(err.code(), 130);
    }

    #[test]
    fn filter_polls_cancellation_within_its_loop() {
        let mut shell = crate::test_helper::core_shell();
        let m = Mooring::adrift();
        let func = lambda(&m, &mut shell, "{ |x| $[0 == 0] }");
        shell.scope = crate::process::Scope::clone(&m.home);
        m.home.cancel(crate::process::CancelCause::Interrupted);
        let err = builtin_filter(&[func, ints(500)], &m, &mut shell)
            .expect_err("a cancelled scope must abort filter");
        assert_eq!(err.code(), 130);
    }

    #[test]
    fn fold_polls_cancellation_within_its_loop() {
        let mut shell = crate::test_helper::core_shell();
        let m = Mooring::adrift();
        let func = lambda(&m, &mut shell, "{ |acc x| $x }");
        shell.scope = crate::process::Scope::clone(&m.home);
        m.home.cancel(crate::process::CancelCause::Interrupted);
        let err = builtin_fold(&[func, Value::Int(0), ints(500)], &m, &mut shell)
            .expect_err("a cancelled scope must abort fold");
        assert_eq!(err.code(), 130);
    }

    #[test]
    fn each_polls_cancellation_within_its_loop() {
        let mut shell = crate::test_helper::core_shell();
        let m = Mooring::adrift();
        let func = lambda(&m, &mut shell, "{ |x| $x }");
        shell.scope = crate::process::Scope::clone(&m.home);
        m.home.cancel(crate::process::CancelCause::Interrupted);
        let err = builtin_each(&[func, ints(500)], &m, &mut shell)
            .expect_err("a cancelled scope must abort each");
        assert_eq!(err.code(), 130);
    }

    #[test]
    fn long_range_polls_past_the_chunk() {
        let mut shell = crate::test_helper::core_shell();
        let m = Mooring::adrift();
        shell.scope = crate::process::Scope::clone(&m.home);
        m.home.cancel(crate::process::CancelCause::Interrupted);
        let err = builtin_range(&[Value::Int(0), Value::Int(4096)], &shell)
            .expect_err("a long range under a cancelled scope must abort");
        assert_eq!(err.code(), 130);
    }

    /// The other half of the chunk contract: a short range is cheap because
    /// it never polls, so a cancelled scope does not stop it.
    #[test]
    fn short_range_pays_no_poll() {
        let mut shell = crate::test_helper::core_shell();
        let m = Mooring::adrift();
        shell.scope = crate::process::Scope::clone(&m.home);
        m.home.cancel(crate::process::CancelCause::Interrupted);
        let v = builtin_range(&[Value::Int(0), Value::Int(10)], &shell)
            .expect("a short range pays no poll and completes");
        assert_eq!(v.as_list("range").expect("list").len(), 10);
    }
    #[test]
    fn filter_requires_a_boolean_predicate_result() {
        let mut shell = crate::test_helper::core_shell();
        let mooring = Mooring::adrift();
        let predicate = lambda(&mooring, &mut shell, "{ |x| $x }");
        let Err(Break::Error(error)) = builtin_filter(
            &[predicate, Value::list(vec![Value::Int(1)])],
            &mooring,
            &mut shell,
        ) else {
            panic!("non-boolean predicate result")
        };
        assert_eq!(
            error.reason,
            Value::Int(1)
                .expected("filter", Expected::Shape(ValueShape::Bool))
                .reason
        );
    }

    #[test]
    fn range_start_and_end_shape_backstops_keep_the_argument_index() {
        let shell = crate::test_helper::core_shell();
        for (args, index) in [
            ([Value::Unit, Value::Int(1)], 0usize),
            ([Value::Int(1), Value::Unit], 1usize),
        ] {
            let Err(Break::Error(error)) = builtin_range(&args, &shell) else {
                panic!("invalid range operand")
            };
            assert!(
                matches!(error.reason, Reason::Invalid(f) if f.path == [crate::fact::DataStep::Index(index.into())] && matches!(&f.cause, InvalidCause::Shape(_)))
            );
        }
    }
}
