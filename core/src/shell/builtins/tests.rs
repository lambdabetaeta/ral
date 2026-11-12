//! Registry-driven property test (rec. A9): *a reducer's returned value
//! inhabits its declared return type*.  Nothing else asserts that running
//! a reducer on inhabitants of its argument types yields an inhabitant of
//! its return type — the gap where a builtin's missing arms return the
//! wrong shape (B1), a builtin typed `Bool` but compares `to_string` (B2),
//! an arity-1 string builtin silently accepts zero args (B5), or `echo`'s
//! String-vs-Unit reconciliation goes wrong would hide.
//!
//! Runtime values are first-order, so inhabitation is a direct structural
//! match.  For every builtin whose argument and return types are drawn from
//! the inhabitable first-order fragment, we generate argument inhabitants,
//! run the reducer, and assert the result inhabits the declared return
//! type.  Polymorphic positions (`Ty::Var`) accept any value.  Builtins
//! that need live resources (filesystem, stdin, spawned handles, a host
//! event sink) cannot be swept by a pure reducer call and are listed in
//! `RESOURCE_BACKED` with their reason.
//!
//! The roster is `CORE_BUILTINS`, the manifest's value half: a reducer taking
//! its arguments is what a generated inhabitant can be applied to.  The argv
//! half, `CORE_BASE_FRAMES`, is not swept here — each frame writes to a byte
//! channel no pure call supplies, which is the same reason `RESOURCE_BACKED`
//! exists — and is covered by `core/tests/integration/api/argv.rs`.

use super::{BOUNDARY_BUILTINS, CORE_BUILTINS};
use crate::Break;
use crate::shell::runtime::{Mooring, ValClos, View};
use crate::test_helper::fresh_shell;
use crate::ty::{CompTyNode, Grade, RowNode, Ty, TyNode};

/// Builtins whose reducer reaches a resource a bare in-test call cannot
/// satisfy, so the registry sweep cannot run them.  Each is exercised by
/// dedicated tests elsewhere; the reason is recorded so the list stays
/// honest rather than a silent escape hatch.
const RESOURCE_BACKED: &[(&str, &str)] = &[
    ("glob", "touches the filesystem"),
    ("list-dir", "touches the filesystem"),
    ("file-info", "touches the filesystem"),
    ("temp-dir", "creates a filesystem object"),
    ("temp-file", "creates a filesystem object"),
    ("source", "reads and evaluates a file"),
    ("cwd", "reads the shell's logical cwd"),
    ("fold-lines", "drains stdin"),
    ("ask", "reads the controlling terminal"),
    ("from-bytes", "reads the byte channel"),
    ("from-string", "reads the byte channel"),
    ("from-line", "reads the byte channel"),
    ("from-lines", "reads the byte channel"),
    ("await", "needs a live Handle"),
    ("race", "needs live Handles"),
    ("cancel", "needs a live Handle"),
    ("exit", "raises a process-exit escape"),
    ("quit", "raises a process-exit escape"),
];

/// A representative inhabitant of a concrete first-order type, or `None`
/// for a type the generator does not synthesise (the whole builtin is then
/// skipped).  `Ty::Var` is a polymorphic position — any value inhabits it,
/// so a scalar stands in.
fn inhabitant(ty: &Ty) -> Option<ValClos> {
    Some(match ty.node() {
        TyNode::Unit => ValClos::UNIT,
        TyNode::Bool => ValClos::bool(true),
        TyNode::Int | TyNode::Var(_) => ValClos::int(1),
        TyNode::Float => ValClos::float(crate::data::Finite::new(1.5).expect("finite")),
        TyNode::String => ValClos::string("ab"),
        TyNode::Bytes => ValClos::bytes(vec![1, 2]),
        TyNode::List(elem) => ValClos::list(vec![inhabitant(elem)?, inhabitant(elem)?]),
        TyNode::Map(val) => {
            let pairs: Vec<(String, ValClos)> = vec![("k".into(), inhabitant(val)?)];
            ValClos::map(pairs)
        }
        // Thunk / Handle / Record / Variant arguments need a constructed
        // closure or live resource; out of the pure-reducer fragment.
        _ => return None,
    })
}

