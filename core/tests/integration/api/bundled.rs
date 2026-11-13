//! Bundled `coreutils` tools under the public run entry: captured output,
//! scoped working directory, and byte stages run as direct children.

#![cfg(feature = "coreutils")]

use crate::common::must_succeed;
use ral_core::{ValClos, View};

#[cfg(unix)]
fn must_fail(input: &str) {
    assert!(
        crate::common::eval(input).is_err(),
        "should fail: {input:?}"
    );
}

#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn bundled_uutils_capture_honours_scoped_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let script = format!(
        "within [dir: '{}'] {{ let out = pwd\nreturn $out }}",
        dir.path().display()
    );
    let value = must_succeed(&script);
    let View::String(output) = value.view() else {
        panic!("expected pwd output string");
    };
    assert_eq!(
        std::fs::canonicalize(output.as_str()).unwrap(),
        dir.path().canonicalize().unwrap()
    );
}

#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn bundled_uutils_capture_completes_with_buffer_sink() {
    let out = must_succeed("let out = ls -lah\nreturn $out");
    match out.view() {
        View::String(s) => assert!(!s.is_empty()),
        other => panic!("expected captured ls output, got {other:?}"),
    }
}

/// A bundled byte stage runs as a direct `ral --ral-bundled-tool` child
/// (no helper eval) and produces correct bytes and a success status.
/// `printf … | wc -l` spawns both `printf` and `wc` as direct bundled
/// children; capturing the count through `from-string` recovers `wc`'s
/// bytes (`"4\n"` for four lines) and proves the pipeline succeeded — a
/// non-zero `wc` would fail the `from-string` capture instead.
#[cfg(unix)]
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn bundled_byte_stage_runs_as_direct_child_and_produces_bytes() {
    assert_eq!(
        must_succeed("!{printf 'a\\nb\\nc\\nd\\n' | wc -l | from-string}"),
        ValClos::string("4\n"),
        "a bundled `wc -l` over four lines must emit `4`, recovered through \
         the byte-to-value `from-string` edge"
    );
}

/// A failing bundled tool in a process-staged pipeline surfaces a
/// failure rather than swallowing the non-zero status: `wc` on a missing
/// path exits non-zero, and the pipeline must fail-fast.
#[cfg(unix)]
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn failing_bundled_byte_stage_surfaces_failure() {
    must_fail("printf 'a\\nb\\n' | wc /nonexistent/path");
}
