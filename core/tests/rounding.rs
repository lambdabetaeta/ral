//! Tests for the rounding builtins `round`, `floor`, `ceil`, `trunc`.
//!
//! `round <x> <places>` is the decimal dial — always Float; `floor`/`ceil`/
//! `trunc` map a Float to the Int in their direction.  All four take a Float
//! only (an Int is a static type error — it is already rounded).  The harness
//! mirrors `comparison.rs`: bootstrap a prelude-registered `Shell` and drive
//! each source string through the public `run_turn` door like a REPL
//! turn.

mod common;

use ral_core::transport::{Program, Turn};
use ral_core::types::{Break, Capabilities, Settled, Shell, Value};
use ral_core::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin, builtins};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

fn eval(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run_turn(TurnRequest {
        turn: Turn {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            turn_limit: None,
            deferred_lease: None,
            worker_cap: None,
            io: TurnIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: TurnStdin::Inherit,
        },
        surface: None,
        deferred: None,
        desk: None,
        lifecycle: Box::new(()),
    }) {
        TurnReport::Ran { result, .. } => result,
        TurnReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// Run `source`, expecting it to return `Value::Int(expected)`.
fn expect_int(source: &str, expected: i64) {
    let mut shell = fresh_shell();
    match eval(&mut shell, source) {
        Ok(Value::Int(n)) => assert_eq!(n, expected, "{source:?}"),
        Ok(other) => panic!("{source:?}: expected Int({expected}), got {other:?}"),
        Err(e) => panic!("{source:?}: expected Int({expected}), got error {e:?}"),
    }
}

/// Run `source`, expecting it to return a `Value::Float` within 1e-9 of
/// `expected` (decimal fractions are not exactly representable in `f64`).
fn expect_float(source: &str, expected: f64) {
    let mut shell = fresh_shell();
    match eval(&mut shell, source) {
        Ok(Value::Float(f)) => assert!(
            (f - expected).abs() < 1e-9,
            "{source:?}: expected ~{expected}, got {f}"
        ),
        Ok(other) => panic!("{source:?}: expected Float(~{expected}), got {other:?}"),
        Err(e) => panic!("{source:?}: expected Float(~{expected}), got error {e:?}"),
    }
}

/// Run `source`, expecting a runtime `Break::Error` whose message contains
/// `needle`.
fn expect_error(source: &str, needle: &str) {
    let mut shell = fresh_shell();
    match eval(&mut shell, source) {
        Err(Break::Error(e)) => assert!(
            e.message.contains(needle),
            "{source:?}: error {:?} should mention {needle:?}",
            e.message
        ),
        Ok(v) => panic!("{source:?}: expected an error mentioning {needle:?}, got {v:?}"),
        Err(other) => panic!("{source:?}: expected Break::Error, got {other:?}"),
    }
}

/// Run `source`, expecting it to be rejected by the type checker before it
/// ever runs (a `TurnReport::Static`).
fn expect_static_reject(source: &str) {
    let mut shell = fresh_shell();
    match shell.run_turn(TurnRequest {
        turn: Turn {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            turn_limit: None,
            deferred_lease: None,
            worker_cap: None,
            io: TurnIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: TurnStdin::Inherit,
        },
        surface: None,
        deferred: None,
        desk: None,
        lifecycle: Box::new(()),
    }) {
        TurnReport::Static { .. } => {}
        TurnReport::Ran { result, .. } => {
            panic!("{source:?}: expected a static type error, but it ran: {result:?}")
        }
    }
}

#[test]
fn round_dials_decimal_places_and_returns_float() {
    expect_float("return !{round 3.7 0}", 4.0);
    expect_float("return !{round 3.4 0}", 3.0);
    #[allow(clippy::approx_constant)]
    expect_float("return !{round 3.14159 2}", 3.14);
    #[allow(clippy::approx_constant)]
    expect_float("return !{round 2.71828 3}", 2.718);
}

#[test]
fn round_halves_away_from_zero() {
    expect_float("return !{round 2.5 0}", 3.0);
    expect_float("return !{round -2.5 0}", -3.0);
    expect_float("return !{round 0.125 2}", 0.13);
}

#[test]
fn floor_ceil_trunc_pick_their_directions() {
    expect_int("return !{floor 3.9}", 3);
    expect_int("return !{floor -2.5}", -3);
    expect_int("return !{ceil 3.1}", 4);
    expect_int("return !{ceil -3.9}", -3);
    expect_int("return !{trunc 3.9}", 3);
    expect_int("return !{trunc -3.9}", -3);
}

#[test]
fn an_int_argument_is_a_static_type_error() {
    expect_static_reject("return !{round 5 0}");
    expect_static_reject("return !{floor 7}");
    expect_static_reject("return !{ceil 0}");
    expect_static_reject("return !{trunc 42}");
}

#[test]
fn non_finite_floats_are_refused() {
    expect_error("return !{round !{float \"nan\"} 2}", "not a finite number");
    expect_error("return !{floor !{float \"inf\"}}", "not a finite number");
    expect_error("return !{ceil !{float \"-inf\"}}", "not a finite number");
}

#[test]
fn floats_outside_the_integer_range_are_refused() {
    expect_error(
        "return !{floor !{float \"1e300\"}}",
        "outside the integer range",
    );
    expect_error(
        "return !{trunc !{float \"-1e300\"}}",
        "outside the integer range",
    );
}

#[test]
fn places_must_be_in_range() {
    expect_error("return !{round 3.14 -1}", "between 0 and");
    expect_error("return !{round 3.14 400}", "between 0 and");
}