/// Does `value` inhabit the (resolved) declared return type?  A `Ty::Var`
/// return is polymorphic — any value inhabits it.
fn inhabits(value: &ValClos, ty: &Ty) -> bool {
    match (ty.node(), value.view()) {
        (TyNode::List(elem), View::List(items)) => items.iter().all(|v| inhabits(&v, elem)),
        (TyNode::Map(val), View::Map(m)) => m.iter().all(|(_, v)| inhabits(&v, val)),
        (TyNode::Var(_), _)
        | (TyNode::Unit, View::Unit)
        | (TyNode::Bool, View::Bool(_))
        | (TyNode::Int, View::Int(_))
        // The checker promotes Int to Float at use sites; an Int result for
        // a Float position still inhabits it.
        | (TyNode::Float, View::Float(_) | View::Int(_))
        | (TyNode::String, View::String(_))
        | (TyNode::Bytes, View::Bytes(_))
        | (TyNode::Handle(..), View::Handle(_))
        // Record/Variant returns carry a row; a Map/Variant runtime value is
        // the honest first-order witness and the dedicated builtin tests
        // check the field shapes.
        | (TyNode::Record(_), View::Map(_))
        | (TyNode::Variant(_), View::Variant { .. }) => true,
        _ => false,
    }
}

/// Walk `Thunk(Fun(A₁, … Fun(Aₙ, Return(_, R))))` into `(args, ret)`.
/// Returns `None` for a scheme that is not a thunked function (e.g. a
/// `pure` value scheme), which the sweep ignores.
fn arg_and_return_types(ty: &Ty) -> Option<(Vec<Ty>, Ty)> {
    let TyNode::Thunk(cty) = ty.node() else {
        return None;
    };
    let mut args = Vec::new();
    let mut cur = cty;
    loop {
        match cur.node() {
            CompTyNode::Fun(param, body) => {
                args.push(param.clone());
                cur = body;
            }
            CompTyNode::Return(_, ret) => {
                return Some((args, ret.clone()));
            }
            CompTyNode::Var(_) => return None,
        }
    }
}

#[test]
fn every_scheme_reducer_inhabits_its_return_type() {
    for entry in CORE_BUILTINS {
        let name = entry.decl.name.as_ref();
        if RESOURCE_BACKED.iter().any(|(n, _)| *n == name) {
            continue;
        }

        // Quantified positions are treated as polymorphic (any-value).
        let scheme = entry.decl.scheme();
        let Some((arg_tys, ret_ty)) = arg_and_return_types(&scheme.ty) else {
            continue;
        };
        let Some(args) = arg_tys.iter().map(inhabitant).collect::<Option<Vec<_>>>() else {
            continue;
        };

        let mut shell = fresh_shell();
        match entry.run(&args, &Mooring::adrift(), &mut shell) {
            Ok(result) => assert!(
                inhabits(&result, &ret_ty),
                "builtin `{name}` returned {result:?}, which does not inhabit its \
                 declared return type {ret_ty:?} (args: {args:?})"
            ),
            // A reducer is free to reject a particular inhabitant (e.g. a
            // regex builtin handed a non-pattern); a typed error is not a
            // type-inhabitation failure.  An `Escape` would be — the sweep
            // covers no exiting builtins.
            Err(Break::Error(_)) => {}
            Err(other) => panic!("builtin `{name}` escaped under the sweep: {other:?}"),
        }
    }
}

/// Every producer reachable in `ty`, a `Return` or a handle, as `(grade, value)`.
fn producers(ty: &Ty, out: &mut Vec<(Grade, Ty)>) {
    match ty.node() {
        TyNode::Thunk(cty) => {
            let mut cur = cty;
            while let CompTyNode::Fun(param, body) = cur.node() {
                producers(param, out);
                cur = body;
            }
            if let CompTyNode::Return(grade, value) = cur.node() {
                out.push((*grade, value.clone()));
                producers(value, out);
            }
        }
        TyNode::Handle(grade, value) => {
            out.push((*grade, value.clone()));
            producers(value, out);
        }
        TyNode::List(a) | TyNode::Map(a) => producers(a, out),
        TyNode::Record(row) | TyNode::Variant(row) => {
            let mut cur = row;
            while let RowNode::Extend(_, field, rest) = cur.node() {
                producers(field, out);
                cur = rest;
            }
        }
        TyNode::Unit
        | TyNode::Bytes
        | TyNode::Bool
        | TyNode::Int
        | TyNode::Float
        | TyNode::String
        | TyNode::Var(_) => {}
    }
}

