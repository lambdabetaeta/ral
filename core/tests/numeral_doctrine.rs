//! The word doctrine, as a session observes it.
//!
//! A bare word shaped like a numeral *denotes its number* — in an argv, in a
//! binding, in an interpolation, at a redirect's target: everywhere a word may
//! stand, and with no position exempt.  A user who means the bytes quotes them.
//!
//! Its complement is canonicity: every number has exactly one printed spelling,
//! the shortest decimal that reads back as the same number.  So the two
//! judgments compose into a fixed point — a canonical numeral crosses the shell
//! byte for byte, while `007`, `+5` and `1.50` are normalized on the way out.
//! That normalization is the point of the doctrine, not an accident of it: it is
//! what makes one spelling per number a fact a reader can rely on.
//!
//! The observable half — the normalisation table, quoting, every position, the
//! redirect target, printing — is the golden `tests/lang/numerals.ral`.  What
//! stays here is what a golden cannot reach: the two fixed-point properties,
//! read off `fmt_float` itself, and the interactive renderer, a library
//! function with no command name to reach it by.

mod common;

use common::fresh_shell;

use ral_core::builtins::{REPL_PRINT_PARAMS, pretty_print};
use ral_core::ir::Val;
use ral_core::protocol::{Program, Run};
use ral_core::types::{GrantStack, Value, fmt_float};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};

/// A session as every front end builds one: prelude registered, env seeded,
/// capabilities at root.
/// What `src` writes to stdout, the run having succeeded.
fn printed(src: &str) -> String {
    match fresh_shell().run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<numeral-doctrine>".into(),
            caps: GrantStack::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
    }) {
        RunReport::Ran {
            ending, captured, ..
        } => {
            ending
                .into_result()
                .unwrap_or_else(|e| panic!("{src:?} must run: {e:?}"));
            String::from_utf8(captured.map(|c| c.stdout).unwrap_or_default())
                .expect("captured stdout is UTF-8")
        }
        RunReport::Static { diagnostics, .. } => panic!(
            "{src:?} must reach the evaluator, got {}",
            ral_core::diagnostic::format_static_diagnostics(&diagnostics).0
        ),
    }
}

/// The floats both round-trip properties run over: zero and its sign, halves,
/// integral values, the magnitudes at either end that print in exponent form,
/// the representable extremes, and two irrationals whose shortest spelling
/// uses every digit it is allowed.
const FLOAT_PROBES: [f64; 16] = [
    0.0,
    -0.0,
    0.5,
    -0.5,
    1.5,
    3.0,
    3.1,
    100.0,
    1e-7,
    1e16,
    1e300,
    -1e300,
    f64::MIN_POSITIVE,
    f64::MAX,
    1.0 / 3.0,
    std::f64::consts::SQRT_2,
];

// ── One printed spelling per number ──────────────────────────────────────────

/// The fixed point the doctrine rests on: print a number, hand the spelling
/// back as a bare word, and the shell writes the very same bytes.  Canonical
/// spellings are already at rest — there is nowhere further for them to
/// normalize to.
#[test]
fn a_canonical_spelling_is_a_fixed_point() {
    let ints = [0_i64, 7, -7, 42, 1_000_000, i64::MAX, i64::MIN];
    for spelling in ints
        .iter()
        .map(i64::to_string)
        .chain(FLOAT_PROBES.into_iter().map(fmt_float))
    {
        assert_eq!(
            printed(&format!("echo {spelling}")),
            format!("{spelling}\n"),
            "the canonical spelling {spelling:?} must print itself"
        );
    }
}

/// The same fixed point read at the value rather than the bytes, which is the
/// stronger claim: the printer's whole image lies inside the numeral grammar,
/// so printing a `Float` and classifying the spelling returns that `Float` and
/// not a `String`.  Compared bit for bit, since `-0.0 == 0.0` would let a lost
/// sign pass.
#[test]
fn printing_a_float_then_classifying_returns_the_same_float() {
    for f in FLOAT_PROBES {
        let spelling = Value::Float(f).to_string();
        match ral_core::test_access::val_from_word(&spelling) {
            Val::Float(g) => assert_eq!(
                g.to_bits(),
                f.to_bits(),
                "{spelling:?} must read back as the float it was printed from"
            ),
            other => panic!("{spelling:?} must read back as a Float, got {other:?}"),
        }
    }
}

// ── The corollary: `unit` is a word, `()` is the literal ─────────────────────

/// The interactive renderer, the classifier and the text form agree — the
/// halves of the doctrine no command name reaches: an integral `Float` keeps
/// its point, the widest magnitude keeps its mantissa point, non-finite is a
/// named last resort, and `unit` is a plain word beside the `()` literal.
#[test]
fn the_renderer_and_the_classifier_agree_with_the_text_form() {
    assert_eq!(
        pretty_print(&Value::Float(3.0), 0, &REPL_PRINT_PARAMS),
        "3.0"
    );
    assert_eq!(fmt_float(f64::MAX), "1.7976931348623157e308");
    assert_eq!(fmt_float(f64::NAN), "NaN");
    assert_eq!(fmt_float(f64::INFINITY), "inf");
    assert_eq!(fmt_float(f64::NEG_INFINITY), "-inf");
    assert_eq!(
        ral_core::test_access::val_from_word("unit"),
        Val::String("unit".into())
    );
    assert_eq!(pretty_print(&Value::Unit, 0, &REPL_PRINT_PARAMS), "()");
}
