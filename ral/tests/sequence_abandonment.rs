#![allow(clippy::disallowed_methods, reason = "test code")]

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
    let out = run(
        "ral_abandoned_none",
        "echo first\nfail [status: 1, message: boom]\n",
    );
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
        "echo outer\n!{ echo inner; fail [status: 1, message: boom]; echo a; echo b }\necho last\n",
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

/// `attempt` cannot make a command run, so a step that never became a process
/// is not told to wrap itself in one; a path naming no program offers itself
/// as text instead, bound when it is the whole of a `let`.
#[test]
fn a_command_that_never_ran_is_not_told_to_attempt() {
    let out = run(
        "ral_abandoned_unrunnable",
        "echo a\nnosuchcmd-xyz\necho b\n",
    );
    assert!(!out.stderr.contains("attempt"), "stderr: {}", out.stderr);

    // This crate's own directory: one that exists, spelled with a separator.
    let dir = env!("CARGO_MANIFEST_DIR");
    let out = run(
        "ral_abandoned_directory",
        &format!("let d = {dir}\necho b\n"),
    );
    assert!(
        out.stderr
            .contains(&format!("{dir} is a directory, not a command")),
        "stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains(&format!(
            "to bind the path as text, write `let d = #'{dir}'#`"
        )),
        "stderr: {}",
        out.stderr
    );
    assert!(!out.stderr.contains("attempt"), "stderr: {}", out.stderr);
}
