//! Registry-driven property test (rec. A9): the sixth, previously
//! unchecked facet — *a reducer's returned value inhabits its declared
//! return type*.  Nothing asserted that running the reducer on
//! inhabitants of its argument types yields an inhabitant of its return
//! type.  That gap is where B1 (`equal`'s missing arms returned the wrong
//! shape), B2 (`lt`/`gt` typed `Bool` but compared `to_string`), B5
//! (arity-1 string builtins silently accepting zero args), and `echo`'s
//! String-vs-Unit reconciliation lived.
//!
//! Runtime values are first-order, so inhabitation is a direct structural
//! match.  For every `Scheme` builtin whose argument and return types are
//! drawn from the inhabitable first-order fragment, we generate argument
//! inhabitants, run the reducer, and assert the result inhabits the
//! declared return type.  Polymorphic positions (`Ty::Var`) accept any
//! value.  Builtins that need live resources (filesystem, stdin, spawned
//! handles, a host event sink) cannot be swept by a pure reducer call and
//! are listed in `RESOURCE_BACKED` with their reason.

mod common;

use ral_core::builtins::CORE_BUILTINS;
use ral_core::typecheck::builtins::BuiltinTypeRule;
use ral_core::typecheck::{CompTy, Ty, Unifier};
use ral_core::types::{Map, Mooring, Shell, Value};
use ral_core::{Break, builtins};

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
    ("use", "reads and evaluates a file"),
    ("cwd", "reads the shell's logical cwd"),
    ("fold-lines", "drains stdin"),
    ("ask", "reads the controlling terminal"),
    ("surface", "forwards to a host event sink"),
    ("spawn", "forks a concurrent worker"),
    ("await", "needs a live Handle"),
    ("race", "needs live Handles"),
    ("cancel", "needs a live Handle"),
    ("exit", "raises a process-exit escape"),
    ("quit", "raises a process-exit escape"),
];

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

/// A representative inhabitant of a concrete first-order type, or `None`
/// for a type the generator does not synthesise (the whole builtin is then
/// skipped).  `Ty::Var` is a polymorphic position — any value inhabits it,
/// so a scalar stands in.
fn inhabitant(ty: &Ty) -> Option<Value> {
    Some(match ty {
        Ty::Unit => Value::Unit,
        Ty::Bool => Value::Bool(true),
        Ty::Int | Ty::Var(_) => Value::Int(1),
        Ty::Float => Value::Float(1.5),
        Ty::String => Value::String("ab".into()),
        Ty::Bytes => Value::Bytes(vec![1, 2]),
        Ty::List(elem) => Value::list(vec![inhabitant(elem)?, inhabitant(elem)?]),
        Ty::Map(val) => {
            let pairs: Vec<(String, Value)> = vec![("k".into(), inhabitant(val)?)];
            Value::Map(Map::from(pairs))
        }
        // Thunk / Handle / Record / Variant arguments need a constructed
        // closure or live resource; out of the pure-reducer fragment.
        _ => return None,
    })
}

/// Does `value` inhabit the (resolved) declared return type?  A `Ty::Var`
/// return is polymorphic — any value inhabits it.
fn inhabits(value: &Value, ty: &Ty) -> bool {
    match (ty, value) {
        (Ty::List(elem), Value::List(items)) => items.iter().all(|v| inhabits(v, elem)),
        (Ty::Map(val), Value::Map(m)) => m.iter().all(|(_, v)| inhabits(v, val)),
        (Ty::Var(_), _)
        | (Ty::Unit, Value::Unit)
        | (Ty::Bool, Value::Bool(_))
        | (Ty::Int, Value::Int(_))
        // The checker promotes Int to Float at use sites; an Int result for
        // a Float position still inhabits it.
        | (Ty::Float, Value::Float(_) | Value::Int(_))
        | (Ty::String, Value::String(_))
        | (Ty::Bytes, Value::Bytes(_))
        | (Ty::Handle(_), Value::Handle(_))
        // Record/Variant returns carry a row; a Map/Variant runtime value is
        // the honest first-order witness and the dedicated builtin tests
        // check the field shapes.
        | (Ty::Record(_), Value::Map(_))
        | (Ty::Variant(_), Value::Variant { .. }) => true,
        _ => false,
    }
}

/// Walk `Thunk(Fun(A₁, … Fun(Aₙ, Return(_, R))))` into `(args, ret)`.
/// Returns `None` for a scheme that is not a thunked function (e.g. a
/// `pure` value scheme), which the sweep ignores.
fn arg_and_return_types(u: &mut Unifier, ty: &Ty) -> Option<(Vec<Ty>, Ty)> {
    let Ty::Thunk(cty) = ty else {
        return None;
    };
    let mut args = Vec::new();
    let mut cur: &CompTy = cty;
    loop {
        match cur {
            CompTy::Fun(param, body) => {
                args.push(u.resolve_ty(param));
                cur = body;
            }
            CompTy::Return(_, ret) => return Some((args, u.resolve_ty(ret))),
            CompTy::Var(_) => return None,
        }
    }
}

#[test]
fn every_scheme_reducer_inhabits_its_return_type() {
    let mut checked = 0usize;
    for entry in CORE_BUILTINS {
        let BuiltinTypeRule::Scheme(factory) = entry.type_rule else {
            continue;
        };
        let name = entry.name.as_ref();
        if RESOURCE_BACKED.iter().any(|(n, _)| *n == name) {
            continue;
        }

        // The factory mints fresh unifier vars for each quantified
        // position; nothing is unified, so a quantified `Ty::Var` stays
        // unbound and is treated as a polymorphic (any-value) position.
        let mut u = Unifier::new();
        let scheme = factory(&mut u);
        let Some((arg_tys, ret_ty)) = arg_and_return_types(&mut u, &scheme.ty) else {
            continue;
        };
        let Some(args) = arg_tys.iter().map(inhabitant).collect::<Option<Vec<_>>>() else {
            continue;
        };

        let mut shell = fresh_shell();
        match entry.body.call(&args, &Mooring::adrift(), &mut shell) {
            Ok(result) => {
                assert!(
                    inhabits(&result, &ret_ty),
                    "builtin `{name}` returned {result:?}, which does not inhabit its \
                     declared return type {ret_ty:?} (args: {args:?})"
                );
                checked += 1;
            }
            // A reducer is free to reject a particular inhabitant (e.g. a
            // regex builtin handed a non-pattern); a typed error is not a
            // type-inhabitation failure.  An `Escape` would be — the sweep
            // covers no exiting builtins.
            Err(Break::Error(_)) => checked += 1,
            Err(other) => panic!("builtin `{name}` escaped under the sweep: {other:?}"),
        }
    }
    assert!(
        checked >= 8,
        "the registry sweep checked only {checked} builtins — the inhabitable \
         fragment should not have shrunk this far"
    );
}
