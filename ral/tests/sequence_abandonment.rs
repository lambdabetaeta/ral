#![allow(clippy::disallowed_methods)]

//! A sequence halts at its first failure, and the parts after it never run.
//!
//! That truncation is otherwise invisible: an enclosing `audit` reports a
//! `children` list that stops short, which reads exactly like a sequence that
//! had fewer parts to begin with.  So the error names how many parts it
//! abandoned — on the innermost sequence that abandoned them, only when there
//! were any, and only when the failure has no more specific hint of its own.

mod common;

use common::run;

#[test]
fn an_abandoned_tail_is_named_and_counted() {
    let out = run(
        "ral_abandoned_tail",
        "echo first\n/usr/bin/false\necho second\necho third\n",
    );
    assert!(
        out.stderr
            .contains("2 later steps in this block did not run"),
        "stderr: {}",
        out.stderr
    );
    assert!(!out.stdout.contains("second"), "stdout: {}", out.stdout);
}

#[test]
fn a_failing_final_part_abandons_nothing() {
    let out = run("ral_abandoned_none", "echo first\n/usr/bin/false\n");
    assert!(
        !out.stderr.contains("did not run"),
        "nothing followed the failure, so nothing was abandoned; stderr: {}",
        out.stderr
    );
}

#[test]
fn the_count_belongs_to_the_innermost_sequence() {
    let out = run(
        "ral_abandoned_innermost",
        "echo outer\n!{ echo inner; /usr/bin/false; echo a; echo b }\necho last\n",
    );
    assert!(
        out.stderr
            .contains("2 later steps in this block did not run"),
        "the inner block abandoned two parts, the outer one; stderr: {}",
        out.stderr
    );
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

#[test]
fn audit_records_the_hint_as_data() {
    let out = run(
        "ral_abandoned_in_audit",
        "let r = audit { echo A; /usr/bin/false; echo B }\necho $r[error]\n",
    );
    assert!(
        out.stdout
            .contains("1 later step in this block did not run"),
        "a report read as data must say what the rendered diagnostic says; stdout: {}",
        out.stdout
    );
}

#[test]
fn attempt_runs_every_step_and_reports_on_each() {
    let out = run(
        "ral_attempt_battery",
        "let r = audit { attempt { echo A }; attempt { /usr/bin/false }; attempt { echo C } }\n\
         echo $r[status]\n\
         echo !{length $r[children]}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(
        lines.contains(&"A") && lines.contains(&"C"),
        "every step ran; stdout: {}",
        out.stdout
    );
    assert!(
        lines.contains(&"0") && lines.contains(&"3"),
        "the block succeeded and observed all three steps; stdout: {}",
        out.stdout
    );
}
