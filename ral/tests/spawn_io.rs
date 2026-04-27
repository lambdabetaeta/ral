// Integration tests for `await`'s record return.
//
// `await h` resolves a `Handle α` to `{ value, stdout, stderr, status }`.
// Bytes are not auto-replayed: they only reach the caller's stdout/stderr
// when the user reads the record fields.  A redirect inside the spawned
// block bypasses the buffer entirely.
#![cfg(unix)]

mod common;

use common::{fresh_tmp_path, run};

fn run_io(script: &str) -> common::Output {
    run("ral_spawn_io", script)
}

// `await` does not auto-replay: the spawned block's bytes only reach the
// caller's stdout when the user reads the record's `stdout` field.
#[test]
fn await_does_not_auto_replay() {
    let out = run_io(r#"
        let h = spawn { echo from-child }
        let r = await $h
        echo marker-after
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        !out.stdout.contains("from-child"),
        "block bytes leaked to terminal: {:?}",
        out.stdout
    );
    assert!(out.stdout.contains("marker-after"), "stdout: {:?}", out.stdout);
}

// Reading `r[stdout]` and rendering it gives the spawned block's bytes.
#[test]
fn record_carries_block_stdout() {
    let out = run_io(r#"
        let h = spawn { echo from-child }
        let r = await $h
        echo !{to-bytes $r[stdout] | from-string}
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.contains("from-child"), "stdout: {:?}", out.stdout);
}

// A block that prints AND returns a value: both pieces are independently
// accessible from the record.
#[test]
fn record_value_and_stdout_both_accessible() {
    let out = run_io(r#"
        let h = spawn { echo middle; 7 }
        let r = await $h
        echo "value=$r[value]"
        echo !{to-bytes $r[stdout] | from-string}
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.contains("value=7"), "stdout: {:?}", out.stdout);
    assert!(out.stdout.contains("middle"), "stdout: {:?}", out.stdout);
}

// Repeat-await: cached record returns the same bytes (buffers drained on first).
#[test]
fn await_record_cached_across_calls() {
    let out = run_io(r#"
        let h = spawn { echo just-once }
        let r1 = await $h
        let r2 = await $h
        echo !{to-bytes $r1[stdout] | from-string}
        echo !{to-bytes $r2[stdout] | from-string}
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(
        out.stdout.matches("just-once").count(),
        2,
        "expected both awaits to surface the bytes; stdout: {:?}",
        out.stdout
    );
}

// Two concurrent spawns never awaited or disowned: no leak to the caller's
// stdout between the spawns and program exit.
#[test]
fn unawaited_spawns_do_not_leak() {
    let out = run_io(r#"
        let a = spawn { echo alpha }
        let b = spawn { echo bravo }
        sleep 0.1
        echo only-this
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.contains("only-this"), "stdout: {:?}", out.stdout);
    assert!(
        !out.stdout.contains("alpha"),
        "alpha leaked: {:?}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("bravo"),
        "bravo leaked: {:?}",
        out.stdout
    );
}

// A redirect inside the spawned block sends bytes to the file, bypasses the
// handle buffer, so the awaited record's `stdout` field is empty.
#[test]
fn redirect_bypasses_record() {
    let logfile = fresh_tmp_path("ral_spawn_io_redir", "log");
    let script = format!(
        r#"
        let h = spawn {{ /bin/echo written-to-file > {path} }}
        let r = await $h
        echo !{{to-bytes $r[stdout] | from-string}}
        echo after
        "#,
        path = logfile.to_str().unwrap(),
    );
    let out = run_io(&script);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    let contents = std::fs::read_to_string(&logfile).unwrap_or_default();
    std::fs::remove_file(&logfile).ok();
    assert!(
        contents.contains("written-to-file"),
        "file contents: {:?}",
        contents
    );
    assert!(
        !out.stdout.contains("written-to-file"),
        "redirect bytes leaked into record stdout: {:?}",
        out.stdout
    );
    assert!(out.stdout.contains("after"), "stdout: {:?}", out.stdout);
}

// The spawned block's stderr lives in the record's `stderr` field.
#[test]
fn record_carries_block_stderr() {
    let out = run_io(r#"
        let h = spawn { /bin/sh -c "echo diag >&2" }
        let r = await $h
        echo !{to-bytes $r[stderr] | from-string}
        "#);
    assert_eq!(out.status, 0, "err: {}", out.stderr);
    assert!(out.stdout.contains("diag"), "stdout: {:?}", out.stdout);
    assert!(
        !out.stderr.contains("diag"),
        "stderr leaked to caller's stderr: {:?}",
        out.stderr
    );
}

// `2>&1` inside the block runs inside the child process before the shell
// sees anything, so combined bytes land in stdout_buf and surface as
// `r[stdout]`, with `r[stderr]` empty.
#[test]
fn stderr_to_stdout_inside_block() {
    let out = run_io(r#"
        let h = spawn { /bin/sh -c "echo both >&2" 2>&1 }
        let r = await $h
        echo !{to-bytes $r[stdout] | from-string}
        "#);
    assert_eq!(out.status, 0, "err: {}", out.stderr);
    assert!(out.stdout.contains("both"), "stdout: {:?}", out.stdout);
    assert!(!out.stderr.contains("both"), "stderr: {:?}", out.stderr);
}
