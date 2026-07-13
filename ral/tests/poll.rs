// Integration tests for `poll` and the prelude `is-done`.
//
// `poll h` is the total, non-blocking dual of `await`: it yields
// `` `settled `` with `{stdout, stderr, outcome}` once the block has
// finished — `outcome` is `` `ok `` with its value or `` `err `` with the
// error record (never re-raising) — and `` `pending `` with `{stdout,
// stderr}` (the bytes written so far, a cumulative non-destructive snapshot)
// while the block is still running.  It errors only on a cancelled/forgotten
// handle.  `is-done` is total over a finished handle (true on `` `settled ``,
// false on `` `pending ``) and still propagates the detached error.
//
// Portable: every failing block here raises via the `fail` builtin rather
// than shelling out, so there is nothing Unix-specific in this file.

mod common;

use common::run;

fn run_poll(script: &str) -> common::Output {
    run("ral_poll", script)
}

// A block that printed and returned a value polls as `` `settled `` with an
// `` `ok `` outcome: the value and the buffered stdout are observable.  The
// script's clean exit confirms `poll` itself succeeded.
#[test]
fn poll_settled_carries_value_and_stdout() {
    let out = run_poll(
        r#"
        let h = spawn { echo done-here; 42 }
        sleep 0.2
        let polled = poll $h
        case $polled [
            `settled: { |s|
                case $s[outcome] [
                    `ok: { |v| echo "value=$v" },
                    `err: { |_| echo unexpected-err }
                ]
                echo !{to-bytes $s[stdout] | from-string}
            },
            `pending: { |_| echo unexpected-pending }
        ]
        "#,
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.contains("value=42"), "stdout: {:?}", out.stdout);
    assert!(out.stdout.contains("done-here"), "stdout: {:?}", out.stdout);
}

// A long-running block polls as `` `pending ``.
#[test]
fn poll_pending_while_running() {
    let out = run_poll(
        r"
        let h = spawn { sleep 2; return 1 }
        let polled = poll $h
        case $polled [
            `settled: { |_| echo unexpected-settled },
            `pending: { |_| echo pending }
        ]
        cancel $h
        ",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.contains("pending"), "stdout: {:?}", out.stdout);
}

// A running block's already-written output is observable in the `` `pending ``
// arm's partial `{stdout, stderr}` snapshot: `poll` does not wait for the
// block to finish, yet reads back what it has emitted so far.  The block keeps
// running (a long `sleep`) so the poll lands on `` `pending ``, not
// `` `settled ``.
#[test]
fn poll_pending_carries_partial_stdout() {
    let out = run_poll(
        r"
        let h = spawn { echo partial-line; sleep 2; return 1 }
        sleep 0.3
        let polled = poll $h
        case $polled [
            `settled: { |_| echo unexpected-settled },
            `pending: { |s| echo !{to-bytes $s[stdout] | from-string} }
        ]
        cancel $h
        ",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("partial-line"),
        "pending poll must carry the bytes written so far; stdout: {:?}",
        out.stdout
    );
}

// The partial snapshot is non-destructive: bytes read from a `` `pending ``
// poll are still delivered in full by a later `await`.  A pending poll clones
// the buffer (`peek_buffer`) rather than draining it, so the completion drain
// still sees everything — the total-output invariant holds across a partial
// observation.
#[test]
fn pending_poll_does_not_consume_bytes_from_await() {
    let out = run_poll(
        r#"
        let h = spawn { echo first-chunk; sleep 0.5; echo second-chunk; return 7 }
        sleep 0.2
        let _ = poll $h
        let r = await $h
        echo "value=$r[value]"
        echo !{to-bytes $r[stdout] | from-string}
        "#,
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.contains("value=7"), "stdout: {:?}", out.stdout);
    assert!(
        out.stdout.contains("first-chunk"),
        "await must still see bytes a pending poll peeked; stdout: {:?}",
        out.stdout
    );
    assert!(
        out.stdout.contains("second-chunk"),
        "stdout: {:?}",
        out.stdout
    );
}

// A block that raised polls as `` `settled `` with an `` `err `` outcome
// carrying the nonzero status and the stderr it wrote before failing —
// `poll` does NOT re-raise.  The body reaches `after-poll` and the script
// exits 0, confirming the failure was reported as data rather than raised.
#[test]
fn poll_settled_err_carries_status_and_stderr() {
    let out = run_poll(
        r#"
        let h = spawn { echo before-fail 1>&2; fail [status: 7] }
        sleep 0.2
        let polled = poll $h
        case $polled [
            `settled: { |s|
                case $s[outcome] [
                    `ok: { |_| echo unexpected-ok },
                    `err: { |e| echo "status=$e[status]" }
                ]
                echo !{to-bytes $s[stderr] | from-string}
            },
            `pending: { |_| echo unexpected-pending }
        ]
        echo after-poll
        "#,
    );
    assert_eq!(
        out.status, 0,
        "poll must not re-raise a failed block; stderr: {}",
        out.stderr
    );
    assert!(out.stdout.contains("status=7"), "stdout: {:?}", out.stdout);
    assert!(
        out.stdout.contains("before-fail"),
        "stdout: {:?}",
        out.stdout
    );
    assert!(
        out.stdout.contains("after-poll"),
        "stdout: {:?}",
        out.stdout
    );
}

// Same as `poll_settled_err_carries_status_and_stderr`, but the failing
// producer is a real external process rather than the `fail` builtin.
// Unix-only: pins that an external's nonzero exit and its actual stderr
// fd bytes surface through spawn→poll as a `settled` `err` with
// status+stderr, a code path (waitpid/exit-status mapping, pipe-drained
// stderr) distinct from the in-interpreter `fail` builtin.
#[cfg(unix)]
#[test]
fn poll_settled_err_carries_status_and_stderr_for_external_process() {
    let out = run_poll(
        r#"
        let h = spawn { /bin/sh -c "echo before-fail >&2; exit 7" }
        sleep 0.2
        let polled = poll $h
        case $polled [
            `settled: { |s|
                case $s[outcome] [
                    `ok: { |_| echo unexpected-ok },
                    `err: { |e| echo "status=$e[status]" }
                ]
                echo !{to-bytes $s[stderr] | from-string}
            },
            `pending: { |_| echo unexpected-pending }
        ]
        echo after-poll
        "#,
    );
    assert_eq!(
        out.status, 0,
        "poll must not re-raise a failed block; stderr: {}",
        out.stderr
    );
    assert!(out.stdout.contains("status=7"), "stdout: {:?}", out.stdout);
    assert!(
        out.stdout.contains("before-fail"),
        "stdout: {:?}",
        out.stdout
    );
    assert!(
        out.stdout.contains("after-poll"),
        "stdout: {:?}",
        out.stdout
    );
}

// Repeated polls of a failed handle report identical `` `settled `` outcomes
// (the bytes are drained once into the cache on completion).
#[test]
fn poll_failed_is_consistent_across_calls() {
    let out = run_poll(
        r#"
        let h = spawn { echo diag 1>&2; fail [status: 3] }
        sleep 0.2
        let report = { |h|
            let p = poll $h
            case $p [
                `settled: { |s|
                    case $s[outcome] [
                        `ok: { |_| return "ok" },
                        `err: { |e|
                            let msg = !{to-bytes $s[stderr] | from-string}
                            return "err:$e[status]:$msg"
                        }
                    ]
                },
                `pending: { |_| return "pending" }
            ]
        }
        echo !{report $h}
        echo !{report $h}
        "#,
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| l.starts_with("err:3:"))
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "both polls must report the same failed outcome; stdout: {:?}",
        out.stdout
    );
    assert_eq!(lines[0], lines[1], "stdout: {:?}", out.stdout);
}

