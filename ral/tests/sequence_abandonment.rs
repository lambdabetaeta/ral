#![allow(clippy::disallowed_methods)]

//! A sequence halts at its first failure, and the parts after it never run.
//!
//! That truncation is otherwise invisible: an enclosing `audit` reports a
//! `trail` that stops short, which reads exactly like a sequence that
//! had fewer parts to begin with.  So the error says that later steps were
//! abandoned — on the innermost sequence that abandoned them, only when there
//! were any, and only when the failure has no more specific hint of its own.

mod common;

use common::run;

#[test]
fn a_failing_final_part_abandons_nothing() {
    let out = run("ral_abandoned_none", "echo first\n/usr/bin/false\n");
    assert!(
        !out.stderr.contains("did not run"),
        "nothing followed the failure, so nothing was abandoned; stderr: {}",
        out.stderr
    );
}

/// The tail after the failing step is named, once, on the innermost block.
#[test]
fn the_hint_belongs_to_the_innermost_sequence() {
    let out = run(
        "ral_abandoned_innermost",
        "echo outer\n!{ echo inner; /usr/bin/false; echo a; echo b }\necho last\n",
    );
    assert!(
        out.stderr.contains("later steps in this block did not run"),
        "the inner block abandoned steps; stderr: {}",
        out.stderr
    );
    assert!(!out.stdout.contains("last"), "stdout: {}", out.stdout);
}

/// A signal death already carries its own hint, which says more about the
/// failure than the sequence can say about its own shape.
#[cfg(unix)]
#[test]
fn a_more_specific_hint_survives() {
    let out = run(
        "ral_abandoned_keeps_hint",
        "sh -c #'kill -TERM $$'#\necho second\n",
    );
    assert!(
        out.stderr.contains("terminated from"),
        "stderr: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("did not run"),
        "the signal's own hint holds the one hint slot; stderr: {}",
        out.stderr
    );
}
