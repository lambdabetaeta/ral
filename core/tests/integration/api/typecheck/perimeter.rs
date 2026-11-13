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
//! The enumeration of tables is not the whole perimeter: `env` (typed
//! `Map String`) and a bound head that is not a block (`HeadBoundToValue`) were
//! casts in term rules, and `tests/reject` pins both.

use ral_core::compile::compile_and_typecheck;
use ral_core::ir::Phrase;
use ral_core::source::FileId;
use ral_core::test_access::{
    boundary_result_is_data, builtin_scheme, has_result_only_var, has_weak_residuals,
};
use ral_core::ty::{Closed, Kind};
use ral_core::typecheck::{TypeErrorKind, Unit};

fn defined(src: &str) -> Vec<(String, Closed)> {
    let shell = ral_core::test_helper::fresh_shell();
    let top = compile_and_typecheck(
        src,
        shell.session_schemes(),
        FileId::DUMMY,
        None,
        Unit::Script,
        &mut crate::common::no_modules,
    )
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

#[test]
fn every_boundary_in_the_core_manifest_returns_data() {
    let shell = ral_core::test_helper::fresh_shell();
    for name in shell.builtin_names() {
        let entry = shell
            .lookup_builtin(name)
            .expect("every manifest name has a builtin entry");
        if entry.decl.is_boundary() {
            assert!(
                boundary_result_is_data(builtin_scheme(&entry)),
                "{name}: a boundary must return data with deep rows"
            );
        }
    }
}

#[test]
fn decoded_data_cannot_be_a_function_or_handle_in_its_unit() {
    for (decoder, value) in [
        ("from-json", "$decoded"),
        ("from-json-at []", "$decoded"),
        ("from-jsonl", "$decoded[0]"),
        ("from-csv", "$decoded[0]"),
        ("env", "$decoded"),
    ] {
        for value in [value.to_owned(), format!("{value}[delayed]")] {
            for usage in [format!("{value} 1"), format!("await {value}")] {
                let src = format!("let decoded = {decoder}\n{usage}");
                let errors = super::raw_errors(&src);
                assert_eq!(super::codes_of(&errors), ["T0074"], "{src}: {errors:?}");
                assert!(
                    matches!(errors[0].kind, TypeErrorKind::KindMismatch { kind, .. } if kind == Kind::Data),
                    "{src}: {errors:?}"
                );
                super::assert_error_in(&src, &errors[0], &usage);
            }
        }
    }
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
        "let i = { |p| let d = from-json < $p; $d[a] }",
        "let v = $from-json",
        "let w = from-json-at",
        "let doc = from-json < data.json",
        "let lines = from-jsonl < data.jsonl",
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