// `is-done` is total: pending → false, settled (ok or err) → true.
#[test]
fn is_done_total_over_finished_handle() {
    let out = run_poll(
        r#"
        let running = spawn { sleep 2; return 1 }
        echo "running=!{is-done $running}"
        cancel $running

        let ok = spawn { return 1 }
        let _ = await $ok
        echo "ok=!{is-done $ok}"

        let bad = spawn { fail [status: 9] }
        sleep 0.2
        echo "bad=!{is-done $bad}"
        "#,
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("running=false"),
        "stdout: {:?}",
        out.stdout
    );
    assert!(out.stdout.contains("ok=true"), "stdout: {:?}", out.stdout);
    assert!(out.stdout.contains("bad=true"), "stdout: {:?}", out.stdout);
}

// `is-done` on a cancelled handle raises, exactly as `await` does.
#[test]
fn is_done_raises_on_cancelled_handle() {
    let out = run_poll(
        r#"
        let h = spawn { sleep 2; return 1 }
        cancel $h
        let d = is-done $h
        echo "unreachable=$d"
        "#,
    );
    assert_ne!(
        out.status, 0,
        "is-done on a cancelled handle must raise; stdout: {:?}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("unreachable"),
        "is-done must not return on a cancelled handle; stdout: {:?}",
        out.stdout
    );
}

// `await` on a failed block still re-raises — unchanged behavior.
#[test]
fn await_still_reraises_failed_block() {
    let out = run_poll(
        r#"
        let h = spawn { fail [status: 5] }
        let r = await $h
        echo "unreachable=$r[value]"
        "#,
    );
    assert_eq!(
        out.status, 5,
        "await must re-raise the block's nonzero status; stderr: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("unreachable"),
        "await must not return a record for a failed block; stdout: {:?}",
        out.stdout
    );
}
