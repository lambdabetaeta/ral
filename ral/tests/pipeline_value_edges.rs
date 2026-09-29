#![allow(clippy::disallowed_methods)]
//! Byte-pipeline boundaries that need the harness: what a forced thunk reads
//! from the process's own stdin, and what stays on standard error.  The rest
//! of the capture and value-edge rules are goldens in `tests/lang`
//! (`pipeline-values`, `capture-semantics`, `join-capture`).

mod common;

use common::{run, run_with_stdin};

fn run_pipe(script: &str) -> common::Output {
    run("ral_pipeline_value", script)
}

fn run_pipe_stdin(script: &str, stdin_data: &[u8]) -> common::Output {
    run_with_stdin("ral_pipeline_value", script, stdin_data)
}

/// The stdin wiring belongs to the pipeline, not to a thunk that escapes it.
/// `echo hi`'s bytes go into an edge nobody reads, and forcing `reader`
/// afterwards runs `from-line` against the script's own stdin.
#[test]
fn a_thunk_that_escapes_a_pipeline_reads_ambient_stdin() {
    let o = run_pipe_stdin(
        "let reader = echo hi | { from-line }\n\
         let s = !$reader\n\
         echo \"[$s]\"",
        b"ambient\n",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        "[ambient]",
        "the forced thunk must read the script's stdin, not the dead edge: {:?}",
        o.stdout
    );
    assert!(
        !o.stdout.contains("hi"),
        "the unread edge must swallow the producer's bytes: {:?}",
        o.stdout
    );
}

#[test]
fn warn_writes_stderr_and_stays_out_of_the_capture() {
    // `warn` is the whole diagnostic surface, and its route is Value: the line
    // reaches standard error while the capture binds the byte channel alone.
    let o = run_pipe("let payload = !{ warn 'note'; echo carried }\necho \"[$payload]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "[carried]", "full stdout: {:?}", o.stdout);
    assert!(
        o.stderr.contains("note"),
        "warn must reach standard error: {:?}",
        o.stderr
    );
}
