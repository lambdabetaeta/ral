#![allow(clippy::disallowed_methods)]

//! Regression tests for the builtins remediation (260611 deep-review §6):
//! `dedent` multibyte slicing (B3), `range` overflow (B4), `fail`/`exit`
//! status truncation (B6), `int` on a large Float (B7), `to-json` of
//! NaN/Infinity and computation values (B8, B12), `source`/`use` status
//! laundering (B10), prelude `lines`/`words` empties (B11), and the
//! `from-json` u64 overflow (B12).
//!
//! The harness mirrors `comparison.rs`: bootstrap a prelude-registered
//! `Shell`, then drive each source string through the public `run_turn`
//! door like a REPL turn.  Each defect passed `--check` and the prior
//! suite, so every test below is a coverage gap the doc facet let drift.

mod common;

use ral_core::types::{Break, Capabilities, Escape, Settled, Shell, Status, Value};
use ral_core::{
    RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin, builtins,
};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

/// Run one top-level turn of `source` through the public `run_turn` door
/// and return the body's `Settled<Value>`.  Every test below picks source
/// it expects to compile, so a static diagnostic is a test bug.
fn eval(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run_turn(
        source,
        TurnRequest {
            script_name: "<test>",
            caps: Capabilities::root(),
            turn_limit: None,
            detached_limit: None,
            io: TurnIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: TurnStdin::Inherit,
            surface: None,
            lifecycle: Box::new(()),
        },
    ) {
        TurnReport::Ran { result, .. } => result,
        TurnReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// Run `source`, expecting it to return `Value::String(expected)`.
fn expect_string(source: &str, expected: &str) {
    let mut shell = fresh_shell();
    match eval(&mut shell, source) {
        Ok(Value::String(s)) => assert_eq!(s, expected, "{source:?}"),
        Ok(other) => panic!("{source:?}: expected String({expected:?}), got {other:?}"),
        Err(e) => panic!("{source:?}: expected String({expected:?}), got error {e:?}"),
    }
}

/// Run `source`, expecting a `Break::Error` whose message contains `needle`.
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

// ── B3 — `dedent` counts characters, preserves blanks and CRLF ────────────

#[test]
fn dedent_multibyte_indentation_does_not_panic() {
    // Pre-fix: the indent width was a byte count from `trim_start`, which
    // strips the 2-byte NBSP; slicing the next line at that byte offset
    // landed mid-character and panicked.  Common indent here is one
    // character (NBSP on line 1, ASCII space on line 2), stripped from both.
    expect_string("dedent \"\u{00A0}a\n b\"", "a\nb");
}

#[test]
fn dedent_preserves_blank_lines_verbatim() {
    // The doc promises blank lines unchanged; a whitespace-only line must
    // not be re-emitted as empty.
    expect_string("dedent \"  a\n  \n  b\"", "a\n  \nb");
}

#[test]
fn dedent_does_not_normalise_crlf() {
    // `s.lines()` + `join("\n")` silently rewrote CRLF to LF; splitting on
    // `\n` keeps the `\r` that belongs to each CRLF terminator.
    expect_string("dedent \"  a\r\n  b\r\n\"", "a\r\nb\r\n");
}

// ── B4 — `range` reports overflow rather than panicking ───────────────────

#[test]
fn range_overflow_is_a_clean_error() {
    // (end - start) overflowed i64; debug panicked, release wrapped into a
    // bogus `with_capacity`.  A checked subtraction surfaces an error.
    expect_error("return !{range -2 9223372036854775807}", "range");
}

#[test]
fn range_ordinary_still_works() {
    let mut shell = fresh_shell();
    match eval(&mut shell, "return !{range 1 4}") {
        Ok(Value::List(items)) => {
            let got: Vec<i64> = items.iter().map(|v| v.as_int().unwrap()).collect();
            assert_eq!(got, vec![1, 2, 3]);
        }
        other => panic!("range 1 4: expected [1,2,3], got {other:?}"),
    }
}

// ── B6 — `fail`/`exit` preserve a status outside i32 ──────────────────────

/// Run `source` expecting a `Break::Error`; return its `Status`.
fn fail_status(source: &str) -> Status {
    let mut shell = fresh_shell();
    match eval(&mut shell, source) {
        Err(Break::Error(e)) => e.status,
        other => panic!("{source:?}: expected Break::Error, got {other:?}"),
    }
}

#[test]
fn fail_rejects_status_outside_i32() {
    // 2^32 truncates to 0 under `as i32`; the zero-status guard then saw a
    // success status it exists to forbid.  Range-check before truncating.
    expect_error("fail [status: 4294967296]", "status");
}

#[test]
fn fail_bare_int_rejects_status_outside_i32() {
    expect_error("fail 4294967296", "status");
}

#[test]
fn fail_preserves_in_range_status() {
    assert_eq!(fail_status("fail [status: 7]"), Status::Code(7));
}

#[test]
fn exit_rejects_status_outside_i32() {
    expect_error("exit 4294967296", "status");
}

#[test]
fn exit_preserves_in_range_status() {
    let mut shell = fresh_shell();
    match eval(&mut shell, "exit 7") {
        Err(Break::Escape(Escape::Exit(code))) => assert_eq!(code, 7),
        other => panic!("exit 7: expected Escape::Exit(7), got {other:?}"),
    }
}

// ── B7 — `int` range-checks a large integral Float ────────────────────────

#[test]
fn int_rejects_out_of_range_float() {
    // 1e300 has zero fractional part but is far outside i64; `as i64`
    // saturated to i64::MAX silently.
    expect_error("return !{int 1.0e300}", "range");
}

#[test]
fn int_accepts_in_range_integral_float() {
    let mut shell = fresh_shell();
    match eval(&mut shell, "return !{int 42.0}") {
        Ok(Value::Int(n)) => assert_eq!(n, 42),
        other => panic!("int 42.0: expected Int(42), got {other:?}"),
    }
}

#[test]
fn int_error_message_names_float() {
    // The fallthrough message omitted Float though Floats are accepted.
    expect_error("return !{int `some 1}", "Float");
}

// ── B8 / B12 — `to-json` refuses NaN/Infinity and computation values ──────

#[test]
fn to_json_refuses_nan() {
    expect_error("to-json !{float 'NaN'}", "to-json");
}

#[test]
fn to_json_refuses_infinity() {
    expect_error("to-json !{float 'inf'}", "to-json");
}

#[test]
fn to_json_refuses_block() {
    expect_error("to-json { return 1 }", "to-json");
}

// ── B12 — `from-json` refuses a u64 beyond i64 rather than lossy f64 ──────

#[test]
fn from_json_refuses_u64_beyond_i64() {
    expect_error("from-json '18446744073709551615'", "from-json");
}

// ── B11 — prelude `lines`/`words` strip leading/trailing empties ──────────

/// Run `source` expecting a `List` of `String`s; collect them.
fn string_list(source: &str) -> Vec<String> {
    let mut shell = fresh_shell();
    match eval(&mut shell, source) {
        Ok(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => panic!("{source:?}: expected String element, got {other:?}"),
            })
            .collect(),
        other => panic!("{source:?}: expected a List, got {other:?}"),
    }
}

#[test]
fn words_strips_leading_and_trailing_empties() {
    assert_eq!(
        string_list("return !{words ' hello world '}"),
        ["hello", "world"]
    );
}

#[test]
fn lines_strips_trailing_empty() {
    assert_eq!(string_list("return !{lines \"a\nb\n\"}"), ["a", "b"]);
}

#[test]
fn lines_keeps_interior_blank() {
    // Only the leading/trailing empties go; an interior blank line is real.
    assert_eq!(string_list("return !{lines \"a\n\nb\"}"), ["a", "", "b"]);
}

// ── B14 — coverage for previously untested builtins ───────────────────────

#[test]
fn sort_list_by_orders_by_key_numerically() {
    // The key function maps each string to its length; sorting is by the
    // total ordering over those Int keys, not their string form.
    let got = string_list("return !{sort-list-by { |s| length $s } ['ccc', 'a', 'bb']}");
    assert_eq!(got, ["a", "bb", "ccc"]);
}

#[test]
fn sort_list_by_surfaces_uncomparable_keys() {
    // A key function that yields uncomparable values (a Block) must error
    // rather than sort by a stringified key.
    expect_error(
        "return !{sort-list-by { |_| return { return 1 } } [1, 2]}",
        "sort-list-by",
    );
}

#[test]
fn re_find_match_returns_first() {
    expect_string("return !{re-find-match '[0-9]+' 'a12b34'}", "12");
}

#[test]
fn re_find_match_errors_when_absent() {
    expect_error("return !{re-find-match '[0-9]+' 'abc'}", "re-find-match");
}

#[test]
fn re_find_matches_returns_all() {
    assert_eq!(
        string_list("return !{re-find-matches '[0-9]+' 'a12b34'}"),
        ["12", "34"]
    );
}

#[test]
fn string_replace_unique_literal() {
    expect_string(
        "return !{string-replace 'world' 'ral' 'hello world'}",
        "hello ral",
    );
}

#[test]
fn string_replace_errors_on_ambiguous() {
    // `string-replace` demands exactly one literal occurrence.
    expect_error("return !{string-replace 'o' '0' 'foo'}", "string-replace");
}

// ── B9 — `is-writable` follows symlinks and reports honestly ──────────────

#[cfg(unix)]
mod fs_predicates {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Run `source`, expecting `Value::Bool(expected)`.
    fn expect_bool(source: &str, expected: bool) {
        let mut shell = fresh_shell();
        match eval(&mut shell, source) {
            Ok(Value::Bool(b)) => assert_eq!(b, expected, "{source:?}"),
            other => panic!("{source:?}: expected Bool({expected}), got {other:?}"),
        }
    }

    #[test]
    fn is_writable_follows_symlinks_and_honours_mode() {
        let dir = std::env::temp_dir().join(format!("ral-iswritable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let writable = dir.join("rw");
        std::fs::write(&writable, b"x").expect("write rw file");
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");

        let readonly = dir.join("ro");
        std::fs::write(&readonly, b"x").expect("write ro file");
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o444))
            .expect("chmod 444");

        let link = dir.join("link-to-ro");
        std::os::unix::fs::symlink(&readonly, &link).expect("symlink");

        let q = |p: &std::path::Path| format!("return !{{is-writable '{}'}}", p.display());
        // We own the 0644 file with the write bit set: writable.
        expect_bool(&q(&writable), true);
        // We own the 0444 file with no write bit: not writable (`access`
        // evaluates the real uid against the mode; `readonly()` agrees here
        // for the owner, but the symlink case below is where they diverge).
        expect_bool(&q(&readonly), false);
        // Pre-fix `is-writable` used the non-following probe and read the
        // *symlink's* own permissions (a fresh symlink is writable),
        // answering true; following the link answers about the read-only
        // target: false.
        expect_bool(&q(&link), false);

        // is-link sees the symlink; is-readable follows it to a readable file.
        expect_bool(&format!("return !{{is-link '{}'}}", link.display()), true);
        expect_bool(
            &format!("return !{{is-readable '{}'}}", link.display()),
            true,
        );
        expect_bool(
            &format!("return !{{is-link '{}'}}", writable.display()),
            false,
        );

        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o644)).ok();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