/// The grade invariant, declared: `F^w` is paired with `Unit` alone, and a
/// grade variable with one value type, so no instantiation can pair a
/// command grade with a value.
#[test]
fn every_scheme_pairs_a_grade_with_one_value_type() {
    let table = crate::HostSurface::default().builtin_table();
    let echo = table.get("echo").expect("the base frame `echo`");
    let rows = CORE_BUILTINS
        .iter()
        .chain(BOUNDARY_BUILTINS)
        .chain(std::iter::once(&echo));
    for entry in rows {
        let name = entry.decl.name.as_ref();
        let scheme = entry.decl.scheme();
        let mut found = Vec::new();
        producers(&scheme.ty, &mut found);
        let mut paired: std::collections::HashMap<Grade, Ty> = std::collections::HashMap::new();
        for (grade, value) in found {
            match grade {
                Grade::Output => assert_eq!(value, Ty::unit(), "{name}: a command produces `()`"),
                Grade::Value => {}
                Grade::Var(_) => {
                    let first = paired.entry(grade).or_insert_with(|| value.clone());
                    assert_eq!(*first, value, "{name}: one grade variable, two value types");
                }
            }
        }
    }
}

#[test]
fn ordinary_builtin_backstops_are_typed_and_keep_operations() {
    use crate::fact::{ArithmeticFailure, EvalFailure, InvalidCause, Reason};
    let invalid = |result: crate::shell::runtime::Settled<ValClos>, operation: &str| {
        let Err(Break::Error(error)) = result else {
            panic!("expected refusal for {operation}")
        };
        let Reason::Invalid(failure) = error.reason else {
            panic!("invalid input for {operation}")
        };
        assert_eq!(failure.operation, operation);
        *failure
    };
    let mut shell = fresh_shell();
    for (builtin, name) in [
        (
            super::math::builtin_floor as fn(&[ValClos]) -> crate::shell::runtime::Settled<ValClos>,
            "floor",
        ),
        (super::math::builtin_ceil, "ceil"),
        (super::math::builtin_trunc, "trunc"),
    ] {
        assert!(matches!(
            invalid(builtin(&[ValClos::int(1)]), name).cause,
            InvalidCause::Shape(_)
        ));
        let value = crate::data::Finite::new(f64::MAX).unwrap();
        assert_eq!(
            invalid(builtin(&[ValClos::float(value)]), name).cause,
            InvalidCause::IntegerRange(value)
        );
    }
    for args in [
        [
            ValClos::float(crate::data::Finite::from(1_i64)),
            ValClos::string("2"),
        ],
        [
            ValClos::float(crate::data::Finite::from(1_i64)),
            ValClos::int(309),
        ],
    ] {
        let failure = invalid(super::math::builtin_round(&args), "round");
        assert!(matches!(
            failure.cause,
            InvalidCause::Shape(_) | InvalidCause::Range(_)
        ));
    }
    assert!(
        matches!(super::math::builtin_round(&[ValClos::float(crate::data::Finite::new(f64::MAX).unwrap()), ValClos::int(308)]),
        Err(Break::Error(crate::Error { reason: Reason::Evaluation(f), .. })) if matches!(*f, EvalFailure::Arithmetic(ArithmeticFailure::RoundUnrepresentable(_))))
    );
    assert!(matches!(
        invalid(
            super::collections::builtin_nth(&[ValClos::int(-1), ValClos::list(vec![])]),
            "nth"
        )
        .cause,
        InvalidCause::Range(_)
    ));
    assert!(matches!(
        invalid(
            super::collections::builtin_range(
                &[ValClos::int(i64::MIN), ValClos::int(i64::MAX)],
                &shell
            ),
            "range"
        )
        .cause,
        InvalidCause::AddressableRange(_)
    ));
    for arg in [ValClos::string(""), ValClos::int(1)] {
        let _ = invalid(super::shell::builtin_chdir(&[arg], &mut shell), "cd");
    }
}

#[test]
fn byte_list_validation_keeps_the_element_path() {
    use crate::fact::{DataStep, InvalidCause, Reason};
    for (item, range) in [(ValClos::int(256), true), (ValClos::string("1"), false)] {
        let Err(Break::Error(error)) =
            super::util::as_byte_list(&ValClos::list(vec![ValClos::int(0), item]), "ints-to-bytes")
        else {
            panic!("invalid byte")
        };
        let Reason::Invalid(failure) = error.reason else {
            panic!("invalid input")
        };
        assert_eq!(failure.path, vec![DataStep::Index(1usize.into())]);
        assert_eq!(matches!(failure.cause, InvalidCause::Range(_)), range);
    }
}
