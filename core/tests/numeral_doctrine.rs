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
//! Everything here drives the public `run` door, so each test is the session a
//! user has; the one exception is the interactive renderer, a library function
//! with no command name to reach it by.

mod common;

use ral_core::builtins::{REPL_PRINT_PARAMS, pretty_print};
use ral_core::ir::Val;
use ral_core::protocol::{Program, Run};
use ral_core::types::{Capabilities, Shell, Value, fmt_float};
use ral_core::{
    RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, StaticDiagnostics,
};

/// A session as every front end builds one: prelude registered, env seeded,
/// capabilities at root.
fn fresh_shell() -> Shell {
    ral_core::boot::boot_shell(
        ral_core::io::TerminalState::default(),
        common::prelude(),
        &ral_core::boot::HostSurface::default(),
    )
}

/// What `src` writes to stdout, the run having succeeded.
fn printed(src: &str) -> String {
    match fresh_shell().run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<numeral-doctrine>".into(),
            caps: Capabilities::root(),
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
        lifecycle: Box::new(()),
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
            match diagnostics {
                StaticDiagnostics::Parse(e) => format!("parse {e:?}"),
                StaticDiagnostics::Types(errs) => format!("{} type diagnostic(s)", errs.len()),
                StaticDiagnostics::Host(e) => format!("host {e:?}"),
            }
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

// ── A numeral denotes its number ─────────────────────────────────────────────

/// The normalization table, read where an argv is observable: `echo` writes
/// each atom's text form, so what it writes is what the word denoted.
#[test]
fn a_numeral_word_reaches_an_argv_as_its_number() {
    for (word, number) in [
        ("007", "7"),
        ("+5", "5"),
        ("-0", "0"),
        ("+007", "7"),
        (".5", "0.5"),
        ("2.", "2.0"),
        ("00.500", "0.5"),
        ("1.50", "1.5"),
        ("3.0", "3.0"),
        ("1.50e300", "1.5e300"),
        ("1.0E5", "100000.0"),
    ] {
        assert_eq!(
            printed(&format!("echo {word}")),
            format!("{number}\n"),
            "{word}"
        );
    }
}

/// The version-like token is the edge worth knowing: `3.10` is a numeral, so it
/// is the number 3.1, and quoting is how the digits stay a name.
#[test]
fn a_version_like_numeral_is_a_number_and_quoting_keeps_the_digits() {
    assert_eq!(printed("echo 3.10"), "3.1\n");
    assert_eq!(printed("echo '3.10'"), "3.10\n");
}

/// Quoting is the whole of the escape hatch, and it is exact.
#[test]
fn quoting_keeps_a_numerals_bytes() {
    for word in ["007", "+5", "1.50", "-0", "2."] {
        assert_eq!(
            printed(&format!("echo '{word}'")),
            format!("{word}\n"),
            "{word}"
        );
    }
}

/// And the grammar's edges are words, not numbers: an exponent with no decimal
/// point, a digit separator, another base, a spelling past `Int`, and one that
/// would overflow a `Float` to infinity.
#[test]
fn a_word_the_numeral_grammar_refuses_keeps_its_bytes_unquoted() {
    for word in [
        "1e6",
        "1_000",
        "0x10",
        "9223372036854775808",
        "1.8e308",
        "1.2.3",
        "inf",
        "nan",
    ] {
        assert_eq!(
            printed(&format!("echo {word}")),
            format!("{word}\n"),
            "{word}"
        );
    }
}

// ── No position is exempt ────────────────────────────────────────────────────

/// A binding, an interpolation, and the three encoders read the same word the
/// same way — `to-json` included, so the JSON number and the shell's own text
/// form cannot drift apart.
#[test]
fn the_reading_does_not_depend_on_the_position() {
    assert_eq!(printed("let rate = 1.50; echo $rate"), "1.5\n");
    assert_eq!(
        printed(r#"let rate = 3.0; echo "version $rate""#),
        "version 3.0\n"
    );
    assert_eq!(printed("to-line 1.50"), "1.5\n");
    assert_eq!(printed("to-string 3.0"), "3.0");
    assert_eq!(printed("to-lines [3.0, 1.50, .5]"), "3.0\n1.5\n0.5");
    assert_eq!(printed("to-lines [007, +5, -0]"), "7\n5\n0");
    assert_eq!(printed("to-json [rate: 3.0]"), "{\"rate\":3.0}");
    assert_eq!(printed("to-csv [[rate: 1.50]]"), "rate\n1.5\n");
}

/// A redirect's target is a word like any other, so `> 007` names the file `7`.
/// The surprising position is exactly the one worth pinning.
#[test]
fn a_redirect_target_is_read_as_a_numeral_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().display();
    assert_eq!(
        printed(&format!("within [dir: '{root}'] {{ echo hi > 007 }}")),
        ""
    );
    assert!(
        dir.path().join("7").is_file(),
        "`> 007` must name the file 7"
    );
    assert!(
        !dir.path().join("007").exists(),
        "`> 007` must not name the file 007"
    );
}

// ── One printed spelling per number ──────────────────────────────────────────

/// Floatness survives on screen and on argv: an integral `Float` keeps its
/// decimal point, where the `Int` beside it has none.  Rust's own `{}` erases
/// this distinction, which is why the shell does not use it.
#[test]
fn an_integral_float_still_prints_as_a_float() {
    assert_eq!(printed("echo $[1.0 + 2.0]"), "3.0\n");
    assert_eq!(printed("echo $[1 + 2]"), "3\n");
    assert_eq!(
        pretty_print(&Value::Float(3.0), 0, &REPL_PRINT_PARAMS),
        "3.0",
        "the interactive renderer agrees with the text form"
    );
}

/// A large or small magnitude takes exponent form rather than spelling out
/// hundreds of digits — and keeps its point there too, so the spelling is a
/// numeral of the grammar rather than a word that merely looks numeric.
#[test]
fn an_extreme_magnitude_prints_in_exponent_form_with_its_point() {
    assert_eq!(printed("echo $[1.0e300 * 1.0]"), "1.0e300\n");
    assert_eq!(printed("echo $[1.0e-7 * 1.0]"), "1.0e-7\n");
    assert_eq!(
        fmt_float(f64::MAX),
        "1.7976931348623157e308",
        "a mantissa that already carries a point is left alone"
    );
}

/// Non-finite is unreachable through the language — a `Float` is finite by
/// construction — so the renderer's answer for it is a last resort, and named
/// here so it is a choice rather than a surprise.
#[test]
fn the_renderer_names_the_unreachable_non_finite_cases() {
    assert_eq!(fmt_float(f64::NAN), "NaN");
    assert_eq!(fmt_float(f64::INFINITY), "inf");
    assert_eq!(fmt_float(f64::NEG_INFINITY), "-inf");
}

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
        match Val::from_word(&spelling) {
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

/// A literal whose printed form is *nothing* can obey one-spelling-per-value
/// from neither end, so `unit` gave the name back.  `()` took its place as
/// punctuation the renderer prints as itself, beside `[]` and `[:]`.
#[test]
fn unit_is_an_ordinary_word_and_the_literal_is_punctuation() {
    assert_eq!(printed("echo unit"), "unit\n");
    assert_eq!(Val::from_word("unit"), Val::String("unit".into()));
    assert_eq!(
        pretty_print(&Value::Unit, 0, &REPL_PRINT_PARAMS),
        "()",
        "the renderer prints the literal a user would write"
    );
    // `Display` is also the argv rendering and the interpolation form: one
    // spelling, no special case.
    assert_eq!(printed("echo a () b"), "a () b\n");
    assert_eq!(printed("let uu = ()\necho \"u=$uu\""), "u=()\n");
}
