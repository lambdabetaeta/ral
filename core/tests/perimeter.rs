//! The soundness perimeter, from the stored-scheme side.
//!
//! A value of a type the program did not decide enters typed code only through
//! a boundary, whose result variables are weak: one type per unit, recorded in
//! the scheme as a residual and never quantified.  The builtin tables are swept
//! by `core`'s own unit test and by the `exarch` and `ral` crates, through the
//! same predicate; this file sweeps what a unit *stores*: the `Define` schemes
//! of programs that put every boundary under a `let`, a lambda, a recursive
//! function and a reference.
//!
//! The enumeration of tables is not the whole perimeter: `$ENV` (typed
//! `Map String`) and a bound head that is not a block (`HeadBoundToValue`) were
//! casts in term rules, and `tests/reject` pins both.

mod common;

use ral_core::ir::Phrase;
use ral_core::source::FileId;
use ral_core::test_access::{has_result_only_var, has_weak_residuals};
use ral_core::{Scheme, compile_and_typecheck};

fn defined(src: &str) -> Vec<(String, std::sync::Arc<Scheme>)> {
    let shell = common::fresh_shell();
    let top = compile_and_typecheck(src, shell.session_schemes(), FileId::DUMMY, "", None)
        .unwrap_or_else(|e| panic!("{src:?} must check: {e}"));
    top.phrases
        .iter()
        .filter_map(|phrase| match &phrase.item {
            Phrase::Define { schemes, .. } => Some(schemes.clone()),
            Phrase::Run(_) => None,
        })
        .flatten()
        .collect()
}

/// Each program binds a function or a value whose result a boundary decides;
/// none of the stored schemes may quantify it, and the ones a boundary's
/// result reaches say so with a residual.
#[test]
fn a_boundary_result_is_a_residual_and_never_quantified() {
    let programs = [
        "let f = { |p| from-json < $p }",
        "let g = { from-json }",
        "let h = { |p| from-jsonl < $p }",
        "let at = { |t| from-json-at $t }",
        "let u = { |p| use $p }",
        "let i = { |p| let d = from-json < $p; $d[a] }",
        "let v = $from-json",
        "let w = from-json-at",
        "let doc = from-json < data.json",
        "let lines = from-jsonl < data.jsonl",
        "let m = use 'lib.ral'",
        "let go = { |p n| if $[$n == 0] { from-json < $p } else { go $p $[$n - 1] } }",
        "let viaop = { |p| let d = from-json < $p; return $[$d[n] + 1] }",
    ];
    for src in programs {
        for (name, scheme) in defined(src) {
            assert!(
                !has_result_only_var(&scheme),
                "`{name}` of {src:?} quantifies a variable only a boundary decides"
            );
        }
    }
    for src in [
        "let f = { |p| from-json < $p }",
        "let doc = from-json < data.json",
        "let at = { |t| from-json-at $t }",
        "let w = from-json-at",
    ] {
        assert!(
            defined(src)
                .iter()
                .any(|(_, scheme)| has_weak_residuals(scheme)),
            "{src:?} stores no residual for its boundary's result"
        );
    }
}
